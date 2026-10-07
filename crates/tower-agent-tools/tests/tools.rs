//! 内置工具的端到端测试：通过 `AgentTool` 的 Service 边界驱动，与实际 agent 循环一致。

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use base64::Engine as _;
use futures_util::{StreamExt as _, future::BoxFuture, stream};
use tokio::sync::Semaphore;
use tower::{Service, util::BoxCloneService};
use tower_agent_provider::{
    AssistantEventStream, AssistantMessage, AssistantMessageEvent, BoxStreamService, ContentBlock,
    DoneReason, JsonObject, JsonValue, Model, PiError, StopReason, StreamRequest, TextContent,
    ToolCall, Usage,
};
use tower_agent_runtime::{Agent, AgentEvent, AgentOptions, AgentTool, ToolCallRequest};

fn object(value: JsonValue) -> JsonObject {
    match value {
        JsonValue::Object(map) => map,
        other => panic!("expected object, got {other}"),
    }
}

fn request(name: &str, args: JsonValue) -> ToolCallRequest {
    ToolCallRequest::new(ToolCall {
        id: "call-1".into(),
        name: name.into(),
        arguments: object(args).into(),
        thought_signature: None,
        namespace: None,
    })
}

/// 内置工具集合，顺序与 `plugins` 的内置工具插件（`DEFAULT_TOOL_NAMES`）一致。
///
/// 注册与选择已迁移到插件体系，`tools` crate 只导出单个工具的实现；
/// 这里保留一份本地清单，专门测试这些实现本身。
fn default_tools(cwd: impl AsRef<std::path::Path>) -> Vec<AgentTool> {
    let cwd = cwd.as_ref();
    vec![
        tower_agent_tools::create_read_tool(cwd),
        tower_agent_tools::create_shell_tool(cwd),
        tower_agent_tools::create_edit_tool(cwd),
        tower_agent_tools::create_write_tool(cwd),
        tower_agent_tools::create_grep_tool(cwd),
        tower_agent_tools::create_find_tool(cwd),
        tower_agent_tools::create_ls_tool(cwd),
    ]
}

/// 内置工具名，顺序与 [`default_tools`] 一致。
const DEFAULT_TOOL_NAMES: [&str; 7] = ["read", "shell", "edit", "write", "grep", "find", "ls"];

/// 像 agent 循环那样调用一次工具，收集完整的部分结果 + 终态结果。
async fn run_tool(tool: &AgentTool, args: JsonValue) -> Vec<tower_agent_runtime::AgentToolResult> {
    let mut stream = tool
        .call(request(tool.name(), args))
        .await
        .expect("tool call establishes a stream");
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    items
}

fn first_text(result: &tower_agent_runtime::AgentToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn default_tools_expose_one_unified_shell_tool() {
    let dir = tempfile::tempdir().expect("temp dir");
    let tools = default_tools(dir.path());
    let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
    assert_eq!(
        names,
        vec!["read", "shell", "edit", "write", "grep", "find", "ls"],
        "bash 与 powershell 合并成一个 shell 工具"
    );
    assert_eq!(names.as_slice(), DEFAULT_TOOL_NAMES.as_slice());
}

#[tokio::test]
async fn ls_sorts_marks_directories_and_reports_limits() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir(dir.path().join("zeta")).unwrap();
    std::fs::create_dir(dir.path().join("Alpha")).unwrap();
    std::fs::write(dir.path().join("beta.txt"), "x").unwrap();
    std::fs::write(dir.path().join(".hidden"), "x").unwrap();
    std::fs::create_dir(dir.path().join("empty")).unwrap();
    let ls = tower_agent_tools::create_ls_tool(dir.path());

    let results = run_tool(&ls, serde_json::json!({})).await;
    let text = first_text(&results[0]);
    assert_eq!(
        text, ".hidden\nAlpha/\nbeta.txt\nempty/\nzeta/",
        "sorted case-insensitively, directories suffixed, dotfiles included"
    );

    let results = run_tool(&ls, serde_json::json!({ "path": "empty" })).await;
    assert_eq!(first_text(&results[0]), "(empty directory)");

    let results = run_tool(&ls, serde_json::json!({ "path": "beta.txt" })).await;
    assert!(results[0].is_error);
    assert!(first_text(&results[0]).starts_with("Not a directory:"));

    let results = run_tool(&ls, serde_json::json!({ "path": "nope" })).await;
    assert!(results[0].is_error);
    assert!(first_text(&results[0]).starts_with("Path not found:"));

    let results = run_tool(&ls, serde_json::json!({ "limit": 2 })).await;
    let text = first_text(&results[0]);
    assert!(
        text.contains("2 entries limit reached. Use limit=4 for more"),
        "got: {text}"
    );
}

