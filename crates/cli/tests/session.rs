//! 会话入口的端到端测试：用离线 faux provider 驱动真实的 agent 循环 + 真实内置工具。

use cli::{
    faux::{FauxProvider, model},
    session::Session,
    tools,
};

fn session(cwd: &std::path::Path) -> Session {
    Session::with_model(cwd, FauxProvider::new().boxed(), model())
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
            agent::AgentMessage::Message(ai::Message::ToolResult(result)) => Some(result.clone()),
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

#[tokio::test]
async fn session_registers_all_builtin_tools() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session = session(dir.path());
    let state = session.state();
    let names: Vec<&str> = state.tools.iter().map(|tool| tool.name()).collect();
    assert_eq!(names, tools::tool_names());
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
        cli::session::run_print(subscription, &mut out)
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
