//! 上下文压缩（pi `core/compaction`）。
//!
//! 上下文接近模型窗口上限时，旧消息会被交给 LLM 压成一段**结构化摘要**；之后模型
//! 只看到「system + 摘要 + 最近保留的消息」。**原始 transcript 不被改写**，只是投影
//! 给模型看（pi 同样是「原始条目仍在，只是不再进 LLM context」）。
//!
//! 这是**核心能力**，不是某个前端的私事：
//!
//! - 宿主（CLI / axum / 测试 …）构造一个 [`Compactor`]，放进 [`crate::AgentOptions`]；
//! - agent 循环在每次模型请求前调用 [`Compactor::compact_if_needed`]，按阈值自动压缩；
//! - 宿主也可以自己触发：异步宿主直接 `await` [`Compactor::compact`]；同步宿主
//!   （如 TUI 的斜杠命令）用 [`Compactor::request_compaction`] 排到下一个请求前。

use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;
use tower::ServiceExt as _;
use tower_agent_provider::{
    AssistantMessageEvent, BoxStreamService, ContentBlock, Message, Model, SimpleStreamOptions,
    StreamRequest, TranscriptContext, UserContent, UserMessage,
};

use crate::{
    AgentMessage,
    agent_loop::{BoxFut, now_ms},
};

/// 压缩设置（对齐 pi `settings.json` 的 `compaction` 段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionSettings {
    /// 是否开启自动压缩。
    pub enabled: bool,
    /// 给模型回复预留的 token（阈值 = `context_window - reserve_tokens`）。
    pub reserve_tokens: u64,
    /// 压缩后至少保留的近期 token 量。
    pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        }
    }
}

/// 一次压缩的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compaction {
    /// 结构化摘要（pi 的 `CompactionEntry.summary`）。
    pub summary: String,
    /// 保留窗口在**原始 transcript** 里的起点下标；之前的消息不再直接发给模型。
    pub first_kept: usize,
    /// 压缩前可见上下文的估算 token 数（pi 的 `tokensBefore`）。
    pub tokens_before: u64,
}

impl Compaction {
    /// 把压缩投影到原始 transcript 上：`system + 摘要 + 保留的消息`。
    #[must_use]
    pub fn project(&self, messages: &[AgentMessage]) -> Vec<AgentMessage> {
        let mut out = Vec::with_capacity(messages.len().saturating_sub(self.first_kept) + 2);
        if is_system(messages.first()) {
            out.push(messages[0].clone());
        }
        out.push(summary_message(&self.summary));
        let start = self.first_kept.min(messages.len());
        out.extend(messages[start..].iter().cloned());
        out
    }
}

/// 压缩失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompactionError {
    /// 压缩被关闭。
    #[error("compaction is disabled")]
    Disabled,
    /// 没有可压缩的内容（保留窗口已覆盖全部历史）。
    #[error("nothing to compact")]
    NothingToCompact,
    /// 摘要请求失败（provider 建立请求或流错误）。
    #[error("summarization request failed: {0}")]
    Stream(String),
    /// 摘要响应里没有文本。
    #[error("summarization response had no text")]
    EmptySummary,
    /// 钩子取消了这次压缩（pi `session_before_compact` 返回 `cancel`）。
    #[error("compaction was cancelled by a hook")]
    Cancelled,
}

/// 这次压缩是被什么触发的（pi 的 `reason`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactionReason {
    /// 可见上下文超过阈值。
    Threshold,
    /// 宿主（人或上层）手动请求。
    Manual,
    /// provider 报上下文溢出，压缩后重试。
    Overflow,
    /// 响应被长度上限截断，压缩后重试。
    Length,
}

impl CompactionReason {
    /// 是否属于「压缩后重试当前请求」的恢复路径。
    #[must_use]
    pub fn is_recovery(self) -> bool {
        matches!(self, Self::Overflow | Self::Length)
    }
}

/// 压缩前的事件（pi 扩展事件 `session_before_compact`）。
#[derive(Debug, Clone)]
pub struct BeforeCompact {
    /// 触发原因。
    pub reason: CompactionReason,
    /// 压缩前可见上下文的估算 token 数。
    pub tokens_before: u64,
    /// 即将被总结并丢弃的消息。
    pub messages_to_summarize: Vec<AgentMessage>,
    /// 上一版摘要（重复压缩时）。
    pub previous_summary: Option<String>,
    /// 宿主给的自定义指令。
    pub instructions: Option<String>,
}

