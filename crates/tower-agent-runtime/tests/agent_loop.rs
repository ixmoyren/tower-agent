//! 低层 agent 循环的行为对齐测试（design.md 第 7 节清单 4、5、7）。

mod support;

use std::sync::Arc;

use serde_json::json;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tower_agent_runtime::{
    AgentEvent, AgentMessage, AgentTool, AgentToolResult, ToolExecutionMode,
    agent_loop::{
        AgentContext, BeforeToolCallResult, LoopConfig, TurnDecision, default_convert_to_llm,
        run_agent_loop,
    },
};

use support::{
    ScriptedStreamService, ScriptedTool, TestSink, agent_tool, args, boxed_stream, context,
    error_script, test_model, text, text_script, tool_call, user,
};

fn config(model: &tower_agent_provider::Model) -> LoopConfig {
    LoopConfig::new(model.clone(), Arc::new(default_convert_to_llm))
}

fn assistant_with(
    model: &tower_agent_provider::Model,
    content: Vec<tower_agent_provider::ContentBlock>,
    stop: tower_agent_provider::StopReason,
) -> Vec<tower_agent_provider::AssistantMessageEvent> {
    vec![
        tower_agent_provider::AssistantMessageEvent::Start {
            partial: support::assistant(
                model,
                Vec::new(),
                tower_agent_provider::StopReason::Pending,
            ),
        },
        tower_agent_provider::AssistantMessageEvent::Done {
            reason: match stop {
                tower_agent_provider::StopReason::ToolUse => {
                    tower_agent_provider::DoneReason::ToolUse
                }
                tower_agent_provider::StopReason::Length => {
                    tower_agent_provider::DoneReason::Length
                }
                _ => tower_agent_provider::DoneReason::Stop,
            },
            message: support::assistant(model, content, stop),
        },
    ]
}

#[tokio::test]
async fn text_turn_matches_the_pi_event_lifecycle() {
    let model = test_model();
    let stream = boxed_stream(ScriptedStreamService::new(vec![text_script(
        &model, "hello",
    )]));
    let sink = TestSink::new();
    let signal = CancellationToken::new();

    let new_messages = run_agent_loop(
        vec![user("hi")],
        context(Vec::new(), Vec::new()),
        config(&model),
        &sink,
        &signal,
        stream.clone(),
    )
    .await
    .expect("run succeeds");

    assert_eq!(
        sink.kinds(),
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_update",
            "message_update",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
    assert_eq!(new_messages.len(), 2, "prompt plus assistant message");
    assert!(matches!(
        new_messages.last(),
        Some(AgentMessage::Message(
            tower_agent_provider::Message::Assistant(_)
        ))
    ));
}

#[tokio::test]
async fn unknown_tool_produces_an_error_result_and_continues() {
    let model = test_model();
    let call = tool_call("call-1", "ghost", args(json!({})));
    let scripts = vec![
        assistant_with(
            &model,
            vec![call],
            tower_agent_provider::StopReason::ToolUse,
        ),
        text_script(&model, "recovered"),
    ];
    let stream = boxed_stream(ScriptedStreamService::new(scripts));
    let sink = TestSink::new();
    let signal = CancellationToken::new();

    run_agent_loop(
        vec![user("go")],
        context(Vec::new(), Vec::new()),
        config(&model),
        &sink,
        &signal,
        stream.clone(),
    )
    .await
    .expect("run succeeds");

    let ends: Vec<_> = sink
        .events()
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd {
                tool_name,
                is_error,
                result,
                ..
            } => Some((tool_name, is_error, result)),
            _ => None,
        })
        .collect();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].0, "ghost");
    assert!(ends[0].1, "unknown tool must be an error result");
    assert_eq!(
        match ends[0].2.content.first() {
            Some(tower_agent_provider::ContentBlock::Text(text)) => text.text.clone(),
            _ => String::new(),
        },
        "Tool ghost not found"
    );
}

