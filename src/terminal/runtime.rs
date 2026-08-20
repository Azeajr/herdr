use std::sync::Arc;

use crate::render_signal::RenderSignal;

use bytes::Bytes;
use ratatui::{layout::Rect, Frame};
use tokio::sync::{mpsc, Notify};
use tracing::warn;

use crate::events::AppEvent;
use crate::layout::PaneId;

/// A pane this server had a peer spawn, and the identity that makes its id
/// meaningful.
///
/// The pane id is peer-local, so it names the intended pane only while the
/// server that issued it is still the one answering. Anything acting on the id
/// after a gap — a retry, a deferred cleanup — has to carry the instance with
/// it and compare, or it risks closing an unrelated pane on a replacement
/// server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnedPeerPane<'a> {
    pub peer: &'a str,
    pub peer_pane_id: &'a str,
    pub expected_instance: Option<&'a str>,
}

/// Live runtime for a server-owned terminal.
///
/// The PTY implementation still delegates to the legacy pane runtime while the
/// migration proceeds, but production code now depends on this terminal-layer
/// type instead of the pane module's implementation detail.
pub enum TerminalRuntime {
    /// A local pty, parsed and rendered in this process.
    Pty(crate::pane::PaneRuntime),
    /// A terminal owned by a peer server. The peer holds the screen and streams
    /// rendered cells, so no local VT state backs the queries below.
    ///
    /// Three answers are possible here, and which one a method gets is a
    /// decision, not an oversight:
    ///
    /// - **Forwarded over the open control connection** when the peer already
    ///   accepts the operation and it needs no reply — scrolling and input.
    /// - **Forwarded as a request to the peer's JSON API**, which happens above
    ///   this type: a read has to be able to fail and must not block the event
    ///   loop, and neither is possible inside a `&self` accessor. See
    ///   `App::request_targets_peer_pane`.
    /// - **Reported as absent** for what the peer cannot answer — selection,
    ///   scroll position, VT-derived metadata. Callers that would present an
    ///   affordance from these check [`Self::remote`] first and say so, rather
    ///   than showing one that silently does nothing.
    /// - **Read back off the peer's own last frame** for the few facts the peer
    ///   already streams as part of rendering. The cursor is the one that
    ///   matters: it arrives in every `FrameData`, so reporting it as absent is
    ///   not "the peer cannot answer this", it is discarding an answer already
    ///   in hand — and downstream that became a pane with no cursor at all.
    ///   Check whether the frame carries it before adding to the list above.
    Remote(Box<super::remote::RemoteTerminalRuntime>),
}

impl TerminalRuntime {
    /// The local pty runtime, or `None` for a remote terminal.
    fn pty(&self) -> Option<&crate::pane::PaneRuntime> {
        match self {
            Self::Pty(runtime) => Some(runtime),
            Self::Remote(_) => None,
        }
    }

    /// Whether another server owns this terminal.
    ///
    /// Asked by the layout, which freezes pane sizes while no client is
    /// attached: a local pty's size is invisible until someone looks, but a
    /// remote pane's size is what a *different machine* is rendering at right
    /// now, so it has to keep tracking the layout either way.
    pub(crate) fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    /// The peer-side id a remote terminal controls, or `None` for a local pty.
    ///
    /// This is how an operation on a local pane finds the terminal it mirrors on
    /// the peer, which is per pane rather than per workspace.
    pub fn remote_target(&self) -> Option<&str> {
        self.remote().map(|runtime| runtime.target())
    }

    /// The peer-backed view behind this terminal, or `None` for a local pty.
    pub fn remote(&self) -> Option<&super::remote::RemoteTerminalRuntime> {
        match self {
            Self::Pty(_) => None,
            Self::Remote(runtime) => Some(runtime),
        }
    }

    /// Mutable access to the peer-backed view, for the reconnect bookkeeping the
    /// app event loop owns.
    pub fn remote_mut(&mut self) -> Option<&mut super::remote::RemoteTerminalRuntime> {
        match self {
            Self::Pty(_) => None,
            Self::Remote(runtime) => Some(runtime),
        }
    }

