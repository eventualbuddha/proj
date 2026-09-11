//! The shared fetcher.
//!
//! Every `proj` on this machine used to run its own minute timer: its own
//! `git fetch --prune` in the same repo, its own multi-branch GraphQL query for
//! largely the same branches. Four terminals meant four times the API calls and
//! four writers racing over one cache file. This is that work, done once.
//!
//! It owns the network -- `gh` and `git fetch` -- and nothing else. The
//! filesystem scan stays in each instance: it is sub-second, it is what makes
//! the cursor land on the row you are standing in, and routing it through a
//! socket would buy a few hundred milliseconds at the cost of the one piece of
//! state that is genuinely per-instance.
//!
//! Lifetime: started on demand by the first instance that finds no socket, kept
//! alive by whoever is connected, and gone an hour after the last one leaves.
//! Through that hour it keeps fetching, an order of magnitude more slowly, so
//! the next `proj` opens onto minutes-old state instead of an empty dashboard.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use crate::app::{OWNER, REPO};
use crate::github;
use crate::ipc::{self, Req, Res, WireCheck};
use crate::model::Check;

/// Fetch cadence while someone is watching. The instances' own interval, since
/// it is the same question being asked from a different process.
const ACTIVE_INTERVAL: u64 = 60;

/// Fetch cadence with nobody connected. Ten minutes is slow enough to be
/// invisible in the API budget and fast enough that the cache a returning
/// instance loads is worth drawing.
const IDLE_INTERVAL: u64 = 600;

/// How long an unattended daemon keeps itself around. Long enough to cover
/// lunch, short enough that a laptop left alone overnight is not still polling
/// GitHub in the morning.
const IDLE_EXIT: u64 = 3600;

/// How long a fetched context list is served without re-asking. Check state
/// moves while CI runs, but not within one keypress, and the detail pane asks
/// every time a row is selected.
const CONTEXTS_TTL: u64 = 30;

/// Everything the loop reacts to. One thread owns the state and every other
/// thread -- accept, per-client reader, fetch worker, timer -- only sends.
enum Ev {
    Client(UnixStream),
    Req(usize, Req),
    Gone(usize),
    Fetched(Box<github::Cache>),
    FetchFailed(String),
    Contexts(u32, Vec<Check>),
    Reviewers(usize, Vec<String>),
    Tick,
}

struct Client {
    out: Sender<String>,
    branches: Vec<String>,
    /// Whether this connection is a dashboard watching branches, as opposed to
    /// a `--daemon-status` asking a question and leaving. Only the former keeps
    /// the daemon alive: otherwise polling the status from a script would hold
    /// the idle timer open forever, which is the one thing it exists to close.
    watching: bool,
}

struct Daemon {
    clients: HashMap<usize, Client>,
    next_id: usize,
    cache: Option<github::Cache>,
    /// Last successful fetch, and whether one is running now.
    fetched_at: u64,
    fetching: bool,
    /// A refresh asked for while one was already in flight.
    again: bool,
    /// Branches survive disconnection: the idle fetches are only useful if they
    /// keep asking about the same branches the next instance will want.
    branches: Vec<String>,
    contexts: HashMap<u32, (u64, Vec<Check>)>,
    contexts_inflight: Vec<u32>,
    /// When the last dashboard left. `None` while any is connected.
    idle_since: Option<u64>,
    started_at: u64,
    tx: Sender<Ev>,
}

/// Serve until the idle timer runs out. Returns immediately if another daemon
/// for this binary already holds the socket.
pub fn run() -> Result<()> {
    let path = ipc::socket_path();
    let Some(listener) = bind(&path)? else {
        return Ok(());
    };

    let (tx, rx) = mpsc::channel();

    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if tx.send(Ev::Client(stream)).is_err() {
                    return;
                }
            }
        });
    }
    {
        let tx = tx.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            if tx.send(Ev::Tick).is_err() {
                return;
            }
        });
    }

    let mut d = Daemon {
        clients: HashMap::new(),
        next_id: 0,
        cache: github::load_cache(OWNER, REPO),
        fetched_at: 0,
        fetching: false,
        again: false,
        branches: Vec::new(),
        contexts: HashMap::new(),
        contexts_inflight: Vec::new(),
        idle_since: Some(github::now()),
        started_at: github::now(),
        tx,
    };
    // A cache on disk is as good as one just fetched, up to its age: the timer
    // below is what decides whether it is stale, so start it from when the
    // cache was written rather than from zero.
    d.fetched_at = d.cache.as_ref().map(|c| c.fetched_at).unwrap_or(0);

    let done = d.serve(rx);
    let _ = std::fs::remove_file(&path);
    done
}

