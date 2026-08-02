//! Background thread that owns one Browser pane's `agent-browser` session:
//! applies inbound [`super::BrowserCommand`]s and polls for screenshot
//! frames, pushing decoded PNG bytes into the main loop as
//! [`crate::events::AppEvent::BrowserFrame`].
//!
//! Plain OS thread rather than a tokio task: every operation here is a
//! blocking subprocess call (`agent-browser` CLI), so there is nothing to
//! gain from async and a real thread keeps blocking I/O off the tokio
//! runtime's worker threads.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use tokio::sync::mpsc::Sender as TokioSender;

use crate::events::AppEvent;
use crate::layout::PaneId;

use super::daemon;
use super::BrowserCommand;

/// Screenshot polling interval. `agent-browser`'s live screencast stream is
/// hardcoded to JPEG (see `daemon.rs` module docs / plan notes), which Kitty
/// graphics can't render, so this polls the one-shot PNG screenshot action
/// instead of subscribing to that stream -- not a real-time video feed.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

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

    loop {
        match commands.recv_timeout(POLL_INTERVAL) {
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
                if events
                    .blocking_send(AppEvent::BrowserFrame {
                        pane_id,
                        data,
                    })
                    .is_err()
                {
                    // Main loop is gone; nothing left to render into.
                    break;
                }
            }
            Err(err) => {
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
