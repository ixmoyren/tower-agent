//! TUI 的视图模型：把 [`agent::AgentEvent`] 折成可渲染的 transcript。
//!
//! 这是渲染层唯一的状态来源：`render` 只读 [`TuiState`]，事件归约只发生在
//! [`TuiState::apply`] / [`TuiState::push_message`]。因此同一份状态既能在真实终端渲染，
//! 也能在 `TestBackend` 上断言。

use std::collections::{BTreeSet, HashMap};

use agent::{AgentEvent, AgentMessage};
use ai::{ContentBlock, JsonValue, Message, ToolResultMessage, UserContent};

/// transcript 条目的类别，决定渲染时的前缀与配色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// 用户消息。
    User,
    /// assistant 消息（含流式结束后的最终文本）。
    Assistant,
    /// system 消息。
    System,
    /// 一次工具执行（开始/完成共用一条，完成时就地更新）。
    Tool,
    /// 本地提示：斜杠命令的输出、错误说明等（不来自 agent）。
    Notice,
}

/// 一条 transcript 条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 条目类别。
    pub kind: EntryKind,
    /// 单行或多行文本（已按 `\n` 拆好）。
    pub text: String,
}

/// TUI 的完整视图状态。
#[derive(Debug, Clone)]
pub struct TuiState {
    title: String,
    entries: Vec<Entry>,
    streaming: Option<String>,
    pending_tools: BTreeSet<String>,
    tool_entries: HashMap<String, usize>,
    error: Option<String>,
    running: bool,
    /// 距离底部滚动了多少行；0 表示跟随最新内容。
    scroll: u16,
}

impl Default for TuiState {
    fn default() -> Self {
        Self::new("tower-pi-agent")
    }
}

