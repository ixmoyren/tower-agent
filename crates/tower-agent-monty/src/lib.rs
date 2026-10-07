//! `tower-agent-monty` — 内嵌 [monty](https://crates.io/crates/monty) wasm 沙箱的
//! mods 宿主（`docs/plugins.md` §4.3；cc `mods` 模式）。
//!
//! AI/用户生成的 monty 代码**必须跑在沙箱里**：`tower-agent-monty` 内嵌一个把 monty
//! 解释器编译成 wasm 的组件（`wasm/monty-sandbox`，经 `wasm-opt -Oz --strip-debug`
//! 优化），用 [wasmtime](https://wasmtime.dev) 在**组件沙箱**里执行。没有外部
//! `monty` CLI、没有子进程；mods 代码的越界/栈溢出/内存超限会被 wasmtime 转成 trap
//! 错误，不会带走宿主进程。wasmtime 开启 JIT + JIT 缓存并自行做版本校验。
//!
//! 一个「mod」是 LLM/用户生成的一段 monty 代码，它能：
//!
//! - **调宿主能力**：脚本里未定义的函数调用会让组件返回 `function-call` 进度，经
//!   [`MontyHost`] 派发到宿主（[`ModManager`] 提供 `tool_call` / `log` / `state_*`）；
//! - **提供工具**：安装期调用 `tool_register(name, function, ...)`，宿主把它包成一个
//!   [`ModTool`] 动态注册进工具集；
//! - **改 UI**：调用 `ui_status` / `ui_panel` / `ui_tool_render`，写进前端无关的
//!   [`tower_agent_core::UiState`]，由 TUI / web / GUI 各自渲染。
//!
//! 对 LLM 暴露的作者工具（[`authoring_tools`]）：`mod_write` / `mod_reload` /
//! `mod_list` / `mod_remove`。
//!
//! 重新生成内嵌组件：`./wasm/monty-sandbox/generate.sh`。

mod author;
pub mod host;
mod manager;
mod plugin;
mod python;
mod sandbox;
mod tool;
mod wire;

pub use author::authoring_tools;
pub use host::{HostCall, MontyHost};
pub use manager::{ModManager, ModPermissions, ModSummary};
pub use plugin::MontyPlugin;
pub use tool::{ModTool, ToolSpec};

#[cfg(test)]
mod tests {
    use std::sync::{Arc, OnceLock};

    use futures_util::future::BoxFuture;
    use serde_json::{Value, json};
    use tower_agent_core::UiState;
    use tower_agent_plugins::HostApi;

    use super::*;
    use crate::sandbox::SandboxPool;

    /// 测试共享一个沙箱池（避免每个用例都重新 JIT 编译组件）。
    fn pool() -> Arc<SandboxPool> {
        static POOL: OnceLock<Arc<SandboxPool>> = OnceLock::new();
        Arc::clone(POOL.get_or_init(|| Arc::new(SandboxPool::new(4))))
    }

