//! Finding projects and workstreams on disk, and pairing them with the branches
//! that have no directory.
//!
//! The filesystem is the registry: no index file, no database. A project
//! directory moved or renamed by hand is still correct on the next run, which is
//! the property that makes the layout worth having in the first place.

use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::git;
use crate::model::*;

pub const UNFILED: &str = "unfiled";

pub fn projects_root() -> PathBuf {
    std::env::var_os("PROJECTS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("projects"))
}

pub fn repo_path() -> PathBuf {
    std::env::var_os("PROJ_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("code/vxsuite"))
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME is not set"))
}

/// Read the leading `---` fenced block of a README as key/value pairs.
///
/// A hand-rolled reader for a documented, hand-written schema of scalar keys and
/// one-line lists. Pulling in a YAML parser to read six keys would be a
/// dependency and a build-time cost for no behaviour we need -- and the schema
/// lives in ~/projects/README.md, not in whatever a parser would accept.
fn frontmatter(readme: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(readme) else {
        return Vec::new();
    };
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return Vec::new();
    }

    let mut out = Vec::new();
    for line in lines {
        if line.trim_end() == "---" {
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim().trim_matches('"').trim_matches('\'');
        out.push((k.trim().to_string(), v.to_string()));
    }
    out
}

fn get<'a>(fm: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fm.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Parse a one-line inline list: `["a", "b"]` or `[a, b]`.
fn list(fm: &[(String, String)], key: &str) -> Vec<String> {
    let Some(raw) = get(fm, key) else {
        return Vec::new();
    };
    raw.trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Does `branch` match a glob from a project's `branches:` list?
///
/// Only `*` is supported, which covers every pattern the schema needs; a real
/// glob crate would be a dependency for prefix matching.
fn glob_match(pattern: &str, s: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == s,
        Some((pre, post)) => {
            s.len() >= pre.len() + post.len() && s.starts_with(pre) && s.ends_with(post)
        }
    }
}

/// Scan the root, read git, and return every project with its rows.
pub fn scan() -> Result<Vec<Project>> {
    let root = projects_root();
    let repo = repo_path();
    let base = git::base_ref(&repo);

    let worktrees = git::worktrees(&repo).unwrap_or_default();
    let branches = git::local_branches(&repo).unwrap_or_default();

    let mut projects: Vec<Project> = Vec::new();

    let mut entries: Vec<_> = std::fs::read_dir(&root)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let dir = entry.path();
        let readme = dir.join("README.md");
        // A project is a directory with a README.md. Anything else under the
        // root is not ours.
        if !readme.is_file() {
            continue;
        }

        let slug = entry.file_name().to_string_lossy().to_string();
        let fm = frontmatter(&readme);

        let mut project = Project {
            emoji: get(&fm, "emoji").unwrap_or("📁").to_string(),
            name: get(&fm, "name").unwrap_or(&slug).to_string(),
            kind: match get(&fm, "kind") {
                Some("review") => ProjectKind::Review,
                _ => ProjectKind::Project,
            },
            status: get(&fm, "status").unwrap_or("active").to_string(),
            branch_prefix: get(&fm, "branch-prefix").map(str::to_string),
            branch_globs: list(&fm, "branches"),
            readme,
            slug,
            workstreams: Vec::new(),
        };

        // Materialized rows: directories under the project that are worktrees.
        let mut sub: Vec<_> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir() && e.path().join(".git").exists())
            .collect();
        sub.sort_by_key(|e| e.file_name());

        for ws in sub {
            let path = ws.path();
            let name = ws.file_name().to_string_lossy().to_string();

            // Mid-rebase the worktree is detached and `rev-parse --abbrev-ref
            // HEAD` answers "HEAD", so the branch name has to come from the
            // sequencer state instead. Without it the row is nameless and the
            // branch shows up a second time as an orphan.
            let op = git::in_progress(&path);
            let branch = op
                .as_ref()
                .and_then(|o| o.branch.clone())
                .or_else(|| git::run(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).ok())
                .unwrap_or_else(|| "HEAD".into());

            let mut git_state = git::state(&path, &branch, &base, true);
            git_state.op = op;

            project.workstreams.push(Workstream {
                project: project.slug.clone(),
                name,
                origin: Origin::Worktree,
                path: Some(path),
                git: git_state,
                merged: Merged::No,
                pr: None,
            });
        }

        projects.push(project);
    }

    // Virtual rows: local branches with no worktree, claimed by whichever
    // project's prefix or globs match. This is the case that motivated the
    // split -- `brian/esm-lib-batch-4` is six commits and a draft PR with no
    // directory, and without this it renders nowhere.
    let checked_out: HashSet<&String> = worktrees.keys().collect();
    for (branch, _sha) in &branches {
        if checked_out.contains(branch) {
            continue;
        }
        // `main` is the base, not a workstream.
        if branch == "main" {
            continue;
        }

        // No project claims it -- but dropping it here is exactly the blindness
        // this tool exists to remove, so it goes to a synthetic "unfiled"
        // project instead. An unfiled row is a prompt: file it, or delete it.
        let idx = match claim(&projects, branch) {
            Some(i) => i,
            None => unfiled(&mut projects),
        };

        let git_state = git::state(&repo, branch, &base, false);
        let name = short_name(&projects[idx], branch);
        let slug = projects[idx].slug.clone();
        projects[idx].workstreams.push(Workstream {
            project: slug,
            name,
            origin: Origin::OrphanBranch,
            path: None,
            git: git_state,
            merged: Merged::No,
            pr: None,
        });
    }

    // Unfiled sorts last: it is a to-do list, not a project.
    if let Some(i) = projects.iter().position(|p| p.slug == UNFILED) {
        let u = projects.remove(i);
        projects.push(u);
    }

    for p in &mut projects {
        // Virtual rows sort after materialized ones, then by name: the things
        // you can act on directly come first.
        p.workstreams
            .sort_by(|a, b| a.is_virtual().cmp(&b.is_virtual()).then(a.name.cmp(&b.name)));
    }

    Ok(projects)
}

