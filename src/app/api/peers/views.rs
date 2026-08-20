//! Keeping peer-backed views alive.
//!
//! Depends on `resolve`.
//!
//! Runs from the server loop, declaratively: the desired state is "every view is
//! connected to the server it was opened against", and comparing against it every
//! tick catches failures no single event announces.

use super::*;

impl App {
    /// Stops every view of `handle` that is attached to a server other than
    /// `instance_id`.
    ///
    /// A view's target is a *peer-local* id kept verbatim across reconnects,
    /// because the peer resolves it again on every attach. That is only sound
    /// while the peer is the same server. After a restart with a fresh session
    /// directory, `w1:p1` names an unrelated pane, so reconnecting would render
    /// somebody else's terminal under the previous agent's label — and closing
    /// the view would forward `pane.close` for that id to the new server and
    /// destroy a pane nobody asked about.
    ///
    /// The claim goes first and the view is marked dead rather than closed: the
    /// existing dead-view presentation already draws the last frame dimmed with
    /// a reason, which is the honest thing to show for a pane whose contents can
    /// no longer be reached.
    pub(crate) fn abandon_views_of_replaced_peer(
        &mut self,
        handle: &PeerHandle,
        instance_id: &str,
    ) {
        for runtime in self.terminal_runtimes.values_mut() {
            let Some(remote) = runtime.remote_mut() else {
                continue;
            };
            if remote.peer() != handle.as_str() || !remote.is_on_other_server(instance_id) {
                continue;
            }
            // The pane this claim names belongs to a server that is gone. Left
            // set, closing the view would close whatever now answers to that id.
            remote.clear_spawned_on_peer();
            remote.mark_dead("the peer was replaced by a different server");
        }
    }

    /// Reopens peer-backed views whose control connection dropped.
    ///
    /// Runs on the server loop next to peer reconciliation, and for the same
    /// reason: the desired state is "every view is connected", and comparing
    /// against it every tick catches failures no single event announces. A view
    /// can lose its connection while its peer's control channel stays up — a
    /// broken bridge connection, an attach the peer dropped — so reacting only
    /// to peer state would strand exactly those panes.
    ///
    /// The socket is resolved here rather than remembered by the view: an ssh
    /// peer's bridge answers on a new path after the transport is rebuilt, so a
    /// remembered path names a dead socket. Connecting itself happens on a
    /// worker, because for an ssh peer it starts an ssh child.
    ///
    /// One refusal retires rather than reconnects: when the peer says the
    /// target is gone, the pane died on another machine, and keeping a gray
    /// view of it would strand a workspace entry forever.
    pub(crate) fn reconcile_remote_terminal_views(&mut self) {
        let now = Instant::now();
        let due: Vec<(crate::terminal::TerminalId, String, String, u16, u16)> = self
            .terminal_runtimes
            .iter()
            .filter_map(|(terminal_id, runtime)| {
                let remote = runtime.remote()?;
                if !remote.reconnect_due(now) {
                    return None;
                }
                let crate::terminal::TerminalSize { rows, cols } = remote.current_size();
                Some((
                    terminal_id.clone(),
                    remote.peer().to_string(),
                    remote.target().to_string(),
                    cols,
                    rows,
                ))
            })
            .collect();

        let mut gone = Vec::new();
        for (terminal_id, peer, target, cols, rows) in due {
            let handle = PeerHandle::new(peer.clone());
            // Peer state lives on this loop, so the decision is made here and
            // only the connect runs elsewhere.
            let connection = match self.state.peers.get(&handle) {
                None => Err(Some(format!("peer '{handle}' is no longer configured"))),
                Some(state) => match &state.connection {
                    // A peer that stopped retrying will not answer, and its
                    // views cannot outlast it.
                    PeerConnectionState::Error { message } => {
                        Err(Some(format!("peer '{handle}' is unavailable: {message}")))
                    }
                    // Still down: the view waits for the peer rather than
                    // burning attempts against a socket that cannot answer.
                    PeerConnectionState::Connecting | PeerConnectionState::Reconnecting { .. } => {
                        Err(None)
                    }
                    PeerConnectionState::Connected => {
                        match (state.api_socket(), state.instance_id()) {
                            (Some(api_socket), Some(instance_id)) => {
                                Ok((client_socket_for(api_socket), instance_id.to_string()))
                            }
                            _ => Err(None),
                        }
                    }
                },
            };

            let Some(remote) = self
                .terminal_runtimes
                .get_mut(&terminal_id)
                .and_then(crate::terminal::TerminalRuntime::remote_mut)
            else {
                continue;
            };
            let (client_socket, instance_id) = match connection {
                Ok(connection) => connection,
                Err(Some(reason)) => {
                    remote.mark_dead(reason);
                    continue;
                }
                Err(None) => continue,
            };
            if !remote.begin_reconnect() {
                // The view was just declared dead. A peer that names the target
                // as gone has already destroyed the pane, so the local view
                // follows it rather than showing a gray frame forever.
                if remote.target_is_a_pane_or_terminal() && remote.died_because_target_is_gone() {
                    gone.push(terminal_id.clone());
                }
                continue;
            }

            let event_tx = self.event_tx.clone();
            std::thread::spawn(move || {
                let result = crate::terminal::RemoteTerminalRuntime::connect(
                    &client_socket,
                    peer,
                    target,
                    cols,
                    rows,
                    // Never a takeover: the peer lets this server reclaim its
                    // own abandoned attach, and anything else holding the
                    // terminal is somebody else's session.
                    false,
                )
                .and_then(|runtime| {
                    if runtime.is_on_other_server(&instance_id) {
                        Err(std::io::Error::other(
                            "peer was replaced while reconnecting the view",
                        ))
                    } else {
                        Ok(runtime)
                    }
                })
                .map(Box::new)
                .map_err(|err| err.to_string());
                let _ = event_tx.blocking_send(crate::events::AppEvent::PeerViewReconnected(
                    Box::new(crate::events::PeerViewReconnectResult {
                        terminal_id,
                        result,
                    }),
                ));
            });
        }

        for terminal_id in gone {
            self.retire_gone_peer_view(&terminal_id);
        }

        self.announce_peer_view_liveness();
    }

