//! Google Generative AI provider（对应 pi `src/api/google-generative-ai.ts`）。
//!
//! 这条 API 的形状和前两条差别很大，正好用来验证 provider 抽象：
//!
//! - URL 把模型放在路径里：`POST {base}/{version}/models/{model}:streamGenerateContent?alt=sse`；
//! - 认证用 `x-goog-api-key`，而不是 Bearer / x-api-key；
//! - 请求体是 `contents`（`role: user|model` + `parts`）而不是 `messages`，
//!   system prompt 走 `systemInstruction`，工具声明走 `tools[].functionDeclarations`；
//! - 流里的 `functionCall` 是**整块对象**（不是 JSON 分片），所以一次就发 start/delta/end；
//! - 用量在 `usageMetadata`（input 要减去 cached，output 要加上 thoughts）。

use std::collections::VecDeque;

use futures_util::{StreamExt as _, future::BoxFuture, stream};
use serde_json::{Value as JsonValue, json};
use tower::{Service, ServiceExt as _};

use crate::{
    AssistantEventStream, AssistantMessage, AssistantMessageEvent, ContentBlock, DoneReason,
    ErrorReason, Message, Model, PiError, StopReason, StreamRequest, TextContent, ThinkingContent,
    ToolCall, UserContent,
    http::{ByteStream, CallContext, HttpHeaders, HttpRequest, HttpTransport},
    provider::common::{
        content_text, empty_message, error_stream, retryable_status, set_header, system_and_tools,
        truncate, update_usage_totals,
    },
};

/// pi 里这条 provider 的 api 标识。
pub const API: &str = "google-generative-ai";
/// 默认 API base URL（不含版本段）。
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
/// 默认 API 版本。
pub const DEFAULT_API_VERSION: &str = "v1beta";

/// provider 配置。
#[derive(Debug, Clone)]
pub struct GoogleConfig {
    /// API base URL。
    pub base_url: String,
    /// API 版本段（默认 `v1beta`；base URL 自带版本时可以设成空串）。
    pub api_version: String,
    /// 额外请求头（同名覆盖）。
    pub headers: HttpHeaders,
}

impl Default for GoogleConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            api_version: DEFAULT_API_VERSION.into(),
            headers: HttpHeaders::new(),
        }
    }
}

/// Google Generative AI provider。
#[derive(Clone)]
pub struct GoogleProvider<T: HttpTransport> {
    transport: T,
    api_key: Option<String>,
    config: GoogleConfig,
}

impl<T: HttpTransport> GoogleProvider<T> {
    /// 用给定传输层构造。
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            api_key: None,
            config: GoogleConfig::default(),
        }
    }

    /// 设置 API key（`x-goog-api-key`）。
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

    /// 设置 API 版本段。
    #[must_use]
    pub fn with_api_version(mut self, api_version: impl Into<String>) -> Self {
        self.config.api_version = api_version.into();
        self
    }

    /// 追加一个请求头。
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.headers.insert(name.into(), value.into());
        self
    }

    /// 当前配置。
    pub fn config(&self) -> &GoogleConfig {
        &self.config
    }

    /// 请求 URL（pi 把模型放进路径）。
    pub fn endpoint(&self, model: &str) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        let version = self.config.api_version.trim_matches('/');
        let path = if version.is_empty() {
            format!("models/{model}:streamGenerateContent?alt=sse")
        } else {
            format!("{version}/models/{model}:streamGenerateContent?alt=sse")
        };
        format!("{base}/{path}")
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
        headers.insert("x-goog-api-key".into(), api_key);
        for (name, value) in &self.config.headers {
            set_header(&mut headers, name, Some(value));
        }
        for (name, value) in &request.options.base.headers {
            set_header(&mut headers, name, value.as_deref());
        }

        let http_request = HttpRequest {
            method: "POST".into(),
            url: self.endpoint(&request.model.id),
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
                "Google API error ({status}): {}",
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

impl<T: HttpTransport> Service<StreamRequest> for GoogleProvider<T> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CurrentKind {
    Text,
    Thinking,
}

#[derive(Debug, Clone)]
enum BlockState {
    Text(String),
    Thinking(String),
    Tool(ToolCall),
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
            Self::Tool(call) => ContentBlock::ToolCall(call.clone()),
        }
    }
}

