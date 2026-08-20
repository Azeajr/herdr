//! Routing `worktree.*` to the peer that owns the checkout.
//!
//! Depends on `resolve` for the peer behind a view and on `forward`'s worker
//! shape for everything that has to be asked.
//!
//! A peer view's cwd is a path on another machine. Running `git worktree` here
//! against it either fails or — worse, when the same path also exists locally —
//! succeeds against the wrong host's repo. So none of these are answered here:
//! every one is the peer's own `worktree.*` call, and what comes back is
//! restated in local ids.
//!
//! Creating and opening produce a workspace *on the peer*, which is no use on
//! its own: this server has to be able to see it. Both therefore chain the
//! ordinary peer view open onto the pane the peer's reply already names, exactly
//! as `peer.workspace.create` does, and answer with the local view plus the
//! peer's account of the checkout.

use std::collections::HashMap;

use super::*;
use crate::api::schema::{
    EventData, EventEnvelope, EventKind, WorkspaceWorktreeInfo, WorktreeCreateParams, WorktreeInfo,
    WorktreeListParams, WorktreeOpenParams, WorktreeRemoveParams,
};
use crate::events::PeerWorktreeAnswer;

/// The peer workspace a routed worktree request acts on.
pub(super) struct PeerWorktreeTarget {
    handle: PeerHandle,
    /// The peer's own workspace id behind the view. Peer-local, never
    /// namespaced: it is what the peer knows its workspace by.
    peer_workspace: String,
    /// The local workspace holding the view, by local id rather than index so a
    /// completion can re-resolve it after the peer round trip.
    workspace_id: String,
}

impl App {
    /// Which local workspace a `worktree.*` request means.
    ///
    /// The same resolution the local handlers use, so the routing gate and the
    /// local path can never disagree about which workspace is being asked for.
    fn worktree_request_workspace_index(&self, workspace_id: Option<&str>) -> Option<usize> {
        match workspace_id {
            Some(workspace_id) => self.parse_workspace_id(workspace_id),
            None => self.state.active.or_else(|| {
                self.state
                    .workspaces
                    .get(self.state.selected)
                    .map(|_| self.state.selected)
            }),
        }
    }

    /// Whether a worktree request acts on a workspace that is a view onto a peer.
    ///
    /// A `cwd` never qualifies: it names a path on *this* filesystem, which is
    /// what the caller asked about even when a peer view happens to be active.
    pub(super) fn worktree_request_targets_peer(
        &self,
        workspace_id: Option<&str>,
        cwd: Option<&str>,
    ) -> bool {
        if cwd.is_some() {
            return false;
        }
        self.worktree_request_workspace_index(workspace_id)
            .and_then(|ws_idx| self.state.workspaces.get(ws_idx))
            .is_some_and(|ws| ws.peer.is_some())
    }

    /// The peer and peer-side workspace a routed worktree request must reach.
    fn resolve_peer_worktree_target(
        &self,
        workspace_id: Option<&str>,
    ) -> Result<PeerWorktreeTarget, (&'static str, String)> {
        let Some(ws_idx) = self.worktree_request_workspace_index(workspace_id) else {
            return Err((
                "workspace_not_found",
                "no workspace to run a worktree action in".to_string(),
            ));
        };
        let Some(workspace) = self.state.workspaces.get(ws_idx) else {
            return Err(("workspace_not_found", "workspace not found".to_string()));
        };
        let Some(peer) = workspace.peer.clone() else {
            return Err((
                "invalid_request",
                "workspace is not a view onto a peer".to_string(),
            ));
        };
        // A view onto a bare pane never learned which workspace holds it, so
        // there is no cwd on the peer to start a worktree action from. Say that
        // rather than silently running git on the wrong machine.
        let Some(peer_workspace) = workspace.peer_workspace.clone() else {
            return Err((
                "invalid_request",
                "this view does not name a workspace on its peer".to_string(),
            ));
        };
        Ok(PeerWorktreeTarget {
            handle: PeerHandle::new(peer),
            peer_workspace,
            workspace_id: workspace.id.clone(),
        })
    }

