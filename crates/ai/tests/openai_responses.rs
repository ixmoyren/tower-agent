//! OpenAI Responses provider 的端到端测试（脚本化传输层，无网络）。

#[path = "provider_support/mod.rs"]
mod support;

use ai::{
    AssistantMessageEvent, ContentBlock, OpenAiResponsesProvider, PiError, StopReason, Tool,
    ToolResultMessage,
};
use serde_json::json;
use support::{
    ScriptedResponse, ScriptedTransport, collect, kinds, request, system, test_model, user,
};

fn responses_model() -> ai::Model {
    ai::Model {
        api: ai::OPENAI_RESPONSES_API.into(),
        provider: "openai".into(),
        id: "gpt-5".into(),
        ..test_model()
    }
}

fn provider_with(transport: ScriptedTransport) -> OpenAiResponsesProvider<ScriptedTransport> {
    OpenAiResponsesProvider::new(transport).with_api_key("sk-openai")
}

/// 类型化事件：`event: <type>` + `data: {...}`（Responses 的真实线上形状）。
fn event(payload: serde_json::Value) -> String {
    let kind = payload
        .get("type")
        .and_then(|value| value.as_str())
        .expect("type");
    format!("event: {kind}\ndata: {payload}\n\n")
}

fn completed(response: serde_json::Value) -> String {
    event(json!({ "type": "response.completed", "response": response }))
}

#[tokio::test]
async fn maps_typed_text_events_to_gold_standard_events() {
    let mut sse = String::new();
    sse.push_str(&event(
        json!({ "type": "response.created", "response": { "id": "resp_1" } }),
    ));
    sse.push_str(&event(json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": { "type": "message", "id": "msg_1" },
    })));
    sse.push_str(&event(json!({
        "type": "response.output_text.delta",
        "output_index": 0,
        "delta": "Hello",
    })));
    sse.push_str(&event(json!({
        "type": "response.output_text.delta",
        "output_index": 0,
        "delta": " 世界",
    })));
    sse.push_str(&event(json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "message",
            "id": "msg_1",
            "content": [{ "type": "output_text", "text": "Hello 世界" }],
        },
    })));
    sse.push_str(&completed(json!({
        "id": "resp_1",
        "status": "completed",
        "usage": {
            "input_tokens": 12,
            "output_tokens": 5,
            "total_tokens": 17,
            "input_tokens_details": { "cached_tokens": 4 },
            "output_tokens_details": { "reasoning_tokens": 2 },
        },
    })));

    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport.clone());
    let events = collect(
        provider
            .stream(request(responses_model(), vec![user("hi")]))
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
    assert_eq!(message.response_id.as_deref(), Some("resp_1"));
    match &message.content[0] {
        ContentBlock::Text(text) => assert_eq!(text.text, "Hello 世界"),
        other => panic!("expected text, got {other:?}"),
    }
    assert_eq!(message.usage.input, 8);
    assert_eq!(message.usage.output, 5);
    assert_eq!(message.usage.cache_read, 4);
    assert_eq!(message.usage.reasoning, Some(2));
    assert_eq!(message.usage.total_tokens, 17);

    // 请求形状：/responses、bearer、store:false、input 是类型化 item。
    let sent = transport.requests();
    assert_eq!(sent[0].url, "https://api.openai.com/v1/responses");
    assert_eq!(
        sent[0].headers.get("authorization").map(String::as_str),
        Some("Bearer sk-openai")
    );
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).expect("json");
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["input"][0]["type"], json!("message"));
    assert_eq!(body["input"][0]["content"][0]["type"], json!("input_text"));
    assert_eq!(body["input"][0]["content"][0]["text"], json!("hi"));
}

