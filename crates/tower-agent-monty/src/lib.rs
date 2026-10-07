//! `tower-agent-monty` — 基于 [monty](https://crates.io/crates/monty) 的 mods 宿主
//! （`docs/plugins.md` §4.3；cc `mods` 模式）。
//!
//! monty 是 Rust 写的 Python 子集沙箱；这里用 `monty-pool` 把解释器跑在**工作子进程**
//! 里（崩溃隔离）：崩溃只杀掉 worker，池会替换它，宿主进程不受影响。
//!
//! 一个「mod」就是一段 LLM/用户生成的 monty 代码，它能：
//!
//! - **调宿主能力**：脚本里未定义的函数调用会挂起为 `TurnEvent::FunctionCall`，经
//!   [`MontyHost`] 派发到宿主（[`ModManager`] 提供 `tool_call` / `log` / `state_*`）；
//! - **提供工具**：安装期调用 `tool_register(name, function, ...)`，宿主把它包成一个
//!   [`ModTool`] 动态注册进工具集；
//! - **改 UI**：调用 `ui_status` / `ui_panel` / `ui_tool_render`，写进前端无关的
//!   [`tower_agent_core::UiState`]，由 TUI / web / GUI 各自渲染。
//!
//! 对 LLM 暴露的作者工具（[`authoring_tools`]）：`mod_write` / `mod_reload` /
//! `mod_list` / `mod_remove`。
//!
//! 运行需要 monty worker 二进制（`cargo build -p monty-runtime` 或 PyPI 的
//! `pydantic-monty-runtime`），路径由 [`ModManager::new`] 给出。

mod author;
pub mod host;
mod json;
mod manager;
mod plugin;
mod tool;

