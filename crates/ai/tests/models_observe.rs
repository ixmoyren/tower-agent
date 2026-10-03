//! `Models` 路由与 `onPayload` / `onResponse` 观测层。

#[path = "provider_support/mod.rs"]
mod support;

use std::sync::{Arc, Mutex};

use ai::{
    ANTHROPIC_MESSAGES_API, AnthropicProvider, GOOGLE_GENERATIVE_AI_API, GoogleProvider, Models,
    OPENAI_COMPLETIONS_API, ObserveTransport, OpenAiProvider, PiError, RetryTransport,
};
use support::{ScriptedResponse, ScriptedTransport, collect, kinds, request, test_model, user};
use tower::ServiceExt as _;

/// 观测到的响应：(状态码, x-request-id)。
type ObservedResponses = Arc<Mutex<Vec<(u16, Option<String>)>>>;

fn anthropic_model() -> ai::Model {
    test_model()
}

fn google_model() -> ai::Model {
    ai::Model {
        api: GOOGLE_GENERATIVE_AI_API.into(),
        provider: "google".into(),
        id: "gemini-test".into(),
        ..test_model()
    }
}

fn openai_model() -> ai::Model {
    ai::Model {
        api: OPENAI_COMPLETIONS_API.into(),
        provider: "openai".into(),
        id: "gpt-test".into(),
        ..test_model()
    }
}

const ANTHROPIC_OK: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

#[tokio::test]
async fn routes_by_provider_and_api() {
    let anthropic = ScriptedTransport::new(vec![ScriptedResponse::ok(ANTHROPIC_OK)]);
    let openai = ScriptedTransport::new(vec![ScriptedResponse::ok("data: [DONE]\n\n")]);
    let google = ScriptedTransport::new(vec![ScriptedResponse::ok(
        "data: {\"candidates\":[{\"content\":{\"parts\":[]},\"finishReason\":\"STOP\"}]}\n\n",
    )]);

    let mut models = Models::new();
    models.register(
        "anthropic",
        ANTHROPIC_MESSAGES_API,
        AnthropicProvider::new(anthropic.clone()).with_api_key("sk-a"),
    );
    models.register(
        "openai",
        OPENAI_COMPLETIONS_API,
        OpenAiProvider::new(openai.clone()).with_api_key("sk-o"),
    );
    models.register(
        "google",
        GOOGLE_GENERATIVE_AI_API,
        GoogleProvider::new(google.clone()).with_api_key("goog-key"),
    );
    assert_eq!(
        models.routes(),
        vec![
            ("anthropic", ANTHROPIC_MESSAGES_API),
            ("google", GOOGLE_GENERATIVE_AI_API),
            ("openai", OPENAI_COMPLETIONS_API)
        ]
    );
    assert!(models.supports(&anthropic_model()));
    assert!(models.supports(&openai_model()));
    assert!(models.supports(&google_model()));

    let events = collect(
        models
            .clone()
            .oneshot(request(anthropic_model(), vec![user("hi")]))
            .await
            .expect("routed to anthropic"),
    )
    .await;
    assert_eq!(kinds(&events), vec!["start", "done"]);
    assert_eq!(anthropic.request_count(), 1);
    assert_eq!(
        anthropic.requests()[0].url,
        "https://api.anthropic.com/v1/messages"
    );

    let _ = models
        .clone()
        .oneshot(request(openai_model(), vec![user("hi")]))
        .await
        .expect("routed to openai");
    assert_eq!(openai.request_count(), 1);
    assert_eq!(
        openai.requests()[0].url,
        "https://api.openai.com/v1/chat/completions"
    );

    let events = collect(
        models
            .clone()
            .oneshot(request(google_model(), vec![user("hi")]))
            .await
            .expect("routed to google"),
    )
    .await;
    assert_eq!(kinds(&events), vec!["done"]);
    assert_eq!(google.request_count(), 1);
    assert_eq!(
        google.requests()[0].url,
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-test:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        google.requests()[0]
            .headers
            .get("x-goog-api-key")
            .map(String::as_str),
        Some("goog-key")
    );
}

