//! Git state, read by shelling out to `git`.
//!
//! Deliberately not gix or git2. `wt.fish` already proved the porcelain parsing
//! works, worktree semantics are fiddly enough to get subtly wrong when
//! reimplemented, and none of this is a hot path -- it runs once per refresh,
//! not once per keystroke.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model::{GitState, Merged};

pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;

    // A non-zero exit is routine here -- `rev-parse @{upstream}` on a branch
    // with no upstream, for one -- so callers decide what an error means.
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn ok(dir: &Path, args: &[&str]) -> Option<String> {
    run(dir, args).ok()
}

/// The remote-tracking ref for the base branch, preferring the remote's own
/// `main` over the local one: a local `main` that has not been pulled in a week
/// would understate every "behind" count in the table.
pub fn base_ref(repo: &Path) -> String {
    for remote in ["origin", "upstream"] {
        let r = format!("refs/remotes/{remote}/main");
        if run(repo, &["show-ref", "--verify", "--quiet", &r]).is_ok() {
            return format!("{remote}/main");
        }
    }
    "main".to_string()
}

/// Every registered worktree, as branch -> path.
///
/// Submodules keep their own worktrees under `.git/`, which are not ours to
/// manage, so they are filtered out the way `wt.fish` did.
pub fn worktrees(repo: &Path) -> Result<HashMap<String, PathBuf>> {
    let out = run(repo, &["worktree", "list", "--porcelain"])?;
    let mut map = HashMap::new();
    let mut path: Option<PathBuf> = None;

    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
            if let Some(p) = &path {
                if !p.to_string_lossy().contains("/.git/") {
                    map.insert(rest.to_string(), p.clone());
                }
            }
        }
    }
    Ok(map)
}

/// Local branches with their tip sha, so orphans can be found by set difference
/// against `worktrees()`.
pub fn local_branches(repo: &Path) -> Result<Vec<(String, String)>> {
    let out = run(
        repo,
        &["for-each-ref", "--format=%(refname:short)%09%(objectname)", "refs/heads"],
    )?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect())
}

/// Read everything about one branch. `dir` is the worktree for a materialized
/// row; for a virtual one it is the main repo and only branch-level facts (which
/// need no checkout) are filled in.
pub fn state(dir: &Path, branch: &str, base: &str, materialized: bool) -> GitState {
    let mut st = GitState {
        branch: branch.to_string(),
        ..Default::default()
    };

    st.head = ok(dir, &["rev-parse", "--short", branch]).unwrap_or_default();

    // One rev-list for both directions. Note the order: `--left-right` on
    // `base...branch` reports left (base-only, i.e. behind) then right
    // (branch-only, i.e. ahead), and getting these backwards is an easy way to
    // tell someone their branch is 33 commits ahead of main.
    let range = format!("{base}...{branch}");
    if let Some(counts) = ok(dir, &["rev-list", "--left-right", "--count", &range]) {
        let mut it = counts.split_whitespace();
        st.behind = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        st.ahead = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    }

    st.last_commit = ok(dir, &["log", "-1", "--format=%ct", branch])
        .and_then(|s| s.trim().parse().ok());

    let upstream_ref = format!("{branch}@{{upstream}}");
    st.upstream = ok(dir, &["rev-parse", "--abbrev-ref", &upstream_ref]);
    st.remote_branch = remote_name(branch, st.upstream.as_deref());
    if let Some(up) = &st.upstream {
        let r = format!("{up}..{branch}");
        st.unpushed = ok(dir, &["rev-list", "--count", &r])
            .and_then(|s| s.trim().parse().ok());
    }

    // Working-tree facts exist only where there is a working tree.
    if materialized {
        if let Some(porcelain) = ok(dir, &["status", "--porcelain"]) {
            for line in porcelain.lines() {
                if line.len() >= 2 {
                    let b = line.as_bytes();
                    if b[0] != b' ' && b[0] != b'?' {
                        st.staged += 1;
                    }
                    if b[1] != b' ' || b[0] == b'?' {
                        st.dirty += 1;
                    }
                }
            }
        }
    }

    st
}

/// Bring remote-tracking refs up to date and drop the ones whose branches are
/// gone. Pruning matters as much as fetching here: a branch deleted after its PR
/// merged otherwise keeps a stale `origin/...` ref, and every count measured
/// against it stays frozen at whatever it was the day it merged.
pub fn fetch_prune(repo: &Path) -> Result<()> {
    run(repo, &["fetch", "--quiet", "--prune", "--all"]).map(|_| ())
}

/// The name this branch has on the remote.
///
/// Prefer the configured upstream, which is the truth once the branch has been
/// pushed. Fall back to the convention -- prepend the handle -- so a branch that
/// has never been pushed still matches a PR if one somehow exists, and so `proj`
/// can say what it *would* push to.
pub fn remote_name(branch: &str, upstream: Option<&str>) -> String {
    if let Some(up) = upstream {
        // "origin/brian/foo/bar" -> "brian/foo/bar". Only the first component is
        // the remote; the rest is the branch, slashes and all.
        if let Some((_, rest)) = up.split_once('/') {
            return rest.to_string();
        }
    }
    if branch.starts_with(HANDLE) {
        branch.to_string()
    } else {
        format!("{HANDLE}{branch}")
    }
}

/// The remote-side namespace for this user's branches.
pub const HANDLE: &str = "brian/";

/// Answer the merged question three ways, cheapest and most trustworthy first.
///
/// `git cherry` is the one that matters in this repo: it compares patch-ids, so
/// it still says yes after a squash merge rewrote the history. A branch whose
/// commits are all `-` (equivalent found upstream) is in `main`, whatever
/// ancestry says.
pub fn merged(dir: &Path, branch: &str, base: &str, pr_merged: bool) -> Merged {
    if pr_merged {
        return Merged::Pr;
    }

    let arg = format!("{base}");
    if run(dir, &["merge-base", "--is-ancestor", branch, &arg]).is_ok() {
        return Merged::Ancestor;
    }

    if let Some(out) = ok(dir, &["cherry", base, branch]) {
        let mut any = false;
        for line in out.lines() {
            if line.is_empty() {
                continue;
            }
            any = true;
            // `+` means no equivalent commit upstream, so this branch still has
            // something main does not.
            if line.starts_with('+') {
                return Merged::No;
            }
        }
        if any {
            return Merged::Equivalent;
        }
    }

    Merged::No
}
