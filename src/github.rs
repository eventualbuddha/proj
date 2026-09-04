//! PR and CI state, fetched per branch and cached on disk.
//!
//! The query shape is lazygit's (`pkg/commands/git_commands/github.go`): one
//! aliased sub-query per branch --
//!
//!     a1: pullRequests(first: 5, headRefName: $branch1) { ... }
//!     a2: pullRequests(first: 5, headRefName: $branch2) { ... }
//!
//! -- rather than "the newest N pull requests in the repo, hope yours are among
//! them". It asks exactly about the branches that exist here, so the response is
//! small, complete, and independent of how busy the repo is. The first version
//! of this module asked for the newest 60 PRs with all their check contexts; at
//! roughly 6000 nodes GitHub answered with a 504 every single time.
//!
//! `first: 5` per branch, again from lazygit: several forks can have PRs with
//! the same head ref name, so the owner is checked after the fact.
//!
//! Not the REST status endpoint: vxsuite posts ~63 statuses against a default
//! page size of 30, so a REST caller that forgets `per_page` reports a green PR
//! that is not.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::*;

/// Build the aliased multi-branch query and the variables it needs.
fn list_query(branches: &[String]) -> (String, Vec<(String, String)>) {
    let mut vars = vec![
        ("owner".to_string(), String::new()),
        ("repo".to_string(), String::new()),
    ];
    let mut decls = vec!["$owner: String!".to_string(), "$repo: String!".to_string()];
    let mut fields = Vec::new();

    for (i, branch) in branches.iter().enumerate() {
        let var = format!("branch{}", i + 1);
        decls.push(format!("${var}: String!"));
        vars.push((var.clone(), branch.clone()));
        fields.push(format!(
            r#"  a{n}: pullRequests(first: 5, headRefName: ${var}, orderBy: {{field: CREATED_AT, direction: DESC}}) {{
    nodes {{
      number title url state isDraft headRefName reviewDecision
      author {{ login }}
      headRepositoryOwner {{ login }}
      headRef {{ target {{ ... on Commit {{
        statusCheckRollup {{ state contexts {{ totalCount }} }}
      }} }} }}
    }}
  }}"#,
            n = i + 1,
            var = var
        ));
    }

    let query = format!(
        "query({}) {{\n  repository(owner: $owner, name: $repo) {{\n{}\n  }}\n}}",
        decls.join(", "),
        fields.join("\n")
    );
    (query, vars)
}

const CONTEXTS_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              contexts(first: 100) {
                nodes {
                  __typename
                  ... on CheckRun { name conclusion status detailsUrl }
                  ... on StatusContext { context state targetUrl }
                }
              }
            }
          }
        }
      }
    }
  }
}
"#;

/// The on-disk shape. Its own serde types rather than reusing the model, so a
/// change to the model does not silently invalidate every cache file.
#[derive(serde::Serialize, Deserialize, Default)]
pub struct Cache {
    pub fetched_at: u64,
    pub prs: Vec<CachedPr>,
}
#[derive(serde::Serialize, Deserialize, Clone)]
pub struct CachedPr {
    pub branch: String,
    pub number: u32,
    pub state: String,
    pub title: String,
    pub url: String,
    pub author: String,
    pub review_decision: Option<String>,
    pub check_state: String,
    pub check_total: u32,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cache_path(owner: &str, name: &str) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache"));
    base.join("proj").join(format!("gh-{owner}-{name}.json"))
}

pub fn load_cache(owner: &str, name: &str) -> Option<Cache> {
    let text = std::fs::read_to_string(cache_path(owner, name)).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_cache(owner: &str, name: &str, cache: &Cache) -> Result<()> {
    let path = cache_path(owner, name);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string(cache)?)?;
    Ok(())
}

