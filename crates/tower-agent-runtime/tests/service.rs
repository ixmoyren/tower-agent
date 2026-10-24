//! 无状态 / 流式 / 池化服务的测试（`docs/tower-agent-architecture.md` §4.2）。

mod support;

use std::{sync::Arc, time::Duration};

use futures_util::StreamExt as _;
use tokio::sync::Semaphore;
use tower::ServiceExt as _;
use tower_agent_core::{AgentError, AgentEvent, AgentRequest};
use tower_agent_provider::StopReason;
use tower_agent_runtime::{
    AgentPool, AgentService, AgentStreamService, MemorySessionStore, ShutdownCoordinator,
    ShutdownOptions,
};

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

#[tokio::test]
async fn shutdown_rejects_new_requests_and_drains_active_work() {
    let model = test_model();
    let gate = Arc::new(Semaphore::new(0));
    let shutdown = ShutdownCoordinator::new();
    let service = AgentService::new(boxed_stream(
        ScriptedStreamService::new(vec![text_script(&model, "finished")]).gated(Arc::clone(&gate)),
    ))
    .with_model(model)
    .with_shutdown(shutdown.clone());

    let active = tokio::spawn({
        let service = service.clone();
        async move { service.oneshot(request("active")).await }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while shutdown.inflight() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("active request starts");

    assert!(shutdown.begin_shutdown());
    let error = service
        .clone()
        .oneshot(request("rejected"))
        .await
        .expect_err("new work is rejected");
    assert!(matches!(error, AgentError::ShuttingDown));

    let drain = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            shutdown
                .shutdown_with(ShutdownOptions {
                    grace_period: Duration::from_secs(1),
                    cancellation_timeout: Duration::from_millis(100),
                })
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(shutdown.inflight(), 1);
    gate.add_permits(1);

    let response = active.await.expect("active task").expect("active request");
    assert_eq!(response.text, "finished");
    let report = drain.await.expect("drain task");
    assert!(report.drained);
    assert!(!report.forced);
    assert_eq!(report.remaining_operations, 0);
}

#[tokio::test]
async fn pool_rejects_new_requests_during_shutdown() {
    let model = test_model();
    let shutdown = ShutdownCoordinator::new();
    let service = AgentService::new(boxed_stream(ScriptedStreamService::new(vec![text_script(
        &model, "unused",
    )])))
    .with_model(model)
    .with_shutdown(shutdown.clone());
    let pool = AgentPool::new(service, 1);
    shutdown.begin_shutdown();

    let error = pool
        .oneshot(request("rejected"))
        .await
        .expect_err("pool rejects new work");
    assert!(matches!(error, AgentError::ShuttingDown));
}

#[tokio::test]
async fn streaming_request_remains_inflight_until_background_run_finishes() {
    let model = test_model();
    let gate = Arc::new(Semaphore::new(0));
    let shutdown = ShutdownCoordinator::new();
    let service = AgentService::new(boxed_stream(
        ScriptedStreamService::new(vec![text_script(&model, "streamed")]).gated(Arc::clone(&gate)),
    ))
    .with_model(model)
    .with_shutdown(shutdown.clone());

    let mut stream = service.run_stream(request("stream"));
    tokio::time::timeout(Duration::from_secs(1), async {
        while shutdown.inflight() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("streaming request starts");
    shutdown.begin_shutdown();
    let drain = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { shutdown.shutdown().await }
    });
    gate.add_permits(1);

    while stream.next().await.is_some() {}
    let report = drain.await.expect("drain task");
    assert!(report.drained);
    assert_eq!(report.remaining_operations, 0);
}
