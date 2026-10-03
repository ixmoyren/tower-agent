//! agent 编排循环（pi `agent-loop.ts`）。
//!
//! 一次 [`run_agent_loop`] / [`run_agent_loop_continue`] 调用就是一次完整的 agent run：
//! 它会依次发起 provider 请求、执行工具、注入 steer/follow-up 消息，并把每条
//! [`AgentEvent`] 交给 [`EventSink`]。事件是 push 的、多订阅者的，不进通用 tower
//! 中间件（design.md D1/D3）；一次 provider 调用的请求边界仍然是
//! [`ai::StreamService`]。
//!
//! 行为对齐清单（design.md 第 7 节）中由本模块负责的部分：
//!
//! - 并行工具执行：`tool_execution_end` 按**完成顺序**发出，工具结果消息按
//!   **assistant 源顺序**发出。
//! - `terminate` 仅当一批内所有已定稿结果都为 `true` 时提前结束。
//! - `length` 截断的 assistant 消息里的工具调用一律失败，不执行。

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use ai::{
    AssistantMessage, AssistantMessageEvent, BoxStreamService, ContentBlock, JsonObject, JsonValue,
    Message, Model, ModelThinkingLevel, SimpleStreamOptions, StopReason, StreamRequest,
    SystemMessage, Tool, ToolCall, ToolResultMessage, TranscriptContext, Usage,
};
use futures_util::{
    StreamExt,
    future::{BoxFuture, join_all},
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::{
    AgentError, AgentEvent, AgentMessage, AgentTool, AgentToolResult, ToolCallRequest,
    ToolExecutionMode, ToolResultStream,
};

/// 装箱的 `'static`、`Send` future。
pub type BoxFut<T> = BoxFuture<'static, T>;

/// 事件下沉点。
///
/// 事件是 push、多订阅者的；`emit` 返回的 future 解析时，才表示所有被 await 的
/// listener 都处理完了这个事件（design.md Q6：逐订阅者 mpsc + 背压）。
pub trait EventSink: Clone + Send + Sync + 'static {
    /// 发出一个事件。
    fn emit(&self, event: AgentEvent) -> BoxFut<()>;
}

/// 一次 run 的可见上下文（pi `AgentContext`）。
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    /// 模型可见的会话记录。
    pub messages: Vec<AgentMessage>,
    /// 本次 run 可执行的工具。
    pub tools: Vec<AgentTool>,
}

impl AgentContext {
    /// 用消息构造，工具集为空。
    pub fn new(messages: Vec<AgentMessage>) -> Self {
        Self {
            messages,
            tools: Vec::new(),
        }
    }

    /// 设置工具集。
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<AgentTool>) -> Self {
        self.tools = tools;
        self
    }

    /// 按名字查找工具（pi `tools.find((t) => t.name === call.name)`）。
    pub fn find_tool(&self, name: &str) -> Option<&AgentTool> {
        self.tools.iter().find(|tool| tool.name() == name)
    }
}

/// `AgentMessage[] → Message[]` 的转换（pi `convertToLlm`）。
///
/// 契约：不得抛错；无法转换的消息返回安全的回退值（通常是过滤掉）。
pub type ConvertToLlm = Arc<dyn Fn(Vec<AgentMessage>) -> BoxFut<Vec<Message>> + Send + Sync>;

/// 在 `convertToLlm` 之前对上下文做变换（pi `transformContext`）。
///
/// 契约：不得抛错；返回原消息或另一个安全回退值。
pub type TransformContext =
    Arc<dyn Fn(Vec<AgentMessage>, CancellationToken) -> BoxFut<Vec<AgentMessage>> + Send + Sync>;

/// 取回要注入当前 run 的 steer 消息（pi `getSteeringMessages`）。
pub type SteeringSource = Arc<dyn Fn() -> BoxFut<Vec<AgentMessage>> + Send + Sync>;

/// 取回 agent 本该停止时再处理的 follow-up 消息（pi `getFollowUpMessages`）。
pub type FollowUpSource = Arc<dyn Fn() -> BoxFut<Vec<AgentMessage>> + Send + Sync>;

/// 工具执行前的钩子（pi `beforeToolCall`）。
pub type BeforeToolCall =
    Arc<dyn Fn(BeforeToolCallContext) -> BoxFut<Option<BeforeToolCallResult>> + Send + Sync>;

/// 工具执行后的钩子（pi `afterToolCall`）。
pub type AfterToolCall =
    Arc<dyn Fn(AfterToolCallContext) -> BoxFut<Option<AfterToolCallResult>> + Send + Sync>;

/// 一个 turn 定稿后的回调（pi `finishTurn`）。
pub type FinishTurn = Arc<dyn Fn(AgentTurnContext) -> BoxFut<Option<TurnDecision>> + Send + Sync>;

/// `beforeToolCall` 的上下文（pi `BeforeToolCallContext`）。
#[derive(Debug, Clone)]
pub struct BeforeToolCallContext {
    /// 发起该工具调用的 assistant 消息。
    pub assistant_message: AssistantMessage,
    /// 模型给出的原始工具调用。
    pub tool_call: ToolCall,
    /// 已应用参数垫片的工具参数。
    pub args: JsonObject,
    /// 准备该调用时的 agent 上下文。
    pub context: AgentContext,
}