    /// The peer that spawned this terminal's pane on our behalf, paired with the
    /// peer-side pane id to close.
    ///
    /// `None` for a local pty and for a view onto a pane the peer already had:
    /// closing a view only closes what we asked the peer to create.
    pub fn spawned_peer_pane(&self) -> Option<SpawnedPeerPane<'_>> {
        match self {
            Self::Pty(_) => None,
            Self::Remote(runtime) => runtime.spawned_on_peer().map(|peer| SpawnedPeerPane {
                peer,
                peer_pane_id: runtime.target(),
                expected_instance: runtime.peer_instance_id(),
            }),
        }
    }

    /// Gives up the claim that closing this view should close the pane behind
    /// it, reporting whether there was one.
    ///
    /// Used when the peer itself is going away: the pane stays running on the
    /// peer, which records it as owned by an instance that is no longer
    /// attached, so this side stops trying to reach a peer it just dropped.
    pub fn disown_spawned_peer_pane(&mut self) -> bool {
        match self {
            Self::Pty(_) => false,
            Self::Remote(runtime) => runtime.clear_spawned_on_peer(),
        }
    }

    /// Takes and clears the "a new frame arrived" flag for a peer-backed
    /// terminal. Always false for a local pty, which signals redraws through
    /// the render dirty signal instead.
    pub fn take_remote_frame_dirty(&self) -> bool {
        match self {
            Self::Pty(_) => false,
            Self::Remote(runtime) => runtime.take_dirty(),
        }
    }

    /// Opens a terminal owned by a peer server, already wrapped as a runtime.
    ///
    /// `socket_path` is the peer's *client* protocol socket, not its JSON API
    /// socket. `target` is resolved on the peer side by terminal id, public
    /// pane id, or agent name.
    ///
    /// Test-only. Production opens a peer view on a worker thread and needs the
    /// inner runtime to carry back to the event loop, so it calls
    /// [`super::remote::RemoteTerminalRuntime::connect`] directly.
    #[cfg(test)]
    pub fn connect_remote(
        socket_path: &std::path::Path,
        peer: String,
        target: String,
        cols: u16,
        rows: u16,
        takeover: bool,
    ) -> std::io::Result<Self> {
        super::remote::RemoteTerminalRuntime::connect(
            socket_path,
            peer,
            target,
            cols,
            rows,
            takeover,
        )
        .map(|runtime| Self::Remote(Box::new(runtime)))
    }
}

impl TerminalRuntime {
    pub fn shutdown(self) {
        match self {
            Self::Pty(runtime) => runtime.shutdown(),
            Self::Remote(runtime) => runtime.shutdown(),
        }
    }

    #[cfg(unix)]
    pub fn duplicate_handoff_fd(&self) -> std::io::Result<std::os::fd::RawFd> {
        self.pty()
            .ok_or_else(|| std::io::Error::other("remote terminals have no local pty to hand off"))?
            .duplicate_handoff_fd()
    }

    #[cfg(unix)]
    pub fn preserve_for_handoff(self) {
        if let Self::Pty(runtime) = self {
            runtime.preserve_for_handoff();
        }
    }

    #[cfg(unix)]
    pub fn assume_handoff_ownership(&mut self) {
        if let Self::Pty(runtime) = self {
            runtime.assume_handoff_ownership();
        }
    }

    #[cfg(unix)]
    pub fn set_handoff_reader_paused(&self, paused: bool) {
        if let Some(runtime) = self.pty() {
            runtime.set_handoff_reader_paused(paused);
        }
    }

    #[cfg(unix)]
    pub fn pause_handoff_reader(&self, timeout: std::time::Duration) -> std::io::Result<()> {
        match self.pty() {
            Some(runtime) => runtime.pause_handoff_reader(timeout),
            // Nothing local is reading, so there is nothing to pause.
            None => Ok(()),
        }
    }

    #[cfg(unix)]
    pub fn handoff_runtime_state(
        &self,
        pane_id: u32,
    ) -> crate::handoff_runtime::HandoffRuntimeState {
        match self.pty() {
            Some(runtime) => runtime.handoff_runtime_state(pane_id),
            // Remote terminals hold no local pty, so there is nothing to export.
            // The peer keeps its own terminal across a local handoff.
            None => crate::handoff_runtime::HandoffRuntimeState::empty_for_pane(pane_id),
        }
    }

    #[cfg(unix)]
    pub fn handoff_history_ansi(&self) -> Option<String> {
        self.pty()
            .and_then(|runtime| runtime.handoff_history_ansi())
    }

    #[cfg(unix)]
    pub fn from_handoff_fd(
        import: crate::handoff_runtime::ImportedHandoffRuntime,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::from_handoff_fd(
            import,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self::Pty)
    }

