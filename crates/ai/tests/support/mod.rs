//! `ai` 一致性（conformance）测试的测试辅助。
//!
//! 对齐 pi faux provider（`packages/ai/src/providers/faux.ts`）的确定性部分：
//! 脚本化的 assistant 消息按 `start → *_start/*_delta/*_end → done` 回放，
//! setup 失败则回放为单个不带 `start` 的 `error`。pi 的 delta 大小是随机的，
//! conformance 固定它，使同一份脚本产出唯一确定的序列。

use std::{
    collections::VecDeque,
    future::Ready,
    pin::pin,
    task::{Context, Poll},
};

use ai::{
    AssistantEventStream, AssistantMessage, AssistantMessageEvent, ContentBlock, DoneReason,
    ErrorReason, JsonObject, Message, Model, PiError, SimpleStreamOptions, StopReason,
    StreamRequest, TextContent, ThinkingContent, ToolCall, TranscriptContext, Usage, UserContent,
    UserMessage,
};
use futures_core::Stream;
use futures_util::{FutureExt as _, stream, task::noop_waker};
use tokio_util::sync::CancellationToken;
use tower::Service;

/// 每个 delta 分块包含的字符数。pi 的 faux provider 随机分块；
/// conformance 固定该值，使金标准序列精确可复现。
pub(crate) const CHUNK_CHARS: usize = 4;

/// 一个脚本化响应槽位。
pub(crate) enum FauxResponse {
    /// 正常流式返回一条 assistant 消息。
    Message(Box<AssistantMessage>),
    /// 请求建立阶段失败：单个 `error` 事件，不带 `start`。
    SetupFailure(String),
}

impl FauxResponse {
    /// 脚本化一条正常流式响应。
    pub(crate) fn message(message: AssistantMessage) -> Self {
        Self::Message(Box::new(message))
    }

    /// 脚本化一次 setup 失败，携带给定的错误详情。
    pub(crate) fn setup_failure(detail: impl Into<String>) -> Self {
        Self::SetupFailure(detail.into())
    }
}

/// 实现 pi `StreamFunction` 契约的最小 faux provider。
pub(crate) struct FauxProvider {
    scripts: VecDeque<FauxResponse>,
}

impl FauxProvider {
    /// 排入脚本化响应；每次调用消费一个。
    pub(crate) fn new(scripts: impl IntoIterator<Item = FauxResponse>) -> Self {
        Self {
            scripts: scripts.into_iter().collect(),
        }
    }
}

impl Service<StreamRequest> for FauxProvider {
    type Response = AssistantEventStream;
    type Error = PiError;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: StreamRequest) -> Self::Future {
        let script = self.scripts.pop_front().unwrap_or_else(|| {
            FauxResponse::SetupFailure("no more faux responses queued".to_owned())
        });
        let signal = request.options.base.signal.clone();
        let events = match script {
            FauxResponse::Message(message) => script_message(*message, signal.as_ref()),
            FauxResponse::SetupFailure(detail) => vec![AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error: error_message(&request.model, &detail),
            }],
        };
        std::future::ready(Ok(AssistantEventStream::new(stream::iter(events))))
    }
}

/// 驱动一个脚本化响应直至结束。
pub(crate) fn run(
    provider: &mut FauxProvider,
    options: SimpleStreamOptions,
) -> Vec<AssistantMessageEvent> {
    let stream = provider
        .call(request(options))
        .now_or_never()
        .expect("faux 调用应当立即就绪")
        .expect("faux 调用应当成功");
    collect(stream)
}

fn collect(stream: AssistantEventStream) -> Vec<AssistantMessageEvent> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut stream = pin!(stream);
    let mut events = Vec::new();
    loop {
        match stream.as_mut().poll_next(&mut cx) {
            Poll::Ready(Some(event)) => events.push(event),
            Poll::Ready(None) => break,
            Poll::Pending => panic!("faux 流不应处于 pending"),
        }
    }
    events
}