/// `beforeToolCall` 的返回值（pi `BeforeToolCallResult`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BeforeToolCallResult {
    /// 阻止该工具执行；loop 改发一条 `is_error: true` 的工具结果。
    pub block: bool,
    /// `block` 时展示给模型的文本；缺省使用默认文案。
    pub reason: Option<String>,
    /// 建议本批结束后终止（仅当批内所有定稿结果都为 `true` 时生效）。
    pub terminate: Option<bool>,
}

/// `afterToolCall` 的上下文（pi `AfterToolCallContext`）。
#[derive(Debug, Clone)]
pub struct AfterToolCallContext {
    /// 发起该工具调用的 assistant 消息。
    pub assistant_message: AssistantMessage,
    /// 模型给出的原始工具调用。
    pub tool_call: ToolCall,
    /// 已应用参数垫片的工具参数。
    pub args: JsonObject,
    /// 钩子覆盖前的工具结果。
    pub result: AgentToolResult,
    /// 该结果当前是否被视为错误。
    pub is_error: bool,
    /// 定稿该调用时的 agent 上下文。
    pub context: AgentContext,
}

/// `afterToolCall` 的部分覆盖（pi `AfterToolCallResult`）。
///
/// 字段级合并：提供的字段整体替换原值，未提供的保持原样。`content` 与
/// `structured_content` 的联动与 pi 一致——只替换 `content` 而不给
/// `structured_content` 时会丢弃结构化内容。
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    /// 整体替换工具结果的模型可见内容。
    pub content: Option<Vec<ContentBlock>>,
    /// 整体替换 `details`。
    pub details: Option<JsonValue>,
    /// 整体替换 `structured_content`。
    pub structured_content: Option<JsonValue>,
    /// 替换错误标记。
    pub is_error: Option<bool>,
    /// 替换工具自身的 usage。
    pub usage: Option<Usage>,
    /// 替换提前终止提示。
    pub terminate: Option<bool>,
}

/// `finishTurn` 的上下文（pi `AgentTurnContext`）。
#[derive(Debug, Clone)]
pub struct AgentTurnContext {
    /// 本 turn 定稿的 assistant 消息。
    pub message: AssistantMessage,
    /// 本 turn 产生的工具结果消息。
    pub tool_results: Vec<ToolResultMessage>,
    /// assistant 消息与工具结果都追加后的上下文快照。
    pub context: AgentContext,
    /// 若 loop 此刻退出，本次调用将返回的新消息。
    pub new_messages: Vec<AgentMessage>,
}

/// `finishTurn` 的返回值（pi `AgentTurnDecision`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDecision {
    /// 保证再进行一次 provider 请求（工具结果 / steer / follow-up 可满足它）。
    Continue,
    /// 在轮询队列前直接结束本次 run。
    End,
}

/// 低层循环的配置（pi `AgentLoopConfig`）。
#[derive(Clone)]
pub struct LoopConfig {
    /// 本次 run 使用的模型。
    pub model: Model,
    /// 请求的思考级别（pi `thinkingLevel`）。
    pub thinking_level: ModelThinkingLevel,
    /// 其余 provider 请求选项（pi `AgentLoopConfig extends SimpleStreamOptions`）。
    pub options: SimpleStreamOptions,
    /// 同一条 assistant 消息内多个工具调用的执行方式（默认 parallel）。
    pub tool_execution: ToolExecutionMode,
    /// `AgentMessage[] → Message[]` 转换。
    pub convert_to_llm: ConvertToLlm,
    /// 可选的上下文变换。
    pub transform_context: Option<TransformContext>,
    /// steer 消息来源。
    pub get_steering_messages: Option<SteeringSource>,
    /// follow-up 消息来源。
    pub get_follow_up_messages: Option<FollowUpSource>,
    /// 工具执行前钩子。
    pub before_tool_call: Option<BeforeToolCall>,
    /// 工具执行后钩子。
    pub after_tool_call: Option<AfterToolCall>,
    /// turn 定稿回调。
    pub finish_turn: Option<FinishTurn>,
}

impl LoopConfig {
    /// 用模型与 `convertToLlm` 构造，其余选项取默认值。
    pub fn new(model: Model, convert_to_llm: ConvertToLlm) -> Self {
        Self {
            model,
            thinking_level: ModelThinkingLevel::Off,
            options: SimpleStreamOptions::default(),
            tool_execution: ToolExecutionMode::Parallel,
            convert_to_llm,
            transform_context: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            before_tool_call: None,
            after_tool_call: None,
            finish_turn: None,
        }
    }
}

/// 默认的 `convertToLlm`：只保留四种基础消息，过滤自定义消息。
pub fn default_convert_to_llm(messages: Vec<AgentMessage>) -> BoxFut<Vec<Message>> {
    Box::pin(async move {
        messages
            .into_iter()
            .filter_map(|message| match message {
                AgentMessage::Message(message) => Some(message),
                AgentMessage::Custom(_) => None,
            })
            .collect()
    })
}