    // Wrapper mirrors pane runtime construction arguments.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &crate::pane::PaneLaunchEnv,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn(
            pane_id,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            launch_env,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self::Pty)
    }

    // Wrapper mirrors pane runtime construction arguments.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_initial_history(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &crate::pane::PaneLaunchEnv,
        initial_history_ansi: Option<&str>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_with_initial_history(
            pane_id,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            launch_env,
            initial_history_ansi,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self::Pty)
    }

    // Wrapper mirrors pane runtime construction arguments.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_shell_command(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        command: &str,
        launch_env: &crate::pane::PaneLaunchEnv,
        agent_detection: crate::pane::AgentDetection,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_shell_command(
            pane_id,
            rows,
            cols,
            cwd,
            command,
            launch_env,
            agent_detection,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self::Pty)
    }

    // Wrapper mirrors pane runtime construction arguments, including detection policy.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_argv_command(
        pane_id: PaneId,
        rows: u16,
        cols: u16,
        cwd: std::path::PathBuf,
        argv: &[String],
        launch_env: &crate::pane::PaneLaunchEnv,
        agent_detection: crate::pane::AgentDetection,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<RenderSignal>,
    ) -> std::io::Result<Self> {
        crate::pane::PaneRuntime::spawn_argv_command(
            pane_id,
            rows,
            cols,
            cwd,
            argv,
            launch_env,
            agent_detection,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            events,
            render_notify,
            render_dirty,
        )
        .map(Self::Pty)
    }

    pub fn apply_host_terminal_theme(&self, theme: crate::terminal_theme::TerminalTheme) {
        if let Some(runtime) = self.pty() {
            runtime.apply_host_terminal_theme(theme);
        }
    }

    pub fn apply_host_terminal_appearance(
        &self,
        appearance: Option<crate::terminal_theme::HostAppearance>,
    ) {
        if let Some(runtime) = self.pty() {
            runtime.apply_host_terminal_appearance(appearance);
        }
    }

    pub fn begin_graceful_release(&self, agent: crate::detect::Agent) {
        if let Some(runtime) = self.pty() {
            runtime.begin_graceful_release(agent);
        }
    }

    pub fn reset_agent_detection(&self) {
        if let Some(runtime) = self.pty() {
            runtime.reset_agent_detection();
        }
    }

    #[cfg(test)]
    pub(crate) fn agent_detection_reset_notify_for_test(
        &self,
    ) -> std::sync::Arc<tokio::sync::Notify> {
        self.pty()
            .map(|runtime| runtime.agent_detection_reset_notify_for_test())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn agent_detection_enabled_for_test(&self) -> bool {
        self.pty()
            .map(|runtime| runtime.agent_detection_enabled_for_test())
            .unwrap_or_default()
    }

    pub fn set_full_lifecycle_authority_active(&self, active: bool) {
        if let Some(runtime) = self.pty() {
            runtime.set_full_lifecycle_authority_active(active);
        }
    }

    pub fn resize(&self, rows: u16, cols: u16, cell_width_px: u32, cell_height_px: u32) {
        match self {
            Self::Pty(runtime) => runtime.resize(rows, cols, cell_width_px, cell_height_px),
            // The peer treats the controlling connection as authoritative for
            // this terminal's size, so the local layout drives it.
            Self::Remote(runtime) => runtime.resize(rows, cols, cell_width_px, cell_height_px),
        }
    }

    #[cfg(unix)]
    pub fn nudge_child_redraw_after_handoff(&self) {
        if let Some(runtime) = self.pty() {
            runtime.nudge_child_redraw_after_handoff();
        }
    }

    pub fn scroll_up(&self, lines: usize) {
        match self {
            Self::Pty(runtime) => runtime.scroll_up(lines),
            Self::Remote(runtime) => {
                runtime.scroll(crate::protocol::AttachScrollDirection::Up, lines)
            }
        }
    }

    pub fn scroll_down(&self, lines: usize) {
        match self {
            Self::Pty(runtime) => runtime.scroll_down(lines),
            Self::Remote(runtime) => {
                runtime.scroll(crate::protocol::AttachScrollDirection::Down, lines)
            }
        }
    }

    pub fn scroll_reset(&self) {
        if let Some(runtime) = self.pty() {
            runtime.scroll_reset();
        }
        // A remote terminal needs no explicit reset: every caller here resets
        // before sending input, and the peer already resets its own scrollback
        // when that input arrives. Forwarding a second reset would be a wire
        // round trip to redo what the input itself does.
    }

    pub fn set_scroll_offset_from_bottom(&self, lines: usize) {
        match self {
            Self::Pty(runtime) => runtime.set_scroll_offset_from_bottom(lines),
            // The peer's protocol addresses scrolling in deltas, so an absolute
            // target becomes the distance from where its frame says it is. This
            // exists because a remote pane now reports scroll metrics and so
            // draws a scrollbar: without it the bar appears, accepts a drag,
            // and does nothing.
            Self::Remote(runtime) => {
                let Some(metrics) = runtime.scroll_metrics() else {
                    return;
                };
                let target = lines.min(metrics.max_offset_from_bottom);
                let current = metrics.offset_from_bottom;
                let (direction, delta) = if target >= current {
                    (crate::protocol::AttachScrollDirection::Up, target - current)
                } else {
                    (
                        crate::protocol::AttachScrollDirection::Down,
                        current - target,
                    )
                };
                if delta > 0 {
                    runtime.scroll(direction, delta);
                }
            }
        }
    }

    /// Scrollback position.
    ///
    /// A remote terminal answers from the position its peer stamped on the
    /// retained frame, so the offset always describes the cells currently
    /// drawn. `None` until a frame carrying one has arrived, and from a peer
    /// too old to send it — callers already read `None` as "no scrollbar".
    pub fn scroll_metrics(&self) -> Option<crate::pane::ScrollMetrics> {
        match self {
            Self::Pty(runtime) => runtime.scroll_metrics(),
            Self::Remote(runtime) => runtime.scroll_metrics(),
        }
    }

    /// Screen matches for `query`, or none when called directly on a remote
    /// terminal. Copy mode intercepts that case and asks the owning peer through
    /// `pane.text_query`; this synchronous accessor never performs I/O.
    pub(crate) fn search_text_matches(
        &self,
        query: &str,
        case_sensitive: bool,
    ) -> Vec<crate::pane::TerminalTextMatch> {
        self.pty()
            .map(|runtime| runtime.search_text_matches(query, case_sensitive))
            .unwrap_or_default()
    }

    pub(crate) fn text_match_is_current(&self, text_match: crate::pane::TerminalTextMatch) -> bool {
        match self {
            Self::Pty(runtime) => runtime.text_match_is_current(text_match),
            // Remote matches came from the owning peer and are admitted only
            // for the active query generation. This side has no VT fingerprint
            // with which to validate them again.
            Self::Remote(_) => true,
        }
    }

    pub(crate) fn text_matches_are_current(
        &self,
        text_matches: &[crate::pane::TerminalTextMatch],
    ) -> Vec<bool> {
        match self {
            Self::Pty(runtime) => runtime.text_matches_are_current(text_matches),
            Self::Remote(_) => vec![true; text_matches.len()],
        }
    }

    pub(crate) fn word_motion_target(
        &self,
        row: u32,
        col: u16,
        motion: crate::pane::TerminalWordMotion,
    ) -> Option<crate::pane::TerminalTextPoint> {
        self.pty()
            .and_then(|runtime| runtime.word_motion_target(row, col, motion))
    }

    /// Collects the complete terminal input-mode snapshot.
    ///
    /// This performs multiple terminal queries. Keep it out of render/layout
    /// and pane-scaled loops; add a narrow accessor when one fact is needed.
    #[cfg(test)]
    pub fn input_state(&self) -> Option<crate::pane::InputState> {
        self.pty().and_then(|runtime| runtime.input_state())
    }

    pub fn keyboard_report_all_requested(&self) -> bool {
        self.pty()
            .is_some_and(|runtime| runtime.keyboard_report_all_requested())
    }

    pub fn bracketed_paste_enabled(&self) -> bool {
        self.bracketed_paste()
    }

    pub fn mouse_reporting_enabled(&self) -> bool {
        self.mouse_reporting()
    }

    pub fn sgr_pixel_mouse_enabled(&self) -> bool {
        self.pty()
            .is_some_and(|runtime| runtime.sgr_pixel_mouse_enabled())
    }

    pub fn plain_page_keys_use_host_scrollback(&self) -> Option<bool> {
        self.pty()
            .and_then(|runtime| runtime.plain_page_keys_use_host_scrollback())
    }

    /// Reads only whether the alternate screen is active.
    pub fn alternate_screen_active(&self) -> bool {
        self.pty()
            .map(|runtime| runtime.alternate_screen_active())
            .unwrap_or_default()
    }

    /// Reads only whether this terminal wants pasted text bracketed.
    ///
    /// Not `self.pty().map(...)` like its neighbours, for the same reason
    /// [`Self::cursor_state`] is not: a peer-backed terminal has a paste mode,
    /// it just lives on the other machine. Answering the local default there
    /// sends a paste unwrapped and lets an embedded newline run as a command.
    pub fn bracketed_paste(&self) -> bool {
        match self {
            Self::Pty(runtime) => runtime.bracketed_paste_enabled(),
            Self::Remote(runtime) => runtime.bracketed_paste(),
        }
    }

    /// Whether the program on this terminal has asked to receive mouse events.
    ///
    /// Decides who owns a click: the program, or the pane's own selection. A
    /// peer-backed terminal answers from what its peer reported, because a view
    /// that assumes the program always wants the mouse can never start a
    /// selection — which is what made dragging over such a pane select nothing.
    pub fn mouse_reporting(&self) -> bool {
        match self {
            Self::Pty(runtime) => runtime.mouse_reporting_enabled(),
            Self::Remote(runtime) => runtime.mouse_reporting(),
        }
    }

    /// The focused pane's cursor, from the local VT or from the peer's own
    /// last frame.
    ///
    /// Not `self.pty().and_then(...)` like its neighbours: for a remote pane
    /// that answered `None`, which the pane grid reads as "no cursor" and turns
    /// into `\x1b[?25l` on the client. A peer-backed pane has a cursor, it just
    /// lives on the other machine, so it is asked for rather than skipped.
    pub fn cursor_state(
        &self,
        area: Rect,
        show_cursor: bool,
    ) -> Option<crate::pane::TerminalCursorState> {
        match self {
            Self::Pty(runtime) => runtime.cursor_state(area, show_cursor),
            Self::Remote(runtime) => runtime.cursor_state(area, show_cursor),
        }
    }

    pub fn synchronized_output_active(&self) -> bool {
        self.pty()
            .map(|runtime| runtime.synchronized_output_active())
            .unwrap_or_default()
    }

    pub fn visible_text(&self) -> String {
        self.pty()
            .map(|runtime| runtime.visible_text())
            .unwrap_or_default()
    }

    pub fn visible_ansi(&self) -> String {
        self.pty()
            .map(|runtime| runtime.visible_ansi())
            .unwrap_or_default()
    }

    pub fn detection_text(&self) -> String {
        self.pty()
            .map(|runtime| runtime.detection_text())
            .unwrap_or_default()
    }

    /// The OSC title, from the local VT or from what the peer last reported.
    ///
    /// Feeding the cached value through the same accessor is what lets
    /// `sync_terminal_titles` label a remote pane without knowing peers exist.
    pub fn terminal_title(&self) -> Option<String> {
        match self {
            Self::Pty(runtime) => runtime.terminal_title(),
            Self::Remote(runtime) => runtime
                .metadata()
                .and_then(|metadata| metadata.terminal_title.clone()),
        }
    }

    pub fn agent_osc_title(&self) -> String {
        match self {
            Self::Pty(runtime) => runtime.agent_osc_title(),
            Self::Remote(runtime) => runtime
                .metadata()
                .and_then(|metadata| metadata.agent_osc_title.clone())
                .unwrap_or_default(),
        }
    }

    pub fn agent_osc_progress(&self) -> String {
        match self {
            Self::Pty(runtime) => runtime.agent_osc_progress(),
            Self::Remote(runtime) => runtime
                .metadata()
                .and_then(|metadata| metadata.agent_osc_progress.clone())
                .unwrap_or_default(),
        }
    }

    /// Test-only: production reads go through [`Self::recent_text_snapshot`],
    /// which reports truncation as well as text.
    #[cfg(test)]
    pub fn recent_text(&self, lines: usize) -> String {
        self.pty()
            .map(|runtime| runtime.recent_text_snapshot(lines).text)
            .unwrap_or_default()
    }

    pub(crate) fn recent_text_snapshot(&self, lines: usize) -> crate::pane::TerminalReadSnapshot {
        self.pty()
            .map(|runtime| runtime.recent_text_snapshot(lines))
            .unwrap_or_default()
    }

    pub(crate) fn recent_ansi_snapshot(&self, lines: usize) -> crate::pane::TerminalReadSnapshot {
        self.pty()
            .map(|runtime| runtime.recent_ansi_snapshot(lines))
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn recent_unwrapped_text(&self, lines: usize) -> String {
        self.pty()
            .map(|runtime| runtime.recent_unwrapped_text_snapshot(lines).text)
            .unwrap_or_default()
    }

    pub(crate) fn recent_unwrapped_text_snapshot(
        &self,
        lines: usize,
    ) -> crate::pane::TerminalReadSnapshot {
        self.pty()
            .map(|runtime| runtime.recent_unwrapped_text_snapshot(lines))
            .unwrap_or_default()
    }

    pub(crate) fn recent_unwrapped_ansi_snapshot(
        &self,
        lines: usize,
    ) -> crate::pane::TerminalReadSnapshot {
        self.pty()
            .map(|runtime| runtime.recent_unwrapped_ansi_snapshot(lines))
            .unwrap_or_default()
    }

    /// A deferred read of this terminal's history, for callers that must not
    /// pay for it where they stand. A peer-backed view has no local history.
    ///
    /// There is deliberately no eager variant: reading history costs
    /// milliseconds per populated pane, and the one caller that wants it is the
    /// session save, which must not pay that on the server loop.
    pub fn history_source(&self) -> Option<crate::pane::PaneHistorySource> {
        self.pty().map(crate::pane::PaneRuntime::history_source)
    }

    #[cfg(test)]
    pub fn read_history_now(&self) -> Option<String> {
        self.history_source().and_then(|read| read())
    }

    /// Text under a selection, from a terminal this server owns.
    ///
    /// `None` for a peer-backed pane, and this one stays that way: matching
    /// what the terminal itself would return means matching how it joins
    /// soft-wrapped lines and where it trims, which is a formatter in the
    /// vendored terminal rather than a rule that can be restated against a
    /// grid of cells. The peer runs that formatter, so the peer is asked —
    /// see `pane.read_range`, and the copy path that forwards to it.
    pub fn extract_selection(&self, selection: &crate::selection::Selection) -> Option<String> {
        self.pty()
            .and_then(|runtime| runtime.extract_selection(selection))
    }

    /// The same read, named by coordinates, for answering another server.
    pub fn read_text_range(&self, start: (u16, u32), end: (u16, u32)) -> Option<String> {
        self.pty()
            .and_then(|runtime| runtime.read_text_range(start, end))
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, show_cursor: bool) {
        match self {
            Self::Pty(runtime) => runtime.render(frame, area, show_cursor),
            Self::Remote(runtime) => runtime.render(frame, area, show_cursor),
        }
    }

    pub(crate) fn collect_dirty_patch(
        &self,
        area_width: u16,
        area_height: u16,
    ) -> crate::pane::TerminalDirtyPatchOutcome {
        match self {
            Self::Pty(runtime) => runtime.collect_dirty_patch(area_width, area_height),
            // No local VT state to diff against, so a changed remote pane can
            // only ask for a full redraw. An *unchanged* one must not: the
            // retained path bails to a whole-UI render on the first `Fallback`
            // in the tab, so answering it unconditionally made one peer-backed
            // pane cost every neighbouring local pane its row patch, on every
            // tick, for every attached client.
            Self::Remote(runtime) if !runtime.frame_pending() => {
                crate::pane::TerminalDirtyPatchOutcome::Clean
            }
            Self::Remote(_) => crate::pane::TerminalDirtyPatchOutcome::Fallback,
        }
    }

    /// Reported as absent for a peer-backed terminal: hyperlinks come from the
    /// VT's own cell attributes, which live on the peer. The frame this side
    /// blits carries rendered cells, not their OSC 8 targets.
    /// OSC 8 links on screen, from either kind of terminal.
    ///
    /// A remote terminal answers from its retained frame, which already carries
    /// the URIs and the cells that reference them. Answering empty here is what
    /// made a link in a peer-backed pane unclickable while it was plainly
    /// underlined on screen.
    pub fn visible_hyperlinks(&self, area: Rect) -> Vec<((u16, u16), String, String)> {
        match self {
            Self::Pty(runtime) => runtime.visible_hyperlinks(area),
            Self::Remote(runtime) => runtime.visible_hyperlinks(area),
        }
    }

    /// Reported as absent for a peer-backed terminal: the peer owns the image
    /// store its placements point into, and the frame arrives already rendered.
    pub fn kitty_image_placements_with_data_filter<F>(
        &self,
        needs_data: F,
    ) -> Vec<crate::ghostty::KittyImagePlacement>
    where
        F: FnMut(crate::ghostty::KittyImageDescriptor) -> bool,
    {
        self.pty()
            .map(|runtime| runtime.kitty_image_placements_with_data_filter(needs_data))
            .unwrap_or_default()
    }

    /// Which encoding this terminal's program wants for a keypress.
    ///
    /// For a remote terminal this is what the *peer* last reported, not a local
    /// assumption. Keyboard protocol is VT state, so a client that guessed would
    /// be wrong whenever the program changed it — and guessing `Legacy` against
    /// a program that had enabled the Kitty protocol turned Shift+Enter into a
    /// plain Enter, which submits an agent's prompt instead of adding a newline.
    ///
    /// Stale by at most one of the peer's pane polls, and only for a program
    /// that changes the mode after startup. That is a far smaller wrong than the
    /// constant it replaced.
    pub fn keyboard_protocol(&self) -> crate::input::KeyboardProtocol {
        match self {
            Self::Pty(runtime) => runtime.keyboard_protocol(),
            Self::Remote(runtime) => runtime.keyboard_protocol(),
        }
    }

    /// This terminal's keyboard protocol as clients are told it.
    ///
    /// `None` for a remote terminal that its peer has not described yet: the
    /// answer belongs to whichever server holds the screen, and inventing one
    /// here is what this whole path exists to stop.
    pub fn keyboard_protocol_info(&self) -> Option<crate::api::schema::KeyboardProtocolInfo> {
        use crate::api::schema::KeyboardProtocolInfo;
        let protocol = match self {
            Self::Pty(runtime) => runtime.keyboard_protocol(),
            Self::Remote(runtime) => runtime.reported_keyboard_protocol()?,
        };
        Some(match protocol {
            crate::input::KeyboardProtocol::Legacy => KeyboardProtocolInfo::Legacy,
            crate::input::KeyboardProtocol::Kitty { flags } => {
                KeyboardProtocolInfo::Kitty { flags }
            }
        })
    }

    pub fn encode_terminal_key(&self, key: crate::input::TerminalKey) -> Vec<u8> {
        match self.pty() {
            Some(runtime) => runtime.encode_terminal_key(key),
            // A remote terminal still takes input — it is forwarded over the
            // control connection — so this has to encode rather than report
            // absent. It encodes against the protocol the peer reported for its
            // own terminal, which is the only side that can read it.
            None => crate::input::encode_terminal_key(key, self.keyboard_protocol()),
        }
    }

    pub async fn send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::SendError<Bytes>> {
        match self {
            Self::Pty(runtime) => runtime.send_bytes(bytes).await,
            Self::Remote(runtime) => {
                runtime.send_bytes(bytes);
                Ok(())
            }
        }
    }

    /// Writes input, reporting only what a *local* pty can report.
    ///
    /// A remote write is fire-and-forget and always answers `Ok`: the bytes go
    /// onto that view's writer queue, and every way they can fail from there —
    /// a peer that stopped reading, a queue past its bound, a broken socket —
    /// surfaces later as a disconnect, with the view reconnecting and the input
    /// lost either way. So the `Err` half of this
    /// signature is empty for peer-backed panes, and a caller that retries or
    /// reports backpressure on `Err` silently does neither for them. That is
    /// deliberate — there is no answer to give at this point — but it is a real
    /// difference behind a shared signature, so read `Ok` here as "handed off",
    /// not as "delivered".
    pub fn try_send_bytes(&self, bytes: Bytes) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        match self {
            Self::Pty(runtime) => runtime.try_send_bytes(bytes),
            Self::Remote(runtime) => {
                runtime.send_bytes(bytes);
                Ok(())
            }
        }
    }

    pub fn send_bytes_after(&self, bytes: Bytes, delay: std::time::Duration) {
        match self {
            Self::Pty(runtime) => runtime.send_bytes_after(bytes, delay),
            // Forwarded, like every other write. Skipping it here is what made
            // `agent.prompt` on a peer-backed agent type its text and never
            // send the Enter that submits it.
            Self::Remote(runtime) => runtime.send_bytes_after(bytes, delay),
        }
    }

    pub async fn send_paste(&self, text: String) -> Result<(), mpsc::error::SendError<Bytes>> {
        match self {
            Self::Pty(runtime) => runtime.send_paste(text).await,
            Self::Remote(runtime) => {
                runtime.send_paste(text);
                Ok(())
            }
        }
    }

    pub fn try_send_paste(&self, text: String) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        match self {
            Self::Pty(runtime) => runtime.try_send_paste(text),
            Self::Remote(runtime) => {
                runtime.send_paste(text);
                Ok(())
            }
        }
    }

    pub fn try_send_focus_event(&self, event: crate::ghostty::FocusEvent) -> bool {
        match self {
            Self::Pty(runtime) => runtime.try_send_focus_event(event),
            Self::Remote(runtime) => {
                runtime.send_focus_event(event);
                true
            }
        }
    }

    /// Reported as absent for a peer-backed terminal, and nothing local needs
    /// it: wheel input on such a pane goes over the control connection as
    /// `AttachScroll`, where the peer applies its own routing. See
    /// [`super::remote::RemoteTerminalRuntime::scroll`].
    pub fn wheel_routing(&self) -> Option<crate::pane::WheelRouting> {
        self.pty().and_then(|runtime| runtime.wheel_routing())
    }

    /// Reported as absent for a peer-backed terminal. Screen reads on such a
    /// pane are answered by the peer's JSON API instead, which is why this is
    /// not simply "empty text" — see `App::request_targets_peer_pane`.
    pub(crate) fn screen_text_snapshot(
        &self,
    ) -> Option<(
        crate::ghostty::ActiveScreen,
        crate::terminal::ScreenSnapshot,
    )> {
        let (screen, cols, rows) = self.pty()?.screen_text_snapshot()?;
        Some((screen, crate::terminal::ScreenSnapshot { cols, rows }))
    }

    pub(crate) fn screen_text_snapshot_with_seq(
        &self,
    ) -> Option<(
        crate::ghostty::ActiveScreen,
        crate::terminal::ScreenSnapshot,
        u64,
    )> {
        for _ in 0..3 {
            let before = self.content_seq();
            if !before.is_multiple_of(2) {
                continue;
            }
            let (screen, snapshot) = self.screen_text_snapshot()?;
            let after = self.content_seq();
            if before == after {
                return Some((screen, snapshot, after));
            }
        }
        None
    }

    /// Delivers a press, release, or drag to the terminal, wherever it lives.
    ///
    /// Returns whether the terminal took it, which is what tells a caller
    /// whether the running program consumed the click or it should fall back to
    /// Herdr's own handling.
    ///
    /// A local pty is encoded here, because the VT state that decides the
    /// protocol is here. A peer-backed terminal is not: the event goes over the
    /// control connection and the peer encodes it against the terminal it
    /// actually owns, exactly as it already does for a wheel event.
    pub fn try_send_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> bool {
        match self {
            Self::Pty(_) => {
                let Some(bytes) = self.encode_mouse_button(kind, position, modifiers) else {
                    return false;
                };
                self.scroll_reset();
                if let Err(err) = self.try_send_bytes(Bytes::from(bytes)) {
                    warn!(err = %err, kind = ?kind, "failed to forward mouse button event");
                }
                true
            }
            // `AttachMouse` carries a cell, which is all the peer's own encoder
            // needs. A pixel position names no cell — it only arises for a
            // locally owned terminal on a host reporting SGR 1016 — so it is
            // declined rather than rounded into a cell the peer never saw.
            // Declined outright when the peer's program never asked for the
            // mouse, so the click falls through to this server's own selection
            // — the same rule a local pane applies through
            // `encode_mouse_button`, which answers `None` in that case. Taking
            // every click instead is what left a drag over a peer-backed pane
            // selecting nothing at all.
            Self::Remote(runtime) if !runtime.mouse_reporting() => false,
            Self::Remote(runtime) => match position {
                crate::input::mouse::Position::Cell { column, row } => {
                    runtime.send_mouse(kind, column, row, modifiers)
                }
                crate::input::mouse::Position::Pixels { .. } => false,
            },
        }
    }

    /// Encodes a press, release, or drag against a local pty's VT state.
    ///
    /// Absent for a peer-backed terminal, where the peer does this instead —
    /// see [`Self::try_send_mouse_button`], which is what callers should use.
    pub fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.pty()
            .and_then(|runtime| runtime.encode_mouse_button(kind, position, modifiers))
    }

    /// Absent for a peer-backed terminal, for the same reason as
    /// [`Self::encode_mouse_button`].
    pub(crate) fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.pty()
            .and_then(|runtime| runtime.encode_mouse_motion(kind, position, modifiers))
    }

    /// Absent for a peer-backed terminal, and already handled elsewhere: wheel
    /// input on such a pane is forwarded as `AttachScroll`, so no caller needs
    /// a locally encoded wheel report.
    pub(crate) fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: crate::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.pty()
            .and_then(|runtime| runtime.encode_mouse_wheel(kind, position, modifiers))
    }

    pub(crate) fn pixel_size(&self) -> Option<(u32, u32)> {
        self.pty().and_then(|runtime| runtime.pixel_size())
    }

    /// Absent for a peer-backed terminal: alternate-scroll is a translation the
    /// peer performs itself when it applies `AttachScroll`.
    pub fn encode_alternate_scroll(
        &self,
        kind: crossterm::event::MouseEventKind,
    ) -> Option<Vec<u8>> {
        self.pty()
            .and_then(|runtime| runtime.encode_alternate_scroll(kind))
    }

    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        match self {
            Self::Pty(runtime) => runtime.cwd(),
            Self::Remote(runtime) => runtime.metadata().and_then(|metadata| metadata.cwd.clone()),
        }
    }

    /// The cwd a "follow the shell" workspace should track.
    ///
    /// Stays local-only: following retargets a *local* workspace's directory,
    /// and a path that exists on the peer may name nothing here.
    pub fn follow_cwd(&self) -> Option<std::path::PathBuf> {
        self.pty().and_then(|runtime| runtime.follow_cwd())
    }

    pub fn foreground_cwd(&self) -> Option<std::path::PathBuf> {
        match self {
            Self::Pty(runtime) => runtime.foreground_cwd(),
            Self::Remote(runtime) => runtime
                .metadata()
                .and_then(|metadata| metadata.foreground_cwd.clone()),
        }
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.pty().and_then(|runtime| runtime.child_pid())
    }

    pub(crate) fn current_size(&self) -> super::TerminalSize {
        match self {
            Self::Pty(runtime) => runtime.current_size(),
            Self::Remote(runtime) => runtime.current_size(),
        }
    }

    pub(crate) fn content_seq(&self) -> u64 {
        self.pty()
            .map(|runtime| runtime.content_seq())
            .unwrap_or_default()
    }
}

