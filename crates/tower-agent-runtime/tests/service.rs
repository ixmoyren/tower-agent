//! 无状态 / 流式 / 池化服务的测试（`docs/tower-agent-architecture.md` §4.2）。

mod support;

use std::sync::Arc;

use futures_util::StreamExt as _;
use tower::ServiceExt as _;
use tower_agent_core::{AgentEvent, AgentRequest};
use tower_agent_provider::StopReason;
use tower_agent_runtime::{AgentPool, AgentService, AgentStreamService, MemorySessionStore};

use support::{ScriptedStreamService, boxed_stream, test_model, text_script, user};

fn request(text: &str) -> AgentRequest {
    AgentRequest {
        messages: vec![user(text)],
        ..AgentRequest::default()
    }
}

#[tokio::test]
async fn stateless_service_does_not_share_a_transcript() {
    let model = test_model();
    let service = AgentService::new(boxed_stream(ScriptedStreamService::new(vec![
        text_script(&model, "one"),
        text_script(&model, "two"),
    ])))
    .with_model(model);

    let first = service.clone().oneshot(request("hi")).await.unwrap();
    let second = service.oneshot(request("hi")).await.unwrap();

    assert_eq!(first.text, "one");
    assert_eq!(second.text, "two");
    assert_eq!(first.stop_reason, Some(StopReason::Stop));
}

#[tokio::test]
async fn session_store_accumulates_the_transcript() {
    let model = test_model();
    let store = Arc::new(MemorySessionStore::new());
    let service = AgentService::new(boxed_stream(ScriptedStreamService::new(vec![
        text_script(&model, "one"),
        text_script(&model, "two"),
    ])))
    .with_model(model)
    .with_sessions(store.clone());

    let first = service
        .clone()
        .oneshot(AgentRequest {
            messages: vec![user("hi")],
            session_id: Some("s".into()),
            ..AgentRequest::default()
        })
        .await
        .unwrap();
    let second = service
        .oneshot(AgentRequest {
            messages: vec![user("again")],
            session_id: Some("s".into()),
            ..AgentRequest::default()
        })
        .await
        .unwrap();

    assert_eq!(first.text, "one");
    assert_eq!(second.text, "two");
    assert_eq!(store.len(), 1, "one session persisted");
}

#[tokio::test]
async fn streaming_service_emits_the_event_protocol() {
    let model = test_model();
    let service = AgentStreamService::new(
        AgentService::new(boxed_stream(ScriptedStreamService::new(vec![text_script(
            &model, "hi",
        )])))
        .with_model(model),
    );

    let mut stream = service.oneshot(request("hello")).await.unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }

    assert!(
        matches!(events.first(), Some(AgentEvent::AgentStart)),
        "stream starts with agent_start"
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::AgentEnd { .. })),
        "stream ends with agent_end"
    );
}

#[tokio::test]
async fn pool_serves_requests_up_to_its_concurrency() {
    let model = test_model();
    let service = AgentService::new(boxed_stream(ScriptedStreamService::new(vec![
        text_script(&model, "a"),
        text_script(&model, "b"),
    ])))
    .with_model(model);
    let pool = AgentPool::new(service, 2);
    assert_eq!(pool.max_concurrency(), 2);

    let first = pool.clone().oneshot(request("1")).await.unwrap();
    let second = pool.oneshot(request("2")).await.unwrap();
    assert_eq!(first.text, "a");
    assert_eq!(second.text, "b");
}
