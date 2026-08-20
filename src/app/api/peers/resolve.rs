//! Naming a peer, and the ids that name its things.
//!
//! Depends on peer state only. Every question here is answerable from what this
//! server already knows — which peer owns an id, whether a view onto it is
//! already open — so none of it needs a live connection, and all of it stays
//! answerable while a peer is down.

use super::*;

impl App {
    /// Resolves which peer owns `target`, and the id that peer knows it by.
    ///
    /// A target may arrive namespaced (as enumerated ids are) or bare. The
    /// namespace names its own peer, so `name` is only required for a bare
    /// target. Either way the namespace is stripped: the peer only knows its
    /// own local ids.
    ///
    /// Kept apart from [`Self::resolve_peer_connection`] because naming a peer
    /// needs no connection to it. Questions answered from local state — is a
    /// view onto this already open — must stay answerable while the peer is
    /// down.
    pub(super) fn resolve_peer_target(
        &self,
        target: &str,
        name: Option<&str>,
    ) -> Result<(PeerHandle, String), (&'static str, String)> {
        let namespaced = crate::app::peers::split_peer_id(target);
        Ok(match (name, namespaced) {
            (Some(name), Some((instance_id, local_id))) => {
                let handle = PeerHandle::new(name.trim().to_string());
                match self.state.peers.get(&handle) {
                    Some(peer) if peer.instance_id() == Some(instance_id) => {
                        (handle, local_id.to_string())
                    }
                    Some(_) => {
                        return Err((
                            "invalid_request",
                            format!("'{target}' does not belong to peer '{handle}'"),
                        ))
                    }
                    None => return Err(("not_found", format!("no peer named '{handle}'"))),
                }
            }
            (Some(name), None) => (PeerHandle::new(name.trim().to_string()), target.to_string()),
            (None, Some(_)) => match self.state.peers.resolve_peer_id(target) {
                Some((peer, local_id)) => (peer.handle.clone(), local_id.to_string()),
                None => return Err(("not_found", format!("no connected peer owns '{target}'"))),
            },
            (None, None) => {
                return Err((
                    "invalid_request",
                    "name is required when the target is not namespaced".to_string(),
                ))
            }
        })
    }

    /// Resolves which peer owns `target` and how to reach it right now.
    pub(super) fn resolve_peer_connection(
        &self,
        target: &str,
        name: Option<&str>,
    ) -> Result<PeerConnection, (&'static str, String)> {
        let (handle, local_target) = self.resolve_peer_target(target, name)?;

        let Some(peer) = self.state.peers.get(&handle) else {
            return Err(("not_found", format!("no peer named '{handle}'")));
        };
        if !peer.connection.is_connected() {
            return Err((
                "unavailable",
                format!("peer '{handle}' is {}", peer.connection.label()),
            ));
        }
        let Some(api_socket) = peer.api_socket() else {
            return Err((
                "unavailable",
                format!("peer '{handle}' has no transport yet"),
            ));
        };
        let Some(instance_id) = peer.instance_id() else {
            return Err((
                "unavailable",
                format!("peer '{handle}' has not identified itself yet"),
            ));
        };

        Ok(PeerConnection {
            local_target,
            api_socket: api_socket.to_path_buf(),
            instance_id: instance_id.to_string(),
        })
    }

    /// The peer and peer-local id backing a local pane, if that pane is a view
    /// onto a peer's terminal.
    ///
    /// The peer address of a pane is per pane, not per workspace: a peer-backed
    /// workspace records which peer backs it, but each of its panes views a
    /// different terminal on that peer once it has been split.
    pub(in crate::app) fn peer_pane_source(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<(PeerHandle, String)> {
        let handle = PeerHandle::new(self.state.workspaces.get(ws_idx)?.peer.clone()?);
        let target = self
            .state
            .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)?
            .remote_target()?;
        Some((handle, target.to_string()))
    }

    /// Why this pane's view cannot be acted on through its peer, if it cannot.
    ///
    /// A view records the server it attached to; a peer records the server that
    /// last answered. When they differ, the view's peer-local target names a
    /// pane on a machine that is gone, and forwarding anything for it would
    /// reach an unrelated pane on the replacement. `abandon_views_of_replaced_peer`
    /// stops such a view as soon as the change is seen, but a request that was
    /// already resolving can arrive after it, so the check is repeated here
    /// where the view and the peer's current identity are both in scope.
    pub(super) fn peer_pane_server_mismatch(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<(&'static str, String)> {
        let remote = self
            .state
            .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)?
            .remote()?;
        let handle = PeerHandle::new(remote.peer().to_string());
        let current = self.state.peers.get(&handle)?.instance_id()?;
        remote.is_on_other_server(current).then(|| {
            (
                "unavailable",
                format!("peer '{handle}' was replaced by a different server; this view is stale"),
            )
        })
    }

