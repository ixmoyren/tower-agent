//! 确定性离线 provider，**测试 provider**：不联网、不调用模型，只按固定规则产生事件流。
//!
//! 它挂在 `test-util` feature 下（见 `Cargo.toml`），正常构建不会编进任何二进制；
//! 需要它的消费者（例如 `cli` 的会话测试）在 `[dev-dependencies]` 里带 feature 引入即可。
//! 目的有二：一是把会话端到端跑通而不依赖真实模型与凭据；二是给上层一个可复现、
//! 可脚本化的 provider 流。
//!
//! 规则（最后一条消息决定行为）：
//!
//! - 最后一条是工具结果 → 回一段总结文本并 `stop`（让工具往返能正常收敛）。
//! - 最后一条是用户消息且以 `!` 开头 → 解析成 `!<工具名> <JSON 参数>`，产生一次工具调用。
//! - 其他用户消息 → 回显 `[faux] 收到：<文本>` 并 `stop`。

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use crate::{
    AssistantEventStream, AssistantMessage, AssistantMessageEvent, BoxStreamService, ContentBlock,
    DoneReason, InputModality, JsonObject, Message, Model, PiError, StopReason, StreamRequest,
    TextContent, ToolCall, Usage, UserContent,
};
use futures_util::{future::BoxFuture, stream};
use tower::Service;

/// 构造 faux provider 的模型元数据。
#[must_use]
pub fn model() -> Model {
    Model {
        id: "faux".into(),
        name: "Faux (offline)".into(),
        api: "faux".into(),
        provider: "faux".into(),
        input: vec![InputModality::Text],
        context_window: 8192,
        max_tokens: 4096,
        ..Model::default()
    }
}

/// 确定性的离线 provider。
#[derive(Clone, Default)]
pub struct FauxProvider {
    calls: Arc<AtomicUsize>,
}

impl FauxProvider {
    /// 构造一个 faux provider。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 已处理的请求数。
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// 装箱成 agent 需要的 [`BoxStreamService`]。
    #[must_use]
    pub fn boxed(self) -> BoxStreamService {
        tower::util::BoxCloneService::new(self)
    }
}

impl Service<StreamRequest> for FauxProvider {
    type Response = AssistantEventStream;
    type Error = PiError;
    type Future = BoxFuture<'static, Result<AssistantEventStream, PiError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: StreamRequest) -> Self::Future {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let model = request.model.clone();
        let events = respond(&model, request.context.messages());
        Box::pin(async move { Ok(AssistantEventStream::new(stream::iter(events))) })
    }
}

/// 根据 transcript 产生一段完整的事件流（`start → *_start/*_delta/*_end → done`）。
fn respond(model: &Model, messages: &[Message]) -> Vec<AssistantMessageEvent> {
    match messages.last() {
        Some(Message::ToolResult(result)) => {
            let summary = first_line(&text_of_blocks(&result.content));
            text_stream(model, &format!("[faux] 工具结果已收到：{summary}"))
        }
        Some(Message::User(user)) => {
            let text = match &user.content {
                UserContent::Text(text) => text.clone(),
                UserContent::Blocks(blocks) => text_of_blocks(blocks),
            };
            match parse_tool_directive(&text) {
                Some((name, arguments)) => tool_call_stream(model, name, arguments),
                None => text_stream(model, &format!("[faux] 收到：{}", first_line(&text))),
            }
        }
        // 没有可响应的输入（正常流程不会走到这里）。
        _ => text_stream(model, "[faux] 没有可响应的消息。"),
    }
}

/// `!<tool> <json>` → 工具名 + 参数对象。
fn parse_tool_directive(text: &str) -> Option<(String, JsonObject)> {
    let rest = text.trim().strip_prefix('!')?;
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next()?.trim();
    if name.is_empty() {
        return None;
    }
    let arguments = match parts.next().map(str::trim) {
        None | Some("") => JsonObject::new(),
        Some(raw) => match serde_json::from_str::<crate::JsonValue>(raw) {
            Ok(crate::JsonValue::Object(map)) => map,
            // 参数不是 JSON 对象时退化成空参数，让工具自己报参数错误。
            _ => JsonObject::new(),
        },
    };
    Some((name.to_string(), arguments))
}