/// 钩子对本次压缩的决定。
#[derive(Debug, Clone)]
pub enum CompactionDecision {
    /// 取消这次压缩。
    Cancel,
    /// 直接用给定摘要，跳过内置摘要调用。
    Provide {
        /// 自定义摘要正文。
        summary: String,
    },
}

/// 压缩成功后的事件（pi 扩展事件 `session_compact`）。
#[derive(Debug, Clone)]
pub struct AfterCompact {
    /// 触发原因。
    pub reason: CompactionReason,
    /// 产生的压缩结果。
    pub compaction: Compaction,
    /// 摘要是否由钩子提供。
    pub from_hook: bool,
}

/// 压缩失败 / 取消后的事件（pi 扩展事件 `session_compact_failed`）。
#[derive(Debug, Clone)]
pub struct CompactionFailed {
    /// 触发原因。
    pub reason: CompactionReason,
    /// 失败原因（取消时为 `Cancelled` 的文案）。
    pub error: String,
}

/// 压缩钩子：宿主可拦截压缩、提供自定义摘要或做遥测。
///
/// 三个方法默认都是空实现，只实现需要的那几个即可。异步签名与运行时的其它钩子一致
/// （`Arc<dyn Fn> -> BoxFut` 风格）。
pub trait CompactionHook: Send + Sync + 'static {
    /// 压缩前：返回 `None` 走内置摘要，返回决定可取消或提供摘要。
    fn before(&self, event: &BeforeCompact) -> BoxFut<Option<CompactionDecision>> {
        let _ = event;
        Box::pin(async { None })
    }

    /// 压缩成功后。
    fn after(&self, event: &AfterCompact) -> BoxFut<()> {
        let _ = event;
        Box::pin(async {})
    }

    /// 压缩失败或被取消后。
    fn failed(&self, event: &CompactionFailed) -> BoxFut<()> {
        let _ = event;
        Box::pin(async {})
    }
}

/// 可复用的上下文压缩器。
///
/// 内部串行化：同一时刻只跑一次压缩；agent 循环在压缩进行时等待它完成，因此不会出现
/// 「投影读到一半」的状态。
pub struct Compactor {
    inner: Mutex<Inner>,
    gate: AsyncMutex<()>,
    hook: Mutex<Option<Arc<dyn CompactionHook>>>,
}

#[derive(Debug, Clone)]
struct Inner {
    settings: CompactionSettings,
    current: Option<Compaction>,
    /// `Some(instructions)` 表示宿主请求过「下一个请求前强制压缩」。
    request: Option<Option<String>>,
}

impl Compactor {
    /// 用设置构造。
    #[must_use]
    pub fn new(settings: CompactionSettings) -> Self {
        Self {
            inner: Mutex::new(Inner {
                settings,
                current: None,
                request: None,
            }),
            gate: AsyncMutex::new(()),
            hook: Mutex::new(None),
        }
    }

    /// 设置压缩钩子（builder 形态）。
    #[must_use]
    pub fn with_hook(self, hook: Arc<dyn CompactionHook>) -> Self {
        *self.hook.lock().expect("compactor poisoned") = Some(hook);
        self
    }

    /// 替换 / 清除压缩钩子。
    pub fn set_hook(&self, hook: Option<Arc<dyn CompactionHook>>) {
        *self.hook.lock().expect("compactor poisoned") = hook;
    }

    fn hook(&self) -> Option<Arc<dyn CompactionHook>> {
        self.hook.lock().expect("compactor poisoned").clone()
    }

    /// 恢复一份持久化的压缩（续接会话时用，避免重新摘要）。
    pub fn restore(&self, compaction: Compaction) {
        self.inner.lock().expect("compactor poisoned").current = Some(compaction);
    }

    /// 当前设置。
    #[must_use]
    pub fn settings(&self) -> CompactionSettings {
        self.inner.lock().expect("compactor poisoned").settings
    }

    /// 替换设置（例如宿主改了配置）。
    pub fn set_settings(&self, settings: CompactionSettings) {
        self.inner.lock().expect("compactor poisoned").settings = settings;
    }

    /// 当前生效的压缩（没有则为 `None`）。
    #[must_use]
    pub fn current(&self) -> Option<Compaction> {
        self.inner
            .lock()
            .expect("compactor poisoned")
            .current
            .clone()
    }

