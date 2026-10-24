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
//! - [`ShutdownCoordinator`] 为 Axum/tonic 等宿主提供排空、超时取消、资源 Hook 与
//!   新请求拒绝；`AgentService`、`AgentStreamService`、`AgentPool` 都接入同一生命周期。
//! - [`layer`] 提供 `BeforeToolCallLayer` / `AfterToolResultLayer`（`tool_call` / `tool_result`
//!   钩子），[`ToolRegistration`] 提供 schemars 的 typed 工具注册。

mod agent;
pub mod agent_loop;
pub mod compaction;
mod control;
mod event_bus;
mod router;
mod service;
mod shutdown;

pub use agent::{Agent, AgentInitialState, AgentOptions, AgentState, QueueMode};
pub use compaction::{
    AfterCompact, BeforeCompact, Compaction, CompactionDecision, CompactionError, CompactionFailed,
    CompactionHook, CompactionReason, CompactionSettings, Compactor,
};
pub use control::{AgentControl, BoxAgentControl};
pub use event_bus::{EventBus, Subscription};
pub use router::{ToolRouter, ToolRouterFuture};
pub use service::{AgentPool, AgentService, AgentStreamService, MemorySessionStore};
pub use shutdown::{
    ShutdownCoordinator, ShutdownGuard, ShutdownHook, ShutdownOptions, ShutdownReport,
    ShutdownState,
};
pub use tower_agent_core::{
    AfterToolResultLayer, AfterToolResultService, AgentError, AgentEvent, AgentEventStream,
    AgentMessage, AgentRequest, AgentResponse, AgentServiceExt, AgentTool, AgentToolResult,
    AgentToolService, AgentToolServiceExt, BeforeToolCallLayer, BeforeToolCallService,
    BoxAgentService, BoxError, BoxToolService, PrepareArguments, Replay, ToolAnnotations,
    ToolCallDecision, ToolCallRequest, ToolExecutionMode, ToolExposure, ToolMetadata,
    ToolNamespace, ToolNotFound, ToolRegistration, ToolResultStream, ToolRoute, layer,
};
