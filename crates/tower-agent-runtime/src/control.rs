//! 有状态会话的控制句柄（`docs/tower-agent-architecture.md` P4）。
//!
//! CLI/TUI 这类宿主需要的不是 `Agent` 的内部结构，而是「跑一轮」的服务（`BoxAgentService`）
//! 加上一组控制操作（订阅事件、切模型/工具、取消、重置）。这里把控制面抽成对象安全的
//! [`AgentControl`]，宿主只持 `BoxAgentService` + `BoxAgentControl`，不再直接持 `Agent`。

use std::sync::Arc;

use futures_util::future::BoxFuture;
use tower_agent_core::{AgentError, AgentMessage, AgentTool};
use tower_agent_provider::{BoxStreamService, Model, ModelThinkingLevel};

use crate::{Agent, AgentState, Subscription};

/// 有状态会话的控制句柄。
pub trait AgentControl: Send + Sync + 'static {
    /// 订阅会话事件流。
    fn subscribe(&self) -> Subscription;

    /// 当前状态快照。
    fn state(&self) -> AgentState;

    /// 是否正在处理（流式/工具执行中）。
    fn is_streaming(&self) -> bool;

    /// 在当前 turn 结束后注入一条 steer 消息。
    fn steer(&self, message: AgentMessage);

    /// 取消进行中的一轮。
    fn abort(&self);

    /// 替换模型。
    fn set_model(&self, model: Model);

    /// 替换思考级别。
    fn set_thinking_level(&self, level: ModelThinkingLevel);

    /// 替换可执行工具集。
    fn set_tools(&self, tools: Vec<AgentTool>);

    /// 替换 provider 流服务（`/login` 等运行时切换）。
    fn set_stream(&self, stream_fn: BoxStreamService);

    /// 重置会话（清空 transcript 与队列，保留 system prompt 与工具）。
    fn reset(&self) -> Result<(), AgentError>;

    /// 等待当前 run 与所有被 await 的 listener 完成。
    fn wait_for_idle(&self) -> BoxFuture<'static, ()>;
}

impl AgentControl for Agent {
    fn subscribe(&self) -> Subscription {
        Agent::subscribe(self)
    }

    fn state(&self) -> AgentState {
        Agent::state(self)
    }

    fn is_streaming(&self) -> bool {
        Agent::is_streaming(self)
    }

    fn steer(&self, message: AgentMessage) {
        Agent::steer(self, message);
    }

    fn abort(&self) {
        Agent::abort(self);
    }

    fn set_model(&self, model: Model) {
        Agent::set_model(self, model);
    }

    fn set_thinking_level(&self, level: ModelThinkingLevel) {
        Agent::set_thinking_level(self, level);
    }

    fn set_tools(&self, tools: Vec<AgentTool>) {
        Agent::set_tools(self, tools);
    }

    fn set_stream(&self, stream_fn: BoxStreamService) {
        Agent::set_stream(self, stream_fn);
    }

    fn reset(&self) -> Result<(), AgentError> {
        Agent::reset(self)
    }

    fn wait_for_idle(&self) -> BoxFuture<'static, ()> {
        let agent = self.clone();
        Box::pin(async move { agent.wait_for_idle().await })
    }
}

/// 类型擦除的控制句柄。
pub type BoxAgentControl = Arc<dyn AgentControl>;
