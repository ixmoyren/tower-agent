//! Tool capability coverage for the JavaScript codemode engine.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures_util::StreamExt as _;
use tower_agent_codemode::{CodemodeHost, CodemodeOptions, Mount};
use tower_agent_codemode_core::{ScriptHookEvent, ScriptHookRegistry, ScriptPermissions};
use tower_agent_core::{AgentTool, ToolCallRequest};
use tower_agent_plugin_sdk::{
    CapabilityId, HostApi, Permission, PermissionDecision, PermissionPolicy, PermissionPrompter,
};
use tower_agent_plugin_tools::create_read_tool;
use tower_agent_provider::{Arguments, ContentBlock, ToolCall};

struct AllowOncePrompter {
    calls: AtomicUsize,
}

impl PermissionPrompter for AllowOncePrompter {
    fn decide(&self, _capability: &CapabilityId, _permission: &Permission) -> PermissionDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        PermissionDecision::AllowOnce
    }
}

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

    let prompter = Arc::new(AllowOncePrompter {
        calls: AtomicUsize::new(0),
    });
    codemode.set_permission_policy(PermissionPolicy::prompting(prompter.clone()));
    codemode
        .run_async("return await tool_list();", None)
        .await
        .expect("session authorization allows tool_list");
    assert_eq!(prompter.calls.load(Ordering::SeqCst), 1);

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

#[tokio::test]
async fn js_script_can_register_and_unregister_tool_hooks() {
    let session = tempfile::tempdir().expect("session");
    let registry = Arc::new(ScriptHookRegistry::new());
    let codemode = CodemodeHost::new_with_host(
        CodemodeOptions::embedded(session.path()),
        HostApi::from_tools(Vec::<AgentTool>::new()),
        ScriptPermissions::default(),
    )
    .expect("codemode host");
    codemode.set_hook_registry(Arc::clone(&registry));
    codemode
        .run_async(
            r#"
function guard(args) {
  return { seen: args.name, arguments: args.arguments };
}
on("tool_call", "guard");
"#,
            None,
        )
        .await
        .expect("register hook");
    assert_eq!(registry.len(), 1);

    let results = registry
        .invoke(
            ScriptHookEvent::ToolCall,
            serde_json::json!({ "name": "write", "arguments": { "path": "a" } }),
            None,
        )
        .await;
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].value.as_ref().expect("hook result"),
        &serde_json::json!({ "seen": "write", "arguments": { "path": "a" } })
    );

    drop(codemode);
    assert!(registry.is_empty(), "host unload must unregister JS hooks");
}

#[tokio::test]
async fn js_hook_registration_requires_permission() {
    let session = tempfile::tempdir().expect("session");
    let registry = Arc::new(ScriptHookRegistry::new());
    let codemode = CodemodeHost::new_with_host(
        CodemodeOptions::embedded(session.path()),
        HostApi::from_tools(Vec::<AgentTool>::new()),
        ScriptPermissions {
            hooks: false,
            ..ScriptPermissions::default()
        },
    )
    .expect("codemode host");
    codemode.set_hook_registry(registry);
    let error = codemode
        .run_async(
            r#"
function guard(args) { return args; }
on("tool_call", "guard");
"#,
            None,
        )
        .await
        .expect_err("hooks must be denied");
    let message = error.to_string();
    assert!(message.contains("unauthorized"), "{message}");
    assert!(message.contains("\"capability\":\"hooks\""), "{message}");
}
