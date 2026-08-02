use ratatui::layout::Direction;

use crate::api::schema::{BrowserOpenParams, ResponseResult};
use crate::app::App;
use crate::layout::PaneId;

use super::responses::{encode_error, encode_success};

/// Placeholder PTY child for a Browser pane's structural slot -- herdr has
/// no non-PTY pane kind (see plan notes), so every pane needs some PTY
/// child. `agent-browser`'s daemon detaches via `setsid()` and can't be
/// this child (see `crate::browser::daemon` docs), so a Browser pane's
/// actual PTY child is an idle placeholder instead, matching the pattern
/// `portable-pty`'s own tests use for a do-nothing child
/// (`src/pty/backend/unix.rs`). Keyboard input isn't routed away from the
/// PTY yet (MVP scope), so typed keys currently reach this placeholder and
/// get echoed back into the pane's terminal grid; deferred to the keyboard
/// input-routing phase.
const BROWSER_PANE_PLACEHOLDER_ARGV: &[&str] = &["cat"];

/// Owner token this module registers in `AppState.pane_graphics_streams` so
/// a live Browser-pane screencast can't be stomped by an unrelated external
/// `pane.graphics.set`/`pane.graphics.stream.*` call against the same pane,
/// and vice versa.
pub(crate) const BROWSER_STREAM_OWNER: &str = "herdr-browser";

impl App {
    /// Handles `AppEvent::BrowserFrame`: pushes a newly polled PNG
    /// screenshot into the pane's graphics overlay via the same mechanism
    /// `pane.graphics.stream.set` uses (`src/app/api/pane_graphics.rs`),
    /// bypassing its owner check since this pane's `pane_id` was already
    /// reserved under [`BROWSER_STREAM_OWNER`] when the pane was spawned.
    pub(crate) fn handle_browser_frame(&mut self, pane_id: PaneId, data: Vec<u8>) {
        if self.state.apply_browser_frame(pane_id, data) {
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }
    }

    /// Handles `AppEvent::BrowserDaemonExited`: clears the pane's graphics
    /// layer/stream reservation. The pane itself (and its placeholder PTY
    /// child) is left alone -- only the browser session died, not the pane.
    pub(crate) fn handle_browser_daemon_exited(&mut self, pane_id: PaneId, reason: String) {
        tracing::warn!(pane_id = pane_id.raw(), %reason, "browser pane: agent-browser session exited");
        self.state.clear_browser_frame(pane_id);
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    pub(super) fn handle_browser_open(&mut self, id: String, params: BrowserOpenParams) -> String {
        let target_pane_id = match params.pane_id {
            Some(pane_id) => pane_id,
            None => match self.current_public_browser_target_pane_id() {
                Some(pane_id) => pane_id,
                None => return encode_error(id, "no_active_pane", "no active pane"),
            },
        };
        let Some((ws_idx, target_pane)) = self.parse_pane_id(&target_pane_id) else {
            return encode_error(
                id,
                "pane_not_found",
                format!("pane {target_pane_id} not found"),
            );
        };
        let (rows, cols) = self.state.estimate_pane_size();
        let argv: Vec<String> = BROWSER_PANE_PLACEHOLDER_ARGV
            .iter()
            .map(|s| s.to_string())
            .collect();
        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return encode_error(id, "workspace_not_found", "workspace not found");
        };
        let result = ws.split_pane_argv_command_with_ratio(
            target_pane,
            Direction::Horizontal,
            0.5,
            rows.max(4),
            cols.max(10),
            None,
            &argv,
            Vec::new(),
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            true,
        );
        let (tab_idx, new_pane) = match result {
            Some(Ok(result)) => result,
            Some(Err(err)) => return encode_error(id, "browser_pane_open_failed", err.to_string()),
            None => {
                return encode_error(
                    id,
                    "pane_not_found",
                    format!("pane {target_pane_id} not found"),
                )
            }
        };
        let pane_id = new_pane.pane_id;

        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let session = crate::browser::daemon::session_name(pane_id);
        let events = self.event_tx.clone();
        let thread_result = std::thread::Builder::new()
            .name(format!("browser-{}", pane_id.raw()))
            .spawn(move || crate::browser::actor::run(pane_id, session, command_rx, events));
        if let Err(err) = thread_result {
            return encode_error(
                id,
                "browser_pane_open_failed",
                format!("failed to start browser pane actor: {err}"),
            );
        }
        if let Some(url) = params.url {
            let _ = command_tx.send(crate::browser::BrowserCommand::Navigate(url));
        }
        self.browser_actors.insert(pane_id, command_tx);

        let mut terminal = new_pane.terminal;
        terminal.set_manual_label("browser".to_string());
        let terminal_id = terminal.id.clone();
        self.terminal_runtimes
            .insert(terminal_id.clone(), new_pane.runtime);
        self.state.remove_alias_shadowed_by_new_pane(pane_id);
        self.state.terminals.insert(terminal_id, terminal);
        self.state.browser_panes.insert(pane_id);
        self.state
            .pane_graphics_streams
            .insert(pane_id, BROWSER_STREAM_OWNER.to_string());

        let previous_focus = self.state.current_pane_focus_target();
        self.state.switch_workspace_tab(ws_idx, tab_idx);
        self.state
            .record_pane_focus_change(previous_focus, ws_idx, pane_id);
        self.state.mode = crate::app::Mode::Terminal;

        self.schedule_session_save();
        let Some(pane) = self.pane_info(ws_idx, pane_id) else {
            return encode_error(id, "browser_pane_open_failed", "browser pane disappeared");
        };
        self.emit_event(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::PaneCreated,
            data: crate::api::schema::EventData::PaneCreated { pane: pane.clone() },
        });
        self.emit_layout_updated_event(ws_idx, tab_idx);
        encode_success(id, ResponseResult::PaneInfo { pane })
    }

    /// Drains `AppState.browser_pointer_events` (queued by
    /// `forward_pane_mouse_button`, which has no runtime access) and sends
    /// each to its pane's actor. Mirrors
    /// `App::dispatch_pending_clipboard_write`'s queue-then-dispatch shape.
    pub(crate) fn dispatch_browser_pointer_events(&mut self) {
        for (pane_id, command) in std::mem::take(&mut self.state.browser_pointer_events) {
            if let Some(commands) = self.browser_actors.get(&pane_id) {
                let _ = commands.send(command);
            }
        }
    }

    fn current_public_browser_target_pane_id(&self) -> Option<String> {
        let ws_idx = self.state.active?;
        let pane_id = self.state.workspaces.get(ws_idx)?.focused_pane_id()?;
        self.public_pane_id(ws_idx, pane_id)
    }

    pub(super) fn handle_browser_navigate(
        &mut self,
        id: String,
        params: crate::api::schema::BrowserNavigateParams,
    ) -> String {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return encode_error(id, "pane_not_found", format!("pane {} not found", params.pane_id));
        };
        let Some(commands) = self.browser_actors.get(&pane_id) else {
            return encode_error(id, "not_browser_pane", "pane is not a Browser pane");
        };
        if commands
            .send(crate::browser::BrowserCommand::Navigate(params.url))
            .is_err()
        {
            return encode_error(id, "browser_pane_gone", "browser pane actor is no longer running");
        }
        encode_success(id, ResponseResult::Ok {})
    }
}
