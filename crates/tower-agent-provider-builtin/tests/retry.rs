//! Transport middleware tests: header injection and retry.

#[path = "provider_support/mod.rs"]
mod support;

use std::time::Duration;

use support::{ScriptedResponse, ScriptedTransport};
use tower_agent_provider::{
    HeaderTransport, RetryTransport,
    http::{HttpHeaders, HttpTransport},
};

fn request() -> tower_agent_provider::http::HttpRequest {
    tower_agent_provider::http::HttpRequest {
        method: "POST".into(),
        url: "https://example.test/v1/messages".into(),
        headers: HttpHeaders::new(),
        body: Vec::new(),
        context: None,
    }
}

fn retry_after(seconds: &str) -> HttpHeaders {
    let mut headers = HttpHeaders::new();
    headers.insert("retry-after".into(), seconds.into());
    headers
}

#[tokio::test]
async fn header_layer_injects_and_overrides_headers() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok("")]);
    let mut auth = HttpHeaders::new();
    auth.insert("X-Api-Key".into(), "sk-test".into());
    auth.insert("anthropic-version".into(), "2023-06-01".into());
    let layered = HeaderTransport::new(transport.clone(), auth);

    let mut outgoing = request();
    outgoing.headers.insert("x-api-key".into(), "stale".into());
    let _ = layered.send(outgoing).await.expect("sent");

    let sent = transport.requests();
    // 大小写不敏感地覆盖：只保留一个 x-api-key，且是新值。
    let keys: Vec<&String> = sent[0]
        .headers
        .keys()
        .filter(|key| key.eq_ignore_ascii_case("x-api-key"))
        .collect();
    assert_eq!(keys.len(), 1);
    assert_eq!(
        sent[0].headers.get(keys[0]).map(String::as_str),
        Some("sk-test")
    );
    assert_eq!(
        sent[0].headers.get("anthropic-version").map(String::as_str),
        Some("2023-06-01")
    );
}

#[tokio::test]
async fn retries_retryable_status_until_success() {
    let transport = ScriptedTransport::new(vec![
        ScriptedResponse::text(500, "boom"),
        ScriptedResponse::text(429, "slow down"),
        ScriptedResponse::ok("{\"ok\":true}"),
    ]);
    let layered = RetryTransport::new(transport.clone(), 3, Duration::from_millis(50));
    let response = layered.send(request()).await.expect("eventually succeeds");
    assert_eq!(response.status, 200);
    assert_eq!(transport.request_count(), 3);
}

#[tokio::test]
async fn retries_transport_errors() {
    let transport = ScriptedTransport::new(vec![
        ScriptedResponse::Error(tower_agent_provider::http::HttpError::retryable(
            "connection reset",
        )),
        ScriptedResponse::ok("ok"),
    ]);
    let layered = RetryTransport::new(transport.clone(), 2, Duration::from_millis(20));
    let response = layered.send(request()).await.expect("retried");
    assert_eq!(response.status, 200);
    assert_eq!(transport.request_count(), 2);
}

#[tokio::test]
async fn gives_up_after_max_retries() {
    let transport = ScriptedTransport::new(vec![
        ScriptedResponse::text(500, "a"),
        ScriptedResponse::text(500, "b"),
    ]);
    let layered = RetryTransport::new(transport.clone(), 1, Duration::from_millis(20));
    let response = layered
        .send(request())
        .await
        .expect("returns the last response");
    assert_eq!(response.status, 500);
    assert_eq!(transport.request_count(), 2, "1 次初始 + 1 次重试");
}

#[tokio::test]
async fn honors_a_short_retry_after() {
    let transport = ScriptedTransport::new(vec![
        ScriptedResponse::chunked(429, retry_after("0.05"), vec![Vec::new()]),
        ScriptedResponse::ok("ok"),
    ]);
    let layered = RetryTransport::new(transport.clone(), 2, Duration::from_secs(5));
    let started = std::time::Instant::now();
    let response = layered.send(request()).await.expect("retried");
    assert_eq!(response.status, 200);
    assert!(
        started.elapsed() >= Duration::from_millis(40),
        "应当按 Retry-After 等待"
    );
}

#[tokio::test]
async fn refuses_to_wait_when_retry_after_exceeds_the_cap() {
    // pi 的语义：服务器要求的长延迟超过上限时立即失败，而不是等待或静默截断。
    let transport = ScriptedTransport::new(vec![ScriptedResponse::chunked(
        429,
        retry_after("120"),
        vec![Vec::new()],
    )]);
    let layered = RetryTransport::new(transport.clone(), 3, Duration::from_secs(1));
    let started = std::time::Instant::now();
    let response = layered.send(request()).await.expect("returns immediately");
    assert_eq!(response.status, 429);
    assert_eq!(transport.request_count(), 1, "没有重试");
    assert!(started.elapsed() < Duration::from_secs(1), "没有真的等待");
}

#[tokio::test]
async fn does_not_retry_non_retryable_errors() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::Error(
        tower_agent_provider::http::HttpError::fatal("bad request"),
    )]);
    let layered = RetryTransport::new(transport.clone(), 3, Duration::from_millis(10));
    let error = layered.send(request()).await.expect_err("fatal");
    assert!(!error.retryable);
    assert_eq!(transport.request_count(), 1);
}
