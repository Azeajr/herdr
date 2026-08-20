mod history_read;
mod id;
mod remote;
mod runtime;
mod runtime_registry;
mod size;
pub mod state;
mod title;

pub(crate) use history_read::{merge_scrolled_up, snapshot_text, ScreenSnapshot, UpwardMerge};
pub use id::TerminalId;
pub(crate) use remote::{RemotePaneMetadata, RemoteTerminalRuntime};
pub use runtime::TerminalRuntime;
pub(crate) use runtime_registry::TerminalRuntimeRegistry;
pub use size::TerminalSize;
pub use state::{
    AgentMetadataReport, EffectivePresentation, EffectiveStateChange, TerminalState,
    TerminalStateMutation,
};
pub(crate) use title::stripped_terminal_title;
