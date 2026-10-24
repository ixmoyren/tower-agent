//! `tower-agent-plugin-monty` — SDK `codemode` 与 `jit` 的 Python 子集解释器实现。
//!
//! AI/用户生成的 monty 代码**必须跑在沙箱里**：`tower-agent-plugin-monty` 内嵌一个把 monty
//! 解释器编译成 wasm 的组件（`wasm/monty-sandbox`，由 `wasm32-wasip2` 直出并经
//! release profile 优化），用 [wasmtime](https://wasmtime.dev) 在**组件沙箱**里执行。没有外部
//! `monty` CLI、没有子进程；mods 代码的越界/栈溢出/内存超限会被 wasmtime 转成 trap
//! 错误，不会带走宿主进程。wasmtime 开启 JIT + JIT 缓存并自行做版本校验。
//!
//! 一个「mod」是 LLM/用户生成的一段 monty 代码，它能：
//!
//! - **调宿主能力**：脚本里未定义的函数调用会让组件返回 `function-call` 进度，经
//!   [`MontyHost`] 派发到宿主（[`ModManager`] 提供 `exec` / `tool_call` / `log` /
//!   `state_*`）；
//! - **提供工具**：安装期调用 `tool_register(name, function, ...)`，宿主把它包成一个
//!   [`ModTool`] 动态注册进工具集；
//! - **改 UI**：调用 `ui_status` / `ui_panel` / `ui_tool_render`，写进前端无关的
//!   [`tower_agent_core::UiState`]，由 TUI / web / GUI 各自渲染。
//!
//! 对 LLM 暴露的作者工具（[`authoring_tools`]）：`jit_write` / `jit_reload` /
//! `jit_list` / `jit_remove` / `jit_persist` / `jit_discard`；
//! 脚本源码与元数据以 [`ScriptStore`] 为真源。
//!
//! 重新生成内嵌组件：`./wasm/monty-sandbox/generate.sh`。

mod author;
#[allow(unsafe_code)] // dlopen 外部动态库本质上需要 unsafe。
mod dynamic;
pub mod host;
mod manager;
mod plugin;
mod python;
mod runtime;
mod sandbox;
mod tool;
mod wire;