impl TuiState {
    /// 用标题构造空状态。
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            entries: Vec::new(),
            streaming: None,
            pending_tools: BTreeSet::new(),
            tool_entries: HashMap::new(),
            error: None,
            running: false,
            scroll: 0,
        }
    }

    /// 标题。
    pub fn title(&self) -> &str {
        &self.title
    }

    /// 已完成的 transcript 条目。
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// 正在流式生成的 assistant 文本。
    pub fn streaming(&self) -> Option<&str> {
        self.streaming.as_deref()
    }

    /// 正在执行的工具调用 id。
    pub fn pending_tool_calls(&self) -> &BTreeSet<String> {
        &self.pending_tools
    }

    /// 最近一次失败 turn 的错误信息。
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 是否处于一次 run 中。
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// 距离底部的滚动行数（0 = 跟随最新内容）。
    pub fn scroll(&self) -> u16 {
        self.scroll
    }

    /// 向上滚动 `lines` 行（到底部后停住）。
    pub fn scroll_up(&mut self, lines: u16) {
        self.scroll = self.scroll.saturating_add(lines);
    }

    /// 向下滚动 `lines` 行，回到 0 即恢复跟随。
    pub fn scroll_down(&mut self, lines: u16) {
        self.scroll = self.scroll.saturating_sub(lines);
    }

    /// 回到最新内容。
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
    }

    /// 状态行文本（头部栏使用）。
    pub fn status_line(&self) -> String {
        let marker = if self.running { "●" } else { "○" };
        let state = if self.running { "running" } else { "idle" };
        let mut line = format!("{} {state}", marker);
        if !self.pending_tools.is_empty() {
            line.push_str(&format!(" · {} tool(s) running", self.pending_tools.len()));
        }
        if self.scroll > 0 {
            line.push_str(&format!(" · scrolled {} line(s)", self.scroll));
        }
        line
    }

    /// 直接把一条消息加入 transcript（用于从已有会话初始化）。
    pub fn push_message(&mut self, message: &AgentMessage) {
        if matches!(message.role(), Some("toolResult")) {
            // 工具结果由对应的工具条目承担，避免重复。
            return;
        }
        let kind = match message {
            AgentMessage::Message(Message::User(_)) => EntryKind::User,
            AgentMessage::Message(Message::Assistant(_)) => EntryKind::Assistant,
            AgentMessage::Message(Message::System(_)) => EntryKind::System,
            _ => EntryKind::System,
        };
        self.entries.push(Entry {
            kind,
            text: render_message(message),
        });
    }

    /// 追加一条本地提示（斜杠命令的结果）。
    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.entries.push(Entry {
            kind: EntryKind::Notice,
            text: text.into(),
        });
    }

    /// 清空 transcript（`/new`：agent 侧已经 [`agent::Agent::reset`]）。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.streaming = None;
        self.pending_tools.clear();
        self.tool_entries.clear();
        self.error = None;
        self.running = false;
        self.scroll = 0;
    }

    /// 归约一个 agent 事件。
    pub fn apply(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::AgentStart => {
                self.running = true;
                self.error = None;
            }
            AgentEvent::AgentEnd { .. } => {
                self.running = false;
                self.streaming = None;
            }
            AgentEvent::TurnStart => {}
            AgentEvent::TurnEnd { message, .. } => {
                if let AgentMessage::Message(Message::Assistant(assistant)) = message
                    && let Some(error) = &assistant.error_message
                {
                    self.error = Some(error.clone());
                }
            }
            AgentEvent::MessageStart { message } => {
                if matches!(message, AgentMessage::Message(Message::Assistant(_))) {
                    self.streaming = Some(render_message(message));
                }
            }
            AgentEvent::MessageUpdate { message, .. } => {
                if matches!(message, AgentMessage::Message(Message::Assistant(_))) {
                    self.streaming = Some(render_message(message));
                }
            }
            AgentEvent::MessageEnd { message } => {
                if matches!(message, AgentMessage::Message(Message::Assistant(_))) {
                    self.entries.push(Entry {
                        kind: EntryKind::Assistant,
                        text: render_message(message),
                    });
                    self.streaming = None;
                } else {
                    self.push_message(message);
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                self.pending_tools.insert(tool_call_id.clone());
                self.tool_entries
                    .insert(tool_call_id.clone(), self.entries.len());
                self.entries.push(Entry {
                    kind: EntryKind::Tool,
                    text: format!("… {tool_name} {}", summarize_json(args)),
                });
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => {
                if let Some(index) = self.tool_entries.get(tool_call_id).copied() {
                    let name = self
                        .entries
                        .get(index)
                        .map(|entry| tool_name_of(&entry.text))
                        .unwrap_or_default();
                    self.entries[index].text =
                        format!("… {name} {}", preview(&render_tool_result(partial_result)));
                }
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                self.pending_tools.remove(tool_call_id);
                let marker = if *is_error { "✖" } else { "✔" };
                let text = format!(
                    "{marker} {tool_name} {}",
                    preview(&render_tool_result(result))
                );
                match self.tool_entries.get(tool_call_id).copied() {
                    Some(index) if index < self.entries.len() => self.entries[index].text = text,
                    _ => self.entries.push(Entry {
                        kind: EntryKind::Tool,
                        text,
                    }),
                }
            }
        }
    }
}

/// 把一条 agent 消息渲染成纯文本。
pub fn render_message(message: &AgentMessage) -> String {
    match message {
        AgentMessage::Message(Message::User(user)) => match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(blocks) => render_blocks(blocks),
        },
        AgentMessage::Message(Message::Assistant(assistant)) => render_blocks(&assistant.content),
        AgentMessage::Message(Message::System(system)) => render_blocks(&system.content),
        AgentMessage::Message(Message::ToolResult(result)) => render_tool_result_message(result),
        AgentMessage::Custom(value) => value.to_string(),
    }
}

fn render_tool_result(result: &agent::AgentToolResult) -> String {
    render_blocks(&result.content)
}

fn render_tool_result_message(result: &ToolResultMessage) -> String {
    render_blocks(&result.content)
}

/// 内容块 → 文本：文本块逐行保留，工具调用渲染成 `[tool name(args)]`。
fn render_blocks(blocks: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::Thinking(_) => {}
            ContentBlock::Image(image) => parts.push(format!("[image {}]", image.mime_type)),
            ContentBlock::ToolCall(call) => parts.push(format!(
                "[tool {}({})]",
                call.name,
                summarize_json(&JsonValue::Object(call.arguments.as_object().clone()))
            )),
        }
    }
    parts.join("\n")
}

