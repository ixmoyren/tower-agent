//! OpenAI Responses provider（对应 pi `src/api/openai-responses*.ts`）。
//!
//! 这是 OpenAI 的**事件类型化**流式协议，和 `chat/completions` 的分片 delta 是两套东西：
//! 每个 SSE 帧自带 `type`（`response.output_text.delta`、`response.function_call_arguments.delta`、
//! `response.output_item.done`、`response.completed` …），并且要按 `output_index` 把 deltas
//! 归并到对应的输出项上。请求体也是新的：`input` 为类型化 item 列表（`function_call` /
//! `function_call_output` 与消息平级），`store: false`，输出上限走 `max_output_tokens`。

use std::collections::{BTreeMap, VecDeque};

use futures_util::{StreamExt as _, future::BoxFuture, stream};
use serde_json::{Value as JsonValue, json};
use tower::{Service, ServiceExt as _};

use crate::{
    AssistantEventStream, AssistantMessage, AssistantMessageEvent, ContentBlock, DoneReason,
    ErrorReason, Message, Model, PiError, StopReason, StreamRequest, TextContent, ThinkingContent,
    ToolCall, UserContent,
    http::{ByteStream, CallContext, HttpHeaders, HttpRequest, HttpTransport},
    provider::common::{
        content_text, empty_message, error_stream, openai_image_part, retryable_status, set_header,
        system_and_tools, truncate, update_usage_totals,
    },
};

/// pi 里这条 provider 的 api 标识。
pub const API: &str = "openai-responses";
/// 默认 API base URL（含 `/v1`）。
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// Responses 拒绝小于 16 的输出上限（pi `OPENAI_RESPONSES_MIN_OUTPUT_TOKENS`）。
pub const MIN_OUTPUT_TOKENS: u32 = 16;

/// provider 配置。
#[derive(Debug, Clone)]
pub struct OpenAiResponsesConfig {
    /// API base URL（不含 `/responses`）。
    pub base_url: String,
    /// 额外请求头（同名覆盖）。
    pub headers: HttpHeaders,
    /// 组织标识。
    pub organization: Option<String>,
}

impl Default for OpenAiResponsesConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            headers: HttpHeaders::new(),
            organization: None,
        }
    }
}

/// OpenAI Responses provider。
#[derive(Clone)]
pub struct OpenAiResponsesProvider<T: HttpTransport> {
    transport: T,
    api_key: Option<String>,
    config: OpenAiResponsesConfig,
}

