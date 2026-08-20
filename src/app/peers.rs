//! Peer state for federated herdr servers — pure data, no channels or async.
//!
//! A *peer* is another herdr server this one connects out to. This module holds
//! only what the runtime has learned about each peer: how to reach it, whether
//! the connection is up, and the workspaces it last reported. The connections
//! themselves live in `crate::server::peer`, the same way `PaneRuntime` lives
//! apart from `PaneState`.
//!
//! Peer identity, connection state, and workspace enumeration are shared
//! runtime facts, so they belong here in server state rather than in a client.

use std::path::{Path, PathBuf};

use crate::api::schema::WorkspaceInfo;

/// Namespaces one of a peer's ids with that peer's instance id.
///
/// Peer ids are only unique inside the peer's own process, so every id that
/// crosses into this server is prefixed exactly once, at ingest.
pub fn prefix_peer_id(instance_id: &str, local_id: &str) -> String {
    format!("{instance_id}:{local_id}")
}

/// Splits a namespaced id back into `(instance_id, local_id)`.
///
/// Returns `None` for a local id. The two alphabets cannot overlap: an instance
/// id is 32 lowercase hex characters, and a local workspace id is `w` followed
/// by uppercase base32, so no escape or lookup table is needed.
pub fn split_peer_id(id: &str) -> Option<(&str, &str)> {
    let (head, rest) = id.split_once(':')?;
    (crate::instance_id::is_instance_id(head) && !rest.is_empty()).then_some((head, rest))
}

/// Whether `id` belongs to a peer rather than this server.
pub fn is_peer_id(id: &str) -> bool {
    split_peer_id(id).is_some()
}

/// Locally assigned, stable key for a configured peer.
///
/// Chosen when the peer is added, before its instance id is known, so it can
/// name a peer that has never successfully connected.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerHandle(String);

impl PeerHandle {
    pub fn new(handle: impl Into<String>) -> Self {
        Self(handle.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PeerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a peer's JSON API socket lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerTarget {
    /// A socket path reachable directly on this machine. Used for same-host
    /// federation and for testing without SSH.
    SocketPath(PathBuf),
    /// An SSH destination. The socket path is resolved on the remote host.
    Ssh {
        destination: String,
        session: Option<String>,
    },
}

impl PeerTarget {
    /// Short human-readable form, used as the default peer label.
    pub fn describe(&self) -> String {
        match self {
            Self::SocketPath(path) => path.display().to_string(),
            Self::Ssh {
                destination,
                session: None,
            } => destination.clone(),
            Self::Ssh {
                destination,
                session: Some(session),
            } => format!("{destination}:{session}"),
        }
    }
}

/// Lifecycle of one peer connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerConnectionState {
    /// First connection attempt has not resolved yet.
    Connecting,
    /// Control channel is up and the peer has been identified.
    Connected,
    /// The connection failed and is being retried. `message` is why the last
    /// attempt failed.
    Reconnecting { attempt: u32, message: String },
    /// Failed in a way retrying cannot fix; the connection loop has stopped and
    /// the user has to act.
    Error { message: String },
}

impl PeerConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }

    /// Whether content from this peer should render as stale.
    pub fn is_stale(&self) -> bool {
        !self.is_connected()
    }

    /// Short label for status output and the sidebar.
    pub fn label(&self) -> &'static str {
        self.kind().label()
    }

    /// This state without its payload, as clients are told it.
    ///
    /// The attempt count and the failure message are reported in their own
    /// fields, so the wire type carries only which of the four states this is.
    pub fn kind(&self) -> crate::api::schema::PeerConnectionKind {
        use crate::api::schema::PeerConnectionKind;
        match self {
            Self::Connecting => PeerConnectionKind::Connecting,
            Self::Connected => PeerConnectionKind::Connected,
            Self::Reconnecting { .. } => PeerConnectionKind::Reconnecting,
            Self::Error { .. } => PeerConnectionKind::Error,
        }
    }
}

