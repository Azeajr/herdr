use serde::{Deserialize, Serialize};

use super::workspaces::WorkspaceInfo;

/// How to reach a peer server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerTargetSpec {
    /// A herdr API socket path on this machine.
    SocketPath { path: String },
    /// An SSH destination; the socket path resolves on the remote host.
    Ssh {
        destination: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PeerAddParams {
    /// Local name for the peer. Must be unique.
    pub name: String,
    pub target: PeerTargetSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PeerRef {
    pub name: String,
}

/// Where a peer's control connection currently stands.
///
/// A generated enum rather than a free-form string, matching `PeerViewState`:
/// the two describe the same kind of fact at different scopes, and a client
/// should not get a typed answer for one and an unconstrained string for the
/// other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeerConnectionKind {
    /// The first attempt has not finished yet.
    Connecting,
    /// The peer is answering.
    Connected,
    /// The connection dropped and is being retried with backoff.
    Reconnecting,
    /// The peer cannot be federated with as configured. Retrying would only
    /// hide the misconfiguration, so nothing is retrying.
    Error,
}

impl PeerConnectionKind {
    /// Short label for status output and the sidebar.
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for PeerConnectionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// A peer and what it last reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PeerInfo {
    pub name: String,
    pub label: String,
    pub target: PeerTargetSpec,
    pub connection: PeerConnectionKind,
    /// Set while the connection is retrying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// Set when the last attempt failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The peer's server instance id, once it has identified itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    /// Whether this peer's content is currently out of date.
    pub stale: bool,
    /// Panes this server spawned on the peer and could not close there.
    ///
    /// Closing a local view cannot fail or be rolled back, so a cleanup that
    /// does not reach the peer is counted here instead of reported as an error.
    /// The peer may still be running those panes; it tracks them itself as
    /// owned by an instance that is no longer attached.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failed_pane_cleanups: usize,
    /// The peer's workspaces as of its last enumeration.
    pub workspaces: Vec<WorkspaceInfo>,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PeerTerminalOpenParams {
    /// Local name of the peer. Optional when `target` carries a peer namespace,
    /// which already identifies the peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Terminal id, pane id, or agent name to control on the peer.
    pub target: String,
    pub cols: u16,
    pub rows: u16,
    /// Replace an existing writable controller for that terminal on the peer.
    #[serde(default)]
    pub takeover: bool,
}

/// Release a peer-backed terminal opened without a workspace around it.
///
/// The counterpart to `peer.terminal.open`. A terminal a pane is attached to is
/// released by closing that pane; this is for the ones nothing holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalTarget {
    pub terminal_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PeerTerminalInfo {
    /// Local terminal id for the peer-backed terminal.
    pub terminal_id: String,
    pub name: String,
    /// Target as requested, which may be namespaced with the peer instance id.
    pub target: String,
    /// The peer-local id actually sent over the wire.
    pub local_target: String,
    pub cols: u16,
    pub rows: u16,
}

/// Create a workspace on a peer and open a local view onto it.
///
/// Distinct from `workspace.create` with a namespaced id: this names a *peer*,
/// not a peer workspace, so there is no existing id to route on. The reply is
/// the **local** workspace, the same as `peer.workspace.open`, because the
/// caller can only act on the view it now holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PeerWorkspaceCreateParams {
    /// Local name of the peer to create the workspace on.
    pub name: String,
    /// Working directory for the new workspace, resolved on the peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Label for the local view. Defaults to the peer name and target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PeerWorkspaceOpenParams {
    /// Namespaced or peer-local id of the pane or terminal to view.
    pub target: String,
    /// Local name of the peer. Optional when `target` carries a peer namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Label for the local workspace. Defaults to the peer name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub focus: bool,
    /// Replace an existing writable controller for that terminal on the peer.
    #[serde(default)]
    pub takeover: bool,
}
