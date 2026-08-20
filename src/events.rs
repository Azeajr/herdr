//! Internal app events delivered via channel.
//!
//! Background tasks (PTY child watchers, future hook listeners, etc.) send
//! events to the main loop through this channel. No polling needed.

use std::sync::OnceLock;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::detect::{Agent, AgentState};
use crate::layout::PaneId;
use crate::workspace::{GitStatusCacheEntry, WorkspaceGitStatus};

/// The running loop's event sender, for code that reaches it from off the loop.
///
/// Published for the same reason [`crate::render_signal::set_server_wake`] is,
/// and by the same code path: a peer view's reader thread has an event to
/// deliver — a peer forwarded its pane's OSC 52 clipboard write — but it is
/// started from a worker thread several frames below anything holding
/// application state, and the runtime only reaches the loop afterwards.
///
/// Kept to that use. Anything already holding an `App` has `event_tx` and
/// should send through it rather than reaching for this.
static SERVER_EVENTS: OnceLock<mpsc::Sender<AppEvent>> = OnceLock::new();

/// Publishes the loop's event sender. Called once, as the server starts.
pub(crate) fn set_server_events(tx: mpsc::Sender<AppEvent>) {
    let _ = SERVER_EVENTS.set(tx);
}

/// The loop's event sender, absent outside a running server.
pub(crate) fn server_events() -> Option<mpsc::Sender<AppEvent>> {
    SERVER_EVENTS.get().cloned()
}

#[derive(Debug)]
pub struct ApiWorktreeAddRequest {
    pub id: String,
    pub operation_id: u64,
    pub checkout_key: std::path::PathBuf,
    pub source_workspace_id: Option<String>,
    pub source_existing_membership: Option<crate::workspace::WorktreeSpaceMembership>,
    pub source_checkout_path: std::path::PathBuf,
    pub source_repo_root: std::path::PathBuf,
    pub repo_key: String,
    pub repo_name: String,
    pub label: Option<String>,
    pub focus: bool,
    pub respond_to: std::sync::mpsc::Sender<String>,
}

#[derive(Debug)]
pub struct WorktreeAddResult {
    pub path: std::path::PathBuf,
    pub api_request: Option<ApiWorktreeAddRequest>,
    pub result: Result<(), String>,
}

#[derive(Debug)]
pub struct ApiWorktreeRemoveRequest {
    pub id: String,
    pub operation_id: u64,
    pub checkout_key: std::path::PathBuf,
    pub respond_to: std::sync::mpsc::Sender<String>,
}

#[derive(Debug)]
pub struct WorktreeRemoveResult {
    pub workspace_id: String,
    pub path: std::path::PathBuf,
    pub workspace: Option<Box<crate::api::schema::WorkspaceInfo>>,
    pub worktree: Option<Box<crate::api::schema::WorktreeInfo>>,
    pub forced: bool,
    pub api_request: Option<ApiWorktreeRemoveRequest>,
    pub result: Result<(), String>,
}

#[derive(Debug)]
pub struct PeerCopyModeQueryResult {
    pub request: crate::app::state::PeerCopyModeQuery,
    pub result: Result<crate::api::schema::PaneTextQueryAnswer, String>,
}

/// The outcome of splitting a peer-backed pane.
///
/// Both slow steps — asking the peer to spawn a pane and connecting a view onto
/// it — run on a worker thread, so what reaches the main loop is a runtime ready
/// to be placed in the local layout. The workspace is identified by id rather
/// than index because the list can change while the peer round trip is in
/// flight.
#[derive(Debug)]
pub struct PeerPaneSplitResult {
    /// The originating API request id.
    pub id: String,
    pub workspace_id: String,
    pub target_pane: PaneId,
    pub direction: ratatui::layout::Direction,
    pub ratio: Option<f32>,
    pub focus: bool,
    /// The connected view onto the pane the peer spawned, or why there is none.
    pub result: Result<Box<crate::terminal::RemoteTerminalRuntime>, (String, String)>,
    pub respond_to: std::sync::mpsc::Sender<String>,
}

/// The outcome of asking a peer to create a workspace.
#[derive(Debug)]
pub struct PeerWorkspaceCreateResult {
    /// The originating API request id.
    pub id: String,
    pub handle: crate::app::peers::PeerHandle,
    pub label: Option<String>,
    pub focus: bool,
    /// The peer-local id of the pane the peer's new workspace opened with, or
    /// why there is none. A pane rather than a workspace id because the peer's
    /// reply already names it, which avoids waiting for the enumeration that
    /// would otherwise have to arrive before the view could be opened.
    pub result: Result<String, (String, String)>,
    pub respond_to: std::sync::mpsc::Sender<String>,
}

