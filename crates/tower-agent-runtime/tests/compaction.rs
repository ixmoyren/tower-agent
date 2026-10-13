//! 上下文压缩的端到端行为：超阈值时自动压缩，且下一次模型请求看到的是压缩后的投影。

mod support;

use std::sync::{Arc, Mutex};

use tower_agent_provider::{Message, Model, StopReason, UserContent};
use tower_agent_runtime::{
    AfterCompact, Agent, AgentInitialState, AgentMessage, AgentOptions, BeforeCompact, Compaction,
    CompactionDecision, CompactionFailed, CompactionHook, CompactionReason, CompactionSettings,
    Compactor, agent_loop::BoxFut,
};

use support::{ScriptedStreamService, boxed_stream, error_script, test_model, text_script, user};

/// 记录调用并可选地给出决定的钩子。
#[derive(Default)]
struct RecordingHook {
    before: Mutex<Vec<CompactionReason>>,
    after: Mutex<Vec<(CompactionReason, bool)>>,
    failed: Mutex<Vec<String>>,
    decision: Option<CompactionDecision>,
}

impl CompactionHook for RecordingHook {
    fn before(&self, event: &BeforeCompact) -> BoxFut<Option<CompactionDecision>> {
        self.before.lock().expect("hook mutex").push(event.reason);
        let decision = self.decision.clone();
        Box::pin(async move { decision })
    }

    fn after(&self, event: &AfterCompact) -> BoxFut<()> {
        self.after
            .lock()
            .expect("hook mutex")
            .push((event.reason, event.from_hook));
        Box::pin(async {})
    }

    fn failed(&self, event: &CompactionFailed) -> BoxFut<()> {
        self.failed
            .lock()
            .expect("hook mutex")
            .push(event.error.clone());
        Box::pin(async {})
    }
}

fn assistant_message(model: &Model, text: &str) -> AgentMessage {
    AgentMessage::message(Message::Assistant(Box::new(support::assistant(
        model,
        vec![support::text(text)],
        StopReason::Stop,
    ))))
}

fn contains_summary(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::User(user) => match &user.content {
            UserContent::Text(text) => text.contains("<summary>"),
            UserContent::Blocks(_) => false,
        },
        _ => false,
    })
}

#[tokio::test]
async fn loop_auto_compacts_over_threshold_and_projects_the_summary() {
    // 小窗口模型：阈值 = 100 - 1 = 99 tokens。
    let model = Model {
        context_window: 100,
        ..test_model()
    };
    let summary = "## Goal\n继续实现压缩";
    let service = ScriptedStreamService::new(vec![
        text_script(&model, summary),
        text_script(&model, "done"),
    ]);
    let recorder = service.clone();

    let compactor = Arc::new(Compactor::new(CompactionSettings {
        enabled: true,
        reserve_tokens: 1,
        keep_recent_tokens: 20,
    }));

    let long = "历史".repeat(200); // 400 字 ≈ 100 tokens / 条
    let mut options = AgentOptions::new(boxed_stream(service));
    options.initial = AgentInitialState {
        model: Some(model.clone()),
        messages: vec![
            user(&long),
            assistant_message(&model, "ok"),
            user(&long),
            assistant_message(&model, "ok"),
        ],
        ..AgentInitialState::default()
    };
    options.compactor = Some(Arc::clone(&compactor));
    let agent = Agent::new(options);

    agent.prompt_text("继续").await.expect("run succeeds");

    // 触发了自动压缩。
    let compaction = compactor.current().expect("compaction ran");
    assert!(
        compaction.summary.contains("继续实现压缩"),
        "got: {}",
        compaction.summary
    );
    assert!(compaction.first_kept >= 1);

    // 第 1 次请求是摘要，第 2 次是真正的 turn，且看到的是「摘要 + 保留的消息」。
    let requests = recorder.requests();
    assert_eq!(requests.len(), 2, "先摘要，再 turn");
    let summary_request = &requests[0].context.messages()[0];
    match summary_request {
        Message::User(user) => match &user.content {
            UserContent::Text(text) => {
                assert!(text.contains("[User]:"), "序列化对话：{text}");
                assert!(text.contains("## Goal"), "带摘要提示词：{text}");
            }
            other => panic!("expected text, got {other:?}"),
        },
        other => panic!("expected a user message, got {other:?}"),
    }
    assert!(
        contains_summary(requests[1].context.messages()),
        "turn 请求应包含压缩摘要：{:?}",
        requests[1].context.messages()
    );
}

