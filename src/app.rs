//! Application state and the background refresh.
//!
//! Rendering never blocks on the network. The cache is read synchronously at
//! startup and drawn immediately; the API call runs on a thread and arrives as a
//! message. A TUI that blocks for five seconds on launch is a TUI you stop
//! opening, and then it may as well not exist.

use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use ratatui::widgets::{ListState, TableState};

use crate::discover;
use crate::git;
use crate::github;
use crate::model::*;

pub const OWNER: &str = "votingworks";
pub const REPO: &str = "vxsuite";

/// How often to re-read the filesystem and git. lazygit refreshes local state
/// every 10s; this scan is heavier than the one lazygit does on that cadence
/// (per-branch rev-list, status, and `git cherry` over every row), so it gets a
/// slightly longer leash.
const SCAN_INTERVAL: u64 = 15;

/// How often to `git fetch` and re-query GitHub. lazygit's fetchInterval, which
/// is 60s, for the same reason: it is the network, and nothing on screen changes
/// faster than a CI run finishes.
const NETWORK_INTERVAL: u64 = 60;

pub enum Msg {
    /// The filesystem+git scan finished. Carries the whole tree, because the
    /// scan runs off the UI thread and there is nothing useful to show until it
    /// is complete.
    Scanned(Box<Vec<Project>>),
    Refreshed(Box<github::Cache>),
    RefreshFailed(String),
    /// Merged-ness, keyed by branch. Deliberately *not* a whole tree: sending
    /// one back would clobber anything fetched lazily since the copy was taken,
    /// and would re-enter the scan handler.
    Merged(Vec<(String, Merged)>),
    Contexts(u32, Vec<Check>),
    Reviewers(Vec<String>),
}

/// Where to open, and whether that request is strong enough to move focus.
pub struct Select {
    pub project: String,
    pub workstream: Option<String>,
    /// Focus the workstreams pane. True when something asked for a specific
    /// workstream -- the wrapper reopening after lazygit or a rebase. False when
    /// it was only inferred from the current directory: standing inside a
    /// project says which project you care about, not that you are done choosing
    /// within it, so the cursor stays on the left where navigation starts.
    pub focus: bool,
}

/// A menu of things worth copying off the selected row.
///
/// A single copy key has to guess which of a dozen strings you meant, and it
/// will guess wrong most of the time -- the path, the branch, the remote branch,
/// the sha and the PR url are all things you copy out of here, and which one you
/// want is the whole question. So it asks.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum MenuKind {
    Copy,
    Github,
    /// Picking a reviewer; `pending` is the action to run once one is chosen.
    Reviewer,
}

pub struct CopyMenu {
    pub items: Vec<CopyItem>,
    pub idx: usize,
    pub kind: MenuKind,
    pub title: String,
    pub loading: bool,
    /// For `Reviewer`: the action verb awaiting a login.
    pub pending: Option<String>,
}

impl CopyMenu {
    pub fn copy(items: Vec<CopyItem>) -> Self {
        CopyMenu { items, idx: 0, kind: MenuKind::Copy, title: " copy ".into(), loading: false, pending: None }
    }

    pub fn github(items: Vec<CopyItem>) -> Self {
        CopyMenu { items, idx: 0, kind: MenuKind::Github, title: " github ".into(), loading: false, pending: None }
    }

    pub fn reviewer(pending: String) -> Self {
        CopyMenu {
            items: Vec::new(),
            idx: 0,
            kind: MenuKind::Reviewer,
            title: " request review from ".into(),
            loading: true,
            pending: Some(pending),
        }
    }
}

/// One entry. `note` is an annotation *about* the value rather than part of it
/// -- the CI job a url belongs to, say. It has its own field because the first
/// version folded it into the label, where it overran the label column and ran
/// into the value with nothing to say which was which.
pub struct CopyItem {
    pub label: String,
    pub value: String,
    pub note: Option<String>,
    /// Internal token for action menus; `value` stays human-readable.
    pub action: Option<String>,
}