/// The outcome of asking a peer to create a tab in the workspace a local view
/// is attached to.
#[derive(Debug)]
pub struct PeerTabCreateResult {
    /// The originating API request id.
    pub id: String,
    /// The local workspace the new tab belongs to, re-resolved on completion
    /// because it can close while the peer round trip is in flight.
    pub workspace_id: String,
    pub label: Option<String>,
    pub focus: bool,
    /// The view already connected onto the pane the peer's new tab opened with,
    /// or why there is none.
    pub result: Result<Box<crate::terminal::RemoteTerminalRuntime>, (String, String)>,
    pub respond_to: std::sync::mpsc::Sender<String>,
}

/// A view a worker connected, and the peer-local id it actually attached to.
///
/// The id is an output rather than an input because a workspace target has to be
/// resolved to the pane behind it, and that resolution is a request to the peer —
/// so only the worker knows the answer.
#[derive(Debug)]
pub struct PeerViewOpened {
    pub runtime: Box<crate::terminal::RemoteTerminalRuntime>,
    pub local_target: String,
}

/// The worktree a peer view was opened around, and how it got there.
///
/// Present only when the view exists because a `worktree.*` request was routed
/// to the peer. It carries the peer's own worktree facts — the checkout lives on
/// that machine and nothing local can read it — so the answer this server sends
/// back names a local workspace but a remote checkout.
#[derive(Debug)]
pub enum PeerWorktreeAnswer {
    /// The peer created the checkout for this request.
    Created {
        worktree: crate::api::schema::WorktreeInfo,
    },
    /// The peer opened a checkout that already existed there. `already_open` is
    /// whether a view onto it was already open *here*, which is a different
    /// question from whether the peer already had the workspace.
    Opened {
        worktree: crate::api::schema::WorktreeInfo,
        already_open: bool,
    },
}

impl PeerWorktreeAnswer {
    /// The same answer, restated as one this server already held a view for.
    pub fn into_already_open(self) -> Self {
        match self {
            Self::Created { worktree } | Self::Opened { worktree, .. } => Self::Opened {
                worktree,
                already_open: true,
            },
        }
    }
}

/// The outcome of asking a peer to create or open a worktree of its own.
#[derive(Debug)]
pub struct PeerWorktreeViewResult {
    /// The originating API request id.
    pub id: String,
    pub handle: crate::app::peers::PeerHandle,
    pub label: Option<String>,
    pub focus: bool,
    /// The peer-local id of the pane the peer's worktree workspace opened with,
    /// and the peer's own account of the checkout behind it. A pane rather than
    /// a workspace id for the same reason `peer.workspace.create` uses one: the
    /// reply already names it, so no enumeration has to arrive first.
    pub result: Result<(String, Box<PeerWorktreeAnswer>), (String, String)>,
    pub respond_to: std::sync::mpsc::Sender<String>,
}

/// A peer's answer to `worktree.list`, as a worktree dialog needs it.
///
/// Both halves are load-bearing and neither can be had locally. The list is what
/// the open dialog offers; the source is what the new-worktree dialog names, and
/// its absence — the peer refusing because the workspace is not in a work tree —
/// is the only way this server can learn that at all. A peer that no client is
/// attached to never refreshes its cached git space, so its enumeration cannot
/// be asked instead.
#[derive(Debug)]
pub struct PeerWorktreeListing {
    pub source: crate::api::schema::WorktreeSourceInfo,
    pub worktrees: Vec<crate::api::schema::WorktreeInfo>,
}

/// The outcome of asking a peer to remove one of its own worktree checkouts.
#[derive(Debug)]
pub struct PeerWorktreeRemoveResult {
    /// The originating API request id.
    pub id: String,
    /// The local workspace holding the view, re-resolved on completion because
    /// it can close while the peer round trip is in flight.
    pub workspace_id: String,
    /// The removed checkout path on the peer and whether removal was forced, or
    /// why the peer refused.
    pub result: Result<(String, bool), (String, String)>,
    pub respond_to: std::sync::mpsc::Sender<String>,
}

/// What a freshly opened peer view should become once it reaches the loop.
#[derive(Debug)]
pub enum PeerViewPlacement {
    /// A local workspace holding one pane, as `peer.workspace.open` and
    /// `workspace.focus` on a peer id produce.
    Workspace {
        /// The peer's own workspace id behind the view, when the target named
        /// one. Nothing else records it, and it is what later names this
        /// workspace on the peer.
        peer_workspace: Option<String>,
        label: Option<String>,
        focus: bool,
        /// The worktree this view was opened around, when a routed `worktree.*`
        /// request is what opened it. The view itself is identical either way;
        /// only the answer the caller gets differs.
        worktree: Option<Box<PeerWorktreeAnswer>>,
    },
    /// A bare terminal with no workspace around it, as `peer.terminal.open`
    /// produces. The target is carried through because the response restates it.
    Terminal { target: String },
}