    /// Removes a view whose peer said its target does not exist.
    ///
    /// The peer's answer is authoritative — the pane was destroyed there — so
    /// the local side runs the ordinary pane-death path and the tab or
    /// workspace follows the pane exactly as if a local process had exited.
    /// The spawned-on-peer claim is cleared first: there is nothing left on
    /// the peer to close, and forwarding `pane.close` for it would only log a
    /// refused cleanup.
    pub(crate) fn retire_gone_peer_view(&mut self, terminal_id: &crate::terminal::TerminalId) {
        if let Some(remote) = self
            .terminal_runtimes
            .get_mut(terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote_mut)
        {
            remote.clear_spawned_on_peer();
        }
        if let Some((_, pane_id)) = self.pane_location_for_terminal(terminal_id) {
            self.state.handle_pane_died(pane_id);
        } else {
            // A bare terminal (`peer.terminal.open`) is held by no pane;
            // removing the terminal record queues its runtime shutdown exactly
            // as `peer.terminal.close` does.
            self.state
                .remove_unattached_terminal_ids([terminal_id.clone()]);
        }
        self.shutdown_detached_terminal_runtimes();
    }

    /// Emits `pane.updated` for every view whose liveness just changed.
    ///
    /// Runs off the same sweep rather than the places that change the state,
    /// because most of them are not places at all: a connection drops on its own
    /// reader thread and reaches the event loop only as an atomic flag. Diffing
    /// against what was last announced catches those the same way it catches the
    /// transitions this loop makes itself, and makes a repeat impossible.
    fn announce_peer_view_liveness(&mut self) {
        let changed: Vec<crate::terminal::TerminalId> = self
            .terminal_runtimes
            .iter_mut()
            .filter_map(|(terminal_id, runtime)| {
                runtime.remote_mut()?.take_view_state_change()?;
                Some(terminal_id.clone())
            })
            .collect();
        for terminal_id in changed {
            // A view whose pane is already gone has nothing to report about.
            let Some((ws_idx, pane_id)) = self.pane_location_for_terminal(&terminal_id) else {
                continue;
            };
            self.emit_pane_updated(ws_idx, pane_id);
        }
    }