/// 用一批初始 prompt 启动 agent run（pi `agentLoop` / `runAgentLoop`）。
///
/// 返回本次 run 新增的消息（prompt 消息 + assistant 消息 + 工具结果消息）。
pub async fn run_agent_loop<S: EventSink>(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: LoopConfig,
    sink: &S,
    signal: &CancellationToken,
    stream_fn: BoxStreamService,
) -> Result<Vec<AgentMessage>, AgentError> {
    let initial_messages = declare_tool_changes(&context, prompts);
    let mut new_messages = initial_messages.clone();
    let mut current = AgentContext {
        messages: context.messages.clone(),
        tools: context.tools.clone(),
    };
    current.messages.extend(initial_messages.iter().cloned());

    sink.emit(AgentEvent::AgentStart).await;
    sink.emit(AgentEvent::TurnStart).await;
    for message in &initial_messages {
        emit_message_lifecycle(sink, message).await;
    }

    run_loop(
        current,
        std::mem::take(&mut new_messages),
        config,
        sink,
        signal,
        stream_fn,
    )
    .await
}

/// 从当前上下文继续一次 run，不新增 prompt 消息（pi `agentLoopContinue`）。
///
/// 上下文最后一条消息不得是 assistant（必须能转换成 user / toolResult）。
pub async fn run_agent_loop_continue<S: EventSink>(
    context: AgentContext,
    config: LoopConfig,
    sink: &S,
    signal: &CancellationToken,
    stream_fn: BoxStreamService,
) -> Result<Vec<AgentMessage>, AgentError> {
    if context.messages.is_empty() {
        return Err(AgentError::Loop(
            "Cannot continue: no messages in context".into(),
        ));
    }
    if matches!(
        context.messages.last(),
        Some(AgentMessage::Message(Message::Assistant(_)))
    ) {
        return Err(AgentError::Loop(
            "Cannot continue from message role: assistant".into(),
        ));
    }

    sink.emit(AgentEvent::AgentStart).await;
    sink.emit(AgentEvent::TurnStart).await;

    run_loop(context, Vec::new(), config, sink, signal, stream_fn).await
}

/// 低层主循环（pi `runLoop`）。
async fn run_loop<S: EventSink>(
    mut current: AgentContext,
    mut new_messages: Vec<AgentMessage>,
    config: LoopConfig,
    sink: &S,
    signal: &CancellationToken,
    stream_fn: BoxStreamService,
) -> Result<Vec<AgentMessage>, AgentError> {
    let mut explicit_continuation = false;
    let mut pending_messages = poll_steering(&config, signal).await;
    let mut has_completed_turn = false;

    'outer: loop {
        let mut has_more_tool_calls = true;

        while has_more_tool_calls || !pending_messages.is_empty() {
            if has_completed_turn {
                // prepareNextTurn / prepareRequest 钩子尚未移植（见 design.md 第 9 节后续步骤）。
                if pending_messages.is_empty() {
                    pending_messages = poll_steering(&config, signal).await;
                }
                sink.emit(AgentEvent::TurnStart).await;
            }

            for message in declare_tool_changes(&current, std::mem::take(&mut pending_messages)) {
                emit_message_lifecycle(sink, &message).await;
                current.messages.push(message.clone());
                new_messages.push(message);
            }

            let message =
                stream_assistant_response(&mut current, &config, signal, sink, stream_fn.clone())
                    .await?;
            let assistant = assistant_message(message.clone());
            new_messages.push(assistant.clone());

            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                await_finish_turn(
                    &config,
                    &message,
                    Vec::new(),
                    current.clone(),
                    &new_messages,
                )
                .await;
                sink.emit(AgentEvent::TurnEnd {
                    message: assistant,
                    tool_results: Vec::new(),
                })
                .await;
                sink.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return Ok(new_messages);
            }

            let tool_calls = collect_tool_calls(&message);
            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                let batch = if message.stop_reason == StopReason::Length {
                    fail_truncated_tool_calls(&tool_calls, sink).await
                } else {
                    execute_tool_calls(
                        current.clone(),
                        &message,
                        &config,
                        &tool_calls,
                        signal,
                        sink,
                    )
                    .await?
                };
                tool_results.extend(batch.messages);
                has_more_tool_calls = !batch.terminate;
                for result in &tool_results {
                    let message = AgentMessage::Message(Message::ToolResult(result.clone()));
                    current.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            let decision = await_finish_turn(
                &config,
                &message,
                tool_results.clone(),
                current.clone(),
                &new_messages,
            )
            .await;
            sink.emit(AgentEvent::TurnEnd {
                message: assistant,
                tool_results,
            })
            .await;
            has_completed_turn = true;

            if decision == Some(TurnDecision::End) {
                sink.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return Ok(new_messages);
            }

            explicit_continuation = decision == Some(TurnDecision::Continue);
            pending_messages = poll_steering(&config, signal).await;
            if has_more_tool_calls || !pending_messages.is_empty() {
                explicit_continuation = false;
            }
        }

        let follow_up = poll_follow_up(&config, signal).await;
        if !follow_up.is_empty() {
            explicit_continuation = false;
            pending_messages = follow_up;
            continue 'outer;
        }
        if explicit_continuation {
            explicit_continuation = false;
            continue 'outer;
        }
        break;
    }

    sink.emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    })
    .await;
    Ok(new_messages)
}

