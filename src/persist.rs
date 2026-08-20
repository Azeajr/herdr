//! Session persistence — save/restore workspaces, layouts, and working directories.
//!
//! Stored at `~/.config/herdr/session.json`.
//! Optional pane screen history is stored separately at `session-history.json`.
//! Installed plugins are persisted separately at `plugins.json`.

mod io;
pub mod plugin_registry;
mod restore;
mod snapshot;

pub use self::io::{clear, clear_history, load, load_history, save};
pub use self::restore::restore;
#[cfg(unix)]
pub use self::restore::{handoff_pane_aliases, restore_handoff};
pub use self::snapshot::{
    capture, capture_history, DirectionSnapshot, LayoutSnapshot, PeerSnapshot, PeerTargetSnapshot,
    SessionHistoryCapture, SessionHistorySnapshot, SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
};

/// Converts configured peers into their persisted form.
///
/// Only how to reach a peer is stored. Connection state and enumerated
/// workspaces are deliberately left out: they describe a live connection and
/// are rebuilt by reconnecting, so persisting them would restore stale claims
/// about a peer that may now be unreachable or a different server entirely.
pub fn peer_snapshots(peers: &crate::app::peers::PeerRegistryState) -> Vec<PeerSnapshot> {
    peers
        .iter()
        .map(|peer| PeerSnapshot {
            name: peer.handle.as_str().to_string(),
            // The default label is derived from the target, so only a label the
            // user actually changed is worth storing.
            label: (peer.label != peer.target.describe()).then(|| peer.label.clone()),
            target: match &peer.target {
                crate::app::peers::PeerTarget::SocketPath(path) => {
                    PeerTargetSnapshot::SocketPath { path: path.clone() }
                }
                crate::app::peers::PeerTarget::Ssh {
                    destination,
                    session,
                } => PeerTargetSnapshot::Ssh {
                    destination: destination.clone(),
                    session: session.clone(),
                },
            },
        })
        .collect()
}

/// Rebuilds the configured peer set from a snapshot.
///
/// Every restored peer starts disconnected; the server's reconcile pass opens
/// the connections.
pub fn restore_peers(snapshots: &[PeerSnapshot]) -> crate::app::peers::PeerRegistryState {
    let mut peers = crate::app::peers::PeerRegistryState::default();
    for snapshot in snapshots {
        let target = match &snapshot.target {
            PeerTargetSnapshot::SocketPath { path } => {
                crate::app::peers::PeerTarget::SocketPath(path.clone())
            }
            PeerTargetSnapshot::Ssh {
                destination,
                session,
            } => crate::app::peers::PeerTarget::Ssh {
                destination: destination.clone(),
                session: session.clone(),
            },
        };
        let handle = crate::app::peers::PeerHandle::new(snapshot.name.clone());
        // A duplicate in the snapshot means the file was hand-edited; keep the
        // first and drop the rest rather than failing the whole restore.
        if peers.add(handle.clone(), target).is_ok() {
            if let (Some(label), Some(peer)) = (snapshot.label.clone(), peers.get_mut(&handle)) {
                peer.label = label;
            }
        }
    }
    peers
}
