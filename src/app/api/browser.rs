use ratatui::layout::Direction;

use crate::api::schema::{BrowserOpenParams, ResponseResult};
use crate::app::App;
use crate::layout::PaneId;

use super::responses::{encode_error, encode_success};

/// Owner token this module registers in `AppState.pane_graphics_streams` so
/// a live Browser-pane screencast can't be stomped by an unrelated external
/// `pane.graphics.set`/`pane.graphics.stream.*` call against the same pane,
/// and vice versa.
pub(crate) const BROWSER_STREAM_OWNER: &str = "herdr-browser";

/// Ceiling on simultaneously open Browser panes. Each one is a full Chrome
/// plus a polling actor thread, so an unbounded `browser.open` loop is a way
/// to take the machine down rather than a useful workflow.
pub(crate) const MAX_BROWSER_PANES: usize = 4;

/// Short pane label for a page: its host, with `www.` dropped. Falls back to
/// `None` for urls with no host (`about:blank`, `file://`), letting the
/// caller keep the generic label.
fn browser_pane_label(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest)?;
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Strip userinfo and port, which are noise in a pane border.
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    let host = host.strip_prefix("www.").unwrap_or(host);
    (!host.is_empty()).then(|| host.to_string())
}

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

    /// Handles `AppEvent::BrowserDaemonExited`: the actor has already
    /// exhausted its own relaunch attempts (see `crate::browser::actor`), so
    /// the session is gone for good.
    ///
    /// The pane stays a Browser pane and keeps its slot in the layout. That is
    /// what makes recovery possible: [`App::retry_browser_pane`] can put the
    /// same page back in the same place, which neither closing the pane nor
    /// demoting it to a shell could do. Until then it renders the failure
    /// message (`src/ui/panes.rs`) instead of a frame.
    pub(crate) fn handle_browser_daemon_exited(&mut self, pane_id: PaneId, reason: String) {
        tracing::warn!(pane_id = pane_id.raw(), %reason, "browser pane: agent-browser session exited");
        self.detach_browser_actor(pane_id);
        // Drop the stale picture but keep the stream reservation: the pane is
        // still a Browser pane, and nothing else should be able to claim it
        // out from under a pending retry.
        self.state.pane_graphics_layers.remove(&pane_id);
        self.state.pane_graphics_revision = self.state.pane_graphics_revision.wrapping_add(1);
        self.state.browser_pane_errors.insert(pane_id, reason);
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// Handles `AppEvent::BrowserPageInfo`: reflects the page's identity onto
    /// the pane.
    ///
    /// The host goes in the manual label (a pane border has room for
    /// `example.com`, not a headline) and the full title goes in the terminal
    /// title, which is what the sidebar and `pane.list` already surface. Both
    /// are runtime facts about the pane, so they live in server state rather
    /// than only in the TUI.
    pub(crate) fn handle_browser_page_info(
        &mut self,
        pane_id: PaneId,
        url: Option<String>,
        title: Option<String>,
    ) {
        if !self.state.is_browser_pane(pane_id) {
            return;
        }
        match &url {
            Some(url) => {
                self.state.browser_pane_urls.insert(pane_id, url.clone());
            }
            None => {
                self.state.browser_pane_urls.remove(&pane_id);
            }
        }
        let label = url
            .as_deref()
            .and_then(browser_pane_label)
            .unwrap_or_else(|| "browser".to_string());
        let Some((ws_idx, _)) = self.find_pane(pane_id) else {
            return;
        };
        let Some(terminal_id) = self.state.terminal_id_for_pane(ws_idx, pane_id) else {
            return;
        };
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return;
        };
        terminal.set_manual_label(label);
        terminal.set_terminal_title(title);
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// Handles `AppEvent::BrowserCommandFailed`: a command the API already
    /// answered `ok` to turned out to fail. Surfaced as a toast because the
    /// request is long gone by the time the actor runs it.
    pub(crate) fn handle_browser_command_failed(
        &mut self,
        pane_id: PaneId,
        action: &str,
        reason: &str,
    ) {
        let previous_toast = self.state.toast.clone();
        self.state.toast = Some(crate::app::state::ToastNotification {
            kind: crate::app::state::ToastKind::NeedsAttention,
            title: format!("browser {action} failed"),
            context: reason.to_string(),
            position: None,
            target: self
                .find_pane(pane_id)
                .map(|(ws_idx, _)| crate::app::state::ToastTarget {
                    workspace_id: self.state.workspaces[ws_idx].id.clone(),
                    pane_id,
                }),
        });
        self.sync_toast_deadline(previous_toast);
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }

    /// Relaunches a failed Browser pane's session in place, back onto the page
    /// it was last on. No-op for a pane that is not in the failed state.
    pub(crate) fn retry_browser_pane(&mut self, pane_id: PaneId) -> bool {
        if self.state.browser_pane_errors.remove(&pane_id).is_none() {
            return false;
        }
        let url = self.state.browser_pane_urls.get(&pane_id).cloned();
        if let Err(err) = self.spawn_browser_actor(pane_id, url) {
            self.state.browser_pane_errors.insert(
                pane_id,
                format!("failed to start browser pane actor: {err}"),
            );
        }
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
        true
    }

    /// Starts a Browser pane's actor thread and registers its handle.
    ///
    /// The pane must already exist and already be a Browser pane; this is the
    /// shared half of first open and [`App::retry_browser_pane`].
    fn spawn_browser_actor(&mut self, pane_id: PaneId, url: Option<String>) -> std::io::Result<()> {
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        // Not started under `cfg(test)`: the actor's first act is
        // `daemon::open`, which launches a real headless Chrome. Unit tests
        // cover the bookkeeping around the thread, and the thread itself is
        // covered by the live smoke test.
        #[cfg(not(test))]
        {
            let session = crate::browser::daemon::session_name(pane_id);
            let events = self.event_tx.clone();
            std::thread::Builder::new()
                .name(format!("browser-{}", pane_id.raw()))
                .spawn(move || crate::browser::actor::run(pane_id, session, command_rx, events))?;
        }
        #[cfg(test)]
        // Held so the sender stays connected, exactly as a live actor would.
        self.test_browser_command_rx.insert(pane_id, command_rx);
        if let Some(url) = url {
            self.state.browser_pane_urls.insert(pane_id, url.clone());
            let _ = command_tx.send(crate::browser::BrowserCommand::Navigate(url));
        }
        // Forget the previous session's size so the next geometry sync pushes
        // the viewport to the freshly launched browser.
        self.browser_viewports.remove(&pane_id);
        self.browser_actors.insert(pane_id, command_tx);
        Ok(())
    }

    /// Stops a Browser pane's actor without changing what the pane *is*.
    /// Dropping the actor handle is the thread's shutdown signal (see
    /// `crate::browser::BrowserActorHandle`); the queued shutdown and input
    /// entries go too, because nothing would ever match them once the handle
    /// is gone.
    fn detach_browser_actor(&mut self, pane_id: PaneId) {
        self.browser_actors.remove(&pane_id);
        self.browser_viewports.remove(&pane_id);
        self.state
            .browser_pane_shutdowns
            .retain(|queued| *queued != pane_id);
        self.state
            .browser_input_events
            .retain(|(queued, _)| *queued != pane_id);
    }

    /// Drops every Browser-pane record for `pane_id` without touching the
    /// pane itself, leaving an ordinary (runtime-less) pane behind. Used when
    /// the pane is going away or is being unwound, not when its session
    /// merely died -- see [`App::handle_browser_daemon_exited`] for that.
    fn retire_browser_pane(&mut self, pane_id: PaneId) {
        self.detach_browser_actor(pane_id);
        self.state.demote_browser_pane(pane_id);
        self.state.browser_pane_urls.remove(&pane_id);
        self.state.browser_pane_errors.remove(&pane_id);
        self.state.clear_browser_frame(pane_id);
    }

    /// Stops a pane's `agent-browser` session.
    ///
    /// Skipped under `cfg(test)` for the same reason
    /// [`App::spawn_browser_actor`] starts no thread there: this shells out to
    /// the real CLI, and a unit-test suite has no business spawning it.
    fn stop_browser_session(&mut self, pane_id: PaneId) {
        #[cfg(not(test))]
        crate::browser::daemon::stop(&crate::browser::daemon::session_name(pane_id));
        #[cfg(test)]
        self.test_stopped_browser_sessions.push(pane_id);
    }

    /// Pushes each Browser pane's current pixel size to its session, so the
    /// page is laid out at the pane's aspect ratio instead of being stretched
    /// into it by the Kitty placement.
    ///
    /// Reads the geometry `compute_view` last produced; a tick of lag is
    /// irrelevant next to the cost of a resize round-trip, and skipping
    /// unchanged sizes keeps this free on every other tick.
    pub(crate) fn sync_browser_viewports(&mut self) {
        if self.browser_actors.is_empty() {
            return;
        }
        let cell_size = self.state.host_cell_size;
        if !cell_size.is_known() {
            return;
        }
        for info in &self.state.view.pane_infos {
            let Some(commands) = self.browser_actors.get(&info.id) else {
                continue;
            };
            let width = u32::from(info.inner_rect.width) * cell_size.width_px;
            let height = u32::from(info.inner_rect.height) * cell_size.height_px;
            if width == 0 || height == 0 {
                continue;
            }
            if self.browser_viewports.get(&info.id) == Some(&(width, height)) {
                continue;
            }
            if commands
                .send(crate::browser::BrowserCommand::SetViewport { width, height })
                .is_ok()
            {
                self.browser_viewports.insert(info.id, (width, height));
            }
        }
    }

    /// Stops every live Browser pane's `agent-browser` session. Called on
    /// server shutdown: the actor threads stop their own session when their
    /// sender drops, but process exit kills them before they get to run, so
    /// without this each herdr shutdown leaks a detached browser daemon (see
    /// `crate::browser::daemon`, which documents the `setsid()` detach).
    /// Stopping by derived session name means no extra runtime state is
    /// needed, and it stays correct for panes whose actor already exited.
    pub(crate) fn stop_all_browser_sessions(&mut self) {
        let pane_ids = self.state.browser_pane_ids();
        for pane_id in pane_ids {
            self.retire_browser_pane(pane_id);
            self.stop_browser_session(pane_id);
        }
    }

    /// Undoes a partially opened Browser pane after a failure in
    /// [`App::handle_browser_open`], so a failed open never leaves a pane in
    /// the workspace layout whose terminal/runtime records are missing.
    fn rollback_browser_pane(&mut self, ws_idx: usize, pane_id: PaneId) {
        self.retire_browser_pane(pane_id);
        let terminal_id = self.state.terminal_id_for_pane(ws_idx, pane_id);
        if let Some(ws) = self.state.workspaces.get_mut(ws_idx) {
            // Can't close the workspace: this pane was just split off an
            // existing sibling, so the tab always keeps at least one pane.
            ws.close_pane(pane_id);
        }
        self.state.remove_plugin_pane_records([pane_id]);
        self.state.remove_unattached_terminal_ids(terminal_id);
        self.shutdown_detached_terminal_runtimes();
    }

    pub(super) fn handle_browser_open(&mut self, id: String, params: BrowserOpenParams) -> String {
        // A Browser pane is drawn entirely through the pane graphics overlay,
        // which `src/server/headless.rs` only encodes when this is enabled.
        // Without the gate an open would silently launch a real browser and
        // poll it forever while the pane stayed blank.
        if let Err(response) = super::pane_graphics::require_pane_graphics_enabled(self, &id) {
            return response;
        }
        if self.state.browser_pane_ids().len() >= MAX_BROWSER_PANES {
            return encode_error(
                id,
                "browser_pane_limit",
                format!("at most {MAX_BROWSER_PANES} browser panes can be open at once"),
            );
        }
        // Validated before the pane is allocated, so a bad url is an error
        // instead of a blank pane that quietly failed to navigate.
        let url = match params.url.as_deref().map(crate::browser::url::normalize) {
            Some(Ok(url)) => Some(url),
            Some(Err(err)) => return encode_error(id, "invalid_url", err),
            None => None,
        };
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
        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            return encode_error(id, "workspace_not_found", "workspace not found");
        };
        let Some((tab_idx, new_pane)) =
            ws.split_pane_browser(target_pane, Direction::Horizontal, Some(0.5), None, true)
        else {
            return encode_error(
                id,
                "pane_not_found",
                format!("pane {target_pane_id} not found"),
            );
        };
        let pane_id = new_pane.pane_id;

        // Register the pane's terminal record before anything else can fail,
        // so every later failure path has a fully formed pane to hand to
        // `rollback_browser_pane` instead of leaving a pane in the layout
        // with no `AppState.terminals` entry. No runtime is inserted: a
        // Browser pane has no PTY child (see `Tab::split_focused_browser`).
        let mut terminal = new_pane.terminal;
        terminal.set_manual_label("browser".to_string());
        let terminal_id = terminal.id.clone();
        self.state.remove_alias_shadowed_by_new_pane(pane_id);
        self.state.terminals.insert(terminal_id, terminal);

        if let Err(err) = self.spawn_browser_actor(pane_id, url) {
            self.rollback_browser_pane(ws_idx, pane_id);
            return encode_error(
                id,
                "browser_pane_open_failed",
                format!("failed to start browser pane actor: {err}"),
            );
        }
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
            self.rollback_browser_pane(ws_idx, pane_id);
            return encode_error(id, "browser_pane_open_failed", "browser pane disappeared");
        };
        self.emit_event(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::PaneCreated,
            data: crate::api::schema::EventData::PaneCreated { pane: pane.clone() },
        });
        self.emit_layout_updated_event(ws_idx, tab_idx);
        encode_success(id, ResponseResult::PaneInfo { pane })
    }

    /// Drains `AppState.browser_input_events` (queued by
    /// `forward_pane_mouse_button`, which has no runtime access) and sends
    /// each to its pane's actor. Mirrors
    /// `App::dispatch_pending_clipboard_write`'s queue-then-dispatch shape.
    pub(crate) fn dispatch_browser_input_events(&mut self) {
        for (pane_id, command) in std::mem::take(&mut self.state.browser_input_events) {
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
        let url = match crate::browser::url::normalize(&params.url) {
            Ok(url) => url,
            Err(err) => return encode_error(id, "invalid_url", err),
        };
        let pane_id = match self.browser_pane_for_request(&params.pane_id) {
            Ok(pane_id) => pane_id,
            Err(response) => return encode_error(id, response.0, response.1),
        };
        if let Err(response) = self.send_browser_command(
            pane_id,
            crate::browser::BrowserCommand::Navigate(url.clone()),
        ) {
            return encode_error(id, response.0, response.1);
        }
        // Remembered so a later relaunch lands back on this page rather than
        // on `about:blank` (see `App::retry_browser_pane`).
        self.state.browser_pane_urls.insert(pane_id, url);
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_browser_reload(
        &mut self,
        id: String,
        params: crate::api::schema::BrowserPaneTarget,
    ) -> String {
        self.handle_browser_page_action(id, &params.pane_id, crate::browser::BrowserCommand::Reload)
    }

    pub(super) fn handle_browser_back(
        &mut self,
        id: String,
        params: crate::api::schema::BrowserPaneTarget,
    ) -> String {
        self.handle_browser_page_action(id, &params.pane_id, crate::browser::BrowserCommand::Back)
    }

    pub(super) fn handle_browser_forward(
        &mut self,
        id: String,
        params: crate::api::schema::BrowserPaneTarget,
    ) -> String {
        self.handle_browser_page_action(
            id,
            &params.pane_id,
            crate::browser::BrowserCommand::Forward,
        )
    }

    fn handle_browser_page_action(
        &mut self,
        id: String,
        public_pane_id: &str,
        command: crate::browser::BrowserCommand,
    ) -> String {
        let pane_id = match self.browser_pane_for_request(public_pane_id) {
            Ok(pane_id) => pane_id,
            Err(response) => return encode_error(id, response.0, response.1),
        };
        match self.send_browser_command(pane_id, command) {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(response) => encode_error(id, response.0, response.1),
        }
    }

    /// Reports what a Browser pane is showing. Answers from state the actor
    /// last reported rather than querying the session, so it stays a cheap
    /// non-blocking read; the values lag a navigation by up to one poll.
    pub(super) fn handle_browser_info(
        &mut self,
        id: String,
        params: crate::api::schema::BrowserPaneTarget,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return encode_error(
                id,
                "pane_not_found",
                format!("pane {} not found", params.pane_id),
            );
        };
        if !self.state.is_browser_pane(pane_id) {
            return encode_error(id, "not_browser_pane", "pane is not a Browser pane");
        }
        let title = self
            .state
            .terminal_id_for_pane(ws_idx, pane_id)
            .and_then(|terminal_id| self.state.terminals.get(&terminal_id))
            .and_then(|terminal| terminal.terminal_title.clone());
        encode_success(
            id,
            ResponseResult::BrowserPageInfo {
                page: crate::api::schema::BrowserPageInfo {
                    pane_id: params.pane_id,
                    url: self.state.browser_pane_urls.get(&pane_id).cloned(),
                    title,
                    error: self.state.browser_pane_errors.get(&pane_id).cloned(),
                },
            },
        )
    }

    /// Closes a Browser pane: stops its session and removes the pane, the
    /// same outcome as `pane.close` on an ordinary pane.
    pub(super) fn handle_browser_close(
        &mut self,
        id: String,
        params: crate::api::schema::BrowserPaneTarget,
    ) -> String {
        let pane_id = match self.browser_pane_for_request(&params.pane_id) {
            Ok(pane_id) => pane_id,
            Err(response) => return encode_error(id, response.0, response.1),
        };
        // Stop the browser first: once the pane leaves the layout its kind is
        // no longer readable, and `pane.close` has no idea a detached
        // agent-browser daemon is attached to it.
        self.retire_browser_pane(pane_id);
        self.stop_browser_session(pane_id);
        // Reuse the ordinary pane teardown so workspace collapse, events and
        // session save behave exactly as they do for any other pane.
        let target = crate::api::schema::PaneTarget {
            pane_id: params.pane_id,
        };
        match self.close_pane(id.clone(), &target) {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(response) => response,
        }
    }

    /// Resolves a public pane id to a live Browser pane, or the error code and
    /// message to answer with.
    fn browser_pane_for_request(
        &self,
        public_pane_id: &str,
    ) -> Result<PaneId, (&'static str, String)> {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(public_pane_id) else {
            return Err(("pane_not_found", format!("pane {public_pane_id} not found")));
        };
        if !self.state.is_browser_pane(pane_id) {
            return Err(("not_browser_pane", "pane is not a Browser pane".to_string()));
        }
        Ok(pane_id)
    }

    fn send_browser_command(
        &self,
        pane_id: PaneId,
        command: crate::browser::BrowserCommand,
    ) -> Result<(), (&'static str, String)> {
        let Some(commands) = self.browser_actors.get(&pane_id) else {
            // The pane is still a Browser pane, so this is the failed state
            // rather than a wrong pane id.
            return Err((
                "browser_pane_gone",
                self.state
                    .browser_pane_errors
                    .get(&pane_id)
                    .cloned()
                    .unwrap_or_else(|| "browser pane actor is no longer running".to_string()),
            ));
        };
        commands.send(command).map_err(|_| {
            (
                "browser_pane_gone",
                "browser pane actor is no longer running".to_string(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::BrowserCommand;
    use crate::workspace::Workspace;

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    /// Adds a second pane to a fresh workspace and registers it as a Browser
    /// pane, returning the pane and the receiver that keeps its actor channel
    /// alive so `browser_actors` holds a live sender.
    fn app_with_browser_pane() -> (App, PaneId, std::sync::mpsc::Receiver<BrowserCommand>) {
        let mut app = test_app();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws
            .split_pane_browser(
                ws.tabs[0].root_pane,
                Direction::Horizontal,
                Some(0.5),
                None,
                true,
            )
            .expect("split browser pane")
            .1
            .pane_id;
        let terminal_ids: Vec<_> = ws.tabs[0]
            .panes
            .values()
            .map(|pane| pane.attached_terminal_id.clone())
            .collect();
        app.state.workspaces.push(ws);
        app.state.active = Some(0);
        for terminal_id in terminal_ids {
            app.state.terminals.insert(
                terminal_id.clone(),
                crate::terminal::TerminalState::new(terminal_id, "/tmp".into()),
            );
        }

        let (command_tx, command_rx) = std::sync::mpsc::channel();
        app.browser_actors.insert(pane_id, command_tx);
        app.state
            .pane_graphics_streams
            .insert(pane_id, BROWSER_STREAM_OWNER.to_string());
        app.state.pane_graphics_layers.insert(
            pane_id,
            crate::app::state::PaneGraphicsLayer::new(
                crate::api::schema::PaneGraphicsFormat::Png,
                4,
                4,
                vec![0u8; 8],
                crate::api::schema::PaneGraphicsPlacementParams {
                    viewport_col: 0,
                    viewport_row: 0,
                    grid_cols: 0,
                    grid_rows: 0,
                },
            ),
        );
        (app, pane_id, command_rx)
    }

    fn public_pane_id_of(app: &App, pane_id: PaneId) -> String {
        app.public_pane_id(0, pane_id).expect("public pane id")
    }

    fn error_code(response: &str) -> String {
        let error: crate::api::schema::ErrorResponse =
            serde_json::from_str(response).expect("error response");
        error.error.code
    }

    #[test]
    fn page_labels_use_the_host_and_the_title() {
        assert_eq!(
            browser_pane_label("https://www.example.com/a/b?c=d#e").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            browser_pane_label("http://user:pw@example.org:8443/x").as_deref(),
            Some("example.org")
        );
        // No host to show; the caller keeps the generic label.
        assert_eq!(browser_pane_label("about:blank"), None);
        assert_eq!(browser_pane_label("file:///tmp/x.html"), None);
    }

    #[test]
    fn page_info_replaces_the_placeholder_label_and_title() {
        let (mut app, pane_id, _command_rx) = app_with_browser_pane();

        app.handle_browser_page_info(
            pane_id,
            Some("https://www.rust-lang.org/learn".to_string()),
            Some("Learn Rust".to_string()),
        );

        let terminal_id = app
            .state
            .terminal_id_for_pane(0, pane_id)
            .expect("browser pane terminal");
        let terminal = app.state.terminals.get(&terminal_id).expect("terminal");
        // Was the literal "browser" forever before this.
        assert_eq!(terminal.manual_label.as_deref(), Some("rust-lang.org"));
        assert_eq!(terminal.terminal_title.as_deref(), Some("Learn Rust"));
        assert_eq!(
            app.state
                .browser_pane_urls
                .get(&pane_id)
                .map(String::as_str),
            Some("https://www.rust-lang.org/learn")
        );
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn page_info_is_ignored_for_panes_that_are_not_browser_panes() {
        let (mut app, pane_id, _command_rx) = app_with_browser_pane();
        app.retire_browser_pane(pane_id);

        app.handle_browser_page_info(pane_id, Some("https://example.com".to_string()), None);

        assert!(!app.state.browser_pane_urls.contains_key(&pane_id));
    }

    #[test]
    fn a_failed_command_becomes_a_toast() {
        let (mut app, pane_id, _command_rx) = app_with_browser_pane();
        assert!(app.state.toast.is_none());

        app.handle_browser_command_failed(pane_id, "navigate", "net::ERR_NAME_NOT_RESOLVED");

        // The API answered `ok` long before the actor ran this, so a silent
        // failure is indistinguishable from success.
        let toast = app.state.toast.as_ref().expect("toast");
        assert_eq!(toast.title, "browser navigate failed");
        assert_eq!(toast.context, "net::ERR_NAME_NOT_RESOLVED");
        assert_eq!(toast.target.as_ref().map(|t| t.pane_id), Some(pane_id));
    }

    #[test]
    fn page_actions_reach_the_actor() {
        let (mut app, pane_id, command_rx) = app_with_browser_pane();
        let public = public_pane_id_of(&app, pane_id);
        let target = crate::api::schema::BrowserPaneTarget {
            pane_id: public.clone(),
        };

        app.handle_browser_reload("1".into(), target.clone());
        app.handle_browser_back("2".into(), target.clone());
        app.handle_browser_forward("3".into(), target);

        assert_eq!(command_rx.try_recv().unwrap(), BrowserCommand::Reload);
        assert_eq!(command_rx.try_recv().unwrap(), BrowserCommand::Back);
        assert_eq!(command_rx.try_recv().unwrap(), BrowserCommand::Forward);
    }

    #[test]
    fn page_actions_reject_panes_that_are_not_browser_panes() {
        let (mut app, pane_id, _command_rx) = app_with_browser_pane();
        let shell_pane = app.state.workspaces[0].tabs[0].root_pane;
        let public = public_pane_id_of(&app, shell_pane);
        assert_ne!(shell_pane, pane_id);

        let response = app.handle_browser_reload(
            "1".into(),
            crate::api::schema::BrowserPaneTarget { pane_id: public },
        );

        assert_eq!(error_code(&response), "not_browser_pane");
    }

    #[test]
    fn navigate_normalizes_and_rejects_urls() {
        let (mut app, pane_id, command_rx) = app_with_browser_pane();
        let public = public_pane_id_of(&app, pane_id);

        let response = app.handle_browser_navigate(
            "1".into(),
            crate::api::schema::BrowserNavigateParams {
                pane_id: public.clone(),
                url: "example.com".to_string(),
            },
        );
        assert!(!response.contains("\"error\""), "{response}");
        assert_eq!(
            command_rx.try_recv().unwrap(),
            BrowserCommand::Navigate("https://example.com".to_string())
        );
        assert_eq!(
            app.state
                .browser_pane_urls
                .get(&pane_id)
                .map(String::as_str),
            Some("https://example.com")
        );

        // Would otherwise be handed to the CLI as a flag.
        let response = app.handle_browser_navigate(
            "2".into(),
            crate::api::schema::BrowserNavigateParams {
                pane_id: public,
                url: "--version".to_string(),
            },
        );
        assert_eq!(error_code(&response), "invalid_url");
        assert!(command_rx.try_recv().is_err());
    }

    #[test]
    fn open_rejects_a_bad_url_before_allocating_a_pane() {
        let mut app = test_app();
        app.state.kitty_graphics_enabled = true;
        app.state.workspaces.push(Workspace::test_new("test"));
        app.state.active = Some(0);
        let panes_before = app.state.workspaces[0].tabs[0].panes.len();

        let response = app.handle_browser_open(
            "1".to_string(),
            BrowserOpenParams {
                pane_id: None,
                url: Some("javascript:alert(1)".to_string()),
            },
        );

        assert_eq!(error_code(&response), "invalid_url");
        // A pane that failed to navigate would just be a blank rectangle.
        assert_eq!(app.state.workspaces[0].tabs[0].panes.len(), panes_before);
        assert!(app.state.browser_pane_ids().is_empty());
    }

    #[test]
    fn info_reports_url_title_and_failure() {
        let (mut app, pane_id, _command_rx) = app_with_browser_pane();
        let public = public_pane_id_of(&app, pane_id);
        app.handle_browser_page_info(
            pane_id,
            Some("https://example.com/x".to_string()),
            Some("Example".to_string()),
        );

        let response = app.handle_browser_info(
            "1".into(),
            crate::api::schema::BrowserPaneTarget {
                pane_id: public.clone(),
            },
        );
        let value: serde_json::Value = serde_json::from_str(&response).expect("response");
        let page = &value["result"]["page"];
        assert_eq!(page["url"], "https://example.com/x");
        assert_eq!(page["title"], "Example");
        assert!(page.get("error").is_none(), "{page}");

        app.handle_browser_daemon_exited(pane_id, "session died".to_string());
        let response = app.handle_browser_info(
            "2".into(),
            crate::api::schema::BrowserPaneTarget { pane_id: public },
        );
        let value: serde_json::Value = serde_json::from_str(&response).expect("response");
        assert_eq!(value["result"]["page"]["error"], "session died");
    }

    #[test]
    fn close_stops_the_session_and_removes_the_pane() {
        let (mut app, pane_id, command_rx) = app_with_browser_pane();
        let public = public_pane_id_of(&app, pane_id);
        let terminal_id = app
            .state
            .terminal_id_for_pane(0, pane_id)
            .expect("browser pane terminal");

        let response = app.handle_browser_close(
            "1".into(),
            crate::api::schema::BrowserPaneTarget { pane_id: public },
        );

        assert!(!response.contains("\"error\""), "{response}");
        // The daemon is detached, so closing the pane alone would leak it.
        assert_eq!(app.test_stopped_browser_sessions, vec![pane_id]);
        assert!(app.find_pane(pane_id).is_none());
        assert!(!app.state.terminals.contains_key(&terminal_id));
        assert!(!app.state.is_browser_pane(pane_id));
        assert!(matches!(
            command_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn commands_for_a_failed_pane_report_why() {
        let (mut app, pane_id, _command_rx) = app_with_browser_pane();
        let public = public_pane_id_of(&app, pane_id);
        app.handle_browser_daemon_exited(pane_id, "chrome crashed".to_string());

        let response = app.handle_browser_reload(
            "1".into(),
            crate::api::schema::BrowserPaneTarget { pane_id: public },
        );

        let value: serde_json::Value = serde_json::from_str(&response).expect("response");
        assert_eq!(value["error"]["code"], "browser_pane_gone");
        // The recorded failure is more use than "actor is no longer running".
        assert_eq!(value["error"]["message"], "chrome crashed");
    }

    #[test]
    fn browser_open_requires_kitty_graphics() {
        let mut app = test_app();
        assert!(!app.state.kitty_graphics_enabled);

        let response = app.handle_browser_open(
            "1".to_string(),
            BrowserOpenParams {
                pane_id: None,
                url: None,
            },
        );

        // Without this the open would launch a real browser and poll it
        // forever behind a pane that can never draw its frames.
        let error: crate::api::schema::ErrorResponse =
            serde_json::from_str(&response).expect("error response");
        assert_eq!(error.error.code, "feature_disabled");
        assert!(app.state.browser_pane_ids().is_empty());
        assert!(app.browser_actors.is_empty());
    }

    #[test]
    fn browser_daemon_exit_leaves_a_retryable_pane_in_place() {
        let (mut app, pane_id, command_rx) = app_with_browser_pane();
        app.state
            .browser_input_events
            .push((pane_id, BrowserCommand::MouseDown { x: 1, y: 2 }));
        app.browser_viewports.insert(pane_id, (800, 600));

        app.handle_browser_daemon_exited(pane_id, "session died".to_string());

        // The pane keeps its slot and its identity so the same page can come
        // back in the same place; only the live session is gone.
        assert!(app.state.is_browser_pane(pane_id));
        assert!(app.find_pane(pane_id).is_some());
        assert_eq!(
            app.state
                .browser_pane_errors
                .get(&pane_id)
                .map(String::as_str),
            Some("session died")
        );
        assert!(app.state.pane_graphics_streams.contains_key(&pane_id));

        assert!(!app.browser_actors.contains_key(&pane_id));
        assert!(!app.browser_viewports.contains_key(&pane_id));
        assert!(app.state.browser_input_events.is_empty());
        // The stale picture goes, so the pane shows the failure rather than a
        // frozen page.
        assert!(!app.state.pane_graphics_layers.contains_key(&pane_id));
        // Dropping the sender is the actor's shutdown signal.
        assert!(matches!(
            command_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn retrying_a_failed_pane_relaunches_it_on_the_last_url() {
        let (mut app, pane_id, _command_rx) = app_with_browser_pane();
        app.state
            .browser_pane_urls
            .insert(pane_id, "https://example.com".to_string());
        app.handle_browser_daemon_exited(pane_id, "session died".to_string());

        assert!(app.retry_browser_pane(pane_id));

        assert!(!app.state.browser_pane_errors.contains_key(&pane_id));
        assert!(app.browser_actors.contains_key(&pane_id));
        // The relaunched session is sent back to the page the user was on,
        // not left on about:blank.
        let relaunched = app
            .test_browser_command_rx
            .get(&pane_id)
            .expect("retry installs a fresh channel");
        assert_eq!(
            relaunched.try_recv().expect("navigate command"),
            BrowserCommand::Navigate("https://example.com".to_string())
        );
        app.state.assert_invariants_for_test();
    }

    #[test]
    fn retrying_a_healthy_pane_does_nothing() {
        let (mut app, pane_id, command_rx) = app_with_browser_pane();
        assert!(!app.retry_browser_pane(pane_id));
        // The original actor handle is untouched -- a stray retry must not
        // tear down a working session.
        assert!(app.browser_actors.contains_key(&pane_id));
        assert!(matches!(
            command_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn viewport_is_pushed_once_per_geometry_change() {
        let (mut app, pane_id, command_rx) = app_with_browser_pane();
        app.state.host_cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 42, 12),
            inner_rect: ratatui::layout::Rect::new(0, 0, 40, 10),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::NONE,
            is_focused: true,
        }];

        app.sync_browser_viewports();
        assert_eq!(
            command_rx.try_recv().expect("viewport command"),
            BrowserCommand::SetViewport {
                width: 400,
                height: 200
            }
        );

        // Unchanged geometry must not spend another subprocess call.
        app.sync_browser_viewports();
        assert!(matches!(
            command_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        app.state.view.pane_infos[0].inner_rect.width = 20;
        app.sync_browser_viewports();
        assert_eq!(
            command_rx
                .try_recv()
                .expect("viewport command after resize"),
            BrowserCommand::SetViewport {
                width: 200,
                height: 200
            }
        );
    }

    #[test]
    fn browser_open_is_capped() {
        let mut app = test_app();
        app.state.kitty_graphics_enabled = true;
        let mut ws = Workspace::test_new("test");
        for _ in 0..MAX_BROWSER_PANES {
            ws.split_pane_browser(
                ws.tabs[0].root_pane,
                Direction::Horizontal,
                Some(0.5),
                None,
                true,
            )
            .expect("split browser pane");
        }
        app.state.workspaces.push(ws);
        app.state.active = Some(0);

        let response = app.handle_browser_open(
            "1".to_string(),
            BrowserOpenParams {
                pane_id: None,
                url: None,
            },
        );

        // Each Browser pane is a full Chrome; an uncapped open loop takes the
        // machine down rather than doing anything useful.
        let error: crate::api::schema::ErrorResponse =
            serde_json::from_str(&response).expect("error response");
        assert_eq!(error.error.code, "browser_pane_limit");
        assert_eq!(app.state.browser_pane_ids().len(), MAX_BROWSER_PANES);
    }

    #[test]
    fn browser_pane_rollback_removes_the_pane_and_its_terminal() {
        let (mut app, pane_id, _command_rx) = app_with_browser_pane();
        let terminal_id = app
            .state
            .terminal_id_for_pane(0, pane_id)
            .expect("browser pane terminal");

        app.rollback_browser_pane(0, pane_id);

        assert!(app.find_pane(pane_id).is_none());
        assert!(!app.state.terminals.contains_key(&terminal_id));
        assert!(!app.state.is_browser_pane(pane_id));
        assert!(!app.browser_actors.contains_key(&pane_id));
        assert!(!app.state.pane_graphics_streams.contains_key(&pane_id));
        assert!(app.state.browser_pane_shutdowns.is_empty());
        app.state.assert_invariants_for_test();
    }
}