impl CopyItem {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        CopyItem {
            label: label.into(),
            value: value.into(),
            note: None,
            action: None,
        }
    }

    pub fn with_action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

/// Strip the `ci/circleci: ` prefix every context here carries, so the job name
/// is what shows.
fn short_job(name: &str) -> String {
    name.rsplit(": ").next().unwrap_or(name).to_string()
}

impl CopyMenu {
    /// Everything copyable about a workstream, most-wanted first, skipping
    /// whatever does not apply -- a virtual row has no path, an unpushed branch
    /// no upstream, a workstream without a PR no urls.
    ///
    /// Deliberately not the PR title or the project directory. Neither is
    /// something you paste anywhere: the title you read off the row you are
    /// already looking at, and the project directory is one `cd ..` from the
    /// worktree path that is already here. A menu earns its length by every
    /// entry being one you have actually wanted.
    pub fn build(w: &Workstream) -> Self {
        let mut items: Vec<CopyItem> = Vec::new();

        if let Some(p) = &w.path {
            items.push(CopyItem::new("worktree path", p.display().to_string()));
        }
        items.push(CopyItem::new("branch", w.git.branch.clone()));
        items.push(CopyItem::new("remote branch", w.git.remote_branch.clone()));
        if !w.git.head.is_empty() {
            items.push(CopyItem::new("HEAD", w.git.head.clone()));
        }
        if let Some(up) = &w.git.upstream {
            items.push(CopyItem::new("upstream", up.clone()));
        }
        if let Some(pr) = &w.pr {
            if !pr.url.is_empty() {
                items.push(CopyItem::new("PR url", pr.url.clone()));
            }
            // The CircleCI job itself. Which job it is goes in the note, not the
            // label: "the failing one" and "the first one" are different links,
            // and you should be able to see which you are taking without it
            // looking like part of the url.
            if let Some(c) = pr.checks.ci_url() {
                if !c.url.is_empty() {
                    let label = if c.failed { "CI ✗" } else { "CI" };
                    items.push(
                        CopyItem::new(label, c.url.clone()).with_note(short_job(&c.name)),
                    );
                }
            }
            items.push(CopyItem::new("PR number", format!("#{}", pr.number)));
        }

        CopyMenu::copy(items)
    }

    /// The same idea for someone else's PR: what you paste while reviewing is
    /// the url, the CI link, and the branch you would check out.
    pub fn for_review(r: &Review) -> Self {
        let mut items = vec![
            CopyItem::new("PR url", r.url.clone()),
            CopyItem::new("PR number", format!("#{}", r.number)),
            CopyItem::new("branch", r.branch.clone()),
        ];
        if let Some(c) = r.checks.ci_url() {
            if !c.url.is_empty() {
                let label = if c.failed { "CI ✗" } else { "CI" };
                items.push(CopyItem::new(label, c.url.clone()).with_note(short_job(&c.name)));
            }
        }
        items.push(CopyItem::new(
            "checkout",
            format!("gh pr checkout {}", r.number),
        ));
        CopyMenu::copy(items)
    }

    /// GitHub actions for a workstream's PR.
    pub fn github_for(w: &Workstream) -> Vec<CopyItem> {
        let mut items = Vec::new();
        let remote = &w.git.remote_branch;
        items.push(
            CopyItem::new(
                "open a PR",
                format!("https://github.com/votingworks/vxsuite/compare/main...{remote}?expand=1"),
            )
            .with_note("copies the compare url"),
        );
        if let Some(pr) = &w.pr {
            let n = pr.number;
            if pr.state == PrState::Draft {
                items.push(
                    CopyItem::new("mark ready", format!("gh pr ready {n}"))
                        .with_action(format!("ready:{n}")),
                );
                items.push(
                    CopyItem::new("mark ready, request review", format!("gh pr ready {n} && gh pr edit {n} --add-reviewer …"))
                        .with_action(format!("ready-review:{n}"))
                        .with_note("asks who"),
                );
            } else {
                items.push(
                    CopyItem::new("request review", format!("gh pr edit {n} --add-reviewer …"))
                        .with_action(format!("review:{n}"))
                        .with_note("asks who"),
                );
            }
            items.push(
                CopyItem::new("re-run failed checks", format!("gh run rerun --failed ({n})"))
                    .with_action(format!("rerun:{n}")),
            );
            items.push(CopyItem::new("PR url", pr.url.clone()));
        }
        items
    }