    /// 通过 ToolRouter 调用一个 mod 工具，返回模型可见文本。
    async fn call_mod_tool(tool: &tower_agent_core::AgentTool, args: Value) -> String {
        use futures_util::StreamExt as _;
        use tower::ServiceExt as _;

        let mut router = tower_agent_runtime::ToolRouter::new();
        router.insert_boxed(tool.name(), tool.service().clone());
        let request = tower_agent_core::ToolCallRequest::new(tower_agent_provider::ToolCall {
            id: "call".into(),
            name: tool.name().into(),
            arguments: args.as_object().cloned().unwrap_or_default().into(),
            thought_signature: None,
            namespace: None,
        });
        let stream = router.oneshot(request).await.expect("router call");
        let results: Vec<_> = stream.collect().await;
        results[0]
            .content
            .iter()
            .filter_map(|block| match block {
                tower_agent_provider::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    fn manifest(name: &str) -> tower_agent_plugins::PluginManifest {
        tower_agent_plugins::PluginManifest::new(
            name,
            semver::Version::new(1, 0, 0),
            tower_agent_plugins::PluginKind::Jit,
        )
    }

    /// 一个把宿主调用记录下来的派发器。
    struct RecordingHost;

    impl MontyHost for RecordingHost {
        fn dispatch(&self, call: HostCall) -> BoxFuture<'static, Result<Value, String>> {
            Box::pin(async move { Ok(json!({ "echo": call.function })) })
        }
    }

    #[tokio::test]
    async fn runs_a_script_and_calls_a_function() {
        let plugin = MontyPlugin::load(
            manifest("demo"),
            "def upper(value):\n    return value.upper()\n",
            Arc::new(RecordingHost),
            pool(),
        )
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
        struct Greet;
        impl MontyHost for Greet {
            fn dispatch(&self, call: HostCall) -> BoxFuture<'static, Result<Value, String>> {
                Box::pin(
                    async move { Ok(json!(format!("{}({})", call.function, call.args.len()))) },
                )
            }
        }
        let plugin = MontyPlugin::load(
            manifest("greeter"),
            "def greet(value):\n    return host_hello(value)\n",
            Arc::new(Greet),
            pool(),
        )
            .expect("load monty plugin");
        plugin.activate().await.expect("activate");
        let result = plugin
            .call_function("greet", &json!("world"))
            .await
            .expect("call");
        assert_eq!(result, json!("host_hello(1)"));
    }

    #[tokio::test]
    async fn infinite_loop_is_bounded_by_resource_limits() {
        let plugin = MontyPlugin::load(
            manifest("spin"),
            "while True:\n    pass\n",
            Arc::new(RecordingHost),
            pool(),
        )
            .expect("load monty plugin");
        let error = plugin.activate().await.expect_err("must be bounded");
        let message = error.to_string();
        assert!(
            message.contains("time limit") || message.contains("Timeout"),
            "unexpected error: {message}"
        );
    }

    #[tokio::test]
    async fn mod_registers_a_tool_and_writes_status() {
        let ui = UiState::new();
        let host = HostApi::from_tools(Vec::<tower_agent_core::AgentTool>::new());
        let manager = ModManager::new(host, ui.clone());

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
        let text = call_mod_tool(&tools[0], json!({ "text": "hi" })).await;
        assert_eq!(text, "HI", "mod 工具真的执行了 Python 函数");
    }

    #[tokio::test]
    async fn mod_can_call_host_tools() {
        use schemars::JsonSchema;
        use serde::Deserialize;

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
        let manager = ModManager::new(host, UiState::new());

        let code = "\
tool_register(\"shout\", \"shout\", \"uppercase via host echo\", {\"type\": \"object\"})

def shout(args):
    return tool_call(\"echo\", {\"text\": args[\"text\"]}).upper()
";
        manager.write("echoer", code).await.expect("write mod");

        let tools = manager.tools();
        let text = call_mod_tool(&tools[0], json!({ "text": "hi" })).await;
        assert_eq!(text, "HI", "mod 通过 tool_call 调用了宿主工具并拿回结果");
    }

    #[tokio::test]
    async fn mod_reload_swaps_the_tool_implementation() {
        let manager = ModManager::new(
            HostApi::from_tools(Vec::<tower_agent_core::AgentTool>::new()),
            UiState::new(),
        );
        let header = "tool_register(\"shout\", \"shout\", \"\", {\"type\": \"object\"})\n\n\
                      def shout(args):\n";

        let first = format!("{header}    return args[\"text\"]\n");
        manager.write("demo", &first).await.expect("install");
        assert_eq!(
            call_mod_tool(&manager.tools()[0], json!({ "text": "hi" })).await,
            "hi"
        );

        let second = format!("{header}    return args[\"text\"].upper()\n");
        manager.reload("demo", &second).await.expect("reload");
        assert_eq!(
            call_mod_tool(&manager.tools()[0], json!({ "text": "hi" })).await,
            "HI",
            "热重载替换了工具实现"
        );
    }

    #[tokio::test]
    async fn warmup_prefills_the_sandbox_pool() {
        let manager = ModManager::new(
            HostApi::from_tools(Vec::<tower_agent_core::AgentTool>::new()),
            UiState::new(),
        );
        manager.warmup(2).expect("warmup");
        assert!(
            manager.idle_sandboxes() >= 2,
            "warmup 应预先创建实例，实际 {}",
            manager.idle_sandboxes()
        );
    }

    #[tokio::test]
    async fn mod_reads_a_mounted_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("note.txt"), "hello from mount").expect("write note");

        let manager = ModManager::new(
            HostApi::from_tools(Vec::<tower_agent_core::AgentTool>::new()),
            UiState::new(),
        );
        manager
            .mount("/work", dir.path(), monty_fs::MountMode::ReadOnly)
            .expect("mount");

        let code = "\
from pathlib import Path

tool_register(\"read_note\", \"read_note\", \"\", {\"type\": \"object\"})

def read_note(args):
    return Path(\"/work/note.txt\").read_text()
";
        manager.write("reader", code).await.expect("write mod");
        let text = call_mod_tool(&manager.tools()[0], json!({})).await;
        assert_eq!(text, "hello from mount", "mod 读到了挂载的宿主文件");
    }

    #[tokio::test]
    async fn fs_permission_denies_mounted_reads() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("note.txt"), "secret").expect("write note");

        let manager = ModManager::new(
            HostApi::from_tools(Vec::<tower_agent_core::AgentTool>::new()),
            UiState::new(),
        );
        manager
            .mount("/work", dir.path(), monty_fs::MountMode::ReadOnly)
            .expect("mount");
        manager.set_permissions(
            "locked",
            ModPermissions {
                fs: false,
                ..ModPermissions::default()
            },
        );

        let code = "\
from pathlib import Path

tool_register(\"read_note\", \"read_note\", \"\", {\"type\": \"object\"})

def read_note(args):
    return Path(\"/work/note.txt\").read_text()
";
        manager.write("locked", code).await.expect("write mod");
        let text = call_mod_tool(&manager.tools()[0], json!({})).await;
        assert!(
            text.contains("PermissionError") || text.contains("not allowed"),
            "无 fs 权限时应被拒，实际：{text}"
        );
    }
}
