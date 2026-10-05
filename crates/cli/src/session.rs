//! 会话入口：把 agent、内置工具与（可选的）TUI 拼成一个可运行的会话。
//!
//! 一层的职责划分：
//!
//! - [`Session`] 持有 `agent::Agent`（编排循环 + 队列 + 事件总线）与内置工具集；
//! - 事件通过 [`Session::subscribe`] 拿到的 `agent::Subscription` 流出，
//!   既可以交给 [`tui::run_agent_tui`] 渲染，也可以用 [`run_print`] 打成纯文本（便于脚本与测试）；
//! - provider 由调用方注入（[`Session::new`] 的 `stream` 参数）；测试用确定性的离线
//!   provider [`ai::provider::faux::FauxProvider`]，二进制则接真实 provider。

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use agent::{
    Agent, AgentError, AgentEvent, AgentMessage, AgentOptions, AgentState, AgentTool, Subscription,
};
use ai::{BoxStreamService, Message, Model, ModelThinkingLevel, UserContent, UserMessage};
use tokio::sync::mpsc;

/// 会话。
pub struct Session {
    agent: Agent,
    cwd: PathBuf,
}

impl Session {
    /// 用工作目录和一个 provider 流服务构造会话，并注册全部内置工具。
    ///
    /// 默认把 provider 报出来的模型记进 transcript；[`Session::with_model`] 可以覆盖。
    pub fn new(cwd: impl AsRef<Path>, stream: BoxStreamService) -> Self {
        Self::with_model(cwd, stream, default_model())
    }

    /// 用显式模型构造会话。
    pub fn with_model(cwd: impl AsRef<Path>, stream: BoxStreamService, model: Model) -> Self {
        let cwd = cwd.as_ref().to_path_buf();
        let system_prompt = format!(
            "你是 tower-pi-agent（pi 的 Rust 移植版）的编码助手。可用工具：{}。\n\
         使用工具而不是猜测文件内容；修改文件前先读一遍。",
            tools::DEFAULT_TOOL_NAMES.join(", ")
        );
        let mut options = AgentOptions::new(stream);
        options.initial.tools = tools::default_tools(&cwd);
        options.initial.model = Some(model);
        options.initial.system_prompt = Some(system_prompt);
        Self {
            agent: Agent::new(options),
            cwd,
        }
    }

    /// 会话的工作目录。
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 订阅会话事件流（交给 TUI 或自定义渲染器）。
    pub fn subscribe(&self) -> Subscription {
        self.agent.subscribe()
    }

    /// 当前状态快照。
    pub fn state(&self) -> AgentState {
        self.agent.state()
    }

    /// 发一条用户消息并等待整轮结束（助手回复 + 工具执行都完成）。
    pub async fn ask(&self, text: impl Into<String>) -> Result<(), AgentError> {
        self.agent.prompt_text(text).await
    }

    /// 从当前 transcript 继续一轮（最后一条必须不是 assistant）。
    pub async fn continue_run(&self) -> Result<(), AgentError> {
        self.agent.continue_run().await
    }

    /// 提交一条用户输入。
    ///
    /// 空闲时直接开启新一轮；正在跑的时候用 `steer` 注入，等当前 turn 结束后生效——
    /// 这样用户在 agent 工作期间继续输入不会撞上 "already processing"。
    pub async fn submit(&self, text: impl Into<String>) -> Result<(), AgentError> {
        let text = text.into();
        if self.agent.is_streaming() {
            self.agent.steer(user_message(text));
            Ok(())
        } else {
            self.agent.prompt_text(text).await
        }
    }

    /// 取消进行中的一轮。
    pub fn abort(&self) {
        self.agent.abort();
    }

    /// 替换当前模型（`--model` 与 TUI 的 `/model` 都走这里）。
    pub fn set_model(&self, model: Model) {
        self.agent.set_model(model);
    }

    /// 替换思考级别（`--thinking` 与 `/thinking`）。
    pub fn set_thinking_level(&self, level: ModelThinkingLevel) {
        self.agent.set_thinking_level(level);
    }

