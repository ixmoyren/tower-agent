//! `tower-agent-core` — tower-agent 的核心契约。
//!
//! 只放「契约与类型」，不放编排实现：agent 请求/响应与流式事件契约、消息/事件类型、
//! 工具边界、生命周期钩子层、typed 工具注册、错误与取消约定。有状态运行时（编排循环、
//! 队列、事件总线）在 `agent`（将改名为 `tower-agent-runtime`）。

mod error;
mod event;
pub mod layer;
mod registration;
mod route;
mod service;
mod tool;
mod ui;

pub use error::{AgentError, BoxError};
pub use event::{AgentEvent, AgentMessage};
pub use layer::{
    AfterToolResultLayer, AfterToolResultService, BeforeToolCallLayer, BeforeToolCallService,
    ToolCallDecision,
};
pub use registration::ToolRegistration;
pub use route::{ToolNotFound, ToolRoute};
pub use service::{
    AgentEventStream, AgentRequest, AgentResponse, AgentServiceExt, BoxAgentService, SessionStore,
};
pub use tool::{
    AgentTool, AgentToolResult, AgentToolService, AgentToolServiceExt, BoxToolService,
    PrepareArguments, Replay, ToolAnnotations, ToolCallRequest, ToolExecutionMode, ToolExposure,
    ToolMetadata, ToolNamespace, ToolResultStream,
};
pub use ui::{
    Panel, PanelAnchor, StatusSegment, ToolRenderer, UiContribution, UiState, render_template,
};
