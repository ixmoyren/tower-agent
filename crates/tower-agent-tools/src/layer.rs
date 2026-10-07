//! 工具层：把每个内置工具表达成一个 tower [`Layer`]。
//!
//! 每个工具 `XTool` 除实现 `Service<ToolCallRequest>`（叶服务）外，还实现
//! `Layer<S>`：层叠到内层按名分派服务 `S` 上后，请求的 `tool_call.name` 命中
//! 本工具就交给工具执行，否则原样委托内层。
//!
//! `tower_agent_tools::create_*_tool` 正是把工具层叠到 [`ToolNotFound`] 基础服务上，产出发给
//! agent 的可执行服务——于是每个内置工具都是可组合的 tower 单元。

use std::task::{Context, Poll};

use tower::{Layer, Service, ServiceExt};
use tower_agent_runtime::{AgentError, AgentToolResult, ToolCallRequest, ToolResultStream};

use super::{
    EditTool, FindTool, GrepTool, LsTool, ReadTool, ShellTool, WriteTool, support::ToolFuture,
};

/// 「工具未找到」基础服务：内层没有任何工具命中时，按 pi 的文案返回错误结果。
///
/// 与 `tower_agent_runtime::ToolRouter` 的未命中行为一致（`Tool ${name} not found`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolNotFound;

impl Service<ToolCallRequest> for ToolNotFound {
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = ToolFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        let name = request.tool_call.name;
        Box::pin(async move {
            Ok(ToolResultStream::once(AgentToolResult::error(format!(
                "Tool {name} not found"
            ))))
        })
    }
}

/// 工具层产出的按名分派服务。
///
/// `tool_call.name` 命中 `name` 时调用 `tool`，否则委托 `inner`。
#[derive(Debug, Clone)]
pub struct ToolRoute<S, T> {
    inner: S,
    tool: T,
    name: &'static str,
}

impl<S, T> ToolRoute<S, T> {
    /// 用内层服务、工具服务与工具名构造。
    pub fn new(inner: S, tool: T, name: &'static str) -> Self {
        Self { inner, tool, name }
    }
}

impl<S, T> Service<ToolCallRequest> for ToolRoute<S, T>
where
    S: Service<ToolCallRequest, Response = ToolResultStream, Error = AgentError>
    + Clone
    + Send
    + 'static,
    S::Future: Send + 'static,
    T: Service<ToolCallRequest, Response = ToolResultStream, Error = AgentError>
    + Clone
    + Send
    + 'static,
    T::Future: Send + 'static,
{
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = ToolFuture;

    // 具体分派到哪个服务要到 `call` 才知道，因此这里总是 ready；对被选中服务的
    // 就绪等待由 `call` 内的 `oneshot`（先 poll_ready 再 call）承担。
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        if request.tool_call.name == self.name {
            let tool = self.tool.clone();
            Box::pin(async move { tool.oneshot(request).await })
        } else {
            let inner = self.inner.clone();
            Box::pin(async move { inner.oneshot(request).await })
        }
    }
}

/// 为内置工具实现 `Layer<S>`（层叠到按名分派服务上）。
macro_rules! impl_tool_layer {
    ($tool:ty) => {
        impl<S> Layer<S> for $tool
        where
            S: Service<ToolCallRequest, Response = ToolResultStream, Error = AgentError>
                + Clone
                + Send
                + 'static,
            S::Future: Send + 'static,
        {
            type Service = ToolRoute<S, $tool>;

            fn layer(&self, inner: S) -> Self::Service {
                ToolRoute::new(inner, self.clone(), <$tool>::NAME)
            }
        }
    };
}

impl_tool_layer!(ReadTool);
impl_tool_layer!(ShellTool);
impl_tool_layer!(EditTool);
impl_tool_layer!(WriteTool);
impl_tool_layer!(GrepTool);
impl_tool_layer!(FindTool);
impl_tool_layer!(LsTool);