pub use author::authoring_tools;
pub use host::{HostCall, MontyHost};
pub use manager::{ModManager, ModSummary, manager_with_worker};
pub use plugin::MontyPlugin;
pub use tool::{ModTool, ToolSpec};

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use futures_util::future::BoxFuture;
    use serde_json::{Value, json};
    use tower_agent_core::UiState;
    use tower_agent_plugins::HostApi;

    use super::*;

    fn worker() -> Option<PathBuf> {
        std::env::var_os("TPA_MONTY_BIN").map(PathBuf::from)
    }

    /// 一个把宿主调用记录下来的派发器。
    struct RecordingHost;

    impl MontyHost for RecordingHost {
        fn dispatch(&self, call: HostCall) -> BoxFuture<'static, Result<Value, String>> {
            Box::pin(async move { Ok(json!({ "echo": call.function })) })
        }
    }

    #[tokio::test]
    async fn runs_a_script_when_the_worker_is_available() {
        let Some(worker) = worker() else {
            eprintln!("skipping: set TPA_MONTY_BIN to the monty worker binary");
            return;
        };
        let plugin = MontyPlugin::load(
            tower_agent_plugins::PluginManifest::new(
                "demo",
                semver::Version::new(1, 0, 0),
                tower_agent_plugins::PluginKind::Jit,
            ),
            "def upper(value):\n    return value.upper()\n",
            worker,
            Arc::new(RecordingHost),
        )
        .await
        .expect("load monty plugin");
        plugin.activate().await.expect("activate");
        let result = plugin
            .call_function("upper", &json!("hi"))
            .await
            .expect("call");
        assert_eq!(result, json!("HI"));
    }

    #[tokio::test]
    async fn host_functions_are_resolved_by_the_callback() {
        let Some(worker) = worker() else {
            eprintln!("skipping: set TPA_MONTY_BIN to the monty worker binary");
            return;
        };
        struct Greet;
        impl MontyHost for Greet {
            fn dispatch(&self, call: HostCall) -> BoxFuture<'static, Result<Value, String>> {
                Box::pin(
                    async move { Ok(json!(format!("{}({})", call.function, call.args.len()))) },
                )
            }
        }
        let plugin = MontyPlugin::load(
            tower_agent_plugins::PluginManifest::new(
                "greeter",
                semver::Version::new(1, 0, 0),
                tower_agent_plugins::PluginKind::Jit,
            ),
            "def greet(value):\n    return host_hello(value)\n",
            worker,
            Arc::new(Greet),
        )
        .await
        .expect("load monty plugin");
        plugin.activate().await.expect("activate");
        let result = plugin
            .call_function("greet", &json!("world"))
            .await
            .expect("call");
        assert_eq!(result, json!("host_hello(1)"));
    }

    #[tokio::test]
    async fn mod_registers_a_tool_and_writes_status() {
        let Some(worker) = worker() else {
            eprintln!("skipping: set TPA_MONTY_BIN to the monty worker binary");
            return;
        };
        let ui = UiState::new();
        let host = HostApi::from_tools(Vec::<tower_agent_core::AgentTool>::new());
        let manager = ModManager::new(worker, host, ui.clone());

        let code = "\
tool_register(\"shout\", \"shout\", \"shout a string\", {\"type\": \"object\"})
ui_status(\"mod:demo\", \"demo active\", \"cyan\", 5)

def shout(args):
    return args[\"text\"].upper()
";
        manager.write("demo", code).await.expect("write mod");

        let summaries = manager.list();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].tools, vec!["shout"]);
        assert_eq!(
            ui.snapshot().status[0].text,
            "demo active",
            "ui_status 写进了共享 UI 状态"
        );

        let tools = manager.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "shout");

        // 通过 ToolRouter 真的调用 mod 工具。
        let mut router = tower_agent_runtime::ToolRouter::new();
        router.insert_boxed("shout", tools[0].service().clone());
        let request = tower_agent_core::ToolCallRequest::new(tower_agent_provider::ToolCall {
            id: "c1".into(),
            name: "shout".into(),
            arguments: serde_json::json!({ "text": "hi" })
                .as_object()
                .cloned()
                .unwrap()
                .into(),
            thought_signature: None,
            namespace: None,
        });
        use tower::ServiceExt as _;
        let stream = router.oneshot(request).await.expect("router call");
        let results: Vec<_> = {
            use futures_util::StreamExt as _;
            stream.collect().await
        };
        assert_eq!(results.len(), 1);
        assert!(!results[0].is_error);
        let text: String = results[0]
            .content
            .iter()
            .filter_map(|block| match block {
                tower_agent_provider::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "HI", "mod 工具真的执行了 Python 函数");
    }

    #[tokio::test]
    async fn mod_can_call_host_tools() {
        use schemars::JsonSchema;
        use serde::Deserialize;

        let Some(worker) = worker() else {
            eprintln!("skipping: set TPA_MONTY_BIN to the monty worker binary");
            return;
        };

        #[derive(Deserialize, JsonSchema)]
        struct EchoParams {
            text: String,
        }

        let echo = tower_agent_core::ToolRegistration::new::<EchoParams, _, _>(
            "echo",
            "echo text back",
            |params: EchoParams, _request| async move {
                Ok(tower_agent_core::AgentToolResult::text(params.text))
            },
        );
        let host = HostApi::from_tools([echo]);
        let manager = ModManager::new(worker, host, UiState::new());

        let code = "\
tool_register(\"shout\", \"shout\", \"uppercase via host echo\", {\"type\": \"object\"})

def shout(args):
    return tool_call(\"echo\", {\"text\": args[\"text\"]}).upper()
";
        manager.write("echoer", code).await.expect("write mod");

        let tools = manager.tools();
        let mut router = tower_agent_runtime::ToolRouter::new();
        router.insert_boxed("shout", tools[0].service().clone());
        let request = tower_agent_core::ToolCallRequest::new(tower_agent_provider::ToolCall {
            id: "c2".into(),
            name: "shout".into(),
            arguments: serde_json::json!({ "text": "hi" })
                .as_object()
                .cloned()
                .unwrap()
                .into(),
            thought_signature: None,
            namespace: None,
        });
        use tower::ServiceExt as _;
        let stream = router.oneshot(request).await.expect("router call");
        let results: Vec<_> = {
            use futures_util::StreamExt as _;
            stream.collect().await
        };
        let text: String = results[0]
            .content
            .iter()
            .filter_map(|block| match block {
                tower_agent_provider::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "HI", "mod 通过 tool_call 调用了宿主工具并拿回结果");
    }
}
