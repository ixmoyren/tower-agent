//! Capability matrix shared by the Python/monty and JavaScript/rquickjs engines.

#![cfg(all(feature = "codemode-python", feature = "codemode-js"))]

use futures_util::StreamExt as _;
use serde_json::Value;
use tower_agent::assembly::{HostPlugins, Mount, SessionRoot};
use tower_agent::core::{AgentTool, ToolCallRequest, ToolResultStream, UiState};
use tower_agent::monty::{ModOptions, ModPermissions};
use tower_agent::provider::{Arguments, ContentBlock, ToolCall};
use tower_agent_codemode::{CodemodeHost, CodemodeOptions};
use tower_agent_codemode_core::ScriptPermissions;
use tower_agent_plugins::HostApi;

const CAPABILITY_MATRIX: [&str; 7] = [
    "exec",
    "fs",
    "tool_call",
    "tool_register",
    "state",
    "log",
    "ui",
];

fn assert_matrix(engine: &str, covered: &[&str]) {
    assert_eq!(
        covered, CAPABILITY_MATRIX,
        "{engine} capability matrix differs"
    );
}

fn request(name: &str, arguments: Value) -> ToolCallRequest {
    ToolCallRequest::new(ToolCall {
        id: format!("parity:{name}"),
        name: name.to_owned(),
        arguments: Arguments::from(arguments.as_object().cloned().unwrap_or_default()),
        thought_signature: None,
        namespace: None,
    })
}

async fn call_text(tool: &AgentTool, name: &str, arguments: Value) -> String {
    let stream = tool
        .call(request(name, arguments))
        .await
        .expect("tool call");
    first_text(stream).await
}

async fn first_text(stream: ToolResultStream) -> String {
    let results: Vec<_> = stream.collect().await;
    results[0]
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn python_engine_capability_matrix() {
    let workspace = tempfile::tempdir().expect("workspace");
    let note = workspace.path().join("note.txt");
    std::fs::write(&note, "matrix-note\n").expect("write note");
    let ui = UiState::new();
    let manager = HostPlugins::new(workspace.path())
        .with_codemode(
            SessionRoot::Ephemeral,
            vec![Mount::read_only(workspace.path(), "/work")],
        )
        .mods(ui.clone())
        .expect("mods host");
    manager
        .mount_readonly("/work", workspace.path())
        .expect("mount workspace for monty");

    let code = format!(
        r#"
from pathlib import Path

tool_register("parity", "parity", "capability matrix", {{"type": "object"}})
ui_status("parity-status", "ready", "green", 1)
ui_panel("parity-panel", "Parity", ["ready"])

def parity(args):
    command = exec("cat", ["/work/note.txt"])["stdout"]
    filesystem = Path("/work/note.txt").read_text()
    host = tool_call("read", {{"path": "{}"}})
    state_set("parity", {{"n": 7}})
    state = state_get("parity")
    log("info", "parity")
    return {{"command": command, "filesystem": filesystem, "host": host, "state": state}}
"#,
        note.display()
    );
    manager
        .write_with_options(
            "parity",
            &code,
            ModOptions {
                permissions: Some(ModPermissions::default()),
                ..ModOptions::default()
            },
        )
        .await
        .expect("write parity mod");

    let tool = manager
        .tools()
        .into_iter()
        .find(|tool| tool.name() == "parity")
        .expect("registered parity tool");
    let text = call_text(&tool, "parity", serde_json::json!({})).await;
    let result: Value =
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{error}; text={text}"));
    let snapshot = ui.snapshot();

    let mut covered = Vec::new();
    if result["command"] == "matrix-note\n" {
        covered.push("exec");
    }
    if result["filesystem"] == "matrix-note\n" {
        covered.push("fs");
    }
    if result["host"]
        .as_str()
        .is_some_and(|value| value.contains("matrix-note"))
    {
        covered.push("tool_call");
    }
    covered.push("tool_register");
    if result["state"]["n"] == 7 {
        covered.push("state");
    }
    covered.push("log");
    if snapshot.panels.contains_key("parity-panel")
        && snapshot
            .status
            .iter()
            .any(|segment| segment.id == "parity-status")
    {
        covered.push("ui");
    }
    assert_matrix("python", &covered);
}

#[tokio::test]
async fn javascript_engine_capability_matrix() {
    let workspace = tempfile::tempdir().expect("workspace");
    let note = workspace.path().join("note.txt");
    std::fs::write(&note, "matrix-note\n").expect("write note");
    let host = HostApi::from_tools([tower_agent_tools::create_read_tool(workspace.path())]);
    let session = tempfile::tempdir().expect("session");
    let ui = UiState::new();
    let codemode = CodemodeHost::new_with_host_and_ui(
        CodemodeOptions::embedded(session.path())
            .mount(Mount::read_only(workspace.path(), "/work")),
        host,
        ScriptPermissions::default(),
        ui.clone(),
    )
    .expect("codemode host");

    let code = format!(
        r#"
const command = await cat("/work/note.txt");
const filesystem = fs.readFile("/work/note.txt");
const host = await tool_call("read", {{ path: {} }});
await state_set("parity", {{ n: 7 }});
const state = await state_get("parity");
log("info", "parity");
ui_status("parity-status", "ready", "green", 1);
ui_panel("parity-panel", "Parity", ["ready"]);
return {{ command, filesystem, host, state }};
"#,
        serde_json::json!(note)
    );
    let result = codemode.run_async(&code, None).await.expect("run matrix");
    let result: Value =
        serde_json::from_str(result.value.as_deref().expect("script result")).expect("result JSON");

    codemode
        .run_async(
            r#"
function parity() { return "registered"; }
tool_register("parity", "parity", "capability matrix", { type: "object" });
"#,
            None,
        )
        .await
        .expect("register parity tool");
    let tool = codemode
        .host_api()
        .tool("parity")
        .expect("registered parity tool");
    let registered = call_text(&tool, "parity", serde_json::json!({})).await;
    let snapshot = ui.snapshot();

    let mut covered = Vec::new();
    if result["command"] == "matrix-note\n" {
        covered.push("exec");
    }
    if result["filesystem"] == "matrix-note\n" {
        covered.push("fs");
    }
    if result["host"]
        .as_str()
        .is_some_and(|value| value.contains("matrix-note"))
    {
        covered.push("tool_call");
    }
    if registered == "registered" {
        covered.push("tool_register");
    }
    if result["state"]["n"] == 7 {
        covered.push("state");
    }
    covered.push("log");
    if snapshot.panels.contains_key("parity-panel")
        && snapshot
            .status
            .iter()
            .any(|segment| segment.id == "parity-status")
    {
        covered.push("ui");
    }
    assert_matrix("javascript", &covered);
}

#[tokio::test]
async fn javascript_fs_permission_is_enforced() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("note.txt"), "secret").expect("write note");
    let session = tempfile::tempdir().expect("session");
    let codemode = CodemodeHost::new_with_host(
        CodemodeOptions::embedded(session.path())
            .mount(Mount::read_only(workspace.path(), "/work")),
        HostApi::from_tools(Vec::<AgentTool>::new()),
        ScriptPermissions {
            fs: false,
            ..ScriptPermissions::default()
        },
    )
    .expect("codemode host");

    let error = codemode
        .run_async(r#"return fs.readFile("/work/note.txt");"#, None)
        .await
        .expect_err("fs read must be denied");
    let message = error.to_string();
    assert!(message.contains("unauthorized"), "actual: {message}");
    assert!(
        message.contains("\"capability\":\"fs\""),
        "actual: {message}"
    );
}
