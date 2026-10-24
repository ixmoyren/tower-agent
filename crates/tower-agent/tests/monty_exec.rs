//! End-to-end coverage for monty scripts using the shared exec capability.

#![cfg(all(feature = "monty", feature = "rquickjs"))]

use futures_util::StreamExt as _;
use tower::ServiceExt as _;
use tower_agent::assembly::{HostPlugins, Mount, SessionRoot};
use tower_agent::core::{ToolCallRequest, ToolResultStream, UiState};
use tower_agent::monty::{ModManager, ModOptions, ModPermissions};
use tower_agent::provider::ToolCall;

async fn first_text(stream: ToolResultStream) -> String {
    let results: Vec<_> = stream.collect().await;
    results[0]
        .content
        .iter()
        .filter_map(|block| match block {
            tower_agent::provider::ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}

async fn call_tool(manager: &ModManager, name: &str) -> String {
    let tool = manager
        .tools()
        .into_iter()
        .find(|tool| tool.name() == name)
        .unwrap_or_else(|| panic!("tool `{name}` not in catalog"));
    let request = ToolCallRequest::new(ToolCall {
        id: "call".to_owned(),
        name: name.to_owned(),
        arguments: Default::default(),
        thought_signature: None,
        namespace: None,
    });
    let stream = tool
        .service()
        .clone()
        .oneshot(request)
        .await
        .expect("tool call");
    first_text(stream).await
}

#[tokio::test]
async fn monty_exec_runs_coreutils_and_enforces_permission() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("note.txt"), "hello from coreutils\n")
        .expect("write note");

    let manager = HostPlugins::new(workspace.path())
        .with_codemode(
            SessionRoot::Ephemeral,
            vec![Mount::read_only(workspace.path(), "/work")],
        )
        .mods(UiState::new())
        .expect("mods host");

    let code = "\
tool_register(\"read_note\", \"read_note\", \"\", {\"type\": \"object\"})
tool_register(\"count_note\", \"count_note\", \"\", {\"type\": \"object\"})

def read_note(args):
    return exec(\"cat\", [\"/work/note.txt\"])[\"stdout\"]

def count_note(args):
    return exec(\"wc\", [\"-l\", \"/work/note.txt\"])[\"stdout\"]
";
    manager.write("exec_reader", code).await.expect("write mod");
    assert_eq!(
        call_tool(&manager, "read_note").await,
        "hello from coreutils\n"
    );
    let count = call_tool(&manager, "count_note").await;
    assert!(count.contains('1'), "wc output: {count}");

    let blocked = "\
tool_register(\"blocked_exec\", \"blocked_exec\", \"\", {\"type\": \"object\"})

def blocked_exec(args):
    return exec(\"cat\", [\"/work/note.txt\"])
";
    manager
        .write_with_options(
            "exec_locked",
            blocked,
            ModOptions {
                permissions: Some(ModPermissions {
                    exec: false,
                    ..ModPermissions::default()
                }),
                ..ModOptions::default()
            },
        )
        .await
        .expect("write locked mod");
    let denied = call_tool(&manager, "blocked_exec").await;
    assert!(denied.contains("unauthorized"), "actual: {denied}");
    assert!(
        denied.contains("\"capability\":\"exec\""),
        "actual: {denied}"
    );
}
