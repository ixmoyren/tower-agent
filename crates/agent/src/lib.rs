//! `agent` — Rust 对应 pi 的 `packages/agent`（`pi-agent-core`）。
//!
//! 当前落地：工具执行边界（`AgentTool.execute → Service`，按 Q3 建模为
//! `Stream<AgentToolResult>`）、按名分发的工具路由、`AgentEvent` / `AgentMessage`
//! 事件类型，以及 agent 编排循环（`loop` 模块）与 `beforeToolCall` /
//! `afterToolCall` 钩子、顺序/并行工具执行。
//!
//! 关键契约：
//! - 一次工具调用 = 一次 [`tower::Service::call`]，`Response` 是
//!   [`ToolResultStream`]：中间项承载 pi 的 `onUpdate` 部分结果，末项为终态结果。
//! - 未知工具名必须产出 `is_error: true` 的结果，而不是走错误通道。
//! - 事件是 push 的、多订阅者的，不进通用 tower 中间件。
//! - 一次 [`agent_loop::run_agent_loop`] = 一次完整 agent run；并行工具执行时
//!   `tool_execution_end` 按完成顺序发出，工具结果消息按 assistant 源顺序发出。
//! - [`Agent`] 自身实现 `Service<AgentRequest, Response = AgentResponse>`（[`BoxAgentService`]），
//!   因此可以像 `tower-mcp` 那样与 axum 等 tower 生态组合。
//! - [`layer`] 提供 `BeforeToolCallLayer` / `AfterToolResultLayer`（`tool_call` / `tool_result`
//!   钩子），[`ToolRegistration`] 提供 schemars 的 typed 工具注册。

mod agent;
pub mod agent_loop;
mod error;
mod event;
mod event_bus;
pub mod layer;
mod registration;
mod router;
mod tool;

pub use agent::{
    Agent, AgentInitialState, AgentOptions, AgentRequest, AgentResponse, AgentServiceExt,
    AgentState, BoxAgentService, QueueMode,
};
pub use error::{AgentError, BoxError};
pub use event::{AgentEvent, AgentMessage};
pub use event_bus::{EventBus, Subscription};
pub use layer::{
    AfterToolResultLayer, AfterToolResultService, BeforeToolCallLayer, BeforeToolCallService,
    ToolCallDecision,
};
pub use registration::ToolRegistration;
pub use router::{ToolRouter, ToolRouterFuture};
pub use tool::{
    AgentTool, AgentToolResult, AgentToolService, AgentToolServiceExt, BoxToolService,
    PrepareArguments, Replay, ToolCallRequest, ToolExecutionMode, ToolResultStream,
};
