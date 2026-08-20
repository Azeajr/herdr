//! Picker over hidden peers, so a hidden peer can always be brought back.
//!
//! Reached from the peer header's right-click menu ("Unhide peer..."). Session
//! hides are dropped from [`AppState::hidden_peers`]; permanent hides also
//! rewrite `[peer_hidden]` in the config file, the same write the hide action
//! made. Shaped after [`super::peer_picker`]: a modal list, Enter acts, Esc
//! closes.

use crossterm::event::{KeyCode, KeyEvent};

use super::state::{HiddenPeerEntry, HiddenPeersState};
use super::{App, AppState, Mode};

impl AppState {
    /// Rebuilds the picker entries from the two hide sources, session first.
    fn hidden_peer_entries(&self) -> Vec<HiddenPeerEntry> {
        let mut entries: Vec<HiddenPeerEntry> = self
            .hidden_peers
            .iter()
            .map(|peer| HiddenPeerEntry {
                peer: peer.clone(),
                permanent: false,
            })
            .chain(self.hidden_peers_config.iter().map(|peer| HiddenPeerEntry {
                peer: peer.clone(),
                permanent: true,
            }))
            .collect();
        entries.sort_by(|a, b| a.peer.cmp(&b.peer).then(a.permanent.cmp(&b.permanent)));
        entries.dedup_by(|a, b| {
            // A peer hidden both ways unhides both ways at once: `b` is the
            // retained element, so the flag merges onto it.
            if a.peer == b.peer {
                b.permanent |= a.permanent;
                true
            } else {
                false
            }
        });
        entries
    }
}

impl App {
    pub(crate) fn open_hidden_peers_picker(&mut self) {
        let entries = self.state.hidden_peer_entries();
        if entries.is_empty() {
            self.report_action_outcome("no hidden peers", "nothing to unhide");
            return;
        }
        self.state.hidden_peers_picker = Some(HiddenPeersState {
            entries,
            selected: 0,
            error: None,
        });
        self.state.mode = Mode::UnhidePeers;
    }

    pub(crate) fn close_hidden_peers_picker(&mut self) {
        self.state.hidden_peers_picker = None;
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
    }

    pub(crate) fn handle_hidden_peers_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_hidden_peers_picker(),
            KeyCode::Up => {
                if let Some(picker) = &mut self.state.hidden_peers_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(picker) = &mut self.state.hidden_peers_picker {
                    picker.selected =
                        (picker.selected + 1).min(picker.entries.len().saturating_sub(1));
                }
            }
            KeyCode::Enter => self.unhide_selected_peer(),
            _ => {}
        }
    }

    pub(crate) fn unhide_selected_peer(&mut self) {
        let Some(entry) = self
            .state
            .hidden_peers_picker
            .as_ref()
            .and_then(|picker| picker.entries.get(picker.selected))
            .cloned()
        else {
            return;
        };

        self.state.hidden_peers.remove(&entry.peer);
        self.state.mark_session_dirty();

        if entry.permanent {
            let peers: Vec<String> = self
                .state
                .hidden_peers_config
                .iter()
                .filter(|peer| peer.as_str() != entry.peer)
                .cloned()
                .collect();
            match crate::config::write_peer_hidden_peers(&peers) {
                Ok(()) => {
                    self.state.hidden_peers_config.remove(&entry.peer);
                }
                Err(err) => {
                    if let Some(picker) = &mut self.state.hidden_peers_picker {
                        picker.error = Some(err);
                    }
                    return;
                }
            }
        }

        let entries = self.state.hidden_peer_entries();
        if entries.is_empty() {
            self.close_hidden_peers_picker();
            return;
        }
        if let Some(picker) = &mut self.state.hidden_peers_picker {
            picker.entries = entries;
            picker.selected = picker.selected.min(picker.entries.len().saturating_sub(1));
            picker.error = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_merge_both_hide_sources() {
        let mut state = AppState::test_new();
        state.hidden_peers.insert("session-peer".to_string());
        state.hidden_peers_config.insert("config-peer".to_string());

        let entries = state.hidden_peer_entries();

        assert_eq!(
            entries,
            vec![
                HiddenPeerEntry {
                    peer: "config-peer".into(),
                    permanent: true,
                },
                HiddenPeerEntry {
                    peer: "session-peer".into(),
                    permanent: false,
                },
            ]
        );
    }

    #[test]
    fn a_peer_hidden_both_ways_is_one_permanent_entry() {
        let mut state = AppState::test_new();
        state.hidden_peers.insert("peer".to_string());
        state.hidden_peers_config.insert("peer".to_string());

        let entries = state.hidden_peer_entries();

        assert_eq!(
            entries,
            vec![HiddenPeerEntry {
                peer: "peer".into(),
                permanent: true,
            }]
        );
    }
}
