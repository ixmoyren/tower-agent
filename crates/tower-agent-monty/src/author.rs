//! 暴露给 LLM 的 mods 作者工具（cc `mods` 的作者闭环）。
//!
//! 模型自己生成 monty Python 代码，通过 `mod_write` 校验/加载；mod 再用宿主函数
//! 调能力（`tool_call`）、提供工具（`tool_register`）、改界面（`ui_*`）。

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_agent_core::{AgentTool, AgentToolResult, ToolRegistration};

use crate::manager::{ModManager, ModOptions, ModPermissions, ModSummary};

/// `mod_write`：创建/热重载 mod。
pub const MOD_WRITE: &str = "mod_write";
/// `mod_reload`：热重载 mod。
pub const MOD_RELOAD: &str = "mod_reload";
/// `mod_list`：列出 mod。
pub const MOD_LIST: &str = "mod_list";
/// `mod_remove`：卸载 mod。
pub const MOD_REMOVE: &str = "mod_remove";
/// `mod_persist`：把 mod 持久化到 `dirs` 的 config 目录。
pub const MOD_PERSIST: &str = "mod_persist";
/// `mod_discard`：丢弃 mod（卸载 + 删除持久化/临时副本）。
pub const MOD_DISCARD: &str = "mod_discard";

/// JIT 运行时自带的作者工具名（保留，不允许被 JIT 插件遮蔽）。
pub const AUTHORING_TOOL_NAMES: [&str; 6] = [
    MOD_WRITE,
    MOD_RELOAD,
    MOD_LIST,
    MOD_REMOVE,
    MOD_PERSIST,
    MOD_DISCARD,
];

/// `mod_write` / `mod_reload` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
struct WriteParams {
    /// mod 名（唯一）。
    name: String,
    /// monty Python 源码。
    code: String,
    /// 可选权限；缺省 = 默认权限（工具/UI/状态/FS 全开，FS 可写范围由挂载模式决定）。
    #[serde(default)]
    permissions: Option<ModPermissions>,
    /// 可选依赖的宿主能力，形如 `tool.read`；进注册表做依赖校验与拓扑排序。
    #[serde(default)]
    requires: Vec<String>,
}

/// 只带名字的参数。
#[derive(Debug, Deserialize, JsonSchema)]
struct NameParams {
    /// mod 名。
    name: String,
}

/// 无参数。
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct ListParams {}

/// 构造一组作者工具：`mod_write` / `mod_reload` / `mod_list` / `mod_remove`。
#[must_use]
pub fn authoring_tools(manager: Arc<ModManager>) -> Vec<AgentTool> {
    let write = {
        let manager = manager.clone();
        ToolRegistration::new::<WriteParams, _, _>(
            MOD_WRITE,
            "创建或热重载一个 monty mod：模型生成 Python 代码，代码可调用宿主函数 \
             （tool_call/log/state_*）、注册工具（tool_register）、贡献 UI（ui_status/ui_panel/ui_tool_render）。\
             可用 `requires` 声明依赖的宿主能力（如 `tool.read`），`permissions` 收窄权限。",
            move |params, _request| {
                let manager = manager.clone();
                async move {
                    Ok(render(
                        manager
                            .write_with_options(
                                &params.name,
                                &params.code,
                                ModOptions {
                                    permissions: params.permissions,
                                    requires: params.requires,
                                },
                            )
                            .await,
                    ))
                }
            },
        )
    };
    let reload = {
        let manager = manager.clone();
        ToolRegistration::new::<WriteParams, _, _>(
            MOD_RELOAD,
            "热重载一个已存在的 mod；新代码校验通过才替换，失败保留旧版本。",
            move |params, _request| {
                let manager = manager.clone();
                async move { Ok(render(manager.reload(&params.name, &params.code).await)) }
            },
        )
    };
    let list = {
        let manager = manager.clone();
        ToolRegistration::new::<ListParams, _, _>(
            MOD_LIST,
            "列出当前已加载的 mod 及其提供的工具。",
            move |_params, _request| {
                let manager = manager.clone();
                async move {
                    let summaries = manager.list();
                    Ok(AgentToolResult::text(render_list(&summaries)))
                }
            },
        )
    };
    let remove = {
        let manager = manager.clone();
        ToolRegistration::new::<NameParams, _, _>(
            MOD_REMOVE,
            "卸载一个 mod，并注销它注册的工具。",
            move |params, _request| {
                let manager = manager.clone();
                async move {
                    match manager.remove(&params.name) {
                        Ok(true) => Ok(AgentToolResult::text(format!(
                            "removed mod `{}`",
                            params.name
                        ))),
                        Ok(false) => Ok(AgentToolResult::error(format!(
                            "unknown mod `{}`",
                            params.name
                        ))),
                        Err(error) => Ok(AgentToolResult::error(error.to_string())),
                    }
                }
            },
        )
    };
    let persist = {
        let manager = manager.clone();
        ToolRegistration::new::<NameParams, _, _>(
            MOD_PERSIST,
            "把一个 mod 持久化到用户配置目录（跨会话保留）。",
            move |params, _request| {
                let manager = manager.clone();
                async move {
                    match manager.persist(&params.name) {
                        Ok(()) => Ok(AgentToolResult::text(format!(
                            "persisted mod `{}`",
                            params.name
                        ))),
                        Err(error) => Ok(AgentToolResult::error(error.to_string())),
                    }
                }
            },
        )
    };
    let discard = {
        let manager = manager.clone();
        ToolRegistration::new::<NameParams, _, _>(
            MOD_DISCARD,
            "丢弃一个 mod：卸载它并删除持久化/临时副本。",
            move |params, _request| {
                let manager = manager.clone();
                async move {
                    match manager.discard(&params.name) {
                        Ok(true) => Ok(AgentToolResult::text(format!(
                            "discarded mod `{}`",
                            params.name
                        ))),
                        Ok(false) => Ok(AgentToolResult::error(format!(
                            "unknown mod `{}`",
                            params.name
                        ))),
                        Err(error) => Ok(AgentToolResult::error(error.to_string())),
                    }
                }
            },
        )
    };
    vec![write, reload, list, remove, persist, discard]
}

/// 把写/重载结果渲染成模型可见文本。
fn render(result: Result<ModSummary, tower_agent_plugins::PluginError>) -> AgentToolResult {
    match result {
        Ok(summary) => AgentToolResult::text(render_summary(&summary)),
        Err(error) => AgentToolResult::error(error.to_string()),
    }
}

fn render_summary(summary: &ModSummary) -> String {
    if summary.tools.is_empty() {
        format!("loaded mod `{}` (no tools registered yet)", summary.name)
    } else {
        format!(
            "loaded mod `{}` providing tools: {}",
            summary.name,
            summary.tools.join(", ")
        )
    }
}

fn render_list(summaries: &[ModSummary]) -> String {
    if summaries.is_empty() {
        return "no mods loaded".to_string();
    }
    summaries
        .iter()
        .map(|summary| {
            if summary.tools.is_empty() {
                format!("- {} ({} bytes)", summary.name, summary.code_len)
            } else {
                format!(
                    "- {} ({} bytes) → {}",
                    summary.name,
                    summary.code_len,
                    summary.tools.join(", ")
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
