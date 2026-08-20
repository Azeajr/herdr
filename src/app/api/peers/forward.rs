//! Sending work to a peer, off the event loop.
//!
//! Depends on `resolve` to find the peer and on `rewrite` to restate its reply.
//!
//! Everything here follows one shape: decide on the event loop, act on a worker.
//! The decision needs local state and must be instant; the action is a round
//! trip to another machine and, for an ssh peer, the start of an ssh child. The
//! loop must never be the thread that waits for either.

use super::*;

impl App {
    /// Whether a request mutates a peer-owned workspace and so must be forwarded
    /// to that peer rather than handled locally.
    ///
    /// A peer-namespaced id has no local index, so these would otherwise fail as
    /// "not found". Only operations that mutate the peer's own session belong
    /// here; `workspace.focus` on a peer id opens a local view and stays local.
    /// Whether this request has to be handled by asking a peer, rather than
    /// answered from local state.
    ///
    /// Rename and close qualify by carrying a peer-namespaced workspace id.
    /// `peer.workspace.create` always qualifies: it names the peer directly, so
    /// there is no id to test.
    ///
    /// `tab.create` qualifies on a different basis, and the difference matters.
    /// The arms above address a workspace the peer *owns*, by its namespaced id.
    /// A tab is created in a workspace this server holds a *view* of, whose id
    /// is local and never a peer id — so the test is what the workspace is
    /// attached to, not what it is called. Both rules are needed; neither
    /// subsumes the other.
    ///
    /// The three open arms qualify on a third basis again: nothing about them is
    /// forwarded, but every one of them ends in a handshake with another machine
    /// — for an ssh peer, the start of an ssh child behind a local socket that
    /// accepts instantly — and the event loop must not be the thread that waits
    /// for it. `workspace.focus` on a peer id is one of them because it opens a
    /// view rather than focusing a local index.
    ///
    /// The `worktree.*` arms qualify on the `tab.create` basis — a local id
    /// whose workspace is attached to a peer — but for a sharper reason than the
    /// others. A peer view's cwd is a path on the other machine, so running
    /// `git worktree` here either fails, or, when the same path exists locally
    /// too, quietly succeeds against the wrong host's repo.
    pub(crate) fn request_targets_peer_workspace(&self, request: &Request) -> bool {
        match &request.method {
            Method::WorkspaceRename(params) => crate::app::peers::is_peer_id(&params.workspace_id),
            Method::WorkspaceClose(target) => crate::app::peers::is_peer_id(&target.workspace_id),
            Method::PeerWorkspaceCreate(_) => true,
            Method::PeerWorkspaceOpen(_) | Method::PeerTerminalOpen(_) => true,
            Method::WorkspaceFocus(target) => crate::app::peers::is_peer_id(&target.workspace_id),
            Method::TabCreate(params) => self
                .workspace_index_for_tab_create(params.workspace_id.as_deref())
                .and_then(|ws_idx| self.state.workspaces.get(ws_idx))
                .is_some_and(|ws| ws.peer.is_some()),
            Method::WorktreeList(params) => self.worktree_request_targets_peer(
                params.workspace_id.as_deref(),
                params.cwd.as_deref(),
            ),
            Method::WorktreeCreate(params) => self.worktree_request_targets_peer(
                params.workspace_id.as_deref(),
                params.cwd.as_deref(),
            ),
            Method::WorktreeOpen(params) => self.worktree_request_targets_peer(
                params.workspace_id.as_deref(),
                params.cwd.as_deref(),
            ),
            Method::WorktreeRemove(params) => {
                self.worktree_request_targets_peer(Some(&params.workspace_id), None)
            }
            _ => false,
        }
    }

