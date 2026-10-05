//! 一致性（conformance）测试：provider 事件序列被固定为金标准。
//!
//! 序列对齐 pi faux provider（`packages/ai/src/providers/faux.ts`）与设计的行为
//! 清单中的事件协议规则。覆盖：普通文本流、thinking + text、以 `toolUse` 结束的
//! 工具调用、setup 失败，以及预先取消的请求。

mod support;

use std::collections::BTreeMap;

use ai::{
    AssistantMessageEvent, JsonObject, SimpleStreamOptions, StopReason, StreamOptions, ToolCall,
};
use support::{
    FauxProvider, FauxResponse, message, run, text_block, thinking_block, tool_call_block,
};
use tokio_util::sync::CancellationToken;

fn expect(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn describe(event: &AssistantMessageEvent) -> String {
    match event {
        AssistantMessageEvent::Start { .. } => "start".to_owned(),
        AssistantMessageEvent::TextStart { content_index, .. } => {
            format!("text_start#{content_index}")
        }
        AssistantMessageEvent::TextDelta {
            content_index,
            delta,
            ..
        } => format!("text_delta#{content_index}:{delta}"),
        AssistantMessageEvent::TextEnd { content_index, .. } => {
            format!("text_end#{content_index}")
        }
        AssistantMessageEvent::ThinkingStart { content_index, .. } => {
            format!("thinking_start#{content_index}")
        }
        AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
            ..
        } => format!("thinking_delta#{content_index}:{delta}"),
        AssistantMessageEvent::ThinkingEnd { content_index, .. } => {
            format!("thinking_end#{content_index}")
        }
        AssistantMessageEvent::ToolCallStart { content_index, .. } => {
            format!("toolcall_start#{content_index}")
        }
        AssistantMessageEvent::ToolCallDelta {
            content_index,
            delta,
            ..
        } => format!("toolcall_delta#{content_index}:{delta}"),
        AssistantMessageEvent::ToolCallEnd { content_index, .. } => {
            format!("toolcall_end#{content_index}")
        }
        AssistantMessageEvent::Done { reason, .. } => format!("done:{reason:?}"),
        AssistantMessageEvent::Error { reason, .. } => format!("error:{reason:?}"),
    }
}

fn describe_all(events: &[AssistantMessageEvent]) -> Vec<String> {
    events.iter().map(describe).collect()
}

/// 结构视图：连续的 delta 折叠为单个标记，
/// 因此比较时忽略（由 conformance 固定的）分块数量。
fn shape(events: &[AssistantMessageEvent]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for event in events {
        let descriptor = match event {
            AssistantMessageEvent::TextDelta { content_index, .. } => {
                format!("text_delta#{content_index}")
            }
            AssistantMessageEvent::ThinkingDelta { content_index, .. } => {
                format!("thinking_delta#{content_index}")
            }
            AssistantMessageEvent::ToolCallDelta { content_index, .. } => {
                format!("toolcall_delta#{content_index}")
            }
            other => describe(other),
        };
        if out.last() != Some(&descriptor) {
            out.push(descriptor);
        }
    }
    out
}

fn terminal_stop_reason(events: &[AssistantMessageEvent]) -> Option<StopReason> {
    events
        .last()
        .and_then(AssistantMessageEvent::terminal_message)
        .map(|message| message.stop_reason)
}

fn text_delta(event: &AssistantMessageEvent) -> Option<&str> {
    match event {
        AssistantMessageEvent::TextDelta { delta, .. } => Some(delta),
        _ => None,
    }
}

/// 事件协议清单中的共享不变量：恰有一个终止事件且位于最后；
/// 不带 `start` 的流只能是单个 error；块的 delta/end 不得早于其 `*_start`。
fn assert_stream_contract(events: &[AssistantMessageEvent]) {
    assert!(!events.is_empty(), "流至少要发出一个事件");

    let terminals: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.is_terminal())
        .map(|(index, _)| index)
        .collect();
    assert_eq!(terminals.len(), 1, "终止事件必须恰好一个：{terminals:?}");
    assert_eq!(terminals[0], events.len() - 1, "终止事件必须在最后");

    match events.first() {
        Some(AssistantMessageEvent::Start { .. }) => {}
        Some(AssistantMessageEvent::Error { .. }) => {
            assert_eq!(events.len(), 1, "不带 `start` 的流只能是一个终止 error");
        }
        other => panic!("流必须以 `start` 或终止 error 开始，实际为 {other:?}"),
    }

    let mut started: BTreeMap<usize, &'static str> = BTreeMap::new();
    for event in events {
        match event {
            AssistantMessageEvent::Start { .. }
            | AssistantMessageEvent::Done { .. }
            | AssistantMessageEvent::Error { .. } => {}
            AssistantMessageEvent::TextStart { content_index, .. } => {
                started.insert(*content_index, "text");
            }
            AssistantMessageEvent::ThinkingStart { content_index, .. } => {
                started.insert(*content_index, "thinking");
            }
            AssistantMessageEvent::ToolCallStart { content_index, .. } => {
                started.insert(*content_index, "toolcall");
            }
            AssistantMessageEvent::TextDelta { content_index, .. }
            | AssistantMessageEvent::TextEnd { content_index, .. } => {
                assert_eq!(
                    started.get(content_index),
                    Some(&"text"),
                    "索引 {content_index} 处的 text 事件早于 text_start"
                );
            }
            AssistantMessageEvent::ThinkingDelta { content_index, .. }
            | AssistantMessageEvent::ThinkingEnd { content_index, .. } => {
                assert_eq!(
                    started.get(content_index),
                    Some(&"thinking"),
                    "索引 {content_index} 处的 thinking 事件早于 thinking_start"
                );
            }
            AssistantMessageEvent::ToolCallDelta { content_index, .. }
            | AssistantMessageEvent::ToolCallEnd { content_index, .. } => {
                assert_eq!(
                    started.get(content_index),
                    Some(&"toolcall"),
                    "索引 {content_index} 处的 toolcall 事件早于 toolcall_start"
                );
            }
        }
    }
}