struct StreamState {
    body: ByteStream,
    sse: crate::sse::SseDecoder,
    model: Model,
    output: AssistantMessage,
    blocks: Vec<BlockState>,
    /// 当前打开的文本/思考块（Google 的 parts 是增量的，需要自己决定何时换块）。
    current: Option<(usize, CurrentKind)>,
    tool_call_counter: usize,
    queue: VecDeque<AssistantMessageEvent>,
    finished: bool,
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
        current: None,
        tool_call_counter: 0,
        queue: VecDeque::new(),
        finished: false,
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
        let Ok(payload) = serde_json::from_str::<JsonValue>(data) else {
            return;
        };
        if let Some(error) = payload.get("error") {
            let message = error
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or("Google stream error");
            self.failed = Some(message.to_string());
            return;
        }
        let candidate = payload
            .get("candidates")
            .and_then(|value| value.get(0))
            .cloned()
            .unwrap_or(JsonValue::Null);
        if let Some(parts) = candidate
            .get("content")
            .and_then(|value| value.get("parts"))
            .and_then(JsonValue::as_array)
        {
            for part in parts {
                self.handle_part(part);
            }
        }
        if let Some(reason) = candidate.get("finishReason").and_then(JsonValue::as_str) {
            self.output.raw_stop_reason = Some(reason.to_string());
            self.output.stop_reason = map_finish_reason(reason);
            // Gemini 在有工具调用时也可能报 STOP：与 pi 一致地纠正成 toolUse。
            if self
                .output
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolCall(_)))
                && self.output.stop_reason == StopReason::Stop
            {
                self.output.stop_reason = StopReason::ToolUse;
            }
            self.finished = true;
        }
        if let Some(usage) = payload.get("usageMetadata") {
            self.apply_usage(usage);
        }
    }

    fn handle_part(&mut self, part: &JsonValue) {
        if let Some(text) = part.get("text").and_then(JsonValue::as_str) {
            let thinking = part
                .get("thought")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            let kind = if thinking {
                CurrentKind::Thinking
            } else {
                CurrentKind::Text
            };
            self.push_text(kind, text);
        }
        if let Some(call) = part.get("functionCall") {
            self.push_tool_call(call);
        }
    }

    fn push_text(&mut self, kind: CurrentKind, text: &str) {
        // 与 pi 一致：当前块类型变化时先结束旧块，再开新块。
        if self.current.map(|(_, current)| current) != Some(kind) {
            self.close_current_block();
            let state = match kind {
                CurrentKind::Text => BlockState::Text(String::new()),
                CurrentKind::Thinking => BlockState::Thinking(String::new()),
            };
            self.blocks.push(state);
            let position = self.blocks.len() - 1;
            self.current = Some((position, kind));
            self.sync_content();
            let partial = self.output.clone();
            self.queue.push_back(match kind {
                CurrentKind::Text => AssistantMessageEvent::TextStart {
                    content_index: position,
                    partial,
                },
                CurrentKind::Thinking => AssistantMessageEvent::ThinkingStart {
                    content_index: position,
                    partial,
                },
            });
        }
        let Some((position, kind)) = self.current else {
            return;
        };
        match (&mut self.blocks[position], kind) {
            (BlockState::Text(current), CurrentKind::Text) => current.push_str(text),
            (BlockState::Thinking(current), CurrentKind::Thinking) => current.push_str(text),
            _ => return,
        }
        self.sync_content();
        let partial = self.output.clone();
        self.queue.push_back(match kind {
            CurrentKind::Text => AssistantMessageEvent::TextDelta {
                content_index: position,
                delta: text.to_string(),
                partial,
            },
            CurrentKind::Thinking => AssistantMessageEvent::ThinkingDelta {
                content_index: position,
                delta: text.to_string(),
                partial,
            },
        });
    }

    fn close_current_block(&mut self) {
        let Some((position, kind)) = self.current.take() else {
            return;
        };
        self.sync_content();
        let partial = self.output.clone();
        let event = match (&self.blocks[position], kind) {
            (BlockState::Text(text), _) => AssistantMessageEvent::TextEnd {
                content_index: position,
                content: text.clone(),
                partial,
            },
            (BlockState::Thinking(thinking), _) => AssistantMessageEvent::ThinkingEnd {
                content_index: position,
                content: thinking.clone(),
                partial,
            },
            _ => return,
        };
        self.queue.push_back(event);
    }

    fn push_tool_call(&mut self, call: &JsonValue) {
        self.close_current_block();
        let name = call
            .get("name")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string();
        let provided_id = call
            .get("id")
            .and_then(JsonValue::as_str)
            .map(str::to_string);
        // Google 可能不给 id，也可能重复：两种情况都自己生成一个唯一 id（pi 同样处理）。
        let id = match provided_id {
            Some(id)
                if !id.is_empty()
                    && !self.output.content.iter().any(
                        |block| matches!(block, ContentBlock::ToolCall(call) if call.id == id),
                    ) =>
            {
                id
            }
            _ => {
                self.tool_call_counter += 1;
                format!(
                    "{name}_{}_{}",
                    crate::provider::common::now_ms(),
                    self.tool_call_counter
                )
            }
        };
        let tool_call = ToolCall {
            id,
            name,
            arguments: call
                .get("args")
                .and_then(JsonValue::as_object)
                .cloned()
                .unwrap_or_default(),
            thought_signature: call
                .get("thoughtSignature")
                .and_then(JsonValue::as_str)
                .map(str::to_string),
            namespace: None,
        };
        self.blocks.push(BlockState::Tool(tool_call.clone()));
        let position = self.blocks.len() - 1;
        self.sync_content();
        let partial = self.output.clone();
        // functionCall 是整块到达的：一次发 start/delta/end（delta 是参数 JSON）。
        self.queue.push_back(AssistantMessageEvent::ToolCallStart {
            content_index: position,
            partial: partial.clone(),
        });
        self.queue.push_back(AssistantMessageEvent::ToolCallDelta {
            content_index: position,
            delta: JsonValue::Object(tool_call.arguments.clone()).to_string(),
            partial: partial.clone(),
        });
        self.queue.push_back(AssistantMessageEvent::ToolCallEnd {
            content_index: position,
            tool_call,
            partial,
        });
    }

    fn apply_usage(&mut self, usage: &JsonValue) {
        let prompt = usage
            .get("promptTokenCount")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let cached = usage
            .get("cachedContentTokenCount")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let candidates = usage
            .get("candidatesTokenCount")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let thoughts = usage
            .get("thoughtsTokenCount")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let total = usage
            .get("totalTokenCount")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        self.output.usage.input = prompt.saturating_sub(cached);
        self.output.usage.output = candidates + thoughts;
        self.output.usage.cache_read = cached;
        self.output.usage.cache_write = 0;
        self.output.usage.reasoning = Some(thoughts);
        self.output.usage.total_tokens = total;
        update_usage_totals(&self.model, &mut self.output.usage);
    }

    fn finish_stream(&mut self) {
        if self.failed.is_some() {
            return;
        }
        self.close_current_block();
        self.sync_content();
        if !self.finished {
            self.failed = Some("Google stream ended without a finish reason".into());
            return;
        }
        match self.output.stop_reason {
            StopReason::Pending => {
                self.failed = Some("Google stream ended without a finish reason".into());
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

/// Gemini `finishReason` → pi [`StopReason`]（pi `mapStopReason`）。
fn map_finish_reason(reason: &str) -> StopReason {
    match reason {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::Length,
        // 其余（SAFETY / RECITATION / MALFORMED_FUNCTION_CALL / …）pi 都归为 error。
        _ => StopReason::Error,
    }
}

fn build_request_body(request: &StreamRequest) -> Result<JsonValue, PiError> {
    let (system, tools) = system_and_tools(request.context.messages());
    let contents = convert_contents(request.context.messages());

    let mut generation_config = serde_json::Map::new();
    let max_tokens = request
        .options
        .base
        .max_tokens
        .unwrap_or_else(|| request.model.max_tokens.clamp(1, u32::MAX as u64) as u32);
    generation_config.insert("maxOutputTokens".into(), json!(max_tokens));
    if let Some(temperature) = request.options.base.temperature {
        generation_config.insert("temperature".into(), json!(temperature));
    }
    if let Some(level) = request
        .options
        .reasoning
        .filter(|_| request.model.reasoning)
    {
        // Gemini 把思考配置放在 generationConfig.thinkingConfig。
        generation_config.insert(
            "thinkingConfig".into(),
            json!({ "includeThoughts": true, "thinkingLevel": google_thinking_level(level) }),
        );
    }

    let mut body = json!({
        "contents": contents,
        "generationConfig": JsonValue::Object(generation_config),
    });
    if !system.is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
    }
    if !tools.is_empty() {
        body["tools"] = json!([{
            "functionDeclarations": tools.iter().map(|tool| json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            })).collect::<Vec<_>>(),
        }]);
        body["toolConfig"] = json!({ "functionCallingConfig": { "mode": "AUTO" } });
    }
    Ok(body)
}