    /// 替换可执行工具集（`--tools` / `--exclude-tools` / `--no-tools`）。
    pub fn set_tools(&self, tools: Vec<AgentTool>) {
        self.agent.set_tools(tools);
    }

    /// 重置会话（`/new`）：清空 transcript 与队列，保留 system prompt 与工具。
    pub fn reset(&self) -> Result<(), AgentError> {
        self.agent.reset()
    }
}

/// `off` / `minimal` / … / `max` → [`ModelThinkingLevel`]（pi `--thinking` / `/thinking` 的取值）。
#[must_use]
pub fn parse_thinking_level(value: &str) -> Option<ModelThinkingLevel> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Some(ModelThinkingLevel::Off),
        "minimal" => Some(ModelThinkingLevel::Level(ai::ThinkingLevel::Minimal)),
        "low" => Some(ModelThinkingLevel::Level(ai::ThinkingLevel::Low)),
        "medium" => Some(ModelThinkingLevel::Level(ai::ThinkingLevel::Medium)),
        "high" => Some(ModelThinkingLevel::Level(ai::ThinkingLevel::High)),
        "xhigh" => Some(ModelThinkingLevel::Level(ai::ThinkingLevel::XHigh)),
        "max" => Some(ModelThinkingLevel::Level(ai::ThinkingLevel::Max)),
        _ => None,
    }
}

/// [`ModelThinkingLevel`] → pi 的取值字符串。
#[must_use]
pub fn thinking_level_name(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Level(ai::ThinkingLevel::Minimal) => "minimal",
        ModelThinkingLevel::Level(ai::ThinkingLevel::Low) => "low",
        ModelThinkingLevel::Level(ai::ThinkingLevel::Medium) => "medium",
        ModelThinkingLevel::Level(ai::ThinkingLevel::High) => "high",
        ModelThinkingLevel::Level(ai::ThinkingLevel::XHigh) => "xhigh",
        ModelThinkingLevel::Level(ai::ThinkingLevel::Max) => "max",
    }
}

/// pi 的思考级别取值（帮助文案与错误提示共用）。
pub const THINKING_LEVELS: &str = "off, minimal, low, medium, high, xhigh, max";

/// 用户可选的 provider 列表（`/model` 的错误提示用；不含测试用的 faux）。
#[must_use]
pub fn available_providers() -> String {
    crate::provider::ProviderKind::ALL
        .iter()
        .map(|kind| kind.canonical_name())
        .collect::<Vec<_>>()
        .join(" / ")
}

impl tui::CommandHost for Session {
    fn new_session(&self) -> Result<(), String> {
        self.reset().map_err(|error| error.to_string())
    }

    fn session_info(&self) -> Vec<String> {
        let state = self.state();
        vec![
            format!("cwd: {}", self.cwd().display()),
            format!(
                "model: {} (provider {} / api {})",
                state.model.id, state.model.provider, state.model.api
            ),
            format!("thinking: {}", thinking_level_name(state.thinking_level)),
            format!(
                "messages: {} · tools: {}",
                state.messages.len(),
                state.tools.len()
            ),
            format!(
                "state: {}",
                if state.is_streaming {
                    "running"
                } else {
                    "idle"
                }
            ),
        ]
    }

