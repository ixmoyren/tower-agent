//! mods 宿主：管理一组 monty 插件，派发宿主能力调用，并把 mod 提供的工具
//! 动态注册进 [`HostApi`]。
//!
//! 三条能力轴都在这里落地：
//!
//! - **调宿主**：`tool_call` / `log` / `state_*`；
//! - **提供工具**：`tool_register` / `tool_unregister`（运行时生效）；
//! - **改 UI**：`ui_status` / `ui_panel` / `ui_clear` / `ui_tool_render`，写进共享的
//!   [`UiState`]，由各前端消费。

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};

use futures_util::{StreamExt as _, future::BoxFuture};
use semver::Version;
use serde_json::{Value, json};
use tower_agent_core::{
    AgentTool, AgentToolResult, Panel, PanelAnchor, StatusSegment, ToolRenderer, UiState,
};
use tower_agent_plugins::{HostApi, Permission, PluginError, PluginKind, PluginManifest};
use tower_agent_provider::{Arguments, ToolCall};

use crate::{
    host::{HostCall, MontyHost},
    plugin::MontyPlugin,
    tool::{ModTool, ToolSpec},
};

/// 一个已加载 mod 的摘要（用于 `mod_list` / UI 展示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModSummary {
    /// mod 名。
    pub name: String,
    /// 该 mod 当前提供的工具名（按字典序）。
    pub tools: Vec<String>,
    /// 源码字节数。
    pub code_len: usize,
}

/// mods 宿主。
pub struct ModManager {
    worker: PathBuf,
    host: HostApi,
    ui: UiState,
    mods: Mutex<BTreeMap<String, Arc<MontyPlugin>>>,
    tools: Mutex<BTreeMap<String, Vec<ToolSpec>>>,
    kv: Mutex<BTreeMap<String, Value>>,
}

impl std::fmt::Debug for ModManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModManager")
            .field("worker", &self.worker)
            .field("mods", &self.list())
            .finish_non_exhaustive()
    }
}

impl ModManager {
    /// 用 worker 二进制路径、宿主能力表与共享 UI 状态构造。
    #[must_use]
    pub fn new(worker: impl Into<PathBuf>, host: HostApi, ui: UiState) -> Arc<Self> {
        Arc::new(Self {
            worker: worker.into(),
            host,
            ui,
            mods: Mutex::new(BTreeMap::new()),
            tools: Mutex::new(BTreeMap::new()),
            kv: Mutex::new(BTreeMap::new()),
        })
    }

    /// 共享的 UI 状态（前端据此渲染 mod 贡献）。
    #[must_use]
    pub fn ui(&self) -> UiState {
        self.ui.clone()
    }

    /// 宿主能力表（含内置工具与已注册的 mod 工具）。
    #[must_use]
    pub fn host_api(&self) -> &HostApi {
        &self.host
    }

    /// 当前所有 mod 提供的工具。
    #[must_use]
    pub fn tools(&self) -> Vec<AgentTool> {
        let specs: Vec<(String, ToolSpec)> = self
            .tools
            .lock()
            .expect("mod tools poisoned")
            .iter()
            .flat_map(|(name, specs)| specs.iter().map(|spec| (name.clone(), spec.clone())))
            .collect();
        specs
            .into_iter()
            .filter_map(|(name, spec)| {
                let plugin = self
                    .mods
                    .lock()
                    .expect("mods poisoned")
                    .get(&name)
                    .cloned()?;
                Some(ModTool::new(plugin, spec).into_agent_tool())
            })
            .collect()
    }

    /// 当前所有 mod 的摘要（按名字排序）。
    #[must_use]
    pub fn list(&self) -> Vec<ModSummary> {
        let mods = self.mods.lock().expect("mods poisoned");
        let tools = self.tools.lock().expect("mod tools poisoned");
        mods.iter()
            .map(|(name, plugin)| {
                let mut tool_names: Vec<String> = tools
                    .get(name)
                    .map(|specs| specs.iter().map(|spec| spec.name.clone()).collect())
                    .unwrap_or_default();
                tool_names.sort();
                ModSummary {
                    name: name.clone(),
                    tools: tool_names,
                    code_len: plugin.code().len(),
                }
            })
            .collect()
    }

