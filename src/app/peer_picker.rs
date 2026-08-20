//! Picker over the workspaces a peer has enumerated.
//!
//! A peer reports every workspace it holds, whether or not this server has
//! opened a view onto one, and that enumeration already lives in
//! [`crate::app::peers::PeerState::workspaces`]. This module is only the client
//! side of that fact: it turns the enumeration into a modal list, and turns a
//! selection into an ordinary `peer.workspace.open` request. Nothing here
//! fetches, and nothing here is a new shared runtime fact.
//!
//! Shaped after the "open worktree" modal in [`crate::app::worktrees`] on
//! purpose — same filter, selection, and already-open marker — so the two
//! pickers behave identically.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::peers::{PeerConnectionState, PeerHandle};
use super::state::{PeerWorkspaceOpenEntry, PeerWorkspaceOpenState};
use super::{App, Mode};

/// Why a peer cannot be asked to open a workspace right now.
///
/// A view opened against a peer that is not up either fails on a dead socket or
/// has nowhere to dial at all, so the picker says which it is rather than
/// letting the request produce a transport error. Matches the reconnect sweep,
/// which also declines to dispatch against a peer that is not connected.
pub(crate) fn peer_unavailable_reason(connection: &PeerConnectionState) -> Option<String> {
    match connection {
        PeerConnectionState::Connected => None,
        PeerConnectionState::Connecting => Some("peer is still connecting".to_string()),
        PeerConnectionState::Reconnecting { attempt, .. } => {
            Some(format!("peer is reconnecting (attempt {attempt})"))
        }
        PeerConnectionState::Error { message } => Some(format!("peer is unreachable: {message}")),
    }
}

impl App {
    /// Builds the picker for `peer` from that peer's last enumeration.
    ///
    /// Entries are a snapshot; the peer's *connection* state is read live at
    /// render and submit, so a peer that comes back while the picker is open
    /// stops being marked stale without the list being rebuilt.
    pub(crate) fn open_peer_workspace_picker(&mut self, peer: &str) {
        let handle = PeerHandle::new(peer);
        let Some(peer_state) = self.state.peers.get(&handle) else {
            return;
        };
        let enumerated: Vec<(String, String, usize, usize, usize)> = peer_state
            .workspaces
            .iter()
            .map(|workspace| {
                (
                    workspace.workspace_id.clone(),
                    workspace.label.clone(),
                    workspace.number,
                    workspace.pane_count,
                    workspace.tab_count,
                )
            })
            .collect();

        let entries = enumerated
            .into_iter()
            .map(|(target, label, number, pane_count, tab_count)| {
                // Enumerated ids are namespaced; a view records the peer-local
                // target it connected to, so the two are compared unprefixed.
                // A view addresses a pane inside the workspace, not the
                // workspace, which is why this is not a target equality test.
                let already_open_ws_idx = crate::app::peers::split_peer_id(&target)
                    .and_then(|(_, local)| self.workspace_viewing_peer_workspace(peer, local));
                PeerWorkspaceOpenEntry {
                    target,
                    label,
                    number,
                    pane_count,
                    tab_count,
                    already_open_ws_idx,
                }
            })
            .collect::<Vec<_>>();

        if entries.is_empty() {
            self.report_action_outcome(
                format!("'{peer}' has no workspaces"),
                "the peer has not reported any",
            );
            return;
        }

        self.state.peer_workspace_open = Some(PeerWorkspaceOpenState {
            peer: peer.to_string(),
            entries,
            selected: 0,
            query: String::new(),
            search_focused: false,
            error: None,
        });
        self.state.mode = Mode::OpenPeerWorkspace;
    }

    pub(crate) fn close_peer_workspace_picker(&mut self) {
        self.state.peer_workspace_open = None;
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
    }