#[tokio::test]
async fn parallel_tools_end_in_completion_order_and_persist_in_source_order() {
    let model = test_model();
    let gate = Arc::new(Semaphore::new(0));

    let call_a = tool_call("a", "alpha", args(json!({})));
    let call_b = tool_call("b", "beta", args(json!({})));
    let scripts = vec![
        assistant_with(
            &model,
            vec![call_a, call_b],
            tower_agent_provider::StopReason::ToolUse,
        ),
        text_script(&model, "done"),
    ];
    let stream = boxed_stream(ScriptedStreamService::new(scripts));

    // 只在观察到 beta 的 tool_execution_end 之后才放行 alpha 的闸门，
    // 从而把「完成顺序」固定为 beta → alpha。
    let release = {
        let gate = Arc::clone(&gate);
        Arc::new(move |event: &AgentEvent| {
            if let AgentEvent::ToolExecutionEnd { tool_name, .. } = event
                && tool_name == "beta"
            {
                gate.add_permits(1);
            }
        })
    };
    let sink = TestSink::new().with_hook(release);
    let signal = CancellationToken::new();

    let tools = vec![
        agent_tool(
            "alpha",
            ScriptedTool::single(AgentToolResult::text("A")).gated(Arc::clone(&gate)),
        ),
        agent_tool("beta", ScriptedTool::single(AgentToolResult::text("B"))),
    ];

    let events = run_agent_loop(
        vec![user("go")],
        context(Vec::new(), tools),
        config(&model),
        &sink,
        &signal,
        stream.clone(),
    )
    .await
    .expect("run succeeds");

    let end_order: Vec<String> = sink
        .events()
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd { tool_name, .. } => Some(tool_name),
            _ => None,
        })
        .collect();
    assert_eq!(
        end_order,
        vec!["beta", "alpha"],
        "ends follow completion order"
    );

    let turn_end = sink
        .events()
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::TurnEnd { tool_results, .. } => Some(tool_results),
            _ => None,
        })
        .expect("a turn end event");
    let persisted: Vec<String> = turn_end
        .iter()
        .map(|message| message.tool_name.clone())
        .collect();
    assert_eq!(
        persisted,
        vec!["alpha", "beta"],
        "results follow source order"
    );
    let _ = events;
}

#[tokio::test]
async fn length_truncated_messages_fail_every_tool_call() {
    let model = test_model();
    let call = tool_call("call-1", "alpha", args(json!({})));
    let scripts = vec![
        assistant_with(&model, vec![call], tower_agent_provider::StopReason::Length),
        text_script(&model, "done"),
    ];
    let stream = boxed_stream(ScriptedStreamService::new(scripts));
    let sink = TestSink::new();
    let signal = CancellationToken::new();

    let tool = ScriptedTool::single(AgentToolResult::text("should not run"));
    let registered = tool.clone();

    run_agent_loop(
        vec![user("go")],
        context(Vec::new(), vec![agent_tool("alpha", tool)]),
        config(&model),
        &sink,
        &signal,
        stream.clone(),
    )
    .await
    .expect("run succeeds");

    assert_eq!(
        registered.call_count(),
        0,
        "truncated calls are never executed"
    );
    let result = sink
        .events()
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::ToolExecutionEnd {
                result, is_error, ..
            } => Some((result, is_error)),
            _ => None,
        })
        .expect("tool execution end");
    assert!(result.1);
    let text = match result.0.content.first() {
        Some(tower_agent_provider::ContentBlock::Text(text)) => text.text.clone(),
        _ => String::new(),
    };
    assert!(text.contains("output token limit"), "got: {text}");
}

