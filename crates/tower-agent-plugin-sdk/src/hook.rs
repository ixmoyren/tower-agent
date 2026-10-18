//! Async tool hooks built on the runtime `tool_call` / `tool_result` layers.

use std::future::Future;

use tower_agent_plugins::{ToolLayer, tower_tool_layer};
use tower_agent_runtime::{
    AfterToolResultLayer, AgentError, AgentToolResult, BeforeToolCallLayer, ToolCallDecision,
    ToolCallRequest,
};

/// Marker for an execution-before (`tool_call`) hook.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolCall;

/// Marker for an execution-after (`tool_result`) hook.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolResult;

/// Registration contract for [`on`].
pub trait HookEvent<F, Fut> {
    /// Convert an async handler into a runtime-compatible tool layer.
    fn layer(handler: F) -> Box<dyn ToolLayer>;
}

impl<F, Fut> HookEvent<F, Fut> for ToolCall
where
    F: Fn(ToolCallRequest) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<ToolCallDecision, AgentError>> + Send + 'static,
{
    fn layer(handler: F) -> Box<dyn ToolLayer> {
        tower_tool_layer(BeforeToolCallLayer::new(handler))
    }
}

impl<F, Fut> HookEvent<F, Fut> for ToolResult
where
    F: Fn(&ToolCallRequest, AgentToolResult) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<AgentToolResult, AgentError>> + Send + 'static,
{
    fn layer(handler: F) -> Box<dyn ToolLayer> {
        tower_tool_layer(AfterToolResultLayer::new(handler))
    }
}

/// Register an async hook for a runtime tool event.
///
/// ```ignore
/// let layer = on(ToolCall, |request| async move {
///     Ok(ToolCallDecision::Allow(request))
/// });
/// context.register_tool_layer(layer);
/// ```
pub fn on<E, F, Fut>(_event: E, handler: F) -> Box<dyn ToolLayer>
where
    E: HookEvent<F, Fut>,
{
    E::layer(handler)
}

/// Convenience alias for [`on`] with [`ToolCall`].
pub fn on_tool_call<F, Fut>(handler: F) -> Box<dyn ToolLayer>
where
    ToolCall: HookEvent<F, Fut>,
{
    on(ToolCall, handler)
}

/// Convenience alias for [`on`] with [`ToolResult`].
pub fn on_tool_result<F, Fut>(handler: F) -> Box<dyn ToolLayer>
where
    ToolResult: HookEvent<F, Fut>,
{
    on(ToolResult, handler)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context, Poll},
    };

    use futures_util::{StreamExt as _, future::BoxFuture};
    use tower::{Service, ServiceExt as _};
    use tower_agent_provider::{Arguments, ContentBlock, ToolCall as ProviderToolCall};
    use tower_agent_runtime::{AgentToolServiceExt as _, BoxToolService, ToolResultStream};

    use super::*;

    #[derive(Clone)]
    struct Echo {
        called: Arc<AtomicBool>,
    }

    impl Service<ToolCallRequest> for Echo {
        type Response = ToolResultStream;
        type Error = AgentError;
        type Future = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: ToolCallRequest) -> Self::Future {
            self.called.store(true, Ordering::SeqCst);
            Box::pin(async { Ok(ToolResultStream::once(AgentToolResult::text("echo"))) })
        }
    }

    #[derive(Clone)]
    struct ArgumentsEcho;

    impl Service<ToolCallRequest> for ArgumentsEcho {
        type Response = ToolResultStream;
        type Error = AgentError;
        type Future = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: ToolCallRequest) -> Self::Future {
            Box::pin(async move {
                let value = request
                    .tool_call
                    .arguments
                    .arg_str("value")
                    .unwrap_or_default();
                Ok(ToolResultStream::once(AgentToolResult::text(value)))
            })
        }
    }

    fn request() -> ToolCallRequest {
        ToolCallRequest::new(ProviderToolCall {
            id: "hook-call".to_owned(),
            name: "echo".to_owned(),
            arguments: Arguments::default(),
            thought_signature: None,
            namespace: None,
        })
    }

    async fn first_text(service: &BoxToolService) -> AgentToolResult {
        let mut stream = service
            .clone()
            .oneshot(request())
            .await
            .expect("service call");
        stream.next().await.expect("one result")
    }

    fn text(result: &AgentToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn async_tool_call_hook_can_block_a_call() {
        let called = Arc::new(AtomicBool::new(false));
        let layer = on(ToolCall, |_request: ToolCallRequest| async {
            Ok(ToolCallDecision::block("blocked by async hook"))
        });
        let service = layer.apply(
            Echo {
                called: Arc::clone(&called),
            }
            .boxed_tool(),
        );

        let result = first_text(&service).await;
        assert!(result.is_error);
        assert_eq!(text(&result), "blocked by async hook");
        assert!(
            !called.load(Ordering::SeqCst),
            "blocked tool must not reach the inner service"
        );
    }

    #[tokio::test]
    async fn async_tool_result_hook_can_rewrite_results() {
        let layer = on(
            ToolResult,
            |_request: &ToolCallRequest, result: AgentToolResult| async move {
                Ok(AgentToolResult::text(format!("wrapped:{}", text(&result))))
            },
        );
        let service = layer.apply(
            Echo {
                called: Arc::new(AtomicBool::new(false)),
            }
            .boxed_tool(),
        );

        let result = first_text(&service).await;
        assert!(!result.is_error);
        assert_eq!(text(&result), "wrapped:echo");
    }

    #[tokio::test]
    async fn async_tool_call_hook_can_rewrite_arguments() {
        let layer = on(ToolCall, |mut request: ToolCallRequest| async move {
            request.tool_call.arguments = serde_json::json!({ "value": "rewritten" })
                .as_object()
                .cloned()
                .unwrap_or_default()
                .into();
            Ok(ToolCallDecision::Allow(request))
        });
        let service = layer.apply(ArgumentsEcho.boxed_tool());

        let result = first_text(&service).await;
        assert!(!result.is_error);
        assert_eq!(text(&result), "rewritten");
    }

    #[tokio::test]
    async fn hook_failures_become_tool_errors() {
        let before = on(ToolCall, |_request: ToolCallRequest| async {
            Err(AgentError::Tool("before failed".to_owned()))
        });
        let before_service = before.apply(
            Echo {
                called: Arc::new(AtomicBool::new(false)),
            }
            .boxed_tool(),
        );
        let before_result = first_text(&before_service).await;
        assert!(before_result.is_error);
        assert!(text(&before_result).contains("tool_call hook failed"));

        let after = on(
            ToolResult,
            |_request: &ToolCallRequest, _result: AgentToolResult| async {
                Err(AgentError::Tool("after failed".to_owned()))
            },
        );
        let after_service = after.apply(
            Echo {
                called: Arc::new(AtomicBool::new(false)),
            }
            .boxed_tool(),
        );
        let after_result = first_text(&after_service).await;
        assert!(after_result.is_error);
        assert!(text(&after_result).contains("tool_result hook failed"));
    }
}
