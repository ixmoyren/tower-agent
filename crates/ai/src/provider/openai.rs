//! OpenAI Chat Completions provider（对应 pi `src/api/openai-completions.ts`）。
//!
//! 这一条同时覆盖大量兼容端点（OpenAI / Azure OpenAI / OpenRouter / Cerebras / Groq /
//! DeepSeek …），因为它们的 `/chat/completions` 事件形状一致。
//!
//! 覆盖核心路径：请求构造（system / developer 兼容 / tools / temperature / usage 回传）、
//! `content` / `reasoning_content` / `tool_calls` 的流式映射、`finish_reason` 映射、
//! `data: [DONE]` 收尾，以及与 Anthropic 相同的错误归属。

use std::collections::{BTreeMap, VecDeque};

use futures_util::{StreamExt as _, future::BoxFuture, stream};
use serde_json::{Value as JsonValue, json};
use tower::{Service, ServiceExt as _};

use crate::{
    AssistantEventStream, AssistantMessage, AssistantMessageEvent, ContentBlock, DoneReason,
    ErrorReason, ImageContent, Message, Model, PiError, StopReason, StreamRequest, TextContent,
    ThinkingContent, ToolCall, UserContent,
    http::{ByteStream, CallContext, HttpHeaders, HttpRequest, HttpTransport},
    provider::common::{
        content_text, empty_message, error_stream, openai_image_part, retryable_status, set_header,
        system_and_tools, tool_result_text, truncate, update_usage_totals,
    },
};

/// pi 里这条 provider 的 api 标识。
pub const API: &str = "openai-completions";
/// 默认 API base URL（含 `/v1`）。
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// provider 配置。
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// API base URL（不含 `/chat/completions`）。
    pub base_url: String,
    /// 额外请求头（同名覆盖）。
    pub headers: HttpHeaders,
    /// 组织标识（`OpenAI-Organization`）。
    pub organization: Option<String>,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            headers: HttpHeaders::new(),
            organization: None,
        }
    }
}

/// OpenAI Chat Completions provider。
#[derive(Clone)]
pub struct OpenAiProvider<T: HttpTransport> {
    transport: T,
    api_key: Option<String>,
    config: OpenAiConfig,
}

impl<T: HttpTransport> OpenAiProvider<T> {
    /// 用给定传输层构造。
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            api_key: None,
            config: OpenAiConfig::default(),
        }
    }

    /// 设置 API key（作为 `Authorization: Bearer`）。
    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// 设置 base URL。
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.config.base_url = base_url.into();
        self
    }

    /// 追加一个请求头。
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.headers.insert(name.into(), value.into());
        self
    }

    /// 设置组织。
    #[must_use]
    pub fn with_organization(mut self, organization: impl Into<String>) -> Self {
        self.config.organization = Some(organization.into());
        self
    }

    /// 当前配置。
    pub fn config(&self) -> &OpenAiConfig {
        &self.config
    }

    async fn execute(&self, request: StreamRequest) -> Result<AssistantEventStream, PiError> {
        let provider_id = request.model.provider.clone();
        let api_key = self
            .api_key
            .clone()
            .or_else(|| request.options.base.api_key.clone())
            .ok_or_else(|| PiError::MissingCredentials {
                provider: provider_id.clone(),
            })?;

        let body = build_request_body(&request)?;
        let body =
            serde_json::to_vec(&body).map_err(|error| PiError::Serialization(error.to_string()))?;

        let mut headers = HttpHeaders::new();
        headers.insert("content-type".into(), "application/json".into());
        headers.insert("accept".into(), "text/event-stream".into());
        headers.insert("authorization".into(), format!("Bearer {api_key}"));
        if let Some(organization) = &self.config.organization {
            headers.insert("openai-organization".into(), organization.clone());
        }
        for (name, value) in &self.config.headers {
            set_header(&mut headers, name, Some(value));
        }
        for (name, value) in &request.options.base.headers {
            set_header(&mut headers, name, value.as_deref());
        }

        let http_request = HttpRequest {
            method: "POST".into(),
            url: format!(
                "{}/chat/completions",
                self.config.base_url.trim_end_matches('/')
            ),
            headers,
            body,
            context: Some(CallContext {
                provider: request.model.provider.clone(),
                api: request.model.api.clone(),
                model: request.model.id.clone(),
            }),
        };

        let response = self
            .transport
            .send(http_request)
            .await
            .map_err(|error| PiError::Transport(error.message))?;

        if !response.is_success() {
            let status = response.status;
            let text = response.body_text().await.unwrap_or_default();
            let message = format!(
                "OpenAI API error ({status}): {}",
                truncate(text.trim(), 500)
            );
            if retryable_status(status) {
                return Err(PiError::Provider {
                    provider: provider_id,
                    message,
                });
            }
            return Ok(error_stream(&request.model, message));
        }

        Ok(map_response_stream(request.model, response.body))
    }
}