#[cfg(test)]
impl TerminalRuntime {
    pub(crate) fn test_with_channel(cols: u16, rows: u16) -> (Self, mpsc::Receiver<Bytes>) {
        let (runtime, rx) = crate::pane::PaneRuntime::test_with_channel(cols, rows);
        (Self::Pty(runtime), rx)
    }

    pub(crate) fn test_with_channel_capacity(
        cols: u16,
        rows: u16,
        capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (runtime, rx) =
            crate::pane::PaneRuntime::test_with_channel_capacity(cols, rows, capacity);
        (Self::Pty(runtime), rx)
    }

    pub(crate) fn test_with_screen_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Self {
        Self::Pty(crate::pane::PaneRuntime::test_with_screen_bytes(
            cols, rows, bytes,
        ))
    }

    pub(crate) fn test_process_pty_bytes(&self, bytes: &[u8]) {
        if let Some(runtime) = self.pty() {
            runtime.test_process_pty_bytes(bytes);
        }
    }

    pub(crate) fn test_with_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
    ) -> Self {
        Self::Pty(crate::pane::PaneRuntime::test_with_scrollback_bytes(
            cols,
            rows,
            scrollback_limit_bytes,
            bytes,
        ))
    }

    pub(crate) fn test_with_channel_and_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (runtime, rx) = crate::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(
            cols,
            rows,
            scrollback_limit_bytes,
            bytes,
            channel_capacity,
        );
        (Self::Pty(runtime), rx)
    }
}