#[tokio::test]
async fn find_matches_globs_and_respects_gitignore() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".gitignore"), "ignored.txt\nbuild/\n").unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("build")).unwrap();
    std::fs::write(dir.path().join("src/main.ts"), "x").unwrap();
    std::fs::write(dir.path().join("src/main.ts.snap"), "x").unwrap();
    std::fs::write(dir.path().join("kept.txt"), "x").unwrap();
    std::fs::write(dir.path().join("ignored.txt"), "x").unwrap();
    std::fs::write(dir.path().join("build/out.txt"), "x").unwrap();
    let find = tower_agent_tools::create_find_tool(dir.path());

    let results = run_tool(&find, serde_json::json!({ "pattern": "*.txt" })).await;
    let text = first_text(&results[0]);
    let mut found: Vec<&str> = text.lines().collect();
    found.sort_unstable();
    assert_eq!(found, vec!["kept.txt"], "ignored files are filtered");

    let results = run_tool(&find, serde_json::json!({ "pattern": "src/**/*.ts" })).await;
    let text = first_text(&results[0]);
    let mut found: Vec<&str> = text.lines().collect();
    found.sort_unstable();
    assert_eq!(found, vec!["src/main.ts"]);

    let results = run_tool(&find, serde_json::json!({ "pattern": "*.nope" })).await;
    assert_eq!(first_text(&results[0]), "No files found matching pattern");

    let results = run_tool(&find, serde_json::json!({ "pattern": "*.ts", "limit": 1 })).await;
    assert!(first_text(&results[0]).contains("1 results limit reached"));
}

#[tokio::test]
async fn grep_reports_matches_options_and_context() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("a.txt"), "Hello\nthere\nHELLO again\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "hello from rust\n").unwrap();
    let grep = tower_agent_tools::create_grep_tool(dir.path());

    let results = run_tool(&grep, serde_json::json!({ "pattern": "Hello" })).await;
    assert_eq!(first_text(&results[0]), "a.txt:1: Hello");

    let results = run_tool(
        &grep,
        serde_json::json!({ "pattern": "hello", "ignoreCase": true }),
    )
        .await;
    let text = first_text(&results[0]);
    assert!(text.contains("a.txt:1: Hello"), "got: {text}");
    assert!(text.contains("a.txt:3: HELLO again"), "got: {text}");
    assert!(text.contains("b.rs:1: hello from rust"), "got: {text}");

    let results = run_tool(
        &grep,
        serde_json::json!({ "pattern": "hello", "glob": "*.rs" }),
    )
        .await;
    assert_eq!(first_text(&results[0]), "b.rs:1: hello from rust");

    // 上下文行用 `-` 分隔，命中行用 `:` 分隔。
    let results = run_tool(
        &grep,
        serde_json::json!({ "pattern": "there", "context": 1 }),
    )
        .await;
    assert_eq!(
        first_text(&results[0]),
        "a.txt-1- Hello\na.txt:2: there\na.txt-3- HELLO again"
    );

    // literal 让正则元字符按字面匹配。
    std::fs::write(dir.path().join("lit.txt"), "a.b\naxb\n").unwrap();
    let results = run_tool(
        &grep,
        serde_json::json!({ "pattern": "a.b", "literal": true }),
    )
        .await;
    assert_eq!(first_text(&results[0]), "lit.txt:1: a.b");

    let results = run_tool(&grep, serde_json::json!({ "pattern": "zzz-not-here" })).await;
    assert_eq!(first_text(&results[0]), "No matches found");

    let results = run_tool(&grep, serde_json::json!({ "pattern": "(" })).await;
    assert!(results[0].is_error);
    assert!(first_text(&results[0]).starts_with("Invalid regex pattern:"));
}

