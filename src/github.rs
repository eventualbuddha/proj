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
      number title url state isDraft headRefName reviewDecision baseRefName
      author {{ login }}
      headRepositoryOwner {{ login }}
      reviewRequests(first: 5) {{ nodes {{ requestedReviewer {{
        ... on User {{ login }} ... on Team {{ slug }}
      }} }} }}
      latestReviews(first: 10) {{ nodes {{ author {{ login }} state }} }}
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
    /// Defaulted so a cache written before reviews existed still loads rather
    /// than being discarded whole.
    #[serde(default)]
    pub reviews: Vec<CachedReview>,
}

#[derive(serde::Serialize, Deserialize, Clone)]
pub struct CachedReview {
    pub number: u32,
    pub title: String,
    pub url: String,
    pub author: String,
    pub branch: String,
    pub state: String,
    pub reason: String,
    pub check_state: String,
    pub check_total: u32,
    pub updated: i64,
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
    pub check_total: u32,    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub reviewers: Vec<(String, String)>,
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
                base: node["baseRefName"].as_str().unwrap_or("main").to_string(),
                reviewers: parse_reviewers(node),
            });
        }
    }

    // The review queue rides along on the same cycle. A failure here is not a
    // failure of the whole refresh -- an empty queue and an unreachable one look
    // the same on screen, but losing the workstream PRs too would be worse.
    let reviews = match viewer(owner) {
        Ok((me, teams)) => review_queue(owner, name, &me, &teams).unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    let cache = Cache {
        fetched_at: now(),
        prs,
        reviews,
    };
    save_cache(owner, name, &cache)?;
    Ok(cache)
}

/// Reviewers GitHub suggests for a PR, plus anyone already requested.
pub fn suggested_reviewers(owner: &str, name: &str, number: u32) -> Result<Vec<String>> {
    let q = r#"
query($o: String!, $n: String!, $p: Int!) {
  repository(owner: $o, name: $n) {
    pullRequest(number: $p) {
      suggestedReviewers { reviewer { login } }
      reviewRequests(first: 10) { nodes { requestedReviewer {
        ... on User { login } ... on Team { slug }
      } } }
    }
  }
}
"#;
    let body = gh(&[
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("query={q}"),
        "-F".into(),
        format!("o={owner}"),
        "-F".into(),
        format!("n={name}"),
        "-F".into(),
        format!("p={number}"),
    ])?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    let pr = &v["data"]["repository"]["pullRequest"];

    let mut out: Vec<String> = Vec::new();
    let mut push = |l: &str| {
        if !l.is_empty() && !out.iter().any(|x| x == l) {
            out.push(l.to_string());
        }
    };
    for r in pr["suggestedReviewers"].as_array().cloned().unwrap_or_default() {
        push(r["reviewer"]["login"].as_str().unwrap_or(""));
    }
    for r in pr["reviewRequests"]["nodes"].as_array().cloned().unwrap_or_default() {
        let w = &r["requestedReviewer"];
        push(w["login"].as_str().or_else(|| w["slug"].as_str()).unwrap_or(""));
    }
    Ok(out)
}

/// Pending review requests first, then anyone who has already responded.
fn parse_reviewers(node: &serde_json::Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for r in node["reviewRequests"]["nodes"].as_array().cloned().unwrap_or_default() {
        let who = &r["requestedReviewer"];
        if let Some(login) = who["login"].as_str().or_else(|| who["slug"].as_str()) {
            out.push((login.to_string(), "PENDING".to_string()));
        }
    }
    for r in node["latestReviews"]["nodes"].as_array().cloned().unwrap_or_default() {
        let Some(login) = r["author"]["login"].as_str() else { continue };
        if out.iter().any(|(l, _)| l == login) {
            continue;
        }
        out.push((
            login.to_string(),
            r["state"].as_str().unwrap_or("COMMENTED").to_string(),
        ));
    }
    out
}

/// Your review queue: three questions, one request.
///
///   review-requested       someone named you
///   team-review-requested  someone named a team you are in
///   reviewed-by + moved    you already reviewed it and commits landed after
///
/// The third is the one neither of the others returns and the easiest to drop on
/// the floor -- you looked, you approved or asked for changes, and then it
/// changed. It is only counted when the head commit is newer than your latest
/// review, which is why that query asks for both dates.
const REVIEW_QUERY: &str = r#"
query($q1: String!, $q2: String!, $q3: String!, $me: String!) {
  a1: search(query: $q1, type: ISSUE, first: 30) { nodes { ...pr } }
  a2: search(query: $q2, type: ISSUE, first: 30) { nodes { ...pr } }
  a3: search(query: $q3, type: ISSUE, first: 30) { nodes {
    ...pr
    ... on PullRequest { reviews(last: 1, author: $me) { nodes { submittedAt } } }
  } }
}
fragment pr on PullRequest {
  number title url isDraft state headRefName
  author { login }
  commits(last: 1) { nodes { commit {
    committedDate
    statusCheckRollup { state contexts { totalCount } }
  } } }
}
"#;

