//! An instance's end of the daemon connection.
//!
//! Connecting is not allowed to cost startup latency, so it happens on a thread
//! and the handle arrives as a message like everything else. Until it does --
//! and again if the daemon goes away -- the instance fetches for itself, which
//! is what it did before the daemon existed. Nothing here is load-bearing for
//! the dashboard to work; it is load-bearing for it to work once.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use crate::app::Msg;
use crate::ipc::{self, Req, Res};

/// A live connection. Cheap to clone, and sending on a dead one is a no-op --
/// the loss arrives separately as `Msg::DaemonLost`.
#[derive(Clone)]
pub struct Handle {
    tx: Sender<Req>,
}

impl Handle {
    pub fn send(&self, req: Req) {
        let _ = self.tx.send(req);
    }
}

/// Connect on a thread, answering with `Msg::Daemon` if it works.
///
/// Silent on failure: a machine where the daemon cannot start is a machine
/// where `proj` still has to open.
pub fn connect_async(app_tx: Sender<Msg>) {
    // `PROJ_NO_DAEMON` for anyone who wants the old one-process-one-fetcher
    // behaviour back; `cfg!(test)` because the tests build an `App` and a test
    // binary must not start background processes -- least of all by re-execing
    // itself with an argument libtest has never heard of.
    if cfg!(test) || std::env::var_os("PROJ_NO_DAEMON").is_some() {
        return;
    }
    std::thread::spawn(move || {
        if let Some(handle) = connect(&app_tx) {
            let _ = app_tx.send(Msg::Daemon(handle));
        }
    });
}

fn connect(app_tx: &Sender<Msg>) -> Option<Handle> {
    let path = ipc::socket_path();
    let mut stream = UnixStream::connect(&path).ok();
    if stream.is_none() {
        spawn_daemon();
        // The daemon has to start, create its directory and bind before it can
        // answer. Two seconds is long for that and short next to the minute
        // between fetches.
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(50));
            stream = UnixStream::connect(&path).ok();
            if stream.is_some() {
                break;
            }
        }
    }
    let stream = stream?;
    let read_half = stream.try_clone().ok()?;

    let (tx, reqs) = mpsc::channel::<Req>();
    let mut write_half = stream;
    std::thread::spawn(move || {
        for req in reqs {
            if ipc::send(&mut write_half, &req).is_err() {
                return;
            }
        }
    });

    let app_tx = app_tx.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(read_half).lines() {
            let Ok(line) = line else { break };
            let Ok(res) = serde_json::from_str::<Res>(&line) else {
                continue;
            };
            let msg = match res {
                Res::Cache { cache } => Msg::Refreshed(cache),
                Res::Fetching { on } => Msg::Fetching(on),
                Res::Failed { message } => Msg::RefreshFailed(message),
                Res::Contexts { number, checks } => {
                    Msg::Contexts(number, checks.into_iter().map(Into::into).collect())
                }
                Res::Reviewers { logins } => Msg::Reviewers(logins),
                // Only `--daemon-status` asks, and it does not run an App.
                Res::Status { .. } => continue,
            };
            if app_tx.send(msg).is_err() {
                return;
            }
        }
        let _ = app_tx.send(Msg::DaemonLost);
    });

    Some(Handle { tx })
}

/// Start a daemon from this same binary.
///
/// `setsid` matters more than it looks: without it the daemon shares the
/// terminal's process group, and closing the window that happened to start it
/// takes down the fetcher every other window is using.
fn spawn_daemon() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

/// Ask a running daemon what it is doing. Deliberately does not start one: this
/// is the question "is anything fetching for me", and answering it by making the
/// answer yes would be useless.
pub fn status() -> String {
    let path = ipc::socket_path();
    let Ok(mut stream) = UnixStream::connect(&path) else {
        return format!("no daemon on {}", path.display());
    };
    if ipc::send(&mut stream, &Req::Status).is_err() {
        return "daemon stopped answering".into();
    }
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        if let Ok(Res::Status { text }) = serde_json::from_str::<Res>(line.trim()) {
            return format!("{}\nsocket {}", text, path.display());
        }
        line.clear();
    }
    "daemon stopped answering".into()
}

/// Ask a running daemon to exit.
pub fn stop() -> String {
    let path = ipc::socket_path();
    let Ok(mut stream) = UnixStream::connect(&path) else {
        return "no daemon running".into();
    };
    match ipc::send(&mut stream, &Req::Stop) {
        Ok(()) => "daemon stopping".into(),
        Err(e) => format!("daemon: {e:#}"),
    }
}
