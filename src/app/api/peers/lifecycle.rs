//! Placing peer-backed views, and taking them away again.
//!
//! Depends on `resolve` and `views`.
//!
//! The completion half of `forward`: a worker connected something, and this
//! decides where it goes — or, when the place it was going has since closed,
//! disposes of it in the one way that also releases the pane on the peer.

use super::*;

impl App {
    /// Places a view a worker just connected, and answers the waiting caller.
    pub(crate) fn handle_peer_view_open_finished(
        &mut self,
        finished: crate::events::PeerViewOpenResult,
    ) {
        let crate::events::PeerViewOpenResult {
            id,
            handle,
            requested_target,
            started_target,
            placement,
            result,
            respond_to,
        } = finished;
        self.peer_view_opens_in_flight
            .remove(&(handle.as_str().to_string(), started_target));

        let opened = match result {
            Ok(opened) => *opened,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, &code, message));
                return;
            }
        };
        let crate::events::PeerViewOpened {
            runtime,
            local_target,
        } = opened;
        if let Some(existing) = self.peer_view_already_open(handle.as_str(), &local_target, false) {
            runtime.shutdown();
            let response = match (placement, existing) {
                (
                    crate::events::PeerViewPlacement::Workspace {
                        focus, worktree, ..
                    },
                    ExistingPeerView::InWorkspace { ws_idx, .. },
                ) => {
                    if focus {
                        self.state.switch_workspace(ws_idx);
                    }
                    match worktree {
                        Some(answer) => self.peer_worktree_view_response(
                            id,
                            ws_idx,
                            answer.into_already_open(),
                        ),
                        None => encode_success(
                            id,
                            ResponseResult::WorkspaceInfo {
                                workspace: self.workspace_info(ws_idx),
                            },
                        ),
                    }
                }
                (
                    crate::events::PeerViewPlacement::Workspace { .. },
                    ExistingPeerView::Bare { terminal_id },
                ) => encode_error(
                    id,
                    "already_exists",
                    format!(
                        "'{local_target}' on peer '{handle}' is already open as terminal '{terminal_id}'"
                    ),
                ),
                (
                    crate::events::PeerViewPlacement::Terminal { target },
                    existing,
                ) => {
                    let terminal_id = existing.terminal_id().clone();
                    let crate::terminal::TerminalSize { rows, cols } = self
                        .terminal_runtimes
                        .get(&terminal_id)
                        .map(|runtime| runtime.current_size())
                        .unwrap_or(crate::terminal::TerminalSize::new(1, 1));
                    encode_success(
                        id,
                        ResponseResult::PeerTerminal {
                            terminal: PeerTerminalInfo {
                                terminal_id: terminal_id.to_string(),
                                name: handle.as_str().to_string(),
                                target,
                                local_target,
                                cols,
                                rows,
                            },
                        },
                    )
                }
            };
            let _ = respond_to.send(response);
            return;
        }
        let runtime = crate::terminal::TerminalRuntime::Remote(runtime);

        let response = match placement {
            crate::events::PeerViewPlacement::Workspace {
                peer_workspace,
                label,
                focus,
                worktree,
            } => {
                let label = label.unwrap_or_else(|| format!("{handle}:{local_target}"));
                let ws_idx = self.create_attached_workspace(
                    std::path::PathBuf::from("/"),
                    handle.as_str().to_string(),
                    peer_workspace,
                    Some(label),
                    runtime,
                    focus,
                );
                match worktree {
                    Some(answer) => self.peer_worktree_view_response(id, ws_idx, *answer),
                    None => encode_success(
                        id,
                        ResponseResult::WorkspaceInfo {
                            workspace: self.workspace_info(ws_idx),
                        },
                    ),
                }
            }
            crate::events::PeerViewPlacement::Terminal { target } => {
                let crate::terminal::TerminalSize { rows, cols } = runtime.current_size();
                // Registered in both maps, not just the runtime one: the id this
                // returns is only usable if a client can attach to it, and only
                // reapable if releasing it can find a terminal to remove.
                let terminal_id = self.register_peer_terminal(runtime);
                encode_success(
                    id,
                    ResponseResult::PeerTerminal {
                        terminal: PeerTerminalInfo {
                            terminal_id: terminal_id.to_string(),
                            name: handle.as_str().to_string(),
                            target,
                            local_target,
                            cols,
                            rows,
                        },
                    },
                )
            }
        };
        let _ = respond_to.send(response);
        let _ = requested_target;
    }

    /// Registers a peer-backed terminal that no workspace holds.
    ///
    /// Both maps, not just the runtime one. `terminal_id_by_string` searches
    /// `state.terminals`, so a runtime registered without a terminal returns an
    /// id nothing can attach to; and `remove_unattached_terminal_ids` only
    /// queues a shutdown for an id it can remove from `state.terminals`, so such
    /// a runtime could never be reaped either — it would hold a thread, a
    /// socket, the peer's attach lock and the peer's pane poll until the process
    /// exited, while reconnecting forever.
    pub(super) fn register_peer_terminal(
        &mut self,
        runtime: crate::terminal::TerminalRuntime,
    ) -> crate::terminal::TerminalId {
        let terminal_id = crate::terminal::TerminalId::alloc();
        // The cwd is the peer's and is never probed here, matching what a
        // peer-backed workspace records for the same reason.
        let terminal =
            crate::terminal::TerminalState::new(terminal_id.clone(), std::path::PathBuf::from("/"));
        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        self.state.terminals.insert(terminal_id.clone(), terminal);
        terminal_id
    }

    /// Releases a peer-backed terminal opened without a workspace around it.
    ///
    /// The counterpart `peer.terminal.open` needs: a bare terminal is held by
    /// nothing, so without this the only way to release one is to exit the
    /// server. A terminal a pane is attached to is refused rather than silently
    /// left alone — closing the pane is what releases those.
    pub(in crate::app) fn handle_peer_terminal_close(
        &mut self,
        id: String,
        target: crate::api::schema::TerminalTarget,
    ) -> String {
        let Some(terminal_id) = self
            .state
            .terminals
            .keys()
            .find(|known| known.to_string() == target.terminal_id)
            .cloned()
        else {
            return encode_error(
                id,
                "not_found",
                format!("no terminal named '{}'", target.terminal_id),
            );
        };

        if self
            .terminal_runtimes
            .get(&terminal_id)
            .is_none_or(|runtime| runtime.remote().is_none())
        {
            return encode_error(
                id,
                "invalid_request",
                format!("terminal '{}' is not peer-backed", target.terminal_id),
            );
        }

        let attached = self.state.workspaces.iter().any(|ws| {
            ws.tabs.iter().any(|tab| {
                tab.panes
                    .values()
                    .any(|pane| pane.attached_terminal_id == terminal_id)
            })
        });
        if attached {
            return encode_error(
                id,
                "invalid_request",
                format!(
                    "terminal '{}' is attached to a pane; close the pane instead",
                    target.terminal_id
                ),
            );
        }

        self.state
            .remove_unattached_terminal_ids([terminal_id.clone()]);
        self.shutdown_detached_terminal_runtimes();
        encode_success(
            id,
            ResponseResult::TerminalClosed {
                terminal_id: terminal_id.to_string(),
            },
        )
    }

    /// Attaches a view onto the pane a peer just created for us as a new local
    /// tab.
    pub(crate) fn handle_peer_tab_create_finished(
        &mut self,
        finished: crate::events::PeerTabCreateResult,
    ) {
        let crate::events::PeerTabCreateResult {
            id,
            workspace_id,
            label,
            focus,
            result,
            respond_to,
        } = finished;
        let runtime = match result {
            Ok(runtime) => crate::terminal::TerminalRuntime::Remote(runtime),
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, &code, message));
                return;
            }
        };

        // The workspace list can change while the peer round trip is in flight,
        // so the target is re-resolved by id. When it is gone the view has
        // nothing to render into and is closed rather than left connected.
        let Some(ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == workspace_id)
        else {
            self.discard_spawned_peer_view(runtime);
            let _ = respond_to.send(encode_error(
                id,
                "workspace_not_found",
                "workspace closed while the peer was creating the tab",
            ));
            return;
        };
        if self.state.workspaces.get(ws_idx).is_none() {
            self.discard_spawned_peer_view(runtime);
            let _ = respond_to.send(encode_error(
                id,
                "workspace_not_found",
                "workspace not found",
            ));
            return;
        }
        let Some(workspace) = self.state.workspaces.get_mut(ws_idx) else {
            unreachable!("the index was just checked")
        };

        let (tab_idx, terminal, runtime) =
            workspace.create_tab_attached(std::path::PathBuf::from("/"), runtime);
        self.terminal_runtimes.insert(terminal.id.clone(), runtime);
        self.state.terminals.insert(terminal.id.clone(), terminal);
        self.state.remove_alias_shadowed_by_new_pane(
            self.state.workspaces[ws_idx].tabs[tab_idx].root_pane,
        );
        if let Some(label) = label {
            if let Some(tab) = self
                .state
                .workspaces
                .get_mut(ws_idx)
                .and_then(|ws| ws.tabs.get_mut(tab_idx))
            {
                tab.set_custom_name(label);
            }
        }
        if focus {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state.mode = crate::app::Mode::Terminal;
        }
        self.emit_tab_created_events(ws_idx, tab_idx);

        let response = match self.tab_created_result(ws_idx, tab_idx) {
            Some(result) => encode_success(id, result),
            None => encode_error(id, "tab_create_failed", "the new tab disappeared"),
        };
        let _ = respond_to.send(response);
    }

    /// Opens the view onto a workspace a peer just created for us.
    pub(crate) fn handle_peer_workspace_create_finished(
        &mut self,
        finished: crate::events::PeerWorkspaceCreateResult,
    ) {
        let crate::events::PeerWorkspaceCreateResult {
            id,
            handle,
            label,
            focus,
            result,
            respond_to,
        } = finished;
        let peer_pane_id = match result {
            Ok(pane_id) => pane_id,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, &code, message));
                return;
            }
        };

        // Reuses the ordinary open path, so this view reconnects, dedupes and
        // reports exactly like one opened from the picker. The pane already
        // exists on the peer, so it is adopted rather than marked as spawned
        // here: closing this view should not delete a workspace the peer now
        // lists as its own.
        //
        // The deferred variant, not the direct one: this handler runs on the
        // event loop, so connecting from here would put back the very blocking
        // the round trip above was deferred to avoid.
        self.start_peer_workspace_open(
            id,
            peer_pane_id,
            Some(handle.as_str()),
            label,
            focus,
            false,
            None,
            respond_to,
        );
    }

    /// Places a connected peer view into the local layout once the peer has
    /// spawned the pane behind it.
    pub(crate) fn handle_peer_pane_split_finished(
        &mut self,
        finished: crate::events::PeerPaneSplitResult,
    ) {
        let crate::events::PeerPaneSplitResult {
            id,
            workspace_id,
            target_pane,
            direction,
            ratio,
            focus,
            result,
            respond_to,
        } = finished;
        let runtime = match result {
            Ok(runtime) => crate::terminal::TerminalRuntime::Remote(runtime),
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, &code, message));
                return;
            }
        };
        // The workspace list can change while the peer round trip is in flight,
        // so the target is re-resolved by id. When it is gone the view has
        // nothing to render into and is closed rather than left connected.
        let Some(ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == workspace_id)
        else {
            self.discard_spawned_peer_view(runtime);
            let _ = respond_to.send(encode_error(
                id,
                "workspace_not_found",
                "workspace closed while the peer was splitting",
            ));
            return;
        };
        let previous_focus = self.state.current_pane_focus_target();
        if self.state.workspaces.get(ws_idx).is_none() {
            self.discard_spawned_peer_view(runtime);
            let _ = respond_to.send(encode_error(
                id,
                "workspace_not_found",
                "workspace not found",
            ));
            return;
        }
        let Some(ws) = self.state.workspaces.get_mut(ws_idx) else {
            unreachable!("the index was just checked")
        };
        let (tab_idx, new_pane) = match ws.split_pane_attached(
            target_pane,
            direction,
            ratio,
            std::path::PathBuf::from("/"),
            runtime,
            focus,
        ) {
            Ok(split) => split,
            Err(runtime) => {
                self.discard_spawned_peer_view(*runtime);
                let _ = respond_to.send(encode_error(
                    id,
                    "pane_not_found",
                    "pane closed while the peer was splitting",
                ));
                return;
            }
        };
        if focus {
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state
                .record_pane_focus_change(previous_focus, ws_idx, new_pane.pane_id);
            self.state.settle_terminal_mode_after_focus();
        }
        self.terminal_runtimes
            .insert(new_pane.terminal.id.clone(), new_pane.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
        self.state
            .terminals
            .insert(new_pane.terminal.id.clone(), new_pane.terminal);
        // Not persisted: the workspace holding this pane is peer-backed, so the
        // snapshot excludes it and it is rebuilt by reconnecting.
        let Some(pane) = self.pane_info(ws_idx, new_pane.pane_id) else {
            let _ = respond_to.send(encode_error(id, "pane_not_found", "pane not found"));
            return;
        };
        self.emit_event(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::PaneCreated,
            data: crate::api::schema::EventData::PaneCreated { pane: pane.clone() },
        });
        self.emit_layout_updated_event(ws_idx, tab_idx);
        let _ = respond_to.send(encode_success(id, ResponseResult::PaneInfo { pane }));
    }

    /// Closes every workspace backed by a peer that has just been removed.
    ///
    /// Keyed on removal, never on disconnection: a remote view cannot reconnect,
    /// but a peer that dropped out is expected back, and closing its views on a
    /// blip would delete the layout out from under whoever is using it. Removal
    /// is permanent, so the views can only ever render a frozen frame.
    pub(super) fn close_views_backed_by_peer(&mut self, handle: &PeerHandle) {
        let backed: Vec<String> = self
            .state
            .workspaces
            .iter()
            .filter(|ws| ws.peer.as_deref() == Some(handle.as_str()))
            .map(|ws| ws.id.clone())
            .collect();
        // Closing renumbers the list, so each workspace is re-resolved by id.
        for workspace_id in backed {
            let Some(index) = self
                .state
                .workspaces
                .iter()
                .position(|ws| ws.id == workspace_id)
            else {
                continue;
            };
            let public_id = self.public_workspace_id(index);
            let info = self.workspace_info(index);
            let pane_ids = self
                .state
                .workspaces
                .get(index)
                .map(|ws| {
                    ws.tabs
                        .iter()
                        .flat_map(|tab| tab.layout.pane_ids())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            self.state.selected = index;
            self.state.close_selected_workspace();
            self.state.remove_plugin_pane_records(pane_ids);
            self.shutdown_detached_terminal_runtimes();
            self.emit_event(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::WorkspaceClosed,
                data: crate::api::schema::EventData::WorkspaceClosed {
                    workspace_id: public_id,
                    workspace: Some(info),
                },
            });
        }
    }

    /// Retries the closes that failed while this peer was unreachable.
    ///
    /// Runs when the peer reconnects, which is both the first moment a retry
    /// can succeed and a natural rate limit — the peer's own backoff governs
    /// how often this can happen. Records whose instance no longer matches are
    /// left where they are: the ids were issued by a server that is gone, and
    /// sending them to whatever replaced it could close an unrelated pane.
    ///
    /// Records are taken out before retrying and put back by the failure event,
    /// so a retry that fails again is retained exactly once rather than
    /// duplicated.
    pub(crate) fn retry_pending_pane_cleanups(&mut self, handle: &PeerHandle) {
        let Some(instance_id) = self
            .state
            .peers
            .get(handle)
            .and_then(crate::app::peers::PeerState::instance_id)
            .map(str::to_string)
        else {
            return;
        };
        let retryable = self
            .state
            .peers
            .take_retryable_pane_cleanups(handle, &instance_id);
        if retryable.is_empty() {
            return;
        }
        tracing::info!(
            peer = %handle,
            pending = retryable.len(),
            "retrying peer pane cleanups that failed while the peer was away"
        );
        for pending in retryable {
            self.close_spawned_peer_pane(
                handle.as_str(),
                &pending.peer_pane_id,
                pending.expected_instance,
            );
        }
    }

    /// Closes a pane on the peer that spawned it for a view we are tearing down.
    ///
    /// Fire and forget, off the event loop: the local pane is already gone and
    /// cannot be restored, so there is nothing a failure could roll back and
    /// nothing a caller could do about it *now*. A failure is retained against
    /// the peer with the pane id and the instance that issued it, so the next
    /// time that same server is reachable the close can be tried again — a
    /// remote shell, its process tree and its scrollback otherwise stay alive
    /// with nothing left pointing at them.
    ///
    /// `expected_instance` is what makes a retry safe rather than a guess: a
    /// peer-local pane id names the intended pane only while the server that
    /// issued it is answering.
    pub(crate) fn close_spawned_peer_pane(
        &self,
        peer: &str,
        peer_pane_id: &str,
        expected_instance: Option<String>,
    ) {
        let handle = PeerHandle::new(peer.to_string());
        let peer_pane_id = peer_pane_id.to_string();
        let event_tx = self.event_tx.clone();
        // Resolved here because peer state lives on the event loop; everything
        // that can block happens on the worker below.
        let socket = match self.state.peers.get(&handle) {
            None => Err("peer is no longer configured".to_string()),
            Some(state) if !state.connection.is_connected() => {
                Err(format!("peer is {}", state.connection.label()))
            }
            Some(state) => match (state.api_socket(), state.instance_id()) {
                (Some(api_socket), Some(instance_id)) => {
                    Ok((api_socket.to_path_buf(), instance_id.to_string()))
                }
                (None, _) => Err("peer has no transport".to_string()),
                (_, None) => Err("peer has no instance identity".to_string()),
            },
        };

        std::thread::spawn(move || {
            let failure = match socket {
                Ok((api_socket, instance_id)) => {
                    close_peer_pane(&api_socket, &peer_pane_id, &instance_id).err()
                }
                Err(reason) => Some(reason),
            };
            if let Some(reason) = failure {
                let _ = event_tx.blocking_send(crate::events::AppEvent::PeerPaneCleanupFailed {
                    handle,
                    peer_pane_id,
                    expected_instance,
                    reason,
                });
            }
        });
    }

    /// Disposes of a peer-backed view that never reached the layout, closing the
    /// pane on the peer when this server is the one that asked for it.
    ///
    /// `TerminalRuntime::shutdown` only detaches. The claim that closing a view
    /// closes the pane behind it is honoured in exactly one place —
    /// `shutdown_terminal_runtime` — and every path that disposes of a runtime
    /// without going through it leaks a live shell on the peer that nothing
    /// reaps and nobody can see. This is that one place for the runtimes a
    /// completion handler has to throw away.
    pub(crate) fn discard_spawned_peer_view(&mut self, runtime: crate::terminal::TerminalRuntime) {
        if let Some(spawned) = runtime.spawned_peer_pane() {
            let (peer, peer_pane_id, expected_instance) = (
                spawned.peer.to_string(),
                spawned.peer_pane_id.to_string(),
                spawned.expected_instance.map(str::to_string),
            );
            self.close_spawned_peer_pane(&peer, &peer_pane_id, expected_instance);
        }
        runtime.shutdown();
    }
}

