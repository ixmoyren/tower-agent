//! 动态插件（stabby `cdylib`）接入：包成注册表载荷，并打通**宿主回调**。
//!
//! 与宿主的交互走 rkyv 归档载荷（结构化、零反序列化）：宿主把 `AbiToolCall` 归档后交给
//! 插件；插件通过 `HostVTable` 回调宿主函数（`tool_call` / `log` / `state_*` / `ui_*` …），
//! 参数与结果同样是归档载荷。
//!
//! 同步/异步桥：插件 `call` 与宿主回调都是同步 `extern "C"`，而宿主能力（`tool_call`）
//! 是 async。这里把插件调用放进 `spawn_blocking`，回调里用 `Handle::block_on` 等异步派发
//! 完成——因此需要**多线程 runtime**（`#[tokio::main(flavor = "multi_thread")]`）。

use std::{
    collections::BTreeSet,
    path::Path,
    sync::Arc,
    task::{Context, Poll},
};

use futures_util::future::BoxFuture;
use serde_json::Value;
use tower::Service;
use tower_agent_core::{AgentError, AgentTool, AgentToolResult, ToolCallRequest, ToolResultStream};
use tower_agent_plugins::{
    AbiToolCall, AbiValue, ArchivedAbiToolResult, ArchivedAbiValue, HostDispatch, LoadedPlugin,
    PluginContext, PluginError, PluginManifest, PluginRegistry, StaticPlugin,
};

use crate::{host::HostCall, manager::ModManager};

/// 一个已加载动态插件的注册表载荷：把它的 `tool.<name>` 能力暴露成工具。
pub struct LoadedPluginPayload {
    loaded: Arc<LoadedPlugin>,
    manifest: PluginManifest,
}

impl StaticPlugin for LoadedPluginPayload {
    fn manifest(&self) -> PluginManifest {
        self.manifest.clone()
    }

    fn install(&self, context: &mut PluginContext) -> Result<(), PluginError> {
        for provided in &self.manifest.provides {
            if provided.id.namespace != "tool" {
                continue;
            }
            let name = provided.id.name.clone();
            let tool = AgentTool::new(
                name.clone(),
                DynamicTool {
                    loaded: Arc::clone(&self.loaded),
                    plugin: self.manifest.name.clone(),
                },
            )
                .label(name.clone())
                .description(format!(
                    "dynamic plugin `{}` capability `tool.{name}`",
                    self.manifest.name
                ))
                .parameters(serde_json::json!({ "type": "object" }));
            context.register_tool(tool);
        }
        Ok(())
    }
}

/// 动态插件工具：把结构化参数归档后调插件，读回归档结果。
#[derive(Clone)]
struct DynamicTool {
    loaded: Arc<LoadedPlugin>,
    plugin: String,
}

impl Service<ToolCallRequest> for DynamicTool {
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        let loaded = Arc::clone(&self.loaded);
        let tool = self.plugin.clone();
        Box::pin(async move {
            let args = Value::Object(request.tool_call.arguments.as_object().clone());
            let call = AbiToolCall {
                id: request.tool_call.id.clone(),
                tool: request.tool_call.name.clone(),
                args: AbiValue::from_json(&args),
            };
            let request_bytes = match rkyv::to_bytes::<rkyv::rancor::Error>(&call) {
                Ok(bytes) => bytes,
                Err(error) => {
                    return Ok(ToolResultStream::once(AgentToolResult::error(format!(
                        "archive request failed: {error}"
                    ))));
                }
            };
            // 插件调用是同步的，放到阻塞线程；回调里再 block_on 宿主派发。
            let response =
                tokio::task::spawn_blocking(move || loaded.call_archived(&request_bytes))
                    .await
                    .map_err(|error| {
                        AgentError::Tool(format!("plugin `{tool}` join failed: {error}"))
                    })?;
            let response = match response {
                Ok(bytes) => bytes,
                Err(error) => {
                    return Ok(ToolResultStream::once(AgentToolResult::error(
                        error.to_string(),
                    )));
                }
            };
            let result = match rkyv::access::<ArchivedAbiToolResult, rkyv::rancor::Error>(&response)
            {
                Ok(result) => result,
                Err(error) => {
                    return Ok(ToolResultStream::once(AgentToolResult::error(format!(
                        "invalid plugin response: {error}"
                    ))));
                }
            };
            let content = result.content.as_str().to_string();
            let outcome = if result.ok {
                AgentToolResult::text(content)
            } else {
                AgentToolResult::error(content)
            };
            Ok(ToolResultStream::once(outcome))
        })
    }
}

