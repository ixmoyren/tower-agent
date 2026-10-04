//! 会话入口的端到端测试：用离线 faux provider 驱动真实的 agent 循环 + 真实内置工具。

use cli::{
    faux::{FauxProvider, model},
    session::Session,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use ai::{
    AssistantEventStream, AssistantMessage, AssistantMessageEvent, BoxStreamService, ContentBlock,
    DoneReason, PiError, StopReason, StreamRequest, TextContent, Usage,
};
use futures_util::{future::BoxFuture, stream};
use tokio::sync::Semaphore;
use tower::Service;

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
