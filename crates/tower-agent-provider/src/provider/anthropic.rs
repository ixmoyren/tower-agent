//! Anthropic Messages provider（对应 pi `src/api/anthropic-messages.ts`）。
//!
//! 覆盖核心路径：请求构造（system / messages / tools / thinking / temperature）、
//! `text` / `thinking` / `redacted_thinking` / `tool_use` 四种内容块的流式映射、
//! usage 与成本计算、`stop_reason` 映射，以及错误处理（非重试的 HTTP 错误与流中的
//! `error` 事件编码进流；可重试的状态交给上层 retry 层）。

use std::collections::{BTreeMap, VecDeque};

use futures_util::{StreamExt as _, future::BoxFuture, stream};
use serde_json::{Value as JsonValue, json};
use tower::{Service, ServiceExt as _};

use crate::{
    AssistantEventStream, AssistantMessage, AssistantMessageEvent, ContentBlock, DoneReason,
    ErrorReason, ImageContent, Message, Model, PiError, StopReason, StreamRequest, TextContent,
    ThinkingContent, ThinkingLevel, ToolCall, ToolResultMessage, UserContent,
    http::{ByteStream, CallContext, HttpHeaders, HttpRequest, HttpTransport},
    provider::common::{
        empty_message, error_stream, retryable_status, set_header, system_and_tools, truncate,
        update_usage_totals,
    },
};

/// pi 里这条 provider 的 api 标识。
pub const API: &str = "anthropic-messages";
/// 默认 API base URL。
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// 默认 `anthropic-version` 请求头。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// provider 配置。
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// API base URL。
    pub base_url: String,
    /// `anthropic-version` 请求头。
    pub version: String,
    /// 额外请求头（同名覆盖）。
    pub headers: HttpHeaders,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            version: ANTHROPIC_VERSION.into(),
            headers: HttpHeaders::new(),
        }
    }
}

/// Anthropic Messages provider。
#[derive(Clone)]
pub struct AnthropicProvider<T: HttpTransport> {
    transport: T,
    api_key: Option<String>,
    config: AnthropicConfig,
}

