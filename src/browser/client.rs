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

use std::io::Read;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

pub(crate) const BINARY: &str = "agent-browser";

/// How often [`output_with_timeout`] checks whether the child has exited.
/// Each `agent-browser` call already costs tens of milliseconds, so polling
/// this often is free relative to the work being waited on.
const POLL_STEP: Duration = Duration::from_millis(5);

#[derive(Debug, Deserialize)]
pub(crate) struct Response {
    pub success: bool,
    /// Action-specific payload; read by `super::daemon::page_info`.
    pub data: Option<Value>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub(crate) enum CallError {
    Spawn(std::io::Error),
    /// The CLI did not exit within its timeout and was killed. Without this
    /// the calling actor thread blocks forever on a wedged `agent-browser`,
    /// which also stalls herdr's own shutdown via
    /// `App::stop_all_browser_sessions`.
    Timeout {
        timeout: Duration,
    },
    /// `agent-browser` produced something other than its `{success,data,error}`
    /// JSON envelope. Carries the exit status and stderr because the common
    /// case -- a non-zero exit with empty stdout -- otherwise surfaces only as
    /// an opaque "EOF while parsing a value" from serde.
    Decode {
        status: std::process::ExitStatus,
        stderr: String,
        source: serde_json::Error,
    },
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Spawn(err) => write!(f, "failed to run {BINARY}: {err}"),
            CallError::Timeout { timeout } => {
                write!(f, "{BINARY} did not respond within {timeout:?}")
            }
            CallError::Decode {
                status,
                stderr,
                source,
            } => {
                write!(f, "failed to decode {BINARY} output ({status}): {source}")?;
                if !stderr.is_empty() {
                    write!(f, "; stderr: {stderr}")?;
                }
                Ok(())
            }
        }
    }
}

/// Runs `agent-browser --session <session> --json <global_flags...>
/// <action_args...>` and parses its `{success,data,error}` JSON response.
///
/// `global_flags` (e.g. `--screenshot-format png`) must come before
/// `action_args` -- agent-browser parses them as CLI-global, not
/// per-subcommand.
///
/// `timeout` bounds the whole call; the child is killed when it expires.
/// Callers pick it per action (see `super::daemon`) because a page load and a
/// mouse move have very different legitimate durations.
pub(crate) fn call(
    session: &str,
    global_flags: &[&str],
    action_args: &[&str],
    timeout: Duration,
) -> Result<Response, CallError> {
    let mut command = Command::new(BINARY);
    command
        .arg("--session")
        .arg(session)
        .args(global_flags)
        .arg("--json")
        .args(action_args);
    let output = output_with_timeout(command, timeout)?;
    serde_json::from_slice(&output.stdout).map_err(|source| CallError::Decode {
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        source,
    })
}

/// `Command::output()` with a deadline.
///
/// stdout and stderr are drained on their own threads: a child that fills a
/// pipe buffer while we are polling `try_wait` would otherwise block on the
/// write and never exit, turning the timeout into the normal path. The child
/// handle is held until after the kill so its pid cannot be recycled onto an
/// unrelated process in between.
fn output_with_timeout(mut command: Command, timeout: Duration) -> Result<Output, CallError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(CallError::Spawn)?;

    let stdout = child.stdout.take().map(drain_pipe);
    let stderr = child.stderr.take().map(drain_pipe);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(err) => return Err(CallError::Spawn(err)),
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(POLL_STEP);
    };

    let status = match status {
        Some(status) => status,
        None => {
            // Killing closes both pipes, which lets the reader threads finish.
            let _ = child.kill();
            let _ = child.wait();
            return Err(CallError::Timeout { timeout });
        }
    };

    Ok(Output {
        status,
        stdout: stdout
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default(),
        stderr: stderr
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default(),
    })
}

fn drain_pipe<R: Read + Send + 'static>(mut pipe: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// Reusable per-session screenshot path so polling doesn't create garbage
/// files; cleaned up by [`remove_screenshot_path`] on pane teardown. The
/// session name already carries a per-server token (see
/// `super::daemon::session_name`), so two herdr servers never share a path.
pub(crate) fn screenshot_path(session: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("herdr-browser-{session}.png"))
}

pub(crate) fn remove_screenshot_path(path: &Path) {
    let _ = std::fs::remove_file(path);
}

// Both tests drive real child processes through `sh`/`sleep`, so they are
// Unix-only; the timeout logic itself is platform-neutral.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_hung_child_is_killed_at_the_deadline() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let started = Instant::now();
        let err = output_with_timeout(command, Duration::from_millis(150))
            .expect_err("sleep 30 must not finish inside 150ms");
        assert!(matches!(err, CallError::Timeout { .. }), "{err}");
        // Without the kill this would block for the full 30s.
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn output_is_captured_when_the_child_finishes_in_time() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("printf out; printf err >&2");
        let output =
            output_with_timeout(command, Duration::from_secs(10)).expect("child should finish");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }
}