impl Daemon {
    fn serve(&mut self, rx: Receiver<Ev>) -> Result<()> {
        while let Ok(ev) = rx.recv() {
            match ev {
                Ev::Client(stream) => self.accept(stream),
                Ev::Req(id, req) => {
                    if self.request(id, req) {
                        return Ok(());
                    }
                }
                Ev::Gone(id) => {
                    self.clients.remove(&id);
                    self.reassess_idle();
                }
                Ev::Fetched(cache) => {
                    self.fetching = false;
                    self.fetched_at = cache.fetched_at;
                    // The contexts memo describes the commits the last fetch saw;
                    // a new rollup may be about different ones.
                    self.contexts.clear();
                    self.broadcast(&Res::Cache { cache: cache.clone() });
                    self.cache = Some(*cache);
                    self.broadcast(&Res::Fetching { on: false });
                    if std::mem::take(&mut self.again) {
                        self.fetch();
                    }
                }
                Ev::FetchFailed(message) => {
                    self.fetching = false;
                    // `fetched_at` deliberately moves on failure too: a repo
                    // that cannot be reached must not be retried every second.
                    self.fetched_at = github::now();
                    self.broadcast(&Res::Fetching { on: false });
                    self.broadcast(&Res::Failed { message });
                }
                Ev::Contexts(number, checks) => {
                    self.contexts_inflight.retain(|n| *n != number);
                    self.contexts.insert(number, (github::now(), checks.clone()));
                    // To everyone: several instances watching the same PR is the
                    // normal case, and they all key the answer by number.
                    self.broadcast(&Res::Contexts {
                        number,
                        checks: checks.iter().map(WireCheck::from).collect(),
                    });
                }
                Ev::Reviewers(id, logins) => {
                    // Not a broadcast: this fills a menu open in one instance.
                    self.send(id, &Res::Reviewers { logins });
                }
                Ev::Tick => {
                    if self.tick() {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    fn accept(&mut self, stream: UnixStream) {
        let id = self.next_id;
        self.next_id += 1;
        let Ok(read_half) = stream.try_clone() else {
            return;
        };

        // A writer thread per client, so one instance that has stopped reading
        // -- stopped in a debugger, say -- cannot block the loop that serves
        // everybody else.
        let (out, lines) = mpsc::channel::<String>();
        let mut write_half = stream;
        std::thread::spawn(move || {
            use std::io::Write;
            for line in lines {
                if write_half.write_all(line.as_bytes()).is_err() {
                    return;
                }
                let _ = write_half.flush();
            }
        });

        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(read_half).lines() {
                let Ok(line) = line else { break };
                match serde_json::from_str::<Req>(&line) {
                    Ok(req) => {
                        if tx.send(Ev::Req(id, req)).is_err() {
                            return;
                        }
                    }
                    // A line we cannot parse is an older or newer instance
                    // saying something we do not know; ignoring it keeps the
                    // rest of the conversation working.
                    Err(_) => continue,
                }
            }
            let _ = tx.send(Ev::Gone(id));
        });

        self.clients.insert(id, Client { out, branches: Vec::new(), watching: false });

        // Answer the connection itself with whatever is already known, so an
        // instance draws PR state before its first scan has even finished.
        if let Some(cache) = &self.cache {
            let cache = Box::new(cache.clone());
            self.send(id, &Res::Cache { cache });
        }
        if self.fetching {
            self.send(id, &Res::Fetching { on: true });
        }
    }

    /// Handle one request. Returns true to stop the daemon.
    fn request(&mut self, id: usize, req: Req) -> bool {
        match req {
            Req::Branches { branches } => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.branches = branches;
                    c.watching = true;
                }
                self.reassess_idle();
                let known = std::mem::take(&mut self.branches);
                self.branches = self.union(&known);
                // A branch nobody has asked GitHub about yet is a row that will
                // sit blank until the next minute is up, which is most of the
                // time you are looking at a branch you just made.
                if self.branches.iter().any(|b| !known.contains(b)) {
                    self.fetch();
                }
            }
            Req::Refresh => self.fetch(),
            Req::Contexts { number } => self.contexts(id, number),
            Req::Reviewers { number } => {
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let logins =
                        github::suggested_reviewers(OWNER, REPO, number).unwrap_or_default();
                    let _ = tx.send(Ev::Reviewers(id, logins));
                });
            }
            Req::Status => {
                let text = self.status();
                self.send(id, &Res::Status { text });
            }
            Req::Stop => return true,
        }
        false
    }

    /// The branch union over every connected instance, plus what was being
    /// asked for before the last one left.
    fn union(&self, carried: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let live = self.clients.values().flat_map(|c| c.branches.iter());
        // Carried branches last: a branch still open in a window outranks one
        // left over from a window that closed.
        for b in live.chain(carried.iter()) {
            if !b.is_empty() && !out.contains(b) {
                out.push(b.clone());
            }
        }
        out
    }

    fn contexts(&mut self, id: usize, number: u32) {
        if let Some((at, checks)) = self.contexts.get(&number) {
            if github::now().saturating_sub(*at) < CONTEXTS_TTL {
                self.send(
                    id,
                    &Res::Contexts {
                        number,
                        checks: checks.iter().map(WireCheck::from).collect(),
                    },
                );
                return;
            }
        }
        if self.contexts_inflight.contains(&number) {
            return;
        }
        self.contexts_inflight.push(number);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // An empty list on failure, matching what an instance fetching for
            // itself does: the row settles into "no names" rather than spinning.
            let list = github::contexts(OWNER, REPO, number).unwrap_or_default();
            let _ = tx.send(Ev::Contexts(number, list));
        });
    }

    /// Dashboards, not connections.
    fn watchers(&self) -> usize {
        self.clients.values().filter(|c| c.watching).count()
    }

    /// Start or stop the idle countdown to match who is actually watching.
    fn reassess_idle(&mut self) {
        match (self.watchers() > 0, self.idle_since) {
            (true, _) => self.idle_since = None,
            (false, None) => self.idle_since = Some(github::now()),
            (false, Some(_)) => {}
        }
    }

    fn fetch(&mut self) {
        if self.fetching {
            self.again = true;
            return;
        }
        if self.branches.is_empty() {
            return;
        }
        self.fetching = true;
        self.broadcast(&Res::Fetching { on: true });
        let branches = self.branches.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // Fetch first, so "behind main" and the remote-tracking refs are as
            // current as the PR state landing beside them. Failure is not fatal:
            // offline, the local answer is still worth showing.
            let _ = crate::git::fetch_prune(&crate::discover::repo_path());
            let ev = match github::refresh(OWNER, REPO, &branches) {
                Ok(cache) => Ev::Fetched(Box::new(cache)),
                Err(e) => Ev::FetchFailed(format!("{e:#}")),
            };
            let _ = tx.send(ev);
        });
    }

    /// The once-a-second timer. Returns true to stop the daemon.
    fn tick(&mut self) -> bool {
        let now = github::now();
        if let Some(since) = self.idle_since {
            if now.saturating_sub(since) >= IDLE_EXIT {
                return true;
            }
        }
        let interval = self.interval();
        if !self.fetching && now.saturating_sub(self.fetched_at) >= interval {
            self.fetch();
        }
        false
    }

    fn interval(&self) -> u64 {
        if self.watchers() == 0 {
            IDLE_INTERVAL
        } else {
            ACTIVE_INTERVAL
        }
    }

    fn status(&self) -> String {
        let now = github::now();
        let age = |t: u64| if t == 0 { "never".to_string() } else { format!("{}s ago", now - t) };
        format!(
            "pid {}\nup {}s\nwatching {}\nbranches {}\nlast fetch {}{}\nnext fetch every {}s\n{}",
            std::process::id(),
            now - self.started_at,
            self.watchers(),
            self.branches.len(),
            age(self.fetched_at),
            if self.fetching { " (fetching now)" } else { "" },
            self.interval(),
            match self.idle_since {
                Some(since) => format!("idle {}s, exits in {}s", now - since, IDLE_EXIT.saturating_sub(now - since)),
                None => "in use".to_string(),
            },
        )
    }

    fn send(&self, id: usize, res: &Res) {
        let Ok(mut line) = serde_json::to_string(res) else {
            return;
        };
        line.push('\n');
        if let Some(c) = self.clients.get(&id) {
            let _ = c.out.send(line);
        }
    }

    fn broadcast(&self, res: &Res) {
        let Ok(mut line) = serde_json::to_string(res) else {
            return;
        };
        line.push('\n');
        for c in self.clients.values() {
            let _ = c.out.send(line.clone());
        }
    }
}

