//! In-app Browser pane: renders a live `agent-browser`-driven Chrome session
//! into a pane via herdr's existing Kitty-graphics pane overlay pipeline,
//! and accepts commands (navigate, click, scroll) from both local input
//! routing and the socket API.
//!
//! Pane-creation/teardown orchestration (touching `App`/`AppState`) lives in
//! `src/app/api/browser.rs`, mirroring where plugin-pane orchestration lives
//! relative to its own `src/browser`-equivalent primitives. This module only
//! owns the browser-session subsystem itself: the per-pane background actor
//! thread and its `agent-browser` client.
//!
//! Known gap -- Browser panes are runtime-only and are not persisted. Nothing
//! in `src/persist/` records that a pane was a Browser pane, so a saved
//! session restores it as an ordinary shell pane that still carries the
//! `"browser"` manual label set in `src/app/api/browser.rs`. Its
//! `agent-browser` session is not restored; it is stopped at shutdown by
//! `App::stop_all_browser_sessions`. Closing that gap means adding a marker
//! to `PaneSnapshot` and respawning the actor during restore, which touches
//! persisted state and the restore/handoff path.

pub(crate) mod actor;
pub(crate) mod client;
pub(crate) mod daemon;

/// Commands accepted by a Browser pane's actor thread.
#[derive(Debug)]
pub(crate) enum BrowserCommand {
    Navigate(String),
    /// Pixel-coordinate click (move + mousedown + mouseup), for manual
    /// mouse input routed from `src/app/input/mouse.rs`.
    Click {
        x: i32,
        y: i32,
    },
    /// Not yet sent by any caller (MVP only routes clicks, not continuous
    /// motion -- see `forward_pane_mouse_button`'s doc comment). Kept as
    /// part of the actor's command surface since `daemon::mouse_move`
    /// already exists and this is the obvious next input to wire up.
    #[allow(dead_code)]
    MouseMove {
        x: i32,
        y: i32,
    },
    /// Not yet sent by any caller -- scroll routing depends on each pane's
    /// `wheel_routing` mode (`src/app/input/mouse.rs`'s
    /// `forward_pane_reported_wheel`), which needs more design than MVP
    /// scope covers to wire correctly. `daemon::wheel` is ready for it.
    #[allow(dead_code)]
    Scroll {
        delta_x: i32,
        delta_y: i32,
    },
}

/// Runtime handle for one Browser pane's actor thread, the Browser-pane
/// analog of `TerminalRuntime`: lives on `App.browser_actors` (runtime),
/// never on `AppState` -- a channel `Sender` is a live resource, not data.
/// `AppState.browser_panes` holds the corresponding pure-data marker (just
/// "this pane id is a browser pane"); the session name itself is derived
/// deterministically from the pane id via [`daemon::session_name`] rather
/// than stored twice.
///
/// Dropping the sender is the actor's shutdown signal: its `recv_timeout`
/// loop (`actor.rs`) sees `Disconnected` and stops the `agent-browser`
/// session before exiting.
pub(crate) type BrowserActorHandle = std::sync::mpsc::Sender<BrowserCommand>;