async fn await_finish_turn(
    config: &LoopConfig,
    message: &AssistantMessage,
    tool_results: Vec<ToolResultMessage>,
    context: AgentContext,
    new_messages: &[AgentMessage],
) -> Option<TurnDecision> {
    let finish = config.finish_turn.as_ref()?;
    finish(AgentTurnContext {
        message: message.clone(),
        tool_results,
        context,
        new_messages: new_messages.to_vec(),
    })
    .await
}

async fn poll_steering(config: &LoopConfig, signal: &CancellationToken) -> Vec<AgentMessage> {
    let _ = signal;
    match &config.get_steering_messages {
        Some(source) => source().await,
        None => Vec::new(),
    }
}

async fn poll_follow_up(config: &LoopConfig, signal: &CancellationToken) -> Vec<AgentMessage> {
    let _ = signal;
    match &config.get_follow_up_messages {
        Some(source) => source().await,
        None => Vec::new(),
    }
}

async fn emit_message_lifecycle<S: EventSink>(sink: &S, message: &AgentMessage) {
    sink.emit(AgentEvent::MessageStart {
        message: message.clone(),
    })
    .await;
    sink.emit(AgentEvent::MessageEnd {
        message: message.clone(),
    })
    .await;
}

/// 流式获取一次 assistant 响应（pi `streamAssistantResponse`）。
async fn stream_assistant_response<S: EventSink>(
    context: &mut AgentContext,
    config: &LoopConfig,
    signal: &CancellationToken,
    sink: &S,
    stream_fn: BoxStreamService,
) -> Result<AssistantMessage, AgentError> {
    let mut messages = context.messages.clone();
    if let Some(transform) = &config.transform_context {
        messages = transform(messages, signal.clone()).await;
    }
    let llm_messages = (config.convert_to_llm)(messages).await;

    let mut options = config.options.clone();
    options.base.signal = Some(signal.clone());
    options.reasoning = match config.thinking_level {
        ModelThinkingLevel::Off => None,
        ModelThinkingLevel::Level(level) => Some(level),
    };
    let request = StreamRequest {
        model: config.model.clone(),
        context: TranscriptContext::new(llm_messages),
        options,
    };

    let mut stream = stream_fn.clone().oneshot(request).await?;
    let mut partial: Option<AssistantMessage> = None;
    let mut added_partial = false;

    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::Start { partial: p } => {
                partial = Some(p.clone());
                context.messages.push(assistant_message(p.clone()));
                added_partial = true;
                sink.emit(AgentEvent::MessageStart {
                    message: assistant_message(p),
                })
                .await;
            }
            AssistantMessageEvent::Done { message, .. }
            | AssistantMessageEvent::Error { error: message, .. } => {
                let final_message = with_thinking_level(message, config.thinking_level);
                if added_partial {
                    if let Some(last) = context.messages.last_mut() {
                        *last = assistant_message(final_message.clone());
                    }
                } else {
                    context
                        .messages
                        .push(assistant_message(final_message.clone()));
                    sink.emit(AgentEvent::MessageStart {
                        message: assistant_message(final_message.clone()),
                    })
                    .await;
                }
                sink.emit(AgentEvent::MessageEnd {
                    message: assistant_message(final_message.clone()),
                })
                .await;
                return Ok(final_message);
            }
            update => {
                if partial.is_some() {
                    let p = event_partial(&update)
                        .expect("non-terminal assistant event carries a partial message")
                        .clone();
                    partial = Some(p.clone());
                    if let Some(last) = context.messages.last_mut() {
                        *last = assistant_message(p.clone());
                    }
                    sink.emit(AgentEvent::MessageUpdate {
                        message: assistant_message(p),
                        assistant_message_event: update,
                    })
                    .await;
                }
            }
        }
    }

    // 流在没有终态事件的情况下结束：把累积视图当作终态，保持事件序列完整。
    let final_message = with_thinking_level(
        partial.unwrap_or_else(|| {
            error_assistant_message(&config.model, "stream ended", StopReason::Error)
        }),
        config.thinking_level,
    );
    if added_partial {
        if let Some(last) = context.messages.last_mut() {
            *last = assistant_message(final_message.clone());
        }
    } else {
        context
            .messages
            .push(assistant_message(final_message.clone()));
        sink.emit(AgentEvent::MessageStart {
            message: assistant_message(final_message.clone()),
        })
        .await;
    }
    sink.emit(AgentEvent::MessageEnd {
        message: assistant_message(final_message.clone()),
    })
    .await;
    Ok(final_message)
}

/// 一次工具批次的结果。
struct ExecutedBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

/// 已定稿的一次工具调用（pi `FinalizedToolCallOutcome`）。
#[derive(Debug, Clone)]
pub struct FinalizedToolCall {
    /// 原始工具调用。
    pub tool_call: ToolCall,
    /// 钩子覆盖后的结果。
    pub result: AgentToolResult,
    /// 该结果是否表示错误。
    pub is_error: bool,
}

/// 工具调用的准备结果（pi `PreparedToolCall | ImmediateToolCallOutcome`）。
enum Preparation {
    Immediate {
        result: AgentToolResult,
        is_error: bool,
    },
    Prepared(PreparedToolCall),
}

#[derive(Clone)]
struct PreparedToolCall {
    tool_call: ToolCall,
    tool: AgentTool,
    args: JsonObject,
}

struct ExecutedToolCall {
    result: AgentToolResult,
    is_error: bool,
}