    /// The view this server already holds onto `local_target`, if any.
    ///
    /// The one "is it already open" test both open paths use. They used to have
    /// their own: the terminal path searched every runtime while the workspace
    /// path only searched runtimes a workspace holds, so a bare terminal opened
    /// first was invisible to a workspace open and the two views fought over the
    /// peer's attach forever.
    ///
    /// `targets_workspace` says the target is a peer workspace id rather than a
    /// pane id, in which case a view onto *any* of its panes counts: a workspace
    /// target resolves to a pane before connecting, so comparing target strings
    /// would miss the view it produced.
    pub(super) fn peer_view_already_open(
        &self,
        peer: &str,
        local_target: &str,
        targets_workspace: bool,
    ) -> Option<ExistingPeerView> {
        let terminal_id = if targets_workspace {
            self.terminal_viewing_peer_workspace(peer, local_target)
        } else {
            self.existing_peer_view(peer, local_target)
        }?;
        match self
            .state
            .workspaces
            .iter()
            .position(|ws| ws.holds_terminal(&terminal_id))
        {
            Some(ws_idx) => Some(ExistingPeerView::InWorkspace {
                ws_idx,
                terminal_id,
            }),
            None => Some(ExistingPeerView::Bare { terminal_id }),
        }
    }

    /// The terminal already viewing `target` on `peer`, if this server has one.
    pub(super) fn existing_peer_view(
        &self,
        peer: &str,
        target: &str,
    ) -> Option<crate::terminal::TerminalId> {
        self.terminal_runtimes
            .iter()
            .find(|(_, runtime)| {
                runtime
                    .remote()
                    .is_some_and(|remote| remote.peer() == peer && remote.target() == target)
            })
            .map(|(terminal_id, _)| terminal_id.clone())
    }

    /// Whether `local_target` is one of the workspace ids `peer` enumerated.
    ///
    /// Matching against the enumeration rather than guessing from the id's shape
    /// keeps a pane id (`w1:p1`) or a terminal id from ever being mistaken for a
    /// workspace: the peer itself said which ids are workspaces.
    pub(super) fn peer_target_is_an_enumerated_workspace(
        &self,
        handle: &crate::app::peers::PeerHandle,
        local_target: &str,
    ) -> bool {
        self.state.peers.get(handle).is_some_and(|peer| {
            peer.workspaces.iter().any(|workspace| {
                crate::app::peers::split_peer_id(&workspace.workspace_id)
                    .is_some_and(|(_, local)| local == local_target)
            })
        })
    }

    /// The workspace holding a view onto any pane of the peer's `workspace_id`.
    ///
    /// A view addresses a *pane*, and a workspace opened from an enumeration
    /// resolves to one of its panes, so "is this workspace open here" cannot be
    /// answered by comparing target strings. Pane ids are `<workspace>:<pane>`,
    /// which is what makes the prefix test exact — the separator stops `w2`
    /// from matching `w21:p1`.
    pub(in crate::app) fn workspace_viewing_peer_workspace(
        &self,
        peer: &str,
        workspace_id: &str,
    ) -> Option<usize> {
        let terminal_id = self.terminal_viewing_peer_workspace(peer, workspace_id)?;
        self.state
            .workspaces
            .iter()
            .position(|ws| ws.holds_terminal(&terminal_id))
    }

    /// The terminal viewing any pane of the peer's `workspace_id`, whether or
    /// not a local workspace holds it.
    pub(super) fn terminal_viewing_peer_workspace(
        &self,
        peer: &str,
        workspace_id: &str,
    ) -> Option<crate::terminal::TerminalId> {
        let pane_prefix = format!("{workspace_id}:");
        self.terminal_runtimes
            .iter()
            .find(|(_, runtime)| {
                runtime.remote().is_some_and(|remote| {
                    remote.peer() == peer
                        && (remote.target() == workspace_id
                            || remote.target().starts_with(&pane_prefix))
                })
            })
            .map(|(terminal_id, _)| terminal_id.clone())
    }

    /// Where a terminal's pane sits, if one still holds it.
    ///
    /// Separate from [`Self::pane_for_terminal`] because a pane id alone does
    /// not identify a pane: everything that reports one needs the workspace it
    /// belongs to as well.
    pub(super) fn pane_location_for_terminal(
        &self,
        terminal_id: &crate::terminal::TerminalId,
    ) -> Option<(usize, crate::layout::PaneId)> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, ws)| {
                ws.tabs.iter().find_map(|tab| {
                    tab.panes
                        .iter()
                        .find(|(_, pane)| &pane.attached_terminal_id == terminal_id)
                        .map(|(pane_id, _)| (ws_idx, *pane_id))
                })
            })
    }

    /// The pane a terminal is attached to, if one still is.
    pub(crate) fn pane_for_terminal(
        &self,
        terminal_id: &crate::terminal::TerminalId,
    ) -> Option<crate::layout::PaneId> {
        self.state.workspaces.iter().find_map(|ws| {
            ws.tabs.iter().find_map(|tab| {
                tab.panes
                    .iter()
                    .find(|(_, pane)| &pane.attached_terminal_id == terminal_id)
                    .map(|(pane_id, _)| *pane_id)
            })
        })
    }

    /// Which workspace a `tab.create` means, by explicit id or by falling back
    /// to the active one.
    ///
    /// Shared with the handler so the routing gate and the local path can never
    /// disagree about which workspace is being asked for.
    pub(crate) fn workspace_index_for_tab_create(
        &self,
        workspace_id: Option<&str>,
    ) -> Option<usize> {
        match workspace_id {
            Some(workspace_id) => self.parse_workspace_id(workspace_id),
            None => self.state.active,
        }
    }
}

/// The peer's terminal control socket, derived from its API socket the same way
/// this server derives its own. An ssh peer's bridged socket pair follows the
/// same convention, so this does not need to know which kind of peer it is.
pub(super) fn client_socket_for(api_socket: &Path) -> PathBuf {
    crate::server::socket_paths::derive_client_socket_from_api_socket(api_socket)
}
