//! 工具路由：按工具名把调用分发给对应的工具服务。

use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use tower::{Service, ServiceExt};

use crate::{
    AgentError, AgentToolResult, AgentToolService, AgentToolServiceExt, BoxToolService,
    ToolCallRequest, ToolResultStream,
};

/// 工具名 → 工具服务 的路由服务。
///
/// 对应 pi 的 `tools.find((t) => t.name === toolCall.name)`：命中则把
/// [`ToolCallRequest`] 交给该工具服务；未命中时产出 `isError: true` 的结果，
/// 而不是走错误通道——错误文本与 pi 一致：`Tool ${name} not found`。
///
/// 路由本身是 [`tower::Service`]，因此可以像单个工具一样被组合、装箱或再包一层
/// 中间件（`beforeToolCall` / `afterToolCall` 钩子层会落在它的外侧）。
#[derive(Clone, Default)]
pub struct ToolRouter {
    tools: BTreeMap<String, BoxToolService>,
}

impl ToolRouter {
    /// 构造一个空路由。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个具体工具服务，返回被替换掉的旧服务（如果有）。
    ///
    /// 工具服务会被装箱成 [`BoxToolService`]（design.md D5）。
    pub fn insert<S>(&mut self, name: impl Into<String>, tool: S) -> Option<BoxToolService>
    where
        S: AgentToolService + Clone + 'static,
        <S as Service<ToolCallRequest>>::Future: Send + 'static,
    {
        self.insert_boxed(name, tool.boxed_tool())
    }

    /// 注册一个已经装箱的工具服务，返回被替换掉的旧服务（如果有）。
    pub fn insert_boxed(
        &mut self,
        name: impl Into<String>,
        tool: BoxToolService,
    ) -> Option<BoxToolService> {
        self.tools.insert(name.into(), tool)
    }

    /// 按名取出工具服务。
    pub fn get(&self, name: &str) -> Option<&BoxToolService> {
        self.tools.get(name)
    }

    /// 按名移除工具服务。
    pub fn remove(&mut self, name: &str) -> Option<BoxToolService> {
        self.tools.remove(name)
    }

    /// 已注册的工具名（按字典序，便于测试与稳定输出）。
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    /// 已注册的工具数量。
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 是否没有注册任何工具。
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 未命中工具名时的结果（pi `Tool ${name} not found`）。
    fn not_found(&self, name: &str) -> ToolResultStream {
        ToolResultStream::once(AgentToolResult::error(format!("Tool {name} not found")))
    }
}

impl Service<ToolCallRequest> for ToolRouter {
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = ToolRouterFuture;

    // 路由不知道 `call` 会选中哪个工具，因此自身总是 ready；对被选中服务的就绪
    // 等待由 `ToolRouterFuture` 内的 `oneshot`（先 poll_ready 再 call）承担。
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        let name = request.tool_call.name.clone();
        match self.tools.get(&name).cloned() {
            Some(tool) => ToolRouterFuture::route(tool, request),
            None => ToolRouterFuture::ready(Ok(self.not_found(&name))),
        }
    }
}

/// [`ToolRouter`] 的响应 future。
///
/// 路由按名字动态分派到不同的工具服务，future 类型因此需要擦除；内部先对选中的
/// 工具服务 `poll_ready`，再调用它，与 tower 的 `Service` 契约一致。
pub struct ToolRouterFuture {
    inner: Pin<Box<dyn Future<Output = Result<ToolResultStream, AgentError>> + Send + 'static>>,
}

impl ToolRouterFuture {
    fn ready(result: Result<ToolResultStream, AgentError>) -> Self {
        Self {
            inner: Box::pin(std::future::ready(result)),
        }
    }

    fn route(tool: BoxToolService, request: ToolCallRequest) -> Self {
        Self {
            inner: Box::pin(async move { tool.oneshot(request).await }),
        }
    }
}

