//! 有状态 agent 运行时（pi `Agent`）。
//!
//! [`Agent`] 持有当前 transcript、模型/工具配置与 steer / follow-up 队列，把低层
//! [`crate::agent_loop`] 的事件归约进状态并广播给订阅者。它 **不是** tower layer：
//! 编排循环、队列 drain 点、listener await 语义都在这里显式实现（design.md 4.2 映射表）。

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll},
};

use futures_util::future::BoxFuture;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tower::Service;
use tower_agent_provider::{
    BoxStreamService, ContentBlock, Message, Model, ModelThinkingLevel, SimpleStreamOptions,
    StopReason, TextContent, Usage, UserContent, UserMessage,
};

use crate::{
    AgentError, AgentEvent, AgentMessage, AgentRequest, AgentResponse, AgentTool,
    ToolExecutionMode,
    agent_loop::{
        self, AfterToolCall, AgentContext, BeforeToolCall, BoxFut, ConvertToLlm, EventSink,
        FinishTurn, LoopConfig, TransformContext, default_convert_to_llm,
    },
    event_bus::{EventBus, Subscription},
};

/// 队列 drain 模式（pi `QueueMode`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    /// 在每个 drain 点注入全部排队消息。
    All,
    /// 每个 drain 点只注入最旧的一条（默认）。
    #[default]
    OneAtATime,
}

/// 待注入消息队列（pi `PendingMessageQueue`）。
#[derive(Debug, Clone)]
struct PendingMessageQueue {
    messages: Vec<AgentMessage>,
    mode: QueueMode,
}

impl PendingMessageQueue {
    fn new(mode: QueueMode) -> Self {
        Self {
            messages: Vec::new(),
            mode,
        }
    }

    fn enqueue(&mut self, message: AgentMessage) {
        self.messages.push(message);
    }

    fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }

    fn peek(&self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => self.messages.clone(),
            QueueMode::OneAtATime => self.messages.first().cloned().into_iter().collect(),
        }
    }

    fn drain(&mut self) -> Vec<AgentMessage> {
        let drained = self.peek();
        self.messages.drain(..drained.len());
        drained
    }

    fn clear(&mut self) {
        self.messages.clear();
    }
}

/// 构造 [`Agent`] 时的初始状态（pi `AgentInitialState`）。
#[derive(Debug, Clone, Default)]
pub struct AgentInitialState {
    /// 初始 system prompt；会成为开头的 system 消息。
    pub system_prompt: Option<String>,
    /// 初始模型。
    pub model: Option<Model>,
    /// 初始思考级别。
    pub thinking_level: Option<ModelThinkingLevel>,
    /// 初始可执行工具。
    pub tools: Vec<AgentTool>,
    /// 初始 transcript。
    pub messages: Vec<AgentMessage>,
}

/// [`Agent`] 的公开状态快照（pi `AgentState`）。
///
/// pi 的 `state` 是活对象；Rust 里返回快照，避免跨 await 持有内部锁。
#[derive(Debug, Clone)]
pub struct AgentState {
    /// 从 system 消息重放出的当前 prompt。
    pub system_prompt: String,
    /// 后续 turn 使用的模型。
    pub model: Model,
    /// 后续 turn 请求的思考级别。
    pub thinking_level: ModelThinkingLevel,
    /// 可执行工具。
    pub tools: Vec<AgentTool>,
    /// 会话记录。
    pub messages: Vec<AgentMessage>,
    /// 是否正在处理 prompt/continuation；直到被 await 的 `agent_end` listener
    /// 都结算完才变回 `false`。
    pub is_streaming: bool,
    /// 当前流式响应中的部分 assistant 消息。
    pub streaming_message: Option<AgentMessage>,
    /// 正在执行的工具调用 id。
    pub pending_tool_calls: BTreeSet<String>,
    /// 最近一次失败/取消 turn 的错误信息。
    pub error_message: Option<String>,
}