/// What a peer reported about itself on its last successful ping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    pub instance_id: String,
    pub version: Option<String>,
    pub protocol: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerState {
    pub handle: PeerHandle,
    pub label: String,
    pub target: PeerTarget,
    pub connection: PeerConnectionState,
    /// Learned on the first successful ping. Retained across disconnects so a
    /// reconnect can be matched back to the same server.
    pub identity: Option<PeerIdentity>,
    /// Last enumeration received from the peer. Kept while disconnected so the
    /// UI can gray it out instead of blanking.
    pub workspaces: Vec<WorkspaceInfo>,
    /// Local socket an [`PeerTarget::Ssh`] peer became reachable on.
    ///
    /// An ssh peer is reached through a locally bridged socket that only exists
    /// while its transport is up, so this is learned at runtime and never
    /// persisted — a bridge path from a previous run names a dead socket.
    pub transport_socket: Option<PathBuf>,
    /// Panes spawned on this peer that could not be closed there.
    ///
    /// Closing a view cannot fail locally and cannot be rolled back, so a failed
    /// cleanup is retained rather than reported as an error. Retained rather
    /// than merely counted because the pane id is the only thing that can undo
    /// the leak: without it a remote shell, its process tree and its scrollback
    /// stay alive with nothing pointing at them and no way to name them.
    ///
    /// Not persisted: after a restart the peer's own unattached-owner state is
    /// the better record.
    pub pending_pane_cleanups: Vec<PendingPaneCleanup>,
}

/// A pane left running on a peer because closing it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPaneCleanup {
    pub peer_pane_id: String,
    /// The instance that issued `peer_pane_id`.
    ///
    /// A retry is only safe while this still matches what the peer reports.
    /// `None` records cannot be retried at all, and exist to be reported.
    pub expected_instance: Option<String>,
    pub reason: String,
}

/// How many unresolved cleanups to retain for one peer.
///
/// These exist to be retried and reported, and both stop being useful long
/// before the list gets large; the bound is here so a peer that is broken for a
/// long time cannot grow it without limit.
const MAX_PENDING_PANE_CLEANUPS: usize = 64;

impl PeerState {
    pub fn new(handle: PeerHandle, target: PeerTarget) -> Self {
        let label = target.describe();
        Self {
            handle,
            label,
            target,
            connection: PeerConnectionState::Connecting,
            identity: None,
            workspaces: Vec::new(),
            transport_socket: None,
            pending_pane_cleanups: Vec::new(),
        }
    }

    /// How many panes are known to be left running on this peer.
    pub fn failed_pane_cleanups(&self) -> usize {
        self.pending_pane_cleanups.len()
    }

    pub fn instance_id(&self) -> Option<&str> {
        self.identity
            .as_ref()
            .map(|identity| identity.instance_id.as_str())
    }

    pub fn is_stale(&self) -> bool {
        self.connection.is_stale()
    }

    /// Local socket this peer's JSON API is reachable on, if it is reachable at
    /// all yet.
    ///
    /// A socket-path peer is already local. An ssh peer only has one once its
    /// bridge is up, which is what makes the two kinds interchangeable
    /// everywhere downstream.
    pub fn api_socket(&self) -> Option<&Path> {
        match &self.target {
            PeerTarget::SocketPath(path) => Some(path),
            PeerTarget::Ssh { .. } => self.transport_socket.as_deref(),
        }
    }
}

/// All configured peers, in the order they were added.
///
/// Order is insertion order and is the order the UI renders, matching how
/// `AppState::workspaces` works.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerRegistryState {
    peers: Vec<PeerState>,
}

/// What recording a peer's identity did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetIdentity {
    /// The peer is unknown, or already reported exactly this.
    Unchanged,
    /// Stored. `replaced` carries the previous instance id when a *different*
    /// server answered, which invalidates every fact bound to the old one — not
    /// only the enumeration this registry dropped.
    Recorded { replaced: Option<String> },
    /// Another peer already holds this instance id, so nothing was stored.
    /// Both handles would otherwise resolve the same namespaced ids, and every
    /// one of them would route to whichever peer was added first.
    Duplicate { held_by: PeerHandle },
}

/// Why a peer could not be added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddPeerError {
    /// A peer with this handle already exists.
    DuplicateHandle,
    /// A different peer is already configured with the same target.
    DuplicateTarget { handle: PeerHandle },
}

impl std::fmt::Display for AddPeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateHandle => write!(f, "a peer with that name already exists"),
            Self::DuplicateTarget { handle } => {
                write!(f, "that target is already configured as peer '{handle}'")
            }
        }
    }
}

impl PeerRegistryState {
    pub fn iter(&self) -> impl Iterator<Item = &PeerState> {
        self.peers.iter()
    }

    pub fn add(&mut self, handle: PeerHandle, target: PeerTarget) -> Result<(), AddPeerError> {
        if self.get(&handle).is_some() {
            return Err(AddPeerError::DuplicateHandle);
        }
        if let Some(existing) = self.peers.iter().find(|peer| peer.target == target) {
            return Err(AddPeerError::DuplicateTarget {
                handle: existing.handle.clone(),
            });
        }
        self.peers.push(PeerState::new(handle, target));
        Ok(())
    }

