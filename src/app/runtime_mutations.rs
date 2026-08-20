use crate::api::schema::{
    EmptyParams, LayoutSetSplitRatioParams, Method, PaneFocusDirectionParams, PaneInputSetParams,
    PaneRenameParams, PaneResizeParams, PaneSplitParams, PaneSwapParams, PaneTarget,
    PaneZoomParams, TabCreateParams, TabMoveParams, TabRenameParams, TabTarget,
    WorkspaceCreateParams, WorkspaceMoveBlockParams, WorkspaceMoveParams, WorkspaceRenameParams,
    WorkspaceTarget, WorktreeCreateParams, WorktreeOpenParams, WorktreeRemoveParams,
};

use super::App;

/// What a UI-initiated runtime mutation produced *here*.
///
/// [`Self::Accepted`] is not a success. It says the request targets a workspace
/// this server only holds a view onto, so it left for the peer that owns it and
/// is answered on a worker seconds later — see [`App::peer_forward_reporter`]
/// for where that answer goes.
///
/// This used to be an empty string, which reads as neither a success nor an
/// error to anything that parses a response. Both callers that inspect one were
/// shaped by that: `submit_worktree_open_via_api` had to carry its own
/// `on_peer` flag to tell a routed open from a handler that returned nothing,
/// and `submit_peer_workspace_open_via_api` — whose request is *always* routed
/// — reached two parses that could never match and fell out of the bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeMutation {
    /// Answered by this server: the serialized API response.
    Answered(String),
    /// Routed to the peer that owns the target.
    Accepted,
}

impl RuntimeMutation {
    /// The response this server produced, or `None` when a peer owes one.
    pub(crate) fn answered(&self) -> Option<&str> {
        match self {
            Self::Answered(response) => Some(response),
            Self::Accepted => None,
        }
    }
}

/// The failure in a forwarded peer response, if it failed.
///
/// A success needs no telling: the peer's own event stream drives the
/// re-enumeration that shows it.
fn peer_forward_error_message(response: &str) -> Option<String> {
    if response.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(response).ok()?;
    let error = value.get("error")?;
    Some(
        error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("the peer refused the request")
            .to_string(),
    )
}

impl App {
    /// Runs a mutation the UI asked for, routing it to a peer when the
    /// workspace it targets is a view onto one.
    ///
    /// The socket path gates every request on
    /// [`App::request_targets_peer_workspace`] before handling it locally
    /// (`src/app/runtime.rs`, `src/server/headless.rs`). In-process UI actions
    /// never reached that gate, so a new tab in a peer-backed workspace spawned
    /// a local pty inside the workspace the user opened precisely to work on
    /// another machine. Gating here rather than at each call site keeps the two
    /// entry points from drifting again: a keybind, the tab-name dialog and the
    /// `+` button all pass through this one function.
    pub(crate) fn dispatch_runtime_mutation(
        &mut self,
        id: &'static str,
        method: Method,
    ) -> RuntimeMutation {
        let request = crate::api::schema::Request {
            id: id.to_string(),
            method,
        };
        if !self.request_targets_peer_workspace(&request) {
            return RuntimeMutation::Answered(self.handle_api_request(request));
        }
        let respond_to = self.peer_forward_reporter();
        self.handle_deferred_peer_workspace_api_request(request, respond_to);
        // Deferred exactly as a peer-backed split is: the answer arrives on a
        // worker, seconds later, so nothing useful can be returned here. It is
        // not dropped — see `peer_forward_reporter`.
        RuntimeMutation::Accepted
    }

    pub(crate) fn dispatch_deferred_runtime_mutation(
        &mut self,
        id: &'static str,
        method: Method,
    ) -> Option<String> {
        self.dispatch_deferred_api_request(id, method)
    }

