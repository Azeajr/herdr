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

/// Relaunches allowed between two good frames, before the pane is reported
/// failed. A Chrome that crashed or was closed from its own window comes back
/// cleanly on a fresh `open`, so this self-heals the common case with no user
/// action.
///
/// The budget is spent per *frame*, not per death: a session that relaunches
/// happily but never manages a capture (a broken screenshot path, say) would
/// otherwise satisfy `open` every time and relaunch forever, never reaching
/// the failed state the user could act on.
const MAX_RECOVERY_ATTEMPTS: u32 = 3;

/// Delay before the first relaunch, doubled for each further one without an
/// intervening frame.
const RECOVERY_BACKOFF: Duration = Duration::from_secs(1);

/// Cheap identity for a polled frame, so an unchanged page costs nothing
/// beyond the poll itself: no channel send, no render wake-up, no re-upload
/// of the same PNG to the host terminal.
fn frame_fingerprint(data: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

/// What the page should look like after a relaunch: enough to put a recovered
/// session back where the dead one was.
#[derive(Default)]
struct SessionState {
    url: Option<String>,
    viewport: Option<(u32, u32)>,
}

impl SessionState {
    fn record(&mut self, command: &BrowserCommand) {
        match command {
            BrowserCommand::Navigate(url) => self.url = Some(url.clone()),
            BrowserCommand::SetViewport { width, height } => {
                self.viewport = Some((*width, *height));
            }
            _ => {}
        }
    }

    /// Reapplies the recorded page state to a freshly opened session.
    fn restore(&self, session: &str) {
        if let Some((width, height)) = self.viewport {
            if let Err(err) = daemon::set_viewport(session, width, height) {
                tracing::warn!(%err, "browser pane: failed to restore viewport after relaunch");
            }
        }
        if let Some(url) = &self.url {
            if let Err(err) = daemon::navigate(session, url) {
                tracing::warn!(%err, "browser pane: failed to restore url after relaunch");
            }
        }
    }
}

pub(crate) fn run(
    pane_id: PaneId,
    session: String,
    commands: Receiver<BrowserCommand>,
    events: TokioSender<AppEvent>,
) {
    // Deliberately single-shot, unlike the mid-session recovery below: a
    // failing first launch usually means agent-browser or Chrome is missing,
    // and the user is better served by the error now than by the same error
    // several seconds later.
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
    // Reset only by a frame actually arriving, which is what makes the
    // recovery budget bounded -- see `MAX_RECOVERY_ATTEMPTS`.
    let mut relaunches_since_frame: u32 = 0;
    let mut state = SessionState::default();
    let mut last_page_info: Option<std::time::Instant> = None;
    let mut last_page: Option<(Option<String>, Option<String>)> = None;

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
                    state.record(&command);
                    if let Some((action, reason)) = apply(pane_id, &session, command) {
                        // Reported rather than only logged: the API accepted
                        // this command before it ran, so a failure that never
                        // reaches the user looks exactly like success.
                        let _ = events.blocking_send(AppEvent::BrowserCommandFailed {
                            pane_id,
                            action,
                            reason,
                        });
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }

        match daemon::screenshot_png(&session) {
            Ok(data) => {
                consecutive_failures = 0;
                relaunches_since_frame = 0;
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
                // The picture changed, so the page may have too. Rate-limited
                // because it is another subprocess and an animating page
                // changes far more often than its url or title.
                if page_info_due(last_page_info) {
                    last_page_info = Some(std::time::Instant::now());
                    if let Some(event) = poll_page_info(pane_id, &session, &mut last_page) {
                        if events.blocking_send(event).is_err() {
                            break;
                        }
                    }
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
                    if relaunches_since_frame >= MAX_RECOVERY_ATTEMPTS {
                        let _ = events.blocking_send(AppEvent::BrowserDaemonExited {
                            pane_id,
                            reason: err,
                        });
                        break;
                    }
                    relaunches_since_frame = relaunches_since_frame.saturating_add(1);
                    match relaunch(
                        pane_id,
                        &session,
                        &state,
                        &commands,
                        backoff_for(relaunches_since_frame),
                    ) {
                        Relaunch::Ok => {
                            consecutive_failures = 0;
                            last_frame = None;
                            interval = POLL_INTERVAL;
                        }
                        Relaunch::Shutdown => break,
                        Relaunch::Failed(reason) => {
                            // Keep going: the budget above is what ends this,
                            // so a browser that is slow to come back still
                            // gets its remaining attempts.
                            tracing::warn!(
                                pane_id = pane_id.raw(),
                                %reason,
                                relaunches_since_frame,
                                "browser pane: relaunch attempt failed"
                            );
                        }
                    }
                }
            }
        }
    }

    daemon::stop(&session);
}