fn text_of_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

fn empty_message(model: &Model, stop_reason: StopReason) -> AssistantMessage {
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
        stop_reason,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn text_stream(model: &Model, text: &str) -> Vec<AssistantMessageEvent> {
    let start = empty_message(model, StopReason::Pending);
    let mut partial = start.clone();
    partial.content.push(text_block(""));
    let final_message = AssistantMessage {
        content: vec![text_block(text)],
        stop_reason: StopReason::Stop,
        ..empty_message(model, StopReason::Stop)
    };
    vec![
        AssistantMessageEvent::Start { partial: start },
        AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: partial.clone(),
        },
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: text.into(),
            partial: partial.clone(),
        },
        AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: text.into(),
            partial,
        },
        AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: final_message,
        },
    ]
}

fn tool_call_stream(
    model: &Model,
    name: String,
    arguments: JsonObject,
) -> Vec<AssistantMessageEvent> {
    let call = ToolCall {
        id: format!("faux-call-{name}"),
        name,
        arguments,
        thought_signature: None,
        namespace: None,
    };
    let start = empty_message(model, StopReason::Pending);
    let mut partial = start.clone();
    partial.content.push(ContentBlock::ToolCall(call.clone()));
    let final_message = AssistantMessage {
        content: vec![ContentBlock::ToolCall(call.clone())],
        stop_reason: StopReason::ToolUse,
        ..empty_message(model, StopReason::ToolUse)
    };
    vec![
        AssistantMessageEvent::Start { partial: start },
        AssistantMessageEvent::ToolCallStart {
            content_index: 0,
            partial: empty_message(model, StopReason::Pending),
        },
        AssistantMessageEvent::ToolCallDelta {
            content_index: 0,
            delta: String::new(),
            partial: partial.clone(),
        },
        AssistantMessageEvent::ToolCallEnd {
            content_index: 0,
            tool_call: call,
            partial,
        },
        AssistantMessageEvent::Done {
            reason: DoneReason::ToolUse,
            message: final_message,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(model: &Model, messages: Vec<Message>) -> StreamRequest {
        StreamRequest {
            model: model.clone(),
            context: crate::TranscriptContext::new(messages),
            options: crate::SimpleStreamOptions::default(),
        }
    }

    async fn events_for(messages: Vec<Message>) -> Vec<AssistantMessageEvent> {
        use futures_util::StreamExt as _;
        let model = model();
        let mut provider = FauxProvider::new();
        let mut stream = provider
            .call(request(&model, messages))
            .await
            .expect("faux provider never fails");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    fn user(text: &str) -> Message {
        Message::User(crate::UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: 0,
        })
    }

    #[tokio::test]
    async fn plain_user_message_produces_a_text_reply() {
        let events = events_for(vec![user("你好")]).await;
        assert_eq!(
            events.first().map(AssistantMessageEvent::kind),
            Some("start")
        );
        assert_eq!(events.last().map(AssistantMessageEvent::kind), Some("done"));
        let message = events
            .last()
            .and_then(AssistantMessageEvent::terminal_message)
            .expect("terminal message");
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(text_of_blocks(&message.content), "[faux] 收到：你好");
    }

    #[tokio::test]
    async fn bang_directive_produces_a_tool_call() {
        let events = events_for(vec![user("!read {\"path\":\"a.txt\"}")]).await;
        let message = events
            .last()
            .and_then(AssistantMessageEvent::terminal_message)
            .expect("terminal message");
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        match message.content.first() {
            Some(ContentBlock::ToolCall(call)) => {
                assert_eq!(call.name, "read");
                assert_eq!(call.arguments.get("path"), Some(&json!("a.txt")));
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_result_produces_a_summary() {
        let result = crate::ToolResultMessage {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            content: vec![text_block("第一行\n第二行")],
            details: None,
            is_error: false,
            timestamp: 0,
        };
        let events = events_for(vec![user("!read {}"), Message::ToolResult(result)]).await;
        let message = events
            .last()
            .and_then(AssistantMessageEvent::terminal_message)
            .expect("terminal message");
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert!(text_of_blocks(&message.content).contains("第一行"));
    }
}