impl<T: HttpTransport> Service<StreamRequest> for OpenAiProvider<T> {
    type Response = AssistantEventStream;
    type Error = PiError;
    type Future = BoxFuture<'static, Result<AssistantEventStream, PiError>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: StreamRequest) -> Self::Future {
        let provider = self.clone();
        Box::pin(async move { provider.execute(request).await })
    }
}

/// 流里的内容块状态（OpenAI 的 text / thinking / tool_calls 都是懒开启的）。
#[derive(Debug, Clone)]
enum BlockState {
    Text(String),
    Thinking(String),
    Tool {
        call: ToolCall,
        partial_json: String,
    },
}

impl BlockState {
    fn to_block(&self) -> ContentBlock {
        match self {
            Self::Text(text) => ContentBlock::Text(TextContent {
                text: text.clone(),
                text_signature: None,
            }),
            Self::Thinking(thinking) => ContentBlock::Thinking(ThinkingContent {
                thinking: thinking.clone(),
                thinking_signature: None,
                redacted: None,
            }),
            Self::Tool { call, .. } => ContentBlock::ToolCall(call.clone()),
        }
    }
}

struct StreamState {
    body: ByteStream,
    sse: crate::sse::SseDecoder,
    model: Model,
    output: AssistantMessage,
    blocks: Vec<BlockState>,
    /// OpenAI 的 tool_call 下标 → 我们的内容块下标。
    tool_indices: BTreeMap<u64, usize>,
    finished: bool,
    /// 是否已经给所有块补过 `*_end`。
    closed: bool,
    queue: VecDeque<AssistantMessageEvent>,
    stream_ended: bool,
    failed: Option<String>,
}

fn map_response_stream(model: Model, body: ByteStream) -> AssistantEventStream {
    let state = StreamState {
        body,
        sse: crate::sse::SseDecoder::default(),
        output: empty_message(&model),
        model,
        blocks: Vec::new(),
        tool_indices: BTreeMap::new(),
        finished: false,
        closed: false,
        queue: VecDeque::new(),
        stream_ended: false,
        failed: None,
    };
    AssistantEventStream::new(stream::unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.queue.pop_front() {
                return Some((event, state));
            }
            if let Some(message) = state.failed.take() {
                state.output.stop_reason = StopReason::Error;
                state.output.error_message = Some(message.clone());
                state.sync_content();
                state.stream_ended = true;
                return Some((
                    AssistantMessageEvent::Error {
                        reason: ErrorReason::Error,
                        error: state.output.clone(),
                    },
                    state,
                ));
            }
            if state.stream_ended {
                return None;
            }
            match state.body.next().await {
                Some(Ok(chunk)) => {
                    let text = String::from_utf8_lossy(&chunk).into_owned();
                    for event in state.sse.push(&text) {
                        state.handle_sse(event);
                    }
                }
                Some(Err(error)) => state.failed = Some(error.message),
                None => {
                    if let Some(event) = state.sse.finish() {
                        state.handle_sse(event);
                    }
                    state.stream_ended = true;
                    state.finish_stream();
                }
            }
        }
    }))
}