    /// 写一个 mod：不存在则创建，已存在则热重载。
    pub async fn write(
        self: &Arc<Self>,
        name: &str,
        code: &str,
    ) -> Result<ModSummary, PluginError> {
        if self.mods.lock().expect("mods poisoned").contains_key(name) {
            return self.reload(name, code).await;
        }
        self.install(name, code).await
    }

    /// 热重载一个已存在的 mod（失败保留旧版本）。
    pub async fn reload(
        self: &Arc<Self>,
        name: &str,
        code: &str,
    ) -> Result<ModSummary, PluginError> {
        let plugin = self
            .mods
            .lock()
            .expect("mods poisoned")
            .get(name)
            .cloned()
            .ok_or_else(|| mod_error(name, "unknown mod"))?;

        // 先清空旧工具声明；失败时恢复，成功时用新声明重建。
        let old_specs = self.take_specs(name);
        if let Err(error) = plugin.reload_checked(code).await {
            self.push_specs(name, old_specs);
            return Err(error);
        }
        for spec in &old_specs {
            self.host.remove_tool(&spec.name);
        }
        self.refresh_host_tools(name)?;
        Ok(self.summary(name))
    }

    /// 移除一个 mod：注销它的工具并卸载插件。返回是否真的移除了。
    pub fn remove(&self, name: &str) -> Result<bool, PluginError> {
        let plugin = self.mods.lock().expect("mods poisoned").remove(name);
        let Some(plugin) = plugin else {
            return Ok(false);
        };
        for spec in self.take_specs(name) {
            self.host.remove_tool(&spec.name);
        }
        let _ = plugin.stop();
        let _ = plugin.unload();
        Ok(true)
    }

    /// 创建并安装一个新 mod（内部：构造 → `activate` → 注册工具）。
    async fn install(self: &Arc<Self>, name: &str, code: &str) -> Result<ModSummary, PluginError> {
        let manifest = PluginManifest::new(name, Version::new(1, 0, 0), PluginKind::Jit);
        let adapter = Arc::new(ModHostAdapter {
            manager: Arc::downgrade(self),
            plugin: name.to_string(),
            permissions: manifest.permissions.clone(),
        });
        let plugin =
            Arc::new(MontyPlugin::load(manifest, code, self.worker.clone(), adapter).await?);
        self.take_specs(name);
        if let Err(error) = plugin.activate().await {
            // 激活失败：丢掉半途注册的工具声明，保持宿主干净。
            self.take_specs(name);
            return Err(error);
        }
        self.mods
            .lock()
            .expect("mods poisoned")
            .insert(name.to_string(), plugin);
        self.refresh_host_tools(name)?;
        Ok(self.summary(name))
    }

    /// 按 `tools[name]` 重建 host 里的工具能力。
    fn refresh_host_tools(&self, name: &str) -> Result<(), PluginError> {
        let specs = self
            .tools
            .lock()
            .expect("mod tools poisoned")
            .get(name)
            .cloned()
            .unwrap_or_default();
        let plugin = self.mods.lock().expect("mods poisoned").get(name).cloned();
        let Some(plugin) = plugin else {
            return Ok(());
        };
        for spec in specs {
            let tool = ModTool::new(plugin.clone(), spec).into_agent_tool();
            self.host.register_tool(tool);
        }
        Ok(())
    }

    fn take_specs(&self, name: &str) -> Vec<ToolSpec> {
        self.tools
            .lock()
            .expect("mod tools poisoned")
            .remove(name)
            .unwrap_or_default()
    }

    fn push_specs(&self, name: &str, specs: Vec<ToolSpec>) {
        self.tools
            .lock()
            .expect("mod tools poisoned")
            .insert(name.to_string(), specs);
    }

    fn summary(&self, name: &str) -> ModSummary {
        self.list()
            .into_iter()
            .find(|summary| summary.name == name)
            .unwrap_or(ModSummary {
                name: name.to_string(),
                tools: Vec::new(),
                code_len: 0,
            })
    }

