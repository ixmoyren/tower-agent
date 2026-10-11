//! agent 的对外 Service 契约。

use std::pin::Pin;

use futures_core::Stream;
use futures_util::future::BoxFuture;
use tower::{Service, util::BoxCloneService};
use tower_agent_provider::{Model, StopReason, Usage};

use crate::{AgentError, AgentEvent, AgentMessage, AgentTool};

/// 通过 tower [`Service`] 发起一次 agent run 的请求（对应 pi `Agent.prompt`）。
///
/// `messages` 是这次请求要处理的输入；有状态运行时把它当作一次新的 prompt 追加到
/// 会话，无状态运行时把它当作完整上下文。`model` / `tools` / `system_prompt` 非空时
/// 覆盖运行时配置。
#[derive(Debug, Clone, Default)]
pub struct AgentRequest {
    /// 输入消息（有状态运行时视为追加；无状态运行时视为完整上下文）。
    pub messages: Vec<AgentMessage>,
    /// 可选会话 id：配置了 [`SessionStore`] 时按 id 载入/保存 transcript，
    /// 此时 `messages` 只表示本轮新增输入。
    pub session_id: Option<String>,
    /// 可选的模型覆盖。
    pub model: Option<Model>,
    /// 可选的工具集覆盖。
    pub tools: Option<Vec<AgentTool>>,
    /// 可选的 system prompt 覆盖。
    pub system_prompt: Option<String>,
}

/// 一次 agent run 的结果。
#[derive(Debug, Clone)]
pub struct AgentResponse {
    /// 本次 run 新增的消息（assistant / toolResult）。
    pub messages: Vec<AgentMessage>,
    /// 最后一条 assistant 消息的纯文本。
    pub text: String,
    /// 本次 run 的 usage 合计。
    pub usage: Usage,
    /// 最后一条 assistant 消息的 stop reason（没有 assistant 消息时为 `None`）。
    pub stop_reason: Option<StopReason>,
}

/// agent 事件流（面向 SSE / TUI 的流式响应形态）。
///
/// 非流式 [`AgentResponse`] 等价于对它做一次 fold；两者共享同一套 [`AgentEvent`] 协议。
pub type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send + 'static>>;

/// 会话存储：按 session id 存取 transcript（无状态 Service 的可选服务端会话）。
///
/// 这是「无状态契约 + 可选会话」的存储接缝：不配置时请求用 `messages` 携带完整上下文；
/// 配置后可用 `session_id` 让服务端代为存取，调用方不必每轮重发整段 transcript。
pub trait SessionStore: Send + Sync + 'static {
    /// 载入某会话的 transcript（不存在时返回 `None`）。
    fn load(&self, session: &str) -> BoxFuture<'static, Option<Vec<AgentMessage>>>;

    /// 保存某会话的 transcript。
    fn save(&self, session: String, messages: Vec<AgentMessage>) -> BoxFuture<'static, ()>;
}

/// 类型擦除后的 agent 服务，便于像 tower-mcp 那样叠中间件或用 axum 暴露。
pub type BoxAgentService = BoxCloneService<AgentRequest, AgentResponse, AgentError>;

/// 把具体 agent 服务装箱成 [`BoxAgentService`]。
pub trait AgentServiceExt:
Service<AgentRequest, Response = AgentResponse, Error = AgentError> + Clone + Sized + Send + 'static
{
    /// 装箱为类型擦除的 agent 服务。
    fn boxed_agent(self) -> BoxAgentService
    where
        <Self as Service<AgentRequest>>::Future: Send + 'static,
    {
        BoxCloneService::new(self)
    }
}

impl<T> AgentServiceExt for T
where
    T: Service<AgentRequest, Response = AgentResponse, Error = AgentError>
    + Clone
    + Sized
    + Send
    + 'static
{}
