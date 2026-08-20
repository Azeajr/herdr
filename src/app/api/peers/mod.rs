//! `peer.*` API handlers, and everything reached from them.
//!
//! Split along the dependency direction rather than by size, so adding an
//! operation means reading one file with a visible pattern to copy instead of
//! six unrelated concerns sharing one `impl App` block:
//!
//! - [`resolve`] — which peer owns an id, and what it knows the id by. Peer
//!   state only.
//! - [`rewrite`] — restating a peer's answers in local ids. Pure JSON.
//! - [`forward`] — sending work to a peer off the event loop.
//! - [`views`] — keeping peer-backed views connected.
//! - [`lifecycle`] — placing views, and disposing of them.
//! - [`worktrees`] — running `worktree.*` on the machine the checkout is on.
//!
//! This module keeps only the peer registry's own handlers: adding, removing and
//! listing peers mutate the desired peer set and never touch a socket. The
//! headless server reconciles the running connections against it.

mod forward;
mod lifecycle;
mod resolve;
mod rewrite;
mod views;
mod worktrees;

// Re-exported so each module's `use super::*` reaches its siblings' helpers,
// which is what keeps the split from turning every cross-module call into an
// import list that has to be maintained by hand.
use forward::LocalPaneIds;
use lifecycle::close_abandoned_peer_pane;
use resolve::client_socket_for;
use rewrite::{
    agent_state_from_status, peer_pane_id_at, peer_split_pane_id, peer_workspace_of_pane_id,
    rewrite_forwarded_explain, rewrite_forwarded_read, rewrite_forwarded_response,
};

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::api::schema::{
    Method, PaneSplitParams, PeerAddParams, PeerInfo, PeerRef, PeerTargetSpec, PeerTerminalInfo,
    PeerTerminalOpenParams, PeerWorkspaceOpenParams, Request, ResponseResult,
};
use crate::app::api::agents::agent_not_found;
use crate::app::api::responses::{encode_error, encode_error_body, encode_success};
use crate::app::peers::{AddPeerError, PeerConnectionState, PeerHandle, PeerState, PeerTarget};
use crate::app::App;

/// Bound on a request forwarded to a peer, matching the peer control channel's
/// own request timeout. A dead peer must not pin the forwarding thread.
const PEER_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A view onto a peer that this server already holds.
///
/// The distinction matters because only one of the two can be handed back: a
/// view a workspace holds is what a workspace open wanted, while a bare terminal
/// is a view onto the same peer terminal that a workspace cannot be built around
/// without opening a second connection to it.
enum ExistingPeerView {
    InWorkspace {
        ws_idx: usize,
        terminal_id: crate::terminal::TerminalId,
    },
    Bare {
        terminal_id: crate::terminal::TerminalId,
    },
}

impl ExistingPeerView {
    fn terminal_id(&self) -> &crate::terminal::TerminalId {
        match self {
            Self::InWorkspace { terminal_id, .. } | Self::Bare { terminal_id } => terminal_id,
        }
    }
}

/// A resolved, connected peer and everything needed to talk to it.
struct PeerConnection {
    /// The peer-local id to send over the wire, namespace stripped.
    local_target: String,
    /// The peer's JSON API socket on this machine.
    api_socket: PathBuf,
    /// The peer's server instance id, used to re-namespace ids coming back.
    instance_id: String,
}

impl App {
    pub(in crate::app) fn handle_peer_add(&mut self, id: String, params: PeerAddParams) -> String {
        let name = params.name.trim().to_string();
        if name.is_empty() {
            return encode_error(id, "invalid_request", "peer name must not be empty");
        }

        let target = peer_target_from_spec(params.target);
        let handle = PeerHandle::new(name.clone());
        match self.state.peers.add(handle.clone(), target.clone()) {
            Ok(()) => {
                // Every successful add — CLI `peer add`, CLI `peer connect`,
                // or the TUI dialog's connect pane — lands here, so this is
                // the one place history can record all of them. A history
                // write failure must not fail the add itself.
                match crate::config::record_peer_history(&name, &peer_history_target(&target)) {
                    Ok(entries) => self.state.peer_history = entries,
                    Err(err) => {
                        tracing::warn!(error = %err, "could not record peer history")
                    }
                }
                self.schedule_session_save();
                encode_success(
                    id,
                    ResponseResult::PeerList {
                        peers: self.peer_list_info(),
                    },
                )
            }
            Err(err @ AddPeerError::DuplicateHandle) => {
                encode_error(id, "already_exists", err.to_string())
            }
            Err(err @ AddPeerError::DuplicateTarget { .. }) => {
                encode_error(id, "already_exists", err.to_string())
            }
        }
    }

    pub(in crate::app) fn handle_peer_remove(&mut self, id: String, target: PeerRef) -> String {
        let handle = PeerHandle::new(target.name.trim().to_string());
        if self.state.peers.remove(&handle).is_none() {
            return encode_error(id, "not_found", format!("no peer named '{handle}'"));
        }
        // Panes we had this peer spawn stay running there. Dropping the claim
        // first keeps the teardown below from forwarding a close to a peer that
        // is already gone; the peer records them as owned by an instance that is
        // no longer attached.
        for runtime in self.terminal_runtimes.values_mut() {
            if runtime
                .spawned_peer_pane()
                .is_some_and(|spawned| spawned.peer == handle.as_str())
            {
                runtime.disown_spawned_peer_pane();
            }
        }
        let peer_terminal_ids = self
            .terminal_runtimes
            .iter()
            .filter(|(_, runtime)| {
                runtime
                    .remote()
                    .is_some_and(|remote| remote.peer() == handle.as_str())
            })
            .map(|(terminal_id, _)| terminal_id.clone())
            .collect::<Vec<_>>();
        self.state.remove_unattached_terminal_ids(peer_terminal_ids);
        self.shutdown_detached_terminal_runtimes();
        self.close_views_backed_by_peer(&handle);
        self.schedule_session_save();
        encode_success(
            id,
            ResponseResult::PeerList {
                peers: self.peer_list_info(),
            },
        )
    }

    pub(in crate::app) fn handle_peer_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::PeerList {
                peers: self.peer_list_info(),
            },
        )
    }

    pub(super) fn peer_list_info(&self) -> Vec<PeerInfo> {
        self.state.peers.iter().map(peer_info).collect()
    }
}

fn peer_target_from_spec(spec: PeerTargetSpec) -> PeerTarget {
    match spec {
        PeerTargetSpec::SocketPath { path } => PeerTarget::SocketPath(path.into()),
        PeerTargetSpec::Ssh {
            destination,
            session,
        } => PeerTarget::Ssh {
            destination,
            session,
        },
    }
}

fn peer_target_spec(target: &PeerTarget) -> PeerTargetSpec {
    match target {
        PeerTarget::SocketPath(path) => PeerTargetSpec::SocketPath {
            path: path.display().to_string(),
        },
        PeerTarget::Ssh {
            destination,
            session,
        } => PeerTargetSpec::Ssh {
            destination: destination.clone(),
            session: session.clone(),
        },
    }
}

/// Canonical history form of a peer target. History dedupes on this string,
/// so it must identify the destination, not the chosen name: the same ssh
/// host added under two names is one history entry. The add-peer dialog only
/// re-offers `ssh://` entries; socket peers are same-host test infrastructure.
pub(crate) fn peer_history_target(target: &PeerTarget) -> String {
    match target {
        PeerTarget::SocketPath(path) => format!("socket://{}", path.display()),
        PeerTarget::Ssh {
            destination,
            session: None,
        } => format!("ssh://{destination}"),
        PeerTarget::Ssh {
            destination,
            session: Some(session),
        } => format!("ssh://{destination}#{session}"),
    }
}