    /// Forwards a `workspace.rename`/`workspace.close` that targets a peer-owned
    /// id to the owning peer, off the event loop.
    ///
    /// The forward is deferred like `worktree.create`: a dead or slow peer must
    /// never stall the app event loop, and unlike opening a local view there is
    /// no local state to mutate on completion — the peer's own event stream
    /// drives A's re-enumeration — so the worker answers the client directly.
    ///
    /// Returns whether any local UI changed (always `false`: nothing local
    /// changes at dispatch time). Only call after
    /// [`Self::request_targets_peer_workspace`] has confirmed the target.
    pub(crate) fn handle_deferred_peer_workspace_api_request(
        &mut self,
        request: Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        // Creating names a peer rather than one of its workspaces, so it has no
        // id to resolve and takes its own path.
        if let Method::PeerWorkspaceCreate(params) = request.method {
            self.start_peer_workspace_create(request.id, params, respond_to);
            return false;
        }
        // A tab is created inside a view, so the peer is found through what the
        // local workspace is attached to rather than through a peer id.
        if let Method::TabCreate(params) = request.method {
            self.start_peer_tab_create(request.id, params, respond_to);
            return false;
        }
        // Opening is not forwarded at all: it connects here. It is deferred
        // because connecting is the unbounded step, not because a peer answers.
        if let Method::PeerWorkspaceOpen(params) = request.method {
            self.handle_deferred_peer_workspace_open(request.id, params, respond_to);
            return false;
        }
        if let Method::PeerTerminalOpen(params) = request.method {
            self.start_peer_terminal_open(request.id, params, respond_to);
            return false;
        }
        // Focusing a peer-owned id has no local index to focus: it opens a view
        // of that workspace, which is the same deferred open by another name.
        if let Method::WorkspaceFocus(target) = request.method {
            self.start_peer_workspace_open(
                request.id,
                target.workspace_id,
                None,
                None,
                true,
                false,
                None,
                respond_to,
            );
            return false;
        }
        // A worktree action inside a view runs where the checkout is. Each is a
        // peer round trip, and creating or opening chains a view onto whatever
        // the peer made, so none of them can be answered here.
        if let Method::WorktreeList(params) = request.method {
            self.start_peer_worktree_list(request.id, params, respond_to);
            return false;
        }
        if let Method::WorktreeCreate(params) = request.method {
            self.start_peer_worktree_create(request.id, params, respond_to);
            return false;
        }
        if let Method::WorktreeOpen(params) = request.method {
            self.start_peer_worktree_open(request.id, params, respond_to);
            return false;
        }
        if let Method::WorktreeRemove(params) = request.method {
            self.start_peer_worktree_remove(request.id, params, respond_to);
            return false;
        }

        let target_id = match &request.method {
            Method::WorkspaceRename(params) => params.workspace_id.clone(),
            Method::WorkspaceClose(target) => target.workspace_id.clone(),
            // Unreachable via the interception gate, but stay defensive: answer
            // rather than drop the request.
            _ => {
                let _ = respond_to.send(encode_error(
                    request.id,
                    "invalid_request",
                    "request does not target a peer workspace",
                ));
                return false;
            }
        };

        let PeerConnection {
            local_target,
            api_socket,
            instance_id,
            ..
        } = match self.resolve_peer_connection(&target_id, None) {
            Ok(resolved) => resolved,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(request.id, code, message));
                return false;
            }
        };

        // Rebuild the method against the peer's own local id, and note whether
        // its response carries workspace ids to re-namespace on the way back.
        let request_id = request.id;
        let (forwarded_method, remap_ids) = match request.method {
            Method::WorkspaceRename(mut params) => {
                params.workspace_id = local_target;
                (Method::WorkspaceRename(params), true)
            }
            Method::WorkspaceClose(mut target) => {
                target.workspace_id = local_target;
                (Method::WorkspaceClose(target), false)
            }
            _ => unreachable!("target id was extracted from one of these variants"),
        };

        std::thread::spawn(move || {
            let response = forward_workspace_request_to_peer(
                &api_socket,
                &request_id,
                forwarded_method,
                &instance_id,
                remap_ids,
            );
            let _ = respond_to.send(response);
        });
        false
    }

    /// Opens a peer's terminal as a local workspace.
    pub(in crate::app) fn handle_deferred_peer_workspace_open(
        &mut self,
        id: String,
        params: PeerWorkspaceOpenParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        self.start_peer_workspace_open(
            id,
            params.target,
            params.name.as_deref(),
            params.label,
            params.focus,
            params.takeover,
            None,
            respond_to,
        );
    }

    /// Starts opening a peer-backed terminal, off the event loop.
    ///
    /// The peer already renders the terminal, so the view holds one control
    /// connection and blits the cells it returns; no VT parser runs here.
    ///
    /// Deferred for the same reason every other peer open is: connecting is a
    /// handshake with another machine, and for an ssh peer it starts an ssh
    /// child behind a local socket that accepts instantly. Answering "already
    /// open" is the one thing this side can do without asking the peer, so that
    /// happens here and everything else moves to a worker.
    pub(in crate::app) fn start_peer_terminal_open(
        &mut self,
        id: String,
        params: PeerTerminalOpenParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let (handle, local_target) =
            match self.resolve_peer_target(&params.target, params.name.as_deref()) {
                Ok(resolved) => resolved,
                Err((code, message)) => {
                    let _ = respond_to.send(encode_error(id, code, message));
                    return;
                }
            };

        let cols = params.cols.max(1);
        let rows = params.rows.max(1);
        // One view per target: a second one would reclaim the first's attach on
        // the peer, and the first would reclaim it straight back. A view inside
        // a workspace counts as one, which is the half the two open paths used
        // to disagree about.
        if let Some(existing) = self.peer_view_already_open(handle.as_str(), &local_target, false) {
            let terminal_id = existing.terminal_id().clone();
            let crate::terminal::TerminalSize { rows, cols } = self
                .terminal_runtimes
                .get(&terminal_id)
                .map(|runtime| runtime.current_size())
                .unwrap_or(crate::terminal::TerminalSize::new(rows, cols));
            let _ = respond_to.send(encode_success(
                id,
                ResponseResult::PeerTerminal {
                    terminal: PeerTerminalInfo {
                        terminal_id: terminal_id.to_string(),
                        name: handle.as_str().to_string(),
                        target: params.target,
                        local_target,
                        cols,
                        rows,
                    },
                },
            ));
            return;
        }

        self.spawn_peer_view_open(
            id,
            handle,
            params.target.clone(),
            local_target,
            false,
            cols,
            rows,
            params.takeover,
            crate::events::PeerViewPlacement::Terminal {
                target: params.target,
            },
            respond_to,
        );
    }

    /// Starts opening a peer's terminal as a local workspace, off the event loop.
    ///
    /// The workspace is local — local id, local layout, local focus — but its
    /// pane renders frames the peer produced. Shared by `peer.workspace.open`,
    /// by `workspace.focus` on a peer-owned id, and by the view opened onto a
    /// workspace a peer just created for us.
    ///
    /// Idempotent per target: opening a peer terminal this server already views
    /// focuses the existing view. Two views onto one terminal would each reclaim
    /// the other's attach on the peer forever, since both prove the same
    /// instance owns it.
    ///
    /// `worktree` is set only when a routed `worktree.*` request is what opened
    /// this view. It changes nothing about the view — same connection, same
    /// dedupe, same placement — and only decides which shape the caller's answer
    /// takes, because a caller that asked for a worktree needs the checkout back
    /// alongside the workspace.
    #[allow(clippy::too_many_arguments)] // One open path, two callers' answers.
    pub(in crate::app) fn start_peer_workspace_open(
        &mut self,
        id: String,
        target: String,
        name: Option<&str>,
        label: Option<String>,
        focus: bool,
        takeover: bool,
        worktree: Option<Box<crate::events::PeerWorktreeAnswer>>,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        // Answer "already open" before requiring a live connection: a view that
        // exists is switchable even while its peer is down, and it is exactly
        // then that a user reaches for it.
        let (handle, local_target) = match self.resolve_peer_target(&target, name) {
            Ok(resolved) => resolved,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };
        let targets_workspace = self.peer_target_is_an_enumerated_workspace(&handle, &local_target);
        match self.peer_view_already_open(handle.as_str(), &local_target, targets_workspace) {
            Some(ExistingPeerView::InWorkspace { ws_idx, .. }) => {
                if focus {
                    self.state.switch_workspace(ws_idx);
                }
                // A worktree caller asked about a checkout, and this server
                // already had a view onto it — which is exactly what
                // `already_open` reports, so the answer says so rather than
                // repeating whatever the peer said about its own side.
                let response = match worktree {
                    Some(answer) => {
                        self.peer_worktree_view_response(id, ws_idx, answer.into_already_open())
                    }
                    None => encode_success(
                        id,
                        ResponseResult::WorkspaceInfo {
                            workspace: self.workspace_info(ws_idx),
                        },
                    ),
                };
                let _ = respond_to.send(response);
                return;
            }
            // A bare terminal already holds this target. Opening a workspace
            // around it would be a second connection to the same peer terminal,
            // and each would reclaim the other's attach forever.
            Some(ExistingPeerView::Bare { terminal_id }) => {
                let _ = respond_to.send(encode_error(
                    id,
                    "already_exists",
                    format!(
                        "'{local_target}' on peer '{handle}' is already open as terminal '{terminal_id}'"
                    ),
                ));
                return;
            }
            None => {}
        }

        // The peer's workspace id is captured before the worker's resolution
        // replaces it with a pane id: it is what later names this workspace on
        // the peer, and nothing else records it.
        let peer_workspace = if targets_workspace {
            Some(local_target.clone())
        } else {
            peer_workspace_of_pane_id(&local_target)
        };

        // Size the peer's terminal to what the local layout will give it. The
        // peer treats this connection as authoritative, so this is what the
        // remote program sees.
        let crate::terminal::TerminalSize { rows, cols } = self.state.estimate_pane_size();
        self.spawn_peer_view_open(
            id,
            handle,
            target,
            local_target,
            targets_workspace,
            cols,
            rows,
            takeover,
            crate::events::PeerViewPlacement::Workspace {
                peer_workspace,
                label,
                focus,
                worktree,
            },
            respond_to,
        );
    }

    /// Resolves the peer, claims the target, and hands the connect to a worker.
    ///
    /// Everything before the thread runs on the event loop and touches only
    /// local state; everything the peer has to answer runs on the worker.
    #[allow(clippy::too_many_arguments)] // One call shape for two open flavours.
    pub(super) fn spawn_peer_view_open(
        &mut self,
        id: String,
        handle: PeerHandle,
        requested_target: String,
        local_target: String,
        resolve_workspace: bool,
        cols: u16,
        rows: u16,
        takeover: bool,
        placement: crate::events::PeerViewPlacement,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let PeerConnection {
            api_socket,
            instance_id,
            ..
        } = match self.resolve_peer_connection(&local_target, Some(handle.as_str())) {
            Ok(connection) => connection,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };

        // The dedupe above cannot see an open that has not landed yet, and two
        // views onto one peer terminal reclaim each other's attach forever.
        let claim = (handle.as_str().to_string(), local_target.clone());
        if !self.peer_view_opens_in_flight.insert(claim) {
            let _ = respond_to.send(encode_error(
                id,
                "already_exists",
                format!("a view onto '{local_target}' on peer '{handle}' is already opening"),
            ));
            return;
        }

        let event_tx = self.event_tx.clone();
        let started_target = local_target.clone();
        std::thread::spawn(move || {
            let result = open_peer_view(
                &api_socket,
                &handle,
                local_target,
                &instance_id,
                resolve_workspace,
                cols,
                rows,
                takeover,
            );
            let _ = event_tx.blocking_send(crate::events::AppEvent::PeerViewOpenFinished(
                Box::new(crate::events::PeerViewOpenResult {
                    id,
                    handle,
                    requested_target,
                    started_target,
                    placement,
                    result,
                    respond_to,
                }),
            ));
        });
    }

    /// Whether a request operates on a pane that a peer owns.
    ///
    /// Unlike the workspace-level check this needs state: the target pane is
    /// named by a local id and only its runtime says whether it is remote.
    ///
    /// - `pane.split` on such a pane would spawn a local shell beside a remote
    ///   view, so it has to become a pane on the peer plus a second view onto it.
    /// - `pane.read` has no local screen to read. Answered locally it returns
    ///   empty text, which reads as "the pane is blank" rather than "the screen
    ///   is somewhere else" — so it goes to the peer, which has the screen.
    /// - `pane.text_query` searches and moves through that same peer-owned
    ///   buffer, including scrollback and soft-wrap boundaries.
    /// - `agent.read` is the same read reached by agent name instead of pane id.
    ///   A peer's agent is addressable here because its peer reports it, so the
    ///   read behind that name has to reach the same screen.
    /// - `agent.explain` replays detection rules against a screen. The rules
    ///   that decided this pane's state ran on the peer, so answering locally
    ///   reports every rule unmatched while the pane plainly shows an agent.
    pub(crate) fn request_targets_peer_pane(&self, request: &Request) -> bool {
        let target = match &request.method {
            Method::PaneSplit(params) => self.resolve_pane_split_target(params),
            Method::PaneRead(params) => self.parse_pane_id(&params.pane_id),
            Method::PaneReadRange(params) => self.parse_pane_id(&params.pane_id),
            Method::PaneTextQuery(params) => self.parse_pane_id(&params.pane_id),
            Method::AgentRead(params) => self
                .resolve_agent_target(&params.target)
                .ok()
                .map(|resolved| (resolved.ws_idx, resolved.pane_id)),
            Method::AgentExplain(target) => self
                .resolve_agent_target(&target.target)
                .ok()
                .map(|resolved| (resolved.ws_idx, resolved.pane_id)),
            _ => return false,
        };
        target.is_some_and(|(ws_idx, pane_id)| self.peer_pane_source(ws_idx, pane_id).is_some())
    }

    /// Dispatches a request that targets a peer-owned pane off the event loop.
    ///
    /// Returns whether local UI changed (always `false`: the split only lands
    /// when the peer answers). Only call after
    /// [`Self::request_targets_peer_pane`] has confirmed the target.
    pub(crate) fn handle_deferred_peer_pane_api_request(
        &mut self,
        request: Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        match request.method {
            Method::PaneSplit(params) => {
                self.start_peer_pane_split(request.id, params, respond_to);
            }
            Method::PaneRead(params) => {
                self.start_peer_pane_read(request.id, params, respond_to);
            }
            Method::PaneReadRange(params) => {
                self.start_peer_pane_read_range(request.id, params, respond_to);
            }
            Method::PaneTextQuery(params) => {
                self.start_peer_pane_text_query(request.id, params, respond_to);
            }
            Method::AgentRead(params) => {
                self.start_peer_agent_read(request.id, params, respond_to);
            }
            Method::AgentExplain(target) => {
                self.start_peer_agent_explain(request.id, target, respond_to);
            }
            // Unreachable via the interception gate, but stay defensive: answer
            // rather than drop the request.
            _ => {
                let _ = respond_to.send(encode_error(
                    request.id,
                    "invalid_request",
                    "request does not target a peer pane",
                ));
            }
        }
        false
    }

    /// Splits a peer-backed pane by spawning a pane on the peer and connecting a
    /// second view onto it.
    ///
    /// Both slow steps run on a worker thread: the peer round trip, and the
    /// connection to the new terminal, which for an ssh peer starts an ssh
    /// child. Neither may run on the app event loop. Local layout placement
    /// happens when [`crate::events::AppEvent::PeerPaneSplitFinished`] comes
    /// back.
    pub(super) fn start_peer_pane_split(
        &mut self,
        id: String,
        params: PaneSplitParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let Some((ws_idx, target_pane)) = self.resolve_pane_split_target(&params) else {
            let _ = respond_to.send(encode_error(id, "pane_not_found", "pane not found"));
            return;
        };
        let Some((handle, peer_pane_id)) = self.peer_pane_source(ws_idx, target_pane) else {
            let _ = respond_to.send(encode_error(
                id,
                "pane_not_found",
                "pane is not backed by a peer",
            ));
            return;
        };
        if let Some((code, message)) = self.peer_pane_server_mismatch(ws_idx, target_pane) {
            let _ = respond_to.send(encode_error(id, code, message));
            return;
        }
        // Validate the launch environment against local rules before sending it
        // on, so a bad key fails here rather than on the peer.
        let env = match crate::app::api::env::normalize_launch_env(params.env) {
            Ok(env) => env.into_iter().collect(),
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, &code, message));
                return;
            }
        };
        let PeerConnection {
            local_target,
            api_socket,
            instance_id,
        } = match self.resolve_peer_connection(&peer_pane_id, Some(handle.as_str())) {
            Ok(connection) => connection,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };

        let direction = match params.direction {
            crate::api::schema::SplitDirection::Right => ratatui::layout::Direction::Horizontal,
            crate::api::schema::SplitDirection::Down => ratatui::layout::Direction::Vertical,
        };
        // The peer decides where its own new pane goes and what cwd it
        // inherits; only the direction carries over, and focus stays local so
        // splitting here never moves the focus of whoever is using the peer.
        let peer_request = Method::PaneSplit(PaneSplitParams {
            workspace_id: None,
            target_pane_id: Some(local_target),
            direction: params.direction,
            right_click: params.right_click,
            ratio: None,
            cwd: params.cwd,
            focus: false,
            env,
            // The pane exists to back a view here, so the peer records it as
            // ours and can report it once we stop attaching to it.
            owner_instance_id: crate::instance_id::active(),
        });

        let workspace_id = self.state.workspaces[ws_idx].id.clone();
        let crate::terminal::TerminalSize { rows, cols } = self.state.estimate_pane_size();
        let client_socket = client_socket_for(&api_socket);
        let ratio = params.ratio;
        let focus = params.focus;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result = spawn_peer_split_view(
                &api_socket,
                &client_socket,
                peer_request,
                &handle,
                &instance_id,
                cols,
                rows,
            );
            let _ = event_tx.blocking_send(crate::events::AppEvent::PeerPaneSplitFinished(
                Box::new(crate::events::PeerPaneSplitResult {
                    id,
                    workspace_id,
                    target_pane,
                    direction,
                    ratio,
                    focus,
                    result,
                    respond_to,
                }),
            ));
        });
    }

    /// Asks a peer to create a workspace, then opens a local view onto it.
    ///
    /// Deferred for the same reason the split is: this is a peer round trip
    /// followed by a `connect_remote` that starts an ssh child for an ssh peer,
    /// and neither belongs on the event loop.
    ///
    /// Only the round trip runs on the worker. Opening the view happens back on
    /// the loop in [`Self::handle_peer_workspace_create_finished`], because it
    /// creates a local workspace and nothing off the loop may touch that.
    pub(super) fn start_peer_workspace_create(
        &mut self,
        id: String,
        params: crate::api::schema::PeerWorkspaceCreateParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let handle = PeerHandle::new(params.name.trim().to_string());
        let Some(peer) = self.state.peers.get(&handle) else {
            let _ = respond_to.send(encode_error(
                id,
                "not_found",
                format!("no peer named '{handle}'"),
            ));
            return;
        };
        if let Some(reason) = crate::app::peer_picker::peer_unavailable_reason(&peer.connection) {
            let _ = respond_to.send(encode_error(
                id,
                "unavailable",
                format!("peer '{handle}' is unavailable: {reason}"),
            ));
            return;
        }
        let Some(api_socket) = peer.api_socket().map(Path::to_path_buf) else {
            let _ = respond_to.send(encode_error(
                id,
                "unavailable",
                format!("peer '{handle}' has no reachable socket"),
            ));
            return;
        };
        let Some(instance_id) = peer.instance_id().map(str::to_string) else {
            let _ = respond_to.send(encode_error(
                id,
                "unavailable",
                format!("peer '{handle}' has not identified itself yet"),
            ));
            return;
        };

        // The peer picks the cwd when none is given: this server's directories
        // are meaningless on the machine that will run the shell.
        let peer_request = Method::WorkspaceCreate(crate::api::schema::WorkspaceCreateParams {
            cwd: params.cwd,
            // Focus is the peer's own business, exactly as for a split.
            focus: false,
            label: None,
            env: std::collections::HashMap::new(),
        });

        let label = params.label;
        let focus = params.focus;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result =
                request_peer_created_pane(&api_socket, peer_request, &handle, &instance_id);
            let _ = event_tx.blocking_send(crate::events::AppEvent::PeerWorkspaceCreateFinished(
                Box::new(crate::events::PeerWorkspaceCreateResult {
                    id,
                    handle,
                    label,
                    focus,
                    result,
                    respond_to,
                }),
            ));
        });
    }

    /// Asks a peer to create a tab in the workspace a local view is attached
    /// to, then opens a view onto the pane that tab was created around.
    ///
    /// Deferred like the split and the workspace create: a peer round trip
    /// followed by `connect_remote` must not run on the event loop. Creating the
    /// local tab happens back on the loop, because nothing off it may touch
    /// local workspace state.
    pub(super) fn start_peer_tab_create(
        &mut self,
        id: String,
        params: crate::api::schema::TabCreateParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let Some(ws_idx) = self.workspace_index_for_tab_create(params.workspace_id.as_deref())
        else {
            let _ = respond_to.send(encode_error(
                id,
                "workspace_not_found",
                "no workspace to create a tab in",
            ));
            return;
        };
        let Some(workspace) = self.state.workspaces.get(ws_idx) else {
            let _ = respond_to.send(encode_error(
                id,
                "workspace_not_found",
                "workspace not found",
            ));
            return;
        };
        let (Some(peer), Some(peer_workspace)) =
            (workspace.peer.clone(), workspace.peer_workspace.clone())
        else {
            // A view onto a bare pane never learned which workspace holds it, so
            // there is nothing to name on the peer. Say that rather than
            // silently creating the tab on the wrong machine.
            let _ = respond_to.send(encode_error(
                id,
                "invalid_request",
                "this view does not name a workspace on its peer",
            ));
            return;
        };
        let workspace_id = workspace.id.clone();

        // Validate the launch environment locally before sending it on, so a bad
        // key fails here rather than on the peer.
        let env = match crate::app::api::env::normalize_launch_env(params.env) {
            Ok(env) => env.into_iter().collect(),
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, &code, message));
                return;
            }
        };

        // The id is the peer's own, not namespaced, so the peer name has to come
        // alongside it.
        let PeerConnection {
            api_socket,
            instance_id,
            ..
        } = match self.resolve_peer_connection(&peer_workspace, Some(peer.as_str())) {
            Ok(connection) => connection,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };

        // The peer names its own workspace and picks the cwd when none is given:
        // this server's directories mean nothing on the machine that will run
        // the shell. Focus stays local, as it does for a split, so creating a
        // tab here never moves the focus of whoever is using the peer.
        let peer_request = Method::TabCreate(crate::api::schema::TabCreateParams {
            workspace_id: Some(peer_workspace),
            cwd: params.cwd,
            focus: false,
            label: params.label.clone(),
            env,
        });

        let handle = PeerHandle::new(peer);
        let label = params.label;
        let focus = params.focus;
        let crate::terminal::TerminalSize { rows, cols } = self.state.estimate_pane_size();
        let client_socket = client_socket_for(&api_socket);
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result = spawn_peer_tab_view(
                &api_socket,
                &client_socket,
                peer_request,
                &handle,
                &instance_id,
                cols,
                rows,
            );
            let _ = event_tx.blocking_send(crate::events::AppEvent::PeerTabCreateFinished(
                Box::new(crate::events::PeerTabCreateResult {
                    id,
                    workspace_id,
                    label,
                    focus,
                    result,
                    respond_to,
                }),
            ));
        });
    }

    /// Carries a tab rename inside a peer view across to the tab the peer owns.
    ///
    /// Unlike every other peer forward here this one is fire-and-forget: the
    /// local rename has already happened and been answered, so there is no
    /// caller left to fail. A peer that cannot be reached leaves the two names
    /// diverged, which the log records and the next rename corrects.
    ///
    /// The peer's tab id is not stored anywhere on this side — a view records
    /// the peer *pane* it is attached to and nothing above it — so it is asked
    /// for. Any pane in the tab answers: a split inside a peer view is routed to
    /// the peer and lands in the same tab there.
    pub(in crate::app) fn forward_tab_rename_to_peer(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        label: &str,
    ) {
        let Some(workspace) = self.state.workspaces.get(ws_idx) else {
            return;
        };
        if workspace.peer.is_none() {
            return;
        }
        let Some(pane_id) = workspace
            .tabs
            .get(tab_idx)
            .and_then(|tab| tab.panes.keys().copied().next())
        else {
            return;
        };
        let Some((handle, peer_pane_id)) = self.peer_pane_source(ws_idx, pane_id) else {
            return;
        };
        let PeerConnection {
            local_target,
            api_socket,
            instance_id,
        } = match self.resolve_peer_connection(&peer_pane_id, Some(handle.as_str())) {
            Ok(connection) => connection,
            Err((code, message)) => {
                tracing::warn!(
                    peer = handle.as_str(),
                    code,
                    message,
                    "could not reach peer to rename its tab"
                );
                return;
            }
        };

        let label = label.to_string();
        std::thread::spawn(move || {
            if let Err(message) = rename_peer_tab(&api_socket, &local_target, &label, &instance_id)
            {
                tracing::warn!(message, "renaming the peer's tab failed");
            }
        });
    }

    /// Reads a peer-backed pane by asking the peer to read its own.
    ///
    /// The peer holds the screen, so it is the only side that can answer, and
    /// it already exposes exactly this method. Deferred rather than run on the
    /// event loop because a read is not a one-shot: `agent.wait` polls it, so a
    /// slow peer would stall every poll interval — the ssh-wedge lesson applied
    /// to a repeating call.
    ///
    /// The peer answers about its own pane, so the ids it returns name its
    /// workspace, tab, and pane. They are replaced with the local ones captured
    /// here: the caller asked about a pane on this server and has to be able to
    /// use the id it gets back.
    /// Reads a peer's screen for an agent named here.
    ///
    /// Resolving the name is local — this server is the one that knows the agent
    /// as `claude` in workspace 3 — while the screen behind it is not, so the
    /// name becomes a pane id and the read takes the pane path from there.
    /// `agent.read` already answers in `pane_read` shape, so the peer's reply
    /// needs no translation beyond the pane ids the pane path already restates.
    pub(super) fn start_peer_agent_read(
        &mut self,
        id: String,
        params: crate::api::schema::AgentReadParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => {
                let _ = respond_to.send(encode_error_body(id, self.agent_target_error_body(err)));
                return;
            }
        };
        let Some(pane_id) = self.public_pane_id(resolved.ws_idx, resolved.pane_id) else {
            let _ = respond_to.send(agent_not_found(id, &params.target));
            return;
        };
        self.start_peer_pane_read(
            id,
            crate::api::schema::PaneReadParams {
                pane_id,
                source: params.source,
                lines: params.lines,
                format: params.format,
                strip_ansi: params.strip_ansi,
                // Not serialized, so the peer defaults it — the same answer the
                // pane path takes for the same reason.
                intent: Default::default(),
            },
            respond_to,
        );
    }

    /// Explains a peer-backed pane by asking the peer to explain its own.
    ///
    /// The rules that decided this pane's agent state ran on the peer, against a
    /// screen only the peer holds, so the peer is the only side that can answer.
    /// Replaying this server's manifest here reports every rule unmatched while
    /// the pane plainly shows an agent, which invites debugging a manifest that
    /// had no part in the decision.
    ///
    /// Deferred for the same reason as a read: a peer that has gone quiet must
    /// not stall the event loop.
    pub(super) fn start_peer_agent_explain(
        &mut self,
        id: String,
        target: crate::api::schema::AgentTarget,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let resolved = match self.resolve_agent_target(&target.target) {
            Ok(resolved) => resolved,
            Err(err) => {
                let _ = respond_to.send(encode_error_body(id, self.agent_target_error_body(err)));
                return;
            }
        };
        let Some((handle, peer_pane_id)) = self.peer_pane_source(resolved.ws_idx, resolved.pane_id)
        else {
            let _ = respond_to.send(agent_not_found(id, &target.target));
            return;
        };
        if let Some((code, message)) =
            self.peer_pane_server_mismatch(resolved.ws_idx, resolved.pane_id)
        {
            let _ = respond_to.send(encode_error(id, code, message));
            return;
        }
        let PeerConnection {
            local_target,
            api_socket,
            instance_id,
        } = match self.resolve_peer_connection(&peer_pane_id, Some(handle.as_str())) {
            Ok(connection) => connection,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };

        let peer_request = Method::AgentExplain(crate::api::schema::AgentTarget {
            target: local_target.clone(),
        });
        std::thread::spawn(move || {
            let _ = respond_to.send(forward_agent_explain_to_peer(
                &api_socket,
                &id,
                peer_request,
                &handle,
                &local_target,
                &instance_id,
            ));
        });
    }

    pub(super) fn start_peer_pane_read(
        &mut self,
        id: String,
        params: crate::api::schema::PaneReadParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            let _ = respond_to.send(encode_error(id, "pane_not_found", "pane not found"));
            return;
        };
        let Some((handle, peer_pane_id)) = self.peer_pane_source(ws_idx, pane_id) else {
            let _ = respond_to.send(encode_error(
                id,
                "pane_not_found",
                "pane is not backed by a peer",
            ));
            return;
        };
        if let Some((code, message)) = self.peer_pane_server_mismatch(ws_idx, pane_id) {
            let _ = respond_to.send(encode_error(id, code, message));
            return;
        }
        let local_ids = match self.local_read_ids(ws_idx, pane_id) {
            Some(ids) => ids,
            None => {
                let _ = respond_to.send(encode_error(id, "pane_not_found", "pane not found"));
                return;
            }
        };
        let PeerConnection {
            local_target,
            api_socket,
            instance_id,
        } = match self.resolve_peer_connection(&peer_pane_id, Some(handle.as_str())) {
            Ok(connection) => connection,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };

        let peer_request = Method::PaneRead(crate::api::schema::PaneReadParams {
            pane_id: local_target,
            ..params
        });
        std::thread::spawn(move || {
            let _ = respond_to.send(forward_pane_read_to_peer(
                &api_socket,
                &id,
                peer_request,
                &local_ids,
                &instance_id,
            ));
        });
    }

    /// Asks the peer for a span of the screen it owns.
    ///
    /// The rows travel unchanged. A selection's rows are absolute buffer rows
    /// derived from this pane's scroll position, and that position came from
    /// the peer on the frame being looked at, so they already name rows in the
    /// peer's own buffer.
    pub(crate) fn start_peer_pane_read_range(
        &mut self,
        id: String,
        params: crate::api::schema::PaneReadRangeParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            let _ = respond_to.send(encode_error(id, "pane_not_found", "pane not found"));
            return;
        };
        let Some((handle, peer_pane_id)) = self.peer_pane_source(ws_idx, pane_id) else {
            let _ = respond_to.send(encode_error(
                id,
                "pane_not_found",
                "pane is not backed by a peer",
            ));
            return;
        };
        if let Some((code, message)) = self.peer_pane_server_mismatch(ws_idx, pane_id) {
            let _ = respond_to.send(encode_error(id, code, message));
            return;
        }
        let Some(local_pane_id) = self.public_pane_id(ws_idx, pane_id) else {
            let _ = respond_to.send(encode_error(id, "pane_not_found", "pane not found"));
            return;
        };
        let PeerConnection {
            local_target,
            api_socket,
            instance_id,
        } = match self.resolve_peer_connection(&peer_pane_id, Some(handle.as_str())) {
            Ok(connection) => connection,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };

        let peer_request = Method::PaneReadRange(crate::api::schema::PaneReadRangeParams {
            pane_id: local_target,
            ..params
        });
        std::thread::spawn(move || {
            let _ = respond_to.send(forward_pane_read_range_to_peer(
                &api_socket,
                &id,
                peer_request,
                &local_pane_id,
                &instance_id,
            ));
        });
    }

    /// Runs a terminal text query on the peer that owns the pane's buffer.
    pub(crate) fn start_peer_pane_text_query(
        &mut self,
        id: String,
        params: crate::api::schema::PaneTextQueryParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            let _ = respond_to.send(encode_error(id, "pane_not_found", "pane not found"));
            return;
        };
        let Some((handle, peer_pane_id)) = self.peer_pane_source(ws_idx, pane_id) else {
            let _ = respond_to.send(encode_error(
                id,
                "pane_not_found",
                "pane is not backed by a peer",
            ));
            return;
        };
        if let Some((code, message)) = self.peer_pane_server_mismatch(ws_idx, pane_id) {
            let _ = respond_to.send(encode_error(id, code, message));
            return;
        }
        let Some(local_pane_id) = self.public_pane_id(ws_idx, pane_id) else {
            let _ = respond_to.send(encode_error(id, "pane_not_found", "pane not found"));
            return;
        };
        let PeerConnection {
            local_target,
            api_socket,
            instance_id,
        } = match self.resolve_peer_connection(&peer_pane_id, Some(handle.as_str())) {
            Ok(connection) => connection,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };

        let peer_request = Method::PaneTextQuery(crate::api::schema::PaneTextQueryParams {
            pane_id: local_target,
            ..params
        });
        std::thread::spawn(move || {
            let _ = respond_to.send(forward_pane_text_query_to_peer(
                &api_socket,
                &id,
                peer_request,
                &local_pane_id,
                &instance_id,
            ));
        });
    }

    /// The public ids this server knows a pane by, for restating a peer's answer
    /// in local terms.
    pub(super) fn local_read_ids(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<LocalPaneIds> {
        let tab_idx = self
            .state
            .workspaces
            .get(ws_idx)?
            .find_tab_index_for_pane(pane_id)?;
        Some(LocalPaneIds {
            pane_id: self.public_pane_id(ws_idx, pane_id)?,
            workspace_id: self.public_workspace_id(ws_idx),
            tab_id: self.public_tab_id(ws_idx, tab_idx)?,
        })
    }
}

