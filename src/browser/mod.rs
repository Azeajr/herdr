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

// `App::spawn_browser_actor` starts no thread under `cfg(test)` -- the actor's
// first act is launching a real headless Chrome, which a unit-test suite must
// not do. That makes the actor and the CLI wrapper beneath it unreachable from
// the test build, so their `dead_code` warnings there are an artifact of the
// seam rather than unused production code. Each module still has its own unit
// tests, and the live path is covered by the smoke test.
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod actor;
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod client;
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod daemon;
pub(crate) mod keys;
pub(crate) mod url;

/// Commands accepted by a Browser pane's actor thread.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BrowserCommand {
    Navigate(String),
    /// Resizes the page to the pane's pixel size, so the stretched Kitty
    /// placement is a 1:1 blit instead of a distortion. Issued by
    /// `App::sync_browser_viewports` whenever the pane's geometry changes.
    SetViewport {
        width: u32,
        height: u32,
    },
    /// Pixel-coordinate button press. Paired with [`Self::MouseUp`] rather
    /// than pressing and releasing in one go, so a press followed by
    /// [`Self::MouseMove`] is a real drag.
    MouseDown {
        x: i32,
        y: i32,
    },
    /// Pointer moved with the button held: `mouse move` without a press, so
    /// drags, sliders and hover states work.
    MouseMove {
        x: i32,
        y: i32,
    },
    /// Button released at a position, ending a drag started by [`Self::Click`].
    MouseUp {
        x: i32,
        y: i32,
    },
    Reload,
    Back,
    Forward,
    Scroll {
        delta_x: i32,
        delta_y: i32,
    },
    /// Printable text for the focused element. Adjacent values are merged by
    /// the actor into one `keyboard type` call, so a burst of typing is not a
    /// subprocess per character.
    TypeText(String),
    /// A named key or modifier chord, already in agent-browser's spelling
    /// (`Enter`, `Control+a`); see [`keys::command_for_key`].
    PressKey(String),
}

/// Runtime handle for one Browser pane's actor thread, the Browser-pane
/// analog of `TerminalRuntime`: lives on `App.browser_actors` (runtime),
/// never on `AppState` -- a channel `Sender` is a live resource, not data.
/// `PaneState.kind` holds the corresponding pure-data marker (just "this pane
/// is a browser pane"); the session name itself is derived deterministically
/// from the pane id and this server's token via [`daemon::session_name`]
/// rather than stored twice.
///
/// Dropping the sender is the actor's shutdown signal: its `recv_timeout`
/// loop (`actor.rs`) sees `Disconnected` and stops the `agent-browser`
/// session before exiting.
pub(crate) type BrowserActorHandle = std::sync::mpsc::Sender<BrowserCommand>;