    /// 清空压缩状态与待执行请求（换会话时用）。
    pub fn clear(&self) {
        let mut inner = self.inner.lock().expect("compactor poisoned");
        inner.current = None;
        inner.request = None;
    }

    /// 请求在**下一个模型请求前**强制压缩一次（同步宿主用）。
    pub fn request_compaction(&self, instructions: Option<String>) {
        self.inner.lock().expect("compactor poisoned").request = Some(instructions);
    }

    /// 是否有一个尚未执行的强制压缩请求。
    #[must_use]
    pub fn has_pending_request(&self) -> bool {
        self.inner
            .lock()
            .expect("compactor poisoned")
            .request
            .is_some()
    }

    /// 把原始 transcript 投影成模型可见的消息（应用已有的压缩）。
    #[must_use]
    pub fn project(&self, messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
        match self.current() {
            Some(compaction) => compaction.project(&messages),
            None => messages,
        }
    }

    /// 可见上下文是否超过阈值（`context_window - reserve_tokens`）。
    #[must_use]
    pub fn over_threshold(&self, messages: &[AgentMessage], context_window: u64) -> bool {
        let settings = self.settings();
        if !settings.enabled || context_window == 0 {
            return false;
        }
        let reserve = settings
            .reserve_tokens
            .min(context_window.saturating_sub(1));
        estimate_tokens(messages) > context_window.saturating_sub(reserve)
    }

    /// 超阈值（或宿主请求过）就压缩；返回新产生的压缩结果。
    ///
    /// `messages` 是**原始 transcript**；函数内部先投影再判断。
    pub async fn compact_if_needed(
        &self,
        messages: &[AgentMessage],
        model: &Model,
        stream: BoxStreamService,
    ) -> Result<Option<Compaction>, CompactionError> {
        let (enabled, request) = {
            let inner = self.inner.lock().expect("compactor poisoned");
            (inner.settings.enabled, inner.request.clone())
        };
        let visible = self.project(messages.to_vec());
        let reason = if request.is_some() {
            CompactionReason::Manual
        } else if !enabled {
            return Ok(None);
        } else if self.over_threshold(&visible, model.context_window) {
            CompactionReason::Threshold
        } else {
            return Ok(None);
        };
        let instructions = request.flatten();
        let outcome = self
            .compact_with_reason(&visible, model, stream, instructions.as_deref(), reason)
            .await;
        // 无论成功与否，这条「强制压缩」请求都已消费掉。
        self.inner.lock().expect("compactor poisoned").request = None;
        outcome
    }

    /// 直接压缩一次（宿主手动触发）。
    ///
    /// `messages` 应是**可见**消息（即 [`Self::project`] 的结果）。
    pub async fn compact(
        &self,
        messages: &[AgentMessage],
        model: &Model,
        stream: BoxStreamService,
        instructions: Option<&str>,
    ) -> Result<Option<Compaction>, CompactionError> {
        self.compact_with_reason(
            messages,
            model,
            stream,
            instructions,
            CompactionReason::Manual,
        )
        .await
    }

    /// 恢复路径专用：不判断阈值，直接按给定原因压缩一次。
    pub async fn force_compact(
        &self,
        messages: &[AgentMessage],
        model: &Model,
        stream: BoxStreamService,
        reason: CompactionReason,
    ) -> Result<Option<Compaction>, CompactionError> {
        let visible = self.project(messages.to_vec());
        self.compact_with_reason(&visible, model, stream, None, reason)
            .await
    }