enum Relaunch {
    /// The session is live again and the recorded page state was reapplied.
    Ok,
    /// The pane went away while we were waiting out the backoff.
    Shutdown,
    /// This attempt failed.
    Failed(String),
}

/// Waits out `backoff`, then tries once to bring a dead session back.
///
/// The wait is a `recv_timeout` on the command channel rather than a sleep so
/// a pane closed during recovery tears down immediately instead of after the
/// remaining backoff. Commands that arrive during the wait are dropped: they
/// were aimed at a browser that no longer exists.
fn relaunch(
    pane_id: PaneId,
    session: &str,
    state: &SessionState,
    commands: &Receiver<BrowserCommand>,
    backoff: Duration,
) -> Relaunch {
    match wait_for_backoff(commands, backoff) {
        Wait::Elapsed => {}
        Wait::Shutdown => return Relaunch::Shutdown,
    }
    // Clear the dead session's daemon record first; `open` on a session
    // agent-browser still believes is live can reattach to the corpse.
    daemon::stop(session);
    match daemon::open(session) {
        Ok(()) => {
            tracing::info!(
                pane_id = pane_id.raw(),
                "browser pane: relaunched agent-browser session"
            );
            state.restore(session);
            Relaunch::Ok
        }
        Err(err) => Relaunch::Failed(err),
    }
}

/// Backoff before the `attempt`-th relaunch since the last good frame.
fn backoff_for(attempt: u32) -> Duration {
    RECOVERY_BACKOFF * 2u32.saturating_pow(attempt.saturating_sub(1).min(16))
}

enum Wait {
    Elapsed,
    Shutdown,
}

fn wait_for_backoff(commands: &Receiver<BrowserCommand>, backoff: Duration) -> Wait {
    let deadline = std::time::Instant::now() + backoff;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Wait::Elapsed;
        }
        match commands.recv_timeout(remaining) {
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return Wait::Elapsed,
            Err(RecvTimeoutError::Disconnected) => return Wait::Shutdown,
        }
    }
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

/// Runs one command, returning `Some((action, reason))` when it failed so the
/// caller can report it.
fn apply(pane_id: PaneId, session: &str, command: BrowserCommand) -> Option<(String, String)> {
    let (action, result) = match command {
        BrowserCommand::Navigate(url) => ("navigate", daemon::navigate(session, &url)),
        BrowserCommand::Reload => ("reload", daemon::reload(session)),
        BrowserCommand::Back => ("back", daemon::back(session)),
        BrowserCommand::Forward => ("forward", daemon::forward(session)),
        BrowserCommand::SetViewport { width, height } => {
            ("set viewport", daemon::set_viewport(session, width, height))
        }
        BrowserCommand::MouseDown { x, y } => ("mouse down", mouse_down(session, x, y)),
        BrowserCommand::MouseMove { x, y } => ("mouse move", daemon::mouse_move(session, x, y)),
        BrowserCommand::MouseUp { x, y } => ("mouse up", mouse_up(session, x, y)),
        BrowserCommand::Scroll { delta_x, delta_y } => {
            ("scroll", daemon::wheel(session, delta_x, delta_y))
        }
        BrowserCommand::TypeText(text) => ("type", daemon::type_text(session, &text)),
        BrowserCommand::PressKey(key) => ("press", daemon::press_key(session, &key)),
    };
    match result {
        Ok(()) => None,
        Err(err) => {
            tracing::warn!(pane_id = pane_id.raw(), action, %err, "browser pane: command failed");
            // Pointer and viewport traffic is continuous and self-correcting;
            // reporting every transient miss would bury the failures a user
            // can act on.
            reportable(action).then(|| (action.to_string(), err))
        }
    }
}

