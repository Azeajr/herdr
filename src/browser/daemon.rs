//! Lifecycle operations against one pane's `agent-browser` session.
//!
//! `agent-browser`'s daemon detaches via `setsid()` on spawn (confirmed
//! against its source, `cli/src/native/connection.rs`), so it can never be a
//! herdr PTY-pane child for process-lifecycle purposes -- it must be started
//! and stopped explicitly through its own CLI actions rather than relied on
//! for SIGHUP/process-group teardown.

use std::hash::{Hash, Hasher};
use std::sync::OnceLock;
use std::time::Duration;

use crate::layout::PaneId;

use super::client::{self, CallError};

/// Bound for input and capture actions. These talk to an already-running
/// browser and normally return in tens of milliseconds; anything near this is
/// a wedged daemon, not slow work.
const ACTION_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound for actions that wait on a page load. `open` also launches Chrome on
/// a cold session, which is by far the slowest thing this module does.
const NAVIGATE_TIMEOUT: Duration = Duration::from_secs(60);

/// Bound for [`stop`]. Deliberately short: it runs on herdr's shutdown path
/// (`App::stop_all_browser_sessions`) once per live Browser pane, so a wedged
/// session must not hold the whole process open.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-server token mixed into every session name.
///
/// Pane ids restart at 1 in each server, so a name derived from the pane id
/// alone collides across two concurrently running herdr servers -- both would
/// drive the same browser and overwrite the same screenshot file. Derived
/// from the pid and a startup timestamp rather than a random generator to
/// avoid a new dependency; the timestamp is what keeps a recycled pid from
/// adopting a crashed server's orphaned session.
fn server_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::process::id().hash(&mut hasher);
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
            .hash(&mut hasher);
        format!("{:08x}", hasher.finish() as u32)
    })
}

pub(crate) fn session_name(pane_id: PaneId) -> String {
    format!("herdr-{}-{}", server_token(), pane_id.raw())
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
    ok_or_err(
        "open",
        client::call(session, &[], &["open"], NAVIGATE_TIMEOUT),
    )
    .map(|_| ())
}

/// `goto` is an undocumented alias for `open <url>` -- it does not appear in
/// `agent-browser --help`, but it navigates an already-launched session
/// without the launch semantics of `open`. Verified against agent-browser
/// 0.33.1; if a future release drops it, switch to `open <url>`.
pub(crate) fn navigate(session: &str, url: &str) -> Result<(), String> {
    ok_or_err(
        "navigate",
        client::call(session, &[], &["goto", url], NAVIGATE_TIMEOUT),
    )
    .map(|_| ())
}

pub(crate) fn reload(session: &str) -> Result<(), String> {
    ok_or_err(
        "reload",
        client::call(session, &[], &["reload"], NAVIGATE_TIMEOUT),
    )
    .map(|_| ())
}

pub(crate) fn back(session: &str) -> Result<(), String> {
    ok_or_err(
        "back",
        client::call(session, &[], &["back"], NAVIGATE_TIMEOUT),
    )
    .map(|_| ())
}

pub(crate) fn forward(session: &str) -> Result<(), String> {
    ok_or_err(
        "forward",
        client::call(session, &[], &["forward"], NAVIGATE_TIMEOUT),
    )
    .map(|_| ())
}