/// 构造 [`Agent`] 的选项（pi `AgentOptions`）。
#[derive(Clone)]
pub struct AgentOptions {
    /// provider 流服务。
    pub stream_fn: BoxStreamService,
    /// 初始状态。
    pub initial: AgentInitialState,
    /// `AgentMessage[] → Message[]` 转换；缺省过滤自定义消息。
    pub convert_to_llm: Option<ConvertToLlm>,
    /// 上下文变换。
    pub transform_context: Option<TransformContext>,
    /// 工具执行前钩子。
    pub before_tool_call: Option<BeforeToolCall>,
    /// 工具执行后钩子。
    pub after_tool_call: Option<AfterToolCall>,
    /// turn 定稿回调。
    pub finish_turn: Option<FinishTurn>,
    /// 其余 provider 请求选项。
    pub options: SimpleStreamOptions,
    /// steer 队列 drain 模式。
    pub steering_mode: QueueMode,
    /// follow-up 队列 drain 模式。
    pub follow_up_mode: QueueMode,
    /// 工具执行方式。
    pub tool_execution: ToolExecutionMode,
}

impl AgentOptions {
    /// 用 provider 流服务构造，其余取默认值。
    pub fn new(stream_fn: BoxStreamService) -> Self {
        Self {
            stream_fn,
            initial: AgentInitialState::default(),
            convert_to_llm: None,
            transform_context: None,
            before_tool_call: None,
            after_tool_call: None,
            finish_turn: None,
            options: SimpleStreamOptions::default(),
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            tool_execution: ToolExecutionMode::Parallel,
        }
    }
}

/// 每个 turn 的模型调用配置（pi `AgentLoopConfig` 的可变部分）。
struct Core {
    stream_fn: BoxStreamService,
    model: Model,
    thinking_level: ModelThinkingLevel,
    tools: Vec<AgentTool>,
    messages: Vec<AgentMessage>,
    convert_to_llm: ConvertToLlm,
    transform_context: Option<TransformContext>,
    before_tool_call: Option<BeforeToolCall>,
    after_tool_call: Option<AfterToolCall>,
    finish_turn: Option<FinishTurn>,
    options: SimpleStreamOptions,
    tool_execution: ToolExecutionMode,
    steering: PendingMessageQueue,
    follow_up: PendingMessageQueue,
    run_active: bool,
    is_streaming: bool,
    streaming_message: Option<AgentMessage>,
    pending_tool_calls: BTreeSet<String>,
    error_message: Option<String>,
    current_signal: Option<CancellationToken>,
    streaming_tx: watch::Sender<bool>,
}

impl Core {
    /// 用当前状态构造一次 run 的 [`LoopConfig`]。
    ///
    /// steer / follow-up 来源直接 drain 队列，因此 drain 点落在编排层（design.md Q7）。
    fn loop_config(
        &self,
        core: &Arc<Mutex<Core>>,
        skip_initial_steering: Arc<std::sync::atomic::AtomicBool>,
    ) -> LoopConfig {
        let mut config = LoopConfig::new(self.model.clone(), Arc::clone(&self.convert_to_llm));
        config.thinking_level = self.thinking_level;
        config.options = self.options.clone();
        config.tool_execution = self.tool_execution;
        config.transform_context = self.transform_context.clone();
        config.before_tool_call = self.before_tool_call.clone();
        config.after_tool_call = self.after_tool_call.clone();
        config.finish_turn = self.finish_turn.clone();
        let steering = Arc::clone(core);
        config.get_steering_messages = Some(Arc::new(move || {
            let core = Arc::clone(&steering);
            let skip = Arc::clone(&skip_initial_steering);
            Box::pin(async move {
                if skip.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    return Vec::new();
                }
                lock(&core).steering.drain()
            })
        }));

        let follow_up = Arc::clone(core);
        config.get_follow_up_messages = Some(Arc::new(move || {
            let core = Arc::clone(&follow_up);
            Box::pin(async move { lock(&core).follow_up.drain() })
        }));

        config
    }
}

/// 事件归约 + 广播的下沉点。
#[derive(Clone)]
struct Sink {
    core: Arc<Mutex<Core>>,
    bus: EventBus,
}

impl EventSink for Sink {
    fn emit(&self, event: AgentEvent) -> BoxFut<()> {
        reduce(&self.core, &event);
        let bus = self.bus.clone();
        Box::pin(async move { bus.emit(event).await })
    }
}