#[tokio::test]
async fn terminate_ends_the_batch_after_one_request() {
    let model = test_model();
    let call = tool_call("call-1", "alpha", args(json!({})));
    let stream = boxed_stream(ScriptedStreamService::new(vec![assistant_with(
        &model,
        vec![call],
        tower_agent_provider::StopReason::ToolUse,
    )]));
    let sink = TestSink::new();
    let signal = CancellationToken::new();

    let terminating = AgentToolResult {
        terminate: Some(true),
        ..AgentToolResult::text("stop")
    };

    run_agent_loop(
        vec![user("go")],
        context(
            Vec::new(),
            vec![agent_tool("alpha", ScriptedTool::single(terminating))],
        ),
        config(&model),
        &sink,
        &signal,
        stream.clone(),
    )
    .await
    .expect("run succeeds");

    assert_eq!(
        sink.kinds()
            .iter()
            .filter(|kind| **kind == "turn_start")
            .count(),
        1,
        "a terminating batch must not start another turn"
    );
    assert_eq!(sink.kinds().last(), Some(&"agent_end"));
}

#[tokio::test]
async fn before_tool_call_can_block_a_call() {
    let model = test_model();
    let call = tool_call("call-1", "alpha", args(json!({})));
    let scripts = vec![
        assistant_with(
            &model,
            vec![call],
            tower_agent_provider::StopReason::ToolUse,
        ),
        text_script(&model, "done"),
    ];
    let stream = boxed_stream(ScriptedStreamService::new(scripts));
    let sink = TestSink::new();
    let signal = CancellationToken::new();

    let tool = ScriptedTool::single(AgentToolResult::text("should not run"));
    let registered = tool.clone();

    let mut loop_config = config(&model);
    loop_config.before_tool_call = Some(Arc::new(|context| {
        assert_eq!(context.tool_call.name, "alpha");
        Box::pin(async {
            Some(BeforeToolCallResult {
                block: true,
                reason: Some("denied".into()),
                arguments: None,
                terminate: None,
            })
        })
    }));

    run_agent_loop(
        vec![user("go")],
        context(Vec::new(), vec![agent_tool("alpha", tool)]),
        loop_config,
        &sink,
        &signal,
        stream.clone(),
    )
    .await
    .expect("run succeeds");

    assert_eq!(registered.call_count(), 0);
    let end = sink
        .events()
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::ToolExecutionEnd {
                result, is_error, ..
            } => Some((result, is_error)),
            _ => None,
        })
        .expect("tool execution end");
    assert!(end.1);
    assert_eq!(
        match end.0.content.first() {
            Some(tower_agent_provider::ContentBlock::Text(text)) => text.text.clone(),
            _ => String::new(),
        },
        "denied"
    );
}

#[tokio::test]
async fn finish_turn_end_stops_without_another_request() {
    let model = test_model();
    let stream = boxed_stream(ScriptedStreamService::new(vec![
        text_script(&model, "first"),
        text_script(&model, "second"),
    ]));
    let sink = TestSink::new();
    let signal = CancellationToken::new();

    let mut loop_config = config(&model);
    loop_config.finish_turn = Some(Arc::new(|_turn| {
        Box::pin(async { Some(TurnDecision::End) })
    }));

    run_agent_loop(
        vec![user("go")],
        context(Vec::new(), Vec::new()),
        loop_config,
        &sink,
        &signal,
        stream.clone(),
    )
    .await
    .expect("run succeeds");

    assert_eq!(
        sink.kinds()
            .iter()
            .filter(|kind| **kind == "turn_end")
            .count(),
        1
    );
}

#[tokio::test]
async fn error_stream_stops_the_run_and_reports_the_message() {
    let model = test_model();
    let stream = boxed_stream(ScriptedStreamService::new(vec![error_script(
        &model, "boom",
    )]));
    let sink = TestSink::new();
    let signal = CancellationToken::new();

    run_agent_loop(
        vec![user("go")],
        context(Vec::new(), Vec::new()),
        config(&model),
        &sink,
        &signal,
        stream.clone(),
    )
    .await
    .expect("run succeeds");

    let last = sink.events().into_iter().last().expect("agent_end");
    let AgentEvent::AgentEnd { messages } = last else {
        panic!("last event must be agent_end");
    };
    let assistant = messages.last().expect("assistant message");
    match assistant {
        AgentMessage::Message(tower_agent_provider::Message::Assistant(assistant)) => {
            assert_eq!(
                assistant.stop_reason,
                tower_agent_provider::StopReason::Error
            );
            assert_eq!(assistant.error_message.as_deref(), Some("boom"));
        }
        other => panic!("unexpected message: {other:?}"),
    }
}