/// Resolves a target on a peer if needed and connects a view onto it.
///
/// Runs entirely on a worker thread: both steps are round trips to another
/// machine, and for an ssh peer the connect starts an ssh child behind a local
/// socket that accepts instantly, so nothing here may run on the event loop.
#[allow(clippy::too_many_arguments)] // Everything the loop already decided.
pub(super) fn open_peer_view(
    api_socket: &Path,
    handle: &PeerHandle,
    local_target: String,
    instance_id: &str,
    resolve_workspace: bool,
    cols: u16,
    rows: u16,
    takeover: bool,
) -> Result<Box<crate::events::PeerViewOpened>, (String, String)> {
    // A workspace id names a workspace, and the peer's terminal resolver takes
    // panes and terminals. Enumeration carries workspace facts only, so the pane
    // behind a workspace has to be asked for.
    let local_target = if resolve_workspace {
        resolve_peer_workspace_pane(api_socket, &local_target, instance_id).map_err(|err| {
            (
                "unavailable".to_string(),
                format!("could not resolve '{local_target}' on peer '{handle}': {err}"),
            )
        })?
    } else {
        local_target
    };

    let client_socket = client_socket_for(api_socket);
    let runtime = crate::terminal::RemoteTerminalRuntime::connect(
        &client_socket,
        handle.as_str().to_string(),
        local_target.clone(),
        cols,
        rows,
        takeover,
    )
    .map_err(|err| {
        (
            "unavailable".to_string(),
            format!("could not open '{local_target}' on peer '{handle}': {err}"),
        )
    })?;
    if runtime.is_on_other_server(instance_id) {
        return Err((
            "server_replaced".to_string(),
            format!("peer '{handle}' was replaced while opening '{local_target}'"),
        ));
    }
    Ok(Box::new(crate::events::PeerViewOpened {
        runtime: Box::new(runtime),
        local_target,
    }))
}