impl Future for ToolRouterFuture {
    type Output = Result<ToolResultStream, AgentError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

impl fmt::Debug for ToolRouterFuture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolRouterFuture").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::{Ready, ready},
        pin::pin,
        task::{Context, Poll},
    };

    use futures_core::Stream;
    use futures_util::{FutureExt as _, stream, task::noop_waker};
    use pi_ai::{ContentBlock, TextContent, ToolCall};
    use tower::Service;

    use super::ToolRouter;
    use crate::{
        AgentError, AgentToolResult, AgentToolService, AgentToolServiceExt, ToolCallRequest,
        ToolResultStream,
    };

    /// 把「工具名 + 部分结果」编码进响应流，便于断言分派是否命中正确的服务。
    #[derive(Clone)]
    struct EchoTool {
        label: &'static str,
    }

    impl Service<ToolCallRequest> for EchoTool {
        type Response = ToolResultStream;
        type Error = AgentError;
        type Future = Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: ToolCallRequest) -> Self::Future {
            let id = request.tool_call.id;
            let label = self.label;
            ready(Ok(ToolResultStream::new(stream::iter(vec![
                AgentToolResult::text(format!("{id}: partial from {label}")),
                AgentToolResult::text(format!("{id}: final from {label}")),
            ]))))
        }
    }

    /// 工具建立阶段失败：`call` 本身返回错误。
    #[derive(Clone)]
    struct FailingTool;

    impl Service<ToolCallRequest> for FailingTool {
        type Response = ToolResultStream;
        type Error = AgentError;
        type Future = Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: ToolCallRequest) -> Self::Future {
            ready(Err(AgentError::Tool("boom".into())))
        }
    }

    fn request(name: &str, id: &str) -> ToolCallRequest {
        ToolCallRequest::new(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: Default::default(),
            thought_signature: None,
            namespace: None,
        })
    }

    fn collect(stream: ToolResultStream) -> Vec<AgentToolResult> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut stream = pin!(stream);
        let mut items = Vec::new();
        loop {
            match stream.as_mut().poll_next(&mut cx) {
                Poll::Ready(Some(item)) => items.push(item),
                Poll::Ready(None) => break,
                Poll::Pending => panic!("scripted tool stream must not be pending"),
            }
        }
        items
    }

    fn call_router(router: &mut ToolRouter, request: ToolCallRequest) -> ToolResultStream {
        router
            .call(request)
            .now_or_never()
            .expect("tool router future is immediately ready for scripted tools")
            .expect("router call succeeds")
    }

    fn text_of(result: &AgentToolResult) -> Option<&str> {
        match result.content.first() {
            Some(ContentBlock::Text(TextContent { text, .. })) => Some(text),
            _ => None,
        }
    }

    #[test]
    fn names_are_sorted_and_deduped() {
        let mut router = ToolRouter::new();
        assert!(router.is_empty());
        router.insert("zeta", EchoTool { label: "zeta" });
        router.insert("alpha", EchoTool { label: "alpha" });
        router.insert("alpha", EchoTool { label: "alpha-2" });

        assert_eq!(router.len(), 2);
        assert_eq!(router.names().collect::<Vec<_>>(), vec!["alpha", "zeta"]);
    }

    #[test]
    fn routing_dispatches_to_the_named_tool() {
        let mut router = ToolRouter::new();
        router.insert("alpha", EchoTool { label: "alpha" });
        router.insert("beta", EchoTool { label: "beta" });

        let items = collect(call_router(&mut router, request("beta", "call-1")));

        assert_eq!(items.len(), 2, "partial update plus final result");
        assert_eq!(text_of(&items[0]), Some("call-1: partial from beta"));
        assert_eq!(text_of(&items[1]), Some("call-1: final from beta"));
    }

    #[test]
    fn unknown_tool_produces_an_error_result_not_a_call_error() {
        let mut router = ToolRouter::new();
        router.insert("alpha", EchoTool { label: "alpha" });

        let items = collect(call_router(&mut router, request("ghost", "call-9")));

        assert_eq!(items.len(), 1);
        assert!(items[0].is_error, "unknown tool must set is_error");
        assert_eq!(text_of(&items[0]), Some("Tool ghost not found"));
    }

    #[test]
    fn tool_setup_failure_propagates_through_the_error_channel() {
        let mut router = ToolRouter::new();
        router.insert("failing", FailingTool);

        let result = router
            .call(request("failing", "call-1"))
            .now_or_never()
            .expect("ready future");

        assert!(matches!(result, Err(AgentError::Tool(message)) if message == "boom"));
    }

    #[test]
    fn router_is_itself_a_tool_service() {
        fn assert_tool_service<S: AgentToolService>(_: &S) {}

        let mut router = ToolRouter::new();
        router.insert("alpha", EchoTool { label: "alpha" });
        assert_tool_service(&router);

        // 装箱 + clone 后仍可分发，验证路由能作为单个工具被再组合。
        let mut boxed = router.boxed_tool();
        let items = collect(
            boxed
                .call(request("alpha", "call-7"))
                .now_or_never()
                .expect("ready future")
                .expect("call succeeds"),
        );
        assert_eq!(text_of(&items[1]), Some("call-7: final from alpha"));
    }
}