/// Sends `pane.close` to a peer for a pane this server had it spawn.
///
/// Runs on a worker thread. A peer that already closed the pane itself reports
/// an error here; that is still a failed cleanup from this side, because the
/// only fact this server can act on is whether it got a confirmation.
pub(super) fn close_peer_pane(
    api_socket: &Path,
    peer_pane_id: &str,
    instance_id: &str,
) -> Result<(), String> {
    let client = crate::api::client::ApiClient::for_target(
        crate::api::client::ConnectionTarget::SocketPath(api_socket.to_path_buf()),
    );
    let request = Request {
        id: "peer:close-spawned".into(),
        method: Method::PaneClose(crate::api::schema::PaneTarget {
            pane_id: peer_pane_id.to_string(),
        }),
    };
    let value = client
        .request_value_for_instance_with_timeout(&request, instance_id, PEER_REQUEST_TIMEOUT)
        .map_err(|err| format!("peer request failed: {err}"))?;
    match value.get("error") {
        None => Ok(()),
        Some(error) => Err(error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("peer rejected the close")
            .to_string()),
    }
}

/// Closes a pane a worker had a peer create but could not open a view onto.
///
/// Runs on the same worker, because the alternative is carrying the pane id back
/// to the loop only to send it out again — and a worker that fails to connect
/// already holds everything the close needs. The failure is reported rather than
/// retried: there is no local state left to attach a retry to.
pub(super) fn close_abandoned_peer_pane(
    api_socket: &Path,
    handle: &PeerHandle,
    peer_pane_id: &str,
    instance_id: &str,
) {
    if let Err(reason) = close_peer_pane(api_socket, peer_pane_id, instance_id) {
        tracing::warn!(
            peer = %handle,
            pane = %peer_pane_id,
            reason = %reason,
            "could not close a pane this server had a peer create but never attached to"
        );
    }
}