/// Asks a peer to split one of its panes, then connects a view onto whatever
/// pane it created. Runs entirely on a worker thread.
pub(super) fn spawn_peer_split_view(
    api_socket: &Path,
    client_socket: &Path,
    request: Method,
    handle: &PeerHandle,
    instance_id: &str,
    cols: u16,
    rows: u16,
) -> Result<Box<crate::terminal::RemoteTerminalRuntime>, (String, String)> {
    let peer_pane_id = request_peer_split_pane(api_socket, request, handle, instance_id)?;
    let runtime = crate::terminal::RemoteTerminalRuntime::connect(
        client_socket,
        handle.as_str().to_string(),
        peer_pane_id.clone(),
        cols,
        rows,
        false,
    )
    .map_err(|err| {
        // The pane exists on the peer and no view will ever hold it, so nothing
        // downstream can close it. Left alone it is an orphan shell the peer
        // records as owned by an instance that never attaches, and retrying the
        // split makes another one.
        close_abandoned_peer_pane(api_socket, handle, &peer_pane_id, instance_id);
        (
            "unavailable".to_string(),
            format!("could not open '{peer_pane_id}' on peer '{handle}': {err}"),
        )
    })?;
    if runtime.is_on_other_server(instance_id) {
        close_abandoned_peer_pane(api_socket, handle, &peer_pane_id, instance_id);
        return Err((
            "server_replaced".to_string(),
            format!("peer '{handle}' was replaced while opening '{peer_pane_id}'"),
        ));
    }
    let mut runtime = runtime;
    runtime.mark_spawned_on_peer();
    Ok(Box::new(runtime))
}

