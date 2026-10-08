//! 动态插件（stabby `cdylib`）接入：把已加载的动态插件包装成注册表载荷。
//!
//! 动态插件的 ABI 目前是「字符串进 / 字符串出」且无法回调宿主，因此这里把它的每个
//! `tool.<name>` 能力包装成一个 [`AgentTool`]：参数序列化成 JSON 调 `call`，返回值当
//! 作结果文本。它因此也能被更高优先级的 JIT mod 遮蔽（形态优先级 静态 < 动态 < JIT）。

use std::{
    collections::BTreeSet,
    path::Path,
    sync::Arc,
    task::{Context, Poll},
};

use futures_util::future::BoxFuture;
use tower::Service;
use tower_agent_core::{AgentError, AgentTool, AgentToolResult, ToolCallRequest, ToolResultStream};
use tower_agent_plugins::{
    PluginContext, PluginError, PluginManifest, PluginRegistry, StaticPlugin, dynamic::LoadedPlugin,
};

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

/// 动态插件工具：`call(json_args) -> text`。
#[derive(Clone)]
struct DynamicTool {
    loaded: Arc<LoadedPlugin>,
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
        Box::pin(async move {
            let arguments =
                serde_json::Value::Object(request.tool_call.arguments.as_object().clone());
            let result = match loaded.call(&arguments.to_string()) {
                Ok(text) => AgentToolResult::text(text),
                Err(error) => AgentToolResult::error(error.to_string()),
            };
            Ok(ToolResultStream::once(result))
        })
    }
}

/// 从目录加载动态库（`.so` / `.dll` / `.dylib`）并注册进注册表。返回加载的插件名。
///
/// # Safety
///
/// 目录里的库会被 dlopen 并执行，调用方必须保证它们可信。
pub unsafe fn load_from_dir(
    registry: &mut PluginRegistry,
    dir: impl AsRef<Path>,
) -> Result<Vec<String>, PluginError> {
    let dir = dir.as_ref();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut loaded = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_library(&path) {
            continue;
        }
        let plugin = unsafe { LoadedPlugin::load(&path) }?;
        let manifest = plugin.manifest().clone();
        if !seen.insert(manifest.name.clone()) {
            continue;
        }
        let name = manifest.name.clone();
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
