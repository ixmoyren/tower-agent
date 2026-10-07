//! Anthropic provider 的端到端测试（脚本化传输层，无网络）。

#[path = "provider_support/mod.rs"]
mod support;

use serde_json::json;
use support::{
    ScriptedResponse, ScriptedTransport, collect, kinds, request, system, test_model, user,
};
use tower_agent_provider::{
    AnthropicProvider, AssistantMessageEvent, ContentBlock, PiError, StopReason, StreamRequest,
    ThinkingLevel, Tool, ToolResultMessage, provider::anthropic::API,
};

/// 一段完整的 Anthropic 文本流（含 message_start / text / message_delta / message_stop）。
fn text_sse() -> String {
    [
        r#"event: message_start"#,
        r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-test","usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":2,"cache_creation_input_tokens":3}}}"#,
        r#""#,
        r#"event: content_block_start"#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#""#,
        r#"event: content_block_delta"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        r#""#,
        r#"event: content_block_delta"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" 世界"}}"#,
        r#""#,
        r#"event: content_block_stop"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#""#,
        r#"event: message_delta"#,
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8,"output_tokens_details":{"thinking_tokens":2}}}"#,
        r#""#,
        r#"event: message_stop"#,
        r#"data: {"type":"message_stop"}"#,
        r#""#,
        "",
    ]
        .join("\n")
}

fn provider_with(transport: ScriptedTransport) -> AnthropicProvider<ScriptedTransport> {
    AnthropicProvider::new(transport).with_api_key("sk-test")
}

fn text_block_text(event: &AssistantMessageEvent) -> Option<&str> {
    match event {
        AssistantMessageEvent::TextDelta { delta, .. } => Some(delta.as_str()),
        _ => None,
    }
}

#[tokio::test]
async fn maps_a_text_stream_to_gold_standard_events() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(text_sse())]);
    let provider = provider_with(transport.clone());
    let stream = provider
        .stream(request(test_model(), vec![user("hi")]))
        .await
        .expect("request is established");
    let events = collect(stream).await;

    assert_eq!(
        kinds(&events),
        vec![
            "start",
            "text_start",
            "text_delta",
            "text_delta",
            "text_end",
            "done"
        ]
    );
    assert_eq!(text_block_text(&events[2]), Some("Hello"));
    assert_eq!(text_block_text(&events[3]), Some(" 世界"));

    let message = events
        .last()
        .and_then(AssistantMessageEvent::terminal_message)
        .expect("terminal message");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.response_id.as_deref(), Some("msg_1"));
    assert_eq!(message.content.len(), 1);
    match &message.content[0] {
        ContentBlock::Text(text) => assert_eq!(text.text, "Hello 世界"),
        other => panic!("expected text, got {other:?}"),
    }
    // message_start 带上 input/cache，message_delta 覆盖 output 与 reasoning。
    assert_eq!(message.usage.input, 10);
    assert_eq!(message.usage.output, 8);
    assert_eq!(message.usage.cache_read, 2);
    assert_eq!(message.usage.cache_write, 3);
    assert_eq!(message.usage.reasoning, Some(2));
    assert_eq!(message.usage.total_tokens, 23);
    // 成本按模型费率计算：input 10 * 3/1e6 + output 8 * 15/1e6 + cache 2*0.3/1e6 + 3*3.75/1e6
    let expected = 10.0 / 1e6 * 3.0 + 8.0 / 1e6 * 15.0 + 2.0 / 1e6 * 0.3 + 3.0 / 1e6 * 3.75;
    assert!((message.usage.cost.total - expected).abs() < 1e-12);
}