/// Asks a peer to create a tab, then connects a view onto the pane it opened
/// with.
///
/// The reply already names that pane, so nothing waits for the peer's next
/// enumeration to arrive before the view can exist.
pub(super) fn spawn_peer_tab_view(
    api_socket: &Path,
    client_socket: &Path,
    request: Method,
    handle: &PeerHandle,
    instance_id: &str,
    cols: u16,
    rows: u16,
) -> Result<Box<crate::terminal::RemoteTerminalRuntime>, (String, String)> {
    let peer_pane_id = request_peer_created_pane(api_socket, request, handle, instance_id)?;
    let runtime = crate::terminal::RemoteTerminalRuntime::connect(
        client_socket,
        handle.as_str().to_string(),
        peer_pane_id.clone(),
        cols,
        rows,
        false,
    )
    .map_err(|err| {
        // Same orphan as the split path: the peer made the pane, nothing here
        // will ever hold it, and no reaper exists on either side.
        close_abandoned_peer_pane(api_socket, handle, &peer_pane_id, instance_id);
        (
            "unavailable".to_string(),
            format!("could not open '{peer_pane_id}' on peer '{handle}': {err}"),
        )
    })?;
    if runtime.is_on_other_server(instance_id) {
        close_abandoned_peer_pane(api_socket, handle, &peer_pane_id, instance_id);
        return Err((
            "server_replaced".to_string(),
            format!("peer '{handle}' was replaced while opening '{peer_pane_id}'"),
        ));
    }
    let mut runtime = runtime;
    runtime.mark_spawned_on_peer();
    Ok(Box::new(runtime))
}