#[tokio::test]
async fn disabled_compaction_leaves_the_context_alone() {
    let model = Model {
        context_window: 100,
        ..test_model()
    };
    let service = ScriptedStreamService::new(vec![text_script(&model, "done")]);
    let recorder = service.clone();

    let compactor = Arc::new(Compactor::new(CompactionSettings {
        enabled: false,
        reserve_tokens: 1,
        keep_recent_tokens: 20,
    }));

    let long = "历史".repeat(200);
    let mut options = AgentOptions::new(boxed_stream(service));
    options.initial = AgentInitialState {
        model: Some(model.clone()),
        messages: vec![user(&long), assistant_message(&model, "ok")],
        ..AgentInitialState::default()
    };
    options.compactor = Some(Arc::clone(&compactor));
    let agent = Agent::new(options);

    agent.prompt_text("继续").await.expect("run succeeds");

    assert!(compactor.current().is_none(), "关闭时不压缩");
    assert_eq!(recorder.requests().len(), 1, "不额外发摘要请求");
    assert!(!contains_summary(recorder.requests()[0].context.messages()));
}

fn long_history(model: &Model) -> Vec<AgentMessage> {
    let long = "历史".repeat(200);
    vec![
        user(&long),
        assistant_message(model, "ok"),
        user(&long),
        assistant_message(model, "ok"),
    ]
}