impl StreamState {
    fn sync_content(&mut self) {
        self.output.content = self.blocks.iter().map(BlockState::to_block).collect();
    }

    fn handle_sse(&mut self, event: crate::sse::SseEvent) {
        let data = event.data.trim();
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            self.finished = true;
            self.close_blocks();
            return;
        }
        let Ok(payload) = serde_json::from_str::<JsonValue>(data) else {
            return;
        };
        // 有的兼容端点把错误放在流里。
        if let Some(error) = payload.get("error") {
            let message = error
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or("OpenAI stream error");
            self.failed = Some(message.to_string());
            return;
        }
        if let Some(usage) = payload.get("usage").filter(|value| !value.is_null()) {
            self.apply_usage(usage);
        }
        let Some(choice) = payload.get("choices").and_then(|value| value.get(0)) else {
            return;
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(JsonValue::as_str)
                && !content.is_empty()
            {
                self.push_text(content);
            }
            // DeepSeek 等兼容端点用 reasoning_content 承载思考。
            for key in ["reasoning_content", "reasoning"] {
                if let Some(reasoning) = delta.get(key).and_then(JsonValue::as_str)
                    && !reasoning.is_empty()
                {
                    self.push_thinking(reasoning);
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(JsonValue::as_array) {
                for call in calls {
                    self.push_tool_call(call);
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(JsonValue::as_str) {
            self.output.raw_stop_reason = Some(reason.to_string());
            match map_finish_reason(reason) {
                Ok(stop_reason) => self.output.stop_reason = stop_reason,
                Err(message) => self.failed = Some(message),
            }
            self.finished = true;
            self.close_blocks();
        }
    }

    fn push_text(&mut self, content: &str) {
        let position = match self
            .blocks
            .iter()
            .position(|block| matches!(block, BlockState::Text(_)))
        {
            Some(position) => position,
            None => {
                self.blocks.push(BlockState::Text(String::new()));
                let position = self.blocks.len() - 1;
                self.sync_content();
                self.queue.push_back(AssistantMessageEvent::TextStart {
                    content_index: position,
                    partial: self.output.clone(),
                });
                position
            }
        };
        if let BlockState::Text(text) = &mut self.blocks[position] {
            text.push_str(content);
        }
        self.sync_content();
        self.queue.push_back(AssistantMessageEvent::TextDelta {
            content_index: position,
            delta: content.to_string(),
            partial: self.output.clone(),
        });
    }

    fn push_thinking(&mut self, content: &str) {
        let position = match self
            .blocks
            .iter()
            .position(|block| matches!(block, BlockState::Thinking(_)))
        {
            Some(position) => position,
            None => {
                self.blocks.push(BlockState::Thinking(String::new()));
                let position = self.blocks.len() - 1;
                self.sync_content();
                self.queue.push_back(AssistantMessageEvent::ThinkingStart {
                    content_index: position,
                    partial: self.output.clone(),
                });
                position
            }
        };
        if let BlockState::Thinking(thinking) = &mut self.blocks[position] {
            thinking.push_str(content);
        }
        self.sync_content();
        self.queue.push_back(AssistantMessageEvent::ThinkingDelta {
            content_index: position,
            delta: content.to_string(),
            partial: self.output.clone(),
        });
    }

    fn push_tool_call(&mut self, call: &JsonValue) {
        let index = call.get("index").and_then(JsonValue::as_u64).unwrap_or(0);
        let function = call.get("function").cloned().unwrap_or(JsonValue::Null);
        let position = match self.tool_indices.get(&index).copied() {
            Some(position) => position,
            None => {
                let tool_call = ToolCall {
                    id: call
                        .get("id")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: function
                        .get("name")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    arguments: Default::default(),
                    thought_signature: None,
                    namespace: None,
                };
                self.blocks.push(BlockState::Tool {
                    call: tool_call,
                    partial_json: String::new(),
                });
                let position = self.blocks.len() - 1;
                self.tool_indices.insert(index, position);
                self.sync_content();
                self.queue.push_back(AssistantMessageEvent::ToolCallStart {
                    content_index: position,
                    partial: self.output.clone(),
                });
                position
            }
        };
        if let Some(arguments) = function.get("arguments").and_then(JsonValue::as_str) {
            if arguments.is_empty() {
                return;
            }
            if let BlockState::Tool { call, partial_json } = &mut self.blocks[position] {
                partial_json.push_str(arguments);
                if let Ok(JsonValue::Object(parsed)) =
                    serde_json::from_str::<JsonValue>(partial_json)
                {
                    call.arguments = parsed;
                }
            }
            self.sync_content();
            self.queue.push_back(AssistantMessageEvent::ToolCallDelta {
                content_index: position,
                delta: arguments.to_string(),
                partial: self.output.clone(),
            });
        }
    }

    fn apply_usage(&mut self, usage: &JsonValue) {
        if let Some(value) = usage.get("prompt_tokens").and_then(JsonValue::as_u64) {
            self.output.usage.input = value;
        }
        if let Some(value) = usage.get("completion_tokens").and_then(JsonValue::as_u64) {
            self.output.usage.output = value;
        }
        if let Some(value) = usage
            .get("prompt_tokens_details")
            .and_then(|value| value.get("cached_tokens"))
            .and_then(JsonValue::as_u64)
        {
            self.output.usage.cache_read = value;
        }
        if let Some(value) = usage
            .get("completion_tokens_details")
            .and_then(|value| value.get("reasoning_tokens"))
            .and_then(JsonValue::as_u64)
        {
            self.output.usage.reasoning = Some(value);
        }
        if let Some(value) = usage.get("total_tokens").and_then(JsonValue::as_u64) {
            self.output.usage.total_tokens = value;
        }
        update_usage_totals(&self.model, &mut self.output.usage);
    }

    /// 流结束/`[DONE]` 时收尾：给所有打开的内容块补 `*_end`。
    fn close_blocks(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.sync_content();
        for position in 0..self.blocks.len() {
            let partial = self.output.clone();
            let event = match &self.blocks[position] {
                BlockState::Text(text) => AssistantMessageEvent::TextEnd {
                    content_index: position,
                    content: text.clone(),
                    partial,
                },
                BlockState::Thinking(thinking) => AssistantMessageEvent::ThinkingEnd {
                    content_index: position,
                    content: thinking.clone(),
                    partial,
                },
                BlockState::Tool { call, .. } => AssistantMessageEvent::ToolCallEnd {
                    content_index: position,
                    tool_call: call.clone(),
                    partial,
                },
            };
            self.queue.push_back(event);
        }
    }

    fn finish_stream(&mut self) {
        if self.failed.is_some() {
            return;
        }
        // 与 pi 一致：无论后面是 done 还是错误，先把打开的内容块补上 `*_end`。
        self.close_blocks();
        self.sync_content();
        if !self.finished {
            // 没有 finish_reason 也没有 [DONE]：按 pi 的契约视为错误。
            self.failed = Some("OpenAI stream ended without a finish reason".into());
            return;
        }
        match self.output.stop_reason {
            StopReason::Pending => {
                self.failed = Some("OpenAI stream ended without a finish reason".into());
            }
            StopReason::Error | StopReason::Aborted => {
                self.failed = Some(
                    self.output
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "An unknown error occurred".into()),
                );
            }
            stop_reason => {
                let reason = match stop_reason {
                    StopReason::Length => DoneReason::Length,
                    StopReason::ToolUse => DoneReason::ToolUse,
                    StopReason::Deferred => DoneReason::Deferred,
                    _ => DoneReason::Stop,
                };
                self.queue.push_back(AssistantMessageEvent::Done {
                    reason,
                    message: self.output.clone(),
                });
            }
        }
    }
}

/// OpenAI `finish_reason` → pi [`StopReason`]。
fn map_finish_reason(reason: &str) -> Result<StopReason, String> {
    Ok(match reason {
        "stop" => StopReason::Stop,
        "length" => StopReason::Length,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => return Err("Provider stopped with: content_filter".into()),
        other => return Err(format!("Unhandled finish reason: {other}")),
    })
}

fn build_request_body(request: &StreamRequest) -> Result<JsonValue, PiError> {
    let (system, tools) = system_and_tools(request.context.messages());
    let messages = convert_messages(&system, request.context.messages())?;
    let max_tokens = request
        .options
        .base
        .max_tokens
        .unwrap_or_else(|| request.model.max_tokens.clamp(1, u32::MAX as u64) as u32);
    let mut body = json!({
        "model": request.model.id,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if let Some(temperature) = request.options.base.temperature {
        body["temperature"] = json!(temperature);
    }
    if !tools.is_empty() {
        body["tools"] = JsonValue::Array(
            tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        },
                    })
                })
                .collect(),
        );
        body["tool_choice"] = json!("auto");
    }
    if let Some(session_id) = &request.options.base.session_id {
        body["user"] = json!(session_id);
    }
    Ok(body)
}

