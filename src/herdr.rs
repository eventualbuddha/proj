//! Mirrors projects and workstreams into herdr's sidebar, and opens workstreams
//! as herdr workspaces.

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::discover::projects_root;
use crate::model::{CheckState, Project, Review, Workstream};
use crate::repo::code_root;

const SOURCE: &str = "proj";
/// Every workspace token this module owns, so stale ones can be cleared.
const TOKENS: [&str; 5] = ["pr", "checks", "review", "stack", "local"];

fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDR_SOCKET_PATH") {
        return p.into();
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"))
        .join("herdr")
        .join("herdr.sock")
}

/// Whether there is a herdr server to talk to.
pub fn available() -> bool {
    std::env::var_os("PROJ_NO_HERDR").is_none() && socket_path().exists()
}

fn call(method: &str, params: Value) -> Result<Value> {
    let mut s = UnixStream::connect(socket_path()).context("connecting to herdr")?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(
        s,
        "{}",
        json!({"id": SOURCE, "method": method, "params": params})
    )?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line)?;
    let mut res: Value = serde_json::from_str(&line).context("herdr reply")?;
    if let Some(e) = res.get("error") {
        bail!(
            "herdr {method}: {}",
            e["message"].as_str().unwrap_or("failed")
        );
    }
    Ok(res["result"].take())
}

/// Logs and drops a failed call; one bad workspace should not stop the rest.
fn try_call(method: &str, params: Value) {
    if let Err(e) = call(method, params) {
        eprintln!("proj: {e:#}");
    }
}

/// Where a herdr workspace sits in the project tree.
enum Place<'a> {
    Workstream(&'a Project, &'a Workstream),
    Project(&'a Project),
    /// Its worktree was deleted out from under it.
    Gone(&'a Project, String),
}

fn place<'a>(projects: &'a [Project], cwd: &Path) -> Option<Place<'a>> {
    let deepest = projects
        .iter()
        .flat_map(|p| p.workstreams.iter().map(move |w| (p, w)))
        .filter_map(|(p, w)| Some((p, w, w.path.as_ref()?)))
        .filter(|(_, _, path)| cwd.starts_with(path))
        .max_by_key(|(_, _, path)| path.components().count());
    if let Some((p, w, _)) = deepest {
        return Some(Place::Workstream(p, w));
    }

    if let Ok(rel) = cwd.strip_prefix(projects_root()) {
        let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy());
        let slug = parts.next()?;
        let p = projects.iter().find(|p| p.slug == slug)?;
        return Some(match parts.next() {
            Some(name) if name.ends_with(" (deleted)") || !cwd.exists() => {
                Place::Gone(p, name.trim_end_matches(" (deleted)").to_string())
            }
            _ => Place::Project(p),
        });
    }

    // A canonical clone belongs to a project only when exactly one project
    // works in that repo.
    let rel = cwd.strip_prefix(code_root()).ok()?;
    let repo = rel.components().next()?.as_os_str().to_string_lossy();
    let mut owners = projects
        .iter()
        .filter(|p| p.repos.first().is_some_and(|r| *r == repo));
    let p = owners.next()?;
    owners.next().is_none().then_some(Place::Project(p))
}

impl Place<'_> {
    fn project(&self) -> &Project {
        match self {
            Place::Workstream(p, _) | Place::Project(p) | Place::Gone(p, _) => p,
        }
    }

    fn name(&self) -> Option<&str> {
        match self {
            Place::Workstream(_, w) => Some(&w.name),
            Place::Gone(_, name) => Some(name),
            Place::Project(_) => None,
        }
    }

    fn label(&self) -> String {
        let p = self.project();
        match self.name() {
            Some(name) => format!("{} {}/{}", p.emoji, p.slug, name),
            None => format!("{} {}", p.emoji, p.slug),
        }
    }

    fn tokens(&self, reviews: &[Review]) -> Map<String, Value> {
        let mut t = Map::new();
        let w = match self {
            Place::Workstream(_, w) => w,
            Place::Gone(..) => {
                t.insert("local".into(), "worktree gone".into());
                return t;
            }
            Place::Project(_) => return t,
        };

        let mut local = Vec::new();
        if let Some(op) = &w.git.op {
            local.push(op.label());
        }
        if w.git.dirty > 0 {
            local.push(format!("✎{}", w.git.dirty));
        }
        if let Some(n) = w.git.unpushed.filter(|n| *n > 0) {
            local.push(format!("↑{n} unpushed"));
        }
        if !local.is_empty() {
            t.insert("local".into(), local.join(" ").into());
        }

        let checks = if let Some(pr) = &w.pr {
            t.insert(
                "pr".into(),
                format!("#{} {}", pr.number, pr.state.label().to_lowercase()).into(),
            );
            let review = match pr.review_decision.as_deref() {
                Some("APPROVED") => Some("approved"),
                Some("CHANGES_REQUESTED") => Some("changes"),
                Some("REVIEW_REQUIRED") => Some("needs review"),
                _ => None,
            };
            if let Some(r) = review {
                t.insert("review".into(), r.into());
            }
            if !pr.base.is_empty() && pr.base != "main" {
                t.insert("stack".into(), format!("on {}", pr.base).into());
            }
            pr.checks.state
        } else if let Some(r) = reviews.iter().find(|r| r.worktree == w.path) {
            t.insert("pr".into(), format!("#{} by {}", r.number, r.author).into());
            r.checks.state
        } else {
            CheckState::None
        };
        if checks != CheckState::None {
            t.insert("checks".into(), checks.label().into());
        }
        t
    }

    /// Sidebar order: projects in proj's order, each project's root first.
    fn order(&self, projects: &[Project]) -> (u8, usize, bool, String) {
        let idx = projects
            .iter()
            .position(|p| p.slug == self.project().slug)
            .unwrap_or(usize::MAX);
        let gone = matches!(self, Place::Gone(..));
        (
            u8::from(gone) * 2,
            idx,
            self.name().is_some(),
            self.name().unwrap_or_default().to_string(),
        )
    }
}