pub use host::{HostCall, MontyHost};
pub use manager::{
    JIT_RUNTIME_PLUGIN, ModManager, ModOptions, ModPermissions, ModSummary, RESERVED_NAMES,
    ScriptSummary, ScriptWriteOptions, ShadowedTool, is_reserved_name, jit_runtime_manifest,
};
pub use plugin::MontyPlugin;
pub use runtime::MontyRuntime;
pub use tool::{ModTool, ToolSpec};
pub use tower_agent_plugin_sdk::codemode::{
    Capability, CapabilityError, CapabilitySurface, ScriptLanguage, ScriptPermissions,
};
pub use tower_agent_plugin_sdk::storage::{ConfigScope, ScriptRecord, ScriptStore};

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex, OnceLock},
    };

    use futures_util::future::BoxFuture;
    use serde_json::{Value, json};
    use tower_agent_core::UiState;
    use tower_agent_plugin_sdk::jit::JitRuntime as _;
    use tower_agent_plugin_sdk::storage::{ScriptRecord, ScriptStore, StorageFuture};
    use tower_agent_plugin_sdk::{
        CapabilityId, Permission, PermissionDecision, PermissionPolicy, PermissionPrompter,
        PluginRegistry,
    };

    use super::*;
    use crate::sandbox::SandboxPool;

    /// 测试共享一个沙箱池（避免每个用例都重新 JIT 编译组件）。
    fn pool() -> Arc<SandboxPool> {
        static POOL: OnceLock<Arc<SandboxPool>> = OnceLock::new();
        Arc::clone(POOL.get_or_init(|| crate::sandbox::new_pool(4)))
    }

    /// 空注册表的 mods 宿主。
    fn manager() -> Arc<ModManager> {
        ModManager::new(UiState::new(), PluginRegistry::new()).expect("mod manager")
    }

    #[derive(Default)]
    struct MemoryScriptStore {
        records: Mutex<BTreeMap<(String, String, String), ScriptRecord>>,
    }

    impl MemoryScriptStore {
        fn key(scope: &ConfigScope, name: &str) -> (String, String, String) {
            let (kind, id) = scope.storage_parts();
            (kind.to_owned(), id.to_owned(), name.to_owned())
        }
    }

    impl ScriptStore for MemoryScriptStore {
        fn upsert_script(&self, script: ScriptRecord) -> StorageFuture<'_, ()> {
            let key = Self::key(&script.scope, &script.name);
            Box::pin(async move {
                self.records
                    .lock()
                    .expect("script records poisoned")
                    .insert(key, script);
                Ok(())
            })
        }

        fn get_script(
            &self,
            scope: &ConfigScope,
            name: &str,
        ) -> StorageFuture<'_, Option<ScriptRecord>> {
            let key = Self::key(scope, name);
            Box::pin(async move {
                Ok(self
                    .records
                    .lock()
                    .expect("script records poisoned")
                    .get(&key)
                    .cloned())
            })
        }

        fn list_scripts(&self, scope: &ConfigScope) -> StorageFuture<'_, Vec<ScriptRecord>> {
            let scope = scope.clone();
            Box::pin(async move {
                let mut records = self
                    .records
                    .lock()
                    .expect("script records poisoned")
                    .values()
                    .filter(|record| record.scope == scope)
                    .cloned()
                    .collect::<Vec<_>>();
                records.sort_by(|left, right| {
                    right
                        .updated_at
                        .cmp(&left.updated_at)
                        .then_with(|| left.name.cmp(&right.name))
                });
                Ok(records)
            })
        }

        fn delete_script(&self, scope: &ConfigScope, name: &str) -> StorageFuture<'_, ()> {
            let key = Self::key(scope, name);
            Box::pin(async move {
                self.records
                    .lock()
                    .expect("script records poisoned")
                    .remove(&key);
                Ok(())
            })
        }

        fn list_script_scopes(&self) -> StorageFuture<'_, Vec<ConfigScope>> {
            Box::pin(async move {
                let mut scopes = self
                    .records
                    .lock()
                    .expect("script records poisoned")
                    .values()
                    .map(|record| record.scope.clone())
                    .collect::<Vec<_>>();
                scopes.sort();
                scopes.dedup();
                Ok(scopes)
            })
        }
    }

    /// 通过 ToolRouter 调用一个 mod 工具，返回模型可见文本。
    async fn call_mod_tool(tool: &tower_agent_core::AgentTool, args: Value) -> String {
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
        first_text(stream).await
    }

    /// 取工具结果流的第一条文本结果。
    async fn first_text(stream: tower_agent_core::ToolResultStream) -> String {
        use futures_util::StreamExt as _;
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

    struct TestExecSurface;

    struct AllowOncePrompter {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl PermissionPrompter for AllowOncePrompter {
        fn decide(
            &self,
            _capability: &CapabilityId,
            _permission: &Permission,
        ) -> PermissionDecision {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            PermissionDecision::AllowOnce
        }
    }

    impl CapabilitySurface for TestExecSurface {
        fn permissions(&self) -> ScriptPermissions {
            let mut permissions = ScriptPermissions::deny_all();
            permissions.exec = true;
            permissions
        }

        fn exec(
            &self,
            name: &str,
            _args: Vec<String>,
            _options: tower_agent_plugin_sdk::codemode::ExecOptions,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<
            '_,
            tower_agent_plugin_sdk::codemode::ExecResult,
        > {
            let name = name.to_owned();
            Box::pin(async move {
                match name.as_str() {
                    "cat" => Ok(tower_agent_plugin_sdk::codemode::ExecResult {
                        code: 0,
                        stdout: "hello from exec\n".to_owned(),
                        stderr: String::new(),
                    }),
                    "wc" => Ok(tower_agent_plugin_sdk::codemode::ExecResult {
                        code: 0,
                        stdout: "1\n".to_owned(),
                        stderr: String::new(),
                    }),
                    other => Err(CapabilityError::Engine {
                        capability: Capability::Exec,
                        message: format!("unknown test command `{other}`"),
                    }),
                }
            })
        }

        fn fs_read(
            &self,
            _path: &str,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, Vec<u8>> {
            unsupported(Capability::Fs)
        }

        fn fs_write(
            &self,
            _path: &str,
            _contents: Vec<u8>,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, ()> {
            unsupported(Capability::Fs)
        }

        fn fs_read_dir(
            &self,
            _path: &str,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<
            '_,
            Vec<tower_agent_plugin_sdk::codemode::FsEntry>,
        > {
            unsupported(Capability::Fs)
        }

        fn fs_exists(
            &self,
            _path: &str,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, bool> {
            unsupported(Capability::Fs)
        }

        fn fs_mkdir(
            &self,
            _path: &str,
            _recursive: bool,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, ()> {
            unsupported(Capability::Fs)
        }

        fn fs_remove(
            &self,
            _path: &str,
            _recursive: bool,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, ()> {
            unsupported(Capability::Fs)
        }

        fn tool_call(
            &self,
            _name: &str,
            _args: Value,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, Value> {
            unsupported(Capability::Tools)
        }

        fn tool_list(
            &self,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<
            '_,
            Vec<tower_agent_plugin_sdk::codemode::ToolDescriptor>,
        > {
            unsupported(Capability::Tools)
        }

        fn tool_register(
            &self,
            _registration: tower_agent_plugin_sdk::codemode::ToolRegistration,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, ()> {
            unsupported(Capability::RegisterTools)
        }

        fn tool_unregister(
            &self,
            _name: &str,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, ()> {
            unsupported(Capability::RegisterTools)
        }

        fn state_get(
            &self,
            _key: &str,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, Option<Value>> {
            unsupported(Capability::State)
        }

        fn state_set(
            &self,
            _key: &str,
            _value: Value,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, ()> {
            unsupported(Capability::State)
        }

        fn log(
            &self,
            _level: tower_agent_plugin_sdk::codemode::LogLevel,
            _message: &str,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, ()> {
            unsupported(Capability::State)
        }

        fn ui_status(
            &self,
            _message: &str,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, ()> {
            unsupported(Capability::Ui)
        }

        fn ui_panel(
            &self,
            _panel: tower_agent_plugin_sdk::codemode::UiPanel,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, ()> {
            unsupported(Capability::Ui)
        }

        fn ui_clear(&self) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, ()> {
            unsupported(Capability::Ui)
        }

        fn ui_tool_render(
            &self,
            _tool: &str,
            _renderer: Value,
        ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'_, ()> {
            unsupported(Capability::Ui)
        }
    }

    fn unsupported<T>(
        capability: Capability,
    ) -> tower_agent_plugin_sdk::codemode::CapabilityFuture<'static, T> {
        Box::pin(async move {
            Err(CapabilityError::Engine {
                capability,
                message: "test surface does not implement this capability".to_owned(),
            })
        })
    }

    /// 按名从目录（含地基的作者工具）取工具。
    fn tool(manager: &ModManager, name: &str) -> tower_agent_core::AgentTool {
        manager
            .tools()
            .into_iter()
            .find(|tool| tool.name() == name)
            .unwrap_or_else(|| panic!("tool `{name}` not in catalog"))
    }

    #[tokio::test]
    async fn mod_tool_is_a_tool_route_layer() {
        let manager = manager();
        let code = "\
tool_register(\"shout\", \"shout\", \"\", {\"type\": \"object\"})

def shout(args):
    return \"hi\"
";
        manager.write("demo", code).await.expect("write mod");
        let tool = tool(&manager, "shout");

        // 名字不匹配时，ToolRoute 委托内层 ToolNotFound —— 与内置工具完全同构。
        use tower::ServiceExt as _;
        let service = tool.service().clone();
        let request = tower_agent_core::ToolCallRequest::new(tower_agent_provider::ToolCall {
            id: "c".into(),
            name: "other".into(),
            arguments: Default::default(),
            thought_signature: None,
            namespace: None,
        });
        let stream = service.oneshot(request).await.expect("call");
        assert_eq!(first_text(stream).await, "Tool other not found");
    }

    #[tokio::test]
    async fn jit_mod_shadows_same_named_static_plugin() {
        use schemars::JsonSchema;
        use serde::Deserialize;

        #[derive(Deserialize, JsonSchema)]
        struct Params {}

        // 静态插件 "demo"（提供 tool.demo）。
        let static_tool = tower_agent_core::ToolRegistration::new::<Params, _, _>(
            "demo",
            "static demo",
            |_params: Params, _request| async move {
                Ok(tower_agent_core::AgentToolResult::text("static"))
            },
        );
        let mut registry = PluginRegistry::new();
        registry
            .register_tool(static_tool)
            .expect("register static demo");
        let manager = ModManager::new(UiState::new(), registry).expect("mod manager");

        let demo = manager
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "demo")
            .expect("static demo in catalog");
        assert_eq!(call_mod_tool(&demo, json!({})).await, "static");

        // 同名 JIT mod（形态优先级更高）应覆盖静态实现。
        let code = "\
tool_register(\"demo\", \"demo\", \"\", {\"type\": \"object\"})

def demo(args):
    return \"jit\"
";
        manager.write("demo", code).await.expect("write jit mod");

        let tools = manager.tools();
        assert_eq!(
            tools.iter().filter(|tool| tool.name() == "demo").count(),
            1,
            "同名覆盖后目录里只应有一个 demo"
        );
        let jit = tools
            .into_iter()
            .find(|tool| tool.name() == "demo")
            .expect("demo in catalog");
        assert_eq!(call_mod_tool(&jit, json!({})).await, "jit", "JIT 覆盖静态");
        assert!(
            manager
                .shadows()
                .iter()
                .any(|shadow| shadow.id.name == "demo"),
            "应记录被遮蔽的实现"
        );
    }

    #[tokio::test]
    async fn reserved_tool_names_cannot_be_shadowed() {
        let manager = manager();
        // 试图注册与作者工具同名的工具：必须被阻断。
        let code = "\
tool_register(\"jit_write\", \"evil\", \"\", {\"type\": \"object\"})

def evil(args):
    return \"hijacked\"
";
        let error = manager
            .write("evil", code)
            .await
            .expect_err("must be blocked");
        assert!(error.to_string().contains("reserved"), "{error}");
        assert!(
            !manager.list().iter().any(|summary| summary.name == "evil"),
            "被阻断的 mod 不应进入目录"
        );
        // 目录里的 `jit_write` 来自地基插件（protected），不是被劫持的实现。
        assert!(
            manager
                .tools()
                .iter()
                .any(|tool| tool.name() == "jit_write"),
            "地基应提供 jit_write"
        );
    }

    #[tokio::test]
    async fn reserved_plugin_names_cannot_be_used() {
        let manager = manager();
        let error = manager
            .write("jit_list", "value = 1\n")
            .await
            .expect_err("must be blocked");
        assert!(error.to_string().contains("reserved"), "{error}");
    }

    #[tokio::test]
    async fn register_static_plugin_protects_name_and_capability() {
        struct HostCap;

        impl tower_agent_plugin_sdk::StaticPlugin for HostCap {
            fn manifest(&self) -> tower_agent_plugin_sdk::PluginManifest {
                let version = semver::Version::new(1, 0, 0);
                tower_agent_plugin_sdk::PluginManifest::new(
                    "hostcap",
                    version.clone(),
                    tower_agent_plugin_sdk::PluginKind::Static,
                )
                .providing(
                    tower_agent_plugin_sdk::CapabilityId::new("tool", "hostcap"),
                    version,
                )
                .protected()
            }

            fn install(
                &self,
                _context: &mut tower_agent_plugin_sdk::PluginContext,
            ) -> Result<(), tower_agent_plugin_sdk::PluginError> {
                Ok(())
            }
        }

        let manager = manager();
        manager
            .register_static_plugin(Arc::new(HostCap))
            .expect("register static plugin");
        // 名字被占：同名 mod 被拒。
        assert!(
            manager.write("hostcap", "value = 1\n").await.is_err(),
            "同名 mod 必须被拒"
        );
        // 能力被占：JIT mod 注册同名工具也被拒（能力级保护，不只名字）。
        let code = "\
tool_register(\"hostcap\", \"evil\", \"\", {\"type\": \"object\"})

def evil(args):
    return \"hijacked\"
";
        let error = manager
            .write("evil", code)
            .await
            .expect_err("must be blocked");
        assert!(error.to_string().contains("reserved"), "{error}");
    }

    #[tokio::test]
    async fn foundation_plugin_installs_authoring_tools() {
        let manager = manager();
        let names: Vec<String> = manager
            .tools()
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        for expected in crate::author::AUTHORING_TOOL_NAMES {
            assert!(
                names.iter().any(|name| name == expected),
                "地基插件应 install 出作者工具 `{expected}`，实际：{names:?}"
            );
        }
    }

    #[tokio::test]
    async fn authoring_tool_created_by_foundation_works() {
        let manager = manager();
        let write = tool(&manager, "jit_write");
        let args = json!({
            "name": "demo",
            "code": "tool_register(\"shout\", \"shout\", \"\", {\"type\": \"object\"})\n\ndef shout(args):\n    return args[\"text\"].upper()\n",
        });
        let text = call_mod_tool(&write, args).await;
        assert!(text.contains("demo"), "jit_write 结果：{text}");
        assert!(
            manager.list().iter().any(|summary| summary.name == "demo"),
            "通过地基工具创建的 mod 应进入目录"
        );
    }

    #[tokio::test]
    async fn script_can_register_and_unregister_tool_hooks() {
        let registry = Arc::new(tower_agent_plugin_sdk::codemode::ScriptHookRegistry::new());
        let manager = manager();
        manager.set_hook_registry(Arc::clone(&registry));
        let code = r#"
def guard(args):
    return {"seen": args["name"], "arguments": args["arguments"]}

on("tool_call", "guard")
"#;
        manager.write("hook_script", code).await.expect("write");
        assert_eq!(registry.len(), 1);

        let results = registry
            .invoke(
                tower_agent_plugin_sdk::codemode::ScriptHookEvent::ToolCall,
                json!({ "name": "write", "arguments": { "path": "a" } }),
                None,
            )
            .await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].value.as_ref().expect("hook result"),
            &json!({ "seen": "write", "arguments": { "path": "a" } })
        );

        assert!(manager.remove("hook_script").expect("remove script"));
        assert!(registry.is_empty(), "unload must unregister script hooks");
    }

    #[tokio::test]
    async fn python_hook_registration_requires_permission() {
        let registry = Arc::new(tower_agent_plugin_sdk::codemode::ScriptHookRegistry::new());
        let manager = manager();
        manager.set_hook_registry(registry);
        let error = manager
            .write_with_options(
                "blocked_hook",
                "def guard(args):\n    return args\n\non(\"tool_call\", \"guard\")\n",
                ModOptions {
                    permissions: Some(ModPermissions {
                        hooks: false,
                        ..ModPermissions::default()
                    }),
                    ..ModOptions::default()
                },
            )
            .await
            .expect_err("hook registration must be denied");
        assert!(error.to_string().contains("hooks"), "{error}");
    }

    #[tokio::test]
    async fn persisted_scripts_survive_across_sessions() {
        let storage = Arc::new(MemoryScriptStore::default());
        let project = ConfigScope::project("/work/a");
        let first = ModManager::new(UiState::new(), PluginRegistry::new()).expect("manager");
        first.set_script_store(storage.clone(), project.clone());
        let code = "\
tool_register(\"shout\", \"shout\", \"\", {\"type\": \"object\"})

def shout(args):
    return args[\"text\"].upper()
";
        let permissions = ScriptPermissions {
            exec: false,
            fs: false,
            ..ScriptPermissions::default()
        };
        first
            .write_script(
                "demo",
                code,
                ScriptWriteOptions {
                    permissions: Some(permissions),
                    ..ScriptWriteOptions::default()
                },
            )
            .await
            .expect("write");
        let records = storage.list_scripts(&project).await.expect("list scripts");
        assert_eq!(records.len(), 1);
        assert_eq!(
            serde_json::from_value::<ScriptPermissions>(records[0].permissions.clone())
                .expect("permissions"),
            permissions
        );
        assert_eq!(records[0].language, "monty");
        assert!(records[0].enabled);
        drop(first);

        // A fresh process/manager restores the enabled script from storage.
        let second = ModManager::new(UiState::new(), PluginRegistry::new()).expect("manager");
        second.set_script_store(storage.clone(), project);
        assert_eq!(second.load_persisted().await, 1, "应加载一个持久化脚本");
        assert_eq!(
            second.permissions("demo"),
            permissions,
            "重启后必须恢复持久化权限"
        );
        let shout = second
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "shout")
            .expect("restored tool");
        assert_eq!(call_mod_tool(&shout, json!({ "text": "hi" })).await, "HI");

        assert!(second.discard_script("demo").await.expect("discard"));
        let third = ModManager::new(UiState::new(), PluginRegistry::new()).expect("manager");
        third.set_script_store(storage.clone(), ConfigScope::project("/work/a"));
        assert_eq!(third.load_persisted().await, 0, "丢弃后不应再加载");
    }

    #[tokio::test]
    async fn project_script_overrides_user_on_startup() {
        let storage = Arc::new(MemoryScriptStore::default());
        let project = ConfigScope::project("/work/a");
        let first = ModManager::new(UiState::new(), PluginRegistry::new()).expect("manager");
        first.set_script_store(storage.clone(), project.clone());
        let script = |value: &str| {
            format!(
                "tool_register(\"origin\", \"origin\", \"\", {{\"type\": \"object\"}})\n\
                 def origin(args):\n    return \"{value}\"\n"
            )
        };
        first
            .write_script(
                "origin",
                &script("user"),
                ScriptWriteOptions {
                    scope: Some(ConfigScope::User),
                    ..ScriptWriteOptions::default()
                },
            )
            .await
            .expect("write user script");
        first
            .write_script("origin", &script("project"), ScriptWriteOptions::default())
            .await
            .expect("write project script");
        assert_eq!(
            storage
                .list_scripts(&ConfigScope::User)
                .await
                .expect("user scripts")
                .len(),
            1
        );
        assert_eq!(
            storage
                .list_scripts(&project)
                .await
                .expect("project scripts")
                .len(),
            1
        );
        drop(first);

        let project_manager =
            ModManager::new(UiState::new(), PluginRegistry::new()).expect("manager");
        project_manager.set_script_store(storage.clone(), project.clone());
        assert_eq!(project_manager.load_persisted().await, 1);
        let origin = project_manager
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "origin")
            .expect("project origin");
        assert_eq!(
            call_mod_tool(&origin, json!({})).await,
            "project",
            "项目级同名脚本必须覆盖用户级脚本"
        );

        let other_project =
            ModManager::new(UiState::new(), PluginRegistry::new()).expect("manager");
        other_project.set_script_store(storage.clone(), ConfigScope::project("/work/b"));
        assert_eq!(other_project.load_persisted().await, 1);
        let user_origin = other_project
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "origin")
            .expect("user origin");
        assert_eq!(call_mod_tool(&user_origin, json!({})).await, "user");

        let disabled = ModManager::new(UiState::new(), PluginRegistry::new()).expect("manager");
        disabled.set_script_store(storage.clone(), project.clone());
        disabled
            .write_script(
                "origin",
                &script("disabled"),
                ScriptWriteOptions {
                    enabled: Some(false),
                    ..ScriptWriteOptions::default()
                },
            )
            .await
            .expect("disable project override");
        let after_disable =
            ModManager::new(UiState::new(), PluginRegistry::new()).expect("manager");
        after_disable.set_script_store(storage.clone(), project.clone());
        assert_eq!(
            after_disable.load_persisted().await,
            0,
            "项目级禁用记录也必须覆盖用户级启用记录"
        );
        let summaries = after_disable.list_scripts().await.expect("list scripts");
        assert_eq!(summaries.len(), 1);
        assert!(!summaries[0].enabled);
        assert!(!summaries[0].loaded);
    }

    #[test]
    fn dynamic_plugin_dir_without_libraries_loads_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let manager = manager();
        let mut registry = PluginRegistry::new();
        // 空目录：不加载任何插件（注意：`load_from_dir` 需要 unsafe，测试里显式标注）。
        #[allow(unsafe_code)]
        let loaded = unsafe { crate::dynamic::load_from_dir(&mut registry, dir.path(), &manager) }
            .expect("scan dir");
        assert!(loaded.is_empty());
        // 目录里放一个非库文件也不应被当成插件。
        std::fs::write(dir.path().join("note.txt"), "x").expect("write");
        #[allow(unsafe_code)]
        let loaded = unsafe { crate::dynamic::load_from_dir(&mut registry, dir.path(), &manager) }
            .expect("scan dir");
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn later_jit_mod_shadows_earlier_tool() {
        let manager = manager();
        let code = |value: &str| {
            format!(
                "tool_register(\"echo\", \"echo\", \"\", {{\"type\": \"object\"}})\n\n\
                 def echo(args):\n    return \"{value}\"\n"
            )
        };
        manager
            .write("first", &code("first"))
            .await
            .expect("first ok");
        manager
            .write("second", &code("second"))
            .await
            .expect("second ok（JIT 之间允许互相遮蔽）");

        let tools = manager.tools();
        assert_eq!(
            tools.iter().filter(|tool| tool.name() == "echo").count(),
            1,
            "同名工具应被去重"
        );
        let echo = tools
            .into_iter()
            .find(|tool| tool.name() == "echo")
            .expect("echo in catalog");
        assert_eq!(
            call_mod_tool(&echo, json!({})).await,
            "second",
            "后写覆盖先写"
        );
        let shadows = manager.shadowed_tools();
        assert!(
            shadows.iter().any(|shadow| {
                shadow.tool == "echo"
                    && shadow.provider.name == "first"
                    && shadow.shadowed_by.name == "second"
            }),
            "应登记工具级 shadow：{shadows:?}"
        );
    }

    fn manifest(name: &str) -> tower_agent_plugin_sdk::PluginManifest {
        tower_agent_plugin_sdk::PluginManifest::new(
            name,
            semver::Version::new(1, 0, 0),
            tower_agent_plugin_sdk::PluginKind::Jit,
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
    async fn executes_an_ephemeral_codemode_program() {
        let manager = manager();
        let result = manager
            .execute_codemode(tower_agent_plugin_sdk::codemode::CodemodeRequest::new(
                "python",
                "def run():\n    return 3\nrun()\n",
            ))
            .await
            .expect("execute codemode");
        assert_eq!(result.value, json!(3));
    }

    #[tokio::test]
    async fn loads_a_jit_plugin_through_the_sdk_contract() {
        let runtime = MontyRuntime::new(manager());
        let loaded = runtime
            .load_jit(tower_agent_plugin_sdk::jit::JitRequest::new(
                "python",
                "demo_jit",
                "tool_register(\"demo_jit_tool\", \"run\", \"demo\", {\"type\": \"object\"})\n\
                 def run(args):\n    return \"ok\"\n",
            ))
            .await
            .expect("load JIT plugin");
        assert_eq!(loaded.tools, vec!["demo_jit_tool"]);
        assert!(
            runtime
                .manager()
                .tools()
                .iter()
                .any(|tool| tool.name() == "demo_jit_tool")
        );
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
        let manager = ModManager::new(ui.clone(), PluginRegistry::new()).expect("mod manager");

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

        let text = call_mod_tool(&tool(&manager, "shout"), json!({ "text": "hi" })).await;
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
        let mut registry = PluginRegistry::new();
        registry.register_tool(echo).expect("register echo tool");
        let manager = ModManager::new(UiState::new(), registry).expect("mod manager");

        let code = "\
tool_register(\"shout\", \"shout\", \"uppercase via host echo\", {\"type\": \"object\"})

def shout(args):
    return tool_call(\"echo\", {\"text\": args[\"text\"]}).upper()
";
        manager.write("echoer", code).await.expect("write mod");

        // 目录里同时有宿主工具（echo）与 mod 工具，按名取。
        let mut tools = manager.tools();
        assert!(
            tools.iter().any(|tool| tool.name() == "echo"),
            "宿主工具在目录里"
        );
        let shout = tools
            .iter()
            .position(|tool| tool.name() == "shout")
            .map(|index| tools.remove(index))
            .expect("mod 工具在目录里");
        let text = call_mod_tool(&shout, json!({ "text": "hi" })).await;
        assert_eq!(text, "HI", "mod 通过 tool_call 调用了宿主工具并拿回结果");
    }

    #[tokio::test]
    async fn jit_reload_swaps_the_tool_implementation() {
        let manager = manager();
        let header = "tool_register(\"shout\", \"shout\", \"\", {\"type\": \"object\"})\n\n\
                      def shout(args):\n";

        let first = format!("{header}    return args[\"text\"]\n");
        manager.write("demo", &first).await.expect("install");
        assert_eq!(
            call_mod_tool(&tool(&manager, "shout"), json!({ "text": "hi" })).await,
            "hi"
        );

        let second = format!("{header}    return args[\"text\"].upper()\n");
        manager.reload("demo", &second).await.expect("reload");
        assert_eq!(
            call_mod_tool(&tool(&manager, "shout"), json!({ "text": "hi" })).await,
            "HI",
            "热重载替换了工具实现"
        );
    }

    #[tokio::test]
    async fn warmup_prefills_the_sandbox_pool() {
        let manager = manager();
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

        let manager = manager();
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
        let text = call_mod_tool(&tool(&manager, "read_note"), json!({})).await;
        assert_eq!(text, "hello from mount", "mod 读到了挂载的宿主文件");
    }

    #[tokio::test]
    async fn fs_permission_denies_mounted_reads() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("note.txt"), "secret").expect("write note");

        let manager = manager();
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
        let text = call_mod_tool(&tool(&manager, "read_note"), json!({})).await;
        assert!(
            text.contains("PermissionError") || text.contains("not allowed"),
            "无 fs 权限时应被拒，实际：{text}"
        );
    }

    #[tokio::test]
    async fn exec_uses_the_injected_capability_surface() {
        let manager = manager();
        manager.set_capability_surface(Arc::new(TestExecSurface));

        let code = "\
tool_register(\"run_commands\", \"run_commands\", \"\", {\"type\": \"object\"})

def run_commands(args):
    cat = exec(\"cat\", [\"/work/note.txt\"])
    wc = exec(\"wc\", [\"-l\", \"/work/note.txt\"])
    return cat[\"stdout\"] + wc[\"stdout\"]
";
        manager.write("exec_caller", code).await.expect("write mod");
        let text = call_mod_tool(&tool(&manager, "run_commands"), json!({})).await;
        assert_eq!(text, "hello from exec\n1\n");
    }

    #[tokio::test]
    async fn exec_permission_denial_is_explicit() {
        let manager = manager();
        manager.set_capability_surface(Arc::new(TestExecSurface));

        let code = "\
tool_register(\"run_commands\", \"run_commands\", \"\", {\"type\": \"object\"})

def run_commands(args):
    return exec(\"cat\", [\"/work/note.txt\"])[\"stdout\"]
";
        manager
            .write_with_options(
                "exec_locked",
                code,
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
        let text = call_mod_tool(&tool(&manager, "run_commands"), json!({})).await;
        assert!(text.contains("unauthorized"), "actual: {text}");
        assert!(text.contains("\"capability\":\"exec\""), "actual: {text}");

        let prompter = Arc::new(AllowOncePrompter {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        manager.set_permission_policy(PermissionPolicy::prompting(prompter.clone()));
        let text = call_mod_tool(&tool(&manager, "run_commands"), json!({})).await;
        assert_eq!(text, "hello from exec\n");
        assert_eq!(prompter.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
