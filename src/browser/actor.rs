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

    loop {
        let command = commands.recv_timeout(interval);
        if command.is_ok() {
            // Input just landed; poll at full rate so its effect shows up
            // without waiting out an idle backoff.
            interval = POLL_INTERVAL;
        }
        match command {
            Ok(BrowserCommand::Navigate(url)) => {
                if let Err(err) = daemon::navigate(&session, &url) {
                    tracing::warn!(pane_id = pane_id.raw(), %err, "browser pane: navigate failed");
                }
            }
            Ok(BrowserCommand::Click { x, y }) => {
                if let Err(err) = click(&session, x, y) {
                    tracing::warn!(pane_id = pane_id.raw(), %err, "browser pane: click failed");
                }
            }
            Ok(BrowserCommand::MouseMove { x, y }) => {
                if let Err(err) = daemon::mouse_move(&session, x, y) {
                    tracing::warn!(pane_id = pane_id.raw(), %err, "browser pane: mouse move failed");
                }
            }
            Ok(BrowserCommand::Scroll { delta_x, delta_y }) => {
                if let Err(err) = daemon::wheel(&session, delta_x, delta_y) {
                    tracing::warn!(pane_id = pane_id.raw(), %err, "browser pane: scroll failed");
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }

        match daemon::screenshot_png(&session) {
            Ok(data) => {
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
                tracing::warn!(pane_id = pane_id.raw(), %err, "browser pane: screenshot failed");
            }
        }
    }

    daemon::stop(&session);
}

fn click(session: &str, x: i32, y: i32) -> Result<(), String> {
    daemon::mouse_move(session, x, y)?;
    daemon::mouse_button(session, true)?;
    daemon::mouse_button(session, false)
}

fn backed_off(interval: Duration) -> Duration {
    (interval * 2).min(POLL_INTERVAL_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_frames_have_identical_fingerprints() {
        assert_eq!(frame_fingerprint(b"frame"), frame_fingerprint(b"frame"));
        assert_ne!(frame_fingerprint(b"frame"), frame_fingerprint(b"other"));
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
