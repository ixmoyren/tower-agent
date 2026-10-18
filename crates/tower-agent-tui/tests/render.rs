//! 用 `TestBackend` 断言渲染结果——同一个 `Tui` 宿主与同一套渲染代码，
//! 只是后端换成了内存缓冲区。

use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
};
use tower_agent_provider::{
    ContentBlock, Message, StopReason, TextContent, Usage, UserContent, UserMessage,
};
use tower_agent_runtime::{AgentEvent, AgentMessage, AgentToolResult};
use tower_agent_tui::{EntryKind, ToolResultView, Tui, TuiState, UiPlugin};

fn assistant(text: &str) -> AgentMessage {
    AgentMessage::Message(Message::Assistant(Box::new(
        tower_agent_provider::AssistantMessage {
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
        },
    )))
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
        assistant_message_event: tower_agent_provider::AssistantMessageEvent::TextDelta {
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
    use tower_agent_tui::EditorAction;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, Modifiers::NONE))
    }

    let mut tui = tui(40, 14);
    assert_eq!(
        tui.handle_input(&key(KeyCode::Char('i'))),
        EditorAction::None
    );
    for ch in "look at this".chars() {
        assert_eq!(
            tui.handle_input(&key(KeyCode::Char(ch))),
            EditorAction::None
        );
    }
    tui.handle_input(&key(KeyCode::Escape));
    let action = tui.handle_input(&key(KeyCode::Enter));
    assert_eq!(action, EditorAction::Submit("look at this".into()));

    // 提交后清空并回到普通模式。
    assert_eq!(tui.editor().mode(), tower_agent_tui::Mode::Normal);
    assert_eq!(tui.editor().text(), "");
}

#[test]
fn insert_mode_arrows_move_through_the_tui_host() {
    use ratatui::termina::event::{Event, KeyCode, KeyEvent, Modifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, Modifiers::NONE))
    }

    let mut tui = tui(40, 14);
    tui.handle_input(&key(KeyCode::Char('i')));
    for ch in "abc".chars() {
        tui.handle_input(&key(KeyCode::Char(ch)));
    }

    tui.handle_input(&key(KeyCode::Left));
    tui.handle_input(&key(KeyCode::Left));
    assert_eq!(tui.editor().textarea().cursor(), (0, 1));

    tui.handle_input(&key(KeyCode::Right));
    assert_eq!(tui.editor().textarea().cursor(), (0, 2));

    tui.handle_input(&key(KeyCode::Char('X')));
    assert_eq!(tui.editor().text(), "abXc");
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

/// 一个同时贡献状态栏片段、工具结果渲染与浮层的 UI 插件。
struct DemoUiPlugin;

impl UiPlugin for DemoUiPlugin {
    fn name(&self) -> &str {
        "demo"
    }

    fn status_segment(&self) -> Option<Line<'static>> {
        Some(Line::from(Span::styled(
            "BETA",
            Style::default().fg(Color::Magenta),
        )))
    }

    fn render_tool_result(
        &self,
        tool: &str,
        result: &ToolResultView,
        _width: usize,
    ) -> Option<Vec<Line<'static>>> {
        (tool == "read").then(|| vec![Line::from(format!("CUSTOM {}", result.summary))])
    }

    fn render_overlay(&self, area: Rect, buf: &mut Buffer) -> bool {
        buf.set_string(area.x, area.y, "OVL", Style::default().fg(Color::Yellow));
        true
    }
}

#[test]
fn ui_plugins_contribute_status_tool_and_overlay_rendering() {
    let terminal = Terminal::new(TestBackend::new(60, 14)).expect("test terminal");
    let mut tui = Tui::with_plugins(
        terminal,
        TuiState::new("test-agent"),
        vec![Box::new(DemoUiPlugin)],
    );
    tui.handle(&AgentEvent::ToolExecutionStart {
        tool_call_id: "t1".into(),
        tool_name: "read".into(),
        args: serde_json::json!({ "path": "a.txt" }),
    });
    tui.handle(&AgentEvent::ToolExecutionEnd {
        tool_call_id: "t1".into(),
        tool_name: "read".into(),
        result: AgentToolResult::text("file body"),
        is_error: false,
    });
    tui.draw().expect("draw");

    let rendered = render_to_string(tui.terminal());
    assert!(rendered.contains("BETA"), "{rendered}");
    assert!(rendered.contains("CUSTOM file body"), "{rendered}");
    assert!(rendered.contains("OVL"), "{rendered}");
}

#[test]
fn tui_registers_itself_as_a_frontend_plugin() {
    let terminal = Terminal::new(TestBackend::new(60, 12)).expect("test terminal");
    let mut tui = Tui::with_plugins(
        terminal,
        TuiState::new("test-agent"),
        tower_agent_tui::frontend_plugins(),
    );
    tui.draw().expect("draw");

    let rendered = render_to_string(tui.terminal());
    assert!(rendered.contains("[tui]"), "{rendered}");
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
