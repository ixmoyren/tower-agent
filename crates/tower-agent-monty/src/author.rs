//! 暴露给 LLM 的 codemode Python 作者工具。
//!
//! 模型自己生成 monty Python 代码，通过 `codemode_write` 校验/加载；脚本再用宿主函数
//! 调能力（`tool_call`）、提供工具（`tool_register`）、改界面（`ui_*`）。
//!
//! 这些工具由 **JIT 运行时地基插件**（protected）在 `install` 时装进工具目录，因此
//! 和脚本工具同表。工具句柄用 [`Weak`] 指向 `ModManager`，避免
//! 「管理器 → 注册表 → 地基插件 → 工具 → 管理器」的强引用环。

use std::sync::Weak;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_agent_core::{AgentTool, AgentToolResult, ToolRegistration};

use crate::manager::{ModManager, ModOptions, ModPermissions, ModSummary};

/// `codemode_write`：创建/热重载 codemode Python 脚本。
pub const CODEMODE_WRITE: &str = "codemode_write";
/// `codemode_reload`：热重载 codemode Python 脚本。
pub const CODEMODE_RELOAD: &str = "codemode_reload";
/// `codemode_list`：列出 codemode Python 脚本。
pub const CODEMODE_LIST: &str = "codemode_list";
/// `codemode_remove`：卸载 codemode Python 脚本。
pub const CODEMODE_REMOVE: &str = "codemode_remove";
/// `codemode_persist`：把脚本持久化到 `dirs` 的 config 目录。
pub const CODEMODE_PERSIST: &str = "codemode_persist";
/// `codemode_discard`：丢弃脚本（卸载 + 删除持久化/临时副本）。
pub const CODEMODE_DISCARD: &str = "codemode_discard";

/// JIT 运行时自带的作者工具名（保留，不允许被 JIT 插件遮蔽）。
pub const AUTHORING_TOOL_NAMES: [&str; 6] = [
    CODEMODE_WRITE,
    CODEMODE_RELOAD,
    CODEMODE_LIST,
    CODEMODE_REMOVE,
    CODEMODE_PERSIST,
    CODEMODE_DISCARD,
];

/// `codemode_write` / `codemode_reload` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
struct WriteParams {
    /// 脚本名（唯一）。
    name: String,
    /// monty Python 源码。
    code: String,
    /// 可选权限；缺省 = 默认权限（命令/工具/UI/状态/FS 全开，FS 可写范围由挂载模式决定）。
    #[serde(default)]
    permissions: Option<ModPermissions>,
    /// 可选依赖的宿主能力，形如 `tool.read`；进注册表做依赖校验与拓扑排序。
    #[serde(default)]
    requires: Vec<String>,
}

/// 只带名字的参数。
#[derive(Debug, Deserialize, JsonSchema)]
struct NameParams {
    /// 脚本名。
    name: String,
}

/// 无参数。
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct ListParams {}

/// 构造一组作者工具（由地基插件 `install`）：`codemode_write` / `codemode_reload` /
/// `codemode_list` / `codemode_remove` / `codemode_persist` / `codemode_discard`。
#[must_use]
pub(crate) fn authoring_tools(manager: Weak<ModManager>) -> Vec<AgentTool> {
    let write = {
        let manager = Weak::clone(&manager);
        ToolRegistration::new::<WriteParams, _, _>(
            CODEMODE_WRITE,
            "创建或热重载一个 codemode Python 脚本：模型生成 Python 代码，代码可调用宿主函数 \
             （exec/tool_call/log/state_*）、注册工具（tool_register）、贡献 UI（ui_status/ui_panel/ui_tool_render）。\
             可用 `requires` 声明依赖的宿主能力（如 `tool.read`），`permissions` 收窄权限。",
            move |params, _request| {
                let manager = Weak::clone(&manager);
                async move {
                    let Some(manager) = manager.upgrade() else {
                        return Ok(host_gone());
                    };
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
        let manager = Weak::clone(&manager);
        ToolRegistration::new::<WriteParams, _, _>(
            CODEMODE_RELOAD,
            "热重载一个已存在的 codemode Python 脚本；新代码校验通过才替换，失败保留旧版本。",
            move |params, _request| {
                let manager = Weak::clone(&manager);
                async move {
                    let Some(manager) = manager.upgrade() else {
                        return Ok(host_gone());
                    };
                    Ok(render(manager.reload(&params.name, &params.code).await))
                }
            },
        )
    };
    let list = {
        let manager = Weak::clone(&manager);
        ToolRegistration::new::<ListParams, _, _>(
            CODEMODE_LIST,
            "列出当前已加载的 codemode Python 脚本及其提供的工具。",
            move |_params, _request| {
                let manager = Weak::clone(&manager);
                async move {
                    let Some(manager) = manager.upgrade() else {
                        return Ok(host_gone());
                    };
                    Ok(AgentToolResult::text(render_list(&manager.list())))
                }
            },
        )
    };
    let remove = {
        let manager = Weak::clone(&manager);
        ToolRegistration::new::<NameParams, _, _>(
            CODEMODE_REMOVE,
            "卸载一个 codemode Python 脚本，并注销它注册的工具。",
            move |params, _request| {
                let manager = Weak::clone(&manager);
                async move {
                    let Some(manager) = manager.upgrade() else {
                        return Ok(host_gone());
                    };
                    match manager.remove(&params.name) {
                        Ok(true) => Ok(AgentToolResult::text(format!(
                            "removed codemode script `{}`",
                            params.name
                        ))),
                        Ok(false) => Ok(AgentToolResult::error(format!(
                            "unknown codemode script `{}`",
                            params.name
                        ))),
                        Err(error) => Ok(AgentToolResult::error(error.to_string())),
                    }
                }
            },
        )
    };
    let persist = {
        let manager = Weak::clone(&manager);
        ToolRegistration::new::<NameParams, _, _>(
            CODEMODE_PERSIST,
            "把一个 codemode Python 脚本持久化到用户配置目录（跨会话保留）。",
            move |params, _request| {
                let manager = Weak::clone(&manager);
                async move {
                    let Some(manager) = manager.upgrade() else {
                        return Ok(host_gone());
                    };
                    match manager.persist(&params.name) {
                        Ok(()) => Ok(AgentToolResult::text(format!(
                            "persisted codemode script `{}`",
                            params.name
                        ))),
                        Err(error) => Ok(AgentToolResult::error(error.to_string())),
                    }
                }
            },
        )
    };
    let discard = {
        let manager = Weak::clone(&manager);
        ToolRegistration::new::<NameParams, _, _>(
            CODEMODE_DISCARD,
            "丢弃一个 codemode Python 脚本：卸载它并删除持久化/临时副本。",
            move |params, _request| {
                let manager = Weak::clone(&manager);
                async move {
                    let Some(manager) = manager.upgrade() else {
                        return Ok(host_gone());
                    };
                    match manager.discard(&params.name) {
                        Ok(true) => Ok(AgentToolResult::text(format!(
                            "discarded codemode script `{}`",
                            params.name
                        ))),
                        Ok(false) => Ok(AgentToolResult::error(format!(
                            "unknown codemode script `{}`",
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

fn host_gone() -> AgentToolResult {
    AgentToolResult::error("codemode Python host is gone")
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
        format!(
            "loaded codemode script `{}` (no tools registered yet)",
            summary.name
        )
    } else {
        format!(
            "loaded codemode script `{}` providing tools: {}",
            summary.name,
            summary.tools.join(", ")
        )
    }
}

fn render_list(summaries: &[ModSummary]) -> String {
    if summaries.is_empty() {
        return "no codemode Python scripts loaded".to_string();
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