#[tokio::test]
async fn sequential_mode_runs_one_tool_at_a_time() {
    let model = test_model();
    let call_a = tool_call("a", "alpha", args(json!({})));
    let call_b = tool_call("b", "beta", args(json!({})));
    let scripts = vec![
        assistant_with(
            &model,
            vec![call_a, call_b],
            tower_agent_provider::StopReason::ToolUse,
        ),
        text_script(&model, "done"),
    ];
    let stream = boxed_stream(ScriptedStreamService::new(scripts));
    let sink = TestSink::new();
    let signal = CancellationToken::new();

    let tools = vec![
        agent_tool("alpha", ScriptedTool::single(AgentToolResult::text("A"))),
        agent_tool("beta", ScriptedTool::single(AgentToolResult::text("B")))
            .execution_mode(ToolExecutionMode::Sequential),
    ];

    run_agent_loop(
        vec![user("go")],
        context(Vec::new(), tools),
        config(&model),
        &sink,
        &signal,
        stream.clone(),
    )
    .await
    .expect("run succeeds");

    let order: Vec<String> = sink
        .events()
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd { tool_name, .. } => Some(tool_name),
            _ => None,
        })
        .collect();
    assert_eq!(order, vec!["alpha", "beta"]);
}

#[tokio::test]
async fn steering_messages_are_injected_after_the_turn() {
    let model = test_model();
    let stream = boxed_stream(ScriptedStreamService::new(vec![
        text_script(&model, "first"),
        text_script(&model, "second"),
    ]));
    let sink = TestSink::new();
    let signal = CancellationToken::new();

    // 初始 poll 为空；第一次 turn 结束后的 poll 返回 steer 消息，触发第二个 turn。
    let polls: Arc<std::sync::atomic::AtomicUsize> =
        Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut loop_config = config(&model);
    loop_config.get_steering_messages = Some(Arc::new({
        let polls = Arc::clone(&polls);
        move || {
            let polls = Arc::clone(&polls);
            Box::pin(async move {
                if polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                    vec![user("steer")]
                } else {
                    Vec::new()
                }
            })
        }
    }));

    run_agent_loop(
        vec![user("go")],
        context(Vec::new(), Vec::new()),
        loop_config,
        &sink,
        &signal,
        stream.clone(),
    )
    .await
    .expect("run succeeds");

    assert_eq!(
        sink.kinds()
            .iter()
            .filter(|kind| **kind == "turn_start")
            .count(),
        2,
        "steering message triggers a second turn"
    );
}

/// 保证未使用导入（如 AgentContext 构造助手）仍被编译期检查。
#[test]
fn context_builder_is_available() {
    let _: AgentContext = context(
        vec![user("x")],
        vec![AgentTool::new(
            "t",
            ScriptedTool::single(AgentToolResult::text("x")),
        )],
    );
    assert_eq!(text("a"), text("a"));
    let _: AgentTool = agent_tool("t", ScriptedTool::single(AgentToolResult::text("x")));
}

#[tokio::test]
async fn loop_future_is_send() {
    let model = test_model();
    let stream = boxed_stream(ScriptedStreamService::new(vec![text_script(&model, "x")]));
    let sink = TestSink::new();
    let signal = CancellationToken::new();
    let loop_config = config(&model);
    tokio::spawn(async move {
        run_agent_loop(
            vec![user("hi")],
            context(Vec::new(), Vec::new()),
            loop_config,
            &sink,
            &signal,
            stream,
        )
        .await
    })
    .await
    .expect("joins")
    .expect("run succeeds");
}