/// Take the socket, or discover that someone else already has it.
///
/// The lock file closes the window between "nobody answered" and "I have bound"
/// -- two instances launched together both find no daemon, and without it the
/// second unlinks the first's socket seconds after clients started using it.
fn bind(path: &Path) -> Result<Option<UnixListener>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("creating the socket directory")?;
    }
    let lock = path.with_extension("lock");
    let _guard = Lock::take(&lock)?;

    // A socket that answers has a daemon behind it; one that refuses is the
    // leftover of a daemon that was killed, and unlinking it is the only way
    // bind will ever succeed again.
    if UnixStream::connect(path).is_ok() {
        return Ok(None);
    }
    let _ = std::fs::remove_file(path);
    Ok(Some(UnixListener::bind(path).context("binding the daemon socket")?))
}

struct Lock(PathBuf);

impl Lock {
    fn take(path: &Path) -> Result<Lock> {
        for _ in 0..50 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(_) => return Ok(Lock(path.to_path_buf())),
                Err(_) => {
                    // Held for the microseconds of a bind, so anything older
                    // than this is a lock whose holder died before releasing it.
                    let stale = std::fs::metadata(path)
                        .and_then(|m| m.modified())
                        .map(|t| t.elapsed().map(|e| e > Duration::from_secs(10)).unwrap_or(false))
                        .unwrap_or(true);
                    if stale {
                        let _ = std::fs::remove_file(path);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        anyhow::bail!("could not take {}", path.display())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon() -> Daemon {
        let (tx, _rx) = mpsc::channel();
        Daemon {
            clients: HashMap::new(),
            next_id: 0,
            cache: None,
            fetched_at: 0,
            fetching: false,
            again: false,
            branches: Vec::new(),
            contexts: HashMap::new(),
            contexts_inflight: Vec::new(),
            idle_since: Some(github::now()),
            started_at: github::now(),
            tx,
        }
    }

    /// Adds a client and returns its id alongside its queue, which the caller
    /// holds so that sending to it keeps working. `watching` is the difference
    /// between a dashboard and a `--daemon-status`.
    fn client(d: &mut Daemon, branches: &[&str], watching: bool) -> (usize, Receiver<String>) {
        let (out, rx) = mpsc::channel();
        let id = d.next_id;
        d.next_id += 1;
        d.clients.insert(
            id,
            Client {
                out,
                branches: branches.iter().map(|b| b.to_string()).collect(),
                watching,
            },
        );
        d.reassess_idle();
        (id, rx)
    }

    #[test]
    fn asking_for_status_does_not_keep_the_daemon_alive() {
        let mut d = daemon();
        let (id, _q) = client(&mut d, &[], false);
        assert!(d.idle_since.is_some(), "a status query is not a dashboard");
        assert_eq!(d.interval(), IDLE_INTERVAL);
        d.clients.remove(&id);
        d.reassess_idle();
        assert!(d.idle_since.is_some());
    }

    #[test]
    fn a_dashboard_stops_the_idle_clock_and_leaving_starts_it_again() {
        let mut d = daemon();
        let (id, _q) = client(&mut d, &["brian/a"], true);
        assert_eq!(d.idle_since, None);
        assert_eq!(d.interval(), ACTIVE_INTERVAL);

        d.clients.remove(&id);
        d.reassess_idle();
        assert!(d.idle_since.is_some());
        assert_eq!(d.interval(), IDLE_INTERVAL);
    }

    #[test]
    fn the_query_is_the_union_over_every_window() {
        let mut d = daemon();
        let (_a, _qa) = client(&mut d, &["brian/a", "brian/b"], true);
        let (_b, _qb) = client(&mut d, &["brian/b", "brian/c"], true);
        let mut union = d.union(&[]);
        union.sort();
        assert_eq!(union, ["brian/a", "brian/b", "brian/c"]);
    }

    /// The idle fetches only warm the cache if they keep asking about the same
    /// branches the next dashboard will open onto.
    #[test]
    fn branches_outlive_the_window_that_asked_for_them() {
        let mut d = daemon();
        assert_eq!(d.union(&["brian/a".to_string()]), ["brian/a"]);
    }

    /// Also why a failed fetch moves `fetched_at`: an unreachable GitHub would
    /// otherwise look exactly like a stale cache, once a second, forever.
    #[test]
    fn a_cache_that_was_just_fetched_is_not_fetched_again_on_the_next_tick() {
        let mut d = daemon();
        let (_id, _q) = client(&mut d, &["brian/a"], true);
        d.branches = vec!["brian/a".into()];
        d.fetched_at = github::now();
        assert!(!d.tick());
        assert!(!d.fetching, "the next attempt waits for the interval");
    }

    #[test]
    fn an_hour_with_nobody_watching_is_the_end_of_it() {
        let mut d = daemon();
        d.idle_since = Some(github::now() - IDLE_EXIT);
        assert!(d.tick(), "stops");
        // A window opening resets it rather than shortening it: the hour is
        // "since anyone was here", not "since this daemon started".
        d.idle_since = Some(github::now() - IDLE_EXIT);
        let (_id, _q) = client(&mut d, &["brian/a"], true);
        assert_eq!(d.idle_since, None);
    }
}