    pub fn selected(&self) -> Option<&CopyItem> {
        self.items.get(self.idx)
    }
}

pub struct Confirm {
    pub title: String,
    pub body: Vec<String>,
    /// The `--cd-file` verb to write if confirmed.
    pub verb: String,
}

/// Which list the sidebar is showing. lazygit cycles its panels with [ and ],
/// and this is the same gesture: two views of "what is waiting on me", one of
/// them your own work and the other other people's.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Sidebar {
    Projects,
    Reviews,
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Pane {
    Projects,
    Workstreams,
}

pub struct App {
    pub projects: Vec<Project>,
    pub pane: Pane,
    pub sidebar: Sidebar,
    pub reviews: Vec<Review>,
    pub review_idx: usize,
    pub project_idx: usize,
    pub workstream_idx: usize,
    pub fetched_at: Option<u64>,
    pub refreshing: bool,
    pub loading: bool,
    pub scanning: bool,
    pub scanned_at: u64,
    pub network_at: u64,
    /// Automatic refreshes, toggled with `a`. Worth being able to stop: every
    /// cycle spawns `git` and `gh`, and there are times you want the numbers to
    /// hold still while you read them.
    pub auto: bool,
    /// A short note for the footer, and when it expires.
    pub flash: Option<(String, u64)>,
    /// Row of the detail pane holding the PR url, so a click can find it. Set
    /// during render, because the pane's layout depends on what the row has.
    pub url_row: Option<u16>,
    /// PR numbers whose failing-check names are being fetched right now.
    pub contexts_inflight: HashSet<u32>,
    /// Where to land once the first scan arrives. The scan runs on a thread, so
    /// at startup there is nothing to select *in* yet -- the request has to be
    /// held until there is.
    pub pending_select: Option<Select>,
    pub list_state: ListState,
    pub table_state: TableState,
    pub error: Option<String>,
    pub help: bool,
    /// A destructive action waiting on a yes. Holds the verb to emit.
    pub confirm: Option<Confirm>,
    pub copy_menu: Option<CopyMenu>,
    pub filter: Option<String>,
    pub filtering: bool,
    pub tx: Sender<Msg>,
    pub rx: Receiver<Msg>,
    /// An instruction for the fish wrapper, written to --cd-file on exit.
    ///
    /// A verb rather than a bare path because a virtual row has nowhere to cd
    /// *to* yet -- it has to be created first, and creating it means
    /// `pnpm install && pnpm build`, minutes of output you want in your
    /// terminal where you can read it and interrupt it. Running that inside the
    /// TUI would mean a log pane reimplementing what the shell already does.
    pub action: Option<String>,
    pub quit: bool,
}