#[tokio::test]
async fn grep_truncates_long_lines() {
    let dir = tempfile::tempdir().expect("temp dir");
    let long = "x".repeat(600);
    std::fs::write(dir.path().join("long.txt"), format!("{long}needle\n")).unwrap();
    let grep = tower_agent_tools::create_grep_tool(dir.path());

    let results = run_tool(&grep, serde_json::json!({ "pattern": "needle" })).await;
    let text = first_text(&results[0]);
    assert!(text.contains("... [truncated]"), "got: {text}");
    assert!(
        text.contains("Some lines truncated to 500 chars. Use read tool to see full lines"),
        "got: {text}"
    );
}

#[tokio::test]
async fn shell_tool_is_registered_with_pi_metadata() {
    let dir = tempfile::tempdir().expect("temp dir");
    let tools = default_tools(dir.path());
    let shell = tools.iter().find(|tool| tool.name() == "shell").unwrap();
    assert_eq!(shell.display_label(), "shell");
    // 描述文案点明底层 shell：非 Windows 是 bash，Windows 是 PowerShell。
    let description = shell.tool_description();
    assert!(description.starts_with("Execute a "), "got: {description}");
    assert!(description.contains("command"), "got: {description}");
    assert!(shell.declared_output_schema().is_some());
}

#[tokio::test]
async fn write_then_read_round_trips_and_creates_parents() {
    let dir = tempfile::tempdir().expect("temp dir");
    let tools = default_tools(dir.path());
    let write = tools
        .iter()
        .find(|tool| tool.name() == "write")
        .unwrap()
        .clone();
    let read = tools
        .iter()
        .find(|tool| tool.name() == "read")
        .unwrap()
        .clone();

    let results = run_tool(
        &write,
        serde_json::json!({ "path": "nested/hello.txt", "content": "hello\nworld\n" }),
    )
        .await;
    assert_eq!(results.len(), 1);
    assert_eq!(
        first_text(&results[0]),
        "Successfully wrote to nested/hello.txt"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("nested/hello.txt")).unwrap(),
        "hello\nworld\n"
    );

    let results = run_tool(&read, serde_json::json!({ "path": "nested/hello.txt" })).await;
    assert_eq!(first_text(&results[0]), "hello\nworld\n");
    assert!(!results[0].is_error);
}

#[tokio::test]
async fn read_attaches_images_with_base64_data() {
    let dir = tempfile::tempdir().expect("temp dir");
    // Minimum bytes that pi's PNG sniff accepts: signature + IHDR chunk, not animated.
    let mut png = Vec::new();
    png.extend_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    png.extend_from_slice(&[0, 0, 0, 13]);
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&[0u8; 13]);
    png.extend_from_slice(&[0u8; 4]);
    std::fs::write(dir.path().join("pixel.png"), &png).unwrap();
    let read = tower_agent_tools::create_read_tool(dir.path());

    let results = run_tool(&read, serde_json::json!({ "path": "pixel.png" })).await;
    let result = &results[0];
    assert!(!result.is_error);
    assert_eq!(first_text(result), "Read image file [image/png]");
    let image = result
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Image(image) => Some(image),
            _ => None,
        })
        .expect("an image content block");
    assert_eq!(image.mime_type, "image/png");
    // No circular check: every PNG starts with the well-known base64 prefix.
    assert!(image.data.starts_with("iVBORw0KGgo"), "got: {}", image.data);
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&image.data)
            .expect("payload is base64"),
        png
    );
}

#[tokio::test]
async fn read_reports_continuation_for_partial_reads() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("big.txt"),
        (1..=10).map(|i| format!("line {i}\n")).collect::<String>(),
    )
        .unwrap();
    let read = tower_agent_tools::create_read_tool(dir.path());

    let results = run_tool(
        &read,
        serde_json::json!({ "path": "big.txt", "offset": 2, "limit": 3 }),
    )
        .await;
    let text = first_text(&results[0]);
    assert!(text.starts_with("line 2\nline 3\nline 4"), "got: {text}");
    assert!(
        text.contains("7 more lines in file. Use offset=5 to continue."),
        "got: {text}"
    );

    let results = run_tool(&read, serde_json::json!({ "path": "missing.txt" })).await;
    assert!(results[0].is_error);
}