    fn model(&self, arg: Option<&str>) -> Result<String, String> {
        let current = self.state().model;
        let Some(arg) = arg else {
            return Ok(format!(
                "current model: {} (provider {} / api {})\nusage: /model <provider>/<id> or /model <id>",
                current.id, current.provider, current.api
            ));
        };

        let (requested, id) = match arg.split_once('/') {
            Some((provider, id)) => (Some(provider.trim()), id.trim()),
            None => (None, arg.trim()),
        };
        if id.is_empty() {
            return Err("model id must not be empty. usage: /model <provider>/<id>".into());
        }

        // 没写 `provider/` 时：先看它是不是 provider 名（用它的默认模型），否则按当前
        // provider 的模型 id 处理。
        let kind = match requested {
            Some(provider) => crate::provider::ProviderKind::parse(provider).ok_or_else(|| {
                format!(
                    "unknown provider: {provider} (available: {})",
                    available_providers()
                )
            })?,
            None => match crate::provider::ProviderKind::parse(id) {
                Some(kind) => kind,
                None => crate::provider::ProviderKind::ALL
                    .into_iter()
                    // 先按 (provider, api) 精确匹配（openai 与 deepseek 共用
                    // `openai-completions`）；再退回只按 provider 标识匹配。
                    .find(|kind| kind.id() == current.provider && kind.api() == current.api)
                    .or_else(|| {
                        crate::provider::ProviderKind::ALL
                            .into_iter()
                            .find(|kind| kind.id() == current.provider)
                    })
                    .ok_or_else(|| {
                        format!(
                            "cannot determine the current provider ({})",
                            current.provider
                        )
                    })?,
            },
        };
        let model_id = match requested {
            Some(_) => id,
            None => {
                if crate::provider::ProviderKind::parse(id).is_some() {
                    kind.default_model_id()
                } else {
                    id
                }
            }
        };

        if kind.id() != current.provider {
            return Err(format!(
                "this port fixes the provider at startup (currently {}); restart with \
                 `--provider` to switch.",
                current.provider
            ));
        }

        let model = crate::provider::model_for(kind, model_id);
        self.set_model(model.clone());
        Ok(format!(
            "model switched to: {} (provider {})",
            model.id, model.provider
        ))
    }

    fn thinking(&self, arg: Option<&str>) -> Result<String, String> {
        match arg {
            None => Ok(format!(
                "current thinking level: {}\nusage: /thinking <{}>",
                thinking_level_name(self.state().thinking_level),
                THINKING_LEVELS
            )),
            Some(value) => match parse_thinking_level(value) {
                Some(level) => {
                    self.set_thinking_level(level);
                    Ok(format!(
                        "thinking level set to: {}",
                        thinking_level_name(level)
                    ))
                }
                None => Err(format!(
                    "unknown thinking level: {value} (available: {THINKING_LEVELS})"
                )),
            },
        }
    }

    fn login(&self, arg: Option<&str>) -> Result<String, String> {
        let Some(arg) = arg else {
            return Ok(format!(
                "usage: /login <provider> <api-key>\navailable providers: {}",
                available_providers()
            ));
        };
        let mut parts = arg.splitn(2, char::is_whitespace);
        let provider = parts.next().unwrap_or_default().trim();
        let api_key = parts.next().map(str::trim).filter(|key| !key.is_empty());
        let kind = crate::provider::ProviderKind::parse(provider).ok_or_else(|| {
            format!(
                "unknown provider: {provider} (available: {})",
                available_providers()
            )
        })?;

        // 没带 key 时退回环境变量；两条路都缺凭据就报错。
        let options = crate::provider::ProviderStackOptions::default();
        let setup = match api_key {
            Some(api_key) => {
                crate::provider::build_with_api_key(kind, None, options, Some(api_key))
            }
            None => crate::provider::build(kind, None, options),
        }
        .map_err(|error| error.to_string())?;

        self.agent.set_stream(setup.stream_fn());
        self.agent.set_model(setup.model.clone());
        Ok(format!(
            "provider configured: {} (model {})",
            kind.canonical_name(),
            setup.model.id
        ))
    }
}

/// 构造一条用户消息。
fn user_message(text: impl Into<String>) -> AgentMessage {
    AgentMessage::Message(Message::User(UserMessage {
        content: UserContent::Text(text.into()),
        timestamp: agent::agent_loop::now_ms(),
    }))
}

/// provider 未上报模型时的占位模型。
fn default_model() -> Model {
    Model {
        id: "unknown".into(),
        name: "unknown".into(),
        api: "unknown".into(),
        provider: "unknown".into(),
        ..Model::default()
    }
}