/// Index of the synthetic "unfiled" project, creating it on first use.
///
/// Not a directory on disk and deliberately so: it has no README to write, and
/// materialising one would turn a prompt into a project.
fn unfiled(projects: &mut Vec<Project>) -> usize {
    if let Some(i) = projects.iter().position(|p| p.slug == UNFILED) {
        return i;
    }
    projects.push(Project {
        slug: UNFILED.to_string(),
        emoji: "❓".to_string(),
        name: "Branches with no project".to_string(),
        kind: ProjectKind::Project,
        status: "unfiled".to_string(),
        readme: PathBuf::new(),
        branch_prefix: None,
        branch_globs: Vec::new(),
        workstreams: Vec::new(),
    });
    projects.len() - 1
}

/// Which project should own an orphan branch?
///
/// A local branch is named `<project>/<workstream>`, so the first component is
/// the answer and no guessing is required. The glob and prefix paths below are
/// the fallback for branches predating the convention -- an escape hatch, not
/// the mechanism. `branch-prefix: brian/` as a catch-all once swept four
/// unrelated branches into one project, which is what the exact match prevents.
fn claim(projects: &[Project], branch: &str) -> Option<usize> {
    if let Some((head, rest)) = branch.split_once('/') {
        if !rest.is_empty() {
            if let Some(i) = projects.iter().position(|p| p.slug == head) {
                return Some(i);
            }
        }
    }

    let mut best: Option<(usize, usize)> = None;
    for (i, p) in projects.iter().enumerate() {
        for g in &p.branch_globs {
            if glob_match(g, branch) {
                return Some(i);
            }
        }
        if let Some(prefix) = &p.branch_prefix {
            if branch.starts_with(prefix.as_str())
                && best.map_or(true, |(_, len)| prefix.len() > len)
            {
                best = Some((i, prefix.len()));
            }
        }
    }
    best.map(|(i, _)| i)
}

/// The name to show for a virtual row.
///
/// Under the naming convention this is exact: `esm-migration/lib-batch-4` is the
/// workstream `lib-batch-4`, which is also what its directory would be called,
/// so a virtual row sits next to its materialized siblings under the same name
/// it will keep when it gets a worktree.
fn short_name(project: &Project, branch: &str) -> String {
    if let Some(rest) = branch.strip_prefix(&format!("{}/", project.slug)) {
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    if let Some(prefix) = &project.branch_prefix {
        if let Some(rest) = branch.strip_prefix(prefix.as_str()) {
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    branch.rsplit('/').next().unwrap_or(branch).to_string()
}
