//! Thin wrapper around the `agent-browser` CLI binary.
//!
//! `agent-browser` is not a library crate (bin-only, no `[lib]` target), and
//! its daemon wire protocol involves session discovery/auth details that are
//! non-trivial to reimplement correctly. Its own CLI already resolves all of
//! that transparently and reuses a persistent per-session daemon across
//! invocations (~30-50ms overhead per call once the daemon is warm, verified
//! locally against agent-browser 0.33.1) -- so herdr shells out to it per
//! action instead of speaking its socket protocol directly. `--session
//! <name>` scopes every call to one pane's browser instance.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;

pub(crate) const BINARY: &str = "agent-browser";

#[derive(Debug, Deserialize)]
pub(crate) struct Response {
    pub success: bool,
    // Unread for now (MVP only checks success/error); kept to mirror the
    // daemon's actual response shape rather than dropped and re-added later.
    #[allow(dead_code)]
    pub data: Option<Value>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub(crate) enum CallError {
    Spawn(std::io::Error),
    Decode(serde_json::Error),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Spawn(err) => write!(f, "failed to run {BINARY}: {err}"),
            CallError::Decode(err) => write!(f, "failed to decode {BINARY} output: {err}"),
        }
    }
}

/// Runs `agent-browser --session <session> --json <global_flags...>
/// <action_args...>` and parses its `{success,data,error}` JSON response.
///
/// `global_flags` (e.g. `--screenshot-format png`) must come before
/// `action_args` -- agent-browser parses them as CLI-global, not
/// per-subcommand.
pub(crate) fn call(
    session: &str,
    global_flags: &[&str],
    action_args: &[&str],
) -> Result<Response, CallError> {
    let output = Command::new(BINARY)
        .arg("--session")
        .arg(session)
        .args(global_flags)
        .arg("--json")
        .args(action_args)
        .output()
        .map_err(CallError::Spawn)?;
    serde_json::from_slice(&output.stdout).map_err(CallError::Decode)
}

/// Reusable per-session screenshot path so polling doesn't create garbage
/// files; cleaned up by [`remove_screenshot_path`] on pane teardown.
pub(crate) fn screenshot_path(session: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("herdr-browser-{session}.png"))
}

pub(crate) fn remove_screenshot_path(path: &Path) {
    let _ = std::fs::remove_file(path);
}