#[tokio::test]
async fn edit_replaces_text_and_reports_a_diff() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("code.rs"),
        "fn main() {\n    let x = 1;\n}\n",
    )
        .unwrap();
    let edit = tower_agent_tools::create_edit_tool(dir.path());

    let results = run_tool(
        &edit,
        serde_json::json!({
            "path": "code.rs",
            "edits": [{ "oldText": "let x = 1;", "newText": "let x = 42;" }]
        }),
    )
        .await;
    assert!(
        !results[0].is_error,
        "edit failed: {}",
        first_text(&results[0])
    );
    assert_eq!(
        first_text(&results[0]),
        "Successfully replaced 1 block(s) in code.rs."
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("code.rs")).unwrap(),
        "fn main() {\n    let x = 42;\n}\n"
    );
    let details = results[0].details.as_ref().expect("diff details");
    assert!(details.get("diff").is_some());
    assert!(details.get("patch").is_some());
}

#[tokio::test]
async fn edit_reports_a_not_found_error_without_touching_the_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("code.rs"), "unchanged\n").unwrap();
    let edit = tower_agent_tools::create_edit_tool(dir.path());

    let results = run_tool(
        &edit,
        serde_json::json!({
            "path": "code.rs",
            "edits": [{ "oldText": "nope", "newText": "yep" }]
        }),
    )
        .await;
    assert!(results[0].is_error);
    assert!(first_text(&results[0]).contains("Could not find the exact text in code.rs"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("code.rs")).unwrap(),
        "unchanged\n"
    );
}

#[tokio::test]
async fn shell_streams_output_and_reports_exit_codes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let shell = tower_agent_tools::create_shell_tool(dir.path());

    let results = run_tool(
        &shell,
        serde_json::json!({ "command": "printf 'hello\\n'" }),
    )
        .await;
    assert!(
        results.len() >= 2,
        "expected an initial update plus the final result"
    );
    let last = results.last().unwrap();
    assert!(!last.is_error);
    assert_eq!(first_text(last), "hello\n");

    let results = run_tool(&shell, serde_json::json!({ "command": "exit 7" })).await;
    let last = results.last().unwrap();
    assert!(last.is_error);
    assert!(
        first_text(last).contains("Command exited with code 7"),
        "got: {}",
        first_text(last)
    );
}

#[tokio::test]
async fn shell_reports_a_timeout_as_an_error_result() {
    let dir = tempfile::tempdir().expect("temp dir");
    let shell = tower_agent_tools::create_shell_tool(dir.path());

    let results = run_tool(
        &shell,
        serde_json::json!({ "command": "sleep 5", "timeout": 0.2 }),
    )
        .await;
    let last = results.last().unwrap();
    assert!(last.is_error);
    assert!(
        first_text(last).contains("Command timed out after 0.2 seconds"),
        "got: {}",
        first_text(last)
    );
}

/// 超时/取消时要杀掉**整个进程组**，而不只是直接子进程：
/// 这里让子进程 fork 一个后台写文件的孙进程，只有进程组被杀掉它才会停。
#[tokio::test]
async fn shell_timeout_kills_the_whole_process_tree() {
    let dir = tempfile::tempdir().expect("temp dir");
    let ticker = dir.path().join("ticker.txt");
    let shell = tower_agent_tools::create_shell_tool(dir.path());

    let command = format!(
        "bash -c 'while true; do echo tick >> {}; sleep 0.05; done' & sleep 30",
        ticker.display()
    );
    let results = run_tool(
        &shell,
        serde_json::json!({ "command": command, "timeout": 0.5 }),
    )
        .await;
    let last = results.last().expect("terminal result");
    assert!(last.is_error, "timeout is an error result");
    assert!(first_text(last).contains("Command timed out"));

    // 给后台进程一点时间暴露问题，再确认它已停止写入。
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let first = std::fs::read_to_string(&ticker).unwrap_or_default();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let second = std::fs::read_to_string(&ticker).unwrap_or_default();
    assert_eq!(
        first, second,
        "background writer must stop once the process group is killed"
    );
    assert!(
        !first.is_empty(),
        "the background writer did run before the kill"
    );
}

#[tokio::test]
async fn shell_truncates_large_output_and_reports_a_temp_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let shell = tower_agent_tools::create_shell_tool(dir.path());

    // 3000 lines exceeds the 2000-line display limit.
    let results = run_tool(&shell, serde_json::json!({ "command": "seq 1 3000" })).await;
    let last = results.last().unwrap();
    let text = first_text(last);
    assert!(
        text.contains("Showing lines 1001-3000 of 3000"),
        "got: {text}"
    );
    assert!(text.contains("Full output:"), "got: {text}");
    let structured = last
        .structured_content
        .as_ref()
        .expect("structured content");
    assert_eq!(structured["exit_code"], 0);
    assert!(structured["output"].as_str().unwrap().starts_with("1\n2\n"));
}

