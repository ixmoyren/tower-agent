//! 用 `TestBackend` 断言渲染结果——同一个 `Tui` 宿主与同一套渲染代码，
//! 只是后端换成了内存缓冲区。

use agent::{AgentEvent, AgentMessage, AgentToolResult};
use ai::{ContentBlock, Message, StopReason, TextContent, Usage, UserContent, UserMessage};
use ratatui::{Terminal, backend::TestBackend};
use tui::{EntryKind, Tui, TuiState};

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

fn user(text: &str) -> AgentMessage {
    AgentMessage::Message(Message::User(UserMessage {
        content: UserContent::Text(text.into()),
        timestamp: 0,
    }))
}

fn tui(width: u16, height: u16) -> Tui<TestBackend> {
    let backend = TestBackend::new(width, height);
    let terminal = Terminal::new(backend).expect("test terminal");
    Tui::new(terminal, TuiState::new("test-agent"))
}

#[test]
fn renders_header_transcript_and_footer() {
    let mut tui = tui(40, 14);
    tui.handle(&AgentEvent::AgentStart);
    tui.handle(&AgentEvent::MessageStart {
        message: user("hello"),
    });
    tui.handle(&AgentEvent::MessageEnd {
        message: user("hello"),
    });
    tui.handle(&AgentEvent::MessageStart {
        message: assistant(""),
    });
    tui.handle(&AgentEvent::MessageEnd {
        message: assistant("hi there"),
    });
    tui.draw().expect("draw succeeds");

    let rendered = render_to_string(tui.terminal());
    assert!(
        rendered.contains("test-agent"),
        "header has the title:\n{rendered}"
    );
    assert!(
        rendered.contains("● running"),
        "header shows the run state:\n{rendered}"
    );
    assert!(
        rendered.contains("› hello"),
        "user message rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("hi there"),
        "assistant message rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("Enter"),
        "footer hints rendered:\n{rendered}"
    );
}

#[test]
fn streaming_text_is_rendered_with_a_cursor_and_cleared_on_end() {
    let mut tui = tui(30, 12);
    tui.handle(&AgentEvent::AgentStart);
    tui.handle(&AgentEvent::MessageStart {
        message: assistant(""),
    });
    tui.handle(&AgentEvent::MessageUpdate {
        message: assistant("partial"),
        assistant_message_event: ai::AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "partial".into(),
            partial: match assistant("partial") {
                AgentMessage::Message(Message::Assistant(a)) => *a,
                _ => unreachable!(),
            },
        },
    });
    tui.draw().expect("draw succeeds");
    let rendered = render_to_string(tui.terminal());
    assert!(
        rendered.contains("partial▌"),
        "streaming text has a cursor:\n{rendered}"
    );
}

#[test]
fn tool_entries_render_and_update_in_place() {
    let mut tui = tui(60, 14);
    tui.handle(&AgentEvent::AgentStart);
    tui.handle(&AgentEvent::ToolExecutionStart {
        tool_call_id: "call-1".into(),
        tool_name: "read".into(),
        args: serde_json::json!({ "path": "a.txt" }),
    });
    tui.draw().expect("draw succeeds");
    let rendered = render_to_string(tui.terminal());
    assert!(
        rendered.contains("… read"),
        "pending tool rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("1 tool(s) running"),
        "header counts pending tools:\n{rendered}"
    );

    tui.handle(&AgentEvent::ToolExecutionEnd {
        tool_call_id: "call-1".into(),
        tool_name: "read".into(),
        result: AgentToolResult::text("done"),
        is_error: false,
    });
    tui.handle(&AgentEvent::AgentEnd {
        messages: Vec::new(),
    });
    tui.draw().expect("draw succeeds");
    let rendered = render_to_string(tui.terminal());
    assert!(
        rendered.contains("✔ read done"),
        "finished tool rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("○ idle"),
        "header returns to idle:\n{rendered}"
    );
    assert!(
        !rendered.contains("tool(s) running"),
        "no pending tool left:\n{rendered}"
    );
}

#[test]
fn error_is_shown_in_the_footer() {
    let mut tui = tui(120, 12);
    let mut message = match assistant("") {
        AgentMessage::Message(Message::Assistant(a)) => *a,
        _ => unreachable!(),
    };
    message.stop_reason = StopReason::Error;
    message.error_message = Some("boom".into());
    tui.handle(&AgentEvent::TurnEnd {
        message: AgentMessage::Message(Message::Assistant(Box::new(message))),
        tool_results: Vec::new(),
    });
    tui.draw().expect("draw succeeds");
    let rendered = render_to_string(tui.terminal());
    assert!(
        rendered.contains("error: boom"),
        "error rendered in the footer:\n{rendered}"
    );
}

#[test]
fn long_transcripts_follow_the_bottom_and_can_scroll_up() {
    let mut tui = tui(20, 12);
    for index in 0..12 {
        let text = format!("line-{index:02}");
        tui.handle(&AgentEvent::MessageStart {
            message: user(&text),
        });
        tui.handle(&AgentEvent::MessageEnd {
            message: user(&text),
        });
    }
    tui.draw().expect("draw succeeds");
    let rendered = render_to_string(tui.terminal());
    assert!(
        rendered.contains("line-11"),
        "follows the newest line:\n{rendered}"
    );
    assert!(
        !rendered.contains("line-00"),
        "does not show the top:\n{rendered}"
    );

    tui.scroll_up(100);
    tui.draw().expect("draw succeeds");
    let rendered = render_to_string(tui.terminal());
    assert!(
        rendered.contains("line-00"),
        "scrolled to the top:\n{rendered}"
    );

    tui.scroll_to_bottom();
    tui.draw().expect("draw succeeds");
    let rendered = render_to_string(tui.terminal());
    assert!(
        rendered.contains("line-11"),
        "back to the bottom:\n{rendered}"
    );
}

/// 输入按键经 `Tui` 宿主路由到编辑器，Enter 提交后编辑器清空。
#[test]
fn editor_input_routes_through_the_tui_host() {
    use ratatui::termina::event::{Event, KeyCode, KeyEvent, Modifiers};
    use tui::EditorAction;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, Modifiers::NONE))
    }

    let mut tui = tui(40, 14);
    assert_eq!(
        tui.handle_input(&key(KeyCode::Char('i'))),
        EditorAction::None
    );
    for ch in "帮我看看".chars() {
        assert_eq!(
            tui.handle_input(&key(KeyCode::Char(ch))),
            EditorAction::None
        );
    }
    tui.handle_input(&key(KeyCode::Escape));
    let action = tui.handle_input(&key(KeyCode::Enter));
    assert_eq!(action, EditorAction::Submit("帮我看看".into()));

    // 提交后清空并回到普通模式。
    assert_eq!(tui.editor().mode(), tui::Mode::Normal);
    assert_eq!(tui.editor().text(), "");
}

#[test]
fn entry_kinds_are_tracked_in_the_view_model() {
    let state = TuiState::new("t");
    assert_eq!(state.entries().len(), 0);

    let mut state = TuiState::new("t");
    state.push_message(&user("hi"));
    state.push_message(&assistant("hello"));
    assert_eq!(state.entries()[0].kind, EntryKind::User);
    assert_eq!(state.entries()[1].kind, EntryKind::Assistant);
}

/// 读取 `TestBackend` 缓冲区内容。
fn render_to_string(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        output.push_str(line.trim_end());
        output.push('\n');
    }
    output
}