fn reduce(core: &Arc<Mutex<Core>>, event: &AgentEvent) {
    let mut core = lock(core);
    match event {
        AgentEvent::MessageStart { message } | AgentEvent::MessageUpdate { message, .. } => {
            core.streaming_message = Some(message.clone());
        }
        AgentEvent::MessageEnd { message } => {
            core.streaming_message = None;
            core.messages.push(message.clone());
        }
        AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
            core.pending_tool_calls.insert(tool_call_id.clone());
        }
        AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
            core.pending_tool_calls.remove(tool_call_id);
        }
        AgentEvent::TurnEnd { message, .. } => {
            if let AgentMessage::Message(Message::Assistant(assistant)) = message
                && let Some(error) = &assistant.error_message
            {
                core.error_message = Some(error.clone());
            }
        }
        AgentEvent::AgentEnd { .. } => {
            core.streaming_message = None;
        }
        AgentEvent::AgentStart | AgentEvent::TurnStart | AgentEvent::ToolExecutionUpdate { .. } => {}
    }
}

/// 有状态 agent 运行时。
#[derive(Clone)]
pub struct Agent {
    core: Arc<Mutex<Core>>,
    bus: EventBus,
}

impl Agent {
    /// 用给定选项构造 agent。
    pub fn new(options: AgentOptions) -> Self {
        let AgentOptions {
            stream_fn,
            initial,
            convert_to_llm,
            transform_context,
            before_tool_call,
            after_tool_call,
            finish_turn,
            options: stream_options,
            steering_mode,
            follow_up_mode,
            tool_execution,
        } = options;
        let model = initial.model.unwrap_or_default();
        let thinking_level = initial.thinking_level.unwrap_or(ModelThinkingLevel::Off);
        let tools = initial.tools;
        let mut messages = initial.messages;
        let initial_system =
            agent_loop::create_initial_system_message(initial.system_prompt.as_deref(), &tools);
        let starts_with_system = matches!(
            messages.first(),
            Some(AgentMessage::Message(Message::System(_)))
        );
        if !starts_with_system && let Some(system) = initial_system {
            messages.insert(0, AgentMessage::Message(Message::System(system)));
        }

        let (streaming_tx, _) = watch::channel(false);
        let core = Core {
            stream_fn,
            model,
            thinking_level,
            tools,
            messages,
            convert_to_llm: convert_to_llm.unwrap_or_else(|| Arc::new(default_convert_to_llm)),
            transform_context,
            before_tool_call,
            after_tool_call,
            finish_turn,
            options: stream_options,
            tool_execution,
            steering: PendingMessageQueue::new(steering_mode),
            follow_up: PendingMessageQueue::new(follow_up_mode),
            run_active: false,
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: BTreeSet::new(),
            error_message: None,
            current_signal: None,
            streaming_tx,
        };
        Self {
            core: Arc::new(Mutex::new(core)),
            bus: EventBus::new(),
        }
    }

    /// 订阅生命周期事件。
    ///
    /// 事件按产生顺序投递，且 agent 会 await 每个订阅者处理完当前事件后才继续推进；
    /// `agent_end` 是本次 run 的最后一个事件，但 agent 只有在它也被处理完（以及所有
    /// 被 await 的 listener 结算）之后才进入 idle。
    pub fn subscribe(&self) -> Subscription {
        self.bus.subscribe()
    }

    /// 当前状态快照。
    pub fn state(&self) -> AgentState {
        let core = lock(&self.core);
        AgentState {
            system_prompt: agent_loop::current_system_prompt(&core.messages),
            model: core.model.clone(),
            thinking_level: core.thinking_level,
            tools: core.tools.clone(),
            messages: core.messages.clone(),
            is_streaming: core.is_streaming,
            streaming_message: core.streaming_message.clone(),
            pending_tool_calls: core.pending_tool_calls.clone(),
            error_message: core.error_message.clone(),
        }
    }

    /// 替换可执行工具集（等价于 pi 里给 `state.tools` 赋值）。
    pub fn set_tools(&self, tools: Vec<AgentTool>) {
        lock(&self.core).tools = tools;
    }

    /// 替换模型。
    pub fn set_model(&self, model: Model) {
        lock(&self.core).model = model;
    }