async fn execute_tool_calls<S: EventSink>(
    context: AgentContext,
    assistant_message: &AssistantMessage,
    config: &LoopConfig,
    tool_calls: &[ToolCall],
    signal: &CancellationToken,
    sink: &S,
) -> Result<ExecutedBatch, AgentError> {
    let has_sequential = tool_calls.iter().any(|tool_call| {
        context.find_tool(&tool_call.name).is_some_and(|tool| {
            tool.resolved_execution_mode(config.tool_execution) == ToolExecutionMode::Sequential
        })
    });
    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential {
        Ok(execute_tool_calls_sequential(
            context.clone(),
            assistant_message,
            config,
            tool_calls,
            signal,
            sink,
        )
        .await)
    } else {
        Ok(execute_tool_calls_parallel(
            context,
            assistant_message,
            config,
            tool_calls,
            signal,
            sink,
        )
        .await)
    }
}

async fn execute_tool_calls_sequential<S: EventSink>(
    context: AgentContext,
    assistant_message: &AssistantMessage,
    config: &LoopConfig,
    tool_calls: &[ToolCall],
    signal: &CancellationToken,
    sink: &S,
) -> ExecutedBatch {
    let mut finalized_calls = Vec::new();
    let mut messages = Vec::new();

    for tool_call in tool_calls {
        emit_tool_execution_start(tool_call, sink).await;
        let finalized = match prepare_tool_call(
            context.clone(),
            assistant_message,
            tool_call,
            config,
            signal,
        )
        .await
        {
            Preparation::Immediate { result, is_error } => FinalizedToolCall {
                tool_call: tool_call.clone(),
                result,
                is_error,
            },
            Preparation::Prepared(prepared) => {
                let executed = execute_prepared(
                    prepared.clone(),
                    signal,
                    update_emitter(tool_call.clone(), sink.clone()),
                )
                .await;
                finalize_executed_tool_call(
                    context.clone(),
                    assistant_message,
                    prepared,
                    executed,
                    config,
                )
                .await
            }
        };
        emit_tool_execution_end(&finalized, sink).await;
        let message = create_tool_result_message(&finalized);
        emit_tool_result_message(&message, sink).await;
        finalized_calls.push(finalized);
        messages.push(message);

        if signal.is_cancelled() {
            break;
        }
    }

    ExecutedBatch {
        terminate: should_terminate_tool_batch(&finalized_calls),
        messages,
    }
}

async fn execute_tool_calls_parallel<S: EventSink>(
    context: AgentContext,
    assistant_message: &AssistantMessage,
    config: &LoopConfig,
    tool_calls: &[ToolCall],
    signal: &CancellationToken,
    sink: &S,
) -> ExecutedBatch {
    let mut entries: Vec<BoxFut<FinalizedToolCall>> = Vec::new();

    for tool_call in tool_calls {
        emit_tool_execution_start(tool_call, sink).await;
        match prepare_tool_call(
            context.clone(),
            assistant_message,
            tool_call,
            config,
            signal,
        )
        .await
        {
            Preparation::Immediate { result, is_error } => {
                let finalized = FinalizedToolCall {
                    tool_call: tool_call.clone(),
                    result,
                    is_error,
                };
                emit_tool_execution_end(&finalized, sink).await;
                entries.push(Box::pin(std::future::ready(finalized)));
            }
            Preparation::Prepared(prepared) => {
                let context = context.clone();
                let assistant_message = assistant_message.clone();
                let config = config.clone();
                let signal = signal.clone();
                let sink = sink.clone();
                entries.push(Box::pin(async move {
                    if signal.is_cancelled() {
                        let finalized = FinalizedToolCall {
                            tool_call: prepared.tool_call.clone(),
                            result: error_tool_result("Operation aborted"),
                            is_error: true,
                        };
                        emit_tool_execution_end(&finalized, &sink).await;
                        return finalized;
                    }
                    let executed = execute_prepared(
                        prepared.clone(),
                        &signal,
                        update_emitter(prepared.tool_call.clone(), sink.clone()),
                    )
                    .await;
                    let finalized = finalize_executed_tool_call(
                        context.clone(),
                        &assistant_message,
                        prepared,
                        executed,
                        &config,
                    )
                    .await;
                    emit_tool_execution_end(&finalized, &sink).await;
                    finalized
                }));
            }
        }
        if signal.is_cancelled() {
            break;
        }
    }

    let ordered = join_all(entries).await;
    let terminate = should_terminate_tool_batch(&ordered);
    let mut messages = Vec::with_capacity(ordered.len());
    for finalized in &ordered {
        let message = create_tool_result_message(finalized);
        emit_tool_result_message(&message, sink).await;
        messages.push(message);
    }

    ExecutedBatch {
        messages,
        terminate,
    }
}

