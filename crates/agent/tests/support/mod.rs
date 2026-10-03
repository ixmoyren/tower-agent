//! agent 集成测试的共享脚本化替身。
//!
//! 这些替身对应 pi 侧的 faux provider 与 harness：provider 流是固定的金标准事件
//! 序列，工具是确定性结果 + 可控闸门，便于断言事件顺序与并发语义。
#![allow(dead_code)]

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use agent::{
    AgentError, AgentEvent, AgentMessage, AgentTool, AgentToolResult, ToolCallRequest,
    ToolResultStream,
    agent_loop::{AgentContext, BoxFut, EventSink},
};
use ai::{
    AssistantEventStream, AssistantMessage, AssistantMessageEvent, BoxStreamService, ContentBlock,
    DoneReason, JsonObject, JsonValue, Message, Model, PiError, StopReason, StreamRequest,
    TextContent, ToolCall, ToolResultMessage, Usage,
};
use futures_util::{future::BoxFuture, stream};
use tokio::sync::Semaphore;
use tower::Service;

/// 一个通用的测试模型。
pub fn test_model() -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: "test-api".into(),
        provider: "test-provider".into(),
        ..Model::default()
    }
}

/// 构造 assistant 消息。
pub fn assistant(
    model: &Model,
    content: Vec<ContentBlock>,
    stop_reason: StopReason,
) -> AssistantMessage {
    AssistantMessage {
        content,
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

/// 文本内容块。
pub fn text(value: impl Into<String>) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: value.into(),
        text_signature: None,
    })
}

/// 工具调用内容块。
pub fn tool_call(id: &str, name: &str, arguments: JsonObject) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        thought_signature: None,
        namespace: None,
    })
}

/// 用户消息。
pub fn user(text_value: impl Into<String>) -> AgentMessage {
    AgentMessage::Message(Message::User(ai::UserMessage {
        content: ai::UserContent::Text(text_value.into()),
        timestamp: 0,
    }))
}

/// 完整的文本流脚本：`start → text_start → text_delta → text_end → done`。
pub fn text_script(model: &Model, value: &str) -> Vec<AssistantMessageEvent> {
    let mut partial = assistant(model, Vec::new(), StopReason::Pending);
    let mut events = vec![AssistantMessageEvent::Start {
        partial: partial.clone(),
    }];
    partial.content.push(text(""));
    events.push(AssistantMessageEvent::TextStart {
        content_index: 0,
        partial: partial.clone(),
    });
    partial.content[0] = text(value);
    events.push(AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: value.into(),
        partial: partial.clone(),
    });
    events.push(AssistantMessageEvent::TextEnd {
        content_index: 0,
        content: value.into(),
        partial: partial.clone(),
    });
    let final_message = assistant(model, vec![text(value)], StopReason::Stop);
    events.push(AssistantMessageEvent::Done {
        reason: DoneReason::Stop,
        message: final_message,
    });
    events
}

/// 只以 `error` 结束的脚本（对应建立请求失败）。
pub fn error_script(model: &Model, message: &str) -> Vec<AssistantMessageEvent> {
    let mut error = assistant(model, Vec::new(), StopReason::Error);
    error.error_message = Some(message.into());
    vec![AssistantMessageEvent::Error {
        reason: ai::ErrorReason::Error,
        error,
    }]
}

/// 固定脚本的 provider 流服务；超出脚本数量时重复最后一段。
#[derive(Clone)]
pub struct ScriptedStreamService {
    scripts: Arc<Vec<Vec<AssistantMessageEvent>>>,
    calls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<StreamRequest>>>,
    gate: Option<Arc<Semaphore>>,
}

impl ScriptedStreamService {
    /// 用一串脚本构造。
    pub fn new(scripts: Vec<Vec<AssistantMessageEvent>>) -> Self {
        Self {
            scripts: Arc::new(scripts),
            calls: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
            gate: None,
        }
    }

    /// 在建立 provider 流之前先等待该信号量放行。
    #[must_use]
    pub fn gated(mut self, gate: Arc<Semaphore>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// 已收到的请求。
    pub fn requests(&self) -> Vec<StreamRequest> {
        self.requests.lock().expect("requests mutex").clone()
    }

    /// 装箱成 provider 流服务。
    pub fn boxed(self) -> BoxStreamService {
        use tower::util::BoxCloneService;
        BoxCloneService::new(self)
    }
}

impl Service<StreamRequest> for ScriptedStreamService {
    type Response = AssistantEventStream;
    type Error = PiError;
    type Future = BoxFuture<'static, Result<AssistantEventStream, PiError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: StreamRequest) -> Self::Future {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let script = self
            .scripts
            .get(index)
            .or_else(|| self.scripts.last())
            .cloned()
            .unwrap_or_default();
        self.requests.lock().expect("requests mutex").push(request);
        let gate = self.gate.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                let _permit = gate.acquire().await;
            }
            Ok(AssistantEventStream::new(stream::iter(script)))
        })
    }
}

