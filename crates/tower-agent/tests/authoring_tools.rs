//! Shared codemode authoring tools exposed to both script engines.

#![cfg(all(feature = "codemode-python", feature = "codemode-js"))]

use std::sync::Arc;

use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tower_agent::assembly::HostPlugins;
use tower_agent::core::{AgentTool, ToolCallRequest, ToolResultStream, UiState};
use tower_agent::monty::ModManager;
use tower_agent::provider::{Arguments, ContentBlock, ToolCall};
use tower_agent_codemode::{CodemodePlugin, SessionRoot};

fn request(name: &str, arguments: Value) -> ToolCallRequest {
    ToolCallRequest::new(ToolCall {
        id: format!("authoring:{name}"),
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

fn tool(manager: &ModManager, name: &str) -> AgentTool {
    manager
        .tools()
        .into_iter()
        .find(|tool| tool.name() == name)
        .unwrap_or_else(|| panic!("tool `{name}` not in catalog"))
}

#[tokio::test]
async fn python_and_javascript_can_call_the_same_authoring_tool() {
    let workspace = tempfile::tempdir().expect("workspace");
    let manager = HostPlugins::new(workspace.path())
        .mods(UiState::new())
        .expect("codemode Python host");
    manager
        .register_static_plugin(Arc::new(CodemodePlugin::new(
            workspace.path(),
            SessionRoot::Ephemeral,
        )))
        .expect("register JavaScript host");

    let python = "\
tool_register(\"python_author\", \"python_author\", \"\", {\"type\": \"object\"})

def python_author(args):
    return tool_call(\"codemode_list\", {})
";
    manager
        .write("python_author", python)
        .await
        .expect("write Python author script");
    let python_result =
        call_text(&tool(&manager, "python_author"), "python_author", json!({})).await;
    assert!(
        python_result.contains("python_author"),
        "Python authoring tool result: {python_result}"
    );

    let javascript = r#"
return await tool_call("codemode_list", {});
"#;
    let javascript_result = call_text(
        &tool(&manager, "codemode"),
        "codemode",
        json!({ "code": javascript }),
    )
    .await;
    assert!(
        javascript_result.contains("python_author"),
        "JavaScript authoring tool result: {javascript_result}"
    );
}
