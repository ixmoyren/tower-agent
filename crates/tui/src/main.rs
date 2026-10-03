//! `tui` 二进制入口：跑一段脚本化的演示会话，用来观察渲染效果。
//!
//! agent 的真正接线在后续的 `cli` 阶段（`backend::run_agent_tui` 就是给它用的入口）；
//! 这里用一个固定的 `AgentEvent` 序列驱动同一套渲染路径，所以 `cargo run -p tui`
//! 不需要 provider 也能看到流式文本、工具执行与状态栏。输入区提交的内容会被回显成
//! 一轮"问答"，方便观察编辑器的行为。
//!
//! 编辑器默认 vim 普通模式（`i` 插入、`Esc` 回普通、`Enter` 提交、`q` 退出）；
//! `PageUp` / `PageDown` 滚动会话记录，`Ctrl+C` 随时退出。

use std::time::Duration;

use agent::{AgentEvent, AgentMessage, AgentToolResult};
use ai::{
    AssistantMessage, ContentBlock, Message, StopReason, TextContent, ToolCall, Usage, UserContent,
    UserMessage,
};
use tokio::sync::mpsc;
use tui::backend::run_tui;

/// 每条演示事件之间的间隔，模拟真实的流式节奏。
const DEMO_STEP: Duration = Duration::from_millis(150);

fn user(text: &str) -> AgentMessage {
    AgentMessage::Message(Message::User(UserMessage {
        content: UserContent::Text(text.into()),
        timestamp: 0,
    }))
}

fn assistant(text: &str, stop_reason: StopReason, tool_calls: Vec<ToolCall>) -> AgentMessage {
    let mut content: Vec<ContentBlock> = Vec::new();
    if !text.is_empty() {
        content.push(ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        }));
    }
    content.extend(tool_calls.into_iter().map(ContentBlock::ToolCall));
    AgentMessage::Message(Message::Assistant(Box::new(AssistantMessage {
        content,
        api: "demo".into(),
        provider: "demo".into(),
        model: "demo-model".into(),
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
    })))
}

/// 演示脚本：一轮完整的 "流式回答 → 工具执行 → 收尾回答"。
fn demo_script() -> Vec<AgentEvent> {
    let mut events = vec![
        AgentEvent::AgentStart,
        AgentEvent::TurnStart,
        AgentEvent::MessageStart {
            message: user("帮我看一下这个仓库里有哪些内置工具？"),
        },
        AgentEvent::MessageEnd {
            message: user("帮我看一下这个仓库里有哪些内置工具？"),
        },
        AgentEvent::MessageStart {
            message: assistant("", StopReason::Pending, Vec::new()),
        },
    ];

    // 逐段吐出流式文本，模拟 provider 的 delta。
    for partial in ["我先", "我先看一眼", "我先看一眼工具目录。"] {
        events.push(AgentEvent::MessageUpdate {
            message: assistant(partial, StopReason::Pending, Vec::new()),
            assistant_message_event: ai::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: partial.into(),
                partial: match assistant(partial, StopReason::Pending, Vec::new()) {
                    AgentMessage::Message(Message::Assistant(message)) => *message,
                    _ => unreachable!("assistant builder returns an assistant message"),
                },
            },
        });
    }

    let call = ToolCall {
        id: "call-1".into(),
        name: "ls".into(),
        arguments: match serde_json::json!({ "path": "." }) {
            serde_json::Value::Object(map) => map,
            _ => unreachable!("json object literal"),
        },
        thought_signature: None,
        namespace: None,
    };
    events.extend([
        AgentEvent::MessageEnd {
            message: assistant(
                "我先看一眼工具目录。",
                StopReason::ToolUse,
                vec![call.clone()],
            ),
        },
        AgentEvent::ToolExecutionStart {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: serde_json::Value::Object(call.arguments.clone()),
        },
        AgentEvent::ToolExecutionEnd {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            result: AgentToolResult::text("read/\nbash/\nedit/\nwrite/\ngrep/\nfind/\nls/"),
            is_error: false,
        },
        AgentEvent::TurnEnd {
            message: assistant("我先看一眼工具目录。", StopReason::ToolUse, vec![call]),
            tool_results: Vec::new(),
        },
        AgentEvent::TurnStart,
    ]);

    let answer = "一共八个：read、bash、powershell、edit、write、grep、find、ls。";
    events.push(AgentEvent::MessageStart {
        message: assistant("", StopReason::Pending, Vec::new()),
    });
    events.push(AgentEvent::MessageEnd {
        message: assistant(answer, StopReason::Stop, Vec::new()),
    });
    events.extend([
        AgentEvent::TurnEnd {
            message: assistant(answer, StopReason::Stop, Vec::new()),
            tool_results: Vec::new(),
        },
        AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
    ]);
    events
}