/// 固定结果的工具服务；超出调用次数时重复最后一个结果序列。
#[derive(Clone)]
pub struct ScriptedTool {
    results: Vec<Vec<AgentToolResult>>,
    calls: Arc<AtomicUsize>,
    gate: Option<Arc<Semaphore>>,
}

impl ScriptedTool {
    /// 每次调用返回一个终态结果。
    pub fn single(result: AgentToolResult) -> Self {
        Self {
            results: vec![vec![result]],
            calls: Arc::new(AtomicUsize::new(0)),
            gate: None,
        }
    }

    /// 每次调用返回「部分结果 + 终态结果」。
    pub fn with_updates(partials: Vec<AgentToolResult>, result: AgentToolResult) -> Self {
        let mut results = partials;
        results.push(result);
        Self {
            results: vec![results],
            calls: Arc::new(AtomicUsize::new(0)),
            gate: None,
        }
    }

    /// 用每次调用各自的序列构造。
    pub fn sequence(results: Vec<Vec<AgentToolResult>>) -> Self {
        Self {
            results,
            calls: Arc::new(AtomicUsize::new(0)),
            gate: None,
        }
    }

    /// 在被调用前先等待该信号量放行。
    #[must_use]
    pub fn gated(mut self, gate: Arc<Semaphore>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// 已被调用的次数。
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Service<ToolCallRequest> for ScriptedTool {
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: ToolCallRequest) -> Self::Future {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let results = self
            .results
            .get(index)
            .or_else(|| self.results.last())
            .cloned()
            .unwrap_or_default();
        let gate = self.gate.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                let _permit = gate.acquire().await;
            }
            Ok(ToolResultStream::new(stream::iter(results)))
        })
    }
}

/// 收集事件、并可在每个事件上运行副作用的测试下沉点。
type EventHook = Arc<dyn Fn(&AgentEvent) + Send + Sync>;

#[derive(Clone)]
pub struct TestSink {
    events: Arc<Mutex<Vec<AgentEvent>>>,
    hook: Option<EventHook>,
}

impl Default for TestSink {
    fn default() -> Self {
        Self::new()
    }
}

impl TestSink {
    /// 只收集事件。
    pub fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            hook: None,
        }
    }

    /// 每个事件都会触发一次同步钩子（用于在事件点放行闸门）。
    pub fn with_hook(mut self, hook: EventHook) -> Self {
        self.hook = Some(hook);
        self
    }

    /// 已收集的事件。
    pub fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().expect("events mutex").clone()
    }

    /// 已收集事件的 wire `type` 序列。
    pub fn kinds(&self) -> Vec<&'static str> {
        self.events().iter().map(AgentEvent::kind).collect()
    }
}

impl EventSink for TestSink {
    fn emit(&self, event: AgentEvent) -> BoxFut<()> {
        if let Some(hook) = &self.hook {
            hook(&event);
        }
        self.events.lock().expect("events mutex").push(event);
        Box::pin(std::future::ready(()))
    }
}

/// 收集一个 `ToolResultStream` 的全部条目。
pub async fn collect_tool_stream(mut stream: ToolResultStream) -> Vec<AgentToolResult> {
    use futures_util::StreamExt;
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    items
}

/// 把一条工具调用注册进工具集。
pub fn agent_tool(name: &str, tool: ScriptedTool) -> AgentTool {
    AgentTool::new(name, tool)
}

/// 空 JSON 对象。
pub fn empty_args() -> JsonObject {
    JsonObject::new()
}

/// JSON 对象便捷构造。
pub fn args(value: JsonValue) -> JsonObject {
    match value {
        JsonValue::Object(map) => map,
        other => {
            let mut map = JsonObject::new();
            map.insert("value".into(), other);
            map
        }
    }
}

/// 便捷：把服务装箱成 provider 流服务。
pub fn boxed_stream<S>(service: S) -> BoxStreamService
where
    S: Service<StreamRequest, Response = AssistantEventStream, Error = PiError>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    use tower::util::BoxCloneService;
    BoxCloneService::new(service)
}

/// 工具结果消息的文本视图。
pub fn result_text(message: &ToolResultMessage) -> Option<String> {
    message.content.iter().find_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    })
}

/// 构造 agent 上下文。
pub fn context(messages: Vec<AgentMessage>, tools: Vec<AgentTool>) -> AgentContext {
    AgentContext { messages, tools }
}
