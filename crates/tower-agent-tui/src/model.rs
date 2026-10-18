//! Compatibility re-exports for the view model now owned by `tower-agent-ui`.

pub use tower_agent_ui::render_message;
pub use tower_agent_ui::{Entry, EntryKind, ToolEntry, ToolResultView, UiModel};

/// Backward-compatible terminal-facing name for the shared UI model.
pub use tower_agent_ui::UiModel as TuiState;
