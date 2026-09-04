//! The shape of what `proj` knows about.
//!
//! A workstream row is either *materialized* -- it has a directory on disk --
//! or *virtual*: it exists only as a branch, a PR, or both. That split is the
//! centre of the whole design. A filesystem-only tool renders
//! `brian/esm-lib-batch-4` nowhere, even though it is PR #9083 with six commits
//! and no worktree, and that blind spot is what this program exists to close.

use std::path::PathBuf;

/// Where a row came from, and therefore what can be done with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A directory under a project. Has a branch, may have a PR.
    Worktree,
    /// A local branch matching the project, with no worktree anywhere.
    OrphanBranch,
}

/// A sequencer operation the worktree is part-way through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Rebase,
    Merge,
    CherryPick,
    Revert,
    Bisect,
}

#[derive(Debug, Clone)]
pub struct Op {
    pub kind: OpKind,
    pub done: u32,
    pub total: u32,
    /// The branch being rebased, from `rebase-merge/head-name`. HEAD is detached
    /// during the operation, so this is the only place the name survives.
    pub branch: Option<String>,
}

impl Op {
    pub fn label(&self) -> String {
        let verb = match self.kind {
            OpKind::Rebase => "rebasing",
            OpKind::Merge => "merging",
            OpKind::CherryPick => "cherry-picking",
            OpKind::Revert => "reverting",
            OpKind::Bisect => "bisecting",
        };
        if self.total > 0 {
            format!("{verb} {}/{}", self.done, self.total)
        } else {
            verb.to_string()
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GitState {
    pub branch: String,
    /// Commits on this branch that are not on the base.
    pub ahead: u32,
    /// Commits on the base that are not on this branch.
    pub behind: u32,
    pub dirty: u32,
    pub staged: u32,
    // No stash count: `refs/stash` is shared by every worktree of a repo, so a
    // per-row number would be the same everywhere and mean nothing about the row.
    pub upstream: Option<String>,
    /// What this branch is called on the remote. Local branches are
    /// `<project>/<workstream>`; the remote keeps the `brian/` handle
    /// namespace, because that is what tells your branches from a teammate's on
    /// a shared repo. GitHub only ever knows the remote name, so every PR
    /// lookup has to go through this rather than through `branch`.
    pub remote_branch: String,
    /// Commits not pushed to the upstream. `None` when there is no upstream.
    pub unpushed: Option<u32>,
    /// Unix seconds of the branch tip's commit date.
    pub last_commit: Option<i64>,
    pub head: String,
    /// Set when the worktree is mid-rebase, mid-merge, and so on. While it is
    /// set, `branch` comes from the sequencer state rather than from HEAD, and
    /// the ahead/behind counts describe a HEAD part-way through the operation.
    pub op: Option<Op>,
}

/// Merged-ness is three separate questions and conflating them is how a tool
/// starts lying. A squash-merged branch is not an ancestor of `main`, so
/// ancestry alone reports every merged branch as unmerged -- and squash is the
/// common case in this repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Merged {
    #[default]
    No,
    /// The PR is marked merged on GitHub. The most trustworthy signal.
    Pr,
    /// The branch tip is reachable from the base. A real merge commit or a
    /// fast-forward.
    Ancestor,
    /// Every commit has an equivalent patch-id on the base, though the history
    /// was rewritten. This is what a squash merge looks like from here.
    Equivalent,
}

impl Merged {
    pub fn is_merged(self) -> bool {
        self != Merged::No
    }

    pub fn label(self) -> &'static str {
        match self {
            Merged::No => "",
            Merged::Pr => "merged",
            Merged::Ancestor => "merged",
            // Worth distinguishing in the UI: it means "the code is in main"
            // rather than "GitHub says so", which is a weaker claim.
            Merged::Equivalent => "squashed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrState {
    #[default]
    None,
    Draft,
    Open,
    Merged,
    Closed,
}

impl PrState {
    pub fn label(self) -> &'static str {
        match self {
            PrState::None => "—",
            PrState::Draft => "Draft",
            PrState::Open => "Open",
            PrState::Merged => "Merged",
            PrState::Closed => "Closed",
        }
    }

    /// Nerd Font glyph per state, the same codepoints lazygit uses:
    /// nf-oct-git_pull_request, nf-cod-git_pull_request_closed,
    /// nf-oct-git_merge, nf-cod-git_pull_request_draft. The terminal on this
    /// machine already renders Nerd Font glyphs -- the starship prompt is full
    /// of them -- so there is no ASCII fallback to maintain.
    pub fn icon(self) -> &'static str {
        match self {
            PrState::Open => "\u{f407}",
            PrState::Closed => "\u{ebda}",
            PrState::Merged => "\u{f4c9}",
            PrState::Draft => "\u{ebdb}",
            PrState::None => "",
        }
    }

    /// GitHub's own badge colours, lifted from lazygit so a PR reads the same
    /// here as it does on the site and in the other tool on this machine.
    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            PrState::Open => (0x43, 0x84, 0x40),
            PrState::Closed => (0xC9, 0x45, 0x3C),
            PrState::Merged => (0x82, 0x59, 0xDD),
            PrState::Draft => (0x67, 0x6C, 0x75),
            PrState::None => (0x8a, 0x8a, 0x9a),
        }
    }
}

