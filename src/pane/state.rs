use crate::terminal::TerminalId;

/// What a pane actually hosts.
///
/// Herdr panes historically always wrapped a PTY. Browser panes do not: they
/// are driven by an `agent-browser` session (`crate::browser`) and drawn
/// entirely through the pane graphics overlay, so they register no
/// `TerminalRuntime`. They still carry an `attached_terminal_id` and a
/// `TerminalState` record, which is what gives them a label, a cwd, and an
/// identity in every surface that indexes panes by terminal; only the PTY
/// itself is absent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneKind {
    /// PTY-backed pane. The default, and what every pre-Browser snapshot
    /// deserializes to.
    #[default]
    Terminal,
    /// `agent-browser`-backed pane with no PTY child.
    Browser,
}

impl PaneKind {
    pub fn is_browser(self) -> bool {
        matches!(self, PaneKind::Browser)
    }
}

/// Viewport state for a pane.
///
/// Terminal identity, cwd, labels, and agent metadata live in TerminalState.
pub struct PaneState {
    pub attached_terminal_id: TerminalId,
    /// What this pane hosts. Source of truth for "is this a Browser pane" --
    /// it travels with the pane through splits, snapshots, and restore, so
    /// nothing has to keep a parallel id set in sync.
    pub kind: PaneKind,
    /// Whether the user has seen this pane since its last state change to Idle.
    /// False = "Done" (agent finished while user was in another workspace).
    pub seen: bool,
}

impl PaneState {
    pub fn new(attached_terminal_id: TerminalId) -> Self {
        Self {
            attached_terminal_id,
            kind: PaneKind::Terminal,
            seen: true,
        }
    }

    pub fn new_browser(attached_terminal_id: TerminalId) -> Self {
        Self {
            attached_terminal_id,
            kind: PaneKind::Browser,
            seen: true,
        }
    }

    pub fn is_browser(&self) -> bool {
        self.kind.is_browser()
    }
}