#[tokio::test]
async fn unknown_route_is_unsupported() {
    let models = Models::new();
    let error = models
        .clone()
        .oneshot(request(anthropic_model(), vec![user("hi")]))
        .await
        .expect_err("no route");
    match error {
        PiError::Unsupported(message) => {
            assert!(message.contains("anthropic"), "got: {message}");
            assert!(message.contains("claude-test"), "got: {message}");
        }
        other => panic!("expected Unsupported, got {other}"),
    }
}

#[tokio::test]
async fn on_payload_can_observe_and_replace_the_payload() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(ANTHROPIC_OK)]);
    let seen: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = Arc::clone(&seen);
    let observed =
        ObserveTransport::new(transport.clone()).on_payload(Arc::new(move |context, payload| {
            seen_clone.lock().expect("seen").push((
                context.provider.clone(),
                context.api.clone(),
                context.model.clone(),
            ));
            // 替换载荷：注入一个自定义字段。
            let mut replacement = payload.clone();
            replacement["metadata"] = serde_json::json!({ "user_id": "observed" });
            Some(replacement)
        }));
    let provider = AnthropicProvider::new(observed).with_api_key("sk-a");
    let _ = provider
        .stream(request(anthropic_model(), vec![user("hi")]))
        .await
        .expect("established");

    let seen = seen.lock().expect("seen").clone();
    assert_eq!(
        seen,
        vec![(
            "anthropic".to_string(),
            "anthropic-messages".to_string(),
            "claude-test".to_string()
        )]
    );
    let sent = transport.requests();
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).expect("json");
    assert_eq!(body["metadata"]["user_id"], serde_json::json!("observed"));
    assert_eq!(body["stream"], serde_json::json!(true), "其余字段保持不变");
}

#[tokio::test]
async fn on_response_sees_status_and_headers() {
    let mut headers = ai::http::HttpHeaders::new();
    headers.insert("x-request-id".into(), "req-1".into());
    let transport = ScriptedTransport::new(vec![ScriptedResponse::chunked(
        200,
        headers,
        vec![ANTHROPIC_OK.as_bytes().to_vec()],
    )]);
    let seen: ObservedResponses = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = Arc::clone(&seen);
    let observed =
        ObserveTransport::new(transport).on_response(Arc::new(move |_, status, headers| {
            seen_clone
                .lock()
                .expect("seen")
                .push((status, headers.get("x-request-id").cloned()));
        }));
    let provider = AnthropicProvider::new(observed).with_api_key("sk-a");
    let events = collect(
        provider
            .stream(request(anthropic_model(), vec![user("hi")]))
            .await
            .expect("established"),
    )
    .await;
    assert_eq!(kinds(&events), vec!["start", "done"]);
    assert_eq!(
        seen.lock().expect("seen").clone(),
        vec![(200, Some("req-1".to_string()))]
    );
}

#[tokio::test]
async fn observe_and_retry_layers_compose() {
    // 观测层在重试层外侧：重试的每次尝试都会经过 onPayload。
    let transport = ScriptedTransport::new(vec![
        ScriptedResponse::text(500, "boom"),
        ScriptedResponse::ok(ANTHROPIC_OK),
    ]);
    let retrying = RetryTransport::new(transport.clone(), 2, std::time::Duration::from_millis(10));
    let calls = Arc::new(Mutex::new(0usize));
    let calls_clone = Arc::clone(&calls);
    let observed = ObserveTransport::new(retrying).on_payload(Arc::new(move |_, _payload| {
        let mut guard = calls_clone.lock().expect("calls");
        *guard += 1;
        None
    }));
    let provider = AnthropicProvider::new(observed).with_api_key("sk-a");
    let events = collect(
        provider
            .stream(request(anthropic_model(), vec![user("hi")]))
            .await
            .expect("established"),
    )
    .await;
    assert_eq!(kinds(&events), vec!["start", "done"]);
    assert_eq!(transport.request_count(), 2, "第一次 500，第二次成功");
    assert_eq!(
        *calls.lock().expect("calls"),
        1,
        "观测层在重试外侧，只看到一次载荷"
    );
}
