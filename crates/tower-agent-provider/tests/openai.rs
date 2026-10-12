//! OpenAI Chat Completions provider 的端到端测试（脚本化传输层，无网络）。

#[path = "provider_support/mod.rs"]
mod support;

use serde_json::json;
use support::{
    ScriptedResponse, ScriptedTransport, collect, kinds, request, system, test_model, user,
};
use tower_agent_provider::{
    AssistantMessageEvent, ContentBlock, OpenAiProvider, PiError, StopReason, Tool,
    ToolResultMessage,
};

fn openai_model() -> tower_agent_provider::Model {
    tower_agent_provider::Model {
        api: tower_agent_provider::OPENAI_COMPLETIONS_API.into(),
        provider: "openai".into(),
        ..test_model()
    }
}

fn provider_with(transport: ScriptedTransport) -> OpenAiProvider<ScriptedTransport> {
    OpenAiProvider::new(transport).with_api_key("sk-openai")
}

fn chunk(delta: serde_json::Value, finish: serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        json!({ "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })
    )
}

#[tokio::test]
async fn maps_a_text_stream_to_gold_standard_events() {
    let mut sse = String::new();
    sse.push_str(&chunk(
        json!({ "role": "assistant", "content": "" }),
        json!(null),
    ));
    sse.push_str(&chunk(json!({ "content": "Hello" }), json!(null)));
    sse.push_str(&chunk(json!({ "content": " 世界" }), json!(null)));
    sse.push_str(&chunk(json!({}), json!("stop")));
    sse.push_str(&format!(
        "data: {}\n\n",
        json!({ "choices": [], "usage": { "prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18, "prompt_tokens_details": { "cached_tokens": 3 }, "completion_tokens_details": { "reasoning_tokens": 2 } } })
    ));
    sse.push_str("data: [DONE]\n\n");

    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport.clone());
    let events = collect(
        provider
            .stream(request(openai_model(), vec![user("hi")]))
            .await
            .expect("established"),
    )
    .await;

    assert_eq!(
        kinds(&events),
        vec!["text_start", "text_delta", "text_delta", "text_end", "done"]
    );
    let message = events
        .last()
        .and_then(AssistantMessageEvent::terminal_message)
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Stop);
    match &message.content[0] {
        ContentBlock::Text(text) => assert_eq!(text.text, "Hello 世界"),
        other => panic!("expected text, got {other:?}"),
    }
    assert_eq!(message.usage.input, 11);
    assert_eq!(message.usage.output, 7);
    assert_eq!(message.usage.cache_read, 3);
    assert_eq!(message.usage.reasoning, Some(2));

    let sent = transport.requests();
    assert_eq!(sent[0].url, "https://api.openai.com/v1/chat/completions");
    assert_eq!(
        sent[0].headers.get("authorization").map(String::as_str),
        Some("Bearer sk-openai")
    );
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).expect("json");
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"]["include_usage"], json!(true));
    assert_eq!(body["messages"][0]["role"], json!("user"));
}

#[tokio::test]
async fn streams_tool_calls_and_reports_tool_use() {
    let mut sse = String::new();
    sse.push_str(&chunk(
        json!({ "tool_calls": [{ "index": 0, "id": "call_1", "type": "function", "function": { "name": "get_weather", "arguments": "" } }] }),
        json!(null),
    ));
    sse.push_str(&chunk(
        json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "{\"city\"" } }] }),
        json!(null),
    ));
    sse.push_str(&chunk(
        json!({ "tool_calls": [{ "index": 0, "function": { "arguments": ":\"SF\"}" } }] }),
        json!(null),
    ));
    sse.push_str(&chunk(json!({}), json!("tool_calls")));
    sse.push_str("data: [DONE]\n\n");

    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(openai_model(), vec![user("天气")]))
            .await
            .expect("established"),
    )
    .await;

    assert_eq!(
        kinds(&events),
        vec![
            "toolcall_start",
            "toolcall_delta",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ]
    );
    let AssistantMessageEvent::ToolCallEnd { tool_call, .. } = &events[3] else {
        panic!("expected toolcall_end");
    };
    assert_eq!(tool_call.name, "get_weather");
    assert_eq!(tool_call.arguments.get("city"), Some(&json!("SF")));
    let message = events
        .last()
        .and_then(AssistantMessageEvent::terminal_message)
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn maps_reasoning_content_to_thinking() {
    let mut sse = String::new();
    sse.push_str(&chunk(json!({ "reasoning_content": "想想" }), json!(null)));
    sse.push_str(&chunk(json!({ "content": "答案" }), json!(null)));
    sse.push_str(&chunk(json!({}), json!("length")));
    sse.push_str("data: [DONE]\n\n");
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(openai_model(), vec![user("想一下")]))
            .await
            .expect("established"),
    )
    .await;

    assert_eq!(
        kinds(&events),
        vec![
            "thinking_start",
            "thinking_delta",
            "text_start",
            "text_delta",
            "thinking_end",
            "text_end",
            "done"
        ]
    );
    let message = events
        .last()
        .and_then(AssistantMessageEvent::terminal_message)
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Length);
    match (&message.content[0], &message.content[1]) {
        (ContentBlock::Thinking(thinking), ContentBlock::Text(text)) => {
            assert_eq!(thinking.thinking, "想想");
            assert_eq!(text.text, "答案");
        }
        other => panic!("unexpected content {other:?}"),
    }
}

