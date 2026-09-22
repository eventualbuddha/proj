//! The wire between a `proj` instance and the daemon.
//!
//! Newline-delimited JSON over a unix socket, because the payload is already
//! JSON on its way to disk and a frame per line is the whole framing problem.
//!
//! The socket name carries a hash of the daemon binary's own path. On this
//! machine that path is a nix store path, so a rebuilt `proj` gets a different
//! socket and starts a daemon running its own code instead of talking to one
//! serving last generation's queries; the superseded daemon has no clients left
//! and exits on its idle timer.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crate::github::Cache;
use crate::model::Check;

/// Instance to daemon.
#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Req {
    /// The branches this instance wants PR state for. Sent whenever the scan
    /// changes them; the daemon queries the union over every instance.
    Branches {
        branches: Vec<String>,
    },
    /// Fetch now, whatever the timer thinks.
    Refresh,
    Contexts {
        number: u32,
    },
    Reviewers {
        number: u32,
    },
    Status,
    Stop,
}

/// Daemon to instance.
#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Res {
    /// The whole GitHub cache, pushed on connect and after every fetch.
    Cache {
        cache: Box<Cache>,
    },
    /// Whether a fetch is in flight, so the spinner means something.
    Fetching {
        on: bool,
    },
    Failed {
        message: String,
    },
    Contexts {
        number: u32,
        checks: Vec<WireCheck>,
    },
    Reviewers {
        logins: Vec<String>,
    },
    Status {
        text: String,
    },
}

/// `Check` with serde on it. Its own type rather than a derive on the model, for
/// the reason the disk cache has its own: the wire should not change shape
/// because a field moved in the UI's idea of a check.
#[derive(Serialize, Deserialize)]
pub struct WireCheck {
    pub name: String,
    pub url: String,
    pub failed: bool,
}

impl From<&Check> for WireCheck {
    fn from(c: &Check) -> Self {
        WireCheck {
            name: c.name.clone(),
            url: c.url.clone(),
            failed: c.failed,
        }
    }
}

impl From<WireCheck> for Check {
    fn from(c: WireCheck) -> Self {
        Check {
            name: c.name,
            url: c.url,
            failed: c.failed,
        }
    }
}

/// Where the socket lives. `$XDG_RUNTIME_DIR` first: it is per-user, on tmpfs,
/// and cleared at logout, which is exactly the lifetime a socket wants.
pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(cache_dir)
        .join("proj");
    dir.join(format!("daemon-{}.sock", exe_key()))
}

fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache"))
}

/// FNV-1a of the binary's path, so one socket per build of `proj`.
fn exe_key() -> String {
    let path = std::env::current_exe().unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.as_os_str().as_encoded_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

pub fn send<T: Serialize>(stream: &mut UnixStream, msg: &T) -> Result<()> {
    let mut line = serde_json::to_string(msg).context("encoding an ipc message")?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    Ok(())
}