#[tokio::test]
async fn assembles_events_split_across_body_chunks() {
    // 同一个 SSE 载荷切成两块（切点在事件中间），事件序列必须完全一致。
    let sse = text_sse();
    let split = 120;
    let transport = ScriptedTransport::new(vec![ScriptedResponse::chunked(
        200,
        Default::default(),
        vec![
            sse.as_bytes()[..split].to_vec(),
            sse.as_bytes()[split..].to_vec(),
        ],
    )]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(test_model(), vec![user("hi")]))
            .await
            .expect("established"),
    )
        .await;
    assert_eq!(
        kinds(&events),
        vec![
            "start",
            "text_start",
            "text_delta",
            "text_delta",
            "text_end",
            "done"
        ]
    );
    let message = events
        .last()
        .and_then(AssistantMessageEvent::terminal_message)
        .unwrap();
    match &message.content[0] {
        ContentBlock::Text(text) => assert_eq!(text.text, "Hello 世界"),
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test]
async fn sends_the_expected_anthropic_request() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(text_sse())]);
    let provider = provider_with(transport.clone());
    let model = test_model();
    let mut options = tower_agent_provider::SimpleStreamOptions::default();
    options.base.max_tokens = Some(256);
    options.base.temperature = Some(0.25);
    let mut stream_request = request(
        model,
        vec![
            system(
                "你是助手",
                vec![Tool {
                    name: "get_weather".into(),
                    label: "get_weather".into(),
                    description: "查天气".into(),
                    parameters: json!({ "type": "object" }),
                }],
            ),
            user("你好"),
        ],
    );
    stream_request.options = options;
    let _ = provider.stream(stream_request).await.expect("established");

    let sent = transport.requests();
    assert_eq!(sent.len(), 1);
    let sent = &sent[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.url, "https://api.anthropic.com/v1/messages");
    assert_eq!(
        sent.headers.get("x-api-key").map(String::as_str),
        Some("sk-test")
    );
    assert_eq!(
        sent.headers.get("anthropic-version").map(String::as_str),
        Some("2023-06-01")
    );
    assert_eq!(
        sent.headers.get("content-type").map(String::as_str),
        Some("application/json")
    );

    let body: serde_json::Value = serde_json::from_slice(&sent.body).expect("json body");
    assert_eq!(body["model"], json!("claude-test"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["max_tokens"], json!(256));
    assert_eq!(body["temperature"], json!(0.25));
    assert_eq!(body["system"][0]["text"], json!("你是助手"));
    assert_eq!(body["messages"][0]["role"], json!("user"));
    assert_eq!(body["messages"][0]["content"][0]["text"], json!("你好"));
    assert_eq!(body["tools"][0]["name"], json!("get_weather"));
    assert_eq!(
        body["tools"][0]["input_schema"],
        json!({ "type": "object" })
    );
}

#[tokio::test]
async fn streams_tool_calls_with_partial_json() {
    let sse = [
        r#"data: {"type":"message_start","message":{"id":"msg_2","usage":{"input_tokens":1,"output_tokens":1}}}"#,
        r#""#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool_1","name":"get_weather","input":{}}}"#,
        r#""#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\""}}"#,
        r#""#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":":\"SF\"}"}}"#,
        r#""#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#""#,
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}"#,
        r#""#,
        r#"data: {"type":"message_stop"}"#,
        r#""#,
        "",
    ]
        .join("\n");
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(test_model(), vec![user("天气")]))
            .await
            .expect("established"),
    )
        .await;

    assert_eq!(
        kinds(&events),
        vec![
            "start",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ]
    );
    let AssistantMessageEvent::ToolCallEnd { tool_call, .. } = &events[4] else {
        panic!("expected toolcall_end");
    };
    assert_eq!(tool_call.name, "get_weather");
    assert_eq!(tool_call.arguments.get("city"), Some(&json!("SF")));
    let message = events
        .last()
        .and_then(AssistantMessageEvent::terminal_message)
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.content.len(), 1);
}

