//! Background thread that owns one Browser pane's `agent-browser` session:
//! applies inbound [`super::BrowserCommand`]s and polls for screenshot
//! frames, pushing decoded PNG bytes into the main loop as
//! [`crate::events::AppEvent::BrowserFrame`].
//!
//! Plain OS thread rather than a tokio task: every operation here is a
//! blocking subprocess call (`agent-browser` CLI), so there is nothing to
//! gain from async and a real thread keeps blocking I/O off the tokio
//! runtime's worker threads.

use std::hash::{Hash, Hasher};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use tokio::sync::mpsc::Sender as TokioSender;

use crate::events::AppEvent;
use crate::layout::PaneId;

use super::daemon;
use super::BrowserCommand;

/// Screenshot polling interval while the page is changing. `agent-browser`'s
/// live screencast stream is hardcoded to JPEG (see `daemon.rs` module docs /
/// plan notes), which Kitty graphics can't render, so this polls the one-shot
/// PNG screenshot action instead of subscribing to that stream -- not a
/// real-time video feed.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Ceiling for the idle backoff. Each poll is a full `agent-browser`
/// subprocess spawn plus a PNG write and read-back, so an idle page must not
/// keep paying the 5-per-second cost of [`POLL_INTERVAL`] forever. Reached
/// after ~4 unchanged frames.
const POLL_INTERVAL_MAX: Duration = Duration::from_secs(2);

/// Consecutive failed screenshots before the session is treated as gone.
/// A single failure is normal while the page is mid-navigation, but a session
/// whose browser died fails every capture forever, and until this reports the
/// death the pane keeps a stale frame and keeps swallowing mouse input (see
/// `App::handle_browser_daemon_exited`). With the idle backoff this is reached
/// roughly ten seconds after the browser goes away.
const MAX_CONSECUTIVE_SCREENSHOT_FAILURES: u32 = 5;

/// Ceiling on how many queued commands one batch applies before going back to
/// poll for a frame. Without it a held-down key could starve the screenshot
/// loop and freeze the pane's picture while input kept landing.
const MAX_BATCHED_COMMANDS: usize = 32;