    pub(crate) fn handle_peer_workspace_open_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_peer_workspace_picker(),
            KeyCode::Up => {
                if let Some(open) = &mut self.state.peer_workspace_open {
                    open.select_previous_filtered();
                }
            }
            KeyCode::Down => {
                if let Some(open) = &mut self.state.peer_workspace_open {
                    open.select_next_filtered();
                }
            }
            KeyCode::Char('/') => {
                if let Some(open) = &mut self.state.peer_workspace_open {
                    if open.search_focused {
                        open.query.push('/');
                        open.normalize_selection();
                    } else {
                        open.search_focused = true;
                    }
                }
            }
            KeyCode::Char(ch)
                if self
                    .state
                    .peer_workspace_open
                    .as_ref()
                    .is_some_and(|open| open.search_focused)
                    && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
                    && !ch.is_control() =>
            {
                self.insert_peer_workspace_open_search_text(&ch.to_string());
            }
            KeyCode::Backspace
                if self
                    .state
                    .peer_workspace_open
                    .as_ref()
                    .is_some_and(|open| open.search_focused) =>
            {
                if let Some(open) = &mut self.state.peer_workspace_open {
                    open.query.pop();
                    open.normalize_selection();
                }
            }
            KeyCode::Enter => self.submit_peer_workspace_open_via_api(),
            _ => {}
        }
    }

    pub(crate) fn insert_peer_workspace_open_search_text(&mut self, text: &str) {
        let Some(open) = &mut self.state.peer_workspace_open else {
            return;
        };
        if !open.search_focused {
            return;
        }
        open.query.push_str(text);
        open.normalize_selection();
    }

    /// Opens the selected workspace, or explains why the peer cannot answer.
    ///
    /// Opening is idempotent per target, so picking an entry that is already
    /// open switches to the existing view rather than stacking a second one.
    pub(crate) fn submit_peer_workspace_open_via_api(&mut self) {
        let Some(open) = self.state.peer_workspace_open.as_ref() else {
            return;
        };
        let Some(entry_idx) = open.selected_entry_index() else {
            return;
        };
        let Some(entry) = open.entries.get(entry_idx).cloned() else {
            return;
        };
        let peer = open.peer.clone();

        // An entry that is already open needs nothing from the peer: the open
        // path finds the existing view and switches to it before it would dial.
        // Refusing that because the peer is down would strand a view the user
        // can still see.
        let unavailable = if entry.already_open_ws_idx.is_some() {
            None
        } else {
            self.state
                .peers
                .get(&PeerHandle::new(peer.as_str()))
                .map_or_else(
                    || Some(format!("peer '{peer}' is no longer configured")),
                    |peer_state| peer_unavailable_reason(&peer_state.connection),
                )
        };
        if let Some(reason) = unavailable {
            if let Some(open) = &mut self.state.peer_workspace_open {
                open.error = Some(reason);
            }
            return;
        }

        let outcome = self.runtime_peer_workspace_open(
            "tui.peer.workspace.open",
            crate::api::schema::PeerWorkspaceOpenParams {
                target: entry.target,
                name: Some(peer),
                label: None,
                focus: true,
                takeover: false,
            },
        );
        // Every open is routed — `peer.workspace.open` always leaves this
        // server — so `Accepted` is the normal path and the two branches below
        // are only reachable if that ever stops being true. The dialog is left
        // alone deliberately: the request is in flight, a failure arrives as
        // the peer-forward toast, and the click that submitted it has already
        // closed the picker (`src/app/input/mouse.rs`). What is new here is
        // that this is *said*; it used to be an empty string falling through
        // two parses that both quietly failed.
        let Some(response) = outcome.answered() else {
            return;
        };
        if serde_json::from_str::<crate::api::schema::SuccessResponse>(response).is_ok() {
            self.state.peer_workspace_open = None;
            self.state.mode = Mode::Terminal;
        } else if let Ok(error) =
            serde_json::from_str::<crate::api::schema::ErrorResponse>(response)
        {
            if let Some(open) = &mut self.state.peer_workspace_open {
                open.error = Some(error.error.message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{AgentStatus, WorkspaceInfo};
    use crate::app::peers::PeerTarget;

    fn workspace(id: &str, label: &str, number: usize) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: id.to_string(),
            number,
            label: label.to_string(),
            focused: false,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: format!("{id}:t1"),
            agent_status: AgentStatus::Unknown,
            tokens: std::collections::HashMap::new(),
            worktree: None,
        }
    }

    const INSTANCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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

    fn app_with_peer_workspaces(connection: PeerConnectionState) -> App {
        let mut app = test_app();
        let handle = PeerHandle::new("beta");
        app.state
            .peers
            .add(handle.clone(), PeerTarget::SocketPath("/tmp/b.sock".into()))
            .expect("peer added");
        app.state.peers.set_identity(
            &handle,
            crate::app::peers::PeerIdentity {
                instance_id: INSTANCE.to_string(),
                version: None,
                protocol: None,
            },
        );
        app.state.peers.set_workspaces(
            &handle,
            vec![workspace("w1", "api", 1), workspace("w2", "web", 2)],
        );
        app.state.peers.set_connection(&handle, connection);
        app
    }

    #[test]
    fn picker_lists_every_enumerated_workspace_with_namespaced_targets() {
        let mut app = app_with_peer_workspaces(PeerConnectionState::Connected);
        app.open_peer_workspace_picker("beta");

        let open = app.state.peer_workspace_open.expect("picker opened");
        assert_eq!(app.state.mode, Mode::OpenPeerWorkspace);
        assert_eq!(open.peer, "beta");
        let targets: Vec<&str> = open
            .entries
            .iter()
            .map(|entry| entry.target.as_str())
            .collect();
        assert_eq!(
            targets,
            vec![format!("{INSTANCE}:w1"), format!("{INSTANCE}:w2")]
        );
        // Nothing is open locally, so nothing is marked.
        assert!(open
            .entries
            .iter()
            .all(|entry| entry.already_open_ws_idx.is_none()));
    }

    #[test]
    fn a_disconnected_peer_still_lists_its_last_enumeration() {
        let mut app = app_with_peer_workspaces(PeerConnectionState::Reconnecting {
            attempt: 3,
            message: "connection refused".to_string(),
        });
        app.open_peer_workspace_picker("beta");

        let open = app.state.peer_workspace_open.expect("picker opened");
        assert_eq!(open.entries.len(), 2);
    }

    #[test]
    fn submitting_against_a_peer_that_is_not_connected_reports_why() {
        let mut app = app_with_peer_workspaces(PeerConnectionState::Reconnecting {
            attempt: 3,
            message: "connection refused".to_string(),
        });
        app.open_peer_workspace_picker("beta");
        app.submit_peer_workspace_open_via_api();

        let open = app
            .state
            .peer_workspace_open
            .as_ref()
            .expect("picker stays open");
        assert_eq!(
            open.error.as_deref(),
            Some("peer is reconnecting (attempt 3)")
        );
    }

    #[test]
    fn an_already_open_entry_is_not_blocked_by_a_peer_that_is_down() {
        let mut app = app_with_peer_workspaces(PeerConnectionState::Reconnecting {
            attempt: 3,
            message: "connection refused".to_string(),
        });
        app.open_peer_workspace_picker("beta");
        // Stand in for a view that is already open on the first entry; the open
        // path answers those locally, so the peer's state must not gate it.
        if let Some(open) = &mut app.state.peer_workspace_open {
            open.entries[0].already_open_ws_idx = Some(0);
        }

        app.submit_peer_workspace_open_via_api();

        // No "peer is reconnecting" refusal. The open request itself fails here
        // because this fixture has no view to find, but the gate did not fire.
        let open = app
            .state
            .peer_workspace_open
            .as_ref()
            .expect("picker stays open");
        assert_ne!(
            open.error.as_deref(),
            Some("peer is reconnecting (attempt 3)")
        );
    }

    #[test]
    fn a_peer_with_no_enumeration_opens_no_picker() {
        let mut app = test_app();
        let handle = PeerHandle::new("beta");
        app.state
            .peers
            .add(handle.clone(), PeerTarget::SocketPath("/tmp/b.sock".into()))
            .expect("peer added");
        app.open_peer_workspace_picker("beta");

        assert!(app.state.peer_workspace_open.is_none());
        assert_ne!(app.state.mode, Mode::OpenPeerWorkspace);
        // A picker that will not open has to say so, and it says so as action
        // feedback rather than as a config.toml diagnostic.
        assert!(app.state.config_diagnostic.is_none());
        assert!(app
            .state
            .toast
            .as_ref()
            .is_some_and(|toast| toast.title.contains("beta")));
    }

    #[test]
    fn filtering_narrows_to_matching_labels() {
        let mut app = app_with_peer_workspaces(PeerConnectionState::Connected);
        app.open_peer_workspace_picker("beta");
        if let Some(open) = &mut app.state.peer_workspace_open {
            open.search_focused = true;
        }
        app.insert_peer_workspace_open_search_text("web");

        let open = app.state.peer_workspace_open.expect("picker opened");
        let filtered = open.filtered_indices();
        assert_eq!(filtered, vec![1]);
        assert_eq!(open.selected_entry_index(), Some(1));
    }

    #[test]
    fn escape_closes_the_picker() {
        let mut app = app_with_peer_workspaces(PeerConnectionState::Connected);
        app.open_peer_workspace_picker("beta");
        app.handle_peer_workspace_open_key(KeyEvent::from(KeyCode::Esc));

        assert!(app.state.peer_workspace_open.is_none());
        assert_ne!(app.state.mode, Mode::OpenPeerWorkspace);
    }

    #[test]
    fn unavailable_reason_is_only_set_for_a_peer_that_is_not_connected() {
        assert!(peer_unavailable_reason(&PeerConnectionState::Connected).is_none());
        assert!(peer_unavailable_reason(&PeerConnectionState::Connecting).is_some());
        assert!(peer_unavailable_reason(&PeerConnectionState::Error {
            message: "no route".to_string(),
        })
        .is_some());
    }
}
