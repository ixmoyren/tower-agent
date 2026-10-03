//! `TimeoutService` 的行为（design.md 4.3 的 `timeoutMs`）。
//!
//! 超时只覆盖「建立请求」阶段：`call` 返回的 future 若迟迟不结算（连接 / 首个响应头）
//! 就映射成 [`PiError::Timeout`]；一旦请求建立成功，响应流的消费不受约束。

use std::time::Duration;

use ai::{
    AssistantEventStream, Model, PiError, SimpleStreamOptions, StreamRequest, TimeoutService,
    TranscriptContext,
};
use futures_util::{future::BoxFuture, stream};
use tower::Service;

fn request() -> StreamRequest {
    StreamRequest {
        model: Model::default(),
        context: TranscriptContext::new(Vec::new()),
        options: SimpleStreamOptions::default(),
    }
}

/// 永远不结算的「建立请求」：用来触发超时。
#[derive(Clone)]
struct HangingProvider;

impl Service<StreamRequest> for HangingProvider {
    type Response = AssistantEventStream;
    type Error = PiError;
    type Future = BoxFuture<'static, Result<AssistantEventStream, PiError>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: StreamRequest) -> Self::Future {
        Box::pin(async { std::future::pending::<Result<AssistantEventStream, PiError>>().await })
    }
}

/// 立刻建立成功的 provider：用来确认超时层是透明的。
#[derive(Clone)]
struct ImmediateProvider;

impl Service<StreamRequest> for ImmediateProvider {
    type Response = AssistantEventStream;
    type Error = PiError;
    type Future = BoxFuture<'static, Result<AssistantEventStream, PiError>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: StreamRequest) -> Self::Future {
        Box::pin(async { Ok(AssistantEventStream::new(stream::empty())) })
    }
}

#[tokio::test]
async fn a_hanging_establish_is_reported_as_timeout() {
    let mut service = TimeoutService::new(HangingProvider, Duration::from_millis(30));
    let error = service.call(request()).await.expect_err("应当超时");
    assert!(matches!(error, PiError::Timeout));
}

#[tokio::test]
async fn a_fast_establish_passes_the_stream_through() {
    let mut service = TimeoutService::new(ImmediateProvider, Duration::from_secs(30));
    assert!(service.call(request()).await.is_ok());
    assert_eq!(service.duration(), Duration::from_secs(30));
}
