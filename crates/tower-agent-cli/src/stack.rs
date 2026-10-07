//! CLI 的 agent 服务装配（`docs/tower-agent-architecture.md` P4）。
//!
//! 用 `ServiceBuilder` 把 runtime 组合成一个类型擦除的 [`BoxAgentService`]：
//! agent 服务 → 并发限制（会话是单写者，串行执行 turn）→ `.boxed_agent()`。
//! 控制面（订阅事件、切模型/工具、取消、重置）走 [`BoxAgentControl`]，因此会话层
//! 只依赖 Service 契约与控制句柄，不直接依赖 `Agent` 具体类型。

use std::sync::Arc;

use tower::{ServiceBuilder, limit::ConcurrencyLimitLayer};
use tower_agent_runtime::{
    Agent, AgentOptions, AgentServiceExt as _, BoxAgentControl, BoxAgentService,
};

/// 装配 agent 栈，返回（服务、控制句柄）。
#[must_use]
pub fn assemble(options: AgentOptions) -> (BoxAgentService, BoxAgentControl) {
    let agent = Agent::new(options);
    let control: BoxAgentControl = Arc::new(agent.clone());
    let service = ServiceBuilder::new()
        .layer(ConcurrencyLimitLayer::new(1))
        .service(agent.boxed_agent())
        .boxed_agent();
    (service, control)
}