#[tokio::test]
async fn merges_function_call_deltas_by_output_index() {
    let mut sse = String::new();
    sse.push_str(&event(json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": { "type": "function_call", "call_id": "call_1", "name": "get_weather" },
    })));
    sse.push_str(&event(json!({
        "type": "response.function_call_arguments.delta",
        "output_index": 0,
        "delta": "{\"city\"",
    })));
    sse.push_str(&event(json!({
        "type": "response.function_call_arguments.delta",
        "output_index": 0,
        "delta": ":\"SF\"}",
    })));
    sse.push_str(&event(json!({
        "type": "response.function_call_arguments.done",
        "output_index": 0,
        "arguments": "{\"city\":\"SF\"}",
    })));
    sse.push_str(&event(json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "function_call",
            "call_id": "call_1",
            "name": "get_weather",
            "arguments": "{\"city\":\"SF\"}",
        },
    })));
    sse.push_str(&completed(json!({ "id": "resp_2", "status": "completed" })));

    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(responses_model(), vec![user("天气")]))
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
    assert_eq!(tool_call.id, "call_1");
    assert_eq!(tool_call.name, "get_weather");
    assert_eq!(tool_call.arguments.get("city"), Some(&json!("SF")));
    let message = events
        .last()
        .and_then(AssistantMessageEvent::terminal_message)
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn maps_reasoning_summary_to_thinking() {
    let mut sse = String::new();
    sse.push_str(&event(json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": { "type": "reasoning", "id": "rs_1" },
    })));
    sse.push_str(&event(json!({
        "type": "response.reasoning_summary_text.delta",
        "output_index": 0,
        "delta": "想想",
    })));
    sse.push_str(&event(json!({
        "type": "response.reasoning_summary_part.done",
        "output_index": 0,
    })));
    sse.push_str(&event(json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": { "type": "reasoning", "id": "rs_1", "summary": [{ "text": "想想" }] },
    })));
    sse.push_str(&event(json!({
        "type": "response.output_item.added",
        "output_index": 1,
        "item": { "type": "message", "id": "msg_1" },
    })));
    sse.push_str(&event(json!({
        "type": "response.output_text.delta",
        "output_index": 1,
        "delta": "答案",
    })));
    sse.push_str(&event(json!({
        "type": "response.output_item.done",
        "output_index": 1,
        "item": { "type": "message", "id": "msg_1", "content": [{ "type": "output_text", "text": "答案" }] },
    })));
    sse.push_str(&completed(json!({ "id": "resp_3", "status": "completed" })));

    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(responses_model(), vec![user("想一下")]))
            .await
            .expect("established"),
    )
    .await;

    assert_eq!(
        kinds(&events),
        vec![
            "thinking_start",
            "thinking_delta",
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
    match (&message.content[0], &message.content[1]) {
        (ContentBlock::Thinking(thinking), ContentBlock::Text(text)) => {
            // `reasoning_summary_part.done` 会追加 "\n\n"，但随后的 `output_item.done`
            // 用终态 item 的 summary 覆盖回来（pi 同样如此）。
            assert_eq!(thinking.thinking, "想想");
            assert_eq!(text.text, "答案");
        }
        other => panic!("unexpected content {other:?}"),
    }
}

#[tokio::test]
async fn converts_tools_and_function_outputs_in_the_request() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(completed(
        json!({ "id": "resp_4", "status": "completed" }),
    ))]);
    let provider = provider_with(transport.clone());
    let tool_result = ai::Message::ToolResult(ToolResultMessage {
        tool_call_id: "call_1".into(),
        tool_name: "get_weather".into(),
        content: vec![ContentBlock::Text(ai::TextContent {
            text: "晴".into(),
            text_signature: None,
        })],
        details: None,
        is_error: false,
        timestamp: 0,
    });
    let _ = provider
        .stream(request(
            responses_model(),
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
    let input = body["input"].as_array().expect("input");
    assert_eq!(input[0]["role"], json!("system"));
    assert_eq!(input[0]["content"][0]["type"], json!("input_text"));
    assert_eq!(input[0]["content"][0]["text"], json!("你是助手"));
    let output_item = input
        .iter()
        .find(|item| item["type"] == json!("function_call_output"))
        .expect("function_call_output item");
    assert_eq!(output_item["call_id"], json!("call_1"));
    assert_eq!(output_item["output"], json!("晴"));
    assert_eq!(body["tools"][0]["type"], json!("function"));
    assert_eq!(body["tools"][0]["name"], json!("get_weather"));
    assert_eq!(body["tool_choice"], json!("auto"));
}

#[tokio::test]
async fn failed_and_incomplete_events_become_errors() {
    // response.failed 带错误细节。
    let sse = event(json!({
        "type": "response.failed",
        "response": { "status": "failed", "error": { "code": "server_error", "message": "boom" } },
    }));
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(responses_model(), vec![user("hi")]))
            .await
            .expect("established"),
    )
    .await;
    assert_eq!(kinds(&events), vec!["error"]);
    let AssistantMessageEvent::Error { error, .. } = &events[0] else {
        panic!("expected error");
    };
    assert_eq!(error.error_message.as_deref(), Some("server_error: boom"));

    // incomplete + max_output_tokens → length（不是错误）。
    let sse = completed(json!({
        "id": "resp_5",
        "status": "incomplete",
        "incomplete_details": { "reason": "max_output_tokens" },
    }))
    .replace("response.completed", "response.incomplete");
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(responses_model(), vec![user("hi")]))
            .await
            .expect("established"),
    )
    .await;
    let message = events
        .last()
        .and_then(AssistantMessageEvent::terminal_message)
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::Length);
}

#[tokio::test]
async fn errors_follow_the_same_contract() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::text(
        401,
        r#"{"error":{"message":"invalid key"}}"#,
    )]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(responses_model(), vec![user("hi")]))
            .await
            .expect("4xx 编码进流"),
    )
    .await;
    assert_eq!(kinds(&events), vec!["error"]);

    let transport = ScriptedTransport::new(vec![ScriptedResponse::text(500, "boom")]);
    let provider = provider_with(transport);
    let error = provider
        .stream(request(responses_model(), vec![user("hi")]))
        .await
        .expect_err("5xx 交给 retry");
    assert!(matches!(error, PiError::Provider { .. }), "got: {error}");

    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok("")]);
    let provider = OpenAiResponsesProvider::new(transport.clone());
    assert!(matches!(
        provider
            .stream(request(responses_model(), vec![user("hi")]))
            .await
            .expect_err("需要 key"),
        PiError::MissingCredentials { .. }
    ));
    assert_eq!(transport.request_count(), 0);
}