    pub fn remove(&mut self, handle: &PeerHandle) -> Option<PeerState> {
        let index = self.peers.iter().position(|peer| &peer.handle == handle)?;
        Some(self.peers.remove(index))
    }

    pub fn get(&self, handle: &PeerHandle) -> Option<&PeerState> {
        self.peers.iter().find(|peer| &peer.handle == handle)
    }

    pub fn get_mut(&mut self, handle: &PeerHandle) -> Option<&mut PeerState> {
        self.peers.iter_mut().find(|peer| &peer.handle == handle)
    }

    /// Records a connection state transition. Returns whether anything changed,
    /// so callers can skip a redraw on a no-op update.
    pub fn set_connection(&mut self, handle: &PeerHandle, connection: PeerConnectionState) -> bool {
        match self.get_mut(handle) {
            Some(peer) if peer.connection != connection => {
                peer.connection = connection;
                true
            }
            _ => false,
        }
    }

    /// Records the local socket an ssh peer's transport came up on.
    pub fn set_transport_socket(&mut self, handle: &PeerHandle, socket: PathBuf) -> bool {
        match self.get_mut(handle) {
            Some(peer) if peer.transport_socket.as_deref() != Some(socket.as_path()) => {
                peer.transport_socket = Some(socket);
                true
            }
            _ => false,
        }
    }

    /// Retains a pane this server spawned on `handle` that could not be closed
    /// there.
    ///
    /// The local view is already gone, so nothing can be retried right now. The
    /// record is what makes a retry possible later, when the same server is
    /// reachable again.
    pub fn record_failed_pane_cleanup(
        &mut self,
        handle: &PeerHandle,
        pending: PendingPaneCleanup,
    ) -> bool {
        match self.get_mut(handle) {
            Some(peer) => {
                if peer
                    .pending_pane_cleanups
                    .iter()
                    .any(|existing| existing.peer_pane_id == pending.peer_pane_id)
                {
                    // A retry that failed again describes the same leak, not a
                    // second one.
                    return true;
                }
                if peer.pending_pane_cleanups.len() >= MAX_PENDING_PANE_CLEANUPS {
                    peer.pending_pane_cleanups.remove(0);
                }
                peer.pending_pane_cleanups.push(pending);
                true
            }
            None => false,
        }
    }

    /// Takes the cleanups that can be retried now that `instance_id` answers.
    ///
    /// Only records issued by that same instance come back. A different server
    /// answering means the retained ids name nothing here — or worse, name
    /// somebody else's panes — so those records stay put to be reported rather
    /// than acted on.
    pub fn take_retryable_pane_cleanups(
        &mut self,
        handle: &PeerHandle,
        instance_id: &str,
    ) -> Vec<PendingPaneCleanup> {
        let Some(peer) = self.get_mut(handle) else {
            return Vec::new();
        };
        let mut retryable = Vec::new();
        peer.pending_pane_cleanups.retain(|pending| {
            if pending.expected_instance.as_deref() == Some(instance_id) {
                retryable.push(pending.clone());
                false
            } else {
                true
            }
        });
        retryable
    }

    /// Records what a peer reported about itself.
    ///
    /// A peer whose instance id changed is a different server reached through
    /// the same target — its retained workspaces no longer describe it, so they
    /// are dropped rather than shown against the wrong host. Everything else
    /// bound to that identity lives outside this registry, which is why the
    /// replacement is reported rather than only acted on here.
    ///
    /// Two peers reporting the same instance id is the same server reached
    /// twice. `add` cannot catch it — handles and targets are both checked
    /// before any identity is known — and this is the first moment it can be
    /// seen, so it is refused here rather than left to a test-only assertion.
    pub fn set_identity(&mut self, handle: &PeerHandle, identity: PeerIdentity) -> SetIdentity {
        if let Some(other) = self.peers.iter().find(|peer| {
            &peer.handle != handle && peer.instance_id() == Some(&identity.instance_id)
        }) {
            return SetIdentity::Duplicate {
                held_by: other.handle.clone(),
            };
        }

        let Some(peer) = self.get_mut(handle) else {
            return SetIdentity::Unchanged;
        };
        if peer.identity.as_ref() == Some(&identity) {
            return SetIdentity::Unchanged;
        }
        let replaced = peer
            .identity
            .as_ref()
            .filter(|previous| previous.instance_id != identity.instance_id)
            .map(|previous| previous.instance_id.clone());
        peer.identity = Some(identity);
        if replaced.is_some() {
            peer.workspaces.clear();
        }
        SetIdentity::Recorded { replaced }
    }