impl<T: HttpTransport> AnthropicProvider<T> {
    /// 用给定传输层构造（api key 也可以从请求选项里取）。
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            api_key: None,
            config: AnthropicConfig::default(),
        }
    }

    /// 设置 API key。
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

    /// 追加一个请求头（同名覆盖）。
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.headers.insert(name.into(), value.into());
        self
    }

    /// 当前配置。
    pub fn config(&self) -> &AnthropicConfig {
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
        headers.insert("accept".into(), "application/json".into());
        headers.insert("x-api-key".into(), api_key);
        headers.insert("anthropic-version".into(), self.config.version.clone());
        for (name, value) in &self.config.headers {
            set_header(&mut headers, name, Some(value));
        }
        // 请求级 headers：`Some` 覆盖，`None` 抑制同名默认头（pi `ProviderHeaders`）。
        for (name, value) in &request.options.base.headers {
            set_header(&mut headers, name, value.as_deref());
        }

        let http_request = HttpRequest {
            method: "POST".into(),
            url: format!("{}/v1/messages", self.config.base_url.trim_end_matches('/')),
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
                "Anthropic API error ({status}): {}",
                truncate(text.trim(), 500)
            );
            // 可重试的状态交给上层 retry；其它情况按 pi 的契约编码进流。
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

impl<T: HttpTransport> Service<StreamRequest> for AnthropicProvider<T> {
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

/// 内容块在流里的类型（pi 的实现用一个带 `partialJson` 的临时字段）。
#[derive(Debug, Clone)]
enum BlockState {
    Text(String),
    Thinking {
        thinking: String,
        signature: String,
        redacted: bool,
    },
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
            Self::Thinking {
                thinking,
                signature,
                redacted,
            } => ContentBlock::Thinking(ThinkingContent {
                thinking: thinking.clone(),
                thinking_signature: (!signature.is_empty()).then(|| signature.clone()),
                redacted: redacted.then_some(true),
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
    /// Anthropic 的 content block 下标 → 我们的下标。
    index_map: BTreeMap<u64, usize>,
    queue: VecDeque<AssistantMessageEvent>,
    stream_ended: bool,
    failed: Option<String>,
}

/// 把 Anthropic 的 SSE 响应流映射成 [`AssistantMessageEvent`] 流。
fn map_response_stream(model: Model, body: ByteStream) -> AssistantEventStream {
    let state = StreamState {
        body,
        sse: crate::sse::SseDecoder::default(),
        output: empty_message(&model),
        model,
        blocks: Vec::new(),
        index_map: BTreeMap::new(),
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
                // 错误是最后一个事件：不再继续读 body（否则会在结束时再报一次）。
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
                    let text = String::from_utf8_lossy(&chunk);
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
        if event.event.as_deref() == Some("error") {
            let message = error_message_from_json(&event.data).unwrap_or_else(|| {
                format!("Anthropic stream error: {}", truncate(&event.data, 500))
            });
            self.failed = Some(message);
            return;
        }
        let Ok(payload) = serde_json::from_str::<JsonValue>(&event.data) else {
            return;
        };
        let kind = payload
            .get("type")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        match kind {
            "message_start" => self.on_message_start(&payload),
            "content_block_start" => self.on_block_start(&payload),
            "content_block_delta" => self.on_block_delta(&payload),
            "content_block_stop" => self.on_block_stop(&payload),
            "message_delta" => self.on_message_delta(&payload),
            _ => {}
        }
    }

    fn on_message_start(&mut self, payload: &JsonValue) {
        let message = &payload["message"];
        if let Some(id) = message.get("id").and_then(JsonValue::as_str) {
            self.output.response_id = Some(id.to_string());
        }
        if let Some(response_model) = message.get("model").and_then(JsonValue::as_str)
            && response_model != self.model.id
        {
            self.output.response_model = Some(response_model.to_string());
        }
        let usage = &message["usage"];
        self.output.usage.input = usage
            .get("input_tokens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        self.output.usage.output = usage
            .get("output_tokens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        self.output.usage.cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        self.output.usage.cache_write = usage
            .get("cache_creation_input_tokens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        self.output.usage.cache_write_1h = usage
            .get("cache_creation")
            .and_then(|value| value.get("ephemeral_1h_input_tokens"))
            .and_then(JsonValue::as_u64);
        self.update_totals();
        self.queue.push_back(AssistantMessageEvent::Start {
            partial: self.output.clone(),
        });
    }

    fn on_block_start(&mut self, payload: &JsonValue) {
        let index = payload
            .get("index")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let block = &payload["content_block"];
        let state = match block.get("type").and_then(JsonValue::as_str) {
            Some("text") => BlockState::Text(
                block
                    .get("text")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
            Some("thinking") => BlockState::Thinking {
                thinking: block
                    .get("thinking")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
                signature: block
                    .get("signature")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
                redacted: false,
            },
            Some("redacted_thinking") => BlockState::Thinking {
                thinking: "[Reasoning redacted]".into(),
                signature: block
                    .get("data")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
                redacted: true,
            },
            Some("tool_use") => BlockState::Tool {
                call: ToolCall {
                    id: block
                        .get("id")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    arguments: block
                        .get("input")
                        .and_then(JsonValue::as_object)
                        .cloned()
                        .unwrap_or_default()
                        .into(),
                    thought_signature: None,
                    namespace: None,
                },
                partial_json: String::new(),
            },
            _ => return,
        };
        let position = self.blocks.len();
        self.blocks.push(state);
        self.index_map.insert(index, position);
        self.sync_content();
        let partial = self.output.clone();
        let event = match &self.blocks[position] {
            BlockState::Text(_) => AssistantMessageEvent::TextStart {
                content_index: position,
                partial,
            },
            BlockState::Thinking { .. } => AssistantMessageEvent::ThinkingStart {
                content_index: position,
                partial,
            },
            BlockState::Tool { .. } => AssistantMessageEvent::ToolCallStart {
                content_index: position,
                partial,
            },
        };
        self.queue.push_back(event);
    }

    fn on_block_delta(&mut self, payload: &JsonValue) {
        let index = payload
            .get("index")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let Some(position) = self.index_map.get(&index).copied() else {
            return;
        };
        let delta = &payload["delta"];
        let kind = delta
            .get("type")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        match (&mut self.blocks[position], kind) {
            (BlockState::Text(text), "text_delta") => {
                let piece = delta
                    .get("text")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default();
                text.push_str(piece);
                self.sync_content();
                self.queue.push_back(AssistantMessageEvent::TextDelta {
                    content_index: position,
                    delta: piece.to_string(),
                    partial: self.output.clone(),
                });
            }
            (BlockState::Thinking { thinking, .. }, "thinking_delta") => {
                let piece = delta
                    .get("thinking")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default();
                thinking.push_str(piece);
                self.sync_content();
                self.queue.push_back(AssistantMessageEvent::ThinkingDelta {
                    content_index: position,
                    delta: piece.to_string(),
                    partial: self.output.clone(),
                });
            }
            (BlockState::Thinking { signature, .. }, "signature_delta") => {
                signature.push_str(
                    delta
                        .get("signature")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default(),
                );
                self.sync_content();
            }
            (BlockState::Tool { call, partial_json }, "input_json_delta") => {
                let piece = delta
                    .get("partial_json")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default();
                partial_json.push_str(piece);
                if let Ok(JsonValue::Object(parsed)) =
                    serde_json::from_str::<JsonValue>(partial_json)
                {
                    call.arguments = parsed.into();
                }
                self.sync_content();
                self.queue.push_back(AssistantMessageEvent::ToolCallDelta {
                    content_index: position,
                    delta: piece.to_string(),
                    partial: self.output.clone(),
                });
            }
            _ => {}
        }
    }

    fn on_block_stop(&mut self, payload: &JsonValue) {
        let index = payload
            .get("index")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let Some(position) = self.index_map.get(&index).copied() else {
            return;
        };
        self.sync_content();
        let partial = self.output.clone();
        let event = match &self.blocks[position] {
            BlockState::Text(text) => AssistantMessageEvent::TextEnd {
                content_index: position,
                content: text.clone(),
                partial,
            },
            BlockState::Thinking { thinking, .. } => AssistantMessageEvent::ThinkingEnd {
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

    fn on_message_delta(&mut self, payload: &JsonValue) {
        let delta = &payload["delta"];
        if let Some(reason) = delta.get("stop_reason").and_then(JsonValue::as_str) {
            self.output.raw_stop_reason = Some(reason.to_string());
            match map_stop_reason(reason, delta.get("stop_details")) {
                Ok(stop_reason) => self.output.stop_reason = stop_reason,
                Err(message) => self.failed = Some(message),
            }
        }
        if let Some(usage) = payload.get("usage") {
            if let Some(value) = usage.get("input_tokens").and_then(JsonValue::as_u64) {
                self.output.usage.input = value;
            }
            if let Some(value) = usage.get("output_tokens").and_then(JsonValue::as_u64) {
                self.output.usage.output = value;
            }
            if let Some(value) = usage
                .get("cache_read_input_tokens")
                .and_then(JsonValue::as_u64)
            {
                self.output.usage.cache_read = value;
            }
            if let Some(value) = usage
                .get("cache_creation_input_tokens")
                .and_then(JsonValue::as_u64)
            {
                self.output.usage.cache_write = value;
            }
            if let Some(value) = usage
                .get("output_tokens_details")
                .and_then(|value| value.get("thinking_tokens"))
                .and_then(JsonValue::as_u64)
            {
                self.output.usage.reasoning = Some(value);
            }
        }
        self.update_totals();
    }

    fn update_totals(&mut self) {
        update_usage_totals(&self.model, &mut self.output.usage);
    }

    /// 流结束后收尾：没有 stop reason 视为错误；错误/取消编码成 `error` 事件。
    fn finish_stream(&mut self) {
        self.sync_content();
        if self.failed.is_some() {
            return;
        }
        match self.output.stop_reason {
            StopReason::Pending => {
                self.failed = Some("Anthropic stream ended without a stop reason".into());
            }
            StopReason::Error | StopReason::Aborted => {
                self.failed = Some(
                    self.output
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "An unknown error occurred".into()),
                );
            }
            _ => {
                let reason = match self.output.stop_reason {
                    StopReason::Stop => DoneReason::Stop,
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

fn error_message_from_json(data: &str) -> Option<String> {
    let payload: JsonValue = serde_json::from_str(data).ok()?;
    let error = payload.get("error").unwrap_or(&payload);
    let kind = error.get("type").and_then(JsonValue::as_str);
    let message = error.get("message").and_then(JsonValue::as_str)?;
    Some(match kind {
        Some(kind) => format!("{kind}: {message}"),
        None => message.to_string(),
    })
}

/// Anthropic `stop_reason` → pi [`StopReason`]（pi `mapStopReason`）。
fn map_stop_reason(reason: &str, details: Option<&JsonValue>) -> Result<StopReason, String> {
    Ok(match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => StopReason::Stop,
        "max_tokens" => StopReason::Length,
        "tool_use" => StopReason::ToolUse,
        "refusal" => {
            let explanation = details
                .and_then(|value| value.get("explanation"))
                .and_then(JsonValue::as_str);
            return Err(explanation
                .map(str::to_string)
                .unwrap_or_else(|| "The model refused to complete the request".into()));
        }
        "sensitive" => return Err("Provider stopped with: sensitive".into()),
        other => return Err(format!("Unhandled stop reason: {other}")),
    })
}

// ---------- 请求构造 ----------

fn thinking_budget(level: ThinkingLevel, budgets: Option<&crate::ThinkingBudgets>) -> u64 {
    let configured = budgets.and_then(|budgets| match level {
        ThinkingLevel::Minimal => budgets.minimal,
        ThinkingLevel::Low => budgets.low,
        ThinkingLevel::Medium => budgets.medium,
        ThinkingLevel::High => budgets.high,
        ThinkingLevel::XHigh => budgets.xhigh,
        ThinkingLevel::Max => budgets.max,
    });
    configured.unwrap_or(match level {
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 8192,
        ThinkingLevel::High => 16_384,
        ThinkingLevel::XHigh => 32_768,
        ThinkingLevel::Max => 65_536,
    })
}

fn build_request_body(request: &StreamRequest) -> Result<JsonValue, PiError> {
    let (system, tools) = system_and_tools(request.context.messages());
    let messages = convert_messages(request.context.messages())?;

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
    });
    if !system.is_empty() {
        body["system"] = json!([{ "type": "text", "text": system }]);
    }
    if !tools.is_empty() {
        body["tools"] = JsonValue::Array(
            tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    })
                })
                .collect(),
        );
    }

    // 思考与 temperature 互斥（pi 同样处理）。
    let thinking = match request.options.reasoning {
        Some(level) if request.model.reasoning => Some(thinking_budget(
            level,
            request.options.thinking_budgets.as_ref(),
        )),
        _ => None,
    };
    if let Some(budget) = thinking {
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    } else if let Some(temperature) = request.options.base.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(session_id) = &request.options.base.session_id {
        body["metadata"] = json!({ "user_id": session_id });
    }
    Ok(body)
}

/// transcript → Anthropic messages。
fn convert_messages(messages: &[Message]) -> Result<Vec<JsonValue>, PiError> {
    let mut converted: Vec<JsonValue> = Vec::new();
    for message in messages {
        match message {
            Message::System(_) => {
                // system prompt 由 `system` 字段承载。
            }
            Message::User(user) => converted.push(json!({
                "role": "user",
                "content": match &user.content {
                    UserContent::Text(text) => vec![json!({ "type": "text", "text": text })],
                    UserContent::Blocks(blocks) => convert_blocks(blocks)?,
                },
            })),
            Message::Assistant(assistant) => {
                let content = convert_blocks(&assistant.content)?;
                if !content.is_empty() {
                    converted.push(json!({ "role": "assistant", "content": content }));
                }
            }
            Message::ToolResult(result) => {
                let block = tool_result_block(result)?;
                // 连续的 tool_result 合成同一条 user 消息（Anthropic 要求）。
                match converted.last_mut() {
                    Some(JsonValue::Object(last))
                        if last.get("role").and_then(JsonValue::as_str) == Some("user")
                            && last
                                .get("content")
                                .and_then(JsonValue::as_array)
                                .is_some_and(|content| {
                                    content.iter().all(|block| {
                                        block.get("type").and_then(JsonValue::as_str)
                                            == Some("tool_result")
                                    })
                                }) =>
                    {
                        if let Some(JsonValue::Array(content)) = last.get_mut("content") {
                            content.push(block);
                        }
                    }
                    _ => converted.push(json!({ "role": "user", "content": [block] })),
                }
            }
        }
    }
    Ok(converted)
}

fn convert_blocks(blocks: &[ContentBlock]) -> Result<Vec<JsonValue>, PiError> {
    let mut converted = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                converted.push(json!({ "type": "text", "text": text.text }));
            }
            ContentBlock::Thinking(thinking) => converted.push(json!({
                "type": "thinking",
                "thinking": thinking.thinking,
                "signature": thinking.thinking_signature.clone().unwrap_or_default(),
            })),
            ContentBlock::Image(image) => converted.push(image_block(image)),
            ContentBlock::ToolCall(call) => converted.push(json!({
                "type": "tool_use",
                "id": call.id,
                "name": call.name,
                "input": JsonValue::Object(call.arguments.as_object().clone()),
            })),
        }
    }
    Ok(converted)
}

fn image_block(image: &ImageContent) -> JsonValue {
    json!({
        "type": "image",
        "source": { "type": "base64", "media_type": image.mime_type, "data": image.data },
    })
}

fn tool_result_block(result: &ToolResultMessage) -> Result<JsonValue, PiError> {
    let content: Vec<JsonValue> = result
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => Ok(json!({ "type": "text", "text": text.text })),
            ContentBlock::Image(image) => Ok(image_block(image)),
            other => Err(PiError::Serialization(format!(
                "tool result cannot contain {other:?}"
            ))),
        })
        .collect::<Result<_, _>>()?;
    Ok(json!({
        "type": "tool_result",
        "tool_use_id": result.tool_call_id,
        "content": content,
        "is_error": result.is_error,
    }))
}

/// 便捷：把 provider 装箱成 [`crate::BoxStreamService`]（与 `StreamFn` 同形）。
pub fn boxed<T: HttpTransport>(provider: AnthropicProvider<T>) -> crate::BoxStreamService
where
    <AnthropicProvider<T> as Service<StreamRequest>>::Future: Send + 'static,
{
    tower::util::BoxCloneService::new(provider)
}

impl<T: HttpTransport> AnthropicProvider<T> {
    /// 便利方法：直接按 `StreamRequest` 取流（等价于 `call`）。
    pub async fn stream(&self, request: StreamRequest) -> Result<AssistantEventStream, PiError> {
        self.clone().oneshot(request).await
    }
}
