//! Google Generative AI provider 的端到端测试（脚本化传输层，无网络）。

#[path = "provider_support/mod.rs"]
mod support;

use serde_json::json;
use support::{
    ScriptedResponse, ScriptedTransport, collect, kinds, request, system, test_model, user,
};
use tower_agent_provider::{
    AssistantMessageEvent, ContentBlock, GoogleProvider, PiError, StopReason, Tool,
    ToolResultMessage,
};

fn google_model() -> tower_agent_provider::Model {
    tower_agent_provider::Model {
        api: tower_agent_provider::GOOGLE_GENERATIVE_AI_API.into(),
        provider: "google".into(),
        id: "gemini-test".into(),
        ..test_model()
    }
}

fn provider_with(transport: ScriptedTransport) -> GoogleProvider<ScriptedTransport> {
    GoogleProvider::new(transport).with_api_key("goog-key")
}

/// 一个 Gemini 流式 chunk。
fn chunk(parts: serde_json::Value, finish: serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "candidates": [{
                "content": { "role": "model", "parts": parts },
                "finishReason": finish,
            }]
        })
    )
}

#[tokio::test]
async fn maps_a_parts_stream_to_gold_standard_events() {
    let mut sse = String::new();
    sse.push_str(&chunk(json!([{ "text": "Hello" }]), json!(null)));
    sse.push_str(&chunk(json!([{ "text": " 世界" }]), json!(null)));
    sse.push_str(&chunk(json!([]), json!("STOP")));
    sse.push_str(&format!(
        "data: {}\n\n",
        json!({
            "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP" }],
            "usageMetadata": {
                "promptTokenCount": 12,
                "candidatesTokenCount": 5,
                "thoughtsTokenCount": 2,
                "cachedContentTokenCount": 4,
                "totalTokenCount": 19,
            }
        })
    ));

    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport.clone());
    let events = collect(
        provider
            .stream(request(google_model(), vec![user("hi")]))
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
    // input = prompt - cached；output = candidates + thoughts。
    assert_eq!(message.usage.input, 8);
    assert_eq!(message.usage.output, 7);
    assert_eq!(message.usage.cache_read, 4);
    assert_eq!(message.usage.reasoning, Some(2));
    assert_eq!(message.usage.total_tokens, 19);

    // 请求形状：模型在路径里，认证在 x-goog-api-key。
    let sent = transport.requests();
    assert_eq!(
        sent[0].url,
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-test:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        sent[0].headers.get("x-goog-api-key").map(String::as_str),
        Some("goog-key")
    );
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).expect("json");
    assert_eq!(body["contents"][0]["role"], json!("user"));
    assert_eq!(body["contents"][0]["parts"][0]["text"], json!("hi"));
    assert_eq!(body["generationConfig"]["maxOutputTokens"], json!(4096));
}