#[tokio::test]
async fn maps_thinking_blocks() {
    let sse = [
        r#"data: {"type":"message_start","message":{"id":"msg_3","usage":{"input_tokens":1,"output_tokens":1}}}"#,
        r#""#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
        r#""#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"想想"}}"#,
        r#""#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
        r#""#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#""#,
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        r#""#,
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"答案"}}"#,
        r#""#,
        r#"data: {"type":"content_block_stop","index":1}"#,
        r#""#,
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#,
        r#""#,
        r#"data: {"type":"message_stop"}"#,
        r#""#,
        "",
    ]
        .join("\n");
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(test_model(), vec![user("思考")]))
            .await
            .expect("established"),
    )
        .await;

    assert_eq!(
        kinds(&events),
        vec![
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "text_start",
            "text_delta",
            "text_end",
            "done"
        ]
    );
    let message = events
        .last()
        .and_then(AssistantMessageEvent::terminal_message)
        .unwrap();
    match &message.content[0] {
        ContentBlock::Thinking(thinking) => {
            assert_eq!(thinking.thinking, "想想");
            assert_eq!(thinking.thinking_signature.as_deref(), Some("sig"));
        }
        other => panic!("expected thinking, got {other:?}"),
    }
}

#[tokio::test]
async fn renders_tool_results_and_images_in_the_request() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(text_sse())]);
    let provider = provider_with(transport.clone());
    let tool_result = tower_agent_provider::Message::ToolResult(ToolResultMessage {
        tool_call_id: "tool_1".into(),
        tool_name: "get_weather".into(),
        content: vec![ContentBlock::Text(tower_agent_provider::TextContent {
            text: "晴".into(),
            text_signature: None,
        })],
        details: None,
        is_error: false,
        timestamp: 0,
    });
    let image = tower_agent_provider::Message::User(tower_agent_provider::UserMessage {
        content: tower_agent_provider::UserContent::Blocks(vec![ContentBlock::Image(
            tower_agent_provider::ImageContent {
                data: "aGk=".into(),
                mime_type: "image/png".into(),
            },
        )]),
        timestamp: 0,
    });
    let _ = provider
        .stream(request(
            test_model(),
            vec![user("先看一眼"), tool_result, image],
        ))
        .await
        .expect("established");

    let sent = transport.requests();
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).expect("json");
    assert_eq!(
        body["messages"][1]["content"][0]["type"],
        json!("tool_result")
    );
    assert_eq!(
        body["messages"][1]["content"][0]["tool_use_id"],
        json!("tool_1")
    );
    assert_eq!(body["messages"][2]["content"][0]["type"], json!("image"));
    assert_eq!(
        body["messages"][2]["content"][0]["source"]["media_type"],
        json!("image/png")
    );
}

#[tokio::test]
async fn enables_thinking_when_requested() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(text_sse())]);
    let provider = provider_with(transport.clone());
    let mut stream_request = request(test_model(), vec![user("想一下")]);
    stream_request.options.reasoning = Some(ThinkingLevel::High);
    stream_request.options.base.temperature = Some(0.9);
    let _ = provider.stream(stream_request).await.expect("established");

    let sent = transport.requests();
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).expect("json");
    assert_eq!(body["thinking"]["type"], json!("enabled"));
    assert_eq!(body["thinking"]["budget_tokens"], json!(16384));
    // thinking 与 temperature 互斥。
    assert!(body.get("temperature").is_none());
}

#[tokio::test]
async fn http_error_becomes_an_error_event_in_the_stream() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::text(
        401,
        r#"{"error":{"type":"authentication_error","message":"invalid key"}}"#,
    )]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(test_model(), vec![user("hi")]))
            .await
            .expect("4xx 编码进流而不是 Service 错误"),
    )
        .await;

    assert_eq!(kinds(&events), vec!["error"]);
    let AssistantMessageEvent::Error { error, .. } = &events[0] else {
        panic!("expected error event");
    };
    assert_eq!(error.stop_reason, StopReason::Error);
    assert!(
        error
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("401") && message.contains("invalid key")),
        "got: {:?}",
        error.error_message
    );
}