/// 插件 -> 宿主的通道桥：回调在阻塞线程发请求，异步任务在 runtime 上派发。
///
/// 这样与 runtime flavor 无关：插件线程阻塞等待时，runtime 仍在推进派发任务。
type HostRequest = (
    String,
    AbiValue,
    tokio::sync::oneshot::Sender<Result<AbiValue, String>>,
);

struct ChannelHost {
    tx: tokio::sync::mpsc::UnboundedSender<HostRequest>,
}

impl HostDispatch for ChannelHost {
    fn call(&self, function: &str, args: &ArchivedAbiValue) -> Result<AbiValue, String> {
        let value = AbiValue::from_archived(args);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send((function.to_string(), value, reply_tx))
            .map_err(|_| "mods host is gone".to_string())?;
        // SAFETY（行为层面）：本方法运行在 `spawn_blocking` 线程上，阻塞是预期的。
        reply_rx
            .blocking_recv()
            .map_err(|_| "mods host is gone".to_string())?
    }
}

/// 为某个插件建通道桥并启动异步派发任务；任务在通道关闭（插件释放）后自然结束。
fn host_bridge(
    manager: &Arc<ModManager>,
    plugin: &str,
    handle: &tokio::runtime::Handle,
) -> Arc<dyn HostDispatch> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<HostRequest>();
    let manager = Arc::clone(manager);
    let plugin = plugin.to_string();
    handle.spawn(async move {
        while let Some((function, args, reply)) = rx.recv().await {
            let result = manager
                .dispatch(host_call(function, args, plugin.clone()))
                .await
                .map(|value| AbiValue::from_json(&value));
            let _ = reply.send(result);
        }
    });
    Arc::new(ChannelHost { tx })
}

/// 把插件的结构化调用转成宿主的 `HostCall`（对象参数 -> 关键字参数）。
fn host_call(function: String, args: AbiValue, plugin: String) -> HostCall {
    let json = args.to_json();
    let (positional, kwargs) = match json {
        Value::Object(map) => (Vec::new(), map),
        other => (vec![other], serde_json::Map::new()),
    };
    HostCall {
        function,
        args: positional,
        kwargs: kwargs.into_iter().collect(),
        setup: false,
        plugin,
    }
}

/// 从目录加载动态库（`.so` / `.dll` / `.dylib`）并注册进注册表。返回加载的插件名。
///
/// # Safety
///
/// 目录里的库会被 dlopen 并执行，调用方必须保证它们可信。
#[allow(unsafe_code)]
pub unsafe fn load_from_dir(
    registry: &mut PluginRegistry,
    dir: impl AsRef<Path>,
    manager: &Arc<ModManager>,
) -> Result<Vec<String>, PluginError> {
    let dir = dir.as_ref();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let handle = tokio::runtime::Handle::try_current().ok();
    let mut loaded = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_library(&path) {
            continue;
        }
        // 加载器先读清单，再按插件名构造宿主回调表（有 runtime 才建桥）。
        let plugin = unsafe {
            LoadedPlugin::load(&path, |manifest| {
                handle
                    .as_ref()
                    .map(|handle| host_bridge(manager, &manifest.name, handle))
            })
        }?;
        let manifest = plugin.manifest().clone();
        let name = manifest.name.clone();
        if !seen.insert(name.clone()) {
            continue;
        }
        // 受保护的静态插件（monty 自己的地基、以及 wasmtime / codemode 这类宿主能力）
        // **永不被遮蔽**：动态插件拿同名来顶替能力 → 像 JIT 那条路一样**响亮报错**，
        // 而不是静默进入 `Resolution::shadows()`。
        registry.ensure_not_protected(&name)?;
        registry.register_plugin(Arc::new(LoadedPluginPayload {
            loaded: Arc::new(plugin),
            manifest,
        }))?;
        loaded.push(name);
    }
    Ok(loaded)
}

fn is_library(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("so" | "dll" | "dylib")
    )
}