/// 把事件流打印成纯文本（`--print`）。
///
/// 只打印对人有意义的事件：用户/助手消息、工具开始与结束、错误。
pub async fn run_print<W: Write>(
    mut subscription: Subscription,
    mut out: W,
) -> std::io::Result<()> {
    while let Some(event) = subscription.recv().await {
        let is_end = matches!(event, AgentEvent::AgentEnd { .. });
        write_event(&mut out, &event)?;
        if is_end {
            break;
        }
    }
    out.flush()
}

/// 单条事件的文本渲染。
pub fn write_event<W: Write>(out: &mut W, event: &AgentEvent) -> std::io::Result<()> {
    match event {
        AgentEvent::AgentStart => writeln!(out, "[agent] start"),
        AgentEvent::AgentEnd { .. } => writeln!(out, "[agent] end"),
        AgentEvent::MessageEnd { message } => match message {
            agent::AgentMessage::Message(Message::User(_)) => {
                writeln!(out, "[user] {}", tui::render_message(message))
            }
            agent::AgentMessage::Message(Message::Assistant(_)) => {
                writeln!(out, "[assistant] {}", tui::render_message(message))
            }
            agent::AgentMessage::Message(Message::System(_)) => Ok(()),
            // 工具结果由 tool_execution_end 那行呈现。
            _ => Ok(()),
        },
        AgentEvent::ToolExecutionStart {
            tool_name, args, ..
        } => writeln!(out, "[tool] {tool_name} {args}"),
        AgentEvent::ToolExecutionEnd {
            tool_name,
            result,
            is_error,
            ..
        } => writeln!(
            out,
            "[tool:{}] {tool_name} {}",
            if *is_error { "error" } else { "ok" },
            tui::render_message(&agent::AgentMessage::Message(Message::ToolResult(
                ai::ToolResultMessage {
                    tool_call_id: String::new(),
                    tool_name: tool_name.clone(),
                    content: result.content.clone(),
                    details: None,
                    is_error: *is_error,
                    timestamp: 0,
                }
            )))
        ),
        _ => Ok(()),
    }
}

