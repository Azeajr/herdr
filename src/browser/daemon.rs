//! Lifecycle operations against one pane's `agent-browser` session.
//!
//! `agent-browser`'s daemon detaches via `setsid()` on spawn (confirmed
//! against its source, `cli/src/native/connection.rs`), so it can never be a
//! herdr PTY-pane child for process-lifecycle purposes -- it must be started
//! and stopped explicitly through its own CLI actions rather than relied on
//! for SIGHUP/process-group teardown.

use crate::layout::PaneId;

use super::client::{self, CallError};

pub(crate) fn session_name(pane_id: PaneId) -> String {
    format!("herdr-browser-{}", pane_id.raw())
}

fn ok_or_err(
    action: &str,
    result: Result<client::Response, CallError>,
) -> Result<client::Response, String> {
    match result {
        Ok(response) if response.success => Ok(response),
        Ok(response) => Err(response.error.unwrap_or_else(|| format!("{action} failed"))),
        Err(err) => Err(err.to_string()),
    }
}

/// Launches the browser for this session (stays on `about:blank`). `open`'s
/// URL argument is optional; called bare it only launches. Verified against
/// agent-browser 0.33.1.
pub(crate) fn open(session: &str) -> Result<(), String> {
    ok_or_err("open", client::call(session, &[], &["open"])).map(|_| ())
}

/// `goto` is an undocumented alias for `open <url>` -- it does not appear in
/// `agent-browser --help`, but it navigates an already-launched session
/// without the launch semantics of `open`. Verified against agent-browser
/// 0.33.1; if a future release drops it, switch to `open <url>`.
pub(crate) fn navigate(session: &str, url: &str) -> Result<(), String> {
    ok_or_err("navigate", client::call(session, &[], &["goto", url])).map(|_| ())
}

pub(crate) fn mouse_move(session: &str, x: i32, y: i32) -> Result<(), String> {
    let x = x.to_string();
    let y = y.to_string();
    ok_or_err(
        "mouse move",
        client::call(session, &[], &["mouse", "move", &x, &y]),
    )
    .map(|_| ())
}

pub(crate) fn mouse_button(session: &str, down: bool) -> Result<(), String> {
    let sub = if down { "down" } else { "up" };
    ok_or_err(
        "mouse button",
        client::call(session, &[], &["mouse", sub, "left"]),
    )
    .map(|_| ())
}

/// The CLI signature is `mouse wheel <dy> [dx]` -- vertical delta first, the
/// opposite of this function's argument order and of `mouse move <x> <y>`.
/// Verified against agent-browser 0.33.1: `mouse wheel 120 0` reports
/// `{"deltaX":0,"deltaY":120}`.
pub(crate) fn wheel(session: &str, delta_x: i32, delta_y: i32) -> Result<(), String> {
    let dx = delta_x.to_string();
    let dy = delta_y.to_string();
    ok_or_err(
        "mouse wheel",
        client::call(session, &[], &["mouse", "wheel", &dy, &dx]),
    )
    .map(|_| ())
}

/// Types text into whatever the page currently has focused. `keyboard type`
/// rather than `type <selector> <text>` because a Browser pane has no notion
/// of a selector -- the user is looking at pixels and typing at the focus,
/// exactly what this subcommand is for. Verified against agent-browser
/// 0.33.1.
pub(crate) fn type_text(session: &str, text: &str) -> Result<(), String> {
    ok_or_err(
        "keyboard type",
        client::call(session, &[], &["keyboard", "type", text]),
    )
    .map(|_| ())
}

/// Presses a named key or modifier chord (`Enter`, `Control+a`). `press`
/// already operates on the current focus, so it needs no selector either.
pub(crate) fn press_key(session: &str, key: &str) -> Result<(), String> {
    ok_or_err("press", client::call(session, &[], &["press", key])).map(|_| ())
}

/// Captures a PNG screenshot to a reusable per-session temp file and reads
/// it back. agent-browser's `screenshot` action has no inline-base64 output
/// mode (verified locally) -- only a file-path destination.
pub(crate) fn screenshot_png(session: &str) -> Result<Vec<u8>, String> {
    let path = client::screenshot_path(session);
    let path_str = path.to_string_lossy().into_owned();
    ok_or_err(
        "screenshot",
        client::call(
            session,
            &["--screenshot-format", "png"],
            &["screenshot", &path_str],
        ),
    )?;
    std::fs::read(&path).map_err(|err| format!("read screenshot: {err}"))
}

/// Stops this session's daemon. Public CLI action (`close`/`quit`/`exit`
/// with no `--all`), not the internal shutdown constant used by `--all`.
pub(crate) fn stop(session: &str) {
    let _ = client::call(session, &[], &["close"]);
    client::remove_screenshot_path(&client::screenshot_path(session));
}