    /// Retries every view backed by `handle` immediately.
    ///
    /// Called when the peer's own control channel comes back: a view that
    /// earned a ten-second backoff while the peer was down should not keep the
    /// pane frozen for ten more seconds after it returns.
    pub(crate) fn retry_peer_views_now(&mut self, handle: &PeerHandle) {
        for runtime in self.terminal_runtimes.values_mut() {
            if let Some(remote) = runtime.remote_mut() {
                if remote.peer() == handle.as_str() {
                    remote.retry_now();
                }
            }
        }
    }

    /// Puts a reconnected view back in the slot its pane already points at.
    ///
    /// The terminal id does not change, so no pane, layout, or workspace state
    /// moves: from everything above this, the view was simply stale for a while.
    pub(crate) fn handle_peer_view_reconnected(
        &mut self,
        finished: crate::events::PeerViewReconnectResult,
    ) {
        let crate::events::PeerViewReconnectResult {
            terminal_id,
            result,
        } = finished;

        // Read before the runtime map is borrowed: what the peer registry says
        // *now* is what decides whether this answer is still about the server
        // the worker reached. Absent only when the peer is no longer configured,
        // which takes its views with it — so the missing-entry path below has
        // already handled that case by the time it could matter here.
        let current_instance = result.as_ref().ok().and_then(|runtime| {
            self.state
                .peers
                .get(&PeerHandle::new(runtime.peer().to_string()))
                .and_then(crate::app::peers::PeerState::instance_id)
                .map(str::to_string)
        });

        let Some(existing) = self
            .terminal_runtimes
            .get_mut(&terminal_id)
            .and_then(crate::terminal::TerminalRuntime::remote_mut)
        else {
            // The pane closed while the attempt was in flight. Closing the new
            // view releases the peer's terminal instead of leaving it rendering
            // for nobody.
            if let Ok(runtime) = result {
                runtime.shutdown();
            }
            return;
        };

        match result {
            Ok(mut runtime) => {
                // The worker checked the peer's identity when it connected, but
                // its answer travels back as an event and the world moves in
                // between. Installing it unconditionally is what let a view be
                // resurrected onto a server that had already been replaced,
                // still displaying and still accepting input.
                if let Some(reason) = existing.dead_reason() {
                    tracing::info!(
                        peer = %runtime.peer(),
                        target = %runtime.target(),
                        reason = %reason,
                        "discarding a reconnect that finished after its view was retired"
                    );
                    runtime.shutdown();
                    return;
                }
                if current_instance
                    .as_deref()
                    .is_some_and(|instance| runtime.is_on_other_server(instance))
                {
                    tracing::warn!(
                        peer = %runtime.peer(),
                        target = %runtime.target(),
                        "discarding a reconnect that finished against a replaced peer"
                    );
                    runtime.shutdown();
                    // The view itself is still live and still wants a
                    // connection, so this counts as a failed attempt rather
                    // than a silent stall: without it the in-flight flag stays
                    // set and the view is never retried at all.
                    existing.reconnect_failed(
                        Instant::now(),
                        "the peer was replaced while reconnecting the view",
                    );
                    return;
                }

                runtime.inherit_from(existing);
                tracing::info!(
                    peer = %runtime.peer(),
                    target = %runtime.target(),
                    "reconnected a peer-backed view"
                );
                // Replacing the entry drops the old connection, which detaches
                // from the peer if the socket somehow still works.
                self.terminal_runtimes.insert(
                    terminal_id,
                    crate::terminal::TerminalRuntime::Remote(runtime),
                );
            }
            Err(error) => existing.reconnect_failed(Instant::now(), &error),
        }
    }

