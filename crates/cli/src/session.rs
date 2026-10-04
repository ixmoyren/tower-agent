//! 会话入口：把 agent、内置工具与（可选的）TUI 拼成一个可运行的会话。
//!
//! 一层的职责划分：
//!
//! - [`Session`] 持有 `agent::Agent`（编排循环 + 队列 + 事件总线）与内置工具集；
//! - 事件通过 [`Session::subscribe`] 拿到的 `agent::Subscription` 流出，
//!   既可以交给 [`tui::run_agent_tui`] 渲染，也可以用 [`run_print`] 打成纯文本（便于脚本与测试）；
//! - provider 由调用方注入（[`Session::new`] 的 `stream` 参数），真实的 provider 层尚未移植，
//!   所以二进制默认用 [`crate::faux::FauxProvider`]。

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use agent::{Agent, AgentError, AgentEvent, AgentMessage, AgentOptions, AgentState, Subscription};
use ai::{BoxStreamService, Message, Model, UserContent, UserMessage};
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
    pub fn new(cwd: impl Into<PathBuf>, stream: BoxStreamService) -> Self {
        Self::with_model(cwd, stream, default_model())
    }

    /// 用显式模型构造会话。
    pub fn with_model(cwd: impl Into<PathBuf>, stream: BoxStreamService, model: Model) -> Self {
        let cwd = cwd.into();
        let mut options = AgentOptions::new(stream);
        options.initial.tools = tools::default_tools(cwd.clone());
        options.initial.model = Some(model);
        options.initial.system_prompt = Some(system_prompt());
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

/// 会话的系统提示：告诉模型有哪些工具以及 `!` 前缀是给 faux provider 用的。
fn system_prompt() -> String {
    let names = tools::tool_names().join(", ");
    format!(
        "你是 tower-pi-agent（pi 的 Rust 移植版）的编码助手。可用工具：{names}。\n\
         使用工具而不是猜测文件内容；修改文件前先读一遍。\n\
         离线 faux provider 下，以 `!<工具名> <JSON 参数>` 开头的用户消息会被翻译成一次工具调用。"
    )
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
) -> std::io::Result<()> {
    let subscription = session.subscribe();
    let (submissions_tx, mut submissions_rx) = mpsc::unbounded_channel::<String>();

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

    let rendered = tui::run_agent_tui(subscription, submissions_tx, title.to_string()).await;

    // 用户退出：取消进行中的一轮，并等驱动任务自然收尾（避免直接 abort 丢掉取消语义）。
    session.abort();
    let driver_result = driver.await;

    rendered?;
    if let Ok(Some(error)) = driver_result {
        return Err(std::io::Error::other(error.to_string()));
    }
    Ok(())
}
