//! Tool capability coverage for the JavaScript codemode engine.

use futures_util::StreamExt as _;
use tower_agent_codemode::{CodemodeHost, CodemodeOptions, Mount};
use tower_agent_codemode_core::ScriptPermissions;
use tower_agent_core::{AgentTool, ToolCallRequest};
use tower_agent_plugins::HostApi;
use tower_agent_provider::{Arguments, ContentBlock, ToolCall};
use tower_agent_tools::create_read_tool;

fn request(name: &str, arguments: serde_json::Value) -> ToolCallRequest {
    ToolCallRequest::new(ToolCall {
        id: "call".to_owned(),
        name: name.to_owned(),
        arguments: Arguments::from(arguments.as_object().cloned().unwrap_or_default()),
        thought_signature: None,
        namespace: None,
    })
}

async fn result_text(stream: tower_agent_core::ToolResultStream) -> String {
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
async fn js_tool_call_reads_a_host_tool() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("note.txt"),
        "hello from the host read tool",
    )
    .expect("write note");
    let host = HostApi::from_tools([create_read_tool(workspace.path())]);
    let session = tempfile::tempdir().expect("session");
    let options = CodemodeOptions::embedded(session.path())
        .mount(Mount::read_only(workspace.path(), "/work"));
    let codemode = CodemodeHost::new_with_host(options, host, ScriptPermissions::default())
        .expect("codemode host");

    let note = workspace.path().join("note.txt");
    let code = format!(
        "return await tool_call(\"read\", {});",
        serde_json::json!({ "path": note })
    );
    let result = codemode.run_async(&code, None).await.expect("run script");
    assert!(
        result
            .value
            .unwrap_or_default()
            .contains("hello from the host read tool"),
        "read result missing file content"
    );
}

#[tokio::test]
async fn js_registered_tool_is_available_in_the_next_round() {
    let session = tempfile::tempdir().expect("session");
    let codemode = CodemodeHost::new_with_host(
        CodemodeOptions::embedded(session.path()),
        HostApi::from_tools(Vec::<AgentTool>::new()),
        ScriptPermissions::default(),
    )
    .expect("codemode host");

    codemode
        .run_async(
            r#"
function greet(args) {
  return "hello " + args.name;
}
tool_register(
  "greet_js",
  "greet",
  "Greet by name",
  { type: "object", properties: { name: { type: "string" } }, required: ["name"] },
);
"#,
            None,
        )
        .await
        .expect("register tool");

    let tool = codemode
        .host_api()
        .tool("greet_js")
        .expect("registered tool in host table");
    let stream = tool
        .call(request("greet_js", serde_json::json!({ "name": "Ada" })))
        .await
        .expect("call registered tool");
    assert_eq!(result_text(stream).await, "hello Ada");
}

#[tokio::test]
async fn js_tool_permissions_are_enforced() {
    let session = tempfile::tempdir().expect("session");
    let codemode = CodemodeHost::new_with_host(
        CodemodeOptions::embedded(session.path()),
        HostApi::from_tools(Vec::<AgentTool>::new()),
        ScriptPermissions {
            tools: false,
            ..ScriptPermissions::default()
        },
    )
    .expect("codemode host");
    let error = codemode
        .run_async("return await tool_list();", None)
        .await
        .expect_err("tool_list must be denied");
    let message = error.to_string();
    assert!(message.contains("unauthorized"), "actual: {message}");
    assert!(
        message.contains("\"capability\":\"tools\""),
        "actual: {message}"
    );

    let session = tempfile::tempdir().expect("session");
    let codemode = CodemodeHost::new_with_host(
        CodemodeOptions::embedded(session.path()),
        HostApi::from_tools(Vec::<AgentTool>::new()),
        ScriptPermissions {
            register_tools: false,
            ..ScriptPermissions::default()
        },
    )
    .expect("codemode host");
    let error = codemode
        .run_async(
            r#"
function greet(args) { return args.name; }
tool_register("greet_js", "greet", "", { type: "object" });
"#,
            None,
        )
        .await
        .expect_err("tool_register must be denied");
    let message = error.to_string();
    assert!(message.contains("unauthorized"), "actual: {message}");
    assert!(
        message.contains("\"capability\":\"register_tools\""),
        "actual: {message}"
    );
}