    /// 派发一次宿主调用（由 [`ModHostAdapter`] 调用）。
    async fn dispatch(&self, call: HostCall, permissions: &[Permission]) -> Result<Value, String> {
        match call.function.as_str() {
            "tool_call" => self.host_tool_call(&call, permissions).await,
            "tool_list" => Ok(self.tool_names()),
            "tool_register" => self.host_tool_register(&call),
            "tool_unregister" => self.host_tool_unregister(&call),
            "log" => {
                let message = arg_str(&call, 0, "message").unwrap_or_default();
                eprintln!("[mod {}] {message}", call.plugin);
                Ok(Value::Null)
            }
            "state_set" => {
                let key = arg_str(&call, 0, "key").ok_or("state_set requires (key, value)")?;
                let value = call
                    .positional_or_kwarg(1, "value")
                    .cloned()
                    .unwrap_or(Value::Null);
                self.kv
                    .lock()
                    .expect("mod kv poisoned")
                    .insert(key.to_string(), value);
                Ok(Value::Null)
            }
            "state_get" => {
                let key = arg_str(&call, 0, "key").ok_or("state_get requires (key)")?;
                Ok(self
                    .kv
                    .lock()
                    .expect("mod kv poisoned")
                    .get(key)
                    .cloned()
                    .unwrap_or(Value::Null))
            }
            "ui_status" => self.host_ui_status(&call),
            "ui_panel" => self.host_ui_panel(&call),
            "ui_clear" => self.host_ui_clear(&call),
            "ui_tool_render" => self.host_ui_tool_render(&call),
            "ui_state" => serde_json::to_value(self.ui.snapshot()).map_err(|e| e.to_string()),
            other => Err(format!("unknown host function `{other}`")),
        }
    }

    fn tool_names(&self) -> Value {
        let mut names = self.host.capabilities();
        names.sort();
        json!(names)
    }

    async fn host_tool_call(
        &self,
        call: &HostCall,
        permissions: &[Permission],
    ) -> Result<Value, String> {
        let name = arg_str(call, 0, "name").ok_or("tool_call requires (name, args)")?;
        let arguments = match call.positional_or_kwarg(1, "args") {
            Some(Value::Object(object)) => object.clone(),
            Some(Value::Null) | None => serde_json::Map::new(),
            Some(other) => return Err(format!("tool_call args must be an object, got {other}")),
        };
        let request = tower_agent_core::ToolCallRequest::new(ToolCall {
            id: format!("mod:{}:{name}", call.plugin),
            name: name.to_string(),
            arguments: Arguments::from(arguments),
            thought_signature: None,
            namespace: None,
        });
        let stream = self
            .host
            .call_tool_with(name, request, permissions)
            .await
            .map_err(|error| error.to_string())?;
        let mut stream = stream;
        let mut last: Option<AgentToolResult> = None;
        while let Some(result) = stream.next().await {
            last = Some(result);
        }
        let result = last.ok_or_else(|| format!("tool `{name}` produced no result"))?;
        if result.is_error {
            return Err(result_text(&result));
        }
        Ok(Value::String(result_text(&result)))
    }

