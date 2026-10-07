//! `StreamFn → Service` 的 trait 边界。

use tower::{Service, util::BoxCloneService};

use crate::{AssistantEventStream, PiError, StreamRequest};

/// provider 流式调用服务：`Request = StreamRequest`、`Response = AssistantEventStream`。
///
/// 这是 pi `StreamFunction` 的 Rust 对应物。`Service` 只覆盖 `call` 返回的那个
/// future；响应流本身的超时、取消与背压由 stream-aware 层或编排层处理。
pub trait StreamService:
Service<StreamRequest, Response = AssistantEventStream, Error = PiError> + Send + 'static
{}

impl<T> StreamService for T
where
    T: Service<StreamRequest, Response = AssistantEventStream, Error = PiError> + Send + 'static,
{}

/// 类型擦除后的 provider 流服务。
///
/// `BoxCloneService` 让运行时集合得以存放不同的 provider 实现；
/// 其 `Future` 需为 `Send`，这也是 [`StreamServiceExt::boxed_stream`] 的约束。
pub type BoxStreamService = BoxCloneService<StreamRequest, AssistantEventStream, PiError>;

/// pi `StreamFunction` 在 Rust 侧的类型擦除形态。
pub type StreamFn = BoxStreamService;

/// 便捷地把具体服务装箱成 [`BoxStreamService`]。
pub trait StreamServiceExt: StreamService + Clone + Sized {
    /// 装箱为类型擦除的流服务（避免与 tower `ServiceExt::boxed*` 重名）。
    fn boxed_stream(self) -> BoxStreamService
    where
        <Self as Service<StreamRequest>>::Future: Send + 'static,
    {
        BoxCloneService::new(self)
    }
}

impl<T> StreamServiceExt for T where T: StreamService + Clone + Sized {}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        future::Ready,
        pin::pin,
        task::{Context, Poll},
    };

    use futures_core::Stream;
    use futures_util::{FutureExt as _, task::noop_waker};
    use tower::Service;

    use super::{BoxStreamService, StreamService, StreamServiceExt};
    use crate::{
        AssistantEventStream, AssistantMessage, AssistantMessageEvent, ContentBlock, DoneReason,
        InputModality, Message, Model, PiError, SimpleStreamOptions, StopReason, StreamRequest,
        TextContent, TranscriptContext, Usage, UserContent, UserMessage,
    };

    #[derive(Clone)]
    struct MockStreamService;

    impl Service<StreamRequest> for MockStreamService {
        type Response = AssistantEventStream;
        type Error = PiError;
        type Future = Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: StreamRequest) -> Self::Future {
            let partial = sample_message();
            let events = vec![
                AssistantMessageEvent::Start {
                    partial: partial.clone(),
                },
                AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: partial,
                },
            ];
            std::future::ready(Ok(AssistantEventStream::new(futures_util::stream::iter(
                events,
            ))))
        }
    }

    fn sample_message() -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text(TextContent {
                text: "hi".into(),
                text_signature: None,
            })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }
    }

    fn sample_request() -> StreamRequest {
        StreamRequest {
            model: Model {
                id: "test".into(),
                name: "Test".into(),
                api: "test".into(),
                provider: "test".into(),
                input: vec![InputModality::Text],
                context_window: 8192,
                max_tokens: 1024,
                ..Model::default()
            },
            context: TranscriptContext::new(vec![Message::User(UserMessage {
                content: UserContent::Text("hi".into()),
                timestamp: 0,
            })]),
            options: SimpleStreamOptions::default(),
        }
    }

    fn collect_events(stream: AssistantEventStream) -> Vec<AssistantMessageEvent> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut stream = pin!(stream);
        let mut events = Vec::new();
        loop {
            match stream.as_mut().poll_next(&mut cx) {
                Poll::Ready(Some(event)) => events.push(event),
                Poll::Ready(None) => break,
                Poll::Pending => panic!("mock stream must not be pending"),
            }
        }
        events
    }

    #[test]
    fn mock_service_is_a_stream_service() {
        fn assert_stream_service<S: StreamService>(_: &S) {}
        assert_stream_service(&MockStreamService);
    }

    #[test]
    fn boxed_service_emits_events_ending_in_terminal_message() {
        let mut service: BoxStreamService = MockStreamService.boxed_stream();
        let stream = service
            .call(sample_request())
            .now_or_never()
            .expect("mock call is immediately ready")
            .expect("call succeeds");
        let events = collect_events(stream);

        assert!(matches!(
            events.first(),
            Some(AssistantMessageEvent::Start { .. })
        ));
        assert!(
            events
                .last()
                .is_some_and(AssistantMessageEvent::is_terminal)
        );
        assert_eq!(
            events
                .last()
                .and_then(AssistantMessageEvent::terminal_message)
                .map(|message| message.stop_reason),
            Some(StopReason::Stop)
        );
        assert_eq!(events.last().map(AssistantMessageEvent::kind), Some("done"));
    }

    #[test]
    fn default_model_is_empty() {
        assert_eq!(Model::default().headers, BTreeMap::new());
    }
}