// ---- agent 集成：把内置工具注册进 Agent 并由第一次 provider 响应触发工具调用 ----

#[derive(Clone)]
struct ScriptedStreamService {
    scripts: Arc<Vec<Vec<AssistantMessageEvent>>>,
    calls: Arc<AtomicUsize>,
    gate: Option<Arc<Semaphore>>,
}

impl ScriptedStreamService {
    fn new(scripts: Vec<Vec<AssistantMessageEvent>>) -> Self {
        Self {
            scripts: Arc::new(scripts),
            calls: Arc::new(AtomicUsize::new(0)),
            gate: None,
        }
    }
}

impl Service<StreamRequest> for ScriptedStreamService {
    type Response = AssistantEventStream;
    type Error = PiError;
    type Future = BoxFuture<'static, Result<AssistantEventStream, PiError>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: StreamRequest) -> Self::Future {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let script = self
            .scripts
            .get(index)
            .or_else(|| self.scripts.last())
            .cloned()
            .unwrap_or_default();
        let gate = self.gate.clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                let _permit = gate.acquire().await;
            }
            Ok(AssistantEventStream::new(stream::iter(script)))
        })
    }
}

fn boxed_stream<S>(service: S) -> BoxStreamService
where
    S: Service<StreamRequest, Response = AssistantEventStream, Error = PiError>
    + Clone
    + Send
    + 'static,
    S::Future: Send + 'static,
{
    BoxCloneService::new(service)
}

fn test_model() -> Model {
    Model {
        id: "test-model".into(),
        name: "Test".into(),
        api: "test".into(),
        provider: "test".into(),
        ..Model::default()
    }
}

fn assistant(
    model: &Model,
    content: Vec<ContentBlock>,
    stop_reason: StopReason,
) -> AssistantMessage {
    AssistantMessage {
        content,
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        usage: Usage::default(),
        stop_reason,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }
}

fn tool_call_script(
    model: &Model,
    name: &str,
    arguments: JsonObject,
) -> Vec<AssistantMessageEvent> {
    let call = ContentBlock::ToolCall(ToolCall {
        id: "call-1".into(),
        name: name.into(),
        arguments: arguments.into(),
        thought_signature: None,
        namespace: None,
    });
    vec![
        AssistantMessageEvent::Start {
            partial: assistant(model, Vec::new(), StopReason::Pending),
        },
        AssistantMessageEvent::Done {
            reason: DoneReason::ToolUse,
            message: assistant(model, vec![call], StopReason::ToolUse),
        },
    ]
}

fn text_script(model: &Model, value: &str) -> Vec<AssistantMessageEvent> {
    vec![
        AssistantMessageEvent::Start {
            partial: assistant(model, Vec::new(), StopReason::Pending),
        },
        AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: assistant(
                model,
                vec![ContentBlock::Text(TextContent {
                    text: value.into(),
                    text_signature: None,
                })],
                StopReason::Stop,
            ),
        },
    ]
}

#[tokio::test]
async fn agent_loop_drives_the_write_tool() {
    let dir = tempfile::tempdir().expect("temp dir");
    let model = test_model();
    let service = ScriptedStreamService::new(vec![
        tool_call_script(
            &model,
            "write",
            object(serde_json::json!({ "path": "agent.txt", "content": "from the agent" })),
        ),
        text_script(&model, "done"),
    ]);
    let mut options = AgentOptions::new(boxed_stream(service));
    options.initial.model = Some(model);
    options.initial.tools = default_tools(dir.path());
    let agent = Agent::new(options);

    let mut subscription = agent.subscribe();
    let collector = tokio::spawn(async move {
        let mut tool_ends = Vec::new();
        while let Some(event) = subscription.recv().await {
            if let AgentEvent::ToolExecutionEnd {
                tool_name,
                is_error,
                ..
            } = &event
            {
                tool_ends.push((tool_name.clone(), *is_error));
            }
            if event.kind() == "agent_end" {
                break;
            }
        }
        tool_ends
    });

    agent
        .prompt_text("write a file")
        .await
        .expect("prompt succeeds");
    let tool_ends = collector.await.expect("collector joins");

    assert_eq!(tool_ends, vec![("write".to_string(), false)]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("agent.txt")).unwrap(),
        "from the agent"
    );
}
