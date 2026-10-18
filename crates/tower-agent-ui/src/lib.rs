//! Frontend-neutral UI backend for tower-agent.
//!
//! This crate owns the agent-facing half of an interactive frontend:
//!
//! - [`UiModel`] folds [`AgentEvent`]s into a transcript view model.
//! - [`AgentUi`] applies events, executes slash commands, and turns editor text
//!   into an action for the host (`submit`, `handled`, or `quit`).
//! - [`CommandHost`] is the small synchronous session-control port implemented
//!   by a CLI, GUI, or server host.
//! - [`AgentUi`] also implements [`StaticPlugin`], so frontends can register the
//!   same state reducer as a tower-agent plugin.
//!
//! It deliberately has no rendering dependency. `tower-agent-tui` consumes this
//! crate for terminal rendering, while GUI and axum hosts can use it directly to
//! obtain snapshots or serialize [`UiModel`] as JSON.

pub mod command;
pub mod controller;
pub mod model;

pub use command::{
    BUILTIN_SLASH_COMMANDS, CommandHost, SlashCommand, hotkeys, lookup, matching, menu,
};
pub use controller::{AgentUi, CommandOutcome, UiAction, execute_command};
pub use model::{Entry, EntryKind, ToolEntry, ToolResultView, UiModel, render_message};
pub use tower_agent_core::{
    AgentEvent, AgentMessage, AgentToolResult, Panel, PanelAnchor, StatusSegment, ToolRenderer,
    UiContribution, UiState, render_template,
};
pub use tower_agent_plugins::StaticPlugin;
pub use tower_agent_provider::{ContentBlock, JsonValue, Message, ToolResultMessage, UserContent};
pub use tower_agent_runtime::Subscription;