    async fn compact_with_reason(
        &self,
        messages: &[AgentMessage],
        model: &Model,
        stream: BoxStreamService,
        instructions: Option<&str>,
        reason: CompactionReason,
    ) -> Result<Option<Compaction>, CompactionError> {
        let _gate = self.gate.lock().await;
        let (current, settings) = {
            let inner = self.inner.lock().expect("compactor poisoned");
            (inner.current.clone(), inner.settings)
        };
        if !settings.enabled && current.is_none() {
            return Err(CompactionError::Disabled);
        }

        let Some(cut) = find_cut(messages, settings.keep_recent_tokens) else {
            return Err(CompactionError::NothingToCompact);
        };
        // 可见消息布局：`[system?] [上一版摘要?] 保留的消息…`
        let has_previous = current.is_some();
        let base_offset = usize::from(is_system(messages.first()));
        let summary_offset = base_offset + usize::from(has_previous);
        let to_summarize = &messages[summary_offset..cut];
        if to_summarize.is_empty() {
            return Err(CompactionError::NothingToCompact);
        }

        let previous_summary = current.as_ref().map(|c| c.summary.clone());
        let tokens_before = estimate_tokens(messages);

        let hook = self.hook();
        let event = BeforeCompact {
            reason,
            tokens_before,
            messages_to_summarize: to_summarize.to_vec(),
            previous_summary: previous_summary.clone(),
            instructions: instructions.map(str::to_owned),
        };
        let (summary, from_hook) = match self.decide(&hook, &event).await? {
            Some(summary) => (summary, true),
            None => {
                let summary = summarize(
                    stream,
                    model.clone(),
                    to_summarize,
                    previous_summary.as_deref(),
                    instructions,
                )
                .await;
                match summary {
                    Ok(summary) => (summary, false),
                    Err(error) => {
                        self.notify_failed(&hook, reason, &error).await;
                        return Err(error);
                    }
                }
            }
        };

        // 把可见下标映射回原始 transcript 下标。
        let first_kept = match &current {
            Some(previous) => previous.first_kept + cut.saturating_sub(summary_offset),
            None => cut,
        };
        let compaction = Compaction {
            summary,
            first_kept,
            tokens_before,
        };
        self.inner.lock().expect("compactor poisoned").current = Some(compaction.clone());
        if let Some(hook) = &hook {
            hook.after(&AfterCompact {
                reason,
                compaction: compaction.clone(),
                from_hook,
            })
            .await;
        }
        Ok(Some(compaction))
    }

    /// 问钩子要不要取消 / 提供摘要；没有钩子时返回 `None`（走内置摘要）。
    async fn decide(
        &self,
        hook: &Option<Arc<dyn CompactionHook>>,
        event: &BeforeCompact,
    ) -> Result<Option<String>, CompactionError> {
        let Some(hook) = hook else {
            return Ok(None);
        };
        match hook.before(event).await {
            None => Ok(None),
            Some(CompactionDecision::Provide { summary }) => Ok(Some(summary)),
            Some(CompactionDecision::Cancel) => {
                let error = CompactionError::Cancelled;
                self.notify_failed(&Some(Arc::clone(hook)), event.reason, &error)
                    .await;
                Err(error)
            }
        }
    }

    async fn notify_failed(
        &self,
        hook: &Option<Arc<dyn CompactionHook>>,
        reason: CompactionReason,
        error: &CompactionError,
    ) {
        if let Some(hook) = hook {
            hook.failed(&CompactionFailed {
                reason,
                error: error.to_string(),
            })
            .await;
        }
    }
}

/// 在可见消息里找切点：从后往前累计 token 到 `keep_recent_tokens`，再把切点吸附到
/// 最近的 user / assistant 消息上——**绝不切在 tool result 上**（结果要和调用待在一起）。
#[must_use]
pub fn find_cut(messages: &[AgentMessage], keep_recent_tokens: u64) -> Option<usize> {
    let base_offset = usize::from(is_system(messages.first()));
    if messages.len() <= base_offset + 1 {
        return None;
    }
    let mut accumulated = 0u64;
    let mut cut = messages.len();
    let mut index = messages.len();
    while index > base_offset {
        index -= 1;
        accumulated += estimate_tokens(std::slice::from_ref(&messages[index]));
        cut = index;
        if accumulated >= keep_recent_tokens {
            break;
        }
    }
    while cut > base_offset && !is_cut_point(&messages[cut]) {
        cut -= 1;
    }
    (cut > base_offset).then_some(cut)
}

/// 粗略 token 估算：按序列化文本的字符数除以 4。
#[must_use]
pub fn estimate_tokens(messages: &[AgentMessage]) -> u64 {
    let chars: usize = messages.iter().map(message_chars).sum();
    (chars as u64) / 4
}

