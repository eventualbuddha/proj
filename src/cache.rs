//! On-disk snapshot of the last scan, so a relaunch draws before it re-scans.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::model::*;

#[derive(Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub saved_at: u64,
    pub projects: Vec<P>,
}

#[derive(Serialize, Deserialize)]
pub struct P {
    pub slug: String,
    pub emoji: String,
    pub name: String,
    pub status: String,
    pub readme: String,
    pub workstreams: Vec<W>,
}

#[derive(Serialize, Deserialize)]
pub struct W {
    pub name: String,
    pub path: Option<String>,
    pub branch: String,
    pub remote_branch: String,
    pub ahead: u32,
    pub behind: u32,
    pub dirty: u32,
    pub staged: u32,
    pub upstream: Option<String>,
    pub unpushed: Option<u32>,
    pub head: String,
    pub merged: u8,
}

fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache"));
    base.join("proj").join("scan.json")
}

pub fn save(projects: &[Project]) {
    let snap = Snapshot {
        saved_at: crate::github::now(),
        projects: projects
            .iter()
            .filter(|p| p.slug != crate::discover::UNFILED)
            .map(|p| P {
                slug: p.slug.clone(),
                emoji: p.emoji.clone(),
                name: p.name.clone(),
                status: p.status.clone(),
                readme: p.readme.display().to_string(),
                workstreams: p
                    .workstreams
                    .iter()
                    .map(|w| W {
                        name: w.name.clone(),
                        path: w.path.as_ref().map(|p| p.display().to_string()),
                        branch: w.git.branch.clone(),
                        remote_branch: w.git.remote_branch.clone(),
                        ahead: w.git.ahead,
                        behind: w.git.behind,
                        dirty: w.git.dirty,
                        staged: w.git.staged,
                        upstream: w.git.upstream.clone(),
                        unpushed: w.git.unpushed,
                        head: w.git.head.clone(),
                        merged: w.merged as u8,
                    })
                    .collect(),
            })
            .collect(),
    };
    let Ok(text) = serde_json::to_string(&snap) else {
        return;
    };
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Write-then-rename: several instances share this file and a half-written
    // one must never be read.
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

pub fn load() -> Option<Vec<Project>> {
    let snap: Snapshot = serde_json::from_str(&std::fs::read_to_string(path()).ok()?).ok()?;
    Some(
        snap.projects
            .into_iter()
            .map(|p| Project {
                slug: p.slug.clone(),
                emoji: p.emoji,
                name: p.name,
                kind: ProjectKind::Project,
                status: p.status,
                readme: PathBuf::from(p.readme),
                branch_prefix: None,
                branch_globs: Vec::new(),
                workstreams: p
                    .workstreams
                    .into_iter()
                    .map(|w| Workstream {
                        project: p.slug.clone(),
                        name: w.name,
                        origin: if w.path.is_some() {
                            Origin::Worktree
                        } else {
                            Origin::OrphanBranch
                        },
                        path: w.path.map(PathBuf::from),
                        git: GitState {
                            branch: w.branch,
                            remote_branch: w.remote_branch,
                            ahead: w.ahead,
                            behind: w.behind,
                            dirty: w.dirty,
                            staged: w.staged,
                            upstream: w.upstream,
                            unpushed: w.unpushed,
                            head: w.head,
                            last_commit: None,
                            op: None,
                        },
                        merged: match w.merged {
                            1 => Merged::Pr,
                            2 => Merged::Ancestor,
                            3 => Merged::Equivalent,
                            _ => Merged::No,
                        },
                        pr: None,
                    })
                    .collect(),
            })
            .collect(),
    )
}