impl App {
    /// Construct empty and start the scan in the background.
    ///
    /// Everything here is cheap; the ~0.5s of `git worktree list`, per-branch
    /// `rev-list`, `status --porcelain` and (mostly) `git cherry` happens on a
    /// thread. The window is on screen before any of it runs, which is the
    /// difference between a tool that opens and one you wait for.
    pub fn new() -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mut app = App {
            projects: Vec::new(),
            pane: Pane::Projects,
            sidebar: Sidebar::Projects,
            reviews: Vec::new(),
            review_idx: 0,
            project_idx: 0,
            workstream_idx: 0,
            fetched_at: None,
            refreshing: false,
            loading: true,
            scanning: false,
            scanned_at: 0,
            network_at: 0,
            auto: true,
            flash: None,
            url_row: None,
            contexts_inflight: HashSet::new(),
            pending_select: None,
            list_state: ListState::default(),
            table_state: TableState::default(),
            error: None,
            help: false,
            confirm: None,
            copy_menu: None,
            filter: None,
            filtering: false,
            tx,
            rx,
            action: None,
            quit: false,
        };
        // Draw last run's state immediately; the scan replaces it when it lands.
        if let Some(projects) = crate::cache::load() {
            app.projects = projects;
            if let Some(c) = github::load_cache(OWNER, REPO) {
                app.fetched_at = Some(c.fetched_at);
                github::apply(&mut app.projects, &c);
                app.reviews = github::to_reviews(&c);
            }
            app.loading = false;
        }
        app.start_scan(app.projects.is_empty());
        Ok(app)
    }

    /// Fire whichever periodic refresh is due. Driven from the event loop's own
    /// 100ms poll rather than from timer threads: the loop already wakes often
    /// enough for the spinner, so a second timing mechanism would buy nothing
    /// and would need waking up.
    pub fn tick(&mut self) {
        if !self.auto || self.loading {
            return;
        }
        let now = github::now();
        if now.saturating_sub(self.scanned_at) >= SCAN_INTERVAL {
            self.start_scan(false);
        }
        if now.saturating_sub(self.network_at) >= NETWORK_INTERVAL {
            self.start_refresh();
        }
    }

    /// Scan on a thread: filesystem, git, then whatever GitHub state is already
    /// cached, then merged-ness -- in that order, because merged-ness depends on
    /// whether a PR says merged.
    /// `show_loading` only on the first scan. A periodic one must not throw a
    /// modal over a screen you are reading.
    pub fn start_scan(&mut self, show_loading: bool) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        self.loading = show_loading;
        self.scanned_at = github::now();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut projects = discover::scan().unwrap_or_default();
            if let Some(cache) = github::load_cache(OWNER, REPO) {
                github::apply(&mut projects, &cache);
            }
            compute_merged(&mut projects);
            crate::cache::save(&projects);
            let _ = tx.send(Msg::Scanned(Box::new(projects)));
        });
    }

    /// Synchronous scan, for `--dump` and `--render` where there is no event
    /// loop to deliver a message to.
    pub fn scan_now(&mut self) -> Result<()> {
        self.projects = discover::scan()?;
        if let Some(cache) = github::load_cache(OWNER, REPO) {
            self.fetched_at = Some(cache.fetched_at);
            github::apply(&mut self.projects, &cache);
        }
        compute_merged(&mut self.projects);
        self.loading = false;
        self.clamp();
        Ok(())
    }

    pub fn start_refresh(&mut self) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        self.network_at = github::now();
        self.error = None;
        // Ask only about the branches that exist here. Remote names, because
        // that is the only name GitHub knows them by.
        let branches: Vec<String> = self
            .projects
            .iter()
            .flat_map(|p| p.workstreams.iter())
            .map(|w| w.git.remote_branch.clone())
            .collect();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // Fetch first, so "behind main" and the remote-tracking refs are as
            // current as the PR state landing beside them. Failure is not fatal:
            // offline, the local answer is still worth showing.
            let _ = git::fetch_prune(&discover::repo_path());
            let msg = match github::refresh(OWNER, REPO, &branches) {
                Ok(cache) => Msg::Refreshed(Box::new(cache)),
                Err(e) => Msg::RefreshFailed(format!("{e:#}")),
            };
            let _ = tx.send(msg);
        });
    }

    /// Ask for the selected PR's failing check names, once.
    ///
    /// Called every frame, so it has to be idempotent in two directions: an
    /// in-flight request must not be issued again, and a *failed* one must still
    /// answer -- an error that sent nothing left the row saying "loading…" and
    /// re-spawning a request every 100ms for as long as it stayed selected.
    pub fn request_contexts(&mut self) {
        if self.sidebar == Sidebar::Reviews {
            let Some(r) = self.review() else { return };
            if r.checks.contexts.is_some() || r.checks.is_empty() {
                return;
            }
            let number = r.number;
            if !self.contexts_inflight.insert(number) {
                return;
            }
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let list = github::contexts(OWNER, REPO, number).unwrap_or_default();
                let _ = tx.send(Msg::Contexts(number, list));
            });
            return;
        }

        let Some(w) = self.workstream() else { return };
        let Some(pr) = &w.pr else { return };
        // Fetched for any PR with checks, not only failing ones: the copy menu
        // wants a CI url whatever colour the rollup is.
        if pr.checks.contexts.is_some() || pr.checks.is_empty() {
            return;
        }
        let number = pr.number;
        if !self.contexts_inflight.insert(number) {
            return;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // An empty list on failure, so the row settles into "no names" rather
            // than spinning forever.
            let list = github::contexts(OWNER, REPO, number).unwrap_or_default();
            let _ = tx.send(Msg::Contexts(number, list));
        });
    }

    pub fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Scanned(projects) => {
                    let first = self.loading;
                    let pending = self.pending_select.take();
                    // A periodic scan replaces the whole tree, so anything the
                    // old one had learned and the new one cannot know has to be
                    // carried across: which row you were on, and the failing
                    // check names fetched lazily per PR. Without this, the
                    // selection jumps and the failing list blinks out every 15s.
                    let selection = self.selection();
                    let failing = self.failing_by_pr();

                    self.projects = *projects;
                    self.scanning = false;
                    self.loading = false;
                    self.restore_failing(&failing);
                    // An explicit request wins over carrying the old selection
                    // forward; on the first scan there is no old one anyway.
                    match pending {
                        Some(target) => self.apply_selection(target),
                        None => self.restore_selection(selection),
                    }
                    self.clamp();

                    // The branch list is only known once the scan lands, and the
                    // query is built from it, so the first fetch waits for this.
                    // Later ones are on the network timer.
                    if first {
                        self.start_refresh();
                    }
                }
                Msg::Refreshed(cache) => {
                    self.refreshing = false;
                    self.fetched_at = Some(cache.fetched_at);
                    github::apply(&mut self.projects, &cache);
                    self.reviews = github::to_reviews(&cache);
                    // `git cherry` over every row costs a few hundred
                    // milliseconds, so it does not run on the UI thread either.
                    // It answers with pairs rather than a tree -- see Msg::Merged.
                    let rows: Vec<(String, Option<PathBuf>, bool)> = self
                        .projects
                        .iter()
                        .flat_map(|p| p.workstreams.iter())
                        .map(|w| {
                            (
                                w.git.branch.clone(),
                                w.path.clone(),
                                w.pr.as_ref().is_some_and(|pr| pr.state == PrState::Merged),
                            )
                        })
                        .collect();
                    let tx = self.tx.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(Msg::Merged(merged_for(&rows)));
                    });
                }
                Msg::Merged(pairs) => {
                    for (branch, m) in pairs {
                        for p in &mut self.projects {
                            for w in &mut p.workstreams {
                                if w.git.branch == branch {
                                    w.merged = m;
                                }
                            }
                        }
                    }
                }
                Msg::RefreshFailed(e) => {
                    self.refreshing = false;
                    self.error = Some(e);
                }
                Msg::Reviewers(logins) => {
                    if let Some(m) = &mut self.copy_menu {
                        m.loading = false;
                        m.items = logins.into_iter().map(|l| CopyItem::new(l.clone(), l)).collect();
                    }
                }
                Msg::Contexts(number, list) => {
                    self.contexts_inflight.remove(&number);
                    // A number can name a workstream's PR or a review; both
                    // lists are keyed by it, so both get the answer.
                    for r in &mut self.reviews {
                        if r.number == number {
                            r.checks.contexts = Some(list.clone());
                        }
                    }
                    for p in &mut self.projects {
                        for w in &mut p.workstreams {
                            if let Some(pr) = &mut w.pr {
                                if pr.number == number {
                                    pr.checks.contexts = Some(list.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Projects passing the current filter. The filter matches a project's slug
    /// or any of its workstreams, so typing a branch name finds its project.
    pub fn visible(&self) -> Vec<usize> {
        let Some(f) = &self.filter else {
            return (0..self.projects.len()).collect();
        };
        let f = f.to_lowercase();
        self.projects
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.slug.to_lowercase().contains(&f)
                    || p.name.to_lowercase().contains(&f)
                    || p.workstreams.iter().any(|w| {
                        w.name.to_lowercase().contains(&f)
                            || w.git.branch.to_lowercase().contains(&f)
                    })
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn project(&self) -> Option<&Project> {
        self.visible().get(self.project_idx).map(|&i| &self.projects[i])
    }

    pub fn workstream(&self) -> Option<&Workstream> {
        self.project()?.workstreams.get(self.workstream_idx)
    }

    pub fn clamp(&mut self) {
        let n = self.visible().len();
        if n == 0 {
            self.project_idx = 0;
        } else if self.project_idx >= n {
            self.project_idx = n - 1;
        }
        let m = self.project().map_or(0, |p| p.workstreams.len());
        if m == 0 {
            self.workstream_idx = 0;
        } else if self.workstream_idx >= m {
            self.workstream_idx = m - 1;
        }
    }

    pub fn review(&self) -> Option<&Review> {
        self.reviews.get(self.review_idx)
    }

    /// Cycle the sidebar. Only two lists so far, so both keys are the same move
    /// in opposite directions, which is what [ and ] mean in lazygit too.
    pub fn cycle_sidebar(&mut self, _forward: bool) {
        self.sidebar = match self.sidebar {
            Sidebar::Projects => Sidebar::Reviews,
            Sidebar::Reviews => Sidebar::Projects,
        };
        // Reviews have no second pane to be in.
        self.pane = Pane::Projects;
    }

    pub fn move_down(&mut self) {
        if self.sidebar == Sidebar::Reviews {
            if !self.reviews.is_empty() {
                self.review_idx = (self.review_idx + 1) % self.reviews.len();
            }
            return;
        }
        match self.pane {
            Pane::Projects => {
                let n = self.visible().len();
                if n > 0 {
                    self.project_idx = (self.project_idx + 1) % n;
                    self.workstream_idx = 0;
                }
            }
            Pane::Workstreams => {
                let m = self.project().map_or(0, |p| p.workstreams.len());
                if m > 0 {
                    self.workstream_idx = (self.workstream_idx + 1) % m;
                }
            }
        }
    }

    pub fn move_up(&mut self) {
        if self.sidebar == Sidebar::Reviews {
            if !self.reviews.is_empty() {
                self.review_idx = (self.review_idx + self.reviews.len() - 1) % self.reviews.len();
            }
            return;
        }
        match self.pane {
            Pane::Projects => {
                let n = self.visible().len();
                if n > 0 {
                    self.project_idx = (self.project_idx + n - 1) % n;
                    self.workstream_idx = 0;
                }
            }
            Pane::Workstreams => {
                let m = self.project().map_or(0, |p| p.workstreams.len());
                if m > 0 {
                    self.workstream_idx = (self.workstream_idx + m - 1) % m;
                }
            }
        }
    }
}

/// Merged-ness for a set of (branch, worktree, pr-says-merged) rows.
pub fn merged_for(rows: &[(String, Option<PathBuf>, bool)]) -> Vec<(String, Merged)> {
    let repo = discover::repo_path();
    let base = git::base_ref(&repo);
    rows.iter()
        .map(|(branch, path, pr_merged)| {
            let dir = path.clone().unwrap_or_else(|| repo.clone());
            (branch.clone(), git::merged(&dir, branch, &base, *pr_merged))
        })
        .collect()
}

/// Merged-ness for every row. Depends on the PR answer, so it must run *after*
/// GitHub state is applied -- before it, every squash-merged branch reports open.
pub fn compute_merged(projects: &mut [Project]) {
    let repo = discover::repo_path();
    let base = git::base_ref(&repo);
    for p in projects.iter_mut() {
        for w in p.workstreams.iter_mut() {
            let dir = w.path.clone().unwrap_or_else(|| repo.clone());
            let pr_merged = w.pr.as_ref().is_some_and(|pr| pr.state == PrState::Merged);
            w.merged = git::merged(&dir, &w.git.branch, &base, pr_merged);
        }
    }
}

/// A project's worst row, for the glyph next to it in the list.
pub fn project_health(p: &Project) -> CheckState {
    p.workstreams
        .iter()
        .filter_map(|w| w.pr.as_ref())
        .map(|pr| pr.checks.state)
        .max_by_key(|s| s.severity())
        .unwrap_or(CheckState::None)
}

impl App {
    /// The selected row, by name rather than by index -- a rescan can add or
    /// remove rows, and an index would then point at something else.
    fn selection(&self) -> Option<(String, String)> {
        let p = self.project()?;
        match p.workstreams.get(self.workstream_idx) {
            Some(w) => Some((p.slug.clone(), w.name.clone())),
            None => Some((p.slug.clone(), String::new())),
        }
    }

    /// Land on a project, and on a workstream within it when the name resolves.
    pub fn apply_selection(&mut self, target: Select) {
        let Select {
            project,
            workstream,
            focus,
        } = target;
        let visible = self.visible();
        let Some(i) = visible.iter().position(|&i| self.projects[i].slug == project) else {
            return;
        };
        self.project_idx = i;
        if focus {
            self.pane = Pane::Workstreams;
        }
        if let Some(name) = workstream {
            if let Some(j) = self.projects[visible[i]]
                .workstreams
                .iter()
                .position(|w| w.name == name)
            {
                self.workstream_idx = j;
            }
        }
    }

    fn restore_selection(&mut self, sel: Option<(String, String)>) {
        let Some((project, workstream)) = sel else { return };
        let visible = self.visible();
        if let Some(i) = visible
            .iter()
            .position(|&i| self.projects[i].slug == project)
        {
            self.project_idx = i;
            if let Some(j) = self.projects[visible[i]]
                .workstreams
                .iter()
                .position(|w| w.name == workstream)
            {
                self.workstream_idx = j;
            }
        }
    }

    fn failing_by_pr(&self) -> Vec<(u32, Vec<Check>)> {
        self.projects
            .iter()
            .flat_map(|p| p.workstreams.iter())
            .filter_map(|w| w.pr.as_ref())
            .filter_map(|pr| pr.checks.contexts.clone().map(|f| (pr.number, f)))
            .collect()
    }

    fn restore_failing(&mut self, failing: &[(u32, Vec<Check>)]) {
        for p in &mut self.projects {
            for w in &mut p.workstreams {
                if let Some(pr) = &mut w.pr {
                    if let Some((_, f)) = failing.iter().find(|(n, _)| *n == pr.number) {
                        pr.checks.contexts = Some(f.clone());
                    }
                }
            }
        }
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), github::now() + 4));
    }

    /// The url the selection points at, if any.
    pub fn selected_url(&self) -> Option<String> {
        self.workstream()?.pr.as_ref().map(|pr| pr.url.clone())
    }
}

/// lazygit's spinner: four frames at 180ms, indexed straight off the wall clock
/// rather than off a counter, so every animated thing on screen is in step and
/// nothing has to own a tick.
pub fn spinner() -> &'static str {
    const FRAMES: [&str; 4] = ["●∙∙", "∙●∙", "∙∙●", "∙●∙"];
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    FRAMES[(ms / 180) as usize % FRAMES.len()]
}

pub fn ago(then: u64) -> String {
    let now = github::now();
    let d = now.saturating_sub(then);
    if d < 60 {
        format!("{d}s ago")
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86400)
    }
}