/// 把消息序列化成给摘要模型看的纯文本（pi `serializeConversation`）。
///
/// 目的是**别让模型把摘要请求当成一段要继续的对话**；tool result 截到 2000 字符。
#[must_use]
pub fn serialize_conversation(messages: &[AgentMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        match message {
            AgentMessage::Message(Message::System(system)) => {
                let text = blocks_text(&system.content);
                if !text.is_empty() {
                    out.push_str("[System]: ");
                    out.push_str(&text);
                    out.push('\n');
                }
            }
            AgentMessage::Message(Message::User(user)) => {
                out.push_str("[User]: ");
                out.push_str(&user_text(user));
                out.push('\n');
            }
            AgentMessage::Message(Message::Assistant(assistant)) => {
                let text = blocks_text(&assistant.content);
                if !text.is_empty() {
                    out.push_str("[Assistant]: ");
                    out.push_str(&text);
                    out.push('\n');
                }
                let calls: Vec<String> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(call) => {
                            let args = serde_json::to_string(call.arguments.as_object())
                                .unwrap_or_default();
                            Some(format!("{}({args})", call.name))
                        }
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    out.push_str("[Assistant tool calls]: ");
                    out.push_str(&calls.join("; "));
                    out.push('\n');
                }
            }
            AgentMessage::Message(Message::ToolResult(result)) => {
                out.push_str("[Tool result]: ");
                out.push_str(&truncate(&blocks_text(&result.content), TOOL_RESULT_LIMIT));
                out.push('\n');
            }
            AgentMessage::Custom(value) => {
                out.push_str("[Custom]: ");
                out.push_str(&value.to_string());
                out.push('\n');
            }
        }
    }
    out
}

/// 摘要请求里 tool result 的截断长度（pi 同样用 2000）。
const TOOL_RESULT_LIMIT: usize = 2000;

const CREATE_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n\n## Constraints & Preferences\n\n## Progress\n### Done\n### In Progress\n### Blocked\n\n## Key Decisions\n\n## Next Steps\n\n## Critical Context\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_PROMPT: &str = "Update the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- MOVE finished items from \"In Progress\" to \"Done\"\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n\nUse the same EXACT format as the existing summary.";

/// 生成摘要：把对话序列化后连同提示词发给同一个 provider 流服务，收集文本。
async fn summarize(
    stream: BoxStreamService,
    model: Model,
    messages: &[AgentMessage],
    previous_summary: Option<&str>,
    instructions: Option<&str>,
) -> Result<String, CompactionError> {
    let mut prompt = String::new();
    prompt.push_str(&serialize_conversation(messages));
    prompt.push('\n');
    if let Some(previous) = previous_summary {
        prompt.push_str("The existing summary so far:\n\n<summary>\n");
        prompt.push_str(previous);
        prompt.push_str("\n</summary>\n\n");
        prompt.push_str(UPDATE_PROMPT);
    } else {
        prompt.push_str(CREATE_PROMPT);
    }
    if let Some(instructions) = instructions {
        let instructions = instructions.trim();
        if !instructions.is_empty() {
            prompt.push_str("\n\nAdditional instructions from the user: ");
            prompt.push_str(instructions);
        }
    }

    let request = StreamRequest {
        model,
        context: TranscriptContext::new(vec![Message::User(UserMessage {
            content: UserContent::Text(prompt),
            timestamp: now_ms(),
        })]),
        options: SimpleStreamOptions::default(),
    };
    let mut stream = stream
        .oneshot(request)
        .await
        .map_err(|error| CompactionError::Stream(error.to_string()))?;

    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(&delta),
            AssistantMessageEvent::Done { message, .. } => {
                let final_text = blocks_text(&message.content);
                if !final_text.is_empty() {
                    text = final_text;
                }
            }
            AssistantMessageEvent::Error { error, .. } => {
                return Err(CompactionError::Stream(
                    error
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "provider returned an error".to_owned()),
                ));
            }
            _ => {}
        }
    }
    let summary = text.trim();
    if summary.is_empty() {
        return Err(CompactionError::EmptySummary);
    }
    Ok(summary.to_owned())
}

fn summary_message(summary: &str) -> AgentMessage {
    AgentMessage::message(Message::User(UserMessage {
        content: UserContent::Text(format!(
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\n{summary}\n</summary>"
        )),
        timestamp: now_ms(),
    }))
}

fn is_system(message: Option<&AgentMessage>) -> bool {
    matches!(message, Some(AgentMessage::Message(Message::System(_))))
}

/// 合法切点：user / assistant（tool result 必须和它的调用待在一起，不能切）。
fn is_cut_point(message: &AgentMessage) -> bool {
    matches!(
        message,
        AgentMessage::Message(Message::User(_)) | AgentMessage::Message(Message::Assistant(_))
    )
}

fn message_chars(message: &AgentMessage) -> usize {
    match message {
        AgentMessage::Message(Message::System(system)) => blocks_chars(&system.content),
        AgentMessage::Message(Message::User(user)) => match &user.content {
            UserContent::Text(text) => text.chars().count(),
            UserContent::Blocks(blocks) => blocks_chars(blocks),
        },
        AgentMessage::Message(Message::Assistant(assistant)) => blocks_chars(&assistant.content),
        AgentMessage::Message(Message::ToolResult(result)) => blocks_chars(&result.content),
        AgentMessage::Custom(value) => value.to_string().chars().count(),
    }
}