/// Cheap identity for a polled frame, so an unchanged page costs nothing
/// beyond the poll itself: no channel send, no render wake-up, no re-upload
/// of the same PNG to the host terminal.
fn frame_fingerprint(data: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn run(
    pane_id: PaneId,
    session: String,
    commands: Receiver<BrowserCommand>,
    events: TokioSender<AppEvent>,
) {
    if let Err(err) = daemon::open(&session) {
        tracing::warn!(pane_id = pane_id.raw(), %err, "browser pane: failed to open agent-browser session");
        let _ = events.blocking_send(AppEvent::BrowserDaemonExited {
            pane_id,
            reason: err,
        });
        return;
    }

    // Grows while the page keeps producing identical frames, resets to
    // POLL_INTERVAL on any input or any actual visual change.
    let mut interval = POLL_INTERVAL;
    let mut last_frame: Option<u64> = None;
    let mut consecutive_failures: u32 = 0;

    loop {
        let command = commands.recv_timeout(interval);
        if command.is_ok() {
            // Input just landed; poll at full rate so its effect shows up
            // without waiting out an idle backoff.
            interval = POLL_INTERVAL;
        }
        match command {
            Ok(first) => {
                // Every command is a subprocess spawn, so drain whatever else
                // is already queued and merge it before touching the CLI.
                // Fast typing otherwise costs one `keyboard type` per key.
                for command in coalesce(first, &commands) {
                    apply(pane_id, &session, command);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }

        match daemon::screenshot_png(&session) {
            Ok(data) => {
                consecutive_failures = 0;
                let fingerprint = frame_fingerprint(&data);
                if last_frame == Some(fingerprint) {
                    interval = backed_off(interval);
                    continue;
                }
                last_frame = Some(fingerprint);
                interval = POLL_INTERVAL;
                if events
                    .blocking_send(AppEvent::BrowserFrame { pane_id, data })
                    .is_err()
                {
                    // Main loop is gone; nothing left to render into.
                    break;
                }
            }
            Err(err) => {
                // Back off here too: a session that fails every capture must
                // not spawn subprocesses five times a second forever.
                interval = backed_off(interval);
                consecutive_failures = consecutive_failures.saturating_add(1);
                tracing::warn!(
                    pane_id = pane_id.raw(),
                    %err,
                    consecutive_failures,
                    "browser pane: screenshot failed"
                );
                if session_looks_dead(consecutive_failures) {
                    let _ = events.blocking_send(AppEvent::BrowserDaemonExited {
                        pane_id,
                        reason: err,
                    });
                    break;
                }
            }
        }
    }

    daemon::stop(&session);
}

/// Drains everything already queued behind `first` and merges adjacent
/// [`BrowserCommand::TypeText`] into single calls.
///
/// Only text merges. Reordering anything else would change meaning -- a click
/// between two keystrokes moves focus, and a `press Enter` between them
/// submits.
fn coalesce(first: BrowserCommand, commands: &Receiver<BrowserCommand>) -> Vec<BrowserCommand> {
    let mut batch = vec![first];
    while let Ok(next) = commands.try_recv() {
        match (batch.last_mut(), next) {
            (Some(BrowserCommand::TypeText(pending)), BrowserCommand::TypeText(text)) => {
                pending.push_str(&text);
            }
            (_, next) => batch.push(next),
        }
        if batch.len() >= MAX_BATCHED_COMMANDS {
            break;
        }
    }
    batch
}

fn apply(pane_id: PaneId, session: &str, command: BrowserCommand) {
    let (action, result) = match command {
        BrowserCommand::Navigate(url) => ("navigate", daemon::navigate(session, &url)),
        BrowserCommand::MouseDown { x, y } => ("mouse down", mouse_down(session, x, y)),
        BrowserCommand::MouseMove { x, y } => ("mouse move", daemon::mouse_move(session, x, y)),
        BrowserCommand::MouseUp { x, y } => ("mouse up", mouse_up(session, x, y)),
        BrowserCommand::Scroll { delta_x, delta_y } => {
            ("scroll", daemon::wheel(session, delta_x, delta_y))
        }
        BrowserCommand::TypeText(text) => ("type", daemon::type_text(session, &text)),
        BrowserCommand::PressKey(key) => ("press", daemon::press_key(session, &key)),
    };
    if let Err(err) = result {
        tracing::warn!(pane_id = pane_id.raw(), action, %err, "browser pane: command failed");
    }
}

fn mouse_down(session: &str, x: i32, y: i32) -> Result<(), String> {
    daemon::mouse_move(session, x, y)?;
    daemon::mouse_button(session, true)
}

fn mouse_up(session: &str, x: i32, y: i32) -> Result<(), String> {
    daemon::mouse_move(session, x, y)?;
    daemon::mouse_button(session, false)
}

fn backed_off(interval: Duration) -> Duration {
    (interval * 2).min(POLL_INTERVAL_MAX)
}

/// Distinguishes a transient capture failure (mid-navigation, page busy) from
/// a browser that is gone for good.
fn session_looks_dead(consecutive_failures: u32) -> bool {
    consecutive_failures >= MAX_CONSECUTIVE_SCREENSHOT_FAILURES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_bursts_merge_into_one_call() {
        let (tx, rx) = std::sync::mpsc::channel();
        for c in ["e", "l", "l", "o"] {
            tx.send(BrowserCommand::TypeText(c.to_string())).unwrap();
        }
        let batch = coalesce(BrowserCommand::TypeText("h".to_string()), &rx);
        assert_eq!(batch, vec![BrowserCommand::TypeText("hello".to_string())]);
    }

    #[test]
    fn non_text_commands_keep_their_order_and_split_typing() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(BrowserCommand::TypeText("i".to_string())).unwrap();
        tx.send(BrowserCommand::PressKey("Enter".to_string()))
            .unwrap();
        tx.send(BrowserCommand::TypeText("b".to_string())).unwrap();
        tx.send(BrowserCommand::TypeText("c".to_string())).unwrap();
        let batch = coalesce(BrowserCommand::TypeText("h".to_string()), &rx);
        // Merging across the Enter would submit "hibc" instead of "hi".
        assert_eq!(
            batch,
            vec![
                BrowserCommand::TypeText("hi".to_string()),
                BrowserCommand::PressKey("Enter".to_string()),
                BrowserCommand::TypeText("bc".to_string()),
            ]
        );
    }

    #[test]
    fn batching_is_bounded_so_frames_keep_flowing() {
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..(MAX_BATCHED_COMMANDS * 2) {
            tx.send(BrowserCommand::PressKey("Tab".to_string()))
                .unwrap();
        }
        let batch = coalesce(BrowserCommand::PressKey("Tab".to_string()), &rx);
        assert_eq!(batch.len(), MAX_BATCHED_COMMANDS);
    }

    #[test]
    fn identical_frames_have_identical_fingerprints() {
        assert_eq!(frame_fingerprint(b"frame"), frame_fingerprint(b"frame"));
        assert_ne!(frame_fingerprint(b"frame"), frame_fingerprint(b"other"));
    }

    #[test]
    fn a_single_screenshot_failure_does_not_report_the_session_dead() {
        assert!(!session_looks_dead(0));
        assert!(!session_looks_dead(1));
        assert!(!session_looks_dead(MAX_CONSECUTIVE_SCREENSHOT_FAILURES - 1));
        assert!(session_looks_dead(MAX_CONSECUTIVE_SCREENSHOT_FAILURES));
        assert!(session_looks_dead(u32::MAX));
    }

    #[test]
    fn idle_backoff_doubles_up_to_the_ceiling() {
        let mut interval = POLL_INTERVAL;
        for _ in 0..10 {
            interval = backed_off(interval);
            assert!(interval <= POLL_INTERVAL_MAX);
        }
        assert_eq!(interval, POLL_INTERVAL_MAX);
        assert!(backed_off(POLL_INTERVAL) > POLL_INTERVAL);
    }
}