/// ISO 8601 to unix seconds, by shelling out to `date`. Only the ordering
/// matters here, and a date crate for one comparison is not worth the build.
fn iso_to_unix(s: &str) -> i64 {
    std::process::Command::new("date")
        .args(["-u", "-d", s, "+%s"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

/// Fetch the review queue. `teams` are org/team slugs you belong to.
pub fn review_queue(
    owner: &str,
    name: &str,
    me: &str,
    teams: &[String],
) -> Result<Vec<CachedReview>> {
    let repo = format!("repo:{owner}/{name} is:pr is:open");
    let team_clause = if teams.is_empty() {
        // A query that cannot match, rather than one with no filter at all: an
        // empty clause would return every open PR in the repo as a review.
        format!("{repo} team-review-requested:{owner}/__none__")
    } else {
        format!(
            "{repo} {}",
            teams
                .iter()
                .map(|t| format!("team-review-requested:{t}"))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };

    let args: Vec<String> = vec![
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("query={REVIEW_QUERY}"),
        "-F".into(),
        format!("q1={repo} review-requested:{me}"),
        "-F".into(),
        format!("q2={team_clause}"),
        "-F".into(),
        format!("q3={repo} reviewed-by:{me}"),
        "-F".into(),
        format!("me={me}"),
    ];

    let body = gh(&args)?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;

    let mut out: Vec<CachedReview> = Vec::new();
    for (alias, reason) in [("a1", "requested"), ("a2", "team"), ("a3", "re-review")] {
        for n in v["data"][alias]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let number = n["number"].as_u64().unwrap_or(0) as u32;
            if number == 0 || out.iter().any(|r| r.number == number) {
                continue;
            }
            let commit = &n["commits"]["nodes"][0]["commit"];
            let updated = commit["committedDate"].as_str().map(iso_to_unix).unwrap_or(0);

            if reason == "re-review" {
                // Only when it moved since you looked.
                let reviewed = n["reviews"]["nodes"][0]["submittedAt"]
                    .as_str()
                    .map(iso_to_unix)
                    .unwrap_or(0);
                if reviewed == 0 || updated <= reviewed {
                    continue;
                }
            }

            let rollup = &commit["statusCheckRollup"];
            let is_draft = n["isDraft"].as_bool().unwrap_or(false);
            out.push(CachedReview {
                number,
                title: n["title"].as_str().unwrap_or("").to_string(),
                url: n["url"].as_str().unwrap_or("").to_string(),
                author: n["author"]["login"].as_str().unwrap_or("").to_string(),
                branch: n["headRefName"].as_str().unwrap_or("").to_string(),
                state: if is_draft { "DRAFT".into() } else { "OPEN".into() },
                reason: reason.to_string(),
                check_state: rollup["state"].as_str().unwrap_or("NONE").to_string(),
                check_total: rollup["contexts"]["totalCount"].as_u64().unwrap_or(0) as u32,
                updated,
            });
        }
    }
    Ok(out)
}

/// Your login, and the teams you belong to within `owner`.
pub fn viewer(owner: &str) -> Result<(String, Vec<String>)> {
    let me = String::from_utf8_lossy(&gh(&[
        "api".into(),
        "user".into(),
        "--jq".into(),
        ".login".into(),
    ])?)
    .trim()
    .to_string();

    let jq = format!("{}", r#".[] | "\(.organization.login)/\(.slug)""#);
    let teams = gh(&[
        "api".into(),
        "user/teams".into(),
        "--paginate".into(),
        "--jq".into(),
        jq,
    ])
    .map(|o| {
        String::from_utf8_lossy(&o)
            .lines()
            .filter(|l| l.starts_with(&format!("{owner}/")))
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default();

    Ok((me, teams))
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

/// Turn cached review rows into the model's shape.
pub fn to_reviews(cache: &Cache) -> Vec<Review> {
    cache
        .reviews
        .iter()
        .map(|r| Review {
            number: r.number,
            title: r.title.clone(),
            url: r.url.clone(),
            author: r.author.clone(),
            branch: r.branch.clone(),
            state: if r.state == "DRAFT" {
                PrState::Draft
            } else {
                PrState::Open
            },
            checks: Checks {
                state: CheckState::parse(&r.check_state),
                total: r.check_total,
                contexts: None,
            },
            reason: match r.reason.as_str() {
                "team" => ReviewReason::TeamRequested,
                "re-review" => ReviewReason::Rereview,
                _ => ReviewReason::Requested,
            },
            updated: r.updated,
        })
        .collect()
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
                base: c.base.clone(),
                reviewers: c
                    .reviewers
                    .iter()
                    .map(|(login, st)| Reviewer {
                        login: login.clone(),
                        state: match st.as_str() {
                            "PENDING" => ReviewerState::Pending,
                            "APPROVED" => ReviewerState::Approved,
                            "CHANGES_REQUESTED" => ReviewerState::ChangesRequested,
                            _ => ReviewerState::Commented,
                        },
                    })
                    .collect(),
            });
        }
    }
}