fn peer_info(peer: &PeerState) -> PeerInfo {
    let (attempt, error) = match &peer.connection {
        PeerConnectionState::Reconnecting { attempt, message } => {
            (Some(*attempt), Some(message.clone()))
        }
        PeerConnectionState::Error { message } => (None, Some(message.clone())),
        _ => (None, None),
    };

    PeerInfo {
        name: peer.handle.as_str().to_string(),
        label: peer.label.clone(),
        target: peer_target_spec(&peer.target),
        connection: peer.connection.kind(),
        attempt,
        error,
        instance_id: peer.instance_id().map(str::to_string),
        version: peer
            .identity
            .as_ref()
            .and_then(|identity| identity.version.clone()),
        protocol: peer
            .identity
            .as_ref()
            .and_then(|identity| identity.protocol),
        stale: peer.is_stale(),
        failed_pane_cleanups: peer.failed_pane_cleanups(),
        workspaces: peer.workspaces.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EmptyParams, WorkspaceRenameParams, WorkspaceTarget};

    const INSTANCE: &str = "0123456789abcdef0123456789abcdef";

    fn peer_ws_id(local: &str) -> String {
        crate::app::peers::prefix_peer_id(INSTANCE, local)
    }

    fn request(method: Method) -> Request {
        Request {
            id: "req".into(),
            method,
        }
    }

    fn split_request(workspace_id: Option<String>) -> Request {
        request(Method::PaneSplit(PaneSplitParams {
            workspace_id,
            target_pane_id: None,
            direction: crate::api::schema::SplitDirection::Right,
            right_click: Default::default(),
            ratio: None,
            cwd: None,
            focus: true,
            env: Default::default(),
            owner_instance_id: None,
        }))
    }

    fn read_request(pane_id: &str) -> Request {
        request(Method::PaneRead(crate::api::schema::PaneReadParams {
            pane_id: pane_id.into(),
            source: crate::api::schema::ReadSource::Visible,
            lines: None,
            format: Default::default(),
            strip_ansi: true,
            // Not serialized, so the peer defaults it. That is the right answer:
            // the peer is the side holding the screen, so its own read is the
            // interactive one.
            intent: Default::default(),
        }))
    }

    fn text_query_request(pane_id: &str) -> Request {
        request(Method::PaneTextQuery(
            crate::api::schema::PaneTextQueryParams {
                pane_id: pane_id.into(),
                query: crate::api::schema::PaneTextQuery::Search {
                    query: "needle".into(),
                    case_sensitive: false,
                },
            },
        ))
    }

    fn agent_read_request(target: &str) -> Request {
        request(Method::AgentRead(crate::api::schema::AgentReadParams {
            target: target.into(),
            source: crate::api::schema::ReadSource::Visible,
            lines: None,
            format: Default::default(),
            strip_ansi: true,
        }))
    }

    fn agent_explain_request(target: &str) -> Request {
        request(Method::AgentExplain(crate::api::schema::AgentTarget {
            target: target.into(),
        }))
    }

    /// A fake peer control socket that keeps what the client sent it.
    ///
    /// Scroll leaves no local trace — there is no VT state here to move — so the
    /// only evidence it reached the peer is the message on the wire.
    fn fake_peer_control_endpoint_recording(
        path: &std::path::Path,
    ) -> (
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::Mutex<Vec<crate::protocol::ClientMessage>>>,
    ) {
        use crate::protocol::{ClientMessage, RenderEncoding, ServerMessage, MAX_FRAME_SIZE};
        use interprocess::local_socket::traits::Listener as _;

        let listener = crate::ipc::bind_local_listener(path).expect("bind fake peer socket");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&seen);
        let handle = std::thread::spawn(move || {
            let Ok(stream) = listener.accept() else {
                return;
            };
            let Ok(read_half) = interprocess::TryClone::try_clone(&stream) else {
                return;
            };
            let mut reader = std::io::BufReader::new(read_half);
            let mut writer = std::io::BufWriter::new(stream);
            if crate::protocol::read_message::<_, ClientMessage>(&mut reader, MAX_FRAME_SIZE)
                .is_err()
            {
                return;
            }
            let _ = crate::protocol::write_message(
                &mut writer,
                &ServerMessage::Welcome {
                    version: crate::protocol::PROTOCOL_VERSION,
                    encoding: RenderEncoding::SemanticFrame,
                    error: None,
                    instance_id: Some(INSTANCE.to_string()),
                },
            );
            // This peer stands in for one whose program asked for the mouse.
            // Without the report the view declines clicks so its own selection
            // can have them, which is the right default but not what the mouse
            // forwarding tests are about.
            let _ = crate::protocol::write_message(
                &mut writer,
                &ServerMessage::TerminalInputModes {
                    bracketed_paste: false,
                    mouse_reporting: true,
                },
            );
            let _ = std::io::Write::flush(&mut writer);
            loop {
                match crate::protocol::read_message::<_, ClientMessage>(&mut reader, MAX_FRAME_SIZE)
                {
                    Ok(ClientMessage::Detach) | Err(_) => break,
                    Ok(message) => {
                        if let Ok(mut seen) = recorder.lock() {
                            seen.push(message);
                        }
                    }
                }
            }
        });
        (handle, seen)
    }

    fn unique_socket_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("herdr-{name}-{}-{nanos}.sock", std::process::id()))
    }

    /// Stands in for a peer's terminal control socket: completes the handshake,
    /// then holds the connection open, which is all a remote runtime needs in
    /// order to exist.
    fn fake_peer_control_endpoint(path: &std::path::Path) -> std::thread::JoinHandle<()> {
        fake_peer_control_endpoint_serving(path, 1, 0)
    }

    fn fake_peer_control_endpoint_with_frame(
        path: &std::path::Path,
    ) -> std::thread::JoinHandle<()> {
        fake_peer_control_endpoint_serving_frames(path, 1, 0, true)
    }

    /// Stands in for a peer's terminal control socket across several
    /// connections.
    ///
    /// The first `hang_up` connections are closed as soon as the client takes
    /// control — what a peer restart or a dead ssh bridge looks like from this
    /// side. The rest are held until the client detaches: dropping a remote
    /// runtime joins its reader thread, and that thread only ends when the peer
    /// closes the connection, so an endpoint that never hangs up wedges the drop
    /// forever. `connections` is bounded for the same reason — a listener
    /// looping on accept outlives the test.
    fn fake_peer_control_endpoint_serving(
        path: &std::path::Path,
        connections: usize,
        hang_up: usize,
    ) -> std::thread::JoinHandle<()> {
        fake_peer_control_endpoint_serving_frames(path, connections, hang_up, false)
    }

    fn fake_peer_control_endpoint_serving_frames(
        path: &std::path::Path,
        connections: usize,
        hang_up: usize,
        send_frame: bool,
    ) -> std::thread::JoinHandle<()> {
        use crate::protocol::{ClientMessage, RenderEncoding, ServerMessage, MAX_FRAME_SIZE};
        use interprocess::local_socket::traits::Listener as _;

        let listener = crate::ipc::bind_local_listener(path).expect("bind fake peer socket");
        std::thread::spawn(move || {
            let mut handlers = Vec::new();
            for connection in 0..connections {
                let Ok(stream) = listener.accept() else {
                    return;
                };
                handlers.push(std::thread::spawn(move || {
                    let Ok(read_half) = interprocess::TryClone::try_clone(&stream) else {
                        return;
                    };
                    let mut reader = std::io::BufReader::new(read_half);
                    let mut writer = std::io::BufWriter::new(stream);
                    if crate::protocol::read_message::<_, ClientMessage>(
                        &mut reader,
                        MAX_FRAME_SIZE,
                    )
                    .is_err()
                    {
                        return;
                    }
                    let _ = crate::protocol::write_message(
                        &mut writer,
                        &ServerMessage::Welcome {
                            version: crate::protocol::PROTOCOL_VERSION,
                            encoding: RenderEncoding::SemanticFrame,
                            error: None,
                            instance_id: Some(INSTANCE.to_string()),
                        },
                    );
                    if send_frame {
                        let _ = crate::protocol::write_message(
                            &mut writer,
                            &ServerMessage::Frame(crate::protocol::FrameData {
                                cells: Vec::new(),
                                width: 80,
                                height: 24,
                                cursor: Some(crate::protocol::CursorState {
                                    x: 0,
                                    y: 23,
                                    visible: true,
                                    shape: 0,
                                }),
                                hyperlinks: Vec::new(),
                                graphics: Vec::new(),
                                scroll: Some(crate::protocol::FrameScroll {
                                    offset_from_bottom: 0,
                                    max_offset_from_bottom: 0,
                                    viewport_rows: 24,
                                }),
                            }),
                        );
                    }
                    if connection < hang_up {
                        let _ = crate::protocol::read_message::<_, ClientMessage>(
                            &mut reader,
                            MAX_FRAME_SIZE,
                        );
                        return;
                    }
                    loop {
                        match crate::protocol::read_message::<_, ClientMessage>(
                            &mut reader,
                            MAX_FRAME_SIZE,
                        ) {
                            Ok(ClientMessage::Detach) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                }));
            }
            for handler in handlers {
                let _ = handler.join();
            }
        })
    }

    /// Stands in for a peer that refuses every control attach with `reason`.
    ///
    /// The refusal a view gets when its target is gone — or when anything else
    /// the peer can say applies. Each connection completes the handshake, is
    /// answered `ServerShutdown` instead of a frame, and is dropped.
    fn fake_peer_control_endpoint_refusing(
        path: &std::path::Path,
        connections: usize,
        code: crate::protocol::ShutdownCode,
        reason: &str,
    ) -> std::thread::JoinHandle<()> {
        use crate::protocol::{ClientMessage, RenderEncoding, ServerMessage, MAX_FRAME_SIZE};
        use interprocess::local_socket::traits::Listener as _;

        let listener = crate::ipc::bind_local_listener(path).expect("bind fake peer socket");
        let reason = reason.to_string();
        std::thread::spawn(move || {
            for _ in 0..connections {
                let Ok(stream) = listener.accept() else {
                    return;
                };
                let Ok(read_half) = interprocess::TryClone::try_clone(&stream) else {
                    return;
                };
                let mut reader = std::io::BufReader::new(read_half);
                let mut writer = std::io::BufWriter::new(stream);
                if crate::protocol::read_message::<_, ClientMessage>(&mut reader, MAX_FRAME_SIZE)
                    .is_err()
                {
                    return;
                }
                let _ = crate::protocol::write_message(
                    &mut writer,
                    &ServerMessage::Welcome {
                        version: crate::protocol::PROTOCOL_VERSION,
                        encoding: RenderEncoding::SemanticFrame,
                        error: None,
                        instance_id: Some(INSTANCE.to_string()),
                    },
                );
                // The take-control message, answered with the refusal.
                if crate::protocol::read_message::<_, ClientMessage>(&mut reader, MAX_FRAME_SIZE)
                    .is_err()
                {
                    return;
                }
                let _ = crate::protocol::write_message(
                    &mut writer,
                    &ServerMessage::ServerShutdown {
                        reason: Some(reason.clone()),
                        code,
                    },
                );
                let _ = std::io::Write::flush(&mut writer);
            }
        })
    }

    /// Registers a connected peer whose JSON API socket is `api_socket`.
    ///
    /// Reconnecting derives the terminal control socket from it exactly as
    /// opening does, so a test only has to bind its fake endpoint at the derived
    /// path for the whole path to be exercised.
    fn connected_peer(app: &mut App, name: &str, api_socket: &std::path::Path) -> PeerHandle {
        let handle = PeerHandle::new(name.to_string());
        app.state
            .peers
            .add(
                handle.clone(),
                PeerTarget::SocketPath(api_socket.to_path_buf()),
            )
            .expect("add peer");
        app.state.peers.set_identity(
            &handle,
            crate::app::peers::PeerIdentity {
                instance_id: INSTANCE.to_string(),
                version: None,
                protocol: Some(crate::protocol::PROTOCOL_VERSION),
            },
        );
        app.state
            .peers
            .set_connection(&handle, PeerConnectionState::Connected);
        handle
    }

    /// Blocks until a view notices its connection is gone.
    async fn wait_for_disconnect(app: &App, terminal_id: &crate::terminal::TerminalId) {
        for _ in 0..200 {
            let connected = app
                .terminal_runtimes
                .get(terminal_id)
                .and_then(crate::terminal::TerminalRuntime::remote)
                .is_some_and(crate::terminal::RemoteTerminalRuntime::is_connected);
            if !connected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Opens a peer workspace the way the event loop does: start the deferred
    /// open, then land the event the worker sends back.
    ///
    /// Tests used to call a synchronous `open_peer_workspace_local`. That method
    /// is gone precisely because connecting on the loop is what F1 was: the
    /// helper exists so tests exercise the real two-step path rather than a
    /// shortcut through it.
    async fn open_peer_workspace(
        app: &mut App,
        target: &str,
        name: Option<&str>,
        focus: bool,
    ) -> Result<usize, (String, String)> {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        let before = app.state.workspaces.len();
        app.start_peer_workspace_open(
            "test:peer:open".to_string(),
            target.to_string(),
            name,
            None,
            focus,
            false,
            None,
            respond_to,
        );

        // A rejection or an "already open" answers without a worker.
        if let Ok(response) = response_rx.try_recv() {
            return open_result(app, &response, before);
        }

        let event = tokio::time::timeout(Duration::from_secs(5), app.event_rx.recv())
            .await
            .expect("peer open reports back")
            .expect("event channel stays open");
        match event {
            crate::events::AppEvent::PeerViewOpenFinished(result) => {
                app.handle_peer_view_open_finished(*result)
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let response = response_rx.try_recv().expect("open answers once it lands");
        open_result(app, &response, before)
    }

    /// Turns an open's response into the workspace index it produced, or the
    /// error code and message it refused with.
    fn open_result(
        app: &App,
        response: &str,
        workspaces_before: usize,
    ) -> Result<usize, (String, String)> {
        let value: serde_json::Value =
            serde_json::from_str(response).expect("response is valid json");
        if let Some(error) = value.get("error") {
            return Err((
                error["code"].as_str().unwrap_or_default().to_string(),
                error["message"].as_str().unwrap_or_default().to_string(),
            ));
        }
        let workspace_id = value["result"]["workspace"]["workspace_id"]
            .as_str()
            .expect("a successful open names its workspace");
        let _ = workspaces_before;
        app.state
            .workspaces
            .iter()
            .position(|ws| ws.id == workspace_id)
            .ok_or_else(|| {
                (
                    "not_found".to_string(),
                    format!("workspace {workspace_id} is not in the list"),
                )
            })
    }

    /// Runs one reconnect cycle: the sweep that starts an attempt, and the event
    /// that lands its result.
    async fn run_one_reconnect_cycle(app: &mut App, terminal_id: &crate::terminal::TerminalId) {
        wait_for_disconnect(app, terminal_id).await;
        app.reconcile_remote_terminal_views();
        // The sweep either declared the view dead or dispatched an attempt; only
        // the second produces an event.
        let dead = app
            .terminal_runtimes
            .get(terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote)
            .is_some_and(|remote| remote.dead_reason().is_some());
        if dead {
            return;
        }
        let event = tokio::time::timeout(Duration::from_secs(5), app.event_rx.recv())
            .await
            .expect("reconnect attempt reports back")
            .expect("event channel stays open");
        match event {
            crate::events::AppEvent::PeerViewReconnected(result) => {
                app.handle_peer_view_reconnected(*result)
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Drives reconnect cycles until the view settles: retired out from under
    /// the sweep, declared dead and kept, or `max` attempts reached.
    async fn run_reconnect_cycles(
        app: &mut App,
        terminal_id: &crate::terminal::TerminalId,
        max: u32,
    ) {
        for _ in 0..max {
            wait_for_disconnect(app, terminal_id).await;
            app.reconcile_remote_terminal_views();
            let Some(runtime) = app.terminal_runtimes.get(terminal_id) else {
                // The sweep retired the view instead of retrying it.
                return;
            };
            if runtime
                .remote()
                .is_some_and(|remote| remote.dead_reason().is_some())
            {
                return;
            }
            let event = tokio::time::timeout(Duration::from_secs(5), app.event_rx.recv())
                .await
                .expect("reconnect attempt reports back")
                .expect("event channel stays open");
            match event {
                crate::events::AppEvent::PeerViewReconnected(result) => {
                    app.handle_peer_view_reconnected(*result)
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

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

    #[test]
    fn a_successful_add_is_recorded_in_peer_history() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let config_dir =
            std::env::temp_dir().join(format!("herdr-peer-history-test-{}", std::process::id()));
        let config_path = config_dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&config_dir);
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &config_path);

        let mut app = test_app();
        let response = app.handle_peer_add(
            "req".into(),
            PeerAddParams {
                name: "work".into(),
                target: PeerTargetSpec::Ssh {
                    destination: "me@work-box".into(),
                    session: None,
                },
            },
        );
        assert!(response.contains("\"result\""), "add failed: {response}");
        assert_eq!(app.state.peer_history.len(), 1);
        assert_eq!(app.state.peer_history[0].name, "work");
        assert_eq!(app.state.peer_history[0].target, "ssh://me@work-box");

        // The file on disk round-trips through the config loader.
        let loaded = crate::config::load_live_config().expect("config loads");
        assert_eq!(loaded.config.peer_history.recent, app.state.peer_history);

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_dir_all(&config_dir);
    }

    #[test]
    fn a_failed_add_is_not_recorded_in_peer_history() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!(
            "herdr-peer-history-failed-test-{}",
            std::process::id()
        ));
        let config_path = config_dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&config_dir);
        std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &config_path);

        let mut app = test_app();
        let target = PeerTargetSpec::Ssh {
            destination: "me@work-box".into(),
            session: None,
        };
        let first = app.handle_peer_add(
            "req".into(),
            PeerAddParams {
                name: "work".into(),
                target: target.clone(),
            },
        );
        assert!(first.contains("\"result\""), "first add failed: {first}");
        let duplicate = app.handle_peer_add(
            "req".into(),
            PeerAddParams {
                name: "work".into(),
                target,
            },
        );
        assert!(duplicate.contains("already_exists"));
        assert_eq!(app.state.peer_history.len(), 1);

        std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
        let _ = std::fs::remove_dir_all(&config_dir);
    }

    #[test]
    fn rename_and_close_on_a_peer_id_are_forwarded() {
        let rename = request(Method::WorkspaceRename(WorkspaceRenameParams {
            workspace_id: peer_ws_id("w1"),
            label: "new".into(),
        }));
        let close = request(Method::WorkspaceClose(WorkspaceTarget {
            workspace_id: peer_ws_id("w1"),
        }));
        assert!(test_app().request_targets_peer_workspace(&rename));
        assert!(test_app().request_targets_peer_workspace(&close));
    }

    #[test]
    fn local_ids_and_other_methods_stay_local() {
        // A local id must be handled by this server, never forwarded.
        let local_rename = request(Method::WorkspaceRename(WorkspaceRenameParams {
            workspace_id: "w1".into(),
            label: "new".into(),
        }));
        let local_close = request(Method::WorkspaceClose(WorkspaceTarget {
            workspace_id: "w1".into(),
        }));
        assert!(!test_app().request_targets_peer_workspace(&local_rename));
        assert!(!test_app().request_targets_peer_workspace(&local_close));

        // A local focus is answered from the workspace list, so it stays here.
        let local_focus = request(Method::WorkspaceFocus(WorkspaceTarget {
            workspace_id: "w1".into(),
        }));
        let list = request(Method::WorkspaceList(EmptyParams::default()));
        assert!(!test_app().request_targets_peer_workspace(&local_focus));
        assert!(!test_app().request_targets_peer_workspace(&list));
    }

    /// Nothing about these is forwarded — they connect here — but every one ends
    /// in a handshake with another machine, so the loop must not be the thread
    /// that waits for it. That is the same gate, reached for a second reason.
    #[test]
    fn opening_a_view_onto_a_peer_is_routed_off_the_loop() {
        let focus = request(Method::WorkspaceFocus(WorkspaceTarget {
            workspace_id: peer_ws_id("w1"),
        }));
        let open = request(Method::PeerWorkspaceOpen(
            crate::api::schema::PeerWorkspaceOpenParams {
                target: peer_ws_id("w1"),
                name: None,
                label: None,
                focus: true,
                takeover: false,
            },
        ));
        let terminal = request(Method::PeerTerminalOpen(
            crate::api::schema::PeerTerminalOpenParams {
                name: Some("beta".into()),
                target: "w1:p1".into(),
                cols: 80,
                rows: 24,
                takeover: false,
            },
        ));
        assert!(test_app().request_targets_peer_workspace(&focus));
        assert!(test_app().request_targets_peer_workspace(&open));
        assert!(test_app().request_targets_peer_workspace(&terminal));
    }

    /// Reaching the synchronous handler means a caller bypassed the gate above.
    /// Refusing is what keeps that from silently becoming a frozen server again.
    #[test]
    fn the_synchronous_handler_refuses_an_open_that_skipped_the_gate() {
        let mut app = test_app();
        for method in [
            Method::PeerWorkspaceOpen(crate::api::schema::PeerWorkspaceOpenParams {
                target: peer_ws_id("w1"),
                name: None,
                label: None,
                focus: true,
                takeover: false,
            }),
            Method::PeerTerminalOpen(crate::api::schema::PeerTerminalOpenParams {
                name: Some("beta".into()),
                target: "w1:p1".into(),
                cols: 80,
                rows: 24,
                takeover: false,
            }),
        ] {
            let response = app.handle_api_request(request(method));
            let value: serde_json::Value =
                serde_json::from_str(&response).expect("response is valid json");
            assert_eq!(value["error"]["code"], "invalid_request", "{response}");
        }
    }

    #[test]
    fn rename_response_rewrites_id_and_renamespaces_workspace_ids() {
        // What a peer returns for workspace.rename: its own request id and its
        // own peer-local workspace ids.
        let mut value = serde_json::json!({
            "id": "peer:forward",
            "result": {
                "workspace": {
                    "workspace_id": "w1",
                    "active_tab_id": "w1:t1",
                    "label": "renamed"
                }
            }
        });
        rewrite_forwarded_response(&mut value, "caller-42", INSTANCE, true);

        assert_eq!(value["id"], "caller-42");
        assert_eq!(
            value["result"]["workspace"]["workspace_id"],
            peer_ws_id("w1")
        );
        assert_eq!(
            value["result"]["workspace"]["active_tab_id"],
            peer_ws_id("w1:t1")
        );
        // Unrelated fields are left untouched.
        assert_eq!(value["result"]["workspace"]["label"], "renamed");
    }

    #[test]
    fn close_response_rewrites_only_the_id() {
        let mut value = serde_json::json!({ "id": "peer:forward", "result": {} });
        rewrite_forwarded_response(&mut value, "caller-42", INSTANCE, false);
        assert_eq!(value["id"], "caller-42");
        assert_eq!(value["result"], serde_json::json!({}));
    }

    #[test]
    fn read_response_is_restated_in_local_ids() {
        // What a peer returns for pane.read: its own request id, its own ids,
        // and the screen text, which is the part we actually wanted.
        let mut value = serde_json::json!({
            "id": "peer:forward",
            "result": {
                "read": {
                    "pane_id": "w1:p1",
                    "workspace_id": "w1",
                    "tab_id": "w1:t1",
                    "source": "visible",
                    "text": "PEER SCREEN",
                    "truncated": false
                }
            }
        });
        rewrite_forwarded_read(
            &mut value,
            "caller-42",
            &LocalPaneIds {
                pane_id: "w7:p2".into(),
                workspace_id: "w7".into(),
                tab_id: "w7:t1".into(),
            },
        );

        assert_eq!(value["id"], "caller-42");
        // The caller asked about a pane on this server and must get an id it can
        // use again here, not the peer's.
        assert_eq!(value["result"]["read"]["pane_id"], "w7:p2");
        assert_eq!(value["result"]["read"]["workspace_id"], "w7");
        assert_eq!(value["result"]["read"]["tab_id"], "w7:t1");
        // The screen belongs to the peer and is reported verbatim.
        assert_eq!(value["result"]["read"]["text"], "PEER SCREEN");
        assert_eq!(value["result"]["read"]["truncated"], false);
    }

    #[tokio::test]
    async fn reading_a_peer_backed_pane_is_routed_to_the_peer() {
        let socket = unique_socket_path("peer-read-gate");
        let _endpoint = fake_peer_control_endpoint(&socket);
        let mut app = test_app();

        // A local pane in the same app, to prove the gate reads the runtime
        // rather than assuming every pane is a peer's once a peer exists.
        app.create_workspace();
        let local_ws = app.state.workspaces.len() - 1;
        let local_pane = app.state.workspaces[local_ws]
            .focused_pane_id()
            .expect("local workspace has a pane");
        let local_public = app
            .public_pane_id(local_ws, local_pane)
            .expect("local pane has a public id");

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        let ws_idx = app.create_attached_workspace(
            PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            None,
            runtime,
            true,
        );
        let pane_id = app.state.workspaces[ws_idx]
            .focused_pane_id()
            .expect("attached workspace has a pane");
        let public_pane_id = app
            .public_pane_id(ws_idx, pane_id)
            .expect("peer-backed pane has a public id");

        // Answered locally this returns empty text, which reads as "the pane is
        // blank" rather than "the screen is on another server".
        assert!(app.request_targets_peer_pane(&read_request(&public_pane_id)));
        assert!(app.request_targets_peer_pane(&text_query_request(&public_pane_id)));

        // A pane with a local pty answers locally, and an unknown id is not
        // mistaken for a peer's.
        assert!(!app.request_targets_peer_pane(&read_request(&local_public)));
        assert!(!app.request_targets_peer_pane(&text_query_request(&local_public)));
        assert!(!app.request_targets_peer_pane(&read_request("w99:p9")));

        drop(app);
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn copy_mode_opens_on_a_peer_backed_pane() {
        let socket = unique_socket_path("peer-copy-mode");
        let _endpoint = fake_peer_control_endpoint_with_frame(&socket);
        let mut app = test_app();

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        let ws_idx = app.create_attached_workspace(
            PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            None,
            runtime,
            true,
        );
        app.state.active = Some(ws_idx);
        app.state.view.pane_infos = app.state.workspaces[ws_idx]
            .active_tab()
            .expect("attached workspace has a tab")
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 80, 24));

        assert!(app.state.enter_copy_mode(&app.terminal_runtimes));
        assert!(app.state.copy_mode.is_some());
        assert_eq!(app.state.mode, crate::app::Mode::Copy);
        assert!(app.state.copy_feedback.is_none());

        for _ in 0..100 {
            let has_metrics = app
                .state
                .runtime_for_pane_in_workspace(
                    &app.terminal_runtimes,
                    ws_idx,
                    app.state.copy_mode.as_ref().unwrap().pane_id,
                )
                .and_then(crate::terminal::TerminalRuntime::scroll_metrics)
                .is_some();
            if has_metrics {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for key in [
            crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Char('/'),
                crossterm::event::KeyModifiers::NONE,
            ),
            crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::NONE,
            ),
            crate::input::TerminalKey::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
        ] {
            app.state.handle_copy_mode_key(&app.terminal_runtimes, key);
        }
        let request = app
            .state
            .request_peer_copy_mode_query
            .as_ref()
            .expect("remote search is deferred to the peer");
        assert!(matches!(
            &request.query,
            crate::api::schema::PaneTextQuery::Search { query, .. } if query == "a"
        ));

        drop(app);
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn scrolling_a_peer_backed_pane_reaches_the_peer() {
        let socket = unique_socket_path("peer-scroll");
        let (_endpoint, seen) = fake_peer_control_endpoint_recording(&socket);

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");

        runtime.scroll_up(3);
        runtime.scroll_down(2);

        let mut scrolls = Vec::new();
        for _ in 0..200 {
            scrolls = seen
                .lock()
                .expect("recorder")
                .iter()
                .filter_map(|message| match message {
                    crate::protocol::ClientMessage::AttachScroll {
                        direction, lines, ..
                    } => Some((*direction, *lines)),
                    _ => None,
                })
                .collect();
            if scrolls.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The peer owns the scrollback, so the wire is the only place a scroll
        // shows up at all.
        assert_eq!(
            scrolls,
            vec![
                (crate::protocol::AttachScrollDirection::Up, 3),
                (crate::protocol::AttachScrollDirection::Down, 2),
            ]
        );

        runtime.shutdown();
        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn peer_workspace_id_is_read_only_from_a_pane_id() {
        assert_eq!(peer_workspace_of_pane_id("w1:p2").as_deref(), Some("w1"));
        assert_eq!(peer_workspace_of_pane_id("w12:p34").as_deref(), Some("w12"));
        // A terminal id or an agent name names no workspace, and inventing one
        // would target the wrong thing rather than merely fail.
        assert_eq!(peer_workspace_of_pane_id("term_abc123"), None);
        assert_eq!(peer_workspace_of_pane_id("w1:t1"), None);
        assert_eq!(peer_workspace_of_pane_id("claude"), None);
        assert_eq!(peer_workspace_of_pane_id("w1:p"), None);
        assert_eq!(peer_workspace_of_pane_id(":p1"), None);
    }

    #[tokio::test]
    async fn creating_a_tab_in_a_peer_backed_workspace_is_routed_to_the_peer() {
        let socket = unique_socket_path("peer-tab-gate");
        let _endpoint = fake_peer_control_endpoint(&socket);
        let mut app = test_app();

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("local"));
        let local_idx = app.state.workspaces.len() - 1;
        let local_id = app.state.workspaces[local_idx].id.clone();
        let ws_idx = app.create_attached_workspace(
            PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            None,
            runtime,
            true,
        );
        app.state.active = Some(ws_idx);
        let peer_workspace_id = app.state.workspaces[ws_idx].id.clone();

        let tab_create = |workspace_id: Option<&str>| {
            request(Method::TabCreate(crate::api::schema::TabCreateParams {
                workspace_id: workspace_id.map(str::to_string),
                cwd: None,
                focus: true,
                label: None,
                env: Default::default(),
            }))
        };

        // Named explicitly, and by falling back to the active workspace: the TUI
        // sends no workspace id at all, which is how this was reaching the local
        // path in the first place.
        assert!(app.request_targets_peer_workspace(&tab_create(Some(&peer_workspace_id))));
        assert!(app.request_targets_peer_workspace(&tab_create(None)));

        // A local workspace keeps creating its tabs here.
        assert!(!app.request_targets_peer_workspace(&tab_create(Some(&local_id))));
        app.state.active = Some(local_idx);
        assert!(!app.request_targets_peer_workspace(&tab_create(None)));

        drop(app);
        let _ = std::fs::remove_file(&socket);
    }

    /// A worktree action in a peer view has to run where the checkout is.
    ///
    /// The failure this guards is quiet rather than loud: run here, `git
    /// worktree` against the peer's cwd usually fails, but when the same path
    /// exists on both machines it succeeds against the wrong host's repo.
    #[tokio::test]
    async fn worktree_actions_in_a_peer_backed_workspace_are_routed_to_the_peer() {
        let socket = unique_socket_path("peer-worktree-gate");
        let _endpoint = fake_peer_control_endpoint(&socket);
        let mut app = test_app();

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("local"));
        let local_idx = app.state.workspaces.len() - 1;
        let local_id = app.state.workspaces[local_idx].id.clone();
        let ws_idx = app.create_attached_workspace(
            PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            None,
            runtime,
            true,
        );
        app.state.active = Some(ws_idx);
        let view_id = app.state.workspaces[ws_idx].id.clone();

        let list = |workspace_id: Option<&str>, cwd: Option<&str>| {
            request(Method::WorktreeList(
                crate::api::schema::WorktreeListParams {
                    workspace_id: workspace_id.map(str::to_string),
                    cwd: cwd.map(str::to_string),
                },
            ))
        };
        let create = |workspace_id: Option<&str>| {
            request(Method::WorktreeCreate(
                crate::api::schema::WorktreeCreateParams {
                    workspace_id: workspace_id.map(str::to_string),
                    branch: Some("worktree/routed".into()),
                    ..Default::default()
                },
            ))
        };
        let open = |workspace_id: Option<&str>| {
            request(Method::WorktreeOpen(
                crate::api::schema::WorktreeOpenParams {
                    workspace_id: workspace_id.map(str::to_string),
                    branch: Some("worktree/routed".into()),
                    ..Default::default()
                },
            ))
        };
        let remove = |workspace_id: &str| {
            request(Method::WorktreeRemove(
                crate::api::schema::WorktreeRemoveParams {
                    workspace_id: workspace_id.to_string(),
                    force: false,
                },
            ))
        };

        // Named explicitly, and by falling back to the active workspace, which
        // is what the CLI sends when no `--workspace` is given.
        for named in [
            list(Some(&view_id), None),
            create(Some(&view_id)),
            open(Some(&view_id)),
        ] {
            assert!(app.request_targets_peer_workspace(&named));
        }
        for fallback in [list(None, None), create(None), open(None)] {
            assert!(app.request_targets_peer_workspace(&fallback));
        }
        assert!(app.request_targets_peer_workspace(&remove(&view_id)));

        // A `cwd` names a path on *this* filesystem, so it is this server's
        // question even while a peer view is active.
        assert!(!app.request_targets_peer_workspace(&list(None, Some("/repo/herdr"))));

        // A local workspace keeps running its own git.
        assert!(!app.request_targets_peer_workspace(&list(Some(&local_id), None)));
        assert!(!app.request_targets_peer_workspace(&remove(&local_id)));
        app.state.active = Some(local_idx);
        assert!(!app.request_targets_peer_workspace(&list(None, None)));
        assert!(!app.request_targets_peer_workspace(&create(None)));

        drop(app);
        let _ = std::fs::remove_file(&socket);
    }

    /// A view onto a bare pane names no workspace on its peer, so there is no
    /// cwd there to start from. It must say so rather than fall through to the
    /// local handler, which would run git against the peer's path here.
    #[tokio::test]
    async fn a_worktree_action_on_a_bare_peer_view_is_refused() {
        let socket = unique_socket_path("peer-worktree-bare");
        let _endpoint = fake_peer_control_endpoint(&socket);
        let mut app = test_app();

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "term_abc".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        let ws_idx = app.create_attached_workspace(
            PathBuf::from("/"),
            "beta".to_string(),
            None,
            None,
            runtime,
            true,
        );
        app.state.active = Some(ws_idx);
        let view_id = app.state.workspaces[ws_idx].id.clone();

        let create = request(Method::WorktreeCreate(
            crate::api::schema::WorktreeCreateParams {
                workspace_id: Some(view_id),
                branch: Some("worktree/bare".into()),
                ..Default::default()
            },
        ));
        assert!(app.request_targets_peer_workspace(&create));

        let (respond_to, response_rx) = std::sync::mpsc::channel::<String>();
        app.handle_deferred_peer_workspace_api_request(create, respond_to);
        let response = response_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("a refusal is answered inline");
        assert!(
            response.contains("does not name a workspace on its peer"),
            "unexpected response: {response}"
        );

        drop(app);
        let _ = std::fs::remove_file(&socket);
    }

    /// The UI's own entry point has to route like the socket path.
    ///
    /// The gate above only proves the predicate answers correctly. It ran solely
    /// for requests arriving over the socket, while a keybind, the tab-name
    /// dialog and the `+` button all reach `dispatch_runtime_mutation` in
    /// process — which handed them straight to the local handler and spawned a
    /// pty inside a workspace that views another machine. Asserting on the
    /// predicate cannot catch that; only calling the UI entry point can.
    #[tokio::test]
    async fn a_ui_tab_create_on_a_peer_workspace_spawns_no_local_pane() {
        let api_socket = unique_socket_path("peer-tab-ui");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint(&control_socket);
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &control_socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        let ws_idx = app.create_attached_workspace(
            PathBuf::from("/"),
            handle.as_str().to_string(),
            Some("w1".to_string()),
            None,
            runtime,
            true,
        );
        // No workspace id, exactly as every UI new-tab path sends it.
        app.state.active = Some(ws_idx);
        let tabs_before = app.state.workspaces[ws_idx].tabs.len();

        app.runtime_tab_create(
            "test.tui.tab.create",
            crate::api::schema::TabCreateParams {
                workspace_id: None,
                cwd: None,
                focus: true,
                label: None,
                env: Default::default(),
            },
        );

        // The peer answers on the event loop, so the view is attached later. The
        // regression is not "no tab yet" but a local pty appearing right here.
        assert_eq!(
            app.state.workspaces[ws_idx].tabs.len(),
            tabs_before,
            "a peer-backed workspace must not gain a locally spawned tab"
        );
        assert!(
            app.state
                .workspaces
                .iter()
                .all(|ws| ws.peer.is_some() || ws.tabs.len() <= 1),
            "no local pty should have been spawned for this request"
        );

        drop(app);
        let _ = std::fs::remove_file(&api_socket);
        let _ = std::fs::remove_file(&control_socket);
    }

    #[tokio::test]
    async fn a_tab_attached_to_a_peer_keeps_workspace_identity_intact() {
        let socket = unique_socket_path("peer-tab-attach");
        let _endpoint = fake_peer_control_endpoint(&socket);
        let mut app = test_app();

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        let ws_idx = app.create_attached_workspace(
            PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            None,
            runtime,
            true,
        );

        // The attach path is what is under test, not the transport, so this
        // stands in for the second view the peer would have handed back.
        let (second, _second_rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let (tab_idx, terminal, second) =
            app.state.workspaces[ws_idx].create_tab_attached(PathBuf::from("/"), second);
        app.terminal_runtimes.insert(terminal.id.clone(), second);
        app.state.terminals.insert(terminal.id.clone(), terminal);

        assert_eq!(tab_idx, 1, "the attached tab is appended");
        assert_eq!(app.state.workspaces[ws_idx].tabs.len(), 2);
        // Tab and pane numbering is local bookkeeping even though the terminal
        // behind the tab is not, so identity has to survive the attach.
        app.state.workspaces[ws_idx].assert_invariants_for_test();
        app.state.assert_invariants_for_test();

        drop(app);
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn a_peer_backed_pane_encodes_keys_instead_of_dropping_them() {
        let socket = unique_socket_path("peer-key-encode");
        let _endpoint = fake_peer_control_endpoint(&socket);

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");

        // No local VT backs a remote terminal, so the encoding has to come from
        // the protocol-only encoder. Returning empty here drops every keystroke
        // before the send path is ever reached.
        let ctrl_c = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert_eq!(runtime.encode_terminal_key(ctrl_c), b"\x03");

        let plain = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('f'),
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(runtime.encode_terminal_key(plain), b"f");

        // Legacy until the peer says otherwise: a view opened before the first
        // pane enumeration has nothing better to go on.
        assert_eq!(
            runtime.keyboard_protocol(),
            crate::input::KeyboardProtocol::Legacy
        );

        runtime.shutdown();
        let _ = std::fs::remove_file(&socket);
    }

    /// The regression this guards: `keyboard_protocol()` was the constant
    /// `Legacy` for every remote pane, so a program on the peer using the Kitty
    /// protocol got legacy encodings — Shift+Enter arriving as a plain Enter,
    /// which submits an agent's prompt instead of adding a newline. The same
    /// keypress works in a local pane, so the remote one reads as broken.
    #[tokio::test]
    async fn a_peer_backed_pane_encodes_keys_the_way_its_peer_reported() {
        let socket = unique_socket_path("peer-key-protocol");
        let _endpoint = fake_peer_control_endpoint(&socket);

        let mut runtime = crate::terminal::RemoteTerminalRuntime::connect(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");

        let shift_enter = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::SHIFT,
        );
        let legacy = crate::input::encode_terminal_key(
            shift_enter.clone(),
            crate::input::KeyboardProtocol::Legacy,
        );

        // The peer reports that its program asked for the Kitty protocol.
        runtime.set_metadata(crate::terminal::RemotePaneMetadata {
            keyboard_protocol: Some(crate::api::schema::KeyboardProtocolInfo::Kitty { flags: 31 }),
            ..Default::default()
        });
        assert_eq!(
            runtime.keyboard_protocol(),
            crate::input::KeyboardProtocol::Kitty { flags: 31 }
        );

        let enhanced = crate::input::encode_terminal_key(
            shift_enter,
            crate::input::KeyboardProtocol::Kitty { flags: 31 },
        );
        assert_ne!(
            enhanced, legacy,
            "the two protocols have to differ for this test to mean anything"
        );

        runtime.shutdown();
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn typing_into_a_peer_backed_pane_reaches_the_peer() {
        let socket = unique_socket_path("peer-key-input");
        let (_endpoint, seen) = fake_peer_control_endpoint_recording(&socket);

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");

        let key = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('f'),
            crossterm::event::KeyModifiers::NONE,
        );
        let bytes = runtime.encode_terminal_key(key);
        assert!(!bytes.is_empty(), "a peer pane must encode its keys");
        runtime
            .try_send_bytes(bytes::Bytes::from(bytes))
            .expect("sending to a peer-backed pane cannot fail locally");

        let mut input = Vec::new();
        for _ in 0..200 {
            input = seen
                .lock()
                .expect("recorder")
                .iter()
                .filter_map(|message| match message {
                    crate::protocol::ClientMessage::Input { data } => Some(data.clone()),
                    _ => None,
                })
                .collect();
            if !input.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The peer holds the pty, so the wire is the only place the keystroke
        // can show up at all.
        assert_eq!(input, vec![b"f".to_vec()]);

        runtime.shutdown();
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn a_delayed_send_to_a_peer_backed_pane_reaches_the_peer() {
        let socket = unique_socket_path("peer-delayed-send");
        let (_endpoint, seen) = fake_peer_control_endpoint_recording(&socket);

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");

        // `agent.prompt` submits with a delayed Enter, so dropping this makes a
        // peer-backed agent receive its prompt text and never run it.
        runtime.send_bytes_after(bytes::Bytes::from_static(b"\r"), Duration::from_millis(10));

        let mut input = Vec::new();
        for _ in 0..200 {
            input = seen
                .lock()
                .expect("recorder")
                .iter()
                .filter_map(|message| match message {
                    crate::protocol::ClientMessage::Input { data } => Some(data.clone()),
                    _ => None,
                })
                .collect();
            if !input.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(input, vec![b"\r".to_vec()]);

        runtime.shutdown();
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn clicking_a_peer_backed_pane_reaches_the_peer_unencoded() {
        let socket = unique_socket_path("peer-mouse");
        let (_endpoint, seen) = fake_peer_control_endpoint_recording(&socket);

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");

        // Nothing here can encode a mouse report: the protocol the program asked
        // for is VT state on the peer. The click goes over as an event instead.
        //
        // Polled because the peer's mode report races the first click: until it
        // lands the view declines, so that its own selection gets the click.
        let mut accepted = false;
        for _ in 0..200 {
            accepted = runtime.try_send_mouse_button(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                crate::input::mouse::Position::Cell { column: 4, row: 2 },
                crossterm::event::KeyModifiers::NONE,
            );
            if accepted {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            accepted,
            "a peer reporting mouse reporting must be given the click"
        );

        // Motion with no button is not carried, so a hover is refused here
        // rather than filling the wire with events the peer may not want.
        assert!(!runtime.try_send_mouse_button(
            crossterm::event::MouseEventKind::Moved,
            crate::input::mouse::Position::Cell { column: 5, row: 2 },
            crossterm::event::KeyModifiers::NONE,
        ));

        let mut clicks = Vec::new();
        for _ in 0..200 {
            clicks = seen
                .lock()
                .expect("recorder")
                .iter()
                .filter_map(|message| match message {
                    crate::protocol::ClientMessage::AttachMouse {
                        kind,
                        column,
                        row,
                        modifiers,
                    } => Some((*kind, *column, *row, *modifiers)),
                    _ => None,
                })
                .collect();
            if !clicks.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(
            clicks,
            vec![(
                crate::protocol::ClientMouseKind::Down(crate::protocol::ClientMouseButton::Left),
                4,
                2,
                0
            )]
        );

        runtime.shutdown();
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn splitting_a_peer_backed_pane_is_routed_to_the_peer() {
        let socket = unique_socket_path("peer-split-gate");
        let _endpoint = fake_peer_control_endpoint(&socket);
        let mut app = test_app();

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        let ws_idx = app.create_attached_workspace(
            PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            None,
            runtime,
            true,
        );
        let peer_workspace_id = app.state.workspaces[ws_idx].id.clone();
        let pane_id = app.state.workspaces[ws_idx]
            .focused_pane_id()
            .expect("attached workspace has a pane");

        // The pane's peer address comes from its runtime, not its workspace.
        let (handle, target) = app
            .peer_pane_source(ws_idx, pane_id)
            .expect("pane is backed by a peer");
        assert_eq!(handle.as_str(), "beta");
        assert_eq!(target, "w1:p1");

        // Splitting it must not spawn a local shell beside the remote view.
        assert!(app.request_targets_peer_pane(&split_request(Some(peer_workspace_id))));
        // The active workspace is the peer-backed one, so an unqualified split
        // is routed too.
        assert!(app.request_targets_peer_pane(&split_request(None)));

        drop(app);
        let _ = std::fs::remove_file(&socket);
    }

    /// The gate above decides *where* a split runs; this decides what the UI
    /// caller is told about it. `Accepted` is the whole point of the type: the
    /// empty string it replaced parses as neither a response nor an error, so
    /// `submit_worktree_open_via_api` had to carry its own `on_peer` flag to
    /// tell a routed request from a handler that returned nothing.
    #[tokio::test]
    async fn a_routed_split_is_accepted_and_a_refusal_is_still_an_answer() {
        let socket = unique_socket_path("peer-split-outcome");
        let _endpoint = fake_peer_control_endpoint(&socket);
        let mut app = test_app();

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        let ws_idx = app.create_attached_workspace(
            PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            None,
            runtime,
            true,
        );
        let peer_workspace_id = app.state.workspaces[ws_idx].id.clone();

        let Method::PaneSplit(params) = split_request(Some(peer_workspace_id)).method else {
            unreachable!("split_request builds a pane.split");
        };
        assert_eq!(
            app.runtime_pane_split("test.pane.split", params),
            crate::app::runtime_mutations::RuntimeMutation::Accepted
        );

        // An error is still an answer. Only a request that left this server is
        // accepted, which is exactly the distinction an empty string could not
        // carry: both used to arrive as `""`.
        let refused = app.runtime_workspace_focus("test.workspace.focus", "w404".to_string());
        assert!(
            refused
                .answered()
                .is_some_and(|response| response.contains("\"error\"")),
            "a local refusal must be answered, got {refused:?}"
        );

        drop(app);
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn the_picker_marks_the_enumerated_workspace_a_view_is_already_open_on() {
        let api_socket = unique_socket_path("peer-picker-open");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint(&control_socket);
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);
        app.state.peers.set_workspaces(
            &handle,
            vec![
                crate::api::schema::WorkspaceInfo {
                    workspace_id: "w1".into(),
                    number: 1,
                    label: "api".into(),
                    focused: false,
                    pane_count: 1,
                    tab_count: 1,
                    active_tab_id: "w1:t1".into(),
                    agent_status: crate::api::schema::AgentStatus::Unknown,
                    tokens: Default::default(),
                    worktree: None,
                },
                crate::api::schema::WorkspaceInfo {
                    workspace_id: "w2".into(),
                    number: 2,
                    label: "web".into(),
                    focused: false,
                    pane_count: 1,
                    tab_count: 1,
                    active_tab_id: "w2:t1".into(),
                    agent_status: crate::api::schema::AgentStatus::Unknown,
                    tokens: Default::default(),
                    worktree: None,
                },
            ],
        );

        // Open a view onto a *pane* of the first one. A view always addresses a
        // pane — a workspace target resolves to one before connecting — so the
        // marker has to recognise `w1:p1` as "w1 is open".
        let ws_idx = open_peer_workspace(&mut app, &peer_ws_id("w1:p1"), None, true)
            .await
            .expect("open the peer workspace");

        app.open_peer_workspace_picker("beta");
        let open = app
            .state
            .peer_workspace_open
            .as_ref()
            .expect("picker opened");

        // Both stay listed; only the open one is marked, and it points at the
        // view that already exists rather than being filtered away.
        assert_eq!(open.entries.len(), 2);
        assert_eq!(open.entries[0].already_open_ws_idx, Some(ws_idx));
        assert_eq!(open.entries[0].status_label(), "open");
        assert_eq!(open.entries[1].already_open_ws_idx, None);
        assert_eq!(open.entries[1].status_label(), "");

        // Reopening the workspace the view sits inside finds it rather than
        // asking the peer for a pane again — which is also what makes this
        // reachable while the peer is down.
        assert_eq!(
            open_peer_workspace(&mut app, &peer_ws_id("w1"), None, false).await,
            Ok(ws_idx)
        );

        // A workspace id the peer enumerated is recognised as one; a pane id
        // inside it is not, so it is never sent for pane resolution.
        assert!(app.peer_target_is_an_enumerated_workspace(&handle, "w1"));
        assert!(!app.peer_target_is_an_enumerated_workspace(&handle, "w1:p1"));
        assert!(!app.peer_target_is_an_enumerated_workspace(&handle, "w9"));

        // The separator is what keeps the prefix test exact.
        assert_eq!(
            app.workspace_viewing_peer_workspace("beta", "w1"),
            Some(ws_idx)
        );
        assert_eq!(app.workspace_viewing_peer_workspace("beta", "w"), None);
        assert_eq!(app.workspace_viewing_peer_workspace("beta", "w2"), None);

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    #[tokio::test]
    async fn an_open_view_is_found_while_its_peer_is_down() {
        let api_socket = unique_socket_path("peer-picker-down");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint(&control_socket);
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);
        app.state.peers.set_workspaces(
            &handle,
            vec![crate::api::schema::WorkspaceInfo {
                workspace_id: "w1".into(),
                number: 1,
                label: "api".into(),
                focused: false,
                pane_count: 1,
                tab_count: 1,
                active_tab_id: "w1:t1".into(),
                agent_status: crate::api::schema::AgentStatus::Unknown,
                tokens: Default::default(),
                worktree: None,
            }],
        );
        let ws_idx = open_peer_workspace(&mut app, &peer_ws_id("w1:p1"), None, true)
            .await
            .expect("open the peer workspace");

        app.state.peers.set_connection(
            &handle,
            PeerConnectionState::Reconnecting {
                attempt: 2,
                message: "connection refused".into(),
            },
        );

        // Switching to a view you can still see must not need the peer.
        assert_eq!(
            open_peer_workspace(&mut app, &peer_ws_id("w1"), None, true).await,
            Ok(ws_idx)
        );
        // Opening one you do not have still reports the peer's state.
        let (code, _) = open_peer_workspace(&mut app, &peer_ws_id("w2:p1"), None, true)
            .await
            .expect_err("a new view needs the peer");
        assert_eq!(code, "unavailable");

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    #[tokio::test]
    async fn only_a_pane_we_spawned_is_closed_on_the_peer() {
        let adopted_socket = unique_socket_path("peer-adopted-claim");
        let spawned_socket = unique_socket_path("peer-spawned-claim");
        let _adopted_endpoint = fake_peer_control_endpoint(&adopted_socket);
        let _spawned_endpoint = fake_peer_control_endpoint(&spawned_socket);

        let adopted = crate::terminal::TerminalRuntime::connect_remote(
            &adopted_socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        // A view onto a pane the peer already had claims nothing.
        assert_eq!(adopted.spawned_peer_pane(), None);
        drop(adopted);

        let mut spawned = crate::terminal::TerminalRuntime::connect_remote(
            &spawned_socket,
            "beta".to_string(),
            "w1:p2".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        let crate::terminal::TerminalRuntime::Remote(runtime) = &mut spawned else {
            panic!("connect_remote returns a remote runtime");
        };
        runtime.mark_spawned_on_peer();
        // The peer is named on the runtime because the pane has already left
        // the workspace by the time a view is torn down.
        assert_eq!(
            spawned
                .spawned_peer_pane()
                .map(|pane| (pane.peer, pane.peer_pane_id)),
            Some(("beta", "w1:p2"))
        );

        assert!(spawned.disown_spawned_peer_pane());
        assert_eq!(spawned.spawned_peer_pane(), None);
        // Disowning twice is not an error, it just claims nothing.
        assert!(!spawned.disown_spawned_peer_pane());

        drop(spawned);
        let _ = std::fs::remove_file(&adopted_socket);
        let _ = std::fs::remove_file(&spawned_socket);
    }

    #[tokio::test]
    async fn removing_a_peer_closes_its_views_and_drops_their_claims() {
        let socket = unique_socket_path("peer-remove-views");
        let _endpoint = fake_peer_control_endpoint_serving(&socket, 2, 0);
        let mut app = test_app();

        let mut runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        if let crate::terminal::TerminalRuntime::Remote(remote) = &mut runtime {
            remote.mark_spawned_on_peer();
        }
        app.state
            .peers
            .add(
                PeerHandle::new("beta"),
                PeerTarget::SocketPath(socket.clone()),
            )
            .expect("add peer");
        let local_workspaces = app.state.workspaces.len();
        app.create_attached_workspace(
            PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            None,
            runtime,
            true,
        );
        let bare_runtime = crate::terminal::RemoteTerminalRuntime::connect(
            &socket,
            "beta".to_string(),
            "w1:p2".to_string(),
            80,
            24,
            false,
        )
        .expect("connect bare peer terminal");
        let bare_terminal_id = app.register_peer_terminal(
            crate::terminal::TerminalRuntime::Remote(Box::new(bare_runtime)),
        );
        assert!(app.state.terminals.contains_key(&bare_terminal_id));
        assert_eq!(app.state.workspaces.len(), local_workspaces + 1);

        let response = app.handle_peer_remove(
            "req".into(),
            PeerRef {
                name: "beta".into(),
            },
        );
        assert!(response.contains("peer_list"), "unexpected: {response}");
        // The views can only render a frozen frame once the peer is gone, so
        // removal takes them with it.
        assert_eq!(app.state.workspaces.len(), local_workspaces);
        assert!(app.state.peers.get(&PeerHandle::new("beta")).is_none());
        assert!(!app.state.terminals.contains_key(&bare_terminal_id));
        assert!(app.terminal_runtimes.get(&bare_terminal_id).is_none());

        drop(app);
        let _ = std::fs::remove_file(&socket);
    }

    fn peer_pane_info(
        pane_id: &str,
        terminal_id: &str,
        agent: Option<&str>,
        agent_status: crate::api::schema::AgentStatus,
    ) -> crate::api::schema::PaneInfo {
        crate::api::schema::PaneInfo {
            pane_id: pane_id.into(),
            terminal_id: terminal_id.into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            focused: false,
            cwd: Some("/srv/app".into()),
            foreground_cwd: Some("/srv/app/crate".into()),
            label: None,
            agent: agent.map(str::to_string),
            title: None,
            terminal_title: Some("building".into()),
            terminal_title_stripped: None,
            display_agent: None,
            agent_osc_title: Some("Compiling".into()),
            agent_osc_progress: Some("42".into()),
            agent_status,
            state_labels: std::collections::HashMap::new(),
            tokens: std::collections::HashMap::new(),
            agent_session: None,
            scroll: None,
            keyboard_protocol: None,
            peer: None,
            peer_view: None,
            owner_instance_id: None,
            owner_attached: None,
            revision: 0,
        }
    }

    #[tokio::test]
    async fn a_peer_enumeration_labels_the_view_onto_its_pane() {
        let socket = unique_socket_path("peer-pane-metadata");
        let _endpoint = fake_peer_control_endpoint(&socket);
        let mut app = test_app();

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        app.create_attached_workspace(
            PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            None,
            runtime,
            true,
        );

        let panes = vec![
            // The view is onto the first pane; the second is the peer's own
            // business and must not label anything here.
            peer_pane_info(
                "w1:p1",
                "term_1",
                Some("claude"),
                crate::api::schema::AgentStatus::Working,
            ),
            peer_pane_info(
                "w1:p2",
                "term_2",
                Some("codex"),
                crate::api::schema::AgentStatus::Blocked,
            ),
        ];
        let detections = app.handle_peer_panes_updated(&PeerHandle::new("beta"), &panes);

        let metadata = app
            .terminal_runtimes
            .values()
            .find_map(crate::terminal::TerminalRuntime::remote)
            .and_then(crate::terminal::RemoteTerminalRuntime::metadata)
            .cloned()
            .expect("the view onto the peer pane was labeled");
        assert_eq!(metadata.cwd, Some(PathBuf::from("/srv/app")));
        assert_eq!(
            metadata.foreground_cwd,
            Some(PathBuf::from("/srv/app/crate"))
        );
        assert_eq!(metadata.terminal_title.as_deref(), Some("building"));
        assert_eq!(metadata.agent_osc_title.as_deref(), Some("Compiling"));
        assert_eq!(metadata.agent_osc_progress.as_deref(), Some("42"));
        assert_eq!(metadata.agent.as_deref(), Some("claude"));

        assert_eq!(detections.len(), 1, "unexpected: {detections:?}");
        match &detections[0] {
            crate::events::AppEvent::StateChanged {
                agent,
                state,
                process_exited,
                ..
            } => {
                assert_eq!(*agent, Some(crate::detect::Agent::Claude));
                assert_eq!(*state, crate::detect::AgentState::Working);
                assert!(!process_exited);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // The poll repeats every interval, so an unchanged enumeration must not
        // keep re-reporting detection the terminal already has.
        assert!(app
            .handle_peer_panes_updated(&PeerHandle::new("beta"), &panes)
            .is_empty());

        // A peer this view is not onto never claims it, even for the same ids.
        assert!(app
            .handle_peer_panes_updated(&PeerHandle::new("gamma"), &panes)
            .is_empty());

        drop(app);
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn an_agent_a_peer_reports_sends_its_read_back_to_that_peer() {
        let socket = unique_socket_path("peer-agent-read-gate");
        let _endpoint = fake_peer_control_endpoint(&socket);
        let mut app = test_app();

        // A local agent pane, to prove the gate reads the runtime rather than
        // routing every agent away once a peer has one.
        app.create_workspace();
        let local_ws = app.state.workspaces.len() - 1;
        let local_pane = app.state.workspaces[local_ws]
            .focused_pane_id()
            .expect("local workspace has a pane");
        let local_public = app
            .public_pane_id(local_ws, local_pane)
            .expect("local pane has a public id");
        let local_terminal = app.state.workspaces[local_ws]
            .terminal_id(local_pane)
            .expect("local pane has a terminal")
            .clone();
        app.state
            .terminals
            .get_mut(&local_terminal)
            .expect("local terminal")
            .set_detected_state(
                Some(crate::detect::Agent::Claude),
                crate::detect::AgentState::Idle,
            );

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        let ws_idx = app.create_attached_workspace(
            PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            None,
            runtime,
            true,
        );
        let pane_id = app.state.workspaces[ws_idx]
            .focused_pane_id()
            .expect("attached workspace has a pane");
        let public_pane_id = app
            .public_pane_id(ws_idx, pane_id)
            .expect("peer-backed pane has a public id");

        // The peer's report is the only thing that makes this pane an agent
        // here, so the whole chain runs rather than the terminal being poked.
        let detections = app.handle_peer_panes_updated(
            &PeerHandle::new("beta"),
            &[peer_pane_info(
                "w1:p1",
                "term_1",
                Some("claude"),
                crate::api::schema::AgentStatus::Idle,
            )],
        );
        for detection in detections {
            app.state.handle_app_event(detection);
        }

        // Answered locally this reads an empty screen, so an agent the peer
        // reports has to reach the peer that can see it.
        assert!(app.request_targets_peer_pane(&agent_read_request(&public_pane_id)));
        assert!(!app.request_targets_peer_pane(&agent_read_request(&local_public)));
        assert!(!app.request_targets_peer_pane(&agent_read_request("w99:p9")));

        drop(app);
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn explaining_a_peer_backed_agent_asks_the_peer_that_ran_the_rules() {
        let socket = unique_socket_path("peer-agent-explain-gate");
        let _endpoint = fake_peer_control_endpoint(&socket);
        let mut app = test_app();

        // A local agent pane, whose rules did run here, to prove the gate reads
        // the runtime rather than routing every agent away once a peer has one.
        app.create_workspace();
        let local_ws = app.state.workspaces.len() - 1;
        let local_pane = app.state.workspaces[local_ws]
            .focused_pane_id()
            .expect("local workspace has a pane");
        let local_public = app
            .public_pane_id(local_ws, local_pane)
            .expect("local pane has a public id");
        let local_terminal = app.state.workspaces[local_ws]
            .terminal_id(local_pane)
            .expect("local pane has a terminal")
            .clone();
        app.state
            .terminals
            .get_mut(&local_terminal)
            .expect("local terminal")
            .set_detected_state(
                Some(crate::detect::Agent::Claude),
                crate::detect::AgentState::Idle,
            );

        let runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");
        let ws_idx = app.create_attached_workspace(
            PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            None,
            runtime,
            true,
        );
        let pane_id = app.state.workspaces[ws_idx]
            .focused_pane_id()
            .expect("attached workspace has a pane");
        let public_pane_id = app
            .public_pane_id(ws_idx, pane_id)
            .expect("peer-backed pane has a public id");

        let detections = app.handle_peer_panes_updated(
            &PeerHandle::new("beta"),
            &[peer_pane_info(
                "w1:p1",
                "term_1",
                Some("claude"),
                crate::api::schema::AgentStatus::Working,
            )],
        );
        for detection in detections {
            app.state.handle_app_event(detection);
        }

        // The rules that decided this pane's state ran on the peer, so the
        // explanation has to come from there.
        assert!(app.request_targets_peer_pane(&agent_explain_request(&public_pane_id)));
        assert!(!app.request_targets_peer_pane(&agent_explain_request(&local_public)));
        assert!(!app.request_targets_peer_pane(&agent_explain_request("w99:p9")));

        // And the local handler never fabricates one behind the gate's back: it
        // names the peer instead of reporting every rule unmatched.
        let response = app.handle_agent_explain(
            "bypass".into(),
            crate::api::schema::AgentTarget {
                target: public_pane_id,
            },
        );
        let response: serde_json::Value = serde_json::from_str(&response).expect("a json response");
        assert_eq!(response["error"]["code"], "agent_explain_unavailable");
        assert!(
            response["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("beta")),
            "the refusal must name the peer: {response}"
        );

        drop(app);
        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn explain_response_is_stamped_with_the_peer_that_answered() {
        // What a peer returns for agent.explain: its own request id, its own
        // manifest, and its own rule ids — none of which name a pane.
        let mut value = serde_json::json!({
            "id": "peer:forward",
            "result": {
                "explain": {
                    "agent": "claude",
                    "state": "working",
                    "manifest_source": "bundled",
                    "manifest_version": "3",
                    "matched_rule": { "id": "live_working", "region": "bottom" },
                    "evaluated_rules": [{ "id": "live_working", "matched": true }]
                }
            }
        });
        rewrite_forwarded_explain(&mut value, "caller-42", &PeerHandle::new("beta"), "w1:p1");

        assert_eq!(value["id"], "caller-42");
        // The reasoning is the peer's and is reported verbatim.
        assert_eq!(value["result"]["explain"]["state"], "working");
        assert_eq!(
            value["result"]["explain"]["matched_rule"]["id"],
            "live_working"
        );
        // Whose manifest and rule ids these are is the part that would otherwise
        // be missing, and the part that sends someone to the wrong config file.
        assert_eq!(value["result"]["explain"]["peer"], "beta");
        assert_eq!(value["result"]["explain"]["peer_pane_id"], "w1:p1");
    }

    #[test]
    fn a_peer_refusing_to_explain_says_whose_pane_it_refused() {
        // The peer's message names the peer's own pane id, which means nothing
        // on this side until it says whose it is.
        let mut value = serde_json::json!({
            "id": "peer:forward",
            "error": {
                "code": "agent_explain_unavailable",
                "message": "agent target w1:p1 does not have a detected agent label"
            }
        });
        rewrite_forwarded_explain(&mut value, "caller-42", &PeerHandle::new("beta"), "w1:p1");

        assert_eq!(value["id"], "caller-42");
        assert_eq!(value["error"]["code"], "agent_explain_unavailable");
        assert_eq!(
            value["error"]["message"],
            "peer 'beta': agent target w1:p1 does not have a detected agent label"
        );
    }

    #[test]
    fn a_done_agent_reads_as_idle_because_seen_is_a_local_fact() {
        use crate::api::schema::AgentStatus;
        use crate::detect::AgentState;

        assert_eq!(agent_state_from_status(AgentStatus::Done), AgentState::Idle);
        assert_eq!(agent_state_from_status(AgentStatus::Idle), AgentState::Idle);
        assert_eq!(
            agent_state_from_status(AgentStatus::Working),
            AgentState::Working
        );
        assert_eq!(
            agent_state_from_status(AgentStatus::Blocked),
            AgentState::Blocked
        );
        assert_eq!(
            agent_state_from_status(AgentStatus::Unknown),
            AgentState::Unknown
        );
    }

    #[test]
    fn failed_cleanups_are_counted_on_the_peer_they_belong_to() {
        let mut peers = crate::app::peers::PeerRegistryState::default();
        let beta = PeerHandle::new("beta");
        peers
            .add(beta.clone(), PeerTarget::SocketPath("/tmp/b.sock".into()))
            .expect("add peer");

        assert!(peers.record_failed_pane_cleanup(&beta, pending_cleanup("w1:p1", Some("inst-a"))));
        assert!(peers.record_failed_pane_cleanup(&beta, pending_cleanup("w1:p2", Some("inst-a"))));
        // A peer that is already gone has nowhere to record it, which is the
        // race a removal loses.
        assert!(!peers.record_failed_pane_cleanup(
            &PeerHandle::new("gone"),
            pending_cleanup("w1:p3", Some("inst-a"))
        ));

        let peer = peers.get(&beta).expect("peer still configured");
        assert_eq!(peer.failed_pane_cleanups(), 2);
        assert_eq!(peer_info(peer).failed_pane_cleanups, 2);
    }

    fn pending_cleanup(
        peer_pane_id: &str,
        expected_instance: Option<&str>,
    ) -> crate::app::peers::PendingPaneCleanup {
        crate::app::peers::PendingPaneCleanup {
            peer_pane_id: peer_pane_id.to_string(),
            expected_instance: expected_instance.map(str::to_string),
            reason: "peer is disconnected".to_string(),
        }
    }

    /// B3: the pane id is what makes the leak undoable, so a failed cleanup has
    /// to keep it rather than only counting that one happened.
    #[test]
    fn a_failed_cleanup_comes_back_when_the_same_server_returns() {
        let mut peers = crate::app::peers::PeerRegistryState::default();
        let beta = PeerHandle::new("beta");
        peers
            .add(beta.clone(), PeerTarget::SocketPath("/tmp/b.sock".into()))
            .expect("add peer");
        peers.record_failed_pane_cleanup(&beta, pending_cleanup("w1:p1", Some("inst-a")));

        let retryable = peers.take_retryable_pane_cleanups(&beta, "inst-a");

        assert_eq!(retryable.len(), 1);
        assert_eq!(retryable[0].peer_pane_id, "w1:p1");
        assert_eq!(
            peers.get(&beta).expect("peer").failed_pane_cleanups(),
            0,
            "a record taken for retry is no longer outstanding"
        );
    }

    /// The id names a pane on the server that issued it. A replacement server
    /// answering at the same address would apply it to something else, so the
    /// record is kept to be reported rather than acted on.
    #[test]
    fn a_failed_cleanup_is_never_retried_against_a_replacement_server() {
        let mut peers = crate::app::peers::PeerRegistryState::default();
        let beta = PeerHandle::new("beta");
        peers
            .add(beta.clone(), PeerTarget::SocketPath("/tmp/b.sock".into()))
            .expect("add peer");
        peers.record_failed_pane_cleanup(&beta, pending_cleanup("w1:p1", Some("inst-a")));

        assert!(
            peers
                .take_retryable_pane_cleanups(&beta, "inst-b")
                .is_empty(),
            "a different server must never be sent the old server's pane ids"
        );
        assert_eq!(
            peers.get(&beta).expect("peer").failed_pane_cleanups(),
            1,
            "the leak is still real and still reported"
        );
    }

    /// A cleanup that failed before any identity was known cannot be matched
    /// against a returning server, so it is reported and never retried.
    #[test]
    fn a_cleanup_with_no_recorded_instance_is_reported_but_not_retried() {
        let mut peers = crate::app::peers::PeerRegistryState::default();
        let beta = PeerHandle::new("beta");
        peers
            .add(beta.clone(), PeerTarget::SocketPath("/tmp/b.sock".into()))
            .expect("add peer");
        peers.record_failed_pane_cleanup(&beta, pending_cleanup("w1:p1", None));

        assert!(peers
            .take_retryable_pane_cleanups(&beta, "inst-a")
            .is_empty());
        assert_eq!(peers.get(&beta).expect("peer").failed_pane_cleanups(), 1);
    }

    /// A retry that fails again describes the same leak. Counting it twice
    /// would make one stuck pane look like a growing pile.
    #[test]
    fn a_repeated_failure_for_one_pane_is_retained_once() {
        let mut peers = crate::app::peers::PeerRegistryState::default();
        let beta = PeerHandle::new("beta");
        peers
            .add(beta.clone(), PeerTarget::SocketPath("/tmp/b.sock".into()))
            .expect("add peer");

        peers.record_failed_pane_cleanup(&beta, pending_cleanup("w1:p1", Some("inst-a")));
        peers.record_failed_pane_cleanup(&beta, pending_cleanup("w1:p1", Some("inst-a")));

        assert_eq!(peers.get(&beta).expect("peer").failed_pane_cleanups(), 1);
    }

    #[test]
    fn retained_cleanups_stay_bounded_for_a_peer_that_stays_broken() {
        let mut peers = crate::app::peers::PeerRegistryState::default();
        let beta = PeerHandle::new("beta");
        peers
            .add(beta.clone(), PeerTarget::SocketPath("/tmp/b.sock".into()))
            .expect("add peer");

        for index in 0..500 {
            peers.record_failed_pane_cleanup(
                &beta,
                pending_cleanup(&format!("w1:p{index}"), Some("inst-a")),
            );
        }

        assert_eq!(peers.get(&beta).expect("peer").failed_pane_cleanups(), 64);
    }

    #[tokio::test]
    async fn splitting_a_local_pane_stays_local() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("main")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let workspace_id = app.state.workspaces[0].id.clone();

        assert!(!app.request_targets_peer_pane(&split_request(Some(workspace_id))));
        assert!(!app.request_targets_peer_pane(&split_request(None)));
        // A method that is not a split is never routed by this gate.
        assert!(
            !app.request_targets_peer_pane(&request(Method::WorkspaceList(EmptyParams::default())))
        );
    }

    #[test]
    fn peer_split_response_yields_the_pane_the_peer_created() {
        let value = serde_json::json!({
            "id": "peer:forward",
            "result": { "pane": { "pane_id": "w1:p2", "terminal_id": "t7" } }
        });
        assert_eq!(
            peer_split_pane_id(&value, &PeerHandle::new("beta")),
            Ok("w1:p2".to_string())
        );
    }

    #[test]
    fn peer_split_error_keeps_the_peers_own_code_and_message() {
        // The failure happened on the machine that owns the pane, so its
        // wording is what explains it.
        let value = serde_json::json!({
            "id": "peer:forward",
            "error": { "code": "pane_split_failed", "message": "no space to split" }
        });
        assert_eq!(
            peer_split_pane_id(&value, &PeerHandle::new("beta")),
            Err((
                "pane_split_failed".to_string(),
                "no space to split".to_string()
            ))
        );
    }

    #[test]
    fn peer_split_response_without_a_pane_id_is_unavailable() {
        let value = serde_json::json!({ "id": "peer:forward", "result": {} });
        let err = peer_split_pane_id(&value, &PeerHandle::new("beta")).unwrap_err();
        assert_eq!(err.0, "unavailable");
        assert!(err.1.contains("beta"), "{}", err.1);
    }

    #[test]
    fn error_response_rewrites_the_id_without_touching_the_body() {
        // remap_ids is true for a rename, but an error carries no `result`, so
        // the workspace-id remap must simply find nothing to do.
        let mut value = serde_json::json!({
            "id": "peer:forward",
            "error": { "code": "workspace_not_found", "message": "workspace w1 not found" }
        });
        rewrite_forwarded_response(&mut value, "caller-42", INSTANCE, true);
        assert_eq!(value["id"], "caller-42");
        assert_eq!(value["error"]["code"], "workspace_not_found");
    }

    /// Opens a peer-backed workspace against a fake endpoint and returns the
    /// terminal behind its pane.
    async fn open_view(
        app: &mut App,
        handle: &PeerHandle,
        target: &str,
    ) -> crate::terminal::TerminalId {
        let ws_idx = open_peer_workspace(app, &peer_ws_id(target), Some(handle.as_str()), true)
            .await
            .expect("open a view onto the peer terminal");
        let pane_id = app.state.workspaces[ws_idx]
            .focused_pane_id()
            .expect("attached workspace has a pane");
        app.state.workspaces[ws_idx]
            .terminal_id(pane_id)
            .expect("pane has a terminal")
            .clone()
    }

    #[tokio::test]
    async fn a_dropped_view_reconnects_and_keeps_its_identity() {
        let api_socket = unique_socket_path("peer-reconnect");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint_serving(&control_socket, 2, 1);
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);

        let terminal_id = open_view(&mut app, &handle, "w1:p1").await;
        // A view onto a pane we asked the peer to spawn keeps its claim across
        // the reconnect, or closing it would stop closing the pane behind it.
        if let Some(remote) = app
            .terminal_runtimes
            .get_mut(&terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote_mut)
        {
            remote.mark_spawned_on_peer();
        }

        run_one_reconnect_cycle(&mut app, &terminal_id).await;

        let runtime = app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("the terminal keeps its slot across a reconnect");
        let remote = runtime.remote().expect("still a peer-backed view");
        assert!(remote.is_connected(), "the view must be live again");
        assert_eq!(remote.peer(), "beta");
        assert_eq!(remote.target(), "w1:p1");
        assert_eq!(
            runtime
                .spawned_peer_pane()
                .map(|pane| (pane.peer, pane.peer_pane_id)),
            Some(("beta", "w1:p1"))
        );
        assert!(remote.dead_reason().is_none());

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    #[tokio::test]
    async fn a_view_the_peer_keeps_refusing_stops_retrying() {
        let api_socket = unique_socket_path("peer-reconnect-refused");
        let control_socket = client_socket_for(&api_socket);
        // Every connection is accepted and dropped without a frame, which is
        // what a target that no longer exists on the peer looks like.
        let _endpoint = fake_peer_control_endpoint_serving(&control_socket, 8, 8);
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);
        let terminal_id = open_view(&mut app, &handle, "w1:p1").await;

        for _ in 0..4 {
            run_one_reconnect_cycle(&mut app, &terminal_id).await;
        }

        let remote = app
            .terminal_runtimes
            .get(&terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote)
            .expect("the view stays in place when it dies");
        assert!(
            remote.dead_reason().is_some(),
            "a view the peer keeps closing must stop retrying"
        );
        // Nothing else is dispatched once it is dead.
        app.reconcile_remote_terminal_views();
        assert!(app.event_rx.try_recv().is_err());

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    /// The pane behind this view exited on the peer, which now refuses the
    /// reattach with "not found". That answer is authoritative: the local view
    /// runs the ordinary pane-death path instead of sitting gray forever.
    #[tokio::test]
    async fn a_view_the_peer_reports_as_gone_closes_instead_of_going_gray() {
        let api_socket = unique_socket_path("peer-view-gone");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint_refusing(
            &control_socket,
            8,
            crate::protocol::ShutdownCode::TargetGone,
            "terminal session control failed: terminal target w1:p1 not found",
        );
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);
        let terminal_id = open_view(&mut app, &handle, "w1:p1").await;
        app.terminal_runtimes
            .get_mut(&terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote_mut)
            .expect("the view is remote")
            .mark_spawned_on_peer();
        assert_eq!(app.state.workspaces.len(), 1);

        run_reconnect_cycles(&mut app, &terminal_id, 4).await;

        assert!(
            app.terminal_runtimes.get(&terminal_id).is_none(),
            "a gone target retires the view rather than leaving it gray"
        );
        assert!(
            app.state.workspaces.is_empty(),
            "it was the workspace's only pane, so the workspace closed with it"
        );

        // The spawned claim was cleared before teardown: no doomed pane.close
        // round trip, no failed-cleanup event.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(app.event_rx.try_recv().is_err());

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    /// The timing half of the same behaviour: the peer says "not found" once
    /// and the very next sweep retires the view, rather than dispatching two
    /// more connects to hear it repeated. Those cost a full round trip each —
    /// 1.05s apiece on a 200ms link — with the pane gray the whole time.
    #[tokio::test]
    async fn one_gone_refusal_retires_the_view_without_another_attempt() {
        let api_socket = unique_socket_path("peer-view-gone-once");
        let control_socket = client_socket_for(&api_socket);
        // Exactly one connection is served: the one the open makes. Anything
        // that reconnects finds nothing listening and fails as transport, which
        // never retires a view — so this can only pass by acting on the first
        // refusal.
        let _endpoint = fake_peer_control_endpoint_refusing(
            &control_socket,
            1,
            crate::protocol::ShutdownCode::TargetGone,
            "terminal session control failed: terminal target w1:p1 not found",
        );
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);
        let terminal_id = open_view(&mut app, &handle, "w1:p1").await;
        assert_eq!(app.state.workspaces.len(), 1);

        wait_for_disconnect(&app, &terminal_id).await;
        app.reconcile_remote_terminal_views();

        assert!(
            app.terminal_runtimes.get(&terminal_id).is_none(),
            "the first authoritative refusal retires the view"
        );
        assert!(
            app.state.workspaces.is_empty(),
            "it was the workspace's only pane, so the workspace closed with it"
        );
        // No worker was dispatched: a reconnect attempt reports back as an
        // event, and there is none.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(app.event_rx.try_recv().is_err());

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    /// Same refusal in a tab with siblings: the pane closes, the rest of the
    /// workspace is untouched.
    #[tokio::test]
    async fn a_gone_view_closes_its_pane_and_leaves_the_workspace_standing() {
        let api_socket = unique_socket_path("peer-view-gone-sibling");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint_refusing(
            &control_socket,
            8,
            crate::protocol::ShutdownCode::TargetGone,
            "terminal session control failed: terminal target w1:p1 not found",
        );
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);
        let terminal_id = open_view(&mut app, &handle, "w1:p1").await;
        let (ws_idx, _) = app
            .pane_location_for_terminal(&terminal_id)
            .expect("the view opened in a workspace");
        app.state.workspaces[ws_idx].test_add_tab(Some("logs"));
        app.state.ensure_test_terminals();
        assert_eq!(app.state.workspaces[ws_idx].tabs.len(), 2);

        run_reconnect_cycles(&mut app, &terminal_id, 4).await;

        assert!(app.terminal_runtimes.get(&terminal_id).is_none());
        assert_eq!(
            app.state.workspaces.len(),
            1,
            "the workspace survives losing one of its tabs"
        );
        assert_eq!(app.state.workspaces[ws_idx].tabs.len(), 1);
        assert_eq!(
            app.state.workspaces[ws_idx].tabs[0].custom_name.as_deref(),
            Some("logs")
        );

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    /// A refusal that says "retry" is transient: the view keeps its pane and
    /// the gray dead presentation, because the target may well exist.
    #[tokio::test]
    async fn a_transient_refusal_keeps_the_view_gray() {
        let api_socket = unique_socket_path("peer-view-transient");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint_refusing(
            &control_socket,
            8,
            crate::protocol::ShutdownCode::TargetUnavailable,
            "terminal attach failed: terminal term_18d2 has a read in progress; retry",
        );
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);
        let terminal_id = open_view(&mut app, &handle, "w1:p1").await;

        run_reconnect_cycles(&mut app, &terminal_id, 4).await;

        let remote = app
            .terminal_runtimes
            .get(&terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote)
            .expect("a transient refusal keeps the view in place");
        assert!(remote.dead_reason().is_some());
        assert_eq!(app.state.workspaces.len(), 1);
        assert!(
            app.pane_location_for_terminal(&terminal_id).is_some(),
            "the pane is still there, showing its last frame"
        );

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    /// A view targeting an agent by name must not be retired when the agent
    /// exits: the name stops resolving while the pane behind it lives on, so
    /// "not found" is not proof the pane is gone.
    #[tokio::test]
    async fn a_gone_agent_name_keeps_its_view_gray() {
        let api_socket = unique_socket_path("peer-view-agent-gone");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint_refusing(
            &control_socket,
            8,
            crate::protocol::ShutdownCode::TargetGone,
            "terminal session control failed: terminal target claude not found",
        );
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);
        let terminal_id = open_view(&mut app, &handle, "claude").await;

        run_reconnect_cycles(&mut app, &terminal_id, 4).await;

        let remote = app
            .terminal_runtimes
            .get(&terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote)
            .expect("an agent-name target keeps its gray view");
        assert_eq!(
            remote.dead_reason(),
            Some("terminal session control failed: terminal target claude not found")
        );
        assert!(app.pane_location_for_terminal(&terminal_id).is_some());

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    #[tokio::test]
    async fn a_view_waits_for_its_peer_rather_than_dialing_a_socket_that_cannot_answer() {
        let api_socket = unique_socket_path("peer-reconnect-waits");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint_serving(&control_socket, 1, 1);
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);
        let terminal_id = open_view(&mut app, &handle, "w1:p1").await;

        app.state.peers.set_connection(
            &handle,
            PeerConnectionState::Reconnecting {
                attempt: 2,
                message: "peer event stream closed".into(),
            },
        );
        wait_for_disconnect(&app, &terminal_id).await;
        app.reconcile_remote_terminal_views();
        assert!(
            app.event_rx.try_recv().is_err(),
            "no attempt may be dispatched while the peer itself is down"
        );

        let remote = app
            .terminal_runtimes
            .get(&terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote)
            .expect("the view stays in place while its peer is down");
        assert!(!remote.is_connected());
        assert!(
            remote.dead_reason().is_none(),
            "a peer that is reconnecting will be back; its views must not give up"
        );

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    /// Every `pane.updated` the hub has seen, newest last.
    fn pane_updates(app: &App) -> Vec<crate::api::schema::PaneInfo> {
        app.event_hub
            .events_after(0)
            .into_iter()
            .filter_map(|(_, envelope)| match envelope.data {
                crate::api::schema::EventData::PaneUpdated { pane } => Some(pane),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_live_view_reports_itself_connected_and_a_local_pane_reports_nothing() {
        use crate::api::schema::PeerViewState;

        let api_socket = unique_socket_path("peer-view-live");
        let control_socket = client_socket_for(&api_socket);
        // Held open rather than hung up: this is the connected case.
        let _endpoint = fake_peer_control_endpoint(&control_socket);
        let mut app = test_app();
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("local"));
        app.state.ensure_test_terminals();
        let local_idx = app.state.workspaces.len() - 1;
        let local_pane = app.state.workspaces[local_idx].tabs[0].root_pane;

        let handle = connected_peer(&mut app, "beta", &api_socket);
        let terminal_id = open_view(&mut app, &handle, "w1:p1").await;
        let (ws_idx, pane_id) = app
            .pane_location_for_terminal(&terminal_id)
            .expect("the view has a pane");

        let view = app
            .pane_info(ws_idx, pane_id)
            .expect("a view reports as a pane")
            .peer_view
            .expect("a peer-backed pane always reports its view");
        assert_eq!(view.state, PeerViewState::Connected);
        assert_eq!(view.reason, None);
        // Absent rather than connected: a local pane has no connection to
        // describe, and a client must be able to tell the two apart.
        assert_eq!(
            app.pane_info(local_idx, local_pane)
                .expect("a local pane reports too")
                .peer_view,
            None
        );

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    #[tokio::test]
    async fn a_view_that_stops_retrying_reports_itself_disconnected_once() {
        use crate::api::schema::PeerViewState;

        let api_socket = unique_socket_path("peer-view-dead");
        let control_socket = client_socket_for(&api_socket);
        // Accepted and dropped without a frame every time, which is what a pane
        // the peer has already closed looks like from here.
        let _endpoint = fake_peer_control_endpoint_serving(&control_socket, 8, 8);
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);
        let terminal_id = open_view(&mut app, &handle, "w1:p1").await;
        let (ws_idx, pane_id) = app
            .pane_location_for_terminal(&terminal_id)
            .expect("the view has a pane");

        for _ in 0..4 {
            run_one_reconnect_cycle(&mut app, &terminal_id).await;
        }

        let view = app
            .pane_info(ws_idx, pane_id)
            .expect("a dead view keeps its pane")
            .peer_view
            .expect("a peer-backed pane always reports its view");
        assert_eq!(view.state, PeerViewState::Disconnected);
        assert!(
            view.reason.is_some(),
            "giving up has to say what it gave up on"
        );

        let announced: Vec<PeerViewState> = pane_updates(&app)
            .into_iter()
            .filter_map(|pane| pane.peer_view.map(|view| view.state))
            .collect();
        assert_eq!(
            announced.last(),
            Some(&PeerViewState::Disconnected),
            "the pane the peer closed must be announced as dead: {announced:?}"
        );
        let dead_announcements = announced
            .iter()
            .filter(|state| **state == PeerViewState::Disconnected)
            .count();
        assert_eq!(
            dead_announcements, 1,
            "a view dies once; further sweeps must stay quiet: {announced:?}"
        );

        // And it stays quiet: nothing about the pane changes by looking again.
        let before = pane_updates(&app).len();
        app.reconcile_remote_terminal_views();
        app.reconcile_remote_terminal_views();
        assert_eq!(pane_updates(&app).len(), before);

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    #[tokio::test]
    async fn a_view_whose_peer_was_removed_stops_retrying() {
        let api_socket = unique_socket_path("peer-reconnect-removed");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint_serving(&control_socket, 1, 1);
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);
        let terminal_id = open_view(&mut app, &handle, "w1:p1").await;

        app.state.peers.remove(&handle);
        run_one_reconnect_cycle(&mut app, &terminal_id).await;

        let remote = app
            .terminal_runtimes
            .get(&terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote)
            .expect("the view stays in place when it dies");
        let reason = remote.dead_reason().expect("an unconfigured peer is final");
        assert!(reason.contains("beta"), "{reason}");

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    #[tokio::test]
    async fn opening_a_target_this_server_already_views_returns_the_same_view() {
        let api_socket = unique_socket_path("peer-view-dedup");
        let control_socket = client_socket_for(&api_socket);
        // One connection only: a second open must not dial the peer again.
        let _endpoint = fake_peer_control_endpoint_serving(&control_socket, 1, 0);
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);

        let first = open_peer_workspace(&mut app, &peer_ws_id("w1:p1"), Some("beta"), true)
            .await
            .expect("open the view");
        let workspaces = app.state.workspaces.len();
        let second = open_peer_workspace(&mut app, &peer_ws_id("w1:p1"), Some("beta"), true)
            .await
            .expect("reopening returns the existing view");

        assert_eq!(first, second);
        assert_eq!(
            app.state.workspaces.len(),
            workspaces,
            "a second view onto one terminal would fight the first for the peer's attach"
        );
        let _ = handle;

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    /// The regression this guards: `set_identity` invalidated the enumeration
    /// and nothing else. A view's target is a peer-local id kept verbatim across
    /// reconnects, so a peer restarted with a fresh session dir would have the
    /// view silently re-attach to an unrelated `w1:p1` — still labelled with the
    /// old agent — and closing it would forward `pane.close` for that id and
    /// destroy a pane nobody asked about.
    #[tokio::test]
    async fn a_view_whose_server_was_replaced_stops_instead_of_re_attaching() {
        let api_socket = unique_socket_path("peer-identity-replaced");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint(&control_socket);
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);

        let terminal_id = open_view(&mut app, &handle, "w1:p1").await;
        // A split view: this is the one whose stale claim could kill a pane.
        app.terminal_runtimes
            .get_mut(&terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote_mut)
            .expect("the view is remote")
            .mark_spawned_on_peer();
        assert!(app
            .terminal_runtimes
            .get(&terminal_id)
            .and_then(crate::terminal::TerminalRuntime::spawned_peer_pane)
            .is_some());

        // beta restarts with a fresh session directory and reports a new id.
        app.handle_internal_event(crate::events::AppEvent::PeerConnectionChanged {
            handle: handle.clone(),
            connection: PeerConnectionState::Connected,
            identity: Some(crate::app::peers::PeerIdentity {
                instance_id: "ffffffffffffffffffffffffffffffff".to_string(),
                version: None,
                protocol: Some(crate::protocol::PROTOCOL_VERSION),
            }),
        });

        let remote = app
            .terminal_runtimes
            .get(&terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote)
            .expect("the view is still there, showing its last frame");
        assert_eq!(
            remote.dead_reason(),
            Some("the peer was replaced by a different server"),
            "a view onto a server that is gone must not reconnect to its replacement"
        );
        assert!(
            !remote.reconnect_due(Instant::now()),
            "a dead view schedules no attempts"
        );
        assert!(
            app.terminal_runtimes
                .get(&terminal_id)
                .and_then(crate::terminal::TerminalRuntime::spawned_peer_pane)
                .is_none(),
            "the claim names a pane on a server that is gone; closing the view \
             must not forward pane.close to the replacement"
        );

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    /// Opens a bare peer terminal and returns the id and the response.
    async fn open_peer_terminal(
        app: &mut App,
        target: &str,
    ) -> Result<crate::terminal::TerminalId, (String, String)> {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        app.start_peer_terminal_open(
            "test:peer:terminal".to_string(),
            crate::api::schema::PeerTerminalOpenParams {
                name: Some("beta".into()),
                target: target.to_string(),
                cols: 80,
                rows: 24,
                takeover: false,
            },
            respond_to,
        );
        if response_rx.try_recv().is_err() {
            let event = tokio::time::timeout(Duration::from_secs(5), app.event_rx.recv())
                .await
                .expect("terminal open reports back")
                .expect("event channel stays open");
            match event {
                crate::events::AppEvent::PeerViewOpenFinished(result) => {
                    app.handle_peer_view_open_finished(*result)
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        let response = response_rx.try_recv().expect("open answers once it lands");
        let value: serde_json::Value =
            serde_json::from_str(&response).expect("response is valid json");
        if let Some(error) = value.get("error") {
            return Err((
                error["code"].as_str().unwrap_or_default().to_string(),
                error["message"].as_str().unwrap_or_default().to_string(),
            ));
        }
        let terminal_id = value["result"]["terminal"]["terminal_id"]
            .as_str()
            .expect("a successful open names its terminal")
            .to_string();
        app.state
            .terminals
            .keys()
            .find(|known| known.to_string() == terminal_id)
            .cloned()
            .ok_or_else(|| {
                (
                    "not_found".to_string(),
                    format!("terminal {terminal_id} was not registered"),
                )
            })
    }

    /// The regression this guards: `peer.terminal.open` used to insert only into
    /// the runtime map, so the id it returned was in no terminal list. Nothing
    /// could attach to it and nothing could reap it, while the view reconnected
    /// forever and kept the peer polling its panes.
    #[tokio::test]
    async fn a_bare_peer_terminal_is_attachable_and_releasable() {
        let api_socket = unique_socket_path("peer-bare-terminal");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint_serving(&control_socket, 2, 0);
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);

        let terminal_id = open_peer_terminal(&mut app, "w1:p1")
            .await
            .expect("open a bare peer terminal");

        // Registered in both maps: the id is only usable if a client can find it.
        assert!(app.state.terminals.contains_key(&terminal_id));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());

        // Releasing it removes both, so nothing keeps reconnecting to the peer.
        let response = app.handle_api_request(request(Method::PeerTerminalClose(
            crate::api::schema::TerminalTarget {
                terminal_id: terminal_id.to_string(),
            },
        )));
        let value: serde_json::Value =
            serde_json::from_str(&response).expect("response is valid json");
        assert!(value.get("error").is_none(), "{response}");
        assert!(!app.state.terminals.contains_key(&terminal_id));
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());
        let _ = handle;

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    /// The other half of the same finding: the two open paths deduped against
    /// different sets, so a bare terminal opened first was invisible to a
    /// workspace open and the two views reclaimed each other's attach forever.
    #[tokio::test]
    async fn a_workspace_open_sees_a_bare_terminal_on_the_same_target() {
        let api_socket = unique_socket_path("peer-bare-then-workspace");
        let control_socket = client_socket_for(&api_socket);
        // One connection only: the workspace open must not dial the peer again.
        let _endpoint = fake_peer_control_endpoint_serving(&control_socket, 1, 0);
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);

        let terminal_id = open_peer_terminal(&mut app, "w1:p1")
            .await
            .expect("open a bare peer terminal");

        let (code, message) =
            open_peer_workspace(&mut app, &peer_ws_id("w1:p1"), Some("beta"), true)
                .await
                .expect_err("a second connection to the same peer terminal is refused");
        assert_eq!(code, "already_exists");
        assert!(
            message.contains(&terminal_id.to_string()),
            "the refusal should name the view that already holds it: {message}"
        );
        let _ = handle;

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    #[test]
    fn resolved_workspace_and_pane_opens_dedupe_on_completion() {
        let api_socket = unique_socket_path("peer-resolved-open-dedupe");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint_serving(&control_socket, 2, 0);
        let mut app = test_app();
        let handle = connected_peer(&mut app, "beta", &api_socket);

        let first = crate::terminal::RemoteTerminalRuntime::connect(
            &control_socket,
            "beta".into(),
            "w1:p1".into(),
            80,
            24,
            false,
        )
        .unwrap();
        let (first_tx, first_rx) = std::sync::mpsc::channel();
        app.handle_peer_view_open_finished(crate::events::PeerViewOpenResult {
            id: "workspace".into(),
            handle: handle.clone(),
            requested_target: peer_ws_id("w1"),
            started_target: "w1".into(),
            placement: crate::events::PeerViewPlacement::Workspace {
                peer_workspace: Some("w1".into()),
                label: None,
                focus: true,
                worktree: None,
            },
            result: Ok(Box::new(crate::events::PeerViewOpened {
                runtime: Box::new(first),
                local_target: "w1:p1".into(),
            })),
            respond_to: first_tx,
        });
        let first_response: serde_json::Value =
            serde_json::from_str(&first_rx.recv().unwrap()).unwrap();
        assert!(first_response.get("error").is_none());
        let terminal_id = app.state.workspaces[0].tabs[0]
            .terminal_id(app.state.workspaces[0].tabs[0].root_pane)
            .unwrap()
            .clone();

        let duplicate = crate::terminal::RemoteTerminalRuntime::connect(
            &control_socket,
            "beta".into(),
            "w1:p1".into(),
            80,
            24,
            false,
        )
        .unwrap();
        let (duplicate_tx, duplicate_rx) = std::sync::mpsc::channel();
        app.handle_peer_view_open_finished(crate::events::PeerViewOpenResult {
            id: "pane".into(),
            handle,
            requested_target: peer_ws_id("w1:p1"),
            started_target: "w1:p1".into(),
            placement: crate::events::PeerViewPlacement::Terminal {
                target: peer_ws_id("w1:p1"),
            },
            result: Ok(Box::new(crate::events::PeerViewOpened {
                runtime: Box::new(duplicate),
                local_target: "w1:p1".into(),
            })),
            respond_to: duplicate_tx,
        });

        let duplicate_response: serde_json::Value =
            serde_json::from_str(&duplicate_rx.recv().unwrap()).unwrap();
        assert_eq!(
            duplicate_response["result"]["terminal"]["terminal_id"],
            terminal_id.to_string()
        );
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.terminal_runtimes.len(), 1);

        drop(app);
        let _ = std::fs::remove_file(&control_socket);
    }

    #[test]
    fn peer_view_open_rejects_a_replacement_servers_terminal() {
        let api_socket = unique_socket_path("peer-open-replaced");
        let control_socket = client_socket_for(&api_socket);
        let _endpoint = fake_peer_control_endpoint(&control_socket);
        let mut expected = INSTANCE.as_bytes().to_vec();
        expected[0] = if expected[0] == b'0' { b'1' } else { b'0' };
        let expected = String::from_utf8(expected).unwrap();

        let error = super::forward::open_peer_view(
            &api_socket,
            &PeerHandle::new("beta"),
            "w1:p1".into(),
            &expected,
            false,
            80,
            24,
            false,
        )
        .expect_err("a terminal from another server must not be placed");
        assert_eq!(error.0, "server_replaced");

        let _ = std::fs::remove_file(&control_socket);
    }

    /// The test that would have caught `28fb2fdb`.
    ///
    /// Every routed operation has two entry points — the JSON API socket and an
    /// in-process UI action — and both are supposed to consult the same
    /// predicate before handling anything locally. Nothing compared them, so a
    /// `tab.create` in a peer-backed workspace reached the gate on one path and
    /// spawned a local pty on the other. This pins the predicate for both.
    #[test]
    fn both_entry_points_route_the_same_requests_to_a_peer() {
        // (description, request, expected routing)
        let cases: Vec<(&str, Request, bool)> = vec![
            (
                "rename on a peer id",
                request(Method::WorkspaceRename(WorkspaceRenameParams {
                    workspace_id: peer_ws_id("w1"),
                    label: "new".into(),
                })),
                true,
            ),
            (
                "rename on a local id",
                request(Method::WorkspaceRename(WorkspaceRenameParams {
                    workspace_id: "w1".into(),
                    label: "new".into(),
                })),
                false,
            ),
            (
                "close on a peer id",
                request(Method::WorkspaceClose(WorkspaceTarget {
                    workspace_id: peer_ws_id("w1"),
                })),
                true,
            ),
            (
                "close on a local id",
                request(Method::WorkspaceClose(WorkspaceTarget {
                    workspace_id: "w1".into(),
                })),
                false,
            ),
            (
                "focus on a peer id",
                request(Method::WorkspaceFocus(WorkspaceTarget {
                    workspace_id: peer_ws_id("w1"),
                })),
                true,
            ),
            (
                "focus on a local id",
                request(Method::WorkspaceFocus(WorkspaceTarget {
                    workspace_id: "w1".into(),
                })),
                false,
            ),
        ];

        for (what, request, expected) in cases {
            let app = test_app();

            // The socket path's gate, as `headless.rs` applies it.
            let socket_routes = app.request_targets_peer_workspace(&request);

            // The in-process path's gate, as `dispatch_runtime_mutation` applies
            // it: the same predicate against the same state.
            let ui_routes = app.request_targets_peer_workspace(&Request {
                id: "ui".into(),
                method: request.method.clone(),
            });

            assert_eq!(
                socket_routes, ui_routes,
                "{what}: the two entry points disagree about routing"
            );
            assert_eq!(socket_routes, expected, "{what}: routed the wrong way");
        }
    }

    /// A literal encoding of the remote arm's declared policy.
    ///
    /// `TerminalRuntime::Remote` documents three answers — forwarded, deferred to
    /// the JSON API, or deliberately absent — but nothing pinned which accessor
    /// gets which. F16 slipped through exactly there: a caller read
    /// `recent_text` for a remote pane, got `""`, and opened an editor on an
    /// empty file. Anything that reads as absent here needs a gate at its caller.
    #[tokio::test]
    async fn the_remote_runtime_answers_absent_where_it_says_it_does() {
        let socket = unique_socket_path("peer-capability-matrix");
        let _endpoint = fake_peer_control_endpoint(&socket);
        let mut runtime = crate::terminal::TerminalRuntime::connect_remote(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect to the fake peer terminal");

        // Absent: derived from a screen grid this side does not have.
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        assert!(runtime.cursor_state(area, true).is_none(), "cursor_state");
        assert!(runtime.wheel_routing().is_none(), "wheel_routing");
        assert!(runtime.input_state().is_none(), "input_state");
        let selection =
            crate::selection::Selection::anchor(crate::layout::PaneId::alloc(), 0, 0, None);
        assert!(
            runtime.extract_selection(&selection).is_none(),
            "extract_selection"
        );
        assert!(runtime.history_source().is_none(), "history_source");
        assert!(runtime.child_pid().is_none(), "child_pid");
        assert!(runtime.follow_cwd().is_none(), "follow_cwd");
        assert!(
            runtime.search_text_matches("anything", false).is_empty(),
            "search_text_matches"
        );

        // Read from the retained frame, so absent only until one arrives — not
        // absent by design like the group above. This peer sends no frame, so
        // both answer empty here; `terminal::remote` pins what they report once
        // a frame carrying them has landed.
        assert!(
            runtime.scroll_metrics().is_none(),
            "scroll_metrics: no frame has arrived"
        );
        assert!(
            runtime.visible_hyperlinks(area).is_empty(),
            "visible_hyperlinks: no frame has arrived"
        );

        // Answered by asking the peer, so empty here — every one of these has an
        // interception above this type, and a caller that reads them directly is
        // reading a blank screen rather than a remote one.
        assert!(runtime.visible_text().is_empty(), "visible_text");
        assert!(runtime.detection_text().is_empty(), "detection_text");
        assert!(
            runtime.recent_text(usize::MAX).is_empty(),
            "recent_text: the gate on its caller is what F16 was"
        );

        // Reported, not absent: these come from the peer's own metadata and are
        // what keeps a remote pane from reading as unlabeled next to a local one.
        //
        // `(rows, cols)`, and compared against a local runtime rather than a
        // literal: both kinds are reached through the same
        // `TerminalRuntime::current_size`, so the two answering in different
        // orders is the defect, not either order by itself. It did, until this
        // assertion.
        let local = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
        assert_eq!(runtime.current_size(), local.current_size(), "current_size");
        assert_eq!(
            runtime.current_size(),
            crate::terminal::TerminalSize::new(24, 80),
            "current_size"
        );

        // Answered `false`, and that is the safe answer rather than an
        // oversight: synchronized output is a VT mode the peer resolves inside
        // the frame it sends, so by the time a frame arrives here the update is
        // already whole. Claiming otherwise would hold a frame that is not
        // waiting on anything.
        assert!(
            !runtime.synchronized_output_active(),
            "synchronized_output_active"
        );

        runtime.remote_mut().expect("remote runtime").set_metadata(
            crate::terminal::RemotePaneMetadata {
                agent_osc_title: Some("Compiling".into()),
                agent_osc_progress: Some("42".into()),
                ..Default::default()
            },
        );
        assert_eq!(runtime.agent_osc_title(), "Compiling", "agent_osc_title");
        assert_eq!(runtime.agent_osc_progress(), "42", "agent_osc_progress");

        // `Clean` while no frame is waiting, `Fallback` once one is. There is no
        // local VT state to diff, so a changed remote pane can only ask for a
        // full redraw — but an unchanged one must not, or one peer-backed pane
        // costs every local pane in the tab its row patch on every tick.
        assert!(
            matches!(
                runtime.collect_dirty_patch(80, 24),
                crate::pane::TerminalDirtyPatchOutcome::Fallback
            ),
            "collect_dirty_patch: a freshly connected view has never been drawn"
        );
        // What the event loop does once it has rendered: the consuming read is
        // its own, which is why the patch path needs a peeking one.
        assert!(runtime.take_remote_frame_dirty());
        assert!(
            matches!(
                runtime.collect_dirty_patch(80, 24),
                crate::pane::TerminalDirtyPatchOutcome::Clean
            ),
            "collect_dirty_patch: nothing arrived since that render"
        );

        runtime.shutdown();
        let _ = std::fs::remove_file(&socket);
    }

    /// Characterization: the ordinary path this must keep working.
    #[tokio::test]
    async fn a_reconnect_result_replaces_the_view_it_belongs_to() {
        let socket = unique_socket_path("reconnect-installs");
        let _endpoint = fake_peer_control_endpoint_serving(&socket, 2, 0);
        let mut app = test_app();

        let original = crate::terminal::RemoteTerminalRuntime::connect(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect the original view");
        let terminal_id = app
            .register_peer_terminal(crate::terminal::TerminalRuntime::Remote(Box::new(original)));

        let reconnected = crate::terminal::RemoteTerminalRuntime::connect(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect the replacement view");

        app.handle_peer_view_reconnected(crate::events::PeerViewReconnectResult {
            terminal_id: terminal_id.clone(),
            result: Ok(Box::new(reconnected)),
        });

        let installed = app
            .terminal_runtimes
            .get(&terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote)
            .expect("the view is still there");
        assert!(installed.dead_reason().is_none());
        assert!(installed.is_connected(), "the reconnected view is live");

        let _ = std::fs::remove_file(&socket);
    }

    /// B1: a reconnect that succeeded against the peer this view *had* must not
    /// be installed once that peer has been replaced by a different server.
    ///
    /// The worker checks the instance when it connects, but the answer travels
    /// back as an event, and the replacement can be processed in the gap. The
    /// old success then lands on a slot that was deliberately marked dead, and
    /// resurrects a view onto a server that is gone — one that still displays
    /// and still accepts input.
    #[tokio::test]
    async fn a_reconnect_that_finished_against_a_replaced_peer_is_discarded() {
        let socket = unique_socket_path("reconnect-stale-instance");
        let _endpoint = fake_peer_control_endpoint_serving(&socket, 2, 0);
        let mut app = test_app();

        let original = crate::terminal::RemoteTerminalRuntime::connect(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect the original view");
        let terminal_id = app
            .register_peer_terminal(crate::terminal::TerminalRuntime::Remote(Box::new(original)));

        // The reconnect worker got its connection through before the peer was
        // replaced, so it carries a perfectly good runtime onto the old server.
        let in_flight = crate::terminal::RemoteTerminalRuntime::connect(
            &socket,
            "beta".to_string(),
            "w1:p1".to_string(),
            80,
            24,
            false,
        )
        .expect("connect the in-flight view");

        // A different server answers on the peer's address, which abandons the
        // views bound to the old one.
        app.abandon_views_of_replaced_peer(&PeerHandle::new("beta"), "a-different-instance-id");
        assert!(
            app.terminal_runtimes
                .get(&terminal_id)
                .and_then(crate::terminal::TerminalRuntime::remote)
                .expect("the view is still registered")
                .dead_reason()
                .is_some(),
            "replacement marks the view dead"
        );

        // Only now does the worker's answer arrive.
        app.handle_peer_view_reconnected(crate::events::PeerViewReconnectResult {
            terminal_id: terminal_id.clone(),
            result: Ok(Box::new(in_flight)),
        });

        let installed = app
            .terminal_runtimes
            .get(&terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote)
            .expect("the view is still registered");
        assert!(
            installed.dead_reason().is_some(),
            "a view abandoned for peer replacement must stay dead, not be \
             resurrected onto the server that was replaced"
        );

        let _ = std::fs::remove_file(&socket);
    }
}
