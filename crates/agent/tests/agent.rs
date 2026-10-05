//! 有状态 `Agent` 的行为对齐测试（design.md 第 7 节清单 5、6）。

mod support;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agent::{Agent, AgentEvent, AgentInitialState, AgentOptions, AgentState, QueueMode};
use tokio::sync::Semaphore;

use support::{ScriptedStreamService, boxed_stream, test_model, text_script, user};

fn agent_with(service: ScriptedStreamService) -> Agent {
    let mut options = AgentOptions::new(boxed_stream(service));
    options.initial = AgentInitialState {
        model: Some(test_model()),
        ..AgentInitialState::default()
    };
    Agent::new(options)
}

#[tokio::test]
async fn prompt_updates_state_and_emits_the_lifecycle() {
    let model = test_model();
    let service = ScriptedStreamService::new(vec![text_script(&model, "hello")]);
    let recorder = service.clone();
    let agent = agent_with(service);

    let mut subscription = agent.subscribe();
    let collector = tokio::spawn(async move {
        let mut kinds = Vec::new();
        while let Some(event) = subscription.recv().await {
            let kind = event.kind();
            kinds.push(kind);
            if kind == "agent_end" {
                break;
            }
        }
        kinds
    });

    agent.prompt_text("hi").await.expect("prompt succeeds");
    let kinds = collector.await.expect("collector joins");

    assert_eq!(kinds.first(), Some(&"agent_start"));
    assert_eq!(kinds.last(), Some(&"agent_end"));

    let state = agent.state();
    assert!(!state.is_streaming);
    assert_eq!(state.messages.len(), 2, "user prompt plus assistant reply");
    assert_eq!(recorder.requests().len(), 1);
}