fn gh(args: &[String]) -> Result<Vec<u8>> {
    let out = Command::new("gh")
        .args(args)
        .output()
        .context("running gh; is it installed and authenticated?")?;
    if !out.status.success() {
        anyhow::bail!("gh: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// Hit the API and rewrite the cache. Slow enough to belong on a background
/// thread; `branches` are *remote* names, since `headRefName` is the only name
/// GitHub knows.
pub fn refresh(owner: &str, name: &str, branches: &[String]) -> Result<Cache> {
    let mut prs = Vec::new();

    // Chunked because the query grows with the branch count and a variable list
    // is not free. lazygit uses 10 per request with 5 concurrent; sequential
    // chunks of 20 are well inside what the API is happy with here, and this
    // already runs off the UI thread.
    for chunk in branches.chunks(20) {
        let (query, vars) = list_query(chunk);
        let mut args: Vec<String> = vec![
            "api".into(),
            "graphql".into(),
            "-f".into(),
            format!("query={query}"),
        ];
        for (k, v) in vars {
            let value = match k.as_str() {
                "owner" => owner.to_string(),
                "repo" => name.to_string(),
                _ => v,
            };
            args.push("-F".into());
            args.push(format!("{k}={value}"));
        }

        let body = gh(&args)?;
        let v: serde_json::Value =
            serde_json::from_slice(&body).context("parsing the graphql response")?;
        let repo = &v["data"]["repository"];

        for (i, branch) in chunk.iter().enumerate() {
            let nodes = repo[format!("a{}", i + 1)]["nodes"]
                .as_array()
                .cloned()
                .unwrap_or_default();

            // Several forks can carry a PR with this head ref name; only the one
            // in this repo's own owner is ours. Newest first, so the first match
            // is the live PR for a branch reused across several.
            let Some(node) = nodes.iter().find(|n| {
                n["headRepositoryOwner"]["login"].as_str() == Some(owner)
            }) else {
                continue;
            };

            let is_draft = node["isDraft"].as_bool().unwrap_or(false);
            let state = node["state"].as_str().unwrap_or("OPEN");
            // A closed PR whose branch was deleted has a null headRef, so the
            // rollup has to be reached for defensively rather than indexed.
            let rollup = &node["headRef"]["target"]["statusCheckRollup"];

            prs.push(CachedPr {
                branch: branch.clone(),
                number: node["number"].as_u64().unwrap_or(0) as u32,
                state: if is_draft && state == "OPEN" {
                    "DRAFT".into()
                } else {
                    state.to_string()
                },
                title: node["title"].as_str().unwrap_or("").to_string(),
                url: node["url"].as_str().unwrap_or("").to_string(),
                author: node["author"]["login"].as_str().unwrap_or("").to_string(),
                review_decision: node["reviewDecision"].as_str().map(str::to_string),
                check_state: rollup["state"].as_str().unwrap_or("NONE").to_string(),
                check_total: rollup["contexts"]["totalCount"].as_u64().unwrap_or(0) as u32,
            });
        }
    }

    let cache = Cache {
        fetched_at: now(),
        prs,
    };
    save_cache(owner, name, &cache)?;
    Ok(cache)
}

/// Every check context for a PR, with its url. One request, for one PR, on
/// demand.
pub fn contexts(owner: &str, name: &str, number: u32) -> Result<Vec<Check>> {
    let args: Vec<String> = vec![
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("query={CONTEXTS_QUERY}"),
        "-F".into(),
        format!("owner={owner}"),
        "-F".into(),
        format!("name={name}"),
        "-F".into(),
        format!("number={number}"),
    ];
    let body = gh(&args)?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;

    let nodes = v["data"]["repository"]["pullRequest"]["commits"]["nodes"]
        .get(0)
        .and_then(|c| c["commit"]["statusCheckRollup"]["contexts"]["nodes"].as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::new();
    for c in nodes {
        // A CheckRun reports conclusion and detailsUrl; a StatusContext reports
        // state and targetUrl.
        let verdict = c["conclusion"]
            .as_str()
            .or_else(|| c["state"].as_str())
            .unwrap_or("");
        out.push(Check {
            name: c["name"]
                .as_str()
                .or_else(|| c["context"].as_str())
                .unwrap_or("?")
                .to_string(),
            url: c["detailsUrl"]
                .as_str()
                .or_else(|| c["targetUrl"].as_str())
                .unwrap_or("")
                .to_string(),
            failed: matches!(
                verdict,
                "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED"
            ),
        });
    }
    Ok(out)
}

/// Fold a cache into the workstreams whose branch matches.
pub fn apply(projects: &mut [Project], cache: &Cache) {
    let by_branch: HashMap<&str, &CachedPr> =
        cache.prs.iter().map(|p| (p.branch.as_str(), p)).collect();

    for p in projects.iter_mut() {
        for w in p.workstreams.iter_mut() {
            // Match on the remote name: local branches drop the handle, and
            // `headRefName` is always what the remote calls it.
            let Some(c) = by_branch.get(w.git.remote_branch.as_str()) else {
                continue;
            };
            w.pr = Some(PrInfo {
                number: c.number,
                state: match c.state.as_str() {
                    "MERGED" => PrState::Merged,
                    "CLOSED" => PrState::Closed,
                    "DRAFT" => PrState::Draft,
                    _ => PrState::Open,
                },
                title: c.title.clone(),
                url: c.url.clone(),
                author: c.author.clone(),
                review_decision: c.review_decision.clone(),
                checks: Checks {
                    state: CheckState::parse(&c.check_state),
                    total: c.check_total,
                    contexts: None,
                },
            });
        }
    }
}