impl<T: HttpTransport> OpenAiResponsesProvider<T> {
    /// 用给定传输层构造。
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            api_key: None,
            config: OpenAiResponsesConfig::default(),
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
    pub fn config(&self) -> &OpenAiResponsesConfig {
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
            url: format!("{}/responses", self.config.base_url.trim_end_matches('/')),
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
                "OpenAI Responses error ({status}): {}",
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

impl<T: HttpTransport> Service<StreamRequest> for OpenAiResponsesProvider<T> {
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
enum SlotKind {
    Text,
    Thinking,
    Tool,
}

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

/// 一个输出项（按 `output_index` 归并它的 deltas）。
#[derive(Debug, Clone)]
struct Slot {
    content_index: usize,
    kind: SlotKind,
}

struct StreamState {
    body: ByteStream,
    sse: crate::sse::SseDecoder,
    model: Model,
    output: AssistantMessage,
    blocks: Vec<BlockState>,
    slots: BTreeMap<u64, Slot>,
    queue: VecDeque<AssistantMessageEvent>,
    saw_terminal: bool,
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
        slots: BTreeMap::new(),
        queue: VecDeque::new(),
        saw_terminal: false,
        stream_ended: false,
        failed: None,
    };
    AssistantEventStream::new(stream::unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.queue.pop_front() {
                return Some((event, state));
            }
            if let Some(message) = state.failed.take() {
                state.close_all_slots();
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
        let kind = payload
            .get("type")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        match kind {
            "response.created" => {
                if let Some(id) = payload
                    .get("response")
                    .and_then(|response| response.get("id"))
                    .and_then(JsonValue::as_str)
                {
                    self.output.response_id = Some(id.to_string());
                }
            }
            "response.output_item.added" => self.open_slot(&payload),
            "response.output_text.delta" | "response.refusal.delta" => {
                self.push_text_delta(&payload, kind)
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.push_thinking_delta(&payload)
            }
            "response.reasoning_summary_part.done" => {
                self.push_thinking_delta_text(&payload, "\n\n")
            }
            "response.function_call_arguments.delta" => self.push_tool_delta(&payload),
            "response.function_call_arguments.done" => self.finish_tool_arguments(&payload),
            "response.output_item.done" => self.close_slot(&payload),
            "response.completed" | "response.incomplete" => self.on_terminal(&payload, kind),
            "response.failed" => {
                self.saw_terminal = true;
                let response = payload.get("response").cloned().unwrap_or(JsonValue::Null);
                let message = response
                    .get("error")
                    .map(|error| {
                        let code = error
                            .get("code")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("unknown");
                        let message = error
                            .get("message")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("no message");
                        format!("{code}: {message}")
                    })
                    .or_else(|| {
                        response
                            .get("incomplete_details")
                            .and_then(|details| details.get("reason"))
                            .and_then(JsonValue::as_str)
                            .map(|reason| format!("incomplete: {reason}"))
                    })
                    .unwrap_or_else(|| "Unknown error (no error details in response)".into());
                self.failed = Some(message);
            }
            "error" => {
                let code = payload
                    .get("code")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("unknown");
                let message = payload
                    .get("message")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("Unknown error");
                self.failed = Some(format!("Error Code {code}: {message}"));
            }
            _ => {}
        }
    }

    fn open_slot(&mut self, payload: &JsonValue) {
        let index = payload
            .get("output_index")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let item = payload.get("item").cloned().unwrap_or(JsonValue::Null);
        let item_type = item
            .get("type")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        let (block, kind, event) = match item_type {
            "reasoning" => {
                self.blocks.push(BlockState::Thinking(String::new()));
                let position = self.blocks.len() - 1;
                self.sync_content();
                let partial = self.output.clone();
                (
                    position,
                    SlotKind::Thinking,
                    AssistantMessageEvent::ThinkingStart {
                        content_index: position,
                        partial,
                    },
                )
            }
            "function_call" | "custom_tool_call" => {
                let call = ToolCall {
                    id: item
                        .get("call_id")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: item
                        .get("name")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    arguments: Default::default(),
                    thought_signature: None,
                    namespace: None,
                };
                self.blocks.push(BlockState::Tool {
                    call,
                    partial_json: String::new(),
                });
                let position = self.blocks.len() - 1;
                self.sync_content();
                let partial = self.output.clone();
                (
                    position,
                    SlotKind::Tool,
                    AssistantMessageEvent::ToolCallStart {
                        content_index: position,
                        partial,
                    },
                )
            }
            // 默认按消息（文本）处理。
            _ => {
                self.blocks.push(BlockState::Text(String::new()));
                let position = self.blocks.len() - 1;
                self.sync_content();
                let partial = self.output.clone();
                (
                    position,
                    SlotKind::Text,
                    AssistantMessageEvent::TextStart {
                        content_index: position,
                        partial,
                    },
                )
            }
        };
        self.slots.insert(
            index,
            Slot {
                content_index: block,
                kind,
            },
        );
        self.queue.push_back(event);
    }

    fn slot(&self, payload: &JsonValue, expected: SlotKind) -> Option<Slot> {
        let index = payload
            .get("output_index")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        self.slots
            .get(&index)
            .filter(|slot| slot.kind == expected)
            .cloned()
    }

    fn push_text_delta(&mut self, payload: &JsonValue, _kind: &str) {
        let Some(slot) = self.slot(payload, SlotKind::Text) else {
            return;
        };
        let delta = payload
            .get("delta")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        if let Some(BlockState::Text(text)) = self.blocks.get_mut(slot.content_index) {
            text.push_str(delta);
        }
        self.sync_content();
        self.queue.push_back(AssistantMessageEvent::TextDelta {
            content_index: slot.content_index,
            delta: delta.to_string(),
            partial: self.output.clone(),
        });
    }

    fn push_thinking_delta(&mut self, payload: &JsonValue) {
        let delta = payload
            .get("delta")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        self.push_thinking_delta_text(payload, delta);
    }

    fn push_thinking_delta_text(&mut self, payload: &JsonValue, delta: &str) {
        let Some(slot) = self.slot(payload, SlotKind::Thinking) else {
            return;
        };
        if let Some(BlockState::Thinking(thinking)) = self.blocks.get_mut(slot.content_index) {
            thinking.push_str(delta);
        }
        self.sync_content();
        self.queue.push_back(AssistantMessageEvent::ThinkingDelta {
            content_index: slot.content_index,
            delta: delta.to_string(),
            partial: self.output.clone(),
        });
    }

    fn push_tool_delta(&mut self, payload: &JsonValue) {
        let Some(slot) = self.slot(payload, SlotKind::Tool) else {
            return;
        };
        let delta = payload
            .get("delta")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        self.append_tool_arguments(slot.content_index, delta);
        self.queue.push_back(AssistantMessageEvent::ToolCallDelta {
            content_index: slot.content_index,
            delta: delta.to_string(),
            partial: self.output.clone(),
        });
    }

    /// `response.function_call_arguments.done` 带完整参数：补发还没发过的尾巴。
    fn finish_tool_arguments(&mut self, payload: &JsonValue) {
        let Some(slot) = self.slot(payload, SlotKind::Tool) else {
            return;
        };
        let arguments = payload
            .get("arguments")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        let previous = match self.blocks.get(slot.content_index) {
            Some(BlockState::Tool { partial_json, .. }) => partial_json.clone(),
            _ => return,
        };
        if let Some(rest) = arguments.strip_prefix(&previous)
            && !rest.is_empty()
        {
            self.append_tool_arguments(slot.content_index, rest);
            self.queue.push_back(AssistantMessageEvent::ToolCallDelta {
                content_index: slot.content_index,
                delta: rest.to_string(),
                partial: self.output.clone(),
            });
        }
    }

    fn append_tool_arguments(&mut self, position: usize, delta: &str) {
        if let Some(BlockState::Tool { call, partial_json }) = self.blocks.get_mut(position) {
            partial_json.push_str(delta);
            if let Ok(JsonValue::Object(parsed)) = serde_json::from_str::<JsonValue>(partial_json) {
                call.arguments = parsed;
            }
        }
        self.sync_content();
    }

    fn close_slot(&mut self, payload: &JsonValue) {
        let index = payload
            .get("output_index")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let Some(slot) = self.slots.remove(&index) else {
            return;
        };
        let item = payload.get("item").cloned().unwrap_or(JsonValue::Null);
        self.sync_content();
        let partial = self.output.clone();
        let event = match (&self.blocks[slot.content_index], slot.kind) {
            (BlockState::Text(_), _) => {
                // 用终态 item 覆盖文本（pi 在 output_item.done 时以 item 内容为准）。
                let text = item
                    .get("content")
                    .and_then(JsonValue::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .map(|part| {
                                part.get("text")
                                    .or_else(|| part.get("refusal"))
                                    .and_then(JsonValue::as_str)
                                    .unwrap_or_default()
                            })
                            .collect::<String>()
                    })
                    .filter(|text| !text.is_empty());
                if let Some(text) = text
                    && let Some(BlockState::Text(current)) = self.blocks.get_mut(slot.content_index)
                {
                    *current = text;
                }
                let content = match &self.blocks[slot.content_index] {
                    BlockState::Text(text) => text.clone(),
                    _ => String::new(),
                };
                AssistantMessageEvent::TextEnd {
                    content_index: slot.content_index,
                    content,
                    partial,
                }
            }
            (BlockState::Thinking(_), _) => {
                let summary = item
                    .get("summary")
                    .and_then(JsonValue::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .map(|part| {
                                part.get("text")
                                    .and_then(JsonValue::as_str)
                                    .unwrap_or_default()
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .filter(|text| !text.is_empty());
                let content = item
                    .get("content")
                    .and_then(JsonValue::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .map(|part| {
                                part.get("text")
                                    .and_then(JsonValue::as_str)
                                    .unwrap_or_default()
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .filter(|text| !text.is_empty());
                if let Some(text) = summary.or(content)
                    && let Some(BlockState::Thinking(current)) =
                        self.blocks.get_mut(slot.content_index)
                {
                    *current = text;
                }
                let content = match &self.blocks[slot.content_index] {
                    BlockState::Thinking(thinking) => thinking.clone(),
                    _ => String::new(),
                };
                AssistantMessageEvent::ThinkingEnd {
                    content_index: slot.content_index,
                    content,
                    partial,
                }
            }
            (BlockState::Tool { .. }, _) => {
                if let (Some(arguments), Some(BlockState::Tool { call, partial_json })) = (
                    item.get("arguments").and_then(JsonValue::as_str),
                    self.blocks.get_mut(slot.content_index),
                ) {
                    *partial_json = arguments.to_string();
                    if let Ok(JsonValue::Object(parsed)) =
                        serde_json::from_str::<JsonValue>(arguments)
                    {
                        call.arguments = parsed;
                    }
                }
                let call = match &self.blocks[slot.content_index] {
                    BlockState::Tool { call, .. } => call.clone(),
                    _ => return,
                };
                AssistantMessageEvent::ToolCallEnd {
                    content_index: slot.content_index,
                    tool_call: call,
                    partial,
                }
            }
        };
        self.sync_content();
        self.queue.push_back(event);
    }

    fn close_all_slots(&mut self) {
        let indices: Vec<u64> = self.slots.keys().copied().collect();
        for index in indices {
            let payload = json!({ "output_index": index, "item": JsonValue::Null });
            self.close_slot(&payload);
        }
    }

    fn on_terminal(&mut self, payload: &JsonValue, kind: &str) {
        self.saw_terminal = true;
        let response = payload.get("response").cloned().unwrap_or(JsonValue::Null);
        if let Some(usage) = response.get("usage") {
            self.apply_usage(usage);
        }
        let incomplete_reason = response
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(JsonValue::as_str);
        let has_tool_call = self
            .output
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)));
        self.output.stop_reason = if has_tool_call {
            StopReason::ToolUse
        } else {
            match (kind, incomplete_reason) {
                ("response.incomplete", Some("max_output_tokens")) => StopReason::Length,
                ("response.incomplete", _) => StopReason::Error,
                _ => StopReason::Stop,
            }
        };
        if self.output.stop_reason == StopReason::Error {
            self.failed = Some(format!(
                "incomplete: {}",
                incomplete_reason.unwrap_or("unknown reason")
            ));
        }
    }

    fn apply_usage(&mut self, usage: &JsonValue) {
        let cached = usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let input = usage
            .get("input_tokens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let reasoning = usage
            .get("output_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(JsonValue::as_u64);
        self.output.usage.input = input.saturating_sub(cached);
        self.output.usage.output = output;
        self.output.usage.cache_read = cached;
        self.output.usage.reasoning = reasoning;
        self.output.usage.total_tokens = usage
            .get("total_tokens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(input + output);
        update_usage_totals(&self.model, &mut self.output.usage);
    }

    fn finish_stream(&mut self) {
        if self.failed.is_some() {
            return;
        }
        self.close_all_slots();
        self.sync_content();
        if !self.saw_terminal {
            self.failed = Some("Responses stream ended without a terminal event".into());
            return;
        }
        match self.output.stop_reason {
            StopReason::Pending => {
                self.failed = Some("Responses stream ended without a stop reason".into());
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

fn build_request_body(request: &StreamRequest) -> Result<JsonValue, PiError> {
    let (system, tools) = system_and_tools(request.context.messages());
    let input = convert_input(&system, request.context.messages())?;
    let mut body = json!({
        "model": request.model.id,
        "input": input,
        "stream": true,
        "store": false,
    });
    if let Some(max_tokens) = request.options.base.max_tokens {
        body["max_output_tokens"] = json!(max_tokens.max(MIN_OUTPUT_TOKENS));
    } else if request.model.max_tokens > 0 {
        body["max_output_tokens"] = json!(
            (request.model.max_tokens.clamp(1, u32::MAX as u64) as u32).max(MIN_OUTPUT_TOKENS)
        );
    }
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
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    })
                })
                .collect(),
        );
        body["tool_choice"] = json!("auto");
    }
    if let Some(session_id) = &request.options.base.session_id {
        body["prompt_cache_key"] = json!(session_id);
    }
    Ok(body)
}

/// transcript → Responses 的 `input` item 列表（工具调用/结果与消息平级）。
fn convert_input(system: &str, messages: &[Message]) -> Result<Vec<JsonValue>, PiError> {
    let mut items: Vec<JsonValue> = Vec::new();
    if !system.is_empty() {
        items.push(json!({
            "type": "message",
            "role": "system",
            "content": [{ "type": "input_text", "text": system }],
        }));
    }
    for message in messages {
        match message {
            Message::System(_) => {}
            Message::User(user) => {
                let content = match &user.content {
                    UserContent::Text(text) => vec![json!({ "type": "input_text", "text": text })],
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => {
                                Some(json!({ "type": "input_text", "text": text.text }))
                            }
                            ContentBlock::Image(image) => {
                                let mut part = openai_image_part(image);
                                part["type"] = json!("input_image");
                                Some(part)
                            }
                            _ => None,
                        })
                        .collect(),
                };
                items.push(json!({ "type": "message", "role": "user", "content": content }));
            }
            Message::Assistant(assistant) => {
                let text = content_text(&assistant.content);
                if !text.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                for block in &assistant.content {
                    if let ContentBlock::ToolCall(call) = block {
                        items.push(json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": call.name,
                            "arguments": JsonValue::Object(call.arguments.clone()).to_string(),
                        }));
                    }
                }
            }
            Message::ToolResult(result) => items.push(json!({
                "type": "function_call_output",
                "call_id": result.tool_call_id,
                "output": content_text(&result.content),
            })),
        }
    }
    Ok(items)
}

/// 便利：把 provider 装箱成 [`crate::BoxStreamService`]（与 `StreamFn` 同形）。
pub fn boxed<T: HttpTransport>(provider: OpenAiResponsesProvider<T>) -> crate::BoxStreamService
where
    <OpenAiResponsesProvider<T> as Service<StreamRequest>>::Future: Send + 'static,
{
    tower::util::BoxCloneService::new(provider)
}

impl<T: HttpTransport> OpenAiResponsesProvider<T> {
    /// 便利方法：直接按 `StreamRequest` 取流（等价于 `call`）。
    pub async fn stream(&self, request: StreamRequest) -> Result<AssistantEventStream, PiError> {
        self.clone().oneshot(request).await
    }
}