/// Asks a peer which of its tabs holds `peer_pane_id`, then renames that tab.
///
/// Two round trips because the pane is the only peer-side id this server keeps:
/// the tab above it is the peer's own business until it is asked.
fn rename_peer_tab(
    api_socket: &Path,
    peer_pane_id: &str,
    label: &str,
    instance_id: &str,
) -> Result<(), String> {
    let client = crate::api::client::ApiClient::for_target(
        crate::api::client::ConnectionTarget::SocketPath(api_socket.to_path_buf()),
    );
    let pane = client
        .request_value_for_instance_with_timeout(
            &Request {
                id: "peer:forward".into(),
                method: Method::PaneGet(crate::api::schema::PaneTarget {
                    pane_id: peer_pane_id.to_string(),
                }),
            },
            instance_id,
            PEER_REQUEST_TIMEOUT,
        )
        .map_err(|err| format!("peer pane lookup failed: {err}"))?;
    let tab_id = pane
        .get("result")
        .and_then(|result| result.get("pane"))
        .and_then(|pane| pane.get("tab_id"))
        .and_then(|tab_id| tab_id.as_str())
        .ok_or_else(|| format!("peer did not name a tab for pane {peer_pane_id}"))?
        .to_string();
    client
        .request_value_for_instance_with_timeout(
            &Request {
                id: "peer:forward".into(),
                method: Method::TabRename(crate::api::schema::TabRenameParams {
                    tab_id,
                    label: label.to_string(),
                }),
            },
            instance_id,
            PEER_REQUEST_TIMEOUT,
        )
        .map_err(|err| format!("peer tab rename failed: {err}"))?;
    Ok(())
}