    fn host_tool_register(&self, call: &HostCall) -> Result<Value, String> {
        if !call.setup {
            // 非安装期调用（每次工具调用都会重跑脚本）直接忽略，避免重复注册。
            return Ok(Value::Null);
        }
        let name = arg_str(call, 0, "name").ok_or("tool_register requires (name, function)")?;
        let function =
            arg_str(call, 1, "function").ok_or("tool_register requires (name, function)")?;
        let description = arg_str(call, 2, "description").unwrap_or_default();
        let parameters = call
            .positional_or_kwarg(3, "parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" }));
        let spec = ToolSpec {
            name: name.to_string(),
            function: function.to_string(),
            description: description.to_string(),
            parameters,
        };
        let mut tools = self.tools.lock().expect("mod tools poisoned");
        let entry = tools.entry(call.plugin.clone()).or_default();
        entry.retain(|existing| existing.name != spec.name);
        entry.push(spec);
        Ok(Value::Null)
    }

    fn host_tool_unregister(&self, call: &HostCall) -> Result<Value, String> {
        if !call.setup {
            return Ok(Value::Null);
        }
        let name = arg_str(call, 0, "name").ok_or("tool_unregister requires (name)")?;
        let mut tools = self.tools.lock().expect("mod tools poisoned");
        if let Some(entry) = tools.get_mut(&call.plugin) {
            entry.retain(|spec| spec.name != name);
        }
        Ok(Value::Null)
    }

    fn host_ui_status(&self, call: &HostCall) -> Result<Value, String> {
        let id = arg_str(call, 0, "id").ok_or("ui_status requires (id, text)")?;
        let text = arg_str(call, 1, "text").ok_or("ui_status requires (id, text)")?;
        let color = arg_str(call, 2, "color").map(str::to_string);
        let priority = call
            .positional_or_kwarg(3, "priority")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        self.ui.set_status(StatusSegment {
            id: id.to_string(),
            text: text.to_string(),
            color,
            priority,
        });
        Ok(Value::Null)
    }

    fn host_ui_panel(&self, call: &HostCall) -> Result<Value, String> {
        let id = arg_str(call, 0, "id").ok_or("ui_panel requires (id, title, lines)")?;
        let title = arg_str(call, 1, "title").unwrap_or_default();
        let lines = call
            .positional_or_kwarg(2, "lines")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| match item {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
            })
            .ok_or("ui_panel requires lines as an array of strings")?;
        let mut panel = Panel::new(id, title, lines);
        if let Some(anchor) = arg_str(call, 3, "anchor") {
            panel.anchor = PanelAnchor::parse(anchor);
        }
        panel.width = call
            .positional_or_kwarg(4, "width")
            .and_then(Value::as_u64)
            .map(|value| value as u16);
        panel.height = call
            .positional_or_kwarg(5, "height")
            .and_then(Value::as_u64)
            .map(|value| value as u16);
        self.ui.set_panel(panel);
        Ok(Value::Null)
    }

    fn host_ui_clear(&self, call: &HostCall) -> Result<Value, String> {
        match arg_str(call, 0, "id") {
            Some(id) => {
                self.ui.remove_panel(id);
                self.ui.remove_status(id);
            }
            None => self.ui.clear(),
        }
        Ok(Value::Null)
    }

    fn host_ui_tool_render(&self, call: &HostCall) -> Result<Value, String> {
        let tool = arg_str(call, 0, "tool").ok_or("ui_tool_render requires (tool, lines)")?;
        let lines = call
            .positional_or_kwarg(1, "lines")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| match item {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
            })
            .ok_or("ui_tool_render requires lines as an array of strings")?;
        self.ui
            .set_tool_renderer(tool.to_string(), ToolRenderer::new(lines));
        Ok(Value::Null)
    }
}

/// 每个 mod 的宿主适配器：把 [`HostCall`] 转交给 [`ModManager::dispatch`]。
struct ModHostAdapter {
    manager: Weak<ModManager>,
    plugin: String,
    permissions: Vec<Permission>,
}

impl MontyHost for ModHostAdapter {
    fn dispatch(&self, call: HostCall) -> BoxFuture<'static, Result<Value, String>> {
        let manager = self.manager.upgrade();
        let plugin = self.plugin.clone();
        let permissions = self.permissions.clone();
        Box::pin(async move {
            let Some(manager) = manager else {
                return Err(format!("mod `{plugin}` host is gone"));
            };
            manager.dispatch(call, &permissions).await
        })
    }
}

/// 工具结果的文本内容（拼接所有文本块）。
fn result_text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .map(|block| match block {
            tower_agent_provider::ContentBlock::Text(text) => text.text.clone(),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 位置参数或关键字参数里的字符串。
fn arg_str<'a>(call: &'a HostCall, index: usize, name: &str) -> Option<&'a str> {
    call.positional_or_kwarg(index, name)
        .and_then(Value::as_str)
}

fn mod_error(name: &str, message: impl Into<String>) -> PluginError {
    PluginError::Load {
        plugin: name.to_string(),
        message: message.into(),
    }
}

/// 便捷：用 worker 路径构造一个 mods 宿主。
#[must_use]
pub fn manager_with_worker(
    worker: impl AsRef<Path>,
    host: HostApi,
    ui: UiState,
) -> Arc<ModManager> {
    ModManager::new(worker.as_ref().to_path_buf(), host, ui)
}
