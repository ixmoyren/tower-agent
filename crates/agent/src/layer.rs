//! 工具层：把 pi 的 `tool_call` / `tool_result` 钩子做成 tower [`Layer`]。
//!
//! 这两个层包装任意工具服务（`Service<ToolCallRequest, Response = ToolResultStream,
//! Error = AgentError>`），因此可以像普通 tower 中间件一样叠在单个工具或整个工具路由上，
//! 也可以和 tower 生态里的其它 layer（并发限制、超时、限流、buffer……）混用。
//!
//! 错误类型说明：工具服务的错误是 [`AgentError`]，像 [`tower::timeout`] /
//! [`tower::load_shed`] 这类把错误擦成 [`crate::BoxError`] 的 layer 之后，需要用
//! `ServiceBuilder::map_err(AgentError::other)`（或 `tower::ServiceExt::map_err`）转回来。

use std::task::{Context, Poll};

use futures_util::{StreamExt as _, future::BoxFuture};
use tower::{Layer, Service, ServiceExt as _};

use crate::{AgentError, AgentToolResult, ToolCallRequest, ToolResultStream};

/// 工具执行前钩子的判定结果。
#[derive(Debug, Clone)]
pub enum ToolCallDecision {
    /// 放行，并采用（可能被改写过的）请求。
    Allow(ToolCallRequest),
    /// 拦截，直接产出给定结果（pi `beforeToolCall` 的 `block` 语义）。
    Block(AgentToolResult),
}

impl ToolCallDecision {
    /// 原样放行。
    #[must_use]
    pub fn allow(request: ToolCallRequest) -> Self {
        Self::Allow(request)
    }

    /// 用一段文本拦截。
    #[must_use]
    pub fn block(text: impl Into<String>) -> Self {
        Self::Block(AgentToolResult::error(text))
    }
}

/// `tool_call`（执行前）钩子层：可放行/改写请求，也可直接拦截。
#[derive(Debug, Clone)]
pub struct BeforeToolCallLayer<F> {
    hook: F,
}

impl<F> BeforeToolCallLayer<F> {
    /// 用钩子构造。
    pub fn new(hook: F) -> Self {
        Self { hook }
    }
}

impl<S, F> Layer<S> for BeforeToolCallLayer<F>
where
    F: Clone,
{
    type Service = BeforeToolCallService<S, F>;

    fn layer(&self, inner: S) -> Self::Service {
        BeforeToolCallService {
            inner,
            hook: self.hook.clone(),
        }
    }
}

/// [`BeforeToolCallLayer`] 产出的服务。
#[derive(Debug, Clone)]
pub struct BeforeToolCallService<S, F> {
    inner: S,
    hook: F,
}

impl<S, F> Service<ToolCallRequest> for BeforeToolCallService<S, F>
where
    S: Service<ToolCallRequest, Response = ToolResultStream, Error = AgentError>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    F: Fn(ToolCallRequest) -> ToolCallDecision + Clone + Send + 'static,
{
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        match (self.hook)(request) {
            ToolCallDecision::Block(result) => {
                Box::pin(async move { Ok(ToolResultStream::once(result)) })
            }
            ToolCallDecision::Allow(request) => {
                let inner = self.inner.clone();
                Box::pin(async move { inner.oneshot(request).await })
            }
        }
    }
}

/// `tool_result`（执行后）钩子层：逐条改写工具结果（含 `onUpdate` 部分结果）。
#[derive(Debug, Clone)]
pub struct AfterToolResultLayer<F> {
    hook: F,
}

impl<F> AfterToolResultLayer<F> {
    /// 用钩子构造。
    pub fn new(hook: F) -> Self {
        Self { hook }
    }
}

impl<S, F> Layer<S> for AfterToolResultLayer<F>
where
    F: Clone,
{
    type Service = AfterToolResultService<S, F>;

    fn layer(&self, inner: S) -> Self::Service {
        AfterToolResultService {
            inner,
            hook: self.hook.clone(),
        }
    }
}

/// [`AfterToolResultLayer`] 产出的服务。
#[derive(Debug, Clone)]
pub struct AfterToolResultService<S, F> {
    inner: S,
    hook: F,
}

impl<S, F> Service<ToolCallRequest> for AfterToolResultService<S, F>
where
    S: Service<ToolCallRequest, Response = ToolResultStream, Error = AgentError>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    F: Fn(&ToolCallRequest, AgentToolResult) -> AgentToolResult + Clone + Send + 'static,
{
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        let hook = self.hook.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            let stream = inner.oneshot(request.clone()).await?;
            let mapped = stream.map(move |result| hook(&request, result));
            Ok(ToolResultStream::new(mapped))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ai::{Arguments, ToolCall};
    use tower::ServiceBuilder;

    use super::*;
    use crate::{AgentTool, AgentToolServiceExt as _, BoxToolService};

    /// 一个返回固定文本的内层工具服务。
    #[derive(Clone)]
    struct Echo;

    impl Service<ToolCallRequest> for Echo {
        type Response = ToolResultStream;
        type Error = AgentError;
        type Future = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: ToolCallRequest) -> Self::Future {
            Box::pin(async { Ok(ToolResultStream::once(AgentToolResult::text("echo"))) })
        }
    }

    fn request(name: &str) -> ToolCallRequest {
        ToolCallRequest::new(ToolCall {
            id: "call-1".into(),
            name: name.into(),
            arguments: Arguments::default(),
            thought_signature: None,
            namespace: None,
        })
    }

    async fn first_text(service: &BoxToolService, name: &str) -> String {
        use tower::ServiceExt as _;
        let mut stream = service
            .clone()
            .oneshot(request(name))
            .await
            .expect("call succeeds");
        let result = stream.next().await.expect("one result");
        result
            .content
            .iter()
            .filter_map(|block| match block {
                ai::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn before_layer_can_block_a_call() {
        let service = ServiceBuilder::new()
            .layer(BeforeToolCallLayer::new(|_request: ToolCallRequest| {
                ToolCallDecision::block("blocked by policy")
            }))
            .service(Echo)
            .boxed_tool();
        assert_eq!(first_text(&service, "echo").await, "blocked by policy");
    }

    #[tokio::test]
    async fn after_layer_rewrites_results() {
        let service = ServiceBuilder::new()
            .layer(AfterToolResultLayer::new(
                |_request: &ToolCallRequest, result: AgentToolResult| {
                    AgentToolResult::text(format!("wrapped: {}", result.is_error))
                },
            ))
            .service(Echo)
            .boxed_tool();
        assert_eq!(first_text(&service, "echo").await, "wrapped: false");
    }

    #[tokio::test]
    async fn tool_composes_with_tower_ecosystem_layers() {
        // `ConcurrencyLimitLayer` 保留错误类型，叠完仍是合法的工具服务。
        let limited = ServiceBuilder::new()
            .layer(tower::limit::ConcurrencyLimitLayer::new(2))
            .service(Echo);
        let tool = AgentTool::new("echo", limited);
        assert_eq!(tool.name(), "echo");

        // `TimeoutLayer` 把错误擦成 BoxError，用 `map_err` 转回 AgentError 后即可再次成为
        // 工具服务——这就是「tool 能组装 tower 生态任意 layer」的通用接法。
        let timed = ServiceBuilder::new()
            .map_err(AgentError::other)
            .layer(tower::timeout::TimeoutLayer::new(Duration::from_secs(5)))
            .service(Echo)
            .boxed_tool();
        assert_eq!(first_text(&timed, "echo").await, "echo");
    }
}