// 单线程运行时足够：输入读取跑在独立 OS 线程上，事件循环本身不需要多线程调度。
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let (submissions_tx, mut submissions_rx) = mpsc::unbounded_channel::<String>();

    // 开场：脚本化的演示会话。
    let demo_tx = events_tx.clone();
    tokio::spawn(async move {
        for event in demo_script() {
            if demo_tx.send(event).is_err() {
                return;
            }
            tokio::time::sleep(DEMO_STEP).await;
        }
    });

    // 交互：把输入区提交的文本回显成一轮问答（没有真实 provider）。
    tokio::spawn(async move {
        while let Some(text) = submissions_rx.recv().await {
            let question = user(&text);
            let answer = assistant(
                &format!("[demo] 收到：{text}"),
                StopReason::Stop,
                Vec::new(),
            );
            let events = [
                AgentEvent::TurnStart,
                AgentEvent::MessageStart {
                    message: question.clone(),
                },
                AgentEvent::MessageEnd { message: question },
                AgentEvent::MessageStart {
                    message: answer.clone(),
                },
                AgentEvent::MessageEnd {
                    message: answer.clone(),
                },
                AgentEvent::TurnEnd {
                    message: answer,
                    tool_results: Vec::new(),
                },
            ];
            for event in events {
                if events_tx.send(event).is_err() {
                    return;
                }
                tokio::time::sleep(DEMO_STEP).await;
            }
        }
    });

    run_tui(events_rx, submissions_tx, "tower-pi-agent · tui demo").await
}

#[cfg(test)]
mod tests {
    use tui::{EntryKind, TuiState};

    use super::*;

    /// 把演示脚本折进视图模型，验证二进制展示的内容与状态流转。
    #[test]
    fn demo_script_drives_a_full_session() {
        let mut state = TuiState::new("demo");
        for event in demo_script() {
            state.apply(&event);
        }

        assert!(!state.is_running(), "脚本以 AgentEnd 收尾");
        assert_eq!(state.streaming(), None);
        assert!(state.pending_tool_calls().is_empty());
        assert_eq!(state.error(), None);

        let kinds: Vec<EntryKind> = state.entries().iter().map(|entry| entry.kind).collect();
        assert_eq!(
            kinds,
            vec![
                EntryKind::User,
                EntryKind::Assistant,
                EntryKind::Tool,
                EntryKind::Assistant,
            ]
        );

        let texts: Vec<&str> = state
            .entries()
            .iter()
            .map(|entry| entry.text.as_str())
            .collect();
        assert!(texts[0].contains("内置工具"));
        assert!(
            texts[1].starts_with("我先看一眼工具目录。"),
            "assistant text plus the rendered tool call: {}",
            texts[1]
        );
        assert!(
            texts[1].contains("[tool ls("),
            "tool call rendered inline: {}",
            texts[1]
        );
        assert!(
            texts[2].starts_with("✔ ls"),
            "工具条目收敛到结果：{}",
            texts[2]
        );
        assert!(texts[3].contains("一共八个"));
    }

    /// 脚本里每个工具调用 id 都成对出现（start 与 end 匹配）。
    #[test]
    fn demo_script_tool_calls_are_balanced() {
        let events = demo_script();
        let starts = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolExecutionStart { .. }))
            .count();
        let ends = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolExecutionEnd { .. }))
            .count();
        assert_eq!(starts, ends);
        assert!(matches!(events.first(), Some(AgentEvent::AgentStart)));
        assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
    }
}