/// The outcome of opening a view onto a peer, off the event loop.
///
/// Connecting is a handshake with another machine and, for an ssh peer, the
/// start of an ssh child. Neither may run on the loop, so the connect happens on
/// a worker and only the placement lands here.
#[derive(Debug)]
pub struct PeerViewOpenResult {
    /// The originating API request id.
    pub id: String,
    pub handle: crate::app::peers::PeerHandle,
    /// The target as the caller named it, before the namespace was stripped.
    pub requested_target: String,
    /// The peer-local id the open was started against, which is what the
    /// in-flight guard is keyed on.
    pub started_target: String,
    pub placement: PeerViewPlacement,
    pub result: Result<Box<PeerViewOpened>, (String, String)>,
    pub respond_to: std::sync::mpsc::Sender<String>,
}

/// The outcome of one reconnect attempt for a peer-backed view.
#[derive(Debug)]
pub struct PeerViewReconnectResult {
    /// The local terminal the view belongs to. The pane and terminal outlive the
    /// connection, so the reconnected view takes the same slot.
    pub terminal_id: crate::terminal::TerminalId,
    /// The reopened view, or why it could not be reopened.
    pub result: Result<Box<crate::terminal::RemoteTerminalRuntime>, String>,
}

/// An event from a background task to the main loop.
#[derive(Debug)]
pub enum AppEvent {
    /// A pane's child process exited.
    PaneDied {
        pane_id: PaneId,
    },
    /// Process detection identified an agent before its screen state was confirmed.
    AgentProcessDetected {
        pane_id: PaneId,
        agent: Agent,
        observed_at: Instant,
    },
    /// Fallback detector state changed in a pane.
    StateChanged {
        pane_id: PaneId,
        agent: Option<Agent>,
        state: AgentState,
        visible_blocker: bool,
        visible_working: bool,
        process_exited: bool,
        observed_at: Instant,
    },
    /// Hook-authoritative agent state was reported for a pane.
    HookStateReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        seq: Option<u64>,
        session_ref: Option<crate::agent_resume::AgentSessionRef>,
    },
    /// Agent session identity was reported without state authority.
    AgentSessionReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        seq: Option<u64>,
        session_ref: Option<crate::agent_resume::AgentSessionRef>,
        session_start_source: Option<String>,
    },
    /// Display-only agent metadata was reported for a pane.
    HookMetadataReported {
        pane_id: PaneId,
        source: String,
        agent_label: Option<String>,
        applies_to_source: Option<String>,
        title: Option<String>,
        display_agent: Option<String>,
        state_labels: std::collections::HashMap<String, String>,
        clear_title: bool,
        clear_display_agent: bool,
        clear_state_labels: bool,
        seq: Option<u64>,
        ttl: Option<std::time::Duration>,
    },
    /// Hook authority was explicitly cleared for a pane.
    HookAuthorityCleared {
        pane_id: PaneId,
        source: Option<String>,
        seq: Option<u64>,
    },
    /// The current detected agent gracefully released this pane back to the shell.
    HookAgentReleased {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        known_agent: Option<Agent>,
        seq: Option<u64>,
    },
    /// A new version is available through the active installation manager.
    UpdateReady {
        version: String,
        install_command: String,
    },
    /// Remote agent detection manifest update check finished.
    AgentDetectionManifestsUpdated {
        updated: Vec<crate::detect::manifest_update::ManifestUpdateCommit>,
        status: crate::detect::manifest_update::ManifestUpdateStatus,
    },
    /// A pane child emitted one or more executable BEL characters.
    /// The host-facing process forwards them to its outer terminal.
    TerminalBell {
        pane_id: PaneId,
        count: u16,
    },
    /// A pane child emitted a valid OSC 52 clipboard write. The main loop
    /// re-emits it through herdr's own clipboard writer.
    ClipboardWrite {
        /// The pane whose child wrote it, when a pane did.
        ///
        /// `None` for herdr's own copy actions, which are already the acting
        /// client's own doing and have no pty behind them. A pane id is what
        /// lets a server also hand the write to whoever is federating that
        /// terminal, instead of only to its own foreground client.
        pane_id: Option<PaneId>,
        content: Vec<u8>,
    },
    /// Prefix-mode ASCII input-source request, emitted on entering/leaving the ASCII input
    /// realm. The foreground process applies the host-local TIS switch (`active = true`) /
    /// restore (`active = false`): the client in server mode (via server forwarding), the
    /// app itself in monolithic mode.
    PrefixInputSource {
        active: bool,
    },
    /// A pane child reported its shell current directory through terminal
    /// metadata such as OSC 7.
    TerminalCwdReported {
        pane_id: PaneId,
        cwd: std::path::PathBuf,
    },
    /// Background git status refresh completed for workspaces.
    GitStatusRefreshed {
        results: Vec<WorkspaceGitStatus>,
        cache_updates: Vec<(std::path::PathBuf, GitStatusCacheEntry)>,
    },
    /// A configured tab bar status command finished.
    TabBarCommandFinished {
        generation: u64,
        segment_index: usize,
        result: Result<Option<String>, String>,
    },
    /// A plugin action or event command finished.
    PluginCommandFinished {
        log_id: String,
        finished_unix_ms: u64,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        error: Option<String>,
    },
    /// Background `git worktree add` completed.
    WorktreeAddFinished(Box<WorktreeAddResult>),
    /// Background `git worktree remove` completed.
    WorktreeRemoveFinished(Box<WorktreeRemoveResult>),
    /// A federated peer's control channel changed state.
    PeerConnectionChanged {
        handle: crate::app::peers::PeerHandle,
        connection: crate::app::peers::PeerConnectionState,
        /// Present once the peer has identified itself on this connection.
        identity: Option<crate::app::peers::PeerIdentity>,
    },
    /// A federated peer's transport came up on a local socket.
    ///
    /// Only ssh peers emit this: a socket-path peer's target is already the
    /// socket to use.
    PeerTransportReady {
        handle: crate::app::peers::PeerHandle,
        api_socket: std::path::PathBuf,
    },
    /// A federated peer re-enumerated its workspaces.
    PeerWorkspacesUpdated {
        handle: crate::app::peers::PeerHandle,
        workspaces: Vec<crate::api::schema::WorkspaceInfo>,
    },
    /// A federated peer re-enumerated its panes.
    ///
    /// The peer owns the screen, so it is also the only side that can see a
    /// peer-backed pane's cwd, title, and agent. This carries those facts to the
    /// views that mirror them, which is what keeps a remote pane from reading as
    /// unlabeled next to a local one.
    PeerPanesUpdated {
        handle: crate::app::peers::PeerHandle,
        panes: Vec<crate::api::schema::PaneInfo>,
    },
    /// A pane was spawned on a peer for a split and a view onto it is connected.
    PeerPaneSplitFinished(Box<PeerPaneSplitResult>),
    /// A peer finished creating a workspace this server asked it for.
    PeerWorkspaceCreateFinished(Box<PeerWorkspaceCreateResult>),
    PeerTabCreateFinished(Box<PeerTabCreateResult>),
    /// A peer finished creating or opening a worktree this server asked it for.
    PeerWorktreeViewFinished(Box<PeerWorktreeViewResult>),
    /// A peer finished removing a worktree checkout this server asked it to.
    PeerWorktreeRemoveFinished(Box<PeerWorktreeRemoveResult>),
    /// A peer answered the worktree list behind a worktree dialog.
    ///
    /// The dialog it fills is named by the local view it was opened from, since
    /// the user can close or replace it while the peer is answering.
    PeerWorktreeListFinished {
        workspace_id: String,
        result: Result<Box<PeerWorktreeListing>, String>,
    },
    /// A view onto a peer finished connecting and is ready to be placed.
    PeerViewOpenFinished(Box<PeerViewOpenResult>),
    /// A reconnect attempt for a peer-backed view finished.
    PeerViewReconnected(Box<PeerViewReconnectResult>),
    /// A peer-owned terminal answered a copy-mode search or text motion.
    PeerCopyModeQueryFinished(Box<PeerCopyModeQueryResult>),
    /// A request the UI forwarded to a peer was refused.
    ///
    /// Only for UI-initiated forwards. A socket caller is waiting on its own
    /// response channel and gets the peer's answer there; a keybind or a menu
    /// item has no such caller, so its failure would otherwise be invisible.
    PeerForwardFailed {
        message: String,
    },
    /// A pane this server spawned on a peer could not be closed there.
    ///
    /// The local view is already gone by the time this arrives, so nothing can
    /// be retried or rolled back; it is recorded so the peer can report how many
    /// panes it may still be holding.
    PeerPaneCleanupFailed {
        handle: crate::app::peers::PeerHandle,
        peer_pane_id: String,
        /// The peer instance that issued `peer_pane_id`.
        ///
        /// Carried so a later retry can tell "the same server came back" from
        /// "a different server now answers here", where the same id would name
        /// somebody else's pane. `None` when the failure happened before any
        /// identity was known, which is a record that can be reported but never
        /// safely retried.
        expected_instance: Option<String>,
        reason: String,
    },
}
