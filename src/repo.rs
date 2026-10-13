//! Canonical clones: one per repo, at `~/code/<repo>`, named by the `repos:` in
//! each project's README. Which GitHub repo a clone is comes from its `origin`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub fn code_root() -> PathBuf {
    std::env::var_os("PROJ_CODE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").expect("HOME is not set")).join("code")
        })
}

/// The canonical clone of `repo`.
pub fn path(repo: &str) -> PathBuf {
    code_root().join(repo)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHub {
    pub owner: String,
    pub name: String,
}

impl GitHub {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

static GITHUB: Mutex<Option<HashMap<String, Option<GitHub>>>> = Mutex::new(None);

/// The GitHub repo behind `repo`'s canonical clone, or `None` if it has no
/// GitHub `origin`.
pub fn github(repo: &str) -> Option<GitHub> {
    if let Ok(memo) = GITHUB.lock() {
        if let Some(hit) = memo.as_ref().and_then(|m| m.get(repo)) {
            return hit.clone();
        }
    }
    let found = crate::git::run(&path(repo), &["remote", "get-url", "origin"])
        .ok()
        .and_then(|url| parse_github_url(&url));
    if let Ok(mut memo) = GITHUB.lock() {
        memo.get_or_insert_with(HashMap::new)
            .insert(repo.to_string(), found.clone());
    }
    found
}

#[cfg(test)]
pub fn remember(repo: &str, gh: GitHub) {
    GITHUB
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(repo.to_string(), Some(gh));
}

/// `owner/name` from an https, ssh or scp-style GitHub url.
fn parse_github_url(url: &str) -> Option<GitHub> {
    let url = url.trim();
    let rest = [
        "https://github.com/",
        "http://github.com/",
        "ssh://git@github.com/",
        "git@github.com:",
    ]
    .iter()
    .find_map(|p| url.strip_prefix(p))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(GitHub {
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

/// The repo a worktree belongs to: the directory name of the clone its `.git`
/// file points back into.
pub fn of_worktree(dir: &Path) -> Option<String> {
    let dot_git = dir.join(".git");
    let clone = if dot_git.is_dir() {
        dir.to_path_buf()
    } else {
        // `gitdir: <clone>/.git/worktrees/<id>`
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let gitdir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
        gitdir.parent()?.parent()?.parent()?.to_path_buf()
    };
    Some(clone.file_name()?.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_urls_of_every_shape_parse() {
        let want = Some(GitHub {
            owner: "votingworks".into(),
            name: "arlo".into(),
        });
        for url in [
            "https://github.com/votingworks/arlo",
            "https://github.com/votingworks/arlo.git",
            "https://github.com/votingworks/arlo/",
            "git@github.com:votingworks/arlo.git",
            "ssh://git@github.com/votingworks/arlo.git",
        ] {
            assert_eq!(parse_github_url(url), want, "{url}");
        }
        assert_eq!(
            parse_github_url("https://gitlab.com/votingworks/arlo"),
            None
        );
        assert_eq!(parse_github_url("https://github.com/votingworks"), None);
    }

    #[test]
    fn a_worktree_names_the_clone_it_was_added_from() {
        let tmp = std::env::temp_dir().join(format!("proj-repo-test-{}", std::process::id()));
        let wt = tmp.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            "gitdir: /home/x/code/arlo/.git/worktrees/wt\n",
        )
        .unwrap();
        assert_eq!(of_worktree(&wt).as_deref(), Some("arlo"));

        let clone = tmp.join("vxsuite");
        std::fs::create_dir_all(clone.join(".git")).unwrap();
        assert_eq!(of_worktree(&clone).as_deref(), Some("vxsuite"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