/// pi 的思考级别 → Gemini 的 `thinkingLevel`。
fn google_thinking_level(level: crate::ThinkingLevel) -> &'static str {
    match level {
        crate::ThinkingLevel::Minimal => "LOW",
        crate::ThinkingLevel::Low => "LOW",
        crate::ThinkingLevel::Medium => "MEDIUM",
        crate::ThinkingLevel::High | crate::ThinkingLevel::XHigh | crate::ThinkingLevel::Max => {
            "HIGH"
        }
    }
}

fn convert_contents(messages: &[Message]) -> Vec<JsonValue> {
    let mut contents: Vec<JsonValue> = Vec::new();
    for message in messages {
        match message {
            Message::System(_) => {}
            Message::User(user) => {
                let parts = match &user.content {
                    UserContent::Text(text) => vec![json!({ "text": text })],
                    UserContent::Blocks(blocks) => convert_user_blocks(blocks),
                };
                push_content(&mut contents, "user", parts);
            }
            Message::Assistant(assistant) => {
                let parts: Vec<JsonValue> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(json!({ "text": text.text })),
                        ContentBlock::Thinking(thinking) => {
                            Some(json!({ "text": thinking.thinking, "thought": true }))
                        }
                        ContentBlock::ToolCall(call) => Some(json!({
                            "functionCall": {
                                "name": call.name,
                                "args": JsonValue::Object(call.arguments.clone()),
                            },
                        })),
                        ContentBlock::Image(_) => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    push_content(&mut contents, "model", parts);
                }
            }
            Message::ToolResult(result) => {
                // Gemini 用 role: user + functionResponse 回传工具结果。
                let parts = vec![json!({
                    "functionResponse": {
                        "name": result.tool_name,
                        "response": { "content": content_text(&result.content) },
                    },
                })];
                push_content(&mut contents, "user", parts);
            }
        }
    }
    contents
}