    pub(crate) fn runtime_workspace_focus(
        &mut self,
        id: &'static str,
        workspace_id: String,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::WorkspaceFocus(WorkspaceTarget { workspace_id }))
    }

    pub(crate) fn runtime_workspace_create(
        &mut self,
        id: &'static str,
        params: WorkspaceCreateParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::WorkspaceCreate(params))
    }

    pub(crate) fn runtime_workspace_rename(
        &mut self,
        id: &'static str,
        params: WorkspaceRenameParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::WorkspaceRename(params))
    }

    pub(crate) fn runtime_workspace_move(
        &mut self,
        id: &'static str,
        params: WorkspaceMoveParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::WorkspaceMove(params))
    }

    pub(crate) fn runtime_workspace_move_block(
        &mut self,
        id: &'static str,
        params: WorkspaceMoveBlockParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::WorkspaceMoveBlock(params))
    }

    pub(crate) fn runtime_workspace_close(
        &mut self,
        id: &'static str,
        workspace_id: String,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::WorkspaceClose(WorkspaceTarget { workspace_id }))
    }

    pub(crate) fn runtime_tab_create(
        &mut self,
        id: &'static str,
        params: TabCreateParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::TabCreate(params))
    }

    pub(crate) fn runtime_tab_focus(
        &mut self,
        id: &'static str,
        tab_id: String,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::TabFocus(TabTarget { tab_id }))
    }

    pub(crate) fn runtime_tab_rename(
        &mut self,
        id: &'static str,
        params: TabRenameParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::TabRename(params))
    }

    pub(crate) fn runtime_tab_move(
        &mut self,
        id: &'static str,
        params: TabMoveParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::TabMove(params))
    }

    pub(crate) fn runtime_tab_close(
        &mut self,
        id: &'static str,
        tab_id: String,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::TabClose(TabTarget { tab_id }))
    }

    pub(crate) fn runtime_server_reload_config(&mut self, id: &'static str) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::ServerReloadConfig(EmptyParams::default()))
    }

    pub(crate) fn runtime_pane_focus(
        &mut self,
        id: &'static str,
        pane_id: String,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::PaneFocus(PaneTarget { pane_id }))
    }

    pub(crate) fn runtime_pane_close(
        &mut self,
        id: &'static str,
        pane_id: String,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::PaneClose(PaneTarget { pane_id }))
    }

    pub(crate) fn runtime_pane_rename(
        &mut self,
        id: &'static str,
        params: PaneRenameParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::PaneRename(params))
    }

    pub(crate) fn runtime_pane_input_set(
        &mut self,
        id: &'static str,
        params: PaneInputSetParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::PaneInputSet(params))
    }

    pub(crate) fn runtime_pane_focus_direction(
        &mut self,
        id: &'static str,
        params: PaneFocusDirectionParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::PaneFocusDirection(params))
    }

    pub(crate) fn runtime_pane_resize(
        &mut self,
        id: &'static str,
        params: PaneResizeParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::PaneResize(params))
    }

    pub(crate) fn runtime_pane_swap(
        &mut self,
        id: &'static str,
        params: PaneSwapParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::PaneSwap(params))
    }

    /// Splits a pane. Splitting a peer-backed pane is `Accepted` rather than
    /// answered: the split itself lands when the peer replies.
    ///
    /// Note the gate here is the peer *pane*, not the peer workspace
    /// `dispatch_runtime_mutation` tests — a split targets a local pane that
    /// happens to be backed by one.
    pub(crate) fn runtime_pane_split(
        &mut self,
        id: &'static str,
        params: PaneSplitParams,
    ) -> RuntimeMutation {
        let request = crate::api::schema::Request {
            id: id.to_string(),
            method: Method::PaneSplit(params),
        };
        if !self.request_targets_peer_pane(&request) {
            return self.dispatch_runtime_mutation(id, request.method);
        }
        let respond_to = self.peer_forward_reporter();
        self.handle_deferred_peer_pane_api_request(request, respond_to);
        RuntimeMutation::Accepted
    }

    /// A response channel for a forward the *UI* started, whose failures reach
    /// the user instead of a dropped receiver.
    ///
    /// A socket caller is waiting on the other end of its own channel, so its
    /// answer goes back to it. A UI action has no such caller: the old code
    /// built a channel, `try_recv`'d it before the worker could possibly have
    /// answered, and dropped the receiver — so a peer's rejection of a close or
    /// a rename went nowhere and the user saw the workspace reappear on the
    /// peer's next enumeration with no explanation.
    ///
    /// The loop must still not wait, which is why this is a callback rather than
    /// a longer read: a thread parks on the receiver, and the outcome comes back
    /// as an ordinary event.
    fn peer_forward_reporter(&self) -> std::sync::mpsc::Sender<String> {
        let (respond_to, response_rx) = std::sync::mpsc::channel::<String>();
        let event_tx = self.event_tx.clone();
        std::thread::Builder::new()
            .name("herdr-peer-forward-report".to_string())
            .spawn(move || {
                // Ends when the sender is dropped, which every path does: the
                // deferred handler either answers directly or hands it to the
                // worker that will.
                let Ok(response) = response_rx.recv() else {
                    return;
                };
                let Some(message) = peer_forward_error_message(&response) else {
                    return;
                };
                let _ =
                    event_tx.blocking_send(crate::events::AppEvent::PeerForwardFailed { message });
            })
            .ok();
        respond_to
    }

    pub(crate) fn runtime_pane_zoom(
        &mut self,
        id: &'static str,
        params: PaneZoomParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::PaneZoom(params))
    }

    pub(crate) fn runtime_layout_set_split_ratio(
        &mut self,
        id: &'static str,
        params: LayoutSetSplitRatioParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::LayoutSetSplitRatio(params))
    }

    pub(crate) fn runtime_worktree_create_deferred(
        &mut self,
        id: &'static str,
        params: WorktreeCreateParams,
    ) -> Option<String> {
        self.dispatch_deferred_runtime_mutation(id, Method::WorktreeCreate(params))
    }

    pub(crate) fn runtime_worktree_open(
        &mut self,
        id: &'static str,
        params: WorktreeOpenParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::WorktreeOpen(params))
    }

    pub(crate) fn runtime_peer_workspace_open(
        &mut self,
        id: &'static str,
        params: crate::api::schema::PeerWorkspaceOpenParams,
    ) -> RuntimeMutation {
        self.dispatch_runtime_mutation(id, Method::PeerWorkspaceOpen(params))
    }

    pub(crate) fn runtime_worktree_remove_deferred(
        &mut self,
        id: &'static str,
        params: WorktreeRemoveParams,
    ) -> Option<String> {
        self.dispatch_deferred_runtime_mutation(id, Method::WorktreeRemove(params))
    }
}
