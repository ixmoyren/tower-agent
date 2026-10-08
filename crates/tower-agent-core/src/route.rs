//! 工具层：把单个工具表达成 tower [`Layer`](tower::Layer)，并做**按名分派**。
//!
//! 每个工具 `XTool` 除实现 `Service<ToolCallRequest>`（叶服务）外，还可以实现
//! `Layer<S>`：层叠到内层按名分派服务上后，请求的 `tool_call.name` 命中本工具就执行
//! 工具，否则原样委托内层。`create_*_tool` 正是把工具层叠到 [`ToolNotFound`] 上得到
//! 可执行服务——于是每个工具都是可组合的 tower 单元。
//!
//! 这套机制是**工具契约**的一部分，因此放在 core：内置工具（编译期名字）和 monty
//! `ModTool`（运行时名字）都能走同一条路径。

use std::task::{Context, Poll};

use futures_util::future::BoxFuture;
use tower::{Service, ServiceExt};

use crate::{AgentError, AgentToolResult, ToolCallRequest, ToolResultStream};

/// 「工具未找到」基础服务：内层没有任何工具命中时，按 pi 的文案返回错误结果。
///
/// 与 `ToolRouter` 的未命中行为一致（`Tool ${name} not found`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolNotFound;

impl Service<ToolCallRequest> for ToolNotFound {
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

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
/// `tool_call.name` 命中 `name` 时调用 `tool`，否则委托 `inner`。`name` 是运行时的
/// `String`，因此内置工具（`&'static str`）和 mod 工具（动态名）都能用。
#[derive(Debug, Clone)]
pub struct ToolRoute<S, T> {
    inner: S,
    tool: T,
    name: String,
}

impl<S, T> ToolRoute<S, T> {
    /// 用内层服务、工具服务与工具名构造。
    pub fn new(inner: S, tool: T, name: impl Into<String>) -> Self {
        Self {
            inner,
            tool,
            name: name.into(),
        }
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
    type Future = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

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