fn push_content(contents: &mut Vec<JsonValue>, role: &str, parts: Vec<JsonValue>) {
    match contents.last_mut() {
        Some(JsonValue::Object(last))
            if last.get("role").and_then(JsonValue::as_str) == Some(role) =>
        {
            if let Some(JsonValue::Array(existing)) = last.get_mut("parts") {
                existing.extend(parts);
            }
        }
        _ => contents.push(json!({ "role": role, "parts": parts })),
    }
}

fn convert_user_blocks(blocks: &[ContentBlock]) -> Vec<JsonValue> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(json!({ "text": text.text })),
            ContentBlock::Image(image) => Some(json!({
                "inlineData": { "mimeType": image.mime_type, "data": image.data },
            })),
            _ => None,
        })
        .collect()
}

/// 便利：把 provider 装箱成 [`crate::BoxStreamService`]（与 `StreamFn` 同形）。
pub fn boxed<T: HttpTransport>(provider: GoogleProvider<T>) -> crate::BoxStreamService
where
    <GoogleProvider<T> as Service<StreamRequest>>::Future: Send + 'static,
{
    tower::util::BoxCloneService::new(provider)
}

impl<T: HttpTransport> GoogleProvider<T> {
    /// 便利方法：直接按 `StreamRequest` 取流（等价于 `call`）。
    pub async fn stream(&self, request: StreamRequest) -> Result<AssistantEventStream, PiError> {
        self.clone().oneshot(request).await
    }
}
