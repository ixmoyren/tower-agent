//! End-to-end smoke test for the separately built shell plugin.
//!
//! Set `TPA_SHELL_PLUGIN` to the built dynamic library path:
//!
//! ```text
//! cargo build -p tower-agent-plugin-shell
//! TPA_SHELL_PLUGIN=target/debug/libtower_agent_plugin_shell.so \
//!   cargo test -p tower-agent-plugin --features dynamic --test shell_dynamic
//! ```

#![cfg(feature = "dynamic")]
#![allow(unsafe_code)] // Loading a native plugin requires dlopen.

use std::path::PathBuf;

use tower_agent_plugin::{AbiToolCall, AbiValue, ArchivedAbiToolResult, LoadedPlugin, PluginState};

fn plugin_path() -> Option<PathBuf> {
    std::env::var_os("TPA_SHELL_PLUGIN").map(PathBuf::from)
}

#[test]
fn loads_shell_plugin_metadata_and_runs_a_command() {
    let Some(path) = plugin_path() else {
        eprintln!("skipping: set TPA_SHELL_PLUGIN to the built shell plugin");
        return;
    };
    let plugin = unsafe { LoadedPlugin::load(&path, |_| None) }.expect("load shell plugin");
    assert_eq!(plugin.manifest().name, "shell");
    assert_eq!(plugin.lifecycle().state(), PluginState::Started);
    let descriptor = plugin.tool_descriptor("shell").expect("shell descriptor");
    assert!(descriptor.description.contains("bash-compatible"));
    assert!(descriptor.parameters_json.contains("\"command\""));

    let call = AbiToolCall {
        id: "shell-dynamic-1".to_owned(),
        tool: "shell".to_owned(),
        args: AbiValue::from_json(&serde_json::json!({
            "command": "printf 'dynamic-shell\\n'"
        })),
    };
    let request = rkyv::to_bytes::<rkyv::rancor::Error>(&call).expect("archive request");
    let response = plugin.call_archived(&request).expect("call plugin");
    let result = rkyv::access::<ArchivedAbiToolResult, rkyv::rancor::Error>(&response)
        .expect("access response");
    assert!(result.ok, "{}", result.content.as_str());
    assert_eq!(result.content.as_str(), "dynamic-shell\n");

    plugin.stop().expect("stop");
    plugin.unload().expect("unload");
}