/// 单行摘要：JSON 压成一行，超过 `limit` 字符就截断。
fn summarize_json(value: &JsonValue) -> String {
    let rendered = value.to_string();
    preview(&rendered)
}

fn preview(text: &str) -> String {
    const LIMIT: usize = 120;
    let single_line = text.replace('\n', " ⏎ ");
    if single_line.chars().count() <= LIMIT {
        return single_line;
    }
    let head: String = single_line.chars().take(LIMIT).collect();
    format!("{head}…")
}

/// 从 `"… name args"` / `"✔ name ..."` 这类文本里取回工具名。
fn tool_name_of(text: &str) -> String {
    text.split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai::{StopReason, TextContent, ToolCall, Usage};

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage::Message(Message::Assistant(Box::new(ai::AssistantMessage {
            content: vec![ContentBlock::Text(TextContent {
                text: text.into(),
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

    #[test]
    fn streaming_updates_render_the_partial_message() {
        let mut state = TuiState::new("t");
        state.apply(&AgentEvent::AgentStart);
        assert!(state.is_running());

        state.apply(&AgentEvent::MessageStart {
            message: assistant(""),
        });
        assert_eq!(state.streaming(), Some(""));

        state.apply(&AgentEvent::MessageUpdate {
            message: assistant("hel"),
            assistant_message_event: ai::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "hel".into(),
                partial: match assistant("hel") {
                    AgentMessage::Message(Message::Assistant(a)) => *a,
                    _ => unreachable!(),
                },
            },
        });
        assert_eq!(state.streaming(), Some("hel"));

        state.apply(&AgentEvent::MessageEnd {
            message: assistant("hello"),
        });
        assert_eq!(state.streaming(), None);
        assert_eq!(state.entries().len(), 1);
        assert_eq!(state.entries()[0].kind, EntryKind::Assistant);
        assert_eq!(state.entries()[0].text, "hello");
    }

    #[test]
    fn tool_entries_are_updated_in_place() {
        let mut state = TuiState::new("t");
        state.apply(&AgentEvent::ToolExecutionStart {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            args: serde_json::json!({ "path": "a.txt" }),
        });
        assert_eq!(state.pending_tool_calls().len(), 1);
        assert_eq!(state.entries().len(), 1);

        state.apply(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            result: agent::AgentToolResult::text("file body"),
            is_error: false,
        });
        assert!(state.pending_tool_calls().is_empty());
        assert_eq!(state.entries().len(), 1, "same entry updated in place");
        assert_eq!(state.entries()[0].text, "✔ read file body");
    }

    #[test]
    fn turn_end_records_the_error_message() {
        let mut state = TuiState::new("t");
        let mut message = match assistant("") {
            AgentMessage::Message(Message::Assistant(a)) => *a,
            _ => unreachable!(),
        };
        message.stop_reason = StopReason::Error;
        message.error_message = Some("boom".into());
        state.apply(&AgentEvent::TurnEnd {
            message: AgentMessage::Message(Message::Assistant(Box::new(message))),
            tool_results: Vec::new(),
        });
        assert_eq!(state.error(), Some("boom"));
    }

    #[test]
    fn tool_calls_render_as_bracketed_lines() {
        let message = AgentMessage::Message(Message::Assistant(Box::new(ai::AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "c".into(),
                name: "ls".into(),
                arguments: Default::default(),
                thought_signature: None,
                namespace: None,
            })],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        })));
        assert_eq!(render_message(&message), "[tool ls({})]");
    }

    #[test]
    fn notices_are_appended_and_clear_resets_everything() {
        let mut state = TuiState::new("t");
        state.push_message(&AgentMessage::Message(Message::User(ai::UserMessage {
            content: ai::UserContent::Text("hi".into()),
            timestamp: 0,
        })));
        state.push_notice("Started a new session.");
        assert_eq!(state.entries().len(), 2);
        assert_eq!(state.entries()[1].kind, EntryKind::Notice);

        state.apply(&AgentEvent::AgentStart);
        state.clear();
        assert!(state.entries().is_empty());
        assert!(state.streaming().is_none());
        assert!(!state.is_running());
        assert_eq!(state.scroll(), 0);
    }
}
