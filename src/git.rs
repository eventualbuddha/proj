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

use crate::model::{GitState, Op, OpKind};

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
    // One `rev-parse` for every marker, since each `--git-path` answers on its
    // own line; asking one at a time was six processes per worktree per scan.
    const MARKERS: [&str; 6] = [
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
    ];
    let mut args = vec!["rev-parse"];
    for m in MARKERS {
        args.push("--git-path");
        args.push(m);
    }
    let out = ok(dir, &args)?;
    let paths: Vec<PathBuf> = out.lines().map(PathBuf::from).collect();
    if paths.len() != MARKERS.len() {
        return None;
    }
    let git_path = |name: &str| {
        let i = MARKERS.iter().position(|m| *m == name)?;
        let p = &paths[i];
        // `--git-path` answers relative to the worktree unless it is absolute.
        Some(if p.is_absolute() {
            p.clone()
        } else {
            dir.join(p)
        })
    };

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

    st.last_commit =
        ok(dir, &["log", "-1", "--format=%ct", branch]).and_then(|s| s.trim().parse().ok());

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
        st.unpushed = ok(dir, &["rev-list", "--count", &r]).and_then(|s| s.trim().parse().ok());
    }

    // Working-tree facts exist only where there is a working tree.
    if materialized {
        add_status(dir, &mut st);
    }

    st
}

/// Fill in the staged and dirty counts of the worktree at `dir`.
pub fn add_status(dir: &Path, st: &mut GitState) {
    let Some(porcelain) = ok(dir, &["status", "--porcelain"]) else {
        return;
    };
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

/// The branch-level half of `state` for every local branch, from one
/// `for-each-ref`. Working-tree counts are left at zero.
pub fn branch_states(repo: &Path, base: &str) -> HashMap<String, GitState> {
    let format = format!(
        "--format=%(refname:short)%09%(objectname:short)%09%(ahead-behind:{base})%09\
         %(committerdate:unix)%09%(upstream:short)%09%(upstream:remoteref)%09\
         %(upstream:track,nobracket)"
    );
    let Some(out) = ok(repo, &["for-each-ref", &format, "refs/heads"]) else {
        return HashMap::new();
    };
    out.lines().filter_map(parse_branch_state).collect()
}

fn parse_branch_state(line: &str) -> Option<(String, GitState)> {
    // `run` trims the output, which takes the last line's empty trailing fields.
    let mut f = line.split('\t');
    let [branch, head, ahead_behind, date, upstream, merge, track] =
        std::array::from_fn(|_| f.next().unwrap_or(""));
    let nonempty = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let (ahead, behind) = ahead_behind.split_once(' ')?;
    // A tracking ref the remote has pruned is no upstream at all, as
    // `@{upstream}` would say.
    let upstream = nonempty(upstream).filter(|_| track != "gone");
    let unpushed = upstream.as_ref().map(|_| {
        track
            .split(", ")
            .find_map(|t| t.strip_prefix("ahead "))
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    });
    let configured = nonempty(merge);
    Some((
        branch.to_string(),
        GitState {
            branch: branch.to_string(),
            head: head.to_string(),
            ahead: ahead.parse().ok()?,
            behind: behind.parse().ok()?,
            last_commit: date.parse().ok(),
            pushed: configured.is_some(),
            remote_branch: remote_name(branch, upstream.as_deref(), configured.as_deref()),
            upstream,
            unpushed,
            ..Default::default()
        },
    ))
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
/// pushed. Fall back to the local name, which is what `proj` would push to.
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
    branch.to_string()
}

/// Run `f` over every item on its own thread and return the results in order.
///
/// Everything here is a `git` child process waiting on disk, so the rows are
/// independent and the machine has cores to spare; walking them one at a time
/// made a scan cost the sum of its rows instead of the slowest one.
pub fn par_map<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(T) -> R + Sync,
{
    let f = &f;
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .into_iter()
            .map(|item| s.spawn(move || f(item)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("a scan thread panicked"))
            .collect()
    })
}

/// Commits on `dir`'s HEAD that exist neither on `base` nor on a remote.
///
/// This is what `proj rm` refuses on, so it lives here rather than in the
/// shell, next to the counts the delete confirmation shows.
pub fn stranded(dir: &Path, base: &str) -> Vec<String> {
    ok(dir, &["rev-list", "HEAD", "--not", base, "--remotes"])
        .map(|out| out.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch repo, since the whole point of these is what real `git`
    /// does -- a fake would be testing the fake.
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

    #[test]
    fn a_commit_that_landed_nowhere_is_stranded() {
        let s = Scratch::new("unmerged");
        s.git(&["checkout", "--quiet", "-b", "feature"]);
        s.commit("mine", "mine\n");
        assert_eq!(stranded(&s.0, "main").len(), 1);
    }

    #[test]
    fn a_commit_on_a_remote_is_not_stranded() {
        let s = Scratch::new("pushed");
        s.git(&["checkout", "--quiet", "-b", "feature"]);
        s.commit("mine", "mine\n");
        s.git(&["update-ref", "refs/remotes/origin/feature", "HEAD"]);
        s.commit("more", "more\n");
        assert_eq!(stranded(&s.0, "main").len(), 1);
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
    fn without_either_it_falls_back_to_the_local_name() {
        assert_eq!(remote_name("p/w", None, None), "p/w");
        assert_eq!(remote_name("brian/p/w", None, None), "brian/p/w");
        // A config entry that is not a branch ref says nothing useful.
        assert_eq!(remote_name("p/w", None, Some("refs/heads/")), "p/w");
    }

    #[test]
    fn branch_states_match_the_per_branch_answers() {
        let r = Scratch::new("branch-states");
        r.git(&["checkout", "-q", "-b", "feat"]);
        r.commit("b", "2");
        r.commit("c", "3");
        let st = &branch_states(&r.0, "main")["feat"];
        let one = state(&r.0, "feat", "main", false);
        assert_eq!((st.ahead, st.behind, &st.head), (2, 0, &one.head));
        assert_eq!((st.ahead, st.behind), (one.ahead, one.behind));
        assert_eq!((&st.upstream, st.unpushed), (&None, None));
        assert_eq!(st.last_commit, one.last_commit);
    }

    #[test]
    fn a_pruned_upstream_is_no_upstream_but_still_names_the_remote_branch() {
        let (_, st) =
            parse_branch_state("p/w\tabc\t1 2\t5\torigin/brian/p/w\trefs/heads/brian/p/w\tgone")
                .unwrap();
        assert_eq!((st.upstream, st.unpushed, st.pushed), (None, None, true));
        assert_eq!(st.remote_branch, "brian/p/w");
    }

    #[test]
    fn trailing_empty_fields_may_be_missing() {
        let (_, st) = parse_branch_state("p/w\tabc\t3 4\t5").unwrap();
        assert_eq!(
            (st.ahead, st.behind, st.upstream, st.pushed),
            (3, 4, None, false)
        );
    }

    #[test]
    fn unpushed_is_the_ahead_count_against_the_upstream() {
        let line = "p/w\tabc\t1 2\t5\torigin/p/w\trefs/heads/p/w\tahead 3, behind 1";
        assert_eq!(parse_branch_state(line).unwrap().1.unpushed, Some(3));
        let line = "p/w\tabc\t1 2\t5\torigin/p/w\trefs/heads/p/w\t";
        assert_eq!(parse_branch_state(line).unwrap().1.unpushed, Some(0));
    }
}