#[tokio::test]
async fn prompt_resolves_only_after_subscribers_finish_agent_end() {
    let model = test_model();
    let agent = agent_with(ScriptedStreamService::new(vec![text_script(&model, "hi")]));

    let finished = Arc::new(AtomicBool::new(false));
    let mut subscription = agent.subscribe();
    let listener = tokio::spawn({
        let finished = Arc::clone(&finished);
        async move {
            while let Some(event) = subscription.recv().await {
                if event.kind() == "agent_end" {
                    // listener 在 agent_end 上的异步结算属于 idle 判定范围。
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    finished.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
    });

    agent.prompt_text("hi").await.expect("prompt succeeds");
    listener.await.expect("listener joins");

    assert!(
        finished.load(Ordering::SeqCst),
        "prompt must not resolve before awaited listeners settle"
    );
}

#[tokio::test]
async fn wait_for_idle_waits_for_subscribers() {
    let model = test_model();
    let agent = Arc::new(agent_with(ScriptedStreamService::new(vec![text_script(
        &model, "hi",
    )])));

    let finished = Arc::new(AtomicBool::new(false));
    let mut subscription = agent.subscribe();
    let _listener = tokio::spawn({
        let finished = Arc::clone(&finished);
        async move {
            while let Some(event) = subscription.recv().await {
                if event.kind() == "agent_end" {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    finished.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
    });

    let runner = {
        let agent = Arc::clone(&agent);
        tokio::spawn(async move { agent.prompt_text("hi").await })
    };

    // 等 run 真正开始，再等待 idle。
    while !agent.is_streaming() {
        tokio::task::yield_now().await;
    }
    agent.wait_for_idle().await;

    assert!(finished.load(Ordering::SeqCst));
    runner
        .await
        .expect("runner joins")
        .expect("prompt succeeds");
    assert!(!agent.state().is_streaming);
}

#[tokio::test]
async fn steering_queue_respects_one_at_a_time() {
    let model = test_model();
    let service = ScriptedStreamService::new(vec![
        text_script(&model, "first"),
        text_script(&model, "second"),
    ]);
    let recorder = service.clone();
    let agent = agent_with(service);

    agent.set_steering_mode(QueueMode::OneAtATime);
    agent.steer(user("s1"));
    agent.steer(user("s2"));
    assert_eq!(
        agent.peek_queued_messages().len(),
        1,
        "one-at-a-time peeks one"
    );

    agent.prompt_text("go").await.expect("prompt succeeds");

    assert!(
        !agent.has_queued_messages(),
        "both steer messages were drained"
    );
    assert_eq!(
        recorder.requests().len(),
        2,
        "one steer message per turn => two provider requests"
    );
}

#[tokio::test]
async fn steering_queue_drains_all_at_once() {
    let model = test_model();
    let service = ScriptedStreamService::new(vec![text_script(&model, "first")]);
    let recorder = service.clone();
    let agent = agent_with(service);

    agent.set_steering_mode(QueueMode::All);
    agent.steer(user("s1"));
    agent.steer(user("s2"));
    assert_eq!(agent.peek_queued_messages().len(), 2);

    agent.prompt_text("go").await.expect("prompt succeeds");

    assert!(!agent.has_queued_messages());
    assert_eq!(
        recorder.requests().len(),
        1,
        "both steer messages land in the first turn"
    );
}

#[tokio::test]
async fn reset_restores_the_system_baseline() {
    let model = test_model();
    let mut options =
        AgentOptions::new(boxed_stream(ScriptedStreamService::new(vec![text_script(
            &model, "hello",
        )])));
    options.initial = AgentInitialState {
        model: Some(model),
        system_prompt: Some("base prompt".into()),
        ..AgentInitialState::default()
    };
    let agent = Agent::new(options);

    assert_eq!(agent.state().system_prompt, "base prompt");
    agent.prompt_text("hi").await.expect("prompt succeeds");
    assert!(agent.state().messages.len() > 1);

    agent.reset().expect("reset succeeds");
    let state = agent.state();
    assert_eq!(state.messages.len(), 1, "only the replayed system message");
    assert_eq!(state.system_prompt, "base prompt");
}

#[tokio::test]
async fn prompt_while_streaming_is_rejected() {
    let model = test_model();
    let gate = Arc::new(Semaphore::new(0));
    let service =
        ScriptedStreamService::new(vec![text_script(&model, "hi")]).gated(Arc::clone(&gate));
    let agent = Arc::new(agent_with(service));

    let runner = {
        let agent = Arc::clone(&agent);
        tokio::spawn(async move { agent.prompt_text("first").await })
    };

    while !agent.is_streaming() {
        tokio::task::yield_now().await;
    }
    let second = agent.prompt_text("second").await;
    assert!(second.is_err(), "a second prompt while streaming must fail");

    gate.add_permits(1);
    runner
        .await
        .expect("runner joins")
        .expect("first prompt succeeds");
}

#[tokio::test]
async fn state_snapshot_tracks_pending_tool_calls() {
    let state = AgentState {
        system_prompt: String::new(),
        model: test_model(),
        thinking_level: ai::ModelThinkingLevel::Off,
        tools: Vec::new(),
        messages: Vec::new(),
        is_streaming: false,
        streaming_message: None,
        pending_tool_calls: Default::default(),
        error_message: None,
    };
    assert!(state.pending_tool_calls.is_empty());
    let _ = AgentEvent::AgentStart;
}

#[tokio::test]
async fn agent_serves_requests_as_a_tower_service() {
    use agent::{AgentRequest, AgentServiceExt as _};
    use tower::ServiceExt as _;

    let model = test_model();
    let service = ScriptedStreamService::new(vec![text_script(&model, "hello")]);
    let response = agent_with(service)
        .boxed_agent()
        .oneshot(AgentRequest {
            messages: vec![user("hi")],
            ..AgentRequest::default()
        })
        .await
        .expect("agent service call succeeds");

    assert_eq!(response.text, "hello");
    assert_eq!(response.messages.len(), 1, "one assistant reply");
    assert_eq!(response.stop_reason, Some(ai::StopReason::Stop));
}