    /// Records a peer's workspace enumeration, namespacing its ids on the way
    /// in.
    ///
    /// This is the single ingest point for peer ids, so prefixing here is what
    /// guarantees every stored id is namespaced exactly once. A peer that has
    /// not identified itself yet has no namespace to use, so its enumeration is
    /// dropped rather than stored ambiguously.
    pub fn set_workspaces(&mut self, handle: &PeerHandle, workspaces: Vec<WorkspaceInfo>) -> bool {
        let Some(peer) = self.get_mut(handle) else {
            return false;
        };
        let Some(instance_id) = peer
            .identity
            .as_ref()
            .map(|identity| identity.instance_id.clone())
        else {
            return false;
        };

        let workspaces: Vec<WorkspaceInfo> = workspaces
            .into_iter()
            .map(|mut workspace| {
                workspace.workspace_id = prefix_peer_id(&instance_id, &workspace.workspace_id);
                workspace.active_tab_id = prefix_peer_id(&instance_id, &workspace.active_tab_id);
                workspace
            })
            .collect();

        if peer.workspaces == workspaces {
            return false;
        }
        peer.workspaces = workspaces;
        true
    }

    /// The peer owning `instance_id`, used to route a namespaced id back to the
    /// connection it came from.
    pub fn by_instance_id(&self, instance_id: &str) -> Option<&PeerState> {
        self.peers
            .iter()
            .find(|peer| peer.instance_id() == Some(instance_id))
    }