fn script_message(
    message: AssistantMessage,
    signal: Option<&CancellationToken>,
) -> Vec<AssistantMessageEvent> {
    let mut partial = message.clone();
    partial.content.clear();
    partial.stop_reason = StopReason::Pending;

    if signal.is_some_and(CancellationToken::is_cancelled) {
        return vec![AssistantMessageEvent::Error {
            reason: ErrorReason::Aborted,
            error: aborted_message(&partial),
        }];
    }

    let mut events = vec![AssistantMessageEvent::Start {
        partial: partial.clone(),
    }];

    for (index, block) in message.content.iter().enumerate() {
        match block {
            ContentBlock::Thinking(block) => {
                partial
                    .content
                    .push(ContentBlock::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    }));
                events.push(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                for chunk in chunks(&block.thinking) {
                    if let Some(ContentBlock::Thinking(current)) = partial.content.get_mut(index) {
                        current.thinking.push_str(&chunk);
                    }
                    events.push(AssistantMessageEvent::ThinkingDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                events.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: block.thinking.clone(),
                    partial: partial.clone(),
                });
            }
            ContentBlock::Text(block) => {
                partial.content.push(ContentBlock::Text(TextContent {
                    text: String::new(),
                    text_signature: None,
                }));
                events.push(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                for chunk in chunks(&block.text) {
                    if let Some(ContentBlock::Text(current)) = partial.content.get_mut(index) {
                        current.text.push_str(&chunk);
                    }
                    events.push(AssistantMessageEvent::TextDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                events.push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: block.text.clone(),
                    partial: partial.clone(),
                });
            }
            ContentBlock::ToolCall(block) => {
                partial.content.push(ContentBlock::ToolCall(ToolCall {
                    id: block.id.clone(),
                    name: block.name.clone(),
                    arguments: JsonObject::new(),
                    thought_signature: None,
                    namespace: None,
                }));
                events.push(AssistantMessageEvent::ToolCallStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                let encoded = serde_json::Value::Object(block.arguments.clone()).to_string();
                for chunk in chunks(&encoded) {
                    // pi 在 delta 期间保持 `partial.arguments` 为空，
                    // 只在 `toolcall_end` 时装入解析后的参数。
                    events.push(AssistantMessageEvent::ToolCallDelta {
                        content_index: index,
                        delta: chunk,
                        partial: partial.clone(),
                    });
                }
                if let Some(ContentBlock::ToolCall(current)) = partial.content.get_mut(index) {
                    current.arguments = block.arguments.clone();
                }
                events.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call: block.clone(),
                    partial: partial.clone(),
                });
            }
            ContentBlock::Image(_) => panic!("faux provider 不能流式输出图片"),
        }
    }

    match message.stop_reason {
        StopReason::Stop => events.push(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message,
        }),
        StopReason::Length => events.push(AssistantMessageEvent::Done {
            reason: DoneReason::Length,
            message,
        }),
        StopReason::ToolUse => events.push(AssistantMessageEvent::Done {
            reason: DoneReason::ToolUse,
            message,
        }),
        StopReason::Deferred => events.push(AssistantMessageEvent::Done {
            reason: DoneReason::Deferred,
            message,
        }),
        StopReason::Error => events.push(AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: message,
        }),
        StopReason::Aborted => events.push(AssistantMessageEvent::Error {
            reason: ErrorReason::Aborted,
            error: message,
        }),
        StopReason::Pending => panic!("faux 响应结束时缺少 stop reason"),
    }

    events
}

fn aborted_message(partial: &AssistantMessage) -> AssistantMessage {
    let mut message = partial.clone();
    message.stop_reason = StopReason::Aborted;
    message.error_message = Some("Request was aborted".to_owned());
    message
}

fn error_message(model: &Model, error_message: &str) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        deferred: None,
        error_message: Some(error_message.to_owned()),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }
}

fn chunks(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(CHUNK_CHARS)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// 用给定内容与终止原因构造一条 assistant 消息。
pub(crate) fn message(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "faux".to_owned(),
        provider: "faux".to_owned(),
        model: "faux-1".to_owned(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        usage: Usage::default(),
        stop_reason,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }
}

/// 文本内容块。
pub(crate) fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })
}

/// 思考内容块。
pub(crate) fn thinking_block(thinking: &str) -> ContentBlock {
    ContentBlock::Thinking(ThinkingContent {
        thinking: thinking.to_owned(),
        thinking_signature: None,
        redacted: None,
    })
}

/// 工具调用内容块。
pub(crate) fn tool_call_block(id: &str, name: &str, arguments: JsonObject) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments,
        thought_signature: None,
        namespace: None,
    })
}

/// 最小的 faux 模型请求；faux provider 只关心其中的 `options`。
pub(crate) fn request(options: SimpleStreamOptions) -> StreamRequest {
    StreamRequest {
        model: Model {
            id: "faux-1".to_owned(),
            name: "Faux Model".to_owned(),
            api: "faux".to_owned(),
            provider: "faux".to_owned(),
            ..Model::default()
        },
        context: TranscriptContext::new(vec![Message::User(UserMessage {
            content: UserContent::Text("hi".to_owned()),
            timestamp: 0,
        })]),
        options,
    }
}