/// 用 TUI 渲染这条会话：先订阅，再跑一轮，最后交给 [`tui::run_agent_tui`]。
///
/// 订阅事件、把输入区提交的文本交给 agent，直到用户退出；`q`/`Esc`/`Ctrl+C` 退出。
///
/// 编辑器默认 vim 普通模式（`i` 插入 / `Esc` 普通 / `Enter` 提交 / `q` 退出）。
/// 用户退出时会取消还在进行的一轮，等驱动任务收尾后返回。
pub async fn run_tui(
    session: &Arc<Session>,
    prompt: Option<String>,
    title: &str,
    notice: Option<String>,
) -> std::io::Result<()> {
    let subscription = session.subscribe();
    let (submissions_tx, mut submissions_rx) = mpsc::unbounded_channel::<String>();
    // 斜杠命令（`/new`、`/model`…）在 TUI 里执行，通过这个 trait 回调到会话。
    let host: Arc<dyn tui::CommandHost> = session.clone();

    let driver_session = Arc::clone(session);
    let driver = tokio::spawn(async move {
        let mut first_error: Option<AgentError> = None;
        if let Some(prompt) = prompt
            && let Err(error) = driver_session.submit(prompt).await
        {
            first_error = Some(error);
        }
        while let Some(text) = submissions_rx.recv().await {
            if let Err(error) = driver_session.submit(text).await {
                first_error.get_or_insert(error);
            }
        }
        first_error
    });

    let rendered = tui::run_agent_tui(
        subscription,
        submissions_tx,
        host,
        title.to_string(),
        notice,
    )
    .await;

    // 用户退出：取消进行中的一轮，并等驱动任务自然收尾（避免直接 abort 丢掉取消语义）。
    session.abort();
    let driver_result = driver.await;

    rendered?;
    if let Ok(Some(error)) = driver_result {
        return Err(std::io::Error::other(error.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! 会话入口的端到端测试：用离线 faux provider 驱动真实的 agent 循环 + 真实内置工具。

    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
    };

    use ai::{
        AssistantEventStream, AssistantMessage, AssistantMessageEvent, BoxStreamService,
        ContentBlock, DoneReason, PiError, StopReason, StreamRequest, TextContent, Usage,
        provider::faux::{FauxProvider, model},
    };
    use futures_util::{future::BoxFuture, stream};
    use tokio::sync::Semaphore;
    use tower::Service;
    use tui::CommandHost as _;

    use super::{Session, parse_thinking_level, run_print, thinking_level_name};

    fn session(cwd: &std::path::Path) -> Session {
        Session::with_model(cwd, FauxProvider::new().boxed(), model())
    }

    /// 第一次调用会卡在闸门上的 provider；之后的调用立刻返回，用来观察 steer 行为。
    #[derive(Clone)]
    struct GatedProvider {
        gate: Arc<Semaphore>,
        calls: Arc<AtomicUsize>,
    }

    impl GatedProvider {
        fn new(gate: Arc<Semaphore>) -> Self {
            Self {
                gate,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn boxed(self) -> BoxStreamService {
            tower::util::BoxCloneService::new(self)
        }
    }

    impl Service<StreamRequest> for GatedProvider {
        type Response = AssistantEventStream;
        type Error = PiError;
        type Future = BoxFuture<'static, Result<AssistantEventStream, PiError>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: StreamRequest) -> Self::Future {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let gate = self.gate.clone();
            let model = request.model.clone();
            Box::pin(async move {
                if call == 0 {
                    let _permit = gate.acquire().await;
                }
                Ok(AssistantEventStream::new(stream::iter(text_events(
                    &model,
                    &format!("reply-{call}"),
                ))))
            })
        }
    }

    /// 一条最简的文本事件流（`start → text_* → done`）。
    fn text_events(model: &ai::Model, text: &str) -> Vec<AssistantMessageEvent> {
        let message = |stop_reason: StopReason, content: Vec<ContentBlock>| AssistantMessage {
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
        };
        let block = |value: &str| {
            ContentBlock::Text(TextContent {
                text: value.into(),
                text_signature: None,
            })
        };
        vec![
            AssistantMessageEvent::Start {
                partial: message(StopReason::Pending, Vec::new()),
            },
            AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: message(StopReason::Pending, vec![block("")]),
            },
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: text.into(),
                partial: message(StopReason::Pending, vec![block(text)]),
            },
            AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: text.into(),
                partial: message(StopReason::Pending, vec![block(text)]),
            },
            AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: message(StopReason::Stop, vec![block(text)]),
            },
        ]
    }

    fn last_assistant_text(state: &agent::AgentState) -> String {
        state
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                agent::AgentMessage::Message(ai::Message::Assistant(assistant)) => {
                    Some(tui::render_message(&agent::AgentMessage::Message(
                        ai::Message::Assistant(assistant.clone()),
                    )))
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn ask_returns_a_text_reply_without_tools() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = session(dir.path());
        session.ask("你好").await.expect("turn succeeds");

        let state = session.state();
        assert_eq!(state.messages.len(), 3, "system + user + assistant");
        assert!(last_assistant_text(&state).contains("[faux] 收到：你好"));
        assert!(!state.is_streaming);
    }

    #[tokio::test]
    async fn ask_executes_a_real_tool_and_feeds_the_result_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("note.txt"), "工具读到的内容\n").unwrap();
        let session = session(dir.path());

        // faux provider 把 `!read {…}` 翻译成一次工具调用；agent 执行真实 read 工具，
        // 把工具结果写回 transcript，再由 faux provider 收尾。
        session
            .ask("!read {\"path\":\"note.txt\"}")
            .await
            .expect("turn succeeds");

        let state = session.state();
        let roles: Vec<Option<&str>> = state
            .messages
            .iter()
            .map(agent::AgentMessage::role)
            .collect();
        assert_eq!(
            roles,
            vec![
                Some("system"),
                Some("user"),
                Some("assistant"),
                Some("toolResult"),
                Some("assistant"),
            ],
            "user → tool call → tool result → final reply"
        );

        let tool_result = state
            .messages
            .iter()
            .find_map(|message| match message {
                agent::AgentMessage::Message(ai::Message::ToolResult(result)) => Some(result),
                _ => None,
            })
            .expect("a tool result message");
        assert_eq!(tool_result.tool_name, "read");
        assert!(!tool_result.is_error);
        assert_eq!(
            tui::render_message(&agent::AgentMessage::Message(ai::Message::ToolResult(
                tool_result.clone()
            ))),
            "工具读到的内容\n"
        );
        assert!(last_assistant_text(&state).contains("工具结果已收到"));
    }

    #[tokio::test]
    async fn unknown_tool_is_reported_as_an_error_result() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = session(dir.path());
        session.ask("!nope {}").await.expect("turn succeeds");

        let result = session
            .state()
            .messages
            .iter()
            .find_map(|message| match message {
                agent::AgentMessage::Message(ai::Message::ToolResult(result)) => {
                    Some(result.clone())
                }
                _ => None,
            })
            .expect("a tool result message");
        assert!(result.is_error, "unknown tool produces an error result");
        assert!(
            tui::render_message(&agent::AgentMessage::Message(ai::Message::ToolResult(
                result
            )))
            .contains("Tool nope not found")
        );
    }

    /// 用户在 agent 工作期间继续输入时，`submit` 应该走 steer 而不是报 "already processing"。
    #[tokio::test]
    async fn submit_steers_while_a_turn_is_running() {
        let dir = tempfile::tempdir().expect("temp dir");
        let gate = Arc::new(Semaphore::new(0));
        let session = Arc::new(Session::with_model(
            dir.path(),
            GatedProvider::new(Arc::clone(&gate)).boxed(),
            model(),
        ));

        // 第一轮会卡在 provider 的闸门上。
        let first = {
            let session = Arc::clone(&session);
            tokio::spawn(async move { session.submit("first").await })
        };
        while !session.state().is_streaming {
            tokio::task::yield_now().await;
        }

        // 第二轮输入在流式期间提交：应当被 steer 接受。
        session
            .submit("second")
            .await
            .expect("steering while streaming is accepted");

        // 放行第一轮，steer 的消息会在下一个 turn 被注入。
        gate.add_permits(1);
        first
            .await
            .expect("task joins")
            .expect("first turn succeeds");

        let state = session.state();
        let user_texts: Vec<String> = state
            .messages
            .iter()
            .filter_map(|message| match message {
                agent::AgentMessage::Message(ai::Message::User(user)) => match &user.content {
                    ai::UserContent::Text(text) => Some(text.clone()),
                    ai::UserContent::Blocks(_) => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(user_texts, vec!["first", "second"]);
        let assistants = state
            .messages
            .iter()
            .filter(|message| matches!(message.role(), Some("assistant")))
            .count();
        assert_eq!(assistants, 2, "steer 触发第二个 turn");
        assert!(!state.is_streaming);
    }

    #[tokio::test]
    async fn session_registers_all_builtin_tools() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = session(dir.path());
        let state = session.state();
        let names: Vec<&str> = state.tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(names, *tools::DEFAULT_TOOL_NAMES);
        assert!(state.system_prompt.contains("read"));
    }

    #[tokio::test]
    async fn print_renderer_writes_the_whole_turn() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("note.txt"), "内容\n").unwrap();
        let session = session(dir.path());
        let subscription = session.subscribe();

        let mut out: Vec<u8> = Vec::new();
        let (asked, printed) = tokio::join!(
            session.ask("!read {\"path\":\"note.txt\"}"),
            run_print(subscription, &mut out)
        );
        asked.expect("turn succeeds");
        printed.expect("printing succeeds");

        let text = String::from_utf8(out).expect("utf-8 output");
        assert!(text.contains("[agent] start"), "got: {text}");
        assert!(text.contains("[user] !read"), "got: {text}");
        assert!(text.contains("[tool] read"), "got: {text}");
        assert!(text.contains("[tool:ok] read"), "got: {text}");
        assert!(text.contains("[assistant]"), "got: {text}");
        assert!(text.contains("[agent] end"), "got: {text}");
    }

    #[test]
    fn thinking_levels_round_trip_pi_names() {
        for name in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let level = parse_thinking_level(name).expect("known level");
            assert_eq!(thinking_level_name(level), name);
            // 大小写不敏感，和 pi 一样。
            assert_eq!(parse_thinking_level(&name.to_uppercase()), Some(level));
        }
        assert!(parse_thinking_level("nope").is_none());
    }

    #[tokio::test]
    async fn command_host_reports_and_updates_session_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let setup = crate::provider::build_with_api_key(
            crate::provider::ProviderKind::Anthropic,
            Some("claude-test"),
            crate::provider::ProviderStackOptions::default(),
            Some("test-key"),
        )
        .expect("provider setup");
        let session = Session::with_model(dir.path(), setup.stream_fn(), setup.model.clone());

        let info = session.session_info().join("\n");
        assert!(info.contains("claude-test"), "got: {info}");
        assert!(info.contains("anthropic"), "got: {info}");

        // 同一 provider 下换模型 id：真的换掉。
        let message = session.model(Some("claude-other")).expect("switch model");
        assert!(message.contains("claude-other"), "got: {message}");
        assert_eq!(session.state().model.id, "claude-other");
        assert_eq!(session.state().model.provider, "anthropic");

        // 换 provider 需要重启（本移植版的 stream service 在启动时固定）。
        assert!(session.model(Some("faux/faux")).is_err());

        // 思考级别：读回 pi 的名字。
        session.thinking(Some("high")).expect("set thinking");
        assert_eq!(thinking_level_name(session.state().thinking_level), "high");
        assert!(session.thinking(Some("nope")).is_err());
    }

    #[tokio::test]
    async fn command_host_login_configures_a_provider() {
        let dir = tempfile::tempdir().expect("temp dir");
        // 会话开始时没有 provider（CLI 无 --provider / 无环境凭据时的占位）。
        let setup = crate::provider::unconfigured_setup();
        let session = Session::with_model(dir.path(), setup.stream_fn(), setup.model.clone());
        assert_eq!(session.state().model.provider, "unconfigured");

        // `/login <provider> <api-key>` 在 TUI 里换掉 provider 服务与模型。
        let message = session
            .login(Some("anthropic sk-test"))
            .expect("login configures the provider");
        assert!(message.contains("anthropic"), "got: {message}");
        assert_eq!(session.state().model.provider, "anthropic");
        assert!(
            session.state().model.id.starts_with("claude"),
            "got: {}",
            session.state().model.id
        );

        // deepseek 复用 OpenAI 兼容实现，但 provider/api 标识独立。
        session
            .login(Some("deepseek sk-test"))
            .expect("login deepseek");
        assert_eq!(session.state().model.provider, "deepseek");
        assert_eq!(session.state().model.api, "openai-completions");
        assert_eq!(session.state().model.id, "deepseek-chat");

        // 无参数给用法；未知 provider / 缺 key 明确报错。
        assert!(
            session
                .login(None)
                .expect("usage")
                .contains("/login <provider> <api-key>")
        );
        assert!(session.login(Some("nope key")).is_err());
    }

    #[tokio::test]
    async fn new_session_resets_the_transcript() {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = session(dir.path());
        session.ask("你好").await.expect("turn succeeds");
        assert_eq!(
            session.state().messages.len(),
            3,
            "system + user + assistant"
        );

        session.new_session().expect("reset");
        assert_eq!(session.state().messages.len(), 1, "只剩 system 基线");
    }
}
