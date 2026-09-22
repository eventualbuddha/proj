//! Git state, read by shelling out to `git`.
//!
//! Deliberately not gix or git2. `wt.fish` already proved the porcelain parsing
//! works, worktree semantics are fiddly enough to get subtly wrong when
//! reimplemented, and none of this is a hot path -- it runs once per refresh,
//! not once per keystroke.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::model::{GitState, Merged, Op, OpKind};

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

/// A sequencer operation left in progress in this worktree.
///
/// These matter because they *detach HEAD*. Mid-rebase, `git worktree list`
/// reports the worktree as detached rather than on a branch, so the row loses
/// its branch name and the branch -- now attached to no worktree -- gets picked
/// up separately as an orphan. The same workstream then appears twice, once as a
/// nameless worktree and once as a branch with no worktree, and neither row is
/// true. Reading the sequencer state is what puts the name back.
pub fn in_progress(dir: &Path) -> Option<Op> {
    let git_path = |name: &str| ok(dir, &["rev-parse", "--git-path", name]).map(PathBuf::from);

    // rebase-merge is the interactive/merge backend, rebase-apply the am one.
    for (name, kind) in [
        ("rebase-merge", OpKind::Rebase),
        ("rebase-apply", OpKind::Rebase),
    ] {
        let Some(p) = git_path(name) else { continue };
        if !p.is_dir() {
            continue;
        }
        let read = |f: &str| std::fs::read_to_string(p.join(f)).ok();
        // rebase-apply names its counters `next` and `last`; rebase-merge uses
        // `msgnum` and `end`.
        let done = read("msgnum")
            .or_else(|| read("next"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let total = read("end")
            .or_else(|| read("last"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let branch = read("head-name")
            .map(|v| v.trim().trim_start_matches("refs/heads/").to_string())
            .filter(|v| !v.is_empty());
        return Some(Op {
            kind,
            done,
            total,
            branch,
        });
    }

    for (name, kind) in [
        ("MERGE_HEAD", OpKind::Merge),
        ("CHERRY_PICK_HEAD", OpKind::CherryPick),
        ("REVERT_HEAD", OpKind::Revert),
        ("BISECT_LOG", OpKind::Bisect),
    ] {
        if git_path(name).is_some_and(|p| p.exists()) {
            return Some(Op {
                kind,
                done: 0,
                total: 0,
                branch: None,
            });
        }
    }

    None
}

/// Every registered worktree, as branch -> path.
///
/// Submodules keep their own worktrees under `.git/`, which are not ours to
/// manage, so they are filtered out the way `wt.fish` did.
pub fn worktrees(repo: &Path) -> Result<HashMap<String, PathBuf>> {
    let out = run(repo, &["worktree", "list", "--porcelain"])?;
    let mut map = HashMap::new();
    let mut path: Option<PathBuf> = None;
    let mut detached: Vec<PathBuf> = Vec::new();

    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
            if let Some(p) = &path {
                if !p.to_string_lossy().contains("/.git/") {
                    map.insert(rest.to_string(), p.clone());
                }
            }
        } else if line == "detached" {
            if let Some(p) = &path {
                if !p.to_string_lossy().contains("/.git/") {
                    detached.push(p.clone());
                }
            }
        }
    }

    // A worktree part-way through a rebase is detached, but the branch it is
    // rebasing is still spoken for. Without this it would be reported as having
    // no worktree and listed a second time as an orphan branch.
    for p in detached {
        if let Some(branch) = in_progress(&p).and_then(|op| op.branch) {
            map.insert(branch, p);
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
    // The configured push target, which outlives the remote-tracking ref: a
    // merged branch is deleted on the remote, the next prune drops
    // `origin/<name>`, and `@{upstream}` stops resolving. Reading only that ref
    // would leave a merged workstream's remote name guessed rather than known,
    // and the guess misses its PR.
    let configured = ok(dir, &["config", "--get", &format!("branch.{branch}.merge")]);
    st.pushed = configured.is_some();
    st.remote_branch = remote_name(branch, st.upstream.as_deref(), configured.as_deref());
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
pub fn remote_name(branch: &str, upstream: Option<&str>, configured: Option<&str>) -> String {
    // `branch.<name>.merge`, a full ref. Preferred over the tracking ref because
    // it is still there once the remote branch is gone.
    if let Some(rest) = configured.and_then(|c| c.strip_prefix("refs/heads/")) {
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
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

/// Answer the merged question four ways, cheapest and most trustworthy first.
///
/// Patch-ids are what matter in this repo, since a squash merge rewrites the
/// history and ancestry then says no about every merged branch. `git cherry`
/// asks that question one commit at a time, which only settles a branch that
/// squashed down to a single commit; `squashed_into` asks it of the branch's
/// whole diff, which is what a squash of several commits actually becomes.
///
/// A merged PR is reported as merged only when nothing local contradicts it. It
/// is the *PR* that merged, not the branch: commits pushed after the merge, or
/// never pushed at all, sit on the branch with no equivalent in main, and
/// calling that "merged" is how a delete confirmation ends up vouching for work
/// that only exists here.
pub fn merged(dir: &Path, branch: &str, base: &str, pr_merged: bool) -> Merged {
    let arg = base.to_string();
    if run(dir, &["merge-base", "--is-ancestor", branch, &arg]).is_ok() {
        return if pr_merged { Merged::Pr } else { Merged::Ancestor };
    }

    if let Some(out) = ok(dir, &["cherry", base, branch]) {
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        // `+` means no equivalent *commit* upstream. On its own that is not an
        // answer -- a branch squashed into one commit has a `+` against every
        // commit it was built from -- so the whole-branch patch gets a say
        // before this reports anything as unmerged.
        if !lines.is_empty() {
            if !lines.iter().any(|l| l.starts_with('+')) {
                // Every commit has an equivalent in main. A rebase-and-merge
                // looks exactly like this, and so does a one-commit squash.
                return if pr_merged { Merged::Pr } else { Merged::Equivalent };
            }
            if !squashed_into(dir, branch, base) {
                return Merged::No;
            }
            return if pr_merged { Merged::Pr } else { Merged::Equivalent };
        }
    }

    // No local answer -- `cherry` failed, or the branch is unreadable from here.
    // GitHub's is the only one left.
    if pr_merged {
        Merged::Pr
    } else {
        Merged::No
    }
}

/// The patch-id of a diff produced by `git <args>`, as `git patch-id --stable`
/// computes it.
fn patch_id(dir: &Path, args: &[&str]) -> Option<String> {
    let diff = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !diff.status.success() || diff.stdout.is_empty() {
        return None;
    }

    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["patch-id", "--stable"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // The answer is one short line, so the pipe cannot fill while `git
    // patch-id` is still reading its input.
    child.stdin.take()?.write_all(&diff.stdout).ok()?;
    let out = child.wait_with_output().ok()?;

    let text = String::from_utf8_lossy(&out.stdout);
    let id = text.split_whitespace().next()?;
    (!id.is_empty()).then(|| id.to_string())
}

/// Whether the branch landed on `base` as a single squashed commit.
///
/// `git cherry` compares one commit at a time, so a squash of more than one
/// commit defeats it: none of the branch's commits has an equivalent patch
/// upstream, only their sum does. That sum is what this looks for -- the
/// patch-id of the branch's whole diff, against the commits on `base` that
/// touched the same files.
fn squashed_into(dir: &Path, branch: &str, base: &str) -> bool {
    let Some(mb) = ok(dir, &["merge-base", base, branch]) else {
        return false;
    };
    let Some(want) = patch_id(dir, &["diff", &mb, branch]) else {
        return false;
    };

    // Only a commit that touched the same files can carry the same patch, and
    // on a busy `main` that is the difference between a handful of candidates
    // and every commit since the branch started.
    let files = ok(dir, &["diff", "--name-only", &mb, branch]).unwrap_or_default();
    let range = format!("{mb}..{base}");
    let mut args = vec!["rev-list", &range, "--"];
    args.extend(files.lines());
    let Some(candidates) = ok(dir, &args) else {
        return false;
    };

    candidates
        .lines()
        .any(|sha| patch_id(dir, &["show", sha]).as_deref() == Some(want.as_str()))
}

/// Commits on `dir`'s HEAD that exist neither on `base` nor on a remote.
///
/// This is the question `proj rm` refuses on, so it lives here rather than in
/// the shell: the confirmation the TUI shows and the check that enforces it
/// have to be the same answer, and two implementations of patch-id matching
/// were two chances to disagree.
pub fn stranded(dir: &Path, base: &str) -> Vec<String> {
    // A squash merge of more than one commit lands under a sha of its own, and
    // `git cherry` -- one commit at a time -- calls every commit it was built
    // from stranded. The branch's whole diff is the thing that merged.
    if squashed_into(dir, "HEAD", base) {
        return Vec::new();
    }

    let Some(out) = ok(dir, &["cherry", base, "HEAD"]) else {
        return Vec::new();
    };

    out.lines()
        .filter_map(|l| l.strip_prefix("+ "))
        // Still reachable from the remote-tracking ref, so removing the
        // worktree loses nothing.
        .filter(|sha| run(dir, &["merge-base", "--is-ancestor", sha, "@{upstream}"]).is_err())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch repo, since the whole point of these is what real `git`
    /// does with patch-ids -- a fake would be testing the fake.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("proj-git-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            let s = Scratch(dir);
            s.git(&["init", "--initial-branch=main", "--quiet"]);
            s.git(&["config", "user.email", "t@example.com"]);
            s.git(&["config", "user.name", "T"]);
            s.commit("base", "0\n");
            s
        }

        fn git(&self, args: &[&str]) -> String {
            run(&self.0, args).unwrap_or_else(|e| panic!("git {}: {e}", args.join(" ")))
        }

        fn commit(&self, file: &str, body: &str) {
            std::fs::write(self.0.join(file), body).expect("write");
            self.git(&["add", "-A"]);
            self.git(&["commit", "--quiet", "-m", &format!("add {file}")]);
        }
    }

    /// The case that could not be deleted: five commits, squashed into one.
    /// Every one of them is a `+` to `git cherry`, and none of them is at risk.
    #[test]
    fn a_squash_of_several_commits_is_merged_and_strands_nothing() {
        let s = Scratch::new("squash");
        s.git(&["checkout", "--quiet", "-b", "feature"]);
        for i in 0..5 {
            s.commit(&format!("f{i}"), &format!("{i}\n"));
        }

        s.git(&["checkout", "--quiet", "main"]);
        s.git(&["merge", "--quiet", "--squash", "feature"]);
        s.git(&["commit", "--quiet", "-m", "the whole feature (#1)"]);

        assert!(
            squashed_into(&s.0, "feature", "main"),
            "the branch's whole diff is in main"
        );
        assert_eq!(merged(&s.0, "feature", "main", false), Merged::Equivalent);
        assert_eq!(merged(&s.0, "feature", "main", true), Merged::Pr);

        s.git(&["checkout", "--quiet", "feature"]);
        assert!(stranded(&s.0, "main").is_empty(), "nothing is only here");
    }

    /// The check that must not soften: work that really does live nowhere else.
    #[test]
    fn a_commit_that_landed_nowhere_is_still_stranded() {
        let s = Scratch::new("unmerged");
        s.git(&["checkout", "--quiet", "-b", "feature"]);
        s.commit("mine", "mine\n");

        assert!(!squashed_into(&s.0, "feature", "main"));
        assert_eq!(merged(&s.0, "feature", "main", false), Merged::No);
        // Even GitHub saying the PR merged cannot vouch for this one.
        assert_eq!(merged(&s.0, "feature", "main", true), Merged::No);
        assert_eq!(stranded(&s.0, "main").len(), 1);
    }

    /// A commit pushed after the squash landed. The branch is half in main, and
    /// the half that is not is exactly what a delete would lose.
    #[test]
    fn a_commit_added_after_the_squash_is_stranded() {
        let s = Scratch::new("after");
        s.git(&["checkout", "--quiet", "-b", "feature"]);
        s.commit("f0", "0\n");
        s.commit("f1", "1\n");

        s.git(&["checkout", "--quiet", "main"]);
        s.git(&["merge", "--quiet", "--squash", "feature"]);
        s.git(&["commit", "--quiet", "-m", "the feature (#1)"]);

        s.git(&["checkout", "--quiet", "feature"]);
        s.commit("f2", "2\n");

        assert!(!squashed_into(&s.0, "feature", "main"));
        assert_eq!(merged(&s.0, "feature", "main", true), Merged::No);
        assert_eq!(stranded(&s.0, "main").len(), 3);
    }

    #[test]
    fn the_configured_ref_outranks_the_tracking_ref_and_the_convention() {
        assert_eq!(
            remote_name(
                "backup-restore/bump-test-timeout",
                None,
                Some("refs/heads/backup-restore/bump-test-timeout")
            ),
            "backup-restore/bump-test-timeout",
            "a merged branch keeps its config after the tracking ref is pruned"
        );
        assert_eq!(
            remote_name("p/w", Some("origin/brian/p/w"), None),
            "brian/p/w"
        );
    }

    #[test]
    fn without_either_it_falls_back_to_the_handle() {
        assert_eq!(remote_name("p/w", None, None), "brian/p/w");
        assert_eq!(remote_name("brian/p/w", None, None), "brian/p/w");
        // A config entry that is not a branch ref says nothing useful.
        assert_eq!(remote_name("p/w", None, Some("refs/heads/")), "brian/p/w");
    }
}