async fn prepare_tool_call(
    context: AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    config: &LoopConfig,
    signal: &CancellationToken,
) -> Preparation {
    let Some(tool) = context.find_tool(&tool_call.name).cloned() else {
        return Preparation::Immediate {
            result: error_tool_result(format!("Tool {} not found", tool_call.name)),
            is_error: true,
        };
    };

    if signal.is_cancelled() {
        return Preparation::Immediate {
            result: error_tool_result("Operation aborted"),
            is_error: true,
        };
    }

    let args = tool.prepare(tool_call.arguments.clone());

    if let Some(before) = &config.before_tool_call {
        let result = before(BeforeToolCallContext {
            assistant_message: assistant_message.clone(),
            tool_call: tool_call.clone(),
            args: args.clone(),
            context: context.clone(),
        })
        .await;
        if signal.is_cancelled() {
            return Preparation::Immediate {
                result: error_tool_result("Operation aborted"),
                is_error: true,
            };
        }
        if let Some(result) = result
            && result.block
        {
            let mut blocked = error_tool_result(
                result
                    .reason
                    .unwrap_or_else(|| "Tool execution was blocked".into()),
            );
            if result.terminate == Some(true) {
                blocked.terminate = Some(true);
            }
            return Preparation::Immediate {
                result: blocked,
                is_error: true,
            };
        }
    }

    if signal.is_cancelled() {
        return Preparation::Immediate {
            result: error_tool_result("Operation aborted"),
            is_error: true,
        };
    }

    Preparation::Prepared(PreparedToolCall {
        tool_call: tool_call.clone(),
        tool,
        args,
    })
}

async fn execute_prepared<U>(
    prepared: PreparedToolCall,
    signal: &CancellationToken,
    on_update: U,
) -> ExecutedToolCall
where
    U: Fn(AgentToolResult) -> BoxFut<()> + Send,
{
    let request = ToolCallRequest {
        tool_call: prepared.tool_call.clone(),
        signal: Some(signal.clone()),
    };
    match prepared.tool.call(request).await {
        Ok(stream) => drain_tool_stream(stream, prepared.tool_call.name.clone(), on_update).await,
        Err(error) => ExecutedToolCall {
            result: error_tool_result(error.to_string()),
            is_error: true,
        },
    }
}

async fn drain_tool_stream<U>(
    mut stream: ToolResultStream,
    tool_name: String,
    on_update: U,
) -> ExecutedToolCall
where
    U: Fn(AgentToolResult) -> BoxFut<()> + Send,
{
    // 中间项承载 pi 的 `onUpdate` 部分结果，末项是终态结果；因此只有拿到下一项时，
    // 才能确定前一项是部分结果。
    let mut pending: Option<AgentToolResult> = None;
    while let Some(item) = stream.next().await {
        if let Some(previous) = pending.replace(item) {
            on_update(previous).await;
        }
    }
    match pending {
        Some(result) => ExecutedToolCall {
            is_error: result.is_error,
            result,
        },
        None => ExecutedToolCall {
            result: error_tool_result(format!("Tool {tool_name} produced no result")),
            is_error: true,
        },
    }
}

async fn finalize_executed_tool_call(
    context: AgentContext,
    assistant_message: &AssistantMessage,
    prepared: PreparedToolCall,
    executed: ExecutedToolCall,
    config: &LoopConfig,
) -> FinalizedToolCall {
    let mut result = executed.result;
    let mut is_error = executed.is_error;

    if let Some(after) = &config.after_tool_call {
        let override_result = after(AfterToolCallContext {
            assistant_message: assistant_message.clone(),
            tool_call: prepared.tool_call.clone(),
            args: prepared.args.clone(),
            result: result.clone(),
            is_error,
            context,
        })
        .await;

        if let Some(override_result) = override_result {
            // 与 pi 一致：只替换 content 而不给 structured_content 时丢弃结构化内容。
            result.structured_content = match override_result.structured_content {
                Some(value) => Some(value),
                None if override_result.content.is_some() => None,
                None => result.structured_content,
            };
            if let Some(content) = override_result.content {
                result.content = content;
            }
            if let Some(details) = override_result.details {
                result.details = Some(details);
            }
            if let Some(usage) = override_result.usage {
                result.usage = Some(usage);
            }
            if let Some(terminate) = override_result.terminate {
                result.terminate = Some(terminate);
            }
            if let Some(override_is_error) = override_result.is_error {
                is_error = override_is_error;
                result.is_error = override_is_error;
            }
        }
    }

    FinalizedToolCall {
        tool_call: prepared.tool_call.clone(),
        result,
        is_error,
    }
}

async fn fail_truncated_tool_calls<S: EventSink>(
    tool_calls: &[ToolCall],
    sink: &S,
) -> ExecutedBatch {
    let mut messages = Vec::new();
    for tool_call in tool_calls {
        emit_tool_execution_start(tool_call, sink).await;
        let finalized = FinalizedToolCall {
            tool_call: tool_call.clone(),
            result: error_tool_result(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, \
                 so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                tool_call.name
            )),
            is_error: true,
        };
        emit_tool_execution_end(&finalized, sink).await;
        let message = create_tool_result_message(&finalized);
        emit_tool_result_message(&message, sink).await;
        messages.push(message);
    }
    ExecutedBatch {
        messages,
        terminate: false,
    }
}

fn should_terminate_tool_batch(finalized_calls: &[FinalizedToolCall]) -> bool {
    !finalized_calls.is_empty()
        && finalized_calls
            .iter()
            .all(|finalized| finalized.result.terminate == Some(true))
}

async fn emit_tool_execution_start<S: EventSink>(tool_call: &ToolCall, sink: &S) {
    sink.emit(AgentEvent::ToolExecutionStart {
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        args: JsonValue::Object(tool_call.arguments.clone()),
    })
    .await;
}

