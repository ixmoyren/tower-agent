//! `tower-agent` — 面向使用者的门面 crate。
//!
//! 按层再导出，方便 `use tower_agent::{core, runtime, provider, tools, plugins};`：
//!
//! - [`core`]：契约与类型（`AgentRequest` / `AgentResponse` / `AgentEventStream`、消息/事件、
//!   工具边界、钩子层、typed 工具注册）；
//! - [`runtime`]：有状态运行时（`Agent`、编排循环、事件总线）；
//! - [`provider`]：provider Services 与 Models 路由；
//! - [`tools`]：内置工具；
//! - [`plugins`]：插件宿主（静态 / 动态 / 热重载）；`plugin-sdk` feature 打开作者 SDK。
//!
//! tower-native：`runtime::Agent` 实现 `core::AgentServiceExt`/`Service<AgentRequest>`，
//! 可 `.layer()` 组合或 `.boxed_agent()` 后交给 axum/tonic 等宿主。

pub use tower_agent_core as core;
pub use tower_agent_plugins as plugins;
pub use tower_agent_provider as provider;
pub use tower_agent_runtime as runtime;
pub use tower_agent_tools as tools;

pub mod assembly;

#[cfg(feature = "codemode-js")]
pub use tower_agent_codemode as codemode;
#[cfg(feature = "codemode-python")]
pub use tower_agent_monty as monty;
#[cfg(feature = "plugin-sdk")]
pub use tower_agent_plugin_sdk as plugin_sdk;
#[cfg(feature = "protocol")]
pub use tower_agent_protocol as protocol;
#[cfg(feature = "tui")]
pub use tower_agent_tui as tui;
#[cfg(feature = "storage-turso")]
pub use tower_agent_turso as turso;

pub use tower_agent_core::{
    AfterToolResultLayer, AgentError, AgentEvent, AgentEventStream, AgentMessage, AgentRequest,
    AgentResponse, AgentServiceExt, AgentTool, AgentToolResult, BeforeToolCallLayer,
    BoxAgentService, BoxError, BoxToolService, SessionStore, ToolCallDecision, ToolCallRequest,
    ToolRegistration, ToolResultStream,
};
pub use tower_agent_runtime::{
    Agent, AgentOptions, AgentPool, AgentService, AgentState, AgentStreamService, EventBus,
    MemorySessionStore, QueueMode,
};