#[tokio::test]
async fn converts_tools_and_tool_results_in_the_request() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok("data: [DONE]\n\n")]);
    let provider = provider_with(transport.clone());
    let tool_result = tower_agent_provider::Message::ToolResult(ToolResultMessage {
        tool_call_id: "call_1".into(),
        tool_name: "get_weather".into(),
        content: vec![ContentBlock::Text(tower_agent_provider::TextContent {
            text: "晴".into(),
            text_signature: None,
        })],
        details: None,
        is_error: false,
        timestamp: 0,
    });
    let _ = provider
        .stream(request(
            openai_model(),
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
                user("先看一眼"),
                tool_result,
            ],
        ))
        .await
        .expect("established");

    let sent = transport.requests();
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).expect("json");
    assert_eq!(body["messages"][0]["role"], json!("system"));
    assert_eq!(body["messages"][0]["content"], json!("你是助手"));
    assert_eq!(body["messages"][2]["role"], json!("tool"));
    assert_eq!(body["messages"][2]["tool_call_id"], json!("call_1"));
    assert_eq!(body["messages"][2]["content"], json!("晴"));
    assert_eq!(body["tools"][0]["type"], json!("function"));
    assert_eq!(body["tools"][0]["function"]["name"], json!("get_weather"));
    assert_eq!(body["tool_choice"], json!("auto"));
}

#[tokio::test]
async fn http_error_becomes_an_error_event_and_5xx_is_retryable() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::text(
        401,
        r#"{"error":{"message":"invalid key"}}"#,
    )]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(openai_model(), vec![user("hi")]))
            .await
            .expect("4xx 编码进流"),
    )
    .await;
    assert_eq!(kinds(&events), vec!["error"]);

    let transport = ScriptedTransport::new(vec![ScriptedResponse::text(503, "unavailable")]);
    let provider = provider_with(transport);
    let error = provider
        .stream(request(openai_model(), vec![user("hi")]))
        .await
        .expect_err("5xx 交给 retry");
    assert!(matches!(error, PiError::Provider { .. }), "got: {error}");
}

#[tokio::test]
async fn missing_finish_reason_is_an_error() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(chunk(
        json!({ "content": "partial" }),
        json!(null),
    ))]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(openai_model(), vec![user("hi")]))
            .await
            .expect("established"),
    )
    .await;
    assert_eq!(
        kinds(&events),
        vec!["text_start", "text_delta", "text_end", "error"]
    );
    let AssistantMessageEvent::Error { error, .. } = events.last().unwrap() else {
        panic!("expected error");
    };
    assert!(
        error
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("finish reason")),
        "got: {:?}",
        error.error_message
    );
}