async fn emit_tool_execution_end<S: EventSink>(finalized: &FinalizedToolCall, sink: &S) {
    sink.emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        result: finalized.result.clone(),
        is_error: finalized.is_error,
    })
    .await;
}

async fn emit_tool_result_message<S: EventSink>(message: &ToolResultMessage, sink: &S) {
    let message = AgentMessage::Message(Message::ToolResult(message.clone()));
    sink.emit(AgentEvent::MessageStart {
        message: message.clone(),
    })
    .await;
    sink.emit(AgentEvent::MessageEnd { message }).await;
}

/// 把工具流中的部分结果转成 `tool_execution_update` 事件。
fn update_emitter<S: EventSink>(
    tool_call: ToolCall,
    sink: S,
) -> impl Fn(AgentToolResult) -> BoxFut<()> {
    move |partial_result| {
        sink.emit(AgentEvent::ToolExecutionUpdate {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: JsonValue::Object(tool_call.arguments.clone()),
            partial_result,
        })
    }
}

fn create_tool_result_message(finalized: &FinalizedToolCall) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        content: finalized.result.content.clone(),
        details: finalized.result.details.clone(),
        is_error: finalized.is_error,
        timestamp: now_ms(),
    }
}

/// 构造 `is_error: true` 的工具结果（pi `createErrorToolResult`）。
pub fn error_tool_result(message: impl Into<String>) -> AgentToolResult {
    AgentToolResult {
        details: Some(JsonValue::Object(JsonObject::default())),
        ..AgentToolResult::error(message)
    }
}

fn collect_tool_calls(message: &AssistantMessage) -> Vec<ToolCall> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall(tool_call) => Some(tool_call.clone()),
            _ => None,
        })
        .collect()
}

fn assistant_message(message: AssistantMessage) -> AgentMessage {
    AgentMessage::Message(Message::Assistant(Box::new(message)))
}

fn with_thinking_level(
    mut message: AssistantMessage,
    thinking_level: ModelThinkingLevel,
) -> AssistantMessage {
    message.thinking_level = Some(thinking_level);
    message
}

fn event_partial(event: &AssistantMessageEvent) -> Option<&AssistantMessage> {
    match event {
        AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
        AssistantMessageEvent::Start { .. }
        | AssistantMessageEvent::Done { .. }
        | AssistantMessageEvent::Error { .. } => None,
    }
}

fn error_assistant_message(model: &Model, text: &str, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content: if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentBlock::Text(ai::TextContent {
                text: text.into(),
                text_signature: None,
            })]
        },
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
        timestamp: now_ms(),
    }
}

/// 当前 Unix 毫秒时间戳（pi `Date.now()`）。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 构造开头的 system 消息（pi `createInitialSystemMessage`）。
///
/// prompt 与工具都为空时返回 `None`，保持空 transcript 为空。
pub fn create_initial_system_message(
    system_prompt: Option<&str>,
    tools: &[AgentTool],
) -> Option<SystemMessage> {
    let has_prompt = system_prompt.is_some_and(|prompt| !prompt.is_empty());
    let has_tools = !tools.is_empty();
    if !has_prompt && !has_tools {
        return None;
    }
    Some(SystemMessage {
        content: system_prompt
            .filter(|prompt| !prompt.is_empty())
            .map(|prompt| {
                vec![ContentBlock::Text(ai::TextContent {
                    text: prompt.into(),
                    text_signature: None,
                })]
            })
            .unwrap_or_default(),
        sections: Default::default(),
        tools_added: tools.iter().map(AgentTool::declaration).collect(),
        tools_removed: Vec::new(),
        timestamp: 0,
    })
}

/// 重放 system 消息得到当前 system 消息（pi `getCurrentSystemMessage`）。
pub fn current_system_message(messages: &[AgentMessage]) -> Option<SystemMessage> {
    let mut content: Vec<String> = Vec::new();
    let mut sections: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut timestamp: Option<i64> = None;

    for message in messages {
        let AgentMessage::Message(Message::System(system)) = message else {
            continue;
        };
        timestamp.get_or_insert(system.timestamp);
        let text = content_text(&system.content);
        if !text.is_empty() {
            content.push(text);
        }
        for (name, value) in &system.sections {
            match value {
                Some(value) => {
                    sections.insert(name.clone(), value.clone());
                }
                None => {
                    sections.remove(name);
                }
            }
        }
    }

    let tools = current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }

    let joined = content.join("\n\n");
    Some(SystemMessage {
        content: if joined.is_empty() {
            Vec::new()
        } else {
            vec![ContentBlock::Text(ai::TextContent {
                text: joined,
                text_signature: None,
            })]
        },
        sections: sections
            .into_iter()
            .map(|(name, text)| (name, Some(text)))
            .collect(),
        tools_added: tools,
        tools_removed: Vec::new(),
        timestamp: timestamp.unwrap_or(0),
    })
}

/// 重放 system 消息得到当前 prompt 文本（pi `getCurrentSystemPrompt`）。
pub fn current_system_prompt(messages: &[AgentMessage]) -> String {
    current_system_message(messages)
        .map(|system| system_message_text(&system))
        .unwrap_or_default()
}

