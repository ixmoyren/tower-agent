//! 插件可贡献的中间件层（对象安全）与事件处理器（`docs/plugins.md` §4.1 / §4.5）。
//!
//! `tower::Layer` 不是对象安全的（`layer` 是泛型方法），所以插件与宿主之间用两个
//! 对象安全的「已装箱层」trait 交流：[`AgentLayer`] 把一个 [`BoxAgentService`] 再包一层，
//! [`ToolLayer`] 把一个 [`BoxToolService`] 再包一层。想直接用现成的 tower `Layer`，用
//! [`tower_agent_layer`] / [`tower_tool_layer`] 适配即可。

use std::sync::Arc;

use tower::{Layer, Service};
use tower_agent_runtime::{
    AgentError, AgentEvent, AgentRequest, AgentResponse, AgentServiceExt, AgentToolServiceExt,
    BoxAgentService, BoxToolService, ToolCallRequest, ToolResultStream,
};

/// agent 级中间件层：把一个 [`BoxAgentService`] 再包一层。
pub trait AgentLayer: Send + Sync + 'static {
    /// 叠加到内层服务上。
    fn apply(&self, inner: BoxAgentService) -> BoxAgentService;
}

/// 工具级中间件层：把一个 [`BoxToolService`] 再包一层。
pub trait ToolLayer: Send + Sync + 'static {
    /// 叠加到内层服务上。
    fn apply(&self, inner: BoxToolService) -> BoxToolService;
}

/// 纯通知事件处理器（不改请求/响应，不进 tower 中间件）。
pub trait EventHandler: Send + Sync + 'static {
    /// 处理一个事件。
    fn handle(&self, event: &AgentEvent);
}

/// 用闭包构造 [`AgentLayer`]。
pub struct AgentLayerFn<F>(F);
/// 用闭包构造 [`ToolLayer`]。
pub struct ToolLayerFn<F>(F);
/// 用闭包构造 [`EventHandler`]。
pub struct EventHandlerFn<F>(F);

/// 把一个闭包包成 [`AgentLayer`]。
pub fn agent_layer<F>(f: F) -> Box<dyn AgentLayer>
where
    F: Fn(BoxAgentService) -> BoxAgentService + Send + Sync + 'static,
{
    Box::new(AgentLayerFn(f))
}

/// 把一个闭包包成 [`ToolLayer`]。
pub fn tool_layer<F>(f: F) -> Box<dyn ToolLayer>
where
    F: Fn(BoxToolService) -> BoxToolService + Send + Sync + 'static,
{
    Box::new(ToolLayerFn(f))
}

/// 把一个闭包包成 [`EventHandler`]。
pub fn event_handler<F>(f: F) -> Box<dyn EventHandler>
where
    F: Fn(&AgentEvent) + Send + Sync + 'static,
{
    Box::new(EventHandlerFn(f))
}

impl<F> AgentLayer for AgentLayerFn<F>
where
    F: Fn(BoxAgentService) -> BoxAgentService + Send + Sync + 'static,
{
    fn apply(&self, inner: BoxAgentService) -> BoxAgentService {
        (self.0)(inner)
    }
}

impl<F> ToolLayer for ToolLayerFn<F>
where
    F: Fn(BoxToolService) -> BoxToolService + Send + Sync + 'static,
{
    fn apply(&self, inner: BoxToolService) -> BoxToolService {
        (self.0)(inner)
    }
}

impl<F> EventHandler for EventHandlerFn<F>
where
    F: Fn(&AgentEvent) + Send + Sync + 'static,
{
    fn handle(&self, event: &AgentEvent) {
        (self.0)(event);
    }
}

/// 把任意 tower `Layer` 适配成 [`AgentLayer`]。
pub fn tower_agent_layer<L>(layer: L) -> Box<dyn AgentLayer>
where
    L: Layer<BoxAgentService> + Clone + Send + Sync + 'static,
    L::Service: Service<AgentRequest, Response = AgentResponse, Error = AgentError>
        + Clone
        + Send
        + 'static,
    <L::Service as Service<AgentRequest>>::Future: Send + 'static,
{
    let layer = Arc::new(layer);
    agent_layer(move |inner| (*layer).layer(inner).boxed_agent())
}

/// 把任意 tower `Layer` 适配成 [`ToolLayer`]。
pub fn tower_tool_layer<L>(layer: L) -> Box<dyn ToolLayer>
where
    L: Layer<BoxToolService> + Clone + Send + Sync + 'static,
    L::Service: Service<ToolCallRequest, Response = ToolResultStream, Error = AgentError>
        + Clone
        + Send
        + 'static,
    <L::Service as Service<ToolCallRequest>>::Future: Send + 'static,
{
    let layer = Arc::new(layer);
    tool_layer(move |inner| (*layer).layer(inner).boxed_tool())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use futures_util::StreamExt as _;
    use tower::ServiceExt as _;
    use tower_agent_provider::{Arguments, ContentBlock, ToolCall};
    use tower_agent_runtime::{AfterToolResultLayer, AgentToolResult, ToolCallRequest};

    use super::*;

    #[test]
    fn event_handlers_close_over_state() {
        let seen = Arc::new(AtomicUsize::new(0));
        let handler = {
            let seen = Arc::clone(&seen);
            event_handler(move |_event: &AgentEvent| {
                seen.fetch_add(1, Ordering::SeqCst);
            })
        };
        handler.handle(&AgentEvent::AgentStart);
        handler.handle(&AgentEvent::TurnStart);
        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn tower_tool_layer_applies_to_a_tool_service() {
        let cwd = std::env::temp_dir();
        let inner = tower_agent_tools::create_ls_tool(&cwd).service().clone();
        let layer = tower_tool_layer(AfterToolResultLayer::new(
            |_request: &ToolCallRequest, result: AgentToolResult| {
                AgentToolResult::text(format!("wrapped:{}", result.is_error))
            },
        ));
        let service = layer.apply(inner);

        let request = ToolCallRequest::new(ToolCall {
            id: "1".into(),
            name: "ls".into(),
            arguments: Arguments::default(),
            thought_signature: None,
            namespace: None,
        });
        let mut stream = service.oneshot(request).await.expect("call succeeds");
        let result = stream.next().await.expect("one result");
        assert!(matches!(
            result.content.first(),
            Some(ContentBlock::Text(text)) if text.text.starts_with("wrapped:")
        ));
    }
}