/// GitHub's `statusCheckRollup.state`, with lazygit's glyph and wording for each
/// -- worth copying rather than inventing, because `ERROR` and `FAILURE` are
/// genuinely different things (the run broke vs. the check said no) and
/// `EXPECTED` means a required check has not even been reported yet, which reads
/// as "nothing here" unless it has its own mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckState {
    #[default]
    None,
    Success,
    Pending,
    Failure,
    Error,
    Expected,
}

impl CheckState {
    pub fn glyph(self) -> &'static str {
        match self {
            CheckState::None => "",
            CheckState::Success => "✓",
            CheckState::Pending => "●",
            CheckState::Failure => "✗",
            CheckState::Error => "!",
            CheckState::Expected => "○",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CheckState::None => "",
            CheckState::Success => "passing",
            CheckState::Pending => "pending",
            CheckState::Failure => "failing",
            CheckState::Error => "error",
            CheckState::Expected => "expected",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "SUCCESS" => CheckState::Success,
            "PENDING" => CheckState::Pending,
            "FAILURE" => CheckState::Failure,
            "ERROR" => CheckState::Error,
            "EXPECTED" => CheckState::Expected,
            _ => CheckState::None,
        }
    }

    /// Rank for rolling several rows up into one project glyph: worst wins.
    pub fn severity(self) -> u8 {
        match self {
            CheckState::None => 0,
            CheckState::Success => 1,
            CheckState::Expected => 2,
            CheckState::Pending => 3,
            CheckState::Failure => 4,
            CheckState::Error => 5,
        }
    }
}

/// One check context, with wherever it reports to.
///
/// For this repo that is always CircleCI: every context is a StatusContext
/// named `ci/circleci: <job>` whose targetUrl is that job's page. A "checks
/// url" built by appending /checks to the PR only ever gets you GitHub's own
/// summary tab, which is a list of links to these.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub url: String,
    pub failed: bool,
}

/// The rollup of a commit's check runs.
///
/// Only the rollup *state* and the context count come from the list query. The
/// names of failing contexts are fetched per-PR when the detail pane asks,
/// because vxsuite posts ~63 contexts per PR: asking for all of them across 60
/// PRs is up to 6000 nodes in one request, and GitHub answers that with a 504.
/// (It is also why this is GraphQL at all -- the REST status endpoint paginates
/// those 63 into silence at its default page size of 30.)
#[derive(Debug, Clone, Default)]
pub struct Checks {
    pub state: CheckState,
    pub total: u32,
    /// Every context, fetched lazily for the selected row.
    pub contexts: Option<Vec<Check>>,
}

impl Checks {
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub fn failing(&self) -> Vec<&Check> {
        self.contexts
            .as_ref()
            .map(|c| c.iter().filter(|c| c.failed).collect())
            .unwrap_or_default()
    }

    /// The CI page worth opening: the first failing job if anything failed,
    /// otherwise the first job at all. When something is red that is the only
    /// one you want; when nothing is, any of them reaches the workflow.
    pub fn ci_url(&self) -> Option<&Check> {
        let all = self.contexts.as_ref()?;
        all.iter().find(|c| c.failed).or_else(|| all.first())
    }
}

#[derive(Debug, Clone, Default)]
pub struct PrInfo {
    pub number: u32,
    pub state: PrState,
    pub title: String,
    pub url: String,
    pub author: String,
    /// GitHub's `reviewDecision`: APPROVED, CHANGES_REQUESTED, REVIEW_REQUIRED.
    pub review_decision: Option<String>,
    pub checks: Checks,
}

#[derive(Debug, Clone)]
pub struct Workstream {
    pub project: String,
    pub name: String,
    pub origin: Origin,
    /// `None` for a virtual row.
    pub path: Option<PathBuf>,
    pub git: GitState,
    pub merged: Merged,
    pub pr: Option<PrInfo>,
}

impl Workstream {
    pub fn is_virtual(&self) -> bool {
        self.path.is_none()
    }

    pub fn qualified(&self) -> String {
        format!("{}/{}", self.project, self.name)
    }
}

#[derive(Debug, Clone)]
pub struct Project {
    pub slug: String,
    pub emoji: String,
    pub name: String,
    pub kind: ProjectKind,
    pub status: String,
    pub readme: PathBuf,
    pub branch_prefix: Option<String>,
    /// Globs from the frontmatter, used to claim orphan branches.
    pub branch_globs: Vec<String>,
    pub workstreams: Vec<Workstream>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProjectKind {
    #[default]
    Project,
    Review,
}

/// Why a PR is in your review queue. Worth keeping apart: the first two are
/// "someone is waiting on you", the third is "you already looked, and it moved".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewReason {
    Requested,
    TeamRequested,
    Rereview,
}

impl ReviewReason {
    pub fn label(self) -> &'static str {
        match self {
            ReviewReason::Requested => "requested",
            ReviewReason::TeamRequested => "team",
            ReviewReason::Rereview => "re-review",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Review {
    pub number: u32,
    pub title: String,
    pub url: String,
    pub author: String,
    pub branch: String,
    pub state: PrState,
    pub checks: Checks,
    pub reason: ReviewReason,
    /// Unix seconds of the head commit, for "how stale is what I am looking at".
    pub updated: i64,
}