    /// Resolves a namespaced id to its owning peer and the peer-local id to
    /// send back over the wire.
    pub fn resolve_peer_id<'a>(&'a self, id: &'a str) -> Option<(&'a PeerState, &'a str)> {
        let (instance_id, local_id) = split_peer_id(id)?;
        Some((self.by_instance_id(instance_id)?, local_id))
    }

    /// Panics when the registry holds contradictory identity state.
    ///
    /// Handles are the local key and must be unique. Instance ids must also be
    /// unique: two peers reporting the same instance id means the same server
    /// was reached twice, and routing a prefixed id would be ambiguous.
    #[cfg(test)]
    pub fn assert_invariants_for_test(&self) {
        let mut handles = std::collections::HashSet::new();
        for peer in &self.peers {
            assert!(
                handles.insert(peer.handle.clone()),
                "duplicate peer handle: {}",
                peer.handle
            );
        }

        let mut instance_ids = std::collections::HashSet::new();
        for peer in &self.peers {
            if let Some(instance_id) = peer.instance_id() {
                assert!(
                    instance_ids.insert(instance_id.to_string()),
                    "duplicate peer instance id {instance_id} on peer {}",
                    peer.handle
                );
            }
        }

        for peer in &self.peers {
            if peer.connection.is_connected() {
                assert!(
                    peer.identity.is_some(),
                    "peer {} is connected without an identity",
                    peer.handle
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str) -> PeerTarget {
        PeerTarget::SocketPath(PathBuf::from(format!("/tmp/{name}.sock")))
    }

    fn identity(instance_id: &str) -> PeerIdentity {
        PeerIdentity {
            instance_id: instance_id.to_string(),
            version: Some("0.7.5".into()),
            protocol: Some(20),
        }
    }

    fn workspace(id: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: id.to_string(),
            number: 1,
            label: id.to_string(),
            focused: false,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: format!("{id}:t1"),
            agent_status: crate::api::schema::AgentStatus::Unknown,
            tokens: std::collections::HashMap::new(),
            worktree: None,
        }
    }

    #[test]
    fn add_rejects_duplicate_handles_and_targets() {
        let mut registry = PeerRegistryState::default();
        registry
            .add(PeerHandle::new("alpha"), target("a"))
            .expect("first add");

        assert_eq!(
            registry.add(PeerHandle::new("alpha"), target("b")),
            Err(AddPeerError::DuplicateHandle)
        );
        assert_eq!(
            registry.add(PeerHandle::new("beta"), target("a")),
            Err(AddPeerError::DuplicateTarget {
                handle: PeerHandle::new("alpha")
            })
        );
        assert_eq!(registry.iter().count(), 1);
        registry.assert_invariants_for_test();
    }

    #[test]
    fn new_peer_starts_connecting_and_stale() {
        let mut registry = PeerRegistryState::default();
        registry
            .add(PeerHandle::new("alpha"), target("a"))
            .expect("add");

        let peer = registry.get(&PeerHandle::new("alpha")).expect("peer");
        assert_eq!(peer.connection, PeerConnectionState::Connecting);
        assert!(peer.is_stale());
        assert!(peer.workspaces.is_empty());
        assert_eq!(peer.label, "/tmp/a.sock");
    }

    #[test]
    fn set_connection_reports_only_real_transitions() {
        let mut registry = PeerRegistryState::default();
        let handle = PeerHandle::new("alpha");
        registry.add(handle.clone(), target("a")).expect("add");

        assert!(registry.set_connection(&handle, PeerConnectionState::Connected));
        assert!(!registry.set_connection(&handle, PeerConnectionState::Connected));
        assert!(registry.set_connection(
            &handle,
            PeerConnectionState::Reconnecting {
                attempt: 1,
                message: "boom".into()
            }
        ));
        assert!(
            !registry.set_connection(&PeerHandle::new("missing"), PeerConnectionState::Connected)
        );
    }

    #[test]
    fn workspaces_survive_disconnect_and_render_stale() {
        let mut registry = PeerRegistryState::default();
        let handle = PeerHandle::new("alpha");
        registry.add(handle.clone(), target("a")).expect("add");
        registry.set_identity(&handle, identity("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        registry.set_connection(&handle, PeerConnectionState::Connected);
        registry.set_workspaces(&handle, vec![workspace("w1")]);

        registry.set_connection(
            &handle,
            PeerConnectionState::Reconnecting {
                attempt: 2,
                message: "boom".into(),
            },
        );

        let peer = registry.get(&handle).expect("peer");
        assert_eq!(
            peer.workspaces.len(),
            1,
            "last enumeration must be retained"
        );
        assert!(peer.is_stale());
        assert_eq!(peer.instance_id(), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    }

    /// `add` rejects duplicate handles and duplicate targets, but both are
    /// checked before any identity is known. Two different targets that reach
    /// the same server therefore used to both store its instance id — a state
    /// the test-only invariant declares impossible, and one where every
    /// namespaced id silently routes to whichever peer was added first.
    #[test]
    fn a_second_peer_reaching_the_same_server_is_refused() {
        let mut registry = PeerRegistryState::default();
        let local = PeerHandle::new("local");
        let remote = PeerHandle::new("remote");
        registry.add(local.clone(), target("a")).expect("add local");
        registry
            .add(remote.clone(), target("b"))
            .expect("add remote");

        let same_server = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(
            registry.set_identity(&local, identity(same_server)),
            SetIdentity::Recorded { replaced: None }
        );
        assert_eq!(
            registry.set_identity(&remote, identity(same_server)),
            SetIdentity::Duplicate {
                held_by: local.clone()
            }
        );

        assert_eq!(
            registry.get(&remote).expect("peer").instance_id(),
            None,
            "a refused identity must not be stored"
        );
        registry.assert_invariants_for_test();
    }

    /// Reporting the same id again for the peer that already holds it is a
    /// heartbeat, not a duplicate.
    #[test]
    fn a_peer_repeating_its_own_instance_id_is_not_a_duplicate() {
        let mut registry = PeerRegistryState::default();
        let handle = PeerHandle::new("alpha");
        registry.add(handle.clone(), target("a")).expect("add");
        let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        registry.set_identity(&handle, identity(id));
        assert_eq!(
            registry.set_identity(&handle, identity(id)),
            SetIdentity::Unchanged
        );
    }

    #[test]
    fn identity_change_drops_workspaces_from_the_previous_server() {
        let mut registry = PeerRegistryState::default();
        let handle = PeerHandle::new("alpha");
        registry.add(handle.clone(), target("a")).expect("add");
        registry.set_identity(&handle, identity("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        registry.set_workspaces(&handle, vec![workspace("w1")]);

        assert_eq!(
            registry.set_identity(&handle, identity("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")),
            SetIdentity::Recorded {
                replaced: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
            },
            "a replacement has to be reported: state bound to the old id lives elsewhere"
        );

        let peer = registry.get(&handle).expect("peer");
        assert!(
            peer.workspaces.is_empty(),
            "workspaces from the replaced server must not be shown"
        );
        assert_eq!(peer.instance_id(), Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));
        registry.assert_invariants_for_test();
    }

    #[test]
    fn identity_refresh_for_the_same_server_keeps_workspaces() {
        let mut registry = PeerRegistryState::default();
        let handle = PeerHandle::new("alpha");
        registry.add(handle.clone(), target("a")).expect("add");
        registry.set_identity(&handle, identity("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        registry.set_workspaces(&handle, vec![workspace("w1")]);

        assert_eq!(
            registry.set_identity(&handle, identity("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")),
            SetIdentity::Unchanged
        );
        assert_eq!(registry.get(&handle).expect("peer").workspaces.len(), 1);
    }

    #[test]
    fn peer_ids_round_trip_through_the_namespace() {
        let instance = "0123456789abcdef0123456789abcdef";
        let prefixed = prefix_peer_id(instance, "w1:p3");
        assert_eq!(prefixed, format!("{instance}:w1:p3"));
        assert_eq!(split_peer_id(&prefixed), Some((instance, "w1:p3")));
        assert!(is_peer_id(&prefixed));
    }

    #[test]
    fn local_ids_are_never_mistaken_for_peer_ids() {
        // Local ids are `w` plus uppercase base32, so they cannot present a
        // leading lowercase-hex segment of instance-id length.
        for id in ["w1", "w1:p3", "w13A:t2", "p_7", "7", ""] {
            assert!(!is_peer_id(id), "{id} must stay local");
            assert!(split_peer_id(id).is_none(), "{id} must stay local");
        }
        // Right shape, wrong alphabet or length.
        assert!(split_peer_id("0123456789ABCDEF0123456789ABCDEF:w1").is_none());
        assert!(split_peer_id("0123:w1").is_none());
        assert!(split_peer_id("0123456789abcdef0123456789abcdef:").is_none());
    }

    #[test]
    fn enumeration_is_namespaced_on_ingest() {
        let mut registry = PeerRegistryState::default();
        let handle = PeerHandle::new("alpha");
        registry.add(handle.clone(), target("a")).expect("add");
        registry.set_identity(&handle, identity("0123456789abcdef0123456789abcdef"));

        assert!(registry.set_workspaces(&handle, vec![workspace("w1")]));

        let stored = &registry.get(&handle).expect("peer").workspaces[0];
        assert_eq!(stored.workspace_id, "0123456789abcdef0123456789abcdef:w1");
        assert_eq!(
            stored.active_tab_id,
            "0123456789abcdef0123456789abcdef:w1:t1"
        );
    }

    #[test]
    fn enumeration_without_an_identity_is_dropped() {
        let mut registry = PeerRegistryState::default();
        let handle = PeerHandle::new("alpha");
        registry.add(handle.clone(), target("a")).expect("add");

        // No identity yet, so there is no namespace to prefix with.
        assert!(!registry.set_workspaces(&handle, vec![workspace("w1")]));
        assert!(registry.get(&handle).expect("peer").workspaces.is_empty());
    }

    #[test]
    fn resolve_peer_id_routes_to_the_owning_peer_and_strips() {
        let mut registry = PeerRegistryState::default();
        let alpha = PeerHandle::new("alpha");
        let beta = PeerHandle::new("beta");
        registry.add(alpha.clone(), target("a")).expect("add alpha");
        registry.add(beta.clone(), target("b")).expect("add beta");
        registry.set_identity(&alpha, identity("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        registry.set_identity(&beta, identity("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));

        let (peer, local) = registry
            .resolve_peer_id("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb:w2:p1")
            .expect("routes to beta");
        assert_eq!(peer.handle, beta);
        assert_eq!(local, "w2:p1", "the peer only knows its own local id");

        assert!(registry
            .resolve_peer_id("cccccccccccccccccccccccccccccccc:w1")
            .is_none());
        assert!(registry.resolve_peer_id("w1").is_none());
        registry.assert_invariants_for_test();
    }

    #[test]
    fn remove_drops_the_peer_and_keeps_order() {
        let mut registry = PeerRegistryState::default();
        for name in ["a", "b", "c"] {
            registry
                .add(PeerHandle::new(name), target(name))
                .expect("add");
        }

        let removed = registry.remove(&PeerHandle::new("b")).expect("removed");
        assert_eq!(removed.handle, PeerHandle::new("b"));
        let order: Vec<&str> = registry.iter().map(|peer| peer.handle.as_str()).collect();
        assert_eq!(order, vec!["a", "c"]);
        assert!(registry.remove(&PeerHandle::new("b")).is_none());
    }
}