    /// Caches what a peer just reported about its panes on the views onto them.
    ///
    /// Only the peer can see a remote pane's cwd, title, or agent, and a render
    /// frame must never wait on another machine, so the facts are stored on the
    /// runtime and read from there. Everything above this — sidebar labels,
    /// `pane.list`, workspace names — then works on a peer-backed pane without
    /// knowing peers exist.
    ///
    /// Returns the agent detections to replay for panes whose agent facts moved.
    /// Agent state is not runtime data: it belongs to the terminal, where hook
    /// authority, notifications, and the seen flag already live, so it is fed
    /// back through the same event the local detector emits rather than read
    /// from the cache at each use.
    pub(crate) fn handle_peer_panes_updated(
        &mut self,
        handle: &PeerHandle,
        panes: &[crate::api::schema::PaneInfo],
    ) -> Vec<crate::events::AppEvent> {
        let peer = handle.as_str();
        let mut changed_agents = Vec::new();
        let mut retitled = Vec::new();
        let mut changed_any = false;
        for (terminal_id, runtime) in self.terminal_runtimes.iter_mut() {
            let Some(remote) = runtime.remote_mut() else {
                continue;
            };
            let Some(pane) = panes
                .iter()
                .find(|pane| remote.views_peer_pane(peer, &pane.pane_id, &pane.terminal_id))
            else {
                continue;
            };
            let metadata = crate::terminal::RemotePaneMetadata {
                cwd: pane.cwd.as_deref().map(PathBuf::from),
                foreground_cwd: pane.foreground_cwd.as_deref().map(PathBuf::from),
                terminal_title: pane.terminal_title.clone(),
                agent_osc_title: pane.agent_osc_title.clone(),
                agent_osc_progress: pane.agent_osc_progress.clone(),
                agent: pane.agent.clone(),
                agent_status: Some(pane.agent_status),
                keyboard_protocol: pane.keyboard_protocol,
            };
            let agent_facts_changed = remote.metadata().is_none_or(|previous| {
                previous.agent != metadata.agent || previous.agent_status != metadata.agent_status
            });
            let title_changed = remote
                .metadata()
                .is_none_or(|previous| previous.terminal_title != metadata.terminal_title);
            changed_any |= remote.set_metadata(metadata);
            if agent_facts_changed {
                changed_agents.push((terminal_id.clone(), pane.agent.clone(), pane.agent_status));
            }
            if title_changed {
                retitled.push(terminal_id.clone());
            }
        }

        // A peer-backed pane has no local pty, so nothing marks its title dirty
        // the way OSC processing does for a local one. Without this the peer's
        // title reaches the runtime and stops there, never reaching the cached
        // terminal state that `pane.list` and the sidebar read.
        for terminal_id in retitled {
            if let Some(pane_id) = self.pane_for_terminal(&terminal_id) {
                self.render_dirty.request_terminal_title(pane_id);
            }
        }

        if changed_any {
            // cwd and title feed the sidebar, and nothing else in this tick
            // knows they moved.
            self.render_dirty.request_generic();
            self.render_notify.notify_one();
        }

        let observed_at = Instant::now();
        changed_agents
            .into_iter()
            .filter_map(|(terminal_id, agent, status)| {
                let pane_id = self.pane_for_terminal(&terminal_id)?;
                let state = agent_state_from_status(status);
                Some(crate::events::AppEvent::StateChanged {
                    pane_id,
                    // An agent the peer knows and this build does not resolves
                    // to no agent rather than a wrong one; the pane still
                    // reports the state the peer saw.
                    agent: agent.as_deref().and_then(crate::detect::parse_agent_label),
                    state,
                    visible_blocker: state == crate::detect::AgentState::Blocked,
                    visible_working: state == crate::detect::AgentState::Working,
                    // The peer owns the process. A remote agent exiting arrives
                    // as its pane disappearing from the enumeration, not as an
                    // exit this server watched.
                    process_exited: false,
                    observed_at,
                })
            })
            .collect()
    }
}