pub(super) fn request_peer_split_pane(
    api_socket: &Path,
    method: Method,
    handle: &PeerHandle,
    instance_id: &str,
) -> Result<String, (String, String)> {
    let client = crate::api::client::ApiClient::for_target(
        crate::api::client::ConnectionTarget::SocketPath(api_socket.to_path_buf()),
    );
    let request = Request {
        id: "peer:forward".into(),
        method,
    };
    let value = client
        .request_value_for_instance_with_timeout(&request, instance_id, PEER_REQUEST_TIMEOUT)
        .map_err(|err| {
            (
                "unavailable".to_string(),
                format!("peer request failed: {err}"),
            )
        })?;
    peer_split_pane_id(&value, handle)
}

/// Asks a peer to create a workspace and reads the pane it opened with.
pub(super) fn request_peer_created_pane(
    api_socket: &Path,
    method: Method,
    handle: &PeerHandle,
    instance_id: &str,
) -> Result<String, (String, String)> {
    let client = crate::api::client::ApiClient::for_target(
        crate::api::client::ConnectionTarget::SocketPath(api_socket.to_path_buf()),
    );
    let request = Request {
        id: "peer:forward".into(),
        method,
    };
    let value = client
        .request_value_for_instance_with_timeout(&request, instance_id, PEER_REQUEST_TIMEOUT)
        .map_err(|err| {
            (
                "unavailable".to_string(),
                format!("peer request failed: {err}"),
            )
        })?;
    // `root_pane`, not the workspace id: the reply already names the pane a view
    // must attach to, so nothing has to wait for the peer's next enumeration to
    // reach this server before the view can be opened.
    peer_pane_id_at(&value, "root_pane", handle, "the new workspace")
}

/// Asks the peer which pane a view onto `workspace_id` should attach to.
///
/// Its focused pane, falling back to its first: a workspace opened from an
/// enumeration should land where someone at the peer is working, and a
/// workspace always has at least one pane.
///
/// Runs on the same worker as the terminal connection that follows it.
pub(super) fn resolve_peer_workspace_pane(
    api_socket: &Path,
    workspace_id: &str,
    instance_id: &str,
) -> Result<String, String> {
    let client = crate::api::client::ApiClient::for_target(
        crate::api::client::ConnectionTarget::SocketPath(api_socket.to_path_buf()),
    );
    let request = Request {
        id: "peer:panes".into(),
        method: Method::PaneList(crate::api::schema::PaneListParams {
            workspace_id: Some(workspace_id.to_string()),
        }),
    };
    let response = client
        .request_value_for_instance_with_timeout(&request, instance_id, PEER_REQUEST_TIMEOUT)
        .and_then(crate::api::client::parse_response_value)
        .map_err(|err| err.to_string())?;
    let ResponseResult::PaneList { panes } = response.result else {
        return Err("peer returned an unexpected pane list result".to_string());
    };
    panes
        .iter()
        .find(|pane| pane.focused)
        .or_else(|| panes.first())
        .map(|pane| pane.pane_id.clone())
        .ok_or_else(|| format!("workspace {workspace_id} has no panes"))
}