#[tokio::test]
async fn hook_can_provide_a_summary() {
    let model = Model {
        context_window: 100,
        ..test_model()
    };
    let service = ScriptedStreamService::new(vec![text_script(&model, "ok")]);
    let recorder = service.clone();
    let hook = Arc::new(RecordingHook {
        decision: Some(CompactionDecision::Provide {
            summary: "钩子摘要".into(),
        }),
        ..RecordingHook::default()
    });
    let compactor = Arc::new(
        Compactor::new(CompactionSettings {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 20,
        })
        .with_hook(hook.clone()),
    );

    let mut options = AgentOptions::new(boxed_stream(service));
    options.initial = AgentInitialState {
        model: Some(model.clone()),
        messages: long_history(&model),
        ..AgentInitialState::default()
    };
    options.compactor = Some(Arc::clone(&compactor));
    let agent = Agent::new(options);

    agent.prompt_text("继续").await.expect("run succeeds");

    let compaction = compactor.current().expect("compacted");
    assert_eq!(compaction.summary, "钩子摘要");
    // 摘要由钩子提供，因此只有 turn 一次模型调用。
    assert_eq!(recorder.requests().len(), 1, "不额外发摘要请求");
    assert_eq!(hook.before.lock().unwrap().len(), 1);
    let after = hook.after.lock().unwrap();
    assert_eq!(after.len(), 1);
    assert!(after[0].1, "from_hook = true");
    assert!(hook.failed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn hook_can_cancel_and_the_run_continues() {
    let model = Model {
        context_window: 100,
        ..test_model()
    };
    let service = ScriptedStreamService::new(vec![text_script(&model, "ok")]);
    let recorder = service.clone();
    let hook = Arc::new(RecordingHook {
        decision: Some(CompactionDecision::Cancel),
        ..RecordingHook::default()
    });
    let compactor = Arc::new(
        Compactor::new(CompactionSettings {
            enabled: true,
            reserve_tokens: 1,
            keep_recent_tokens: 20,
        })
        .with_hook(hook.clone()),
    );

    let mut options = AgentOptions::new(boxed_stream(service));
    options.initial = AgentInitialState {
        model: Some(model.clone()),
        messages: long_history(&model),
        ..AgentInitialState::default()
    };
    options.compactor = Some(Arc::clone(&compactor));
    let agent = Agent::new(options);

    // 压缩被取消不阻断本轮：还是正常跑完，只是没压缩。
    agent.prompt_text("继续").await.expect("run succeeds");

    assert!(compactor.current().is_none(), "取消后没有压缩结果");
    assert_eq!(recorder.requests().len(), 1);
    assert_eq!(hook.failed.lock().unwrap().len(), 1, "报告了取消");
}

#[tokio::test]
async fn overflow_error_triggers_one_recovery() {
    let model = Model {
        // 窗口够大：不会因为阈值自动压缩，只有 provider 报溢出时才走恢复路径。
        context_window: 100_000,
        ..test_model()
    };
    let service = ScriptedStreamService::new(vec![
        // 第 1 次：provider 报上下文溢出（编码在流里）。
        error_script(&model, "context length exceeded"),
        // 恢复：先跑一次摘要，再重试 turn。
        text_script(&model, "## Goal\n恢复后的摘要"),
        text_script(&model, "ok"),
    ]);
    let recorder = service.clone();
    let compactor = Arc::new(Compactor::new(CompactionSettings {
        enabled: true,
        reserve_tokens: 1,
        keep_recent_tokens: 20,
    }));

    let mut options = AgentOptions::new(boxed_stream(service));
    options.initial = AgentInitialState {
        model: Some(model.clone()),
        messages: long_history(&model),
        ..AgentInitialState::default()
    };
    options.compactor = Some(Arc::clone(&compactor));
    let agent = Agent::new(options);

    agent.prompt_text("继续").await.expect("run succeeds");

    assert_eq!(recorder.requests().len(), 3, "溢出 → 摘要 → 重试");
    let compaction = compactor.current().expect("recovery compacted");
    assert!(compaction.summary.contains("恢复后的摘要"));

    // 失败的尝试被回滚，transcript 以成功的回复收尾。
    let state = agent.state();
    let last = state.messages.last().expect("non-empty");
    match last {
        AgentMessage::Message(Message::Assistant(assistant)) => {
            assert_eq!(assistant.stop_reason, StopReason::Stop);
        }
        other => panic!("expected the retried assistant reply, got {other:?}"),
    }
    // 4 条种子消息 + 1 条用户输入 + 1 条成功回复。
    for (index, message) in state.messages.iter().enumerate() {
        eprintln!("DEBUG {index}: {:?}", message.role());
    }
    assert_eq!(state.messages.len(), 6, "失败的尝试不留在 transcript");
}

#[tokio::test]
async fn persisted_compaction_restores_without_resummarizing() {
    let model = Model {
        // 阈值 299：原始 400 tokens 会触发；恢复后的投影 ≈225 tokens 不会。
        context_window: 300,
        ..test_model()
    };
    // 只有 turn 一次调用：恢复的压缩让首次请求就是压缩后的投影，不需要再摘要。
    let service = ScriptedStreamService::new(vec![text_script(&model, "ok")]);
    let recorder = service.clone();
    let compactor = Arc::new(Compactor::new(CompactionSettings {
        enabled: true,
        reserve_tokens: 1,
        keep_recent_tokens: 20,
    }));
    compactor.restore(Compaction {
        summary: "持久化的摘要".into(),
        first_kept: 2,
        tokens_before: 5000,
    });

    let mut options = AgentOptions::new(boxed_stream(service));
    let long = "历史".repeat(400);
    options.initial = AgentInitialState {
        model: Some(model.clone()),
        messages: vec![
            user(&long),
            assistant_message(&model, "ok"),
            user(&long),
            assistant_message(&model, "ok"),
        ],
        ..AgentInitialState::default()
    };
    options.compactor = Some(Arc::clone(&compactor));
    let agent = Agent::new(options);

    agent.prompt_text("继续").await.expect("run succeeds");

    assert_eq!(recorder.requests().len(), 1, "恢复后不重新摘要");
    assert!(
        contains_summary(recorder.requests()[0].context.messages()),
        "首个请求就用压缩后的投影"
    );
    assert_eq!(
        compactor.current().map(|c| c.summary),
        Some("持久化的摘要".to_owned()),
        "压缩状态保持不变"
    );
}