#[tokio::test]
async fn maps_thought_parts_to_thinking_blocks() {
    let mut sse = String::new();
    sse.push_str(&chunk(
        json!([{ "text": "想想", "thought": true }]),
        json!(null),
    ));
    sse.push_str(&chunk(json!([{ "text": "答案" }]), json!(null)));
    sse.push_str(&chunk(json!([]), json!("MAX_TOKENS")));

    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(google_model(), vec![user("想一下")]))
            .await
            .expect("established"),
    )
        .await;

    // 类型切换时先闭合旧块再开新块（与 pi 一致）。
    assert_eq!(
        kinds(&events),
        vec![
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
async fn function_calls_arrive_whole_and_force_tool_use() {
    let mut sse = String::new();
    sse.push_str(&chunk(json!([{ "text": "我查一下" }]), json!(null)));
    sse.push_str(&chunk(
        json!([{ "functionCall": { "name": "get_weather", "args": { "city": "SF" } } }]),
        json!(null),
    ));
    // Gemini 有工具调用时也可能报 STOP：应当被纠正成 toolUse。
    sse.push_str(&chunk(json!([]), json!("STOP")));

    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(sse)]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(google_model(), vec![user("天气")]))
            .await
            .expect("established"),
    )
        .await;

    assert_eq!(
        kinds(&events),
        vec![
            "text_start",
            "text_delta",
            "text_end",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ]
    );
    let AssistantMessageEvent::ToolCallEnd { tool_call, .. } = &events[5] else {
        panic!("expected toolcall_end");
    };
    assert_eq!(tool_call.name, "get_weather");
    assert_eq!(tool_call.arguments.get("city"), Some(&json!("SF")));
    assert!(!tool_call.id.is_empty(), "缺 id 时由我们生成");
    let message = events
        .last()
        .and_then(AssistantMessageEvent::terminal_message)
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn converts_system_tools_and_function_responses() {
    let transport =
        ScriptedTransport::new(vec![ScriptedResponse::ok(chunk(json!([]), json!("STOP")))]);
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
            google_model(),
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
    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        json!("你是助手")
    );
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["name"],
        json!("get_weather")
    );
    assert_eq!(
        body["toolConfig"]["functionCallingConfig"]["mode"],
        json!("AUTO")
    );
    // 工具结果：role: user + functionResponse（与上一条 user 消息合并成多个 parts）。
    let contents = body["contents"].as_array().expect("contents");
    let last = contents.last().expect("last content");
    assert_eq!(last["role"], json!("user"));
    let function_response = last["parts"]
        .as_array()
        .expect("parts")
        .iter()
        .find(|part| part.get("functionResponse").is_some())
        .expect("functionResponse part");
    assert_eq!(
        function_response["functionResponse"]["name"],
        json!("get_weather")
    );
    assert_eq!(
        function_response["functionResponse"]["response"]["content"],
        json!("晴")
    );
}

#[tokio::test]
async fn enables_thinking_config_when_requested() {
    let transport =
        ScriptedTransport::new(vec![ScriptedResponse::ok(chunk(json!([]), json!("STOP")))]);
    let provider = provider_with(transport.clone());
    let mut stream_request = request(google_model(), vec![user("想一下")]);
    stream_request.options.reasoning = Some(tower_agent_provider::ThinkingLevel::High);
    let _ = provider.stream(stream_request).await.expect("established");
    let sent = transport.requests();
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).expect("json");
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["includeThoughts"],
        json!(true)
    );
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        json!("HIGH")
    );
}

#[tokio::test]
async fn errors_follow_the_same_contract() {
    // 4xx 编码进流。
    let transport = ScriptedTransport::new(vec![ScriptedResponse::text(
        400,
        r#"{"error":{"message":"API key not valid"}}"#,
    )]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(google_model(), vec![user("hi")]))
            .await
            .expect("4xx 编码进流"),
    )
        .await;
    assert_eq!(kinds(&events), vec!["error"]);

    // 5xx 交给 retry 层。
    let transport = ScriptedTransport::new(vec![ScriptedResponse::text(503, "overloaded")]);
    let provider = provider_with(transport);
    let error = provider
        .stream(request(google_model(), vec![user("hi")]))
        .await
        .expect_err("5xx 交给 retry");
    assert!(matches!(error, PiError::Provider { .. }), "got: {error}");

    // 缺凭据是 setup 错误。
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok("")]);
    let provider = GoogleProvider::new(transport.clone());
    assert!(matches!(
        provider
            .stream(request(google_model(), vec![user("hi")]))
            .await
            .expect_err("需要 key"),
        PiError::MissingCredentials { .. }
    ));
    assert_eq!(transport.request_count(), 0);
}

#[tokio::test]
async fn safety_finish_reasons_become_errors() {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok(chunk(
        json!([{ "text": "partial" }]),
        json!("SAFETY"),
    ))]);
    let provider = provider_with(transport);
    let events = collect(
        provider
            .stream(request(google_model(), vec![user("hi")]))
            .await
            .expect("established"),
    )
        .await;
    assert_eq!(
        kinds(&events),
        vec!["text_start", "text_delta", "text_end", "error"]
    );
}