    /// 替换 provider 流服务（用于运行时配置/切换 provider）。
    pub fn set_stream(&self, stream_fn: BoxStreamService) {
        lock(&self.core).stream_fn = stream_fn;
    }

    /// 替换思考级别。
    pub fn set_thinking_level(&self, level: ModelThinkingLevel) {
        lock(&self.core).thinking_level = level;
    }

    /// steer 队列的 drain 模式。
    pub fn steering_mode(&self) -> QueueMode {
        lock(&self.core).steering.mode
    }

    /// 设置 steer 队列的 drain 模式。
    pub fn set_steering_mode(&self, mode: QueueMode) {
        lock(&self.core).steering.mode = mode;
    }

    /// follow-up 队列的 drain 模式。
    pub fn follow_up_mode(&self) -> QueueMode {
        lock(&self.core).follow_up.mode
    }

    /// 设置 follow-up 队列的 drain 模式。
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        lock(&self.core).follow_up.mode = mode;
    }

    /// 在当前 assistant turn 结束后注入一条 steer 消息。
    pub fn steer(&self, message: AgentMessage) {
        lock(&self.core).steering.enqueue(message);
    }

    /// 排队一条只在 agent 本该停止时才处理的 follow-up 消息。
    pub fn follow_up(&self, message: AgentMessage) {
        lock(&self.core).follow_up.enqueue(message);
    }

    /// 清空 steer 队列。
    pub fn clear_steering_queue(&self) {
        lock(&self.core).steering.clear();
    }

    /// 清空 follow-up 队列。
    pub fn clear_follow_up_queue(&self) {
        lock(&self.core).follow_up.clear();
    }

    /// 清空两个队列。
    pub fn clear_all_queues(&self) {
        let mut core = lock(&self.core);
        core.steering.clear();
        core.follow_up.clear();
    }

    /// 两个队列是否都为空。
    pub fn has_queued_messages(&self) -> bool {
        let core = lock(&self.core);
        core.steering.has_items() || core.follow_up.has_items()
    }

    /// 预览下一个 drain 点会取走的消息（不消费）。
    pub fn peek_queued_messages(&self) -> Vec<AgentMessage> {
        let core = lock(&self.core);
        let steering = core.steering.peek();
        if steering.is_empty() {
            core.follow_up.peek()
        } else {
            steering
        }
    }

    /// 当前 run 的取消信号。
    pub fn signal(&self) -> Option<CancellationToken> {
        lock(&self.core).current_signal.clone()
    }

    /// 是否正在处理 prompt / continuation。
    pub fn is_streaming(&self) -> bool {
        lock(&self.core).is_streaming
    }

    /// 取消当前 run。
    pub fn abort(&self) {
        if let Some(signal) = lock(&self.core).current_signal.clone() {
            signal.cancel();
        }
    }

    /// 等待当前 run 与所有被 await 的 listener 完成。
    pub async fn wait_for_idle(&self) {
        let mut rx = lock(&self.core).streaming_tx.subscribe();
        loop {
            if !*rx.borrow_and_update() {
                break;
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
    }

    /// 清空会话记录与队列，保留重放出的 prompt/工具基线。
    pub fn reset(&self) -> Result<(), AgentError> {
        let mut core = lock(&self.core);
        if core.run_active {
            return Err(AgentError::Loop(
                "Agent is already processing. Wait for completion before resetting.".into(),
            ));
        }
        let baseline = agent_loop::current_system_message(&core.messages);
        core.messages = baseline
            .map(|system| vec![AgentMessage::Message(Message::System(system))])
            .unwrap_or_default();
        core.is_streaming = false;
        core.streaming_message = None;
        core.pending_tool_calls.clear();
        core.error_message = None;
        core.steering.clear();
        core.follow_up.clear();
        Ok(())
    }

    /// 用一批消息启动一次新的提示。
    pub async fn prompt_messages(&self, messages: Vec<AgentMessage>) -> Result<(), AgentError> {
        self.run(RunMode::Prompt(messages), false).await
    }

    /// 用一条用户消息启动一次新的提示。
    pub async fn prompt(&self, message: AgentMessage) -> Result<(), AgentError> {
        self.run(RunMode::Prompt(vec![message]), false).await
    }

    /// 用一段文本启动一次新的提示。
    pub async fn prompt_text(&self, text: impl Into<String>) -> Result<(), AgentError> {
        let message = AgentMessage::Message(Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: agent_loop::now_ms(),
        }));
        self.prompt(message).await
    }

    /// 从当前 transcript 继续一次 run（pi `Agent.continue`）。
    pub async fn continue_run(&self) -> Result<(), AgentError> {
        let (last_role, queue_has_steering, queue_has_follow_up, all_system) = {
            let core = lock(&self.core);
            let last_role = core
                .messages
                .last()
                .and_then(AgentMessage::role)
                .map(str::to_owned);
            let all_system = core
                .messages
                .iter()
                .all(|message| message.role() == Some("system"));
            (
                last_role,
                core.steering.has_items(),
                core.follow_up.has_items(),
                all_system,
            )
        };

        if last_role.is_none() || all_system {
            return Err(AgentError::Loop("No messages to continue from".into()));
        }

        if last_role.as_deref() == Some("assistant") {
            if queue_has_steering {
                let messages = lock(&self.core).steering.drain();
                return self.run(RunMode::Prompt(messages), true).await;
            }
            if queue_has_follow_up {
                let messages = lock(&self.core).follow_up.drain();
                return self.run(RunMode::Prompt(messages), false).await;
            }
            return Err(AgentError::Loop(
                "Cannot continue from message role: assistant".into(),
            ));
        }

        self.run(RunMode::Continue, false).await
    }

    async fn run(&self, mode: RunMode, skip_initial_steering: bool) -> Result<(), AgentError> {
        let (signal, stream_fn, context, config, sink) = {
            let mut core = lock(&self.core);
            if core.run_active {
                return Err(AgentError::Loop(
                    "Agent is already processing. Use steer() or followUp() to queue messages, \
                     or wait for completion."
                        .into(),
                ));
            }
            core.run_active = true;
            core.is_streaming = true;
            core.streaming_message = None;
            core.error_message = None;
            core.streaming_tx.send_replace(true);

            let signal = CancellationToken::new();
            core.current_signal = Some(signal.clone());
            let context = AgentContext {
                messages: core.messages.clone(),
                tools: core.tools.clone(),
            };
            let stream_fn = core.stream_fn.clone();
            let skip_initial_steering =
                Arc::new(std::sync::atomic::AtomicBool::new(skip_initial_steering));
            let config = core.loop_config(&self.core, skip_initial_steering);
            (
                signal,
                stream_fn,
                context,
                config,
                Sink {
                    core: Arc::clone(&self.core),
                    bus: self.bus.clone(),
                },
            )
        };

        let result = match mode {
            RunMode::Prompt(messages) => agent_loop::run_agent_loop(
                messages,
                context,
                config,
                &sink,
                &signal,
                stream_fn.clone(),
            )
                .await
                .map(|_| ()),
            RunMode::Continue => {
                agent_loop::run_agent_loop_continue(context, config, &sink, &signal, stream_fn)
                    .await
                    .map(|_| ())
            }
        };

        if let Err(error) = &result {
            self.emit_run_failure(&sink, error, signal.is_cancelled())
                .await;
        }

        {
            let mut core = lock(&self.core);
            core.run_active = false;
            core.is_streaming = false;
            core.streaming_message = None;
            core.pending_tool_calls.clear();
            core.current_signal = None;
            core.streaming_tx.send_replace(false);
        }

        result
    }

    async fn emit_run_failure(&self, sink: &Sink, error: &AgentError, aborted: bool) {
        let model = lock(&self.core).model.clone();
        let message = failure_message(
            &model,
            &error.to_string(),
            if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            },
        );
        let agent_message = AgentMessage::Message(Message::Assistant(Box::new(message)));
        sink.emit(AgentEvent::MessageStart {
            message: agent_message.clone(),
        })
            .await;
        sink.emit(AgentEvent::MessageEnd {
            message: agent_message.clone(),
        })
            .await;
        sink.emit(AgentEvent::TurnEnd {
            message: agent_message.clone(),
            tool_results: Vec::new(),
        })
            .await;
        sink.emit(AgentEvent::AgentEnd {
            messages: vec![agent_message],
        })
            .await;
    }

    /// 设置/替换 system prompt（`None` 表示移除）。
    pub fn set_system_prompt(&self, prompt: Option<&str>) {
        let mut core = lock(&self.core);
        let system = agent_loop::create_initial_system_message(prompt, &core.tools);
        if matches!(
            core.messages.first(),
            Some(AgentMessage::Message(Message::System(_)))
        ) {
            core.messages.remove(0);
        }
        if let Some(system) = system {
            core.messages
                .insert(0, AgentMessage::Message(Message::System(system)));
        }
    }

    /// 执行一次 [`AgentRequest`]，返回本次 run 的新增消息与汇总。
    async fn run_request(&self, request: AgentRequest) -> Result<AgentResponse, AgentError> {
        if let Some(model) = request.model.clone() {
            self.set_model(model);
        }
        if let Some(tools) = request.tools.clone() {
            self.set_tools(tools);
        }
        if let Some(system_prompt) = request.system_prompt.clone() {
            self.set_system_prompt(Some(&system_prompt));
        }

        let before = lock(&self.core).messages.len();
        let prompt_len = request.messages.len();
        if request.messages.is_empty() {
            self.continue_run().await?;
        } else {
            self.prompt_messages(request.messages).await?;
        }
        let messages = lock(&self.core).messages.clone();
        let start = (before + prompt_len).min(messages.len());
        let new_messages = messages[start..].to_vec();
        let (text, usage, stop_reason) = summarize(&new_messages);
        Ok(AgentResponse {
            messages: new_messages,
            text,
            usage,
            stop_reason,
        })
    }
}