    /// The peer's own account of the worktree behind a view, restated in the
    /// shape `worktree.*` answers use.
    ///
    /// The branch is absent rather than guessed: a peer's workspace enumeration
    /// says which checkout it is, not which ref that checkout has out.
    fn peer_view_worktree_info(&self, ws_idx: usize) -> Option<WorktreeInfo> {
        let space = self.state.peer_view_worktree_space(ws_idx)?;
        Some(WorktreeInfo {
            path: space.checkout_path.clone(),
            branch: None,
            is_bare: false,
            is_detached: false,
            is_prunable: false,
            is_linked_worktree: space.is_linked_worktree,
            open_workspace_id: Some(self.public_workspace_id(ws_idx)),
            label: space.repo_name.clone(),
        })
    }

    /// The local workspace ids this server holds views under, keyed by the
    /// peer-local workspace id each view is attached to.
    ///
    /// Built before the round trip because the answer has to be restated the
    /// moment it arrives, on a worker that cannot read app state.
    fn local_workspace_ids_for_peer(&self, handle: &PeerHandle) -> HashMap<String, String> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, ws)| ws.peer.as_deref() == Some(handle.as_str()))
            .filter_map(|(ws_idx, ws)| {
                ws.peer_workspace
                    .clone()
                    .map(|peer_workspace| (peer_workspace, self.public_workspace_id(ws_idx)))
            })
            .collect()
    }

    /// Lists the peer's worktrees for the repo its workspace sits in.
    ///
    /// Deferred rather than answered on the loop for the same reason a read is:
    /// `git worktree list` runs on the peer, behind a socket that may be slow or
    /// gone, and the loop must never be the thread that waits for it.
    pub(super) fn start_peer_worktree_list(
        &mut self,
        id: String,
        params: WorktreeListParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let target = match self.resolve_peer_worktree_target(params.workspace_id.as_deref()) {
            Ok(target) => target,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };
        let PeerConnection {
            local_target,
            api_socket,
            instance_id,
        } = match self.resolve_peer_connection(&target.peer_workspace, Some(target.handle.as_str()))
        {
            Ok(connection) => connection,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };

        let local_ids = self.local_workspace_ids_for_peer(&target.handle);
        let peer_request = Method::WorktreeList(WorktreeListParams {
            workspace_id: Some(local_target),
            cwd: None,
        });
        std::thread::spawn(move || {
            let _ = respond_to.send(forward_worktree_list_to_peer(
                &api_socket,
                &id,
                peer_request,
                &instance_id,
                &local_ids,
            ));
        });
    }

    /// Asks a peer to create a worktree of its own, then opens a view onto it.
    ///
    /// The checkout, the branch and the directory it lands in are all the peer's:
    /// this server's `worktree_directory` names a path on the wrong machine, so
    /// an absent `path` is left absent for the peer to fill in rather than
    /// defaulted here.
    pub(super) fn start_peer_worktree_create(
        &mut self,
        id: String,
        params: WorktreeCreateParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let target = match self.resolve_peer_worktree_target(params.workspace_id.as_deref()) {
            Ok(target) => target,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };
        let PeerConnection {
            local_target,
            api_socket,
            instance_id,
        } = match self.resolve_peer_connection(&target.peer_workspace, Some(target.handle.as_str()))
        {
            Ok(connection) => connection,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };

        // Focus and label stay local: the workspace the peer makes is one it
        // owns, and moving the focus of whoever is at the peer is the same
        // refusal `pane.split` already makes for the same reason.
        let peer_request = Method::WorktreeCreate(WorktreeCreateParams {
            workspace_id: Some(local_target),
            cwd: None,
            branch: params.branch,
            base: params.base,
            path: params.path,
            label: None,
            focus: false,
        });

        let handle = target.handle;
        let label = params.label;
        let focus = params.focus;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result =
                request_peer_worktree_view(&api_socket, peer_request, &handle, &instance_id, false);
            let _ = event_tx.blocking_send(crate::events::AppEvent::PeerWorktreeViewFinished(
                Box::new(crate::events::PeerWorktreeViewResult {
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

    /// Asks a peer to open one of its existing worktrees, then views it.
    ///
    /// `path` and `branch` name the peer's filesystem and the peer's refs, so
    /// both are passed through untouched; validating either here would be
    /// validating against the wrong machine.
    pub(super) fn start_peer_worktree_open(
        &mut self,
        id: String,
        params: WorktreeOpenParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let target = match self.resolve_peer_worktree_target(params.workspace_id.as_deref()) {
            Ok(target) => target,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };
        let PeerConnection {
            local_target,
            api_socket,
            instance_id,
        } = match self.resolve_peer_connection(&target.peer_workspace, Some(target.handle.as_str()))
        {
            Ok(connection) => connection,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };

        let peer_request = Method::WorktreeOpen(WorktreeOpenParams {
            workspace_id: Some(local_target),
            cwd: None,
            path: params.path,
            branch: params.branch,
            label: None,
            focus: false,
        });

        let handle = target.handle;
        let label = params.label;
        let focus = params.focus;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result =
                request_peer_worktree_view(&api_socket, peer_request, &handle, &instance_id, true);
            let _ = event_tx.blocking_send(crate::events::AppEvent::PeerWorktreeViewFinished(
                Box::new(crate::events::PeerWorktreeViewResult {
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

    /// Asks a peer to remove the worktree checkout one of our views sits in.
    ///
    /// The peer closes its own workspace as part of removing the checkout, which
    /// leaves this view attached to a pane that no longer exists — so the local
    /// view is closed when the peer confirms, rather than left to time out.
    pub(super) fn start_peer_worktree_remove(
        &mut self,
        id: String,
        params: WorktreeRemoveParams,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let target = match self.resolve_peer_worktree_target(Some(&params.workspace_id)) {
            Ok(target) => target,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };
        let PeerConnection {
            local_target,
            api_socket,
            instance_id,
        } = match self.resolve_peer_connection(&target.peer_workspace, Some(target.handle.as_str()))
        {
            Ok(connection) => connection,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };

        let peer_request = Method::WorktreeRemove(WorktreeRemoveParams {
            workspace_id: local_target,
            force: params.force,
        });

        let workspace_id = target.workspace_id;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result = request_peer_worktree_removal(&api_socket, peer_request, &instance_id);
            let _ = event_tx.blocking_send(crate::events::AppEvent::PeerWorktreeRemoveFinished(
                Box::new(crate::events::PeerWorktreeRemoveResult {
                    id,
                    workspace_id,
                    result,
                    respond_to,
                }),
            ));
        });
    }

    /// Answers a routed `worktree.create`/`worktree.open` once its view exists.
    ///
    /// The workspace, tab and pane are this server's — the caller asked here and
    /// has to be able to use the ids it gets back — while the checkout is
    /// reported exactly as the peer described it, with only `open_workspace_id`
    /// restated, because that is the one field naming something local.
    ///
    /// The matching event is emitted here too, so a client watching this server
    /// sees `worktree.created`/`worktree.opened` for a peer checkout on the same
    /// terms as a local one.
    pub(super) fn peer_worktree_view_response(
        &mut self,
        id: String,
        ws_idx: usize,
        answer: PeerWorktreeAnswer,
    ) -> String {
        let Some(tab_info) = self.tab_info(ws_idx, self.state.workspaces[ws_idx].active_tab) else {
            return encode_error(
                id,
                "worktree_open_failed",
                "the view onto the peer's worktree has no tab",
            );
        };
        let Some(root_pane) = self.root_pane_info(ws_idx, self.state.workspaces[ws_idx].active_tab)
        else {
            return encode_error(
                id,
                "worktree_open_failed",
                "the view onto the peer's worktree has no pane",
            );
        };
        let workspace = self.workspace_info(ws_idx);
        let result = match answer {
            PeerWorktreeAnswer::Created { worktree } => {
                let worktree = self.local_peer_worktree_info(ws_idx, worktree);
                self.clear_worktree_create_dialog();
                self.emit_worktree_created_event(ws_idx, worktree.clone());
                ResponseResult::WorktreeCreated {
                    workspace,
                    tab: tab_info,
                    root_pane,
                    worktree,
                }
            }
            PeerWorktreeAnswer::Opened {
                worktree,
                already_open,
            } => {
                let worktree = self.local_peer_worktree_info(ws_idx, worktree);
                self.emit_event(EventEnvelope {
                    event: EventKind::WorktreeOpened,
                    data: EventData::WorktreeOpened {
                        workspace: self.workspace_info(ws_idx),
                        worktree: worktree.clone(),
                        already_open,
                    },
                });
                ResponseResult::WorktreeOpened {
                    workspace,
                    tab: tab_info,
                    root_pane,
                    worktree,
                    already_open,
                }
            }
        };
        encode_success(id, result)
    }

    /// The peer's checkout, with the one field that names a workspace restated
    /// as the local view now holding it.
    fn local_peer_worktree_info(&self, ws_idx: usize, mut worktree: WorktreeInfo) -> WorktreeInfo {
        worktree.open_workspace_id = Some(self.public_workspace_id(ws_idx));
        worktree
    }

    /// Opens the view onto the worktree workspace a peer just created or opened.
    pub(crate) fn handle_peer_worktree_view_finished(
        &mut self,
        finished: crate::events::PeerWorktreeViewResult,
    ) {
        let crate::events::PeerWorktreeViewResult {
            id,
            handle,
            label,
            focus,
            result,
            respond_to,
        } = finished;
        let (peer_pane_id, answer) = match result {
            Ok(resolved) => resolved,
            Err((code, message)) => {
                self.report_peer_worktree_create_failed(&message);
                let _ = respond_to.send(encode_error(id, &code, message));
                return;
            }
        };

        // The ordinary open path, so this view reconnects, dedupes and reports
        // exactly like one opened from the picker; only the answer differs. The
        // deferred variant, because this handler runs on the event loop and
        // connecting is the step that must never run there.
        self.start_peer_workspace_open(
            id,
            peer_pane_id,
            Some(handle.as_str()),
            label,
            focus,
            false,
            Some(answer),
            respond_to,
        );
    }

    /// Closes the view whose checkout a peer just removed, and answers.
    pub(crate) fn handle_peer_worktree_remove_finished(
        &mut self,
        finished: crate::events::PeerWorktreeRemoveResult,
    ) {
        let crate::events::PeerWorktreeRemoveResult {
            id,
            workspace_id,
            result,
            respond_to,
        } = finished;
        let (path, forced) = match result {
            Ok(removed) => removed,
            Err((code, message)) => {
                self.report_peer_worktree_remove_failed(&message);
                let _ = respond_to.send(encode_error(id, &code, message));
                return;
            }
        };

        // The workspace list can change while the peer round trip is in flight,
        // so the view is re-resolved by id. Its pane is gone on the peer either
        // way, so a view that is still here is closed rather than left to
        // rediscover that on its own.
        let ws_idx = self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.id == workspace_id);
        let (public_workspace_id, workspace_snapshot, worktree) = match ws_idx {
            Some(ws_idx) => {
                let public_workspace_id = self.public_workspace_id(ws_idx);
                let snapshot = self.workspace_info(ws_idx);
                let worktree = self.peer_view_worktree_info(ws_idx);
                self.state.selected = ws_idx;
                self.state.close_selected_workspace();
                self.shutdown_detached_terminal_runtimes();
                self.emit_event(EventEnvelope {
                    event: EventKind::WorkspaceClosed,
                    data: EventData::WorkspaceClosed {
                        workspace_id: public_workspace_id.clone(),
                        workspace: Some(snapshot.clone()),
                    },
                });
                (public_workspace_id, Some(snapshot), worktree)
            }
            None => (workspace_id, None, None),
        };

        self.clear_worktree_remove_dialog();
        let worktree = worktree.unwrap_or_else(|| removed_peer_worktree_info(&path));
        self.emit_worktree_removed_event(
            public_workspace_id.clone(),
            workspace_snapshot,
            worktree,
            forced,
        );
        let _ = respond_to.send(encode_success(
            id,
            ResponseResult::WorktreeRemoved {
                workspace_id: public_workspace_id,
                path,
                forced,
            },
        ));
    }
}

/// Sends a `worktree.create`/`worktree.open` to a peer and reads back both the
/// pane a view must attach to and the peer's account of the checkout.
///
/// `root_pane`, not the workspace id, for the same reason `peer.workspace.create`
/// reads one: the reply already names the pane, so nothing has to wait for the
/// peer's next enumeration before the view can exist.
fn request_peer_worktree_view(
    api_socket: &Path,
    method: Method,
    handle: &PeerHandle,
    instance_id: &str,
    opened: bool,
) -> Result<(String, Box<PeerWorktreeAnswer>), (String, String)> {
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
    let what = if opened {
        "the worktree open"
    } else {
        "the worktree"
    };
    let pane_id = peer_pane_id_at(&value, "root_pane", handle, what)?;
    let worktree = value
        .get("result")
        .and_then(|result| result.get("worktree"))
        .cloned()
        .and_then(|worktree| {
            serde_json::from_value::<crate::api::schema::WorktreeInfo>(worktree).ok()
        })
        .ok_or_else(|| {
            (
                "unavailable".to_string(),
                format!("peer '{handle}' returned no worktree for {what}"),
            )
        })?;
    let answer = if opened {
        // Whether the *peer* already had it open says nothing about this server,
        // which is what `already_open` reports; the open path decides that when
        // it looks for a view it already holds.
        PeerWorktreeAnswer::Opened {
            worktree,
            already_open: false,
        }
    } else {
        PeerWorktreeAnswer::Created { worktree }
    };
    Ok((pane_id, Box::new(answer)))
}

/// Sends a `worktree.remove` to a peer and reads back what it removed.
fn request_peer_worktree_removal(
    api_socket: &Path,
    method: Method,
    instance_id: &str,
) -> Result<(String, bool), (String, String)> {
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
    // A peer error is reported as-is: its code says whether the checkout was
    // dirty or the workspace was not a worktree at all, and inventing a local
    // code here would hide the one answer the caller needs.
    if let Some(error) = value.get("error") {
        let code = error
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unavailable")
            .to_string();
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "peer refused the worktree removal".to_string());
        return Err((code, message));
    }
    let result = value.get("result").ok_or_else(|| {
        (
            "unavailable".to_string(),
            "peer returned no result for the worktree removal".to_string(),
        )
    })?;
    let path = result
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            (
                "unavailable".to_string(),
                "peer named no path for the removed worktree".to_string(),
            )
        })?;
    let forced = result
        .get("forced")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok((path, forced))
}

/// Forwards a `worktree.list` to the peer and restates the workspace ids in it.
///
/// Only ids are rewritten, never the paths or branches: the repo is the peer's
/// and is reported verbatim. A checkout the peer has open becomes the local view
/// onto it when this server holds one, and the peer-namespaced id otherwise — so
/// every id that comes back is one the caller can act on here.
fn forward_worktree_list_to_peer(
    api_socket: &Path,
    request_id: &str,
    method: Method,
    instance_id: &str,
    local_ids: &HashMap<String, String>,
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
            rewrite_forwarded_worktree_list(&mut value, request_id, instance_id, local_ids);
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

fn rewrite_forwarded_worktree_list(
    value: &mut serde_json::Value,
    request_id: &str,
    instance_id: &str,
    local_ids: &HashMap<String, String>,
) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    obj.insert(
        "id".to_string(),
        serde_json::Value::String(request_id.to_string()),
    );
    let Some(result) = obj
        .get_mut("result")
        .and_then(|result| result.as_object_mut())
    else {
        return;
    };
    if let Some(source) = result
        .get_mut("source")
        .and_then(|source| source.as_object_mut())
    {
        rewrite_peer_workspace_id_field(source, "source_workspace_id", instance_id, local_ids);
    }
    let Some(worktrees) = result
        .get_mut("worktrees")
        .and_then(|worktrees| worktrees.as_array_mut())
    else {
        return;
    };
    for worktree in worktrees {
        if let Some(worktree) = worktree.as_object_mut() {
            rewrite_peer_workspace_id_field(worktree, "open_workspace_id", instance_id, local_ids);
        }
    }
}

fn rewrite_peer_workspace_id_field(
    object: &mut serde_json::Map<String, serde_json::Value>,
    field: &str,
    instance_id: &str,
    local_ids: &HashMap<String, String>,
) {
    let Some(serde_json::Value::String(peer_workspace_id)) = object.get(field) else {
        return;
    };
    let restated = local_ids
        .get(peer_workspace_id)
        .cloned()
        .unwrap_or_else(|| crate::app::peers::prefix_peer_id(instance_id, peer_workspace_id));
    object.insert(field.to_string(), serde_json::Value::String(restated));
}

impl crate::app::state::AppState {
    /// The peer's own account of the worktree the workspace behind a view sits
    /// in, as the peer reported it when enumerating.
    ///
    /// This is the whole reason the git menu can be offered on a peer view at
    /// all: the checkout is on the other machine and nothing here can stat it,
    /// but the peer already says — in every enumeration — which repo its
    /// workspace belongs to and whether that workspace is a linked checkout.
    /// Reading it costs no round trip and stays answerable while the peer is
    /// down, which is what a menu built during a right-click needs.
    ///
    /// On `AppState` rather than `App` because the mouse layer builds the menu
    /// from state alone, and this must be the same answer the routed request
    /// later acts on.
    pub(crate) fn peer_view_worktree_space(&self, ws_idx: usize) -> Option<&WorkspaceWorktreeInfo> {
        let workspace = self.workspaces.get(ws_idx)?;
        let handle = PeerHandle::new(workspace.peer.clone()?);
        let peer_workspace = workspace.peer_workspace.as_deref()?;
        self.peers
            .get(&handle)?
            .workspaces
            .iter()
            .find(|info| {
                crate::app::peers::split_peer_id(&info.workspace_id)
                    .is_some_and(|(_, local)| local == peer_workspace)
            })
            .and_then(|info| info.worktree.as_ref())
    }
}

/// What is known about a removed peer checkout once the view holding it is gone.
///
/// The peer no longer has the workspace to describe, and this server never had
/// the checkout, so only the path it reported survives.
fn removed_peer_worktree_info(path: &str) -> crate::api::schema::WorktreeInfo {
    crate::api::schema::WorktreeInfo {
        path: path.to_string(),
        branch: None,
        is_bare: false,
        is_detached: false,
        is_prunable: false,
        is_linked_worktree: true,
        open_workspace_id: None,
        label: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_list_response() -> serde_json::Value {
        serde_json::json!({
            "id": "peer:forward",
            "result": {
                "source": {
                    "repo_key": "repo-key",
                    "repo_name": "herdr",
                    "repo_root": "/home/b/herdr",
                    "source_checkout_path": "/home/b/herdr",
                    "source_workspace_id": "w1"
                },
                "worktrees": [
                    {
                        "path": "/home/b/herdr",
                        "branch": "master",
                        "is_bare": false,
                        "is_detached": false,
                        "is_prunable": false,
                        "is_linked_worktree": false,
                        "open_workspace_id": "w1",
                        "label": "herdr"
                    },
                    {
                        "path": "/home/b/worktrees/herdr/issue-3",
                        "branch": "worktree/issue-3",
                        "is_bare": false,
                        "is_detached": false,
                        "is_prunable": false,
                        "is_linked_worktree": true,
                        "open_workspace_id": "w4",
                        "label": "herdr"
                    },
                    {
                        "path": "/home/b/worktrees/herdr/spare",
                        "branch": "worktree/spare",
                        "is_bare": false,
                        "is_detached": false,
                        "is_prunable": false,
                        "is_linked_worktree": true,
                        "label": "herdr"
                    }
                ]
            }
        })
    }

    /// Every workspace id in a forwarded list has to be one the caller can act
    /// on here: the local view when this server holds one, and the namespaced
    /// peer id — which `workspace.focus` opens a view for — when it does not.
    /// A peer's own `w1` means nothing on this side and would resolve to an
    /// unrelated local workspace.
    #[test]
    fn a_forwarded_worktree_list_restates_every_workspace_id() {
        let mut value = peer_list_response();
        let local_ids = HashMap::from([("w1".to_string(), "w7".to_string())]);

        rewrite_forwarded_worktree_list(&mut value, "req-9", "instance-b", &local_ids);

        assert_eq!(value["id"], "req-9");
        // Viewed here, so it names the local view.
        assert_eq!(value["result"]["source"]["source_workspace_id"], "w7");
        assert_eq!(value["result"]["worktrees"][0]["open_workspace_id"], "w7");
        // Open on the peer but not viewed here, so it names the peer's.
        assert_eq!(
            value["result"]["worktrees"][1]["open_workspace_id"],
            crate::app::peers::prefix_peer_id("instance-b", "w4")
        );
        // Not open anywhere: absent stays absent rather than becoming a guess.
        assert!(value["result"]["worktrees"][2]
            .get("open_workspace_id")
            .is_none());
        // The peer's paths and branches are its own and are reported verbatim.
        assert_eq!(value["result"]["worktrees"][0]["path"], "/home/b/herdr");
        assert_eq!(
            value["result"]["worktrees"][1]["branch"],
            "worktree/issue-3"
        );
        assert_eq!(value["result"]["source"]["repo_root"], "/home/b/herdr");
    }

    /// A peer that refuses the list is reported as refusing, not as an empty
    /// repo: "no worktrees" and "the peer did not answer" are different facts.
    #[test]
    fn a_refused_worktree_list_keeps_the_peers_error() {
        let mut value = serde_json::json!({
            "id": "peer:forward",
            "error": { "code": "not_git_worktree", "message": "not a Git work tree" }
        });

        rewrite_forwarded_worktree_list(&mut value, "req-9", "instance-b", &HashMap::new());

        assert_eq!(value["id"], "req-9");
        assert_eq!(value["error"]["code"], "not_git_worktree");
    }
}