/// Whether a failed action is worth telling the user about, as opposed to one
/// the next poll or gesture supersedes anyway.
fn reportable(action: &str) -> bool {
    matches!(
        action,
        "navigate" | "reload" | "back" | "forward" | "type" | "press"
    )
}

/// Minimum gap between page-info reads, independent of frame rate.
const PAGE_INFO_INTERVAL: Duration = Duration::from_millis(750);

fn page_info_due(last: Option<std::time::Instant>) -> bool {
    last.is_none_or(|last| last.elapsed() >= PAGE_INFO_INTERVAL)
}

/// Reads the page's url and title, returning an event only when either
/// actually changed so an idle page costs no main-loop work.
fn poll_page_info(
    pane_id: PaneId,
    session: &str,
    last: &mut Option<(Option<String>, Option<String>)>,
) -> Option<AppEvent> {
    let page = match daemon::page_info(session) {
        Ok(page) => page,
        Err(err) => {
            // Not fatal and not worth reporting: the screenshot poll is the
            // authority on whether the session is alive.
            tracing::debug!(pane_id = pane_id.raw(), %err, "browser pane: page info failed");
            return None;
        }
    };
    if last.as_ref() == Some(&page) {
        return None;
    }
    *last = Some(page.clone());
    let (url, title) = page;
    Some(AppEvent::BrowserPageInfo {
        pane_id,
        url,
        title,
    })
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

    #[test]
    fn recovery_replays_the_last_navigate_and_viewport() {
        let mut state = SessionState::default();
        state.record(&BrowserCommand::Navigate("https://example.com".to_string()));
        state.record(&BrowserCommand::SetViewport {
            width: 800,
            height: 600,
        });
        state.record(&BrowserCommand::TypeText("ignored".to_string()));
        state.record(&BrowserCommand::Navigate(
            "https://later.example".to_string(),
        ));

        // Only the newest of each survives; a relaunch restores the page the
        // user was last on, at the pane's current size.
        assert_eq!(state.url.as_deref(), Some("https://later.example"));
        assert_eq!(state.viewport, Some((800, 600)));
    }

    #[test]
    fn relaunch_backoff_grows_and_stays_finite() {
        assert_eq!(backoff_for(1), RECOVERY_BACKOFF);
        assert_eq!(backoff_for(2), RECOVERY_BACKOFF * 2);
        assert_eq!(backoff_for(3), RECOVERY_BACKOFF * 4);
        // The budget stops well before this, but the arithmetic must not
        // overflow if it ever does not.
        assert!(backoff_for(u32::MAX) > RECOVERY_BACKOFF);
    }

    #[test]
    fn a_closed_pane_aborts_recovery_instead_of_waiting_out_the_backoff() {
        let (tx, rx) = std::sync::mpsc::channel::<BrowserCommand>();
        drop(tx);
        let started = std::time::Instant::now();
        assert!(matches!(
            wait_for_backoff(&rx, Duration::from_secs(30)),
            Wait::Shutdown
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn commands_arriving_during_recovery_do_not_cut_the_backoff_short() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(BrowserCommand::PressKey("Enter".to_string()))
            .unwrap();
        let started = std::time::Instant::now();
        assert!(matches!(
            wait_for_backoff(&rx, Duration::from_millis(150)),
            Wait::Elapsed
        ));
        // The queued command is aimed at a browser that no longer exists, so
        // it is dropped rather than shortening the wait.
        assert!(started.elapsed() >= Duration::from_millis(150));
    }
}
