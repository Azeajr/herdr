use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::layout::Direction;
use serde::{Deserialize, Serialize};

use crate::layout::Node;
use crate::terminal::TerminalRuntimeRegistry;
use crate::workspace::Workspace;

/// Current snapshot format version.
pub(super) const SNAPSHOT_VERSION: u32 = 3;

/// Serializable snapshot of the entire herdr session.
#[derive(Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Format version — used to detect incompatible changes.
    #[serde(default)]
    pub version: u32,
    pub workspaces: Vec<WorkspaceSnapshot>,
    pub active: Option<usize>,
    pub selected: usize,
    #[serde(default)]
    pub sidebar_width: Option<u16>,
    #[serde(default)]
    pub sidebar_section_split: Option<f32>,
    #[serde(default)]
    pub collapsed_space_keys: std::collections::HashSet<String>,
    /// Peers hidden from the sidebar for the session. Session-scoped, so it
    /// rides the snapshot rather than the config file.
    #[serde(default, skip_serializing_if = "std::collections::HashSet::is_empty")]
    pub hidden_peers: std::collections::HashSet<String>,
    /// Configured peer servers. Only how to reach a peer is stored; connection
    /// state and enumerated workspaces are rebuilt by reconnecting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<PeerSnapshot>,
}

/// A configured peer, as persisted.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct PeerSnapshot {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub target: PeerTargetSnapshot,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerTargetSnapshot {
    SocketPath {
        path: PathBuf,
    },
    Ssh {
        destination: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
pub struct SessionHistorySnapshot {
    /// Format version follows the matching session snapshot version.
    #[serde(default)]
    pub version: u32,
    pub workspaces: Vec<WorkspaceHistorySnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct WorkspaceHistorySnapshot {
    pub tabs: Vec<TabHistorySnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct TabHistorySnapshot {
    pub panes: HashMap<u32, PaneHistorySnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub custom_name: Option<String>,
    pub identity_cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_space: Option<crate::workspace::WorktreeSpaceMembership>,
    #[serde(default)]
    pub public_pane_numbers: HashMap<u32, usize>,
    #[serde(default)]
    pub next_public_pane_number: usize,
    #[serde(default)]
    pub public_tab_numbers: Vec<usize>,
    #[serde(default)]
    pub next_public_tab_number: usize,
    pub tabs: Vec<TabSnapshot>,
    #[serde(default)]
    pub active_tab: usize,
}

#[derive(Deserialize)]
struct LegacyWorkspaceSnapshot {
    #[serde(default)]
    custom_name: Option<String>,
    layout: LayoutSnapshot,
    panes: HashMap<u32, PaneSnapshot>,
    zoomed: bool,
    #[serde(default)]
    focused: Option<u32>,
    #[serde(default)]
    root_pane: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct TabSnapshot {
    #[serde(default)]
    pub custom_name: Option<String>,
    pub layout: LayoutSnapshot,
    pub panes: HashMap<u32, PaneSnapshot>,
    pub zoomed: bool,
    #[serde(default)]
    pub focused: Option<u32>,
    #[serde(default)]
    pub root_pane: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_agent_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<PaneAgentSessionSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_argv: Option<Vec<String>>,
    /// Instance id of the server that had this pane created here.
    ///
    /// Persisted so a restart does not turn every pane another server asked for
    /// into an anonymous one; without it this server loses the ability to say
    /// which panes are running for a machine that never came back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_instance_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneAgentSessionSnapshot {
    pub source: String,
    pub agent: String,
    pub kind: crate::agent_resume::AgentSessionRefKind,
    pub value: String,
}

#[derive(Serialize, Deserialize)]
pub struct PaneHistorySnapshot {
    pub ansi: String,
    pub lines: usize,
}

/// Serializable BSP tree.
#[derive(Serialize, Deserialize)]
pub enum LayoutSnapshot {
    Pane(u32),
    Split {
        direction: DirectionSnapshot,
        ratio: f32,
        first: Box<LayoutSnapshot>,
        second: Box<LayoutSnapshot>,
    },
}

#[derive(Serialize, Deserialize)]
pub enum DirectionSnapshot {
    Horizontal,
    Vertical,
}

impl From<LegacyWorkspaceSnapshot> for WorkspaceSnapshot {
    fn from(snap: LegacyWorkspaceSnapshot) -> Self {
        let identity_cwd = legacy_identity_cwd(&snap);
        let tab = TabSnapshot {
            custom_name: None,
            layout: snap.layout,
            panes: snap.panes,
            zoomed: snap.zoomed,
            focused: snap.focused,
            root_pane: snap.root_pane,
        };

        Self {
            id: None,
            custom_name: snap.custom_name,
            identity_cwd,
            worktree_space: None,
            public_pane_numbers: HashMap::new(),
            next_public_pane_number: 0,
            public_tab_numbers: Vec::new(),
            next_public_tab_number: 0,
            tabs: vec![tab],
            active_tab: 0,
        }
    }
}

#[derive(Deserialize)]
struct RawSessionSnapshot {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    workspaces: Vec<serde_json::Value>,
    #[serde(default)]
    active: Option<usize>,
    #[serde(default)]
    selected: usize,
    #[serde(default)]
    sidebar_width: Option<u16>,
    #[serde(default)]
    sidebar_section_split: Option<f32>,
    #[serde(default)]
    collapsed_space_keys: std::collections::HashSet<String>,
    #[serde(default)]
    hidden_peers: std::collections::HashSet<String>,
    #[serde(default)]
    peers: Vec<PeerSnapshot>,
}

fn migrate_snapshot(raw: RawSessionSnapshot) -> Result<SessionSnapshot, String> {
    Ok(SessionSnapshot {
        version: raw.version,
        workspaces: raw
            .workspaces
            .into_iter()
            .map(migrate_workspace)
            .collect::<Result<Vec<_>, _>>()?,
        active: raw.active,
        selected: raw.selected,
        sidebar_width: raw.sidebar_width,
        sidebar_section_split: raw.sidebar_section_split,
        collapsed_space_keys: raw.collapsed_space_keys,
        hidden_peers: raw.hidden_peers,
        peers: raw.peers,
    })
}

fn migrate_workspace(raw: serde_json::Value) -> Result<WorkspaceSnapshot, String> {
    if raw.get("identity_cwd").is_some() {
        return serde_json::from_value(raw).map_err(|e| e.to_string());
    }

    if raw.get("layout").is_some() {
        let legacy =
            serde_json::from_value::<LegacyWorkspaceSnapshot>(raw).map_err(|e| e.to_string())?;
        return Ok(legacy.into());
    }

    Err("workspace snapshot is neither current nor legacy format".to_string())
}

fn legacy_identity_cwd(snap: &LegacyWorkspaceSnapshot) -> PathBuf {
    let root_pane = snap
        .root_pane
        .or_else(|| first_pane_id_in_layout(&snap.layout));

    root_pane
        .and_then(|pane_id| snap.panes.get(&pane_id))
        .map(|pane| pane.cwd.clone())
        .or_else(|| {
            first_pane_id_in_layout(&snap.layout)
                .and_then(|pane_id| snap.panes.get(&pane_id))
                .map(|pane| pane.cwd.clone())
        })
        .or_else(|| {
            snap.panes
                .keys()
                .min()
                .and_then(|pane_id| snap.panes.get(pane_id))
                .map(|pane| pane.cwd.clone())
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()))
}

fn first_pane_id_in_layout(layout: &LayoutSnapshot) -> Option<u32> {
    match layout {
        LayoutSnapshot::Pane(id) => Some(*id),
        LayoutSnapshot::Split { first, second, .. } => {
            first_pane_id_in_layout(first).or_else(|| first_pane_id_in_layout(second))
        }
    }
}

/// Capture the current app state into a serializable snapshot.
pub fn capture(
    workspaces: &[Workspace],
    terminals: &std::collections::HashMap<
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &TerminalRuntimeRegistry,
    active: Option<usize>,
    selected: usize,
    sidebar_width: u16,
    sidebar_section_split: f32,
    collapsed_space_keys: std::collections::HashSet<String>,
    hidden_peers: std::collections::HashSet<String>,
    peers: Vec<PeerSnapshot>,
) -> SessionSnapshot {
    // Peer-backed workspaces are views onto another server's terminals. They
    // cannot be restored without that peer, and restoring one as a local
    // workspace would silently spawn a local shell in its place, so they are
    // left out entirely.
    //
    // Dropping entries renumbers what is left, so `active` and `selected` are
    // remapped onto the retained workspaces rather than carried across as raw
    // indexes into a list that no longer exists.
    let retained = retained_workspace_indices(workspaces);
    let remap = |index: usize| retained.iter().position(|kept| *kept == index);

    SessionSnapshot {
        version: SNAPSHOT_VERSION,
        workspaces: retained
            .iter()
            .map(|index| capture_workspace(&workspaces[*index], terminals, terminal_runtimes))
            .collect(),
        active: active.and_then(remap),
        // A selection landing on a dropped workspace falls back to the first
        // retained one rather than pointing past the end of the list.
        selected: remap(selected).unwrap_or(0),
        sidebar_width: Some(sidebar_width),
        sidebar_section_split: Some(sidebar_section_split),
        collapsed_space_keys,
        hidden_peers,
        peers,
    }
}

fn capture_workspace(
    ws: &Workspace,
    terminals: &std::collections::HashMap<
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        id: Some(ws.id.clone()),
        custom_name: ws.custom_name.clone(),
        identity_cwd: ws
            .resolved_identity_cwd_from(terminals, terminal_runtimes)
            .unwrap_or_else(|| ws.identity_cwd.clone()),
        worktree_space: ws.worktree_space.clone(),
        public_pane_numbers: ws
            .public_pane_numbers
            .iter()
            .map(|(pane_id, number)| (pane_id.raw(), *number))
            .collect(),
        next_public_pane_number: ws.next_public_pane_number,
        public_tab_numbers: ws.tabs.iter().map(|tab| tab.number).collect(),
        next_public_tab_number: ws.next_public_tab_number,
        tabs: ws
            .tabs
            .iter()
            .map(|tab| capture_tab(tab, terminals, terminal_runtimes))
            .collect(),
        active_tab: ws.active_tab,
    }
}

fn capture_tab(
    tab: &crate::workspace::Tab,
    terminals: &std::collections::HashMap<
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> TabSnapshot {
    let mut panes = HashMap::new();
    for id in tab.panes.keys() {
        let cwd = tab
            .cwd_for_pane(*id, terminals, terminal_runtimes)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));
        let terminal = tab
            .panes
            .get(id)
            .and_then(|pane| terminals.get(&pane.attached_terminal_id));
        let label = terminal.and_then(|terminal| terminal.manual_label.clone());
        let (agent_name, managed_agent_kind) = terminal
            .filter(|terminal| !terminal.managed_agent_launch_pending())
            .map(|terminal| {
                (
                    terminal.agent_name.clone(),
                    terminal
                        .managed_agent_kind()
                        .map(|agent| crate::detect::agent_label(agent).to_string()),
                )
            })
            .unwrap_or_default();
        let launch_argv = terminal.and_then(|terminal| terminal.launch_argv.clone());
        let agent_session = terminal.and_then(|terminal| {
            if let Some(authority) = terminal.hook_authority.as_ref() {
                if let Some(session_ref) = authority.session_ref.as_ref() {
                    return Some(PaneAgentSessionSnapshot {
                        source: authority.source.clone(),
                        agent: authority.agent_label.clone(),
                        kind: session_ref.kind,
                        value: session_ref.value.clone(),
                    });
                }
            }
            terminal
                .persisted_agent_session
                .as_ref()
                .map(|session| PaneAgentSessionSnapshot {
                    source: session.source.clone(),
                    agent: session.agent.clone(),
                    kind: session.session_ref.kind,
                    value: session.session_ref.value.clone(),
                })
        });
        panes.insert(
            id.raw(),
            PaneSnapshot {
                cwd,
                label,
                agent_name,
                managed_agent_kind,
                agent_session,
                launch_argv,
                owner_instance_id: terminal.and_then(|terminal| terminal.owner_instance_id.clone()),
            },
        );
    }
    TabSnapshot {
        custom_name: tab.custom_name.clone(),
        layout: capture_node(tab.layout.root()),
        panes,
        zoomed: tab.zoomed,
        focused: Some(tab.layout.focused().raw()),
        root_pane: Some(tab.root_pane.raw()),
    }
}

/// Which workspaces a snapshot describes, in snapshot order.
///
/// The single retention rule, shared by [`capture`] and [`capture_history`].
/// The two snapshots are zipped *positionally* on restore — `restore` reads
/// `history.workspaces[idx]` where `idx` indexes the session snapshot — so a
/// rule applied to one and not the other shifts every workspace after the first
/// dropped one and silently restores it with somebody else's scrollback, or
/// none.
fn retained_workspace_indices(workspaces: &[Workspace]) -> Vec<usize> {
    workspaces
        .iter()
        .enumerate()
        .filter_map(|(index, workspace)| workspace.peer.is_none().then_some(index))
        .collect()
}

/// Capture pane screen history separately from the structural session snapshot.
///
/// Describes exactly the workspaces [`capture`] kept, in the same order.
/// A session's history, named but not yet read.
///
/// Which panes to record, and where each sits, is decided on the server loop
/// alongside the structural snapshot, so the two describe the same moment. The
/// bytes are fetched afterwards, on the save thread, because materializing
/// every retained line takes milliseconds per populated pane and the loop is
/// also drawing frames and handling input.
pub struct SessionHistoryCapture {
    workspaces: Vec<Vec<Vec<(u32, crate::pane::PaneHistorySource)>>>,
}

impl SessionHistoryCapture {
    /// Reads every pane's history. Call this off the server loop.
    pub fn resolve(self) -> SessionHistorySnapshot {
        SessionHistorySnapshot {
            version: SNAPSHOT_VERSION,
            workspaces: self
                .workspaces
                .into_iter()
                .map(|tabs| WorkspaceHistorySnapshot {
                    tabs: tabs
                        .into_iter()
                        .map(|panes| TabHistorySnapshot {
                            panes: panes
                                .into_iter()
                                .filter_map(|(id, source)| {
                                    let ansi = source()?;
                                    let lines = ansi.lines().count();
                                    Some((id, PaneHistorySnapshot { ansi, lines }))
                                })
                                .collect(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// Names every pane whose history should be saved, without reading any of it.
pub fn capture_history(
    workspaces: &[Workspace],
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> SessionHistoryCapture {
    SessionHistoryCapture {
        workspaces: retained_workspace_indices(workspaces)
            .into_iter()
            .map(|index| {
                workspaces[index]
                    .tabs
                    .iter()
                    .map(|tab| capture_tab_history(tab, terminal_runtimes))
                    .collect()
            })
            .collect(),
    }
}

fn capture_tab_history(
    tab: &crate::workspace::Tab,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> Vec<(u32, crate::pane::PaneHistorySource)> {
    let mut panes = Vec::new();
    for (id, pane) in &tab.panes {
        if let Some(source) = terminal_runtimes
            .get(&pane.attached_terminal_id)
            .and_then(crate::terminal::TerminalRuntime::history_source)
        {
            panes.push((id.raw(), source));
        }
    }
    panes
}

pub(super) fn capture_node(node: &Node) -> LayoutSnapshot {
    match node {
        Node::Pane(id) => LayoutSnapshot::Pane(id.raw()),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => LayoutSnapshot::Split {
            direction: match direction {
                Direction::Horizontal => DirectionSnapshot::Horizontal,
                Direction::Vertical => DirectionSnapshot::Vertical,
            },
            ratio: *ratio,
            first: Box::new(capture_node(first)),
            second: Box::new(capture_node(second)),
        },
    }
}

pub(super) fn parse_snapshot(content: &str) -> Result<SessionSnapshot, String> {
    let raw = serde_json::from_str::<RawSessionSnapshot>(content).map_err(|e| e.to_string())?;
    if raw.version > SNAPSHOT_VERSION {
        return Err(format!(
            "snapshot version {} is newer than supported {}",
            raw.version, SNAPSHOT_VERSION
        ));
    }
    migrate_snapshot(raw)
}

pub(super) fn parse_history_snapshot(content: &str) -> Result<SessionHistorySnapshot, String> {
    let snapshot =
        serde_json::from_str::<SessionHistorySnapshot>(content).map_err(|e| e.to_string())?;
    if snapshot.version > SNAPSHOT_VERSION {
        return Err(format!(
            "history snapshot version {} is newer than supported {}",
            snapshot.version, SNAPSHOT_VERSION
        ));
    }
    Ok(snapshot)
}

pub(super) fn snapshot_file_version(content: &str) -> Option<u32> {
    serde_json::from_str::<RawSessionSnapshot>(content)
        .ok()
        .map(|raw| raw.version)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use ratatui::layout::{Direction, Rect};

    use super::*;
    use crate::app::{AppState, Mode};
    use crate::layout::NavDirection;
    use crate::workspace::Workspace;

    fn session_fixture(name: &str) -> &'static str {
        match name {
            "current-herdr" => {
                include_str!("../../tests/fixtures/session/current-herdr-session.json")
            }
            "current-herdr-dev" => {
                include_str!("../../tests/fixtures/session/current-herdr-dev-session.json")
            }
            "legacy-pre-tabs-v2" => {
                include_str!("../../tests/fixtures/session/legacy-pre-tabs-v2.json")
            }
            other => panic!("unknown session fixture: {other}"),
        }
    }

    fn test_session_path(name: &str) -> String {
        std::env::current_dir()
            .unwrap()
            .join(name)
            .display()
            .to_string()
    }

    fn state_with_workspaces(names: &[&str]) -> AppState {
        let mut state = AppState::test_new();
        state.workspaces = names.iter().map(|name| Workspace::test_new(name)).collect();
        state.ensure_test_terminals();
        if !state.workspaces.is_empty() {
            state.active = Some(0);
            state.selected = 0;
            state.mode = Mode::Terminal;
        }
        state
    }

    fn capture_from_state(state: &AppState) -> SessionSnapshot {
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        capture_from_state_with_runtimes(state, &terminal_runtimes)
    }

    fn capture_from_state_with_runtimes(
        state: &AppState,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> SessionSnapshot {
        capture(
            &state.workspaces,
            &state.terminals,
            terminal_runtimes,
            state.active,
            state.selected,
            state.sidebar_width,
            state.sidebar_section_split,
            state.collapsed_space_keys.clone(),
            state.hidden_peers.clone(),
            crate::persist::peer_snapshots(&state.peers),
        )
    }

    fn capture_history_from_state_with_runtimes(
        state: &AppState,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> SessionHistorySnapshot {
        // Resolving here is what the save thread does. The positions come from
        // the loop-side capture, so the pairing the callers assert is still the
        // one being tested.
        capture_history(&state.workspaces, terminal_runtimes).resolve()
    }

    fn root_split_ratio(tab: &TabSnapshot) -> Option<f32> {
        match &tab.layout {
            LayoutSnapshot::Split { ratio, .. } => Some(*ratio),
            LayoutSnapshot::Pane(_) => None,
        }
    }

    #[test]
    fn managed_agent_snapshot_omits_pending_and_persists_active_ownership() {
        let mut state = state_with_workspaces(&["managed-snapshot"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        let now = std::time::Instant::now();
        state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .begin_managed_agent(
                "reviewer".into(),
                crate::detect::Agent::Pi,
                now,
                std::time::Duration::ZERO,
                std::time::Duration::from_secs(1),
            );

        let pending = capture_from_state(&state);
        let pending_pane = &pending.workspaces[0].tabs[0].panes[&root.raw()];
        assert_eq!(pending_pane.agent_name, None);
        assert_eq!(pending_pane.managed_agent_kind, None);

        let terminal = state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_detected_state(
            Some(crate::detect::Agent::Pi),
            crate::detect::AgentState::Idle,
        );
        assert!(terminal.reconcile_managed_agent_at(now, false));
        let active = capture_from_state(&state);
        let active_pane = &active.workspaces[0].tabs[0].panes[&root.raw()];
        assert_eq!(active_pane.agent_name.as_deref(), Some("reviewer"));
        assert_eq!(active_pane.managed_agent_kind.as_deref(), Some("pi"));
    }

    #[test]
    fn round_trip_empty_session() {
        let snap = SessionSnapshot {
            version: SNAPSHOT_VERSION,
            workspaces: vec![],
            active: None,
            selected: 0,
            sidebar_width: Some(26),
            sidebar_section_split: Some(0.5),
            collapsed_space_keys: std::collections::HashSet::new(),
            hidden_peers: std::collections::HashSet::new(),
            peers: Vec::new(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let restored = parse_snapshot(&json).unwrap();
        assert!(restored.workspaces.is_empty());
        assert_eq!(restored.active, None);
        assert_eq!(restored.sidebar_width, Some(26));
        assert_eq!(restored.sidebar_section_split, Some(0.5));
    }

    #[test]
    fn round_trip_layout_snapshot() {
        let layout = LayoutSnapshot::Split {
            direction: DirectionSnapshot::Horizontal,
            ratio: 0.6,
            first: Box::new(LayoutSnapshot::Pane(0)),
            second: Box::new(LayoutSnapshot::Split {
                direction: DirectionSnapshot::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Pane(1)),
                second: Box::new(LayoutSnapshot::Pane(2)),
            }),
        };
        let json = serde_json::to_string(&layout).unwrap();
        let restored: LayoutSnapshot = serde_json::from_str(&json).unwrap();

        match restored {
            LayoutSnapshot::Split { ratio, .. } => assert!((ratio - 0.6).abs() < 0.01),
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn round_trip_full_workspace_snapshot() {
        let mut panes = HashMap::new();
        panes.insert(
            0,
            PaneSnapshot {
                cwd: PathBuf::from("/home/can/Projects/herdr"),
                label: None,
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
                owner_instance_id: None,
            },
        );
        panes.insert(
            1,
            PaneSnapshot {
                cwd: PathBuf::from("/home/can/Projects/website"),
                label: Some("website".into()),
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
                owner_instance_id: None,
            },
        );

        let snap = SessionSnapshot {
            workspaces: vec![WorkspaceSnapshot {
                id: Some("wproj".to_string()),
                custom_name: Some("pi-mono".to_string()),
                identity_cwd: PathBuf::from("/home/can/Projects/herdr"),
                worktree_space: None,
                public_pane_numbers: HashMap::from([(0, 1), (1, 2)]),
                next_public_pane_number: 3,
                public_tab_numbers: vec![1],
                next_public_tab_number: 2,
                tabs: vec![TabSnapshot {
                    custom_name: Some("api".to_string()),
                    layout: LayoutSnapshot::Split {
                        direction: DirectionSnapshot::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutSnapshot::Pane(0)),
                        second: Box::new(LayoutSnapshot::Pane(1)),
                    },
                    panes,
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: Some(26),
            sidebar_section_split: Some(0.5),
            collapsed_space_keys: std::collections::HashSet::new(),
            hidden_peers: std::collections::HashSet::new(),
            peers: Vec::new(),
            version: SNAPSHOT_VERSION,
        };

        let json = serde_json::to_string_pretty(&snap).unwrap();
        let restored = parse_snapshot(&json).unwrap();

        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.workspaces[0].id.as_deref(), Some("wproj"));
        assert_eq!(
            restored.workspaces[0].custom_name.as_deref(),
            Some("pi-mono")
        );
        assert_eq!(restored.workspaces[0].tabs.len(), 1);
        assert_eq!(restored.workspaces[0].tabs[0].panes.len(), 2);
        assert_eq!(
            restored.workspaces[0].tabs[0].panes[&0].cwd,
            PathBuf::from("/home/can/Projects/herdr")
        );
        assert_eq!(
            restored.workspaces[0].tabs[0].panes[&1].label.as_deref(),
            Some("website")
        );
        assert_eq!(restored.sidebar_width, Some(26));
        assert_eq!(restored.sidebar_section_split, Some(0.5));
    }

    #[test]
    fn current_session_fixture_parses() {
        let snap = parse_snapshot(session_fixture("current-herdr")).unwrap();

        assert_eq!(snap.version, 3);
        assert_eq!(snap.workspaces.len(), 2);
        assert_eq!(snap.active, Some(0));
        assert_eq!(snap.selected, 0);
        assert_eq!(snap.sidebar_width, None);
        assert_eq!(snap.sidebar_section_split, None);
        assert_eq!(snap.workspaces[0].tabs.len(), 2);
        assert_eq!(
            snap.workspaces[1].identity_cwd,
            PathBuf::from("/home/test/projects/project-b")
        );
    }

    #[test]
    fn current_dev_session_fixture_parses_additive_fields() {
        let snap = parse_snapshot(session_fixture("current-herdr-dev")).unwrap();

        assert_eq!(snap.version, 3);
        assert_eq!(snap.workspaces.len(), 2);
        assert_eq!(snap.sidebar_section_split, Some(0.4));
        assert_eq!(snap.workspaces[0].active_tab, 1);
        assert_eq!(snap.workspaces[1].tabs[0].panes.len(), 2);
    }

    #[test]
    fn old_snapshot_defaults_sidebar_fields() {
        let json = serde_json::json!({
            "version": SNAPSHOT_VERSION,
            "workspaces": [],
            "active": null,
            "selected": 0
        })
        .to_string();

        let restored = parse_snapshot(&json).unwrap();

        assert_eq!(restored.sidebar_width, None);
        assert_eq!(restored.sidebar_section_split, None);
    }

    #[test]
    fn old_pane_snapshot_with_embedded_history_is_ignored() {
        let json = serde_json::json!({
            "version": SNAPSHOT_VERSION,
            "workspaces": [{
                "id": "wtest",
                "identity_cwd": "/tmp",
                "tabs": [{
                    "layout": { "Pane": 0 },
                    "panes": {
                        "0": {
                            "cwd": "/tmp",
                            "history": {
                                "ansi": "legacy-secret",
                                "lines": 1
                            }
                        }
                    },
                    "zoomed": false,
                    "focused": 0,
                    "root_pane": 0
                }],
                "active_tab": 0
            }],
            "active": 0,
            "selected": 0
        })
        .to_string();

        let restored = parse_snapshot(&json).unwrap();

        let encoded = serde_json::to_string(&restored).unwrap();
        assert!(!encoded.contains("legacy-secret"));
        assert!(!encoded.contains("\"history\""));
    }

    #[test]
    fn legacy_workspace_snapshot_migrates_to_single_tab() {
        let snap = parse_snapshot(session_fixture("legacy-pre-tabs-v2")).unwrap();
        let ws = &snap.workspaces[0];

        assert_eq!(snap.version, 2);
        assert_eq!(snap.workspaces.len(), 1);
        assert_eq!(ws.custom_name.as_deref(), Some("legacy"));
        assert_eq!(ws.identity_cwd, PathBuf::from("/tmp/pion"));
        assert_eq!(ws.active_tab, 0);
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.tabs[0].focused, Some(1));
        assert_eq!(ws.tabs[0].root_pane, Some(0));
        assert_eq!(ws.tabs[0].panes[&0].cwd, PathBuf::from("/tmp/pion"));
        assert_eq!(ws.tabs[0].panes[&1].cwd, PathBuf::from("/tmp/herdr"));
    }

    #[test]
    fn capture_contract_tracks_workspace_order_active_and_selected() {
        let mut state = state_with_workspaces(&["a", "b", "c"]);
        state.active = Some(1);
        state.selected = 2;

        state.move_workspace(1, 0);

        let snapshot = capture_from_state(&state);
        let ids: Vec<_> = state.workspaces.iter().map(|ws| ws.id.clone()).collect();
        let captured_ids: Vec<_> = snapshot
            .workspaces
            .iter()
            .map(|ws| ws.id.clone().unwrap())
            .collect();
        assert_eq!(captured_ids, ids);
        assert_eq!(snapshot.active, state.active);
        assert_eq!(snapshot.selected, state.selected);
    }

    #[test]
    fn capture_contract_tracks_workspace_and_tab_names_and_active_tab() {
        let mut state = state_with_workspaces(&["one"]);
        state.workspaces[0].set_custom_name("renamed-workspace".into());
        let second_tab = state.workspaces[0].test_add_tab(Some("logs"));
        state.workspaces[0].switch_tab(second_tab);
        state.workspaces[0].tabs[0].set_custom_name("main".into());

        let snapshot = capture_from_state(&state);
        let workspace = &snapshot.workspaces[0];
        assert_eq!(workspace.custom_name.as_deref(), Some("renamed-workspace"));
        assert_eq!(workspace.active_tab, second_tab);
        assert_eq!(workspace.tabs[0].custom_name.as_deref(), Some("main"));
        assert_eq!(workspace.tabs[1].custom_name.as_deref(), Some("logs"));
    }

    #[test]
    fn capture_contract_tracks_workspace_closure() {
        let mut state = state_with_workspaces(&["one", "two"]);
        state.selected = 1;
        state.active = Some(1);

        state.close_selected_workspace();

        let snapshot = capture_from_state(&state);
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].custom_name.as_deref(), Some("one"));
        assert_eq!(snapshot.active, Some(0));
        assert_eq!(snapshot.selected, 0);
    }

    #[test]
    fn capture_contract_tracks_sidebar_state() {
        let mut state = state_with_workspaces(&["one"]);
        state.sidebar_width = 31;
        state.sidebar_section_split = 0.4;
        state.collapsed_space_keys.insert("repo-key".into());

        let snapshot = capture_from_state(&state);
        assert_eq!(snapshot.sidebar_width, Some(31));
        assert_eq!(snapshot.sidebar_section_split, Some(0.4));
        assert!(snapshot.collapsed_space_keys.contains("repo-key"));
    }

    #[test]
    fn capture_contract_tracks_worktree_space_membership() {
        let mut state = state_with_workspaces(&["main"]);
        state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: PathBuf::from("/repo/herdr"),
            checkout_path: PathBuf::from("/repo/herdr/worktree-a"),
            is_linked_worktree: true,
        });

        let snapshot = capture_from_state(&state);

        assert_eq!(
            snapshot.workspaces[0].worktree_space,
            state.workspaces[0].worktree_space
        );
    }

    #[test]
    fn capture_contract_tracks_layout_focus_zoom_and_root_pane() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        state.workspaces[0].tabs[0].layout.focus_pane(second);
        state.toggle_zoom();

        let snapshot = capture_from_state(&state);
        let tab = &snapshot.workspaces[0].tabs[0];
        assert!(matches!(tab.layout, LayoutSnapshot::Split { .. }));
        assert_eq!(tab.focused, Some(second.raw()));
        assert_eq!(tab.root_pane, Some(root.raw()));
        assert!(tab.zoomed);
        assert_eq!(tab.panes.len(), 2);
    }

    #[test]
    fn capture_contract_tracks_focus_navigation() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        crate::ui::compute_view(&mut state, Rect::new(0, 0, 106, 20));

        state.navigate_pane(NavDirection::Right);

        let snapshot = capture_from_state(&state);
        assert_eq!(snapshot.workspaces[0].tabs[0].focused, Some(second.raw()));
        assert_ne!(snapshot.workspaces[0].tabs[0].focused, Some(root.raw()));
    }

    #[test]
    fn capture_contract_tracks_resize_ratio_changes() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        state.workspaces[0].test_split(Direction::Horizontal);
        state.workspaces[0].layout.focus_pane(root);
        crate::ui::compute_view(&mut state, Rect::new(0, 0, 106, 20));
        let before = capture_from_state(&state);

        state.resize_pane(NavDirection::Right);

        let after = capture_from_state(&state);
        let before_ratio = root_split_ratio(&before.workspaces[0].tabs[0]).unwrap();
        let after_ratio = root_split_ratio(&after.workspaces[0].tabs[0]).unwrap();
        assert_ne!(before_ratio, after_ratio);
    }

    #[test]
    fn capture_contract_tracks_tab_closure() {
        let mut state = state_with_workspaces(&["one"]);
        let second_tab = state.workspaces[0].test_add_tab(Some("logs"));
        state.switch_tab(second_tab);

        state.close_tab();

        let snapshot = capture_from_state(&state);
        let workspace = &snapshot.workspaces[0];
        assert_eq!(workspace.tabs.len(), 1);
        assert_eq!(workspace.active_tab, 0);
        assert!(workspace.tabs[0].custom_name.is_none());
    }

    #[test]
    fn capture_contract_tracks_pane_closure() {
        let mut state = state_with_workspaces(&["one"]);
        state.workspaces[0].test_split(Direction::Horizontal);

        state.close_pane();

        let snapshot = capture_from_state(&state);
        let tab = &snapshot.workspaces[0].tabs[0];
        assert_eq!(tab.panes.len(), 1);
        assert!(matches!(tab.layout, LayoutSnapshot::Pane(_)));
        assert!(!tab.zoomed);
    }

    #[test]
    fn capture_contract_tracks_public_id_counters() {
        let mut state = state_with_workspaces(&["one"]);
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        let third = state.workspaces[0].test_split(Direction::Vertical);
        let second_tab = state.workspaces[0].test_add_tab(None);

        state.workspaces[0].close_pane(second);

        let snapshot = capture_from_state(&state);
        let workspace = &snapshot.workspaces[0];
        assert_eq!(
            workspace.public_pane_numbers,
            HashMap::from([
                (state.workspaces[0].tabs[0].root_pane.raw(), 1),
                (third.raw(), 3),
                (state.workspaces[0].tabs[second_tab].root_pane.raw(), 4),
            ])
        );
        assert_eq!(workspace.next_public_pane_number, 5);
        assert_eq!(workspace.public_tab_numbers, vec![1, 2]);
        assert_eq!(workspace.next_public_tab_number, 3);
    }

    #[test]
    fn capture_contract_tracks_workspace_identity_and_pane_cwds() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        state.workspaces[0].identity_cwd = PathBuf::from("/tmp/pion");
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        state.ensure_test_terminals();
        let root_terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        state.terminals.get_mut(&root_terminal_id).unwrap().cwd = PathBuf::from("/tmp/pion");
        let second_terminal_id = state.workspaces[0].tabs[0].panes[&second]
            .attached_terminal_id
            .clone();
        state.terminals.get_mut(&second_terminal_id).unwrap().cwd = PathBuf::from("/tmp/herdr");

        let snapshot = capture_from_state(&state);
        let workspace = &snapshot.workspaces[0];
        let tab = &workspace.tabs[0];
        assert_eq!(workspace.identity_cwd, PathBuf::from("/tmp/pion"));
        assert_eq!(tab.panes[&root.raw()].cwd, PathBuf::from("/tmp/pion"));
        assert_eq!(tab.panes[&second.raw()].cwd, PathBuf::from("/tmp/herdr"));
    }

    #[tokio::test]
    async fn capture_contract_tracks_pane_history_from_runtime() {
        let state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        let mut terminal_runtimes = TerminalRuntimeRegistry::new();
        terminal_runtimes.insert(
            terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                20,
                3,
                4096,
                b"alpha\r\nbeta\r\ngamma\r\n",
            ),
        );

        let snapshot = capture_from_state_with_runtimes(&state, &terminal_runtimes);
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("alpha"));
        assert!(!encoded.contains("\"history\""));

        let history_snapshot = capture_history_from_state_with_runtimes(&state, &terminal_runtimes);
        let history = &history_snapshot.workspaces[0].tabs[0].panes[&root.raw()];

        assert!(history.ansi.contains("alpha"));
        assert!(history.ansi.contains("gamma"));
        assert!(history.lines >= 3);
    }

    #[tokio::test]
    async fn capture_contract_tracks_history_for_each_pane() {
        let mut state = state_with_workspaces(&["one"]);
        let first = state.workspaces[0].tabs[0].root_pane;
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        let first_terminal_id = state.workspaces[0].tabs[0].panes[&first]
            .attached_terminal_id
            .clone();
        let second_terminal_id = state.workspaces[0].tabs[0].panes[&second]
            .attached_terminal_id
            .clone();
        let mut terminal_runtimes = TerminalRuntimeRegistry::new();
        terminal_runtimes.insert(
            first_terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                20,
                3,
                4096,
                b"first-pane-history\r\n",
            ),
        );
        terminal_runtimes.insert(
            second_terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                20,
                3,
                4096,
                b"second-pane-history\r\n",
            ),
        );

        let snapshot = capture_from_state_with_runtimes(&state, &terminal_runtimes);
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("first-pane-history"));
        assert!(!encoded.contains("second-pane-history"));

        let history_snapshot = capture_history_from_state_with_runtimes(&state, &terminal_runtimes);
        let tab = &history_snapshot.workspaces[0].tabs[0];
        let first_history = &tab.panes[&first.raw()];
        let second_history = &tab.panes[&second.raw()];

        assert!(first_history.ansi.contains("first-pane-history"));
        assert!(second_history.ansi.contains("second-pane-history"));
    }

    #[test]
    fn capture_contract_tracks_hook_authority_agent_session() {
        let mut state = state_with_workspaces(&["one"]);
        let session_path = test_session_path("pi-session.jsonl");
        let root = state.workspaces[0].tabs[0].root_pane;
        state.ensure_test_terminals();
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        let terminal = state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_detected_state(
            Some(crate::detect::Agent::Pi),
            crate::detect::AgentState::Idle,
        );
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            session_ref: crate::agent_resume::AgentSessionRef::path(session_path.clone()).unwrap(),
        });
        terminal.set_hook_authority_with_session_ref(
            "herdr:pi".into(),
            "pi".into(),
            crate::detect::AgentState::Working,
            None,
            crate::agent_resume::AgentSessionRef::path(session_path.clone()),
            Some(20),
        );

        let snapshot = capture_from_state(&state);
        let agent_session = snapshot.workspaces[0].tabs[0].panes[&root.raw()]
            .agent_session
            .as_ref()
            .expect("agent session should be captured");

        assert_eq!(agent_session.source, "herdr:pi");
        assert_eq!(agent_session.agent, "pi");
        assert_eq!(
            agent_session.kind,
            crate::agent_resume::AgentSessionRefKind::Path
        );
        assert_eq!(agent_session.value, session_path);
    }

    #[test]
    fn capture_contract_preserves_restored_agent_session() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0].root_pane;
        state.ensure_test_terminals();
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "herdr:opencode".into(),
                agent: "opencode".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id("opencode-session").unwrap(),
            });

        let snapshot = capture_from_state(&state);
        let agent_session = snapshot.workspaces[0].tabs[0].panes[&root.raw()]
            .agent_session
            .as_ref()
            .expect("persisted agent session should be captured");

        assert_eq!(agent_session.source, "herdr:opencode");
        assert_eq!(agent_session.agent, "opencode");
        assert_eq!(
            agent_session.kind,
            crate::agent_resume::AgentSessionRefKind::Id
        );
        assert_eq!(agent_session.value, "opencode-session");
    }

    #[test]
    fn old_unversioned_snapshot_loads_as_version_0() {
        let json = r#"{"workspaces":[],"active":null,"selected":0}"#;
        let snap = parse_snapshot(json).unwrap();
        assert_eq!(snap.version, 0);
    }

    #[test]
    fn future_version_is_rejected() {
        let json = r#"{"version":999,"workspaces":[],"active":null,"selected":0}"#;
        assert!(parse_snapshot(json).is_err());
    }

    #[test]
    fn active_tab_default_is_zero() {
        let json = r#"{"custom_name":"test","identity_cwd":"/tmp","tabs":[]}"#;
        let ws: WorkspaceSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(ws.active_tab, 0);
    }

    #[test]
    fn restore_falls_back_to_home_when_cwd_missing() {
        let mut panes = HashMap::new();
        panes.insert(
            0,
            PaneSnapshot {
                cwd: PathBuf::from("/tmp/this-directory-does-not-exist-for-herdr-test"),
                label: None,
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
                owner_instance_id: None,
            },
        );
        panes.insert(
            1,
            PaneSnapshot {
                cwd: std::env::var("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| PathBuf::from("/tmp")),
                label: None,
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
                owner_instance_id: None,
            },
        );

        let snap = SessionSnapshot {
            version: SNAPSHOT_VERSION,
            workspaces: vec![WorkspaceSnapshot {
                id: Some("test-ws".to_string()),
                custom_name: Some("fallback test".to_string()),
                identity_cwd: PathBuf::from("/tmp"),
                worktree_space: None,
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Split {
                        direction: DirectionSnapshot::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutSnapshot::Pane(0)),
                        second: Box::new(LayoutSnapshot::Pane(1)),
                    },
                    panes,
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: Some(26),
            sidebar_section_split: Some(0.5),
            collapsed_space_keys: std::collections::HashSet::new(),
            hidden_peers: std::collections::HashSet::new(),
            peers: Vec::new(),
        };

        let json = serde_json::to_string(&snap).unwrap();
        let restored = parse_snapshot(&json).unwrap();
        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(
            restored.workspaces[0].tabs[0].panes[&0].cwd,
            PathBuf::from("/tmp/this-directory-does-not-exist-for-herdr-test")
        );
    }
    #[test]
    fn peer_snapshots_store_reach_but_not_live_state() {
        let mut peers = crate::app::peers::PeerRegistryState::default();
        let alpha = crate::app::peers::PeerHandle::new("alpha");
        peers
            .add(
                alpha.clone(),
                crate::app::peers::PeerTarget::SocketPath("/tmp/a.sock".into()),
            )
            .expect("add");
        peers
            .add(
                crate::app::peers::PeerHandle::new("beta"),
                crate::app::peers::PeerTarget::Ssh {
                    destination: "user@host".into(),
                    session: Some("work".into()),
                },
            )
            .expect("add");
        peers.set_identity(
            &alpha,
            crate::app::peers::PeerIdentity {
                instance_id: "0123456789abcdef0123456789abcdef".into(),
                version: Some("0.7.5".into()),
                protocol: Some(20),
            },
        );
        peers.set_connection(&alpha, crate::app::peers::PeerConnectionState::Connected);

        let snapshots = crate::persist::peer_snapshots(&peers);
        assert_eq!(snapshots.len(), 2);
        // A label matching the target default is not worth storing.
        assert_eq!(snapshots[0].label, None);

        let restored = crate::persist::restore_peers(&snapshots);
        let restored_alpha = restored.get(&alpha).expect("alpha restored");
        assert_eq!(
            restored_alpha.target,
            crate::app::peers::PeerTarget::SocketPath("/tmp/a.sock".into())
        );
        // Live state is deliberately not restored: it describes a connection
        // that no longer exists.
        assert_eq!(
            restored_alpha.connection,
            crate::app::peers::PeerConnectionState::Connecting
        );
        assert!(restored_alpha.identity.is_none());
        assert!(restored_alpha.workspaces.is_empty());

        let beta = restored
            .get(&crate::app::peers::PeerHandle::new("beta"))
            .expect("beta restored");
        assert_eq!(
            beta.target,
            crate::app::peers::PeerTarget::Ssh {
                destination: "user@host".into(),
                session: Some("work".into()),
            }
        );
        restored.assert_invariants_for_test();
    }

    #[test]
    fn restore_peers_drops_hand_edited_duplicates() {
        let snapshots = vec![
            crate::persist::PeerSnapshot {
                name: "alpha".into(),
                label: Some("first".into()),
                target: crate::persist::PeerTargetSnapshot::SocketPath {
                    path: "/tmp/a.sock".into(),
                },
            },
            crate::persist::PeerSnapshot {
                name: "alpha".into(),
                label: Some("second".into()),
                target: crate::persist::PeerTargetSnapshot::SocketPath {
                    path: "/tmp/b.sock".into(),
                },
            },
        ];

        let restored = crate::persist::restore_peers(&snapshots);
        assert_eq!(restored.iter().count(), 1);
        assert_eq!(
            restored
                .get(&crate::app::peers::PeerHandle::new("alpha"))
                .expect("alpha")
                .label,
            "first"
        );
        restored.assert_invariants_for_test();
    }
    #[tokio::test]
    async fn peer_backed_workspaces_are_not_captured() {
        let mut state = AppState::test_new();
        let runtimes = TerminalRuntimeRegistry::new();
        state.workspaces.push(Workspace::test_new("local-a"));
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let (events, _events_rx) = tokio::sync::mpsc::channel(64);
        let (remote, _terminal, _runtime) = Workspace::new_attached(
            std::path::PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            Some("beta:w1:p1".to_string()),
            runtime,
            events,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            std::sync::Arc::new(crate::render_signal::RenderSignal::new()),
        );
        state.workspaces.push(remote);
        state.workspaces.push(Workspace::test_new("local-b"));

        // Focus and selection both sit on workspaces after the dropped one.
        state.active = Some(2);
        state.selected = 2;

        let snapshot = capture_from_state_with_runtimes(&state, &runtimes);

        assert_eq!(
            snapshot.workspaces.len(),
            2,
            "a peer-backed workspace must not reach the snapshot"
        );
        // Index 2 of three becomes index 1 of two once the peer view is dropped.
        assert_eq!(snapshot.active, Some(1));
        assert_eq!(snapshot.selected, 1);
    }

    /// The regression this guards: `capture` learned to drop peer-backed
    /// workspaces and `capture_history` did not. Restore zips the two lists
    /// positionally, so from the first peer view onward every local workspace
    /// read somebody else's history entry — and peer views carry none, so each
    /// one silently came back with no scrollback at all.
    #[tokio::test]
    async fn history_describes_exactly_the_workspaces_the_snapshot_captured() {
        let mut state = AppState::test_new();
        let runtimes = TerminalRuntimeRegistry::new();
        state.workspaces.push(Workspace::test_new("local-a"));
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let (events, _events_rx) = tokio::sync::mpsc::channel(64);
        let (remote, _terminal, _runtime) = Workspace::new_attached(
            std::path::PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            Some("beta:w1:p1".to_string()),
            runtime,
            events,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            std::sync::Arc::new(crate::render_signal::RenderSignal::new()),
        );
        state.workspaces.push(remote);
        state.workspaces.push(Workspace::test_new("local-b"));

        let snapshot = capture_from_state_with_runtimes(&state, &runtimes);
        let history = capture_history_from_state_with_runtimes(&state, &runtimes);

        assert_eq!(
            history.workspaces.len(),
            snapshot.workspaces.len(),
            "history must describe the same workspace list the snapshot captured"
        );
        // The pairing is positional, so equal lengths is necessary but not
        // sufficient: the shapes have to line up entry for entry too.
        for (index, (workspace, workspace_history)) in snapshot
            .workspaces
            .iter()
            .zip(&history.workspaces)
            .enumerate()
        {
            assert_eq!(
                workspace.tabs.len(),
                workspace_history.tabs.len(),
                "workspace {index} pairs with a history entry describing a different workspace"
            );
        }
    }

    #[tokio::test]
    async fn focus_on_a_peer_workspace_does_not_survive_capture() {
        let mut state = AppState::test_new();
        let runtimes = TerminalRuntimeRegistry::new();
        state.workspaces.push(Workspace::test_new("local-a"));
        let (runtime, _rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let (events, _events_rx) = tokio::sync::mpsc::channel(64);
        let (remote, _terminal, _runtime) = Workspace::new_attached(
            std::path::PathBuf::from("/"),
            "beta".to_string(),
            Some("w1".to_string()),
            None,
            runtime,
            events,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            std::sync::Arc::new(crate::render_signal::RenderSignal::new()),
        );
        state.workspaces.push(remote);
        state.active = Some(1);
        state.selected = 1;

        let snapshot = capture_from_state_with_runtimes(&state, &runtimes);

        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(
            snapshot.active, None,
            "focus must not point at a dropped workspace"
        );
        assert_eq!(
            snapshot.selected, 0,
            "selection falls back to a real workspace"
        );
    }
}