fn blocks_chars(blocks: &[ContentBlock]) -> usize {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.chars().count(),
            ContentBlock::Thinking(thinking) => thinking.thinking.chars().count(),
            _ => 0,
        })
        .sum()
}

fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn user_text(user: &UserMessage) -> String {
    match &user.content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks_text(blocks),
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().take(limit).collect();
    let dropped = text.chars().count() - limit;
    format!("{kept}\n…[{dropped} characters truncated]")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use tower_agent_provider::{
        AssistantMessage, StopReason, SystemMessage, TextContent, ToolResultMessage, Usage,
    };

    use super::*;

    fn user(text: &str) -> AgentMessage {
        AgentMessage::message(Message::User(UserMessage {
            content: UserContent::Text(text.to_owned()),
            timestamp: 0,
        }))
    }

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage::message(Message::Assistant(Box::new(AssistantMessage {
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_owned(),
                text_signature: None,
            })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        })))
    }

    fn system(text: &str) -> AgentMessage {
        AgentMessage::message(Message::System(SystemMessage {
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_owned(),
                text_signature: None,
            })],
            sections: BTreeMap::new(),
            tools_added: Vec::new(),
            tools_removed: Vec::new(),
            timestamp: 0,
        }))
    }

    fn tool_result(text: &str) -> AgentMessage {
        AgentMessage::message(Message::ToolResult(ToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_owned(),
                text_signature: None,
            })],
            details: None,
            is_error: false,
            timestamp: 0,
        }))
    }

    #[test]
    fn cut_point_never_lands_on_a_tool_result() {
        let messages = vec![
            system("sys"),
            user("a"),
            assistant("b"),
            tool_result(&"x".repeat(4000)),
            assistant("c"),
        ];
        // 保留窗口极小 → 本该落在 tool result 上，吸附回最近的合法切点。
        let cut = find_cut(&messages, 1).expect("cut");
        assert!(is_cut_point(&messages[cut]), "cut 落在 {cut}");
        assert_ne!(cut, 3);
    }

    #[test]
    fn projection_keeps_system_and_kept_tail() {
        let messages = vec![system("sys"), user("a"), assistant("b"), user("c")];
        let compaction = Compaction {
            summary: "摘要".into(),
            first_kept: 3,
            tokens_before: 10,
        };
        let projected = compaction.project(&messages);
        assert_eq!(projected.len(), 3, "system + 摘要 + 1 条保留");
        assert!(is_system(projected.first()));
        assert_eq!(projected[2], messages[3]);
    }

    #[test]
    fn serialization_matches_pi_shape() {
        let messages = vec![user("你好"), assistant("在")];
        let text = serialize_conversation(&messages);
        assert!(text.contains("[User]: 你好"), "got: {text}");
        assert!(text.contains("[Assistant]: 在"), "got: {text}");
    }

    #[test]
    fn estimate_counts_chars_over_four() {
        let messages = vec![user(&"字".repeat(400))];
        assert_eq!(estimate_tokens(&messages), 100);
    }

    #[test]
    fn over_threshold_respects_reserve_and_disable() {
        let compactor = Compactor::new(CompactionSettings {
            enabled: true,
            reserve_tokens: 10,
            keep_recent_tokens: 10,
        });
        assert!(!compactor.over_threshold(&[user("hi")], 1000));
        let big = vec![user(&"a".repeat(8000))];
        assert!(compactor.over_threshold(&big, 1000));

        compactor.set_settings(CompactionSettings {
            enabled: false,
            reserve_tokens: 10,
            keep_recent_tokens: 10,
        });
        assert!(!compactor.over_threshold(&big, 1000));
    }

    #[test]
    fn clear_resets_current_and_pending_request() {
        let compactor = Compactor::new(CompactionSettings::default());
        compactor.request_compaction(Some("聚焦".into()));
        assert!(compactor.has_pending_request());
        compactor.clear();
        assert!(!compactor.has_pending_request());
        assert!(compactor.current().is_none());
    }

    #[test]
    fn custom_messages_are_serialized() {
        let messages = vec![AgentMessage::Custom(json!({"kind": "note"}))];
        assert!(serialize_conversation(&messages).contains("[Custom]: "));
    }
}