impl Service<AgentRequest> for Agent {
    type Response = AgentResponse;
    type Error = AgentError;
    type Future = BoxFuture<'static, Result<AgentResponse, AgentError>>;

    // agent 是有状态单写者：并发请求由 `run` 的 `run_active` 检查拒绝（返回
    // `AgentError::Loop`），因此这里无需真的阻塞。
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: AgentRequest) -> Self::Future {
        let agent = self.clone();
        Box::pin(async move { agent.run_request(request).await })
    }
}

/// 汇总本次 run 新增的消息：最后一条 assistant 文本、usage 合计与 stop reason。
pub(crate) fn summarize(messages: &[AgentMessage]) -> (String, Usage, Option<StopReason>) {
    let mut usage = Usage::default();
    let mut text = String::new();
    let mut stop_reason = None;
    for message in messages {
        if let AgentMessage::Message(Message::Assistant(assistant)) = message {
            merge_usage(&mut usage, &assistant.usage);
            text = assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect();
            stop_reason = Some(assistant.stop_reason);
        }
    }
    (text, usage, stop_reason)
}

/// 累加 usage（`tower_agent_provider::Usage` 没有内建的合并方法）。
fn merge_usage(total: &mut Usage, other: &Usage) {
    total.input += other.input;
    total.output += other.output;
    total.cache_read += other.cache_read;
    total.cache_write += other.cache_write;
    total.total_tokens += other.total_tokens;
    total.cache_write_1h = add_optional(total.cache_write_1h, other.cache_write_1h);
    total.reasoning = add_optional(total.reasoning, other.reasoning);
    total.cost.input += other.cost.input;
    total.cost.output += other.cost.output;
    total.cost.cache_read += other.cost.cache_read;
    total.cost.cache_write += other.cost.cache_write;
    total.cost.total += other.cost.total;
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (Some(value), None) | (None, Some(value)) => Some(value),
        (Some(left), Some(right)) => Some(left + right),
    }
}

enum RunMode {
    Prompt(Vec<AgentMessage>),
    Continue,
}

fn lock(core: &Arc<Mutex<Core>>) -> MutexGuard<'_, Core> {
    core.lock().expect("agent core mutex poisoned")
}

pub(crate) fn failure_message(
    model: &Model,
    text: &str,
    stop_reason: StopReason,
) -> tower_agent_provider::AssistantMessage {
    tower_agent_provider::AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: String::new(),
            text_signature: None,
        })],
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
        error_message: Some(text.into()),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: agent_loop::now_ms(),
    }
}