/// The ids a pane is known by here, used to restate a peer's read as our own.
pub(super) struct LocalPaneIds {
    pub(super) pane_id: String,
    pub(super) workspace_id: String,
    pub(super) tab_id: String,
}

/// Forwards a `pane.read` to the peer and restates the answer in local ids.
///
/// Only the ids are rewritten, never the text: the screen belongs to the peer
/// and is reported verbatim, including its truncation flag.
pub(super) fn forward_pane_read_to_peer(
    api_socket: &Path,
    request_id: &str,
    method: Method,
    local_ids: &LocalPaneIds,
    instance_id: &str,
) -> String {
    let client = crate::api::client::ApiClient::for_target(
        crate::api::client::ConnectionTarget::SocketPath(api_socket.to_path_buf()),
    );
    let request = Request {
        id: "peer:forward".into(),
        method,
    };
    match client.request_value_for_instance_with_timeout(
        &request,
        instance_id,
        PEER_REQUEST_TIMEOUT,
    ) {
        Ok(mut value) => {
            rewrite_forwarded_read(&mut value, request_id, local_ids);
            serde_json::to_string(&value).unwrap_or_else(|err| {
                encode_error(
                    request_id.to_string(),
                    "serialization_error",
                    err.to_string(),
                )
            })
        }
        Err(err) => encode_error(
            request_id.to_string(),
            "unavailable",
            format!("peer request failed: {err}"),
        ),
    }
}

/// Forwards a `pane.read_range` and restates the pane it answers about.
///
/// Only the pane id needs rewriting: the payload is the text and the id, and
/// the text is the peer's screen either way.
pub(super) fn forward_pane_read_range_to_peer(
    api_socket: &Path,
    request_id: &str,
    method: Method,
    local_pane_id: &str,
    instance_id: &str,
) -> String {
    let client = crate::api::client::ApiClient::for_target(
        crate::api::client::ConnectionTarget::SocketPath(api_socket.to_path_buf()),
    );
    let request = Request {
        id: "peer:forward".into(),
        method,
    };
    match client.request_value_for_instance_with_timeout(
        &request,
        instance_id,
        PEER_REQUEST_TIMEOUT,
    ) {
        Ok(mut value) => {
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "id".to_string(),
                    serde_json::Value::String(request_id.to_string()),
                );
                // The pane id sits inside `read`, beside the text, and it is
                // the peer's name for the pane. Restated here so the caller
                // sees the pane it actually asked about.
                if let Some(read) = object
                    .get_mut("result")
                    .and_then(|result| result.get_mut("read"))
                    .and_then(|read| read.as_object_mut())
                {
                    read.insert(
                        "pane_id".to_string(),
                        serde_json::Value::String(local_pane_id.to_string()),
                    );
                }
            }
            serde_json::to_string(&value).unwrap_or_else(|err| {
                encode_error(
                    request_id.to_string(),
                    "serialization_error",
                    err.to_string(),
                )
            })
        }
        Err(err) => encode_error(
            request_id.to_string(),
            "unavailable",
            format!("peer request failed: {err}"),
        ),
    }
}

/// Forwards a terminal text query and restates the pane it answers about.
pub(super) fn forward_pane_text_query_to_peer(
    api_socket: &Path,
    request_id: &str,
    method: Method,
    local_pane_id: &str,
    instance_id: &str,
) -> String {
    let client = crate::api::client::ApiClient::for_target(
        crate::api::client::ConnectionTarget::SocketPath(api_socket.to_path_buf()),
    );
    let request = Request {
        id: "peer:forward".into(),
        method,
    };
    match client.request_value_for_instance_with_timeout(
        &request,
        instance_id,
        PEER_REQUEST_TIMEOUT,
    ) {
        Ok(mut value) => {
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "id".to_string(),
                    serde_json::Value::String(request_id.to_string()),
                );
                if let Some(query) = object
                    .get_mut("result")
                    .and_then(|result| result.get_mut("query"))
                    .and_then(|query| query.as_object_mut())
                {
                    query.insert(
                        "pane_id".to_string(),
                        serde_json::Value::String(local_pane_id.to_string()),
                    );
                }
            }
            serde_json::to_string(&value).unwrap_or_else(|err| {
                encode_error(
                    request_id.to_string(),
                    "serialization_error",
                    err.to_string(),
                )
            })
        }
        Err(err) => encode_error(
            request_id.to_string(),
            "unavailable",
            format!("peer request failed: {err}"),
        ),
    }
}

/// Forwards an `agent.explain` to the peer and stamps whose answer it is.
///
/// Nothing in an explain payload names a pane, workspace, or tab — it is the
/// agent, the manifest, and the rules — so unlike a forwarded read there are no
/// ids to restate here. What does need restating is provenance: the manifest and
/// every rule id in the answer belong to the peer's install, and without saying
/// so the reply reads as this server's own detection.
pub(super) fn forward_agent_explain_to_peer(
    api_socket: &Path,
    request_id: &str,
    method: Method,
    peer: &PeerHandle,
    peer_pane_id: &str,
    instance_id: &str,
) -> String {
    let client = crate::api::client::ApiClient::for_target(
        crate::api::client::ConnectionTarget::SocketPath(api_socket.to_path_buf()),
    );
    let request = Request {
        id: "peer:forward".into(),
        method,
    };
    match client.request_value_for_instance_with_timeout(
        &request,
        instance_id,
        PEER_REQUEST_TIMEOUT,
    ) {
        Ok(mut value) => {
            rewrite_forwarded_explain(&mut value, request_id, peer, peer_pane_id);
            serde_json::to_string(&value).unwrap_or_else(|err| {
                encode_error(
                    request_id.to_string(),
                    "serialization_error",
                    err.to_string(),
                )
            })
        }
        Err(err) => encode_error(
            request_id.to_string(),
            "unavailable",
            format!("peer request failed: {err}"),
        ),
    }
}

pub(super) fn forward_workspace_request_to_peer(
    api_socket: &Path,
    request_id: &str,
    method: Method,
    instance_id: &str,
    remap_ids: bool,
) -> String {
    let client = crate::api::client::ApiClient::for_target(
        crate::api::client::ConnectionTarget::SocketPath(api_socket.to_path_buf()),
    );
    let request = Request {
        id: "peer:forward".into(),
        method,
    };
    match client.request_value_for_instance_with_timeout(
        &request,
        instance_id,
        PEER_REQUEST_TIMEOUT,
    ) {
        Ok(mut value) => {
            rewrite_forwarded_response(&mut value, request_id, instance_id, remap_ids);
            serde_json::to_string(&value).unwrap_or_else(|err| {
                encode_error(
                    request_id.to_string(),
                    "serialization_error",
                    err.to_string(),
                )
            })
        }
        Err(err) => encode_error(
            request_id.to_string(),
            "unavailable",
            format!("peer request failed: {err}"),
        ),
    }
}