#[test]
fn text_stream_matches_exact_gold_standard() {
    let mut provider = FauxProvider::new([FauxResponse::message(message(
        vec![text_block("hello world!")],
        StopReason::Stop,
    ))]);
    let events = run(&mut provider, SimpleStreamOptions::default());
    assert_stream_contract(&events);

    assert_eq!(
        describe_all(&events),
        expect(&[
            "start",
            "text_start#0",
            "text_delta#0:hell",
            "text_delta#0:o wo",
            "text_delta#0:rld!",
            "text_end#0",
            "done:Stop",
        ])
    );

    let streamed: String = events.iter().filter_map(text_delta).collect();
    assert_eq!(streamed, "hello world!");
    assert_eq!(terminal_stop_reason(&events), Some(StopReason::Stop));
}

#[test]
fn thinking_then_text_matches_structural_gold_standard() {
    let mut provider = FauxProvider::new([FauxResponse::message(message(
        vec![thinking_block("think"), text_block("answer")],
        StopReason::Stop,
    ))]);
    let events = run(&mut provider, SimpleStreamOptions::default());
    assert_stream_contract(&events);

    assert_eq!(
        shape(&events),
        expect(&[
            "start",
            "thinking_start#0",
            "thinking_delta#0",
            "thinking_end#0",
            "text_start#1",
            "text_delta#1",
            "text_end#1",
            "done:Stop",
        ])
    );
    assert_eq!(terminal_stop_reason(&events), Some(StopReason::Stop));
}

#[test]
fn tool_call_stream_ends_with_tool_use() {
    let mut arguments = JsonObject::new();
    arguments.insert("city".to_owned(), serde_json::Value::from("Paris"));
    let original = ToolCall {
        id: "call-1".to_owned(),
        name: "lookup".to_owned(),
        arguments: arguments.into(),
        thought_signature: None,
        namespace: None,
    };

    let mut provider = FauxProvider::new([FauxResponse::message(message(
        vec![tool_call_block(
            "call-1",
            "lookup",
            original.arguments.clone(),
        )],
        StopReason::ToolUse,
    ))]);
    let events = run(&mut provider, SimpleStreamOptions::default());
    assert_stream_contract(&events);

    assert_eq!(
        shape(&events),
        expect(&[
            "start",
            "toolcall_start#0",
            "toolcall_delta#0",
            "toolcall_end#0",
            "done:ToolUse",
        ])
    );

    let ended = events
        .iter()
        .find_map(|event| match event {
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => Some(tool_call),
            _ => None,
        })
        .expect("toolcall_end 应携带解析后的调用");
    assert_eq!(ended, &original);
    assert_eq!(terminal_stop_reason(&events), Some(StopReason::ToolUse));
}

#[test]
fn setup_failure_emits_single_error_without_start() {
    let mut provider = FauxProvider::new([FauxResponse::setup_failure("boom")]);
    let events = run(&mut provider, SimpleStreamOptions::default());
    assert_stream_contract(&events);

    assert_eq!(describe_all(&events), expect(&["error:Error"]));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AssistantMessageEvent::Start { .. }))
    );
    let terminal = events[0].terminal_message().expect("error 终止该流");
    assert_eq!(terminal.stop_reason, StopReason::Error);
    assert_eq!(terminal.error_message.as_deref(), Some("boom"));
}

#[test]
fn pre_aborted_request_emits_aborted_error_without_start() {
    let token = CancellationToken::new();
    token.cancel();
    let options = SimpleStreamOptions {
        base: StreamOptions {
            signal: Some(token),
            ..StreamOptions::default()
        },
        ..SimpleStreamOptions::default()
    };

    let mut provider = FauxProvider::new([FauxResponse::message(message(
        vec![text_block("hello")],
        StopReason::Stop,
    ))]);
    let events = run(&mut provider, options);
    assert_stream_contract(&events);

    assert_eq!(describe_all(&events), expect(&["error:Aborted"]));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AssistantMessageEvent::Start { .. }))
    );
}

#[test]
fn stop_reason_wire_strings_match_pi() {
    for (reason, wire) in [
        (StopReason::Pending, "pending"),
        (StopReason::Stop, "stop"),
        (StopReason::Length, "length"),
        (StopReason::ToolUse, "toolUse"),
        (StopReason::Error, "error"),
        (StopReason::Aborted, "aborted"),
        (StopReason::Deferred, "deferred"),
    ] {
        assert_eq!(reason.as_str(), wire);
    }
}