struct Ws<'a> {
    id: String,
    label: String,
    tokens: Value,
    /// The pane whose cwd decides where the workspace belongs: the first agent,
    /// else the first pane.
    cwd: Option<PathBuf>,
    agents: Vec<&'a Value>,
}

fn workspaces(snap: &Value) -> Vec<Ws<'_>> {
    let panes = snap["panes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    snap["workspaces"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|ws| {
            let id = ws["workspace_id"].as_str().unwrap_or_default().to_string();
            let mine: Vec<&Value> = panes
                .iter()
                .filter(|p| p["workspace_id"].as_str() == Some(&id))
                .collect();
            let agents: Vec<&Value> = mine
                .iter()
                .copied()
                .filter(|p| p["agent"].is_string())
                .collect();
            let cwd = agents
                .first()
                .or(mine.first())
                .and_then(|p| p["cwd"].as_str())
                .map(PathBuf::from);
            Ws {
                label: ws["label"].as_str().unwrap_or_default().to_string(),
                tokens: ws["tokens"].clone(),
                id,
                cwd,
                agents,
            }
        })
        .collect()
}

/// The working directory of every pane.
pub fn cwds() -> Result<Vec<PathBuf>> {
    let snap = call("session.snapshot", json!({}))?;
    Ok(snap["snapshot"]["panes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p["cwd"].as_str().map(PathBuf::from))
        .collect())
}

/// Bring herdr's labels, metadata and order in line with `projects`. Only
/// what differs is sent.
pub fn sync(projects: &[Project], reviews: &[Review]) -> Result<()> {
    let snap = call("session.snapshot", json!({}))?;
    let all = workspaces(&snap["snapshot"]);

    let mut order = Vec::new();
    for (i, ws) in all.iter().enumerate() {
        let place = ws.cwd.as_deref().and_then(|c| place(projects, c));
        let Some(place) = place else {
            order.push(((1, i, false, String::new()), ws.id.clone()));
            continue;
        };
        order.push((place.order(projects), ws.id.clone()));

        let label = place.label();
        if ws.label != label {
            try_call(
                "workspace.rename",
                json!({"workspace_id": ws.id, "label": label}),
            );
        }

        let want = place.tokens(reviews);
        let patch: Map<String, Value> = TOKENS
            .iter()
            .filter(|k| ws.tokens.get(**k) != want.get(**k))
            .map(|k| (k.to_string(), want.get(*k).cloned().unwrap_or(Value::Null)))
            .collect();
        if !patch.is_empty() {
            try_call(
                "workspace.report_metadata",
                json!({"workspace_id": ws.id, "source": SOURCE, "tokens": patch}),
            );
        }

        let p = place.project();
        let display = place.name().unwrap_or(&p.slug).to_string();
        let project = format!("{} {}", p.emoji, p.name);
        for pane in &ws.agents {
            let t = &pane["tokens"];
            if t["workstream"].as_str() == Some(&display) && t["project"].as_str() == Some(&project)
            {
                continue;
            }
            try_call(
                "pane.report_metadata",
                json!({
                    "pane_id": pane["pane_id"],
                    "source": SOURCE,
                    "display_agent": display,
                    "tokens": {"project": project, "workstream": display},
                }),
            );
        }
    }

    let current: Vec<String> = order.iter().map(|(_, id)| id.clone()).collect();
    order.sort_by(|a, b| a.0.cmp(&b.0));
    let wanted: Vec<String> = order.into_iter().map(|(_, id)| id).collect();
    if wanted != current {
        try_call("workspace.move_block", json!({"workspace_ids": wanted}));
    }
    Ok(())
}

/// Focus the workspace already at `path`, or create one there.
pub fn open(projects: &[Project], reviews: &[Review], path: &Path) -> Result<()> {
    let snap = call("session.snapshot", json!({}))?;
    let existing = workspaces(&snap["snapshot"])
        .into_iter()
        .find(|ws| ws.cwd.as_deref().is_some_and(|c| c.starts_with(path)));
    match existing {
        Some(ws) => {
            call("workspace.focus", json!({"workspace_id": ws.id}))?;
        }
        None => {
            let label = place(projects, path).map_or_else(
                || {
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                },
                |p| p.label(),
            );
            call(
                "workspace.create",
                json!({"cwd": path, "label": label, "focus": true}),
            )?;
        }
    }
    sync(projects, reviews)
}