/// 把 system 消息渲染为完整 prompt：content 后接各 section（pi `getSystemMessageText`）。
pub fn system_message_text(message: &SystemMessage) -> String {
    let mut parts = vec![content_text(&message.content)];
    for text in message.sections.values().flatten() {
        parts.push(text.clone());
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn content_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const NO_CHANGES: ToolStateChanges = ToolStateChanges {
    tools_added: Vec::new(),
    tools_removed: Vec::new(),
};

/// 工具装载变化（pi `ToolStateChanges`）。
#[derive(Debug, Clone, Default)]
struct ToolStateChanges {
    tools_added: Vec<Tool>,
    tools_removed: Vec<ai::ToolReference>,
}

/// 把可执行工具集相对 transcript 的差异声明成 system 消息（pi `declareToolChanges`）。
fn declare_tool_changes(context: &AgentContext, pending: Vec<AgentMessage>) -> Vec<AgentMessage> {
    let system_index = pending.iter().rposition(is_system_message);
    let baseline: Vec<AgentMessage> = match system_index {
        Some(index) => pending
            .iter()
            .enumerate()
            .map(|(position, message)| {
                if position == index {
                    with_tool_changes(message, &NO_CHANGES)
                } else {
                    message.clone()
                }
            })
            .collect(),
        None => pending.clone(),
    };

    let mut transcript = context.messages.clone();
    transcript.extend(baseline.iter().cloned());
    let previous = current_tools(&transcript);
    let desired: Vec<Tool> = context.tools.iter().map(AgentTool::declaration).collect();
    let changes = tool_state_changes(&previous, &desired);
    let unchanged = changes.tools_added.is_empty() && changes.tools_removed.is_empty();

    if let Some(index) = system_index {
        let declares_changes = matches!(
            &pending[index],
            AgentMessage::Message(Message::System(system))
                if !system.tools_added.is_empty() || !system.tools_removed.is_empty()
        );
        if unchanged && !declares_changes {
            return pending;
        }
        return baseline
            .iter()
            .enumerate()
            .map(|(position, message)| {
                if position == index {
                    with_tool_changes(message, &changes)
                } else {
                    message.clone()
                }
            })
            .collect();
    }

    if unchanged {
        return pending;
    }

    let update = AgentMessage::Message(Message::System(with_tool_changes_message(
        &SystemMessage {
            content: Vec::new(),
            sections: Default::default(),
            tools_added: Vec::new(),
            tools_removed: Vec::new(),
            timestamp: now_ms(),
        },
        &changes,
    )));
    let insert_index = pending
        .iter()
        .position(|message| !is_system_message(message))
        .unwrap_or(pending.len());
    let mut result = pending;
    result.insert(insert_index, update);
    result
}

fn with_tool_changes(message: &AgentMessage, changes: &ToolStateChanges) -> AgentMessage {
    match message {
        AgentMessage::Message(Message::System(system)) => {
            AgentMessage::Message(Message::System(with_tool_changes_message(system, changes)))
        }
        other => other.clone(),
    }
}

fn with_tool_changes_message(message: &SystemMessage, changes: &ToolStateChanges) -> SystemMessage {
    SystemMessage {
        content: message.content.clone(),
        sections: message.sections.clone(),
        tools_added: changes.tools_added.clone(),
        tools_removed: changes.tools_removed.clone(),
        timestamp: message.timestamp,
    }
}

fn is_system_message(message: &AgentMessage) -> bool {
    matches!(message, AgentMessage::Message(Message::System(_)))
}

/// 重放 transcript 里所有 system 消息的工具增量（pi `getCurrentTools`）。
fn current_tools(messages: &[AgentMessage]) -> Vec<Tool> {
    let mut tools: Vec<Tool> = Vec::new();
    for message in messages {
        let AgentMessage::Message(Message::System(system)) = message else {
            continue;
        };
        for removed in &system.tools_removed {
            tools.retain(|tool| tool.name != removed.name);
        }
        for added in &system.tools_added {
            match tools.iter_mut().find(|tool| tool.name == added.name) {
                Some(existing) => *existing = added.clone(),
                None => tools.push(added.clone()),
            }
        }
    }
    tools
}

/// 比较两个完整工具状态（pi `getToolStateChanges`）。
fn tool_state_changes(previous: &[Tool], current: &[Tool]) -> ToolStateChanges {
    let added = current
        .iter()
        .filter(|tool| {
            previous
                .iter()
                .find(|candidate| candidate.name == tool.name)
                .is_none_or(|existing| !declarations_equal(existing, tool))
        })
        .cloned()
        .collect();
    let removed = previous
        .iter()
        .filter(|tool| {
            current
                .iter()
                .find(|candidate| candidate.name == tool.name)
                .is_none_or(|existing| !declarations_equal(tool, existing))
        })
        .map(|tool| ai::ToolReference {
            name: tool.name.clone(),
        })
        .collect();
    ToolStateChanges {
        tools_added: added,
        tools_removed: removed,
    }
}

/// 两个工具是否对模型声明同一个接口（pi `declarationsEqual`）。
///
/// `label` 只用于 UI，不参与比较。
fn declarations_equal(left: &Tool, right: &Tool) -> bool {
    left.name == right.name
        && left.description == right.description
        && left.parameters == right.parameters
}