fn convert_messages(system: &str, messages: &[Message]) -> Result<Vec<JsonValue>, PiError> {
    let mut converted = Vec::new();
    if !system.is_empty() {
        converted.push(json!({ "role": "system", "content": system }));
    }
    for message in messages {
        match message {
            Message::System(_) => {}
            Message::User(user) => converted.push(match &user.content {
                UserContent::Text(text) => json!({ "role": "user", "content": text }),
                UserContent::Blocks(blocks) => json!({
                    "role": "user",
                    "content": convert_user_blocks(blocks),
                }),
            }),
            Message::Assistant(assistant) => {
                let text = content_text(&assistant.content);
                let tool_calls: Vec<JsonValue> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(call) => Some(json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": JsonValue::Object(call.arguments.clone()).to_string(),
                            },
                        })),
                        _ => None,
                    })
                    .collect();
                if text.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                let mut entry = json!({ "role": "assistant", "content": text });
                if !tool_calls.is_empty() {
                    entry["tool_calls"] = JsonValue::Array(tool_calls);
                    if text.is_empty() {
                        entry["content"] = JsonValue::Null;
                    }
                }
                converted.push(entry);
            }
            Message::ToolResult(result) => converted.push(json!({
                "role": "tool",
                "tool_call_id": result.tool_call_id,
                "content": tool_result_text(&result.content),
            })),
        }
    }
    Ok(converted)
}

fn convert_user_blocks(blocks: &[ContentBlock]) -> Vec<JsonValue> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(json!({ "type": "text", "text": text.text })),
            ContentBlock::Image(image) => Some(openai_image_part(image)),
            _ => None,
        })
        .collect()
}

/// 便利：把 provider 装箱成 [`crate::BoxStreamService`]（与 `StreamFn` 同形）。
pub fn boxed<T: HttpTransport>(provider: OpenAiProvider<T>) -> crate::BoxStreamService
where
    <OpenAiProvider<T> as Service<StreamRequest>>::Future: Send + 'static,
{
    tower::util::BoxCloneService::new(provider)
}

impl<T: HttpTransport> OpenAiProvider<T> {
    /// 便利方法：直接按 `StreamRequest` 取流（等价于 `call`）。
    pub async fn stream(&self, request: StreamRequest) -> Result<AssistantEventStream, PiError> {
        self.clone().oneshot(request).await
    }
}

/// 让 `ImageContent` 的导入在未使用图片时也不报警告（图片路径会在 `convert_user_blocks` 用到）。
const _: fn(&ImageContent) -> JsonValue = |image| openai_image_part(image);
