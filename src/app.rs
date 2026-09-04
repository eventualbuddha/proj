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
    Contexts(u32, Vec<String>),
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Pane {
    Projects,
    Workstreams,
}

pub struct App {
    pub projects: Vec<Project>,
    pub pane: Pane,
    pub project_idx: usize,
    pub workstream_idx: usize,
    pub fetched_at: Option<u64>,
    pub refreshing: bool,
    pub loading: bool,
    /// A short note for the footer, and when it expires.
    pub flash: Option<(String, u64)>,
    /// Row of the detail pane holding the PR url, so a click can find it. Set
    /// during render, because the pane's layout depends on what the row has.
    pub url_row: Option<u16>,
    /// PR numbers whose failing-check names are being fetched right now.
    pub contexts_inflight: HashSet<u32>,
    pub list_state: ListState,
    pub table_state: TableState,
    pub error: Option<String>,
    pub help: bool,
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
            project_idx: 0,
            workstream_idx: 0,
            fetched_at: None,
            refreshing: false,
            loading: true,
            flash: None,
            url_row: None,
            contexts_inflight: HashSet::new(),
            list_state: ListState::default(),
            table_state: TableState::default(),
            error: None,
            help: false,
            filter: None,
            filtering: false,
            tx,
            rx,
            action: None,
            quit: false,
        };
        app.start_scan();
        Ok(app)
    }

    /// Scan on a thread: filesystem, git, then whatever GitHub state is already
    /// cached, then merged-ness -- in that order, because merged-ness depends on
    /// whether a PR says merged.
    pub fn start_scan(&mut self) {
        self.loading = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut projects = discover::scan().unwrap_or_default();
            if let Some(cache) = github::load_cache(OWNER, REPO) {
                github::apply(&mut projects, &cache);
            }
            compute_merged(&mut projects);
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
        let Some(w) = self.workstream() else { return };
        let Some(pr) = &w.pr else { return };
        if pr.checks.failing.is_some() || pr.checks.state != CheckState::Failure {
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
            let list = github::failing_contexts(OWNER, REPO, number).unwrap_or_default();
            let _ = tx.send(Msg::Contexts(number, list));
        });
    }

    pub fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Scanned(projects) => {
                    self.projects = *projects;
                    self.loading = false;
                    self.clamp();
                    // The branch list is only known once the scan lands, and the
                    // query is built from it, so the fetch waits for this.
                    self.start_refresh();
                }
                Msg::Refreshed(cache) => {
                    self.refreshing = false;
                    self.fetched_at = Some(cache.fetched_at);
                    github::apply(&mut self.projects, &cache);
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
                Msg::Contexts(number, list) => {
                    self.contexts_inflight.remove(&number);
                    for p in &mut self.projects {
                        for w in &mut p.workstreams {
                            if let Some(pr) = &mut w.pr {
                                if pr.number == number {
                                    pr.checks.failing = Some(list.clone());
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

    pub fn move_down(&mut self) {
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