/// Reads the page's URL and title in one call.
///
/// `eval` rather than `get url` + `get title` because each action is its own
/// subprocess: polling two of them per visual change would roughly double the
/// cost of an active page.
pub(crate) fn page_info(session: &str) -> Result<(Option<String>, Option<String>), String> {
    let response = ok_or_err(
        "page info",
        client::call(
            session,
            &[],
            &["eval", "JSON.stringify([location.href, document.title])"],
            ACTION_TIMEOUT,
        ),
    )?;
    let raw = response
        .data
        .as_ref()
        .and_then(|data| data.get("result"))
        .and_then(|result| result.as_str())
        .ok_or_else(|| "page info response had no result".to_string())?;
    let parsed: (String, String) =
        serde_json::from_str(raw).map_err(|err| format!("decode page info: {err}"))?;
    Ok((non_empty(parsed.0), non_empty(parsed.1)))
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

/// Resizes the page itself so its aspect ratio matches the pane's.
///
/// Kitty-graphics placement stretches the frame to fill the pane's inner rect
/// (`src/ghostty/mod.rs`), so without this every pane whose shape differs
/// from the browser's default viewport renders a distorted page.
pub(crate) fn set_viewport(session: &str, width: u32, height: u32) -> Result<(), String> {
    let width = width.to_string();
    let height = height.to_string();
    ok_or_err(
        "set viewport",
        client::call(
            session,
            &[],
            &["set", "viewport", &width, &height],
            ACTION_TIMEOUT,
        ),
    )
    .map(|_| ())
}

pub(crate) fn mouse_move(session: &str, x: i32, y: i32) -> Result<(), String> {
    let x = x.to_string();
    let y = y.to_string();
    ok_or_err(
        "mouse move",
        client::call(session, &[], &["mouse", "move", &x, &y], ACTION_TIMEOUT),
    )
    .map(|_| ())
}

pub(crate) fn mouse_button(session: &str, down: bool) -> Result<(), String> {
    let sub = if down { "down" } else { "up" };
    ok_or_err(
        "mouse button",
        client::call(session, &[], &["mouse", sub, "left"], ACTION_TIMEOUT),
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
        client::call(session, &[], &["mouse", "wheel", &dy, &dx], ACTION_TIMEOUT),
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
        client::call(session, &[], &["keyboard", "type", text], ACTION_TIMEOUT),
    )
    .map(|_| ())
}

/// Presses a named key or modifier chord (`Enter`, `Control+a`). `press`
/// already operates on the current focus, so it needs no selector either.
pub(crate) fn press_key(session: &str, key: &str) -> Result<(), String> {
    ok_or_err(
        "press",
        client::call(session, &[], &["press", key], ACTION_TIMEOUT),
    )
    .map(|_| ())
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
            ACTION_TIMEOUT,
        ),
    )?;
    let data = std::fs::read(&path).map_err(|err| format!("read screenshot: {err}"))?;
    if !is_complete_png(&data) {
        // The writer is a separate process, so a killed or timed-out capture
        // can leave a half-written file behind. Rejecting it here keeps a
        // torn frame out of the graphics layer; the next poll re-captures.
        return Err("screenshot file is not a complete PNG".to_string());
    }
    Ok(data)
}

/// Checks the PNG signature and the terminating `IEND` chunk, which together
/// mean the file was written all the way through.
fn is_complete_png(data: &[u8]) -> bool {
    const SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    const IEND: &[u8] = &[b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82];
    data.starts_with(SIGNATURE) && data.ends_with(IEND)
}

/// Stops this session's daemon. Public CLI action (`close`/`quit`/`exit`
/// with no `--all`), not the internal shutdown constant used by `--all`.
pub(crate) fn stop(session: &str) {
    let _ = client::call(session, &[], &["close"], STOP_TIMEOUT);
    client::remove_screenshot_path(&client::screenshot_path(session));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_names_are_scoped_to_this_server() {
        let one = session_name(PaneId::from_raw(1));
        let two = session_name(PaneId::from_raw(2));
        assert_ne!(one, two);
        // Pane ids restart at 1 per server, so the shared token is what keeps
        // two concurrent herdr servers off each other's browser.
        assert!(one.starts_with("herdr-"), "{one}");
        assert!(one.ends_with("-1"), "{one}");
        assert_eq!(server_token(), server_token());
        assert_eq!(server_token().len(), 8);
    }

    #[test]
    fn truncated_screenshots_are_rejected() {
        let mut complete = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        complete.extend_from_slice(b"payload");
        complete.extend_from_slice(&[b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82]);
        assert!(is_complete_png(&complete));

        assert!(!is_complete_png(&complete[..complete.len() - 1]));
        assert!(!is_complete_png(&complete[1..]));
        assert!(!is_complete_png(&[]));
    }
}