#[tokio::test]
async fn retryable_status_is_a_service_error() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::text(500, "boom")]);
    let provider = provider_with(transport);
    let error = provider
        .stream(request(test_model(), vec![user("hi")]))
        .await
        .expect_err("5xx 交给 retry 层");
    assert!(matches!(error, PiError::Provider { .. }), "got: {error}");
}

#[tokio::test]
async fn missing_api_key_is_a_setup_error() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(text_sse())]);
    let provider = AnthropicProvider::new(transport.clone());
    let error = provider
        .stream(request(test_model(), vec![user("hi")]))
        .await
        .expect_err("需要凭据");
    assert!(
        matches!(error, PiError::MissingCredentials { .. }),
        "got: {error}"
    );
    assert_eq!(transport.request_count(), 0, "没有凭据时不应发出请求");
}

#[tokio::test]
async fn sse_error_event_becomes_an_error_event() {
    let sse = [
        r#"data: {"type":"message_start","message":{"id":"m","usage":{"input_tokens":1,"output_tokens":0}}}"#,
        r#""#,
        r#"event: error"#,
        r#"data: {"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#,
        r#""#,
        "",
    ]
        .join("\n");
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(test_model(), vec![user("hi")]))
            .await
            .expect("established"),
    )
        .await;

    assert_eq!(kinds(&events), vec!["start", "error"]);
    let AssistantMessageEvent::Error { error, .. } = events.last().unwrap() else {
        panic!("expected error event");
    };
    assert!(
        error
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("overloaded_error")),
        "got: {:?}",
        error.error_message
    );
}

#[tokio::test]
async fn stream_without_a_stop_reason_is_an_error() {
    let sse = [
        r#"data: {"type":"message_start","message":{"id":"m","usage":{"input_tokens":1,"output_tokens":0}}}"#,
        r#""#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#""#,
        "",
    ]
        .join("\n");
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(test_model(), vec![user("hi")]))
            .await
            .expect("established"),
    )
        .await;

    assert_eq!(kinds(&events), vec!["start", "text_start", "error"]);
    let AssistantMessageEvent::Error { error, .. } = events.last().unwrap() else {
        panic!("expected error event");
    };
    assert!(
        error
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("without a stop reason")),
        "got: {:?}",
        error.error_message
    );
}

#[tokio::test]
async fn maps_max_tokens_and_refusal_stop_reasons() {
    async fn run(
        stop_reason: &str,
        details: Option<serde_json::Value>,
    ) -> Vec<AssistantMessageEvent> {
        let delta = match details {
            Some(details) => json!({ "stop_reason": stop_reason, "stop_details": details }),
            None => json!({ "stop_reason": stop_reason }),
        };
        let delta_event = json!({
            "type": "message_delta",
            "delta": delta,
            "usage": { "output_tokens": 3 },
        });
        let sse = format!(
            "{}\n\ndata: {delta_event}\n\n",
            r#"data: {"type":"message_start","message":{"id":"m","usage":{"input_tokens":1,"output_tokens":1}}}"#,
        );
        let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
        let provider = provider_with(transport);
        collect(
            provider
                .stream(request(test_model(), vec![user("hi")]))
                .await
                .expect("established"),
        )
            .await
    }

    let events = run("max_tokens", None).await;
    let message = events
        .last()
        .and_then(AssistantMessageEvent::terminal_message)
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Length);

    let events = run("refusal", Some(json!({ "explanation": "policy" }))).await;
    let AssistantMessageEvent::Error { error, .. } = events.last().unwrap() else {
        panic!("refusal 应当是错误事件");
    };
    assert_eq!(error.error_message.as_deref(), Some("policy"));

    let events = run("something_new", None).await;
    let AssistantMessageEvent::Error { error, .. } = events.last().unwrap() else {
        panic!("未知 stop reason 应当是错误事件");
    };
    assert!(
        error
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("Unhandled stop reason")),
        "got: {:?}",
        error.error_message
    );
}

#[tokio::test]
async fn api_constant_matches_pi() {
    assert_eq!(API, "anthropic-messages");
    let _: fn(StreamRequest) = |_| {};
}
