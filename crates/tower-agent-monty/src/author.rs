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
use tower_agent_codemode_core::ScriptLanguage;
use tower_agent_core::{AgentTool, AgentToolResult, ToolRegistration};
use tower_agent_storage::ConfigScope;

use crate::manager::{ModManager, ModPermissions, ModSummary, ScriptSummary, ScriptWriteOptions};

/// `codemode_write`：创建/热重载 codemode Python 脚本。
pub const CODEMODE_WRITE: &str = "codemode_write";
/// `codemode_reload`：热重载 codemode Python 脚本。
pub const CODEMODE_RELOAD: &str = "codemode_reload";
/// `codemode_list`：列出 codemode Python 脚本。
pub const CODEMODE_LIST: &str = "codemode_list";
/// `codemode_remove`：卸载 codemode Python 脚本。
pub const CODEMODE_REMOVE: &str = "codemode_remove";
/// `codemode_persist`：确保脚本以启用状态保存到 `ScriptStore`。
pub const CODEMODE_PERSIST: &str = "codemode_persist";
/// `codemode_discard`：从当前项目卸载并删除脚本记录。
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
    /// 可选脚本语言；当前 Python 宿主只接受 `monty`。
    #[serde(default)]
    language: Option<String>,
    /// 是否在启动时加载，缺省为 `true`。
    #[serde(default)]
    enabled: Option<bool>,
    /// 存储作用域：`project`（缺省）或 `user`。
    #[serde(default)]
    scope: Option<String>,
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
    /// 删除作用域：`project`（缺省）或 `user`。
    #[serde(default)]
    scope: Option<String>,
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
            "创建或热重载一个 codemode Python 脚本，并写入当前项目的 ScriptStore：模型生成 Python 代码，代码可调用宿主函数 \
             （exec/tool_call/log/state_*）、注册工具（tool_register）、贡献 UI（ui_status/ui_panel/ui_tool_render）。\
             可用 on(\"tool_call\"|\"tool_result\", handler) 注册脚本 hook。\
             可用 `requires` 声明依赖的宿主能力（如 `tool.read`），`permissions` 收窄权限；\
             `enabled=false` 只保存不加载，`scope=user` 可写用户级脚本。",
            move |params, _request| {
                let manager = Weak::clone(&manager);
                async move {
                    let Some(manager) = manager.upgrade() else {
                        return Ok(host_gone());
                    };
                    let language = match params.language.as_deref() {
                        Some(language) => match ScriptLanguage::parse(language) {
                            Some(language) => Some(language),
                            None => {
                                return Ok(AgentToolResult::error(format!(
                                    "unknown codemode language `{language}`"
                                )));
                            }
                        },
                        None => None,
                    };
                    let scope = match parse_scope(params.scope.as_deref()) {
                        Ok(scope) => scope,
                        Err(error) => return Ok(AgentToolResult::error(error)),
                    };
                    Ok(render(
                        manager
                            .write_script(
                                &params.name,
                                &params.code,
                                ScriptWriteOptions {
                                    permissions: params.permissions,
                                    requires: params.requires,
                                    language,
                                    enabled: params.enabled,
                                    scope,
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
            "热重载一个已存在的 codemode Python 脚本并更新 ScriptStore；新代码校验通过才替换，失败保留旧版本。",
            move |params, _request| {
                let manager = Weak::clone(&manager);
                async move {
                    let Some(manager) = manager.upgrade() else {
                        return Ok(host_gone());
                    };
                    let language = match params.language.as_deref() {
                        Some(language) => match ScriptLanguage::parse(language) {
                            Some(language) => Some(language),
                            None => {
                                return Ok(AgentToolResult::error(format!(
                                    "unknown codemode language `{language}`"
                                )));
                            }
                        },
                        None => None,
                    };
                    let scope = match parse_scope(params.scope.as_deref()) {
                        Ok(scope) => scope,
                        Err(error) => return Ok(AgentToolResult::error(error)),
                    };
                    Ok(render(
                        manager
                            .write_script(
                                &params.name,
                                &params.code,
                                ScriptWriteOptions {
                                    permissions: params.permissions,
                                    requires: params.requires,
                                    language,
                                    enabled: Some(true),
                                    scope,
                                },
                            )
                            .await,
                    ))
                }
            },
        )
    };
    let list = {
        let manager = Weak::clone(&manager);
        ToolRegistration::new::<ListParams, _, _>(
            CODEMODE_LIST,
            "列出当前项目有效的 codemode 脚本；项目脚本覆盖用户同名脚本，并显示启用与加载状态。",
            move |_params, _request| {
                let manager = Weak::clone(&manager);
                async move {
                    let Some(manager) = manager.upgrade() else {
                        return Ok(host_gone());
                    };
                    match manager.list_scripts().await {
                        Ok(summaries) => Ok(AgentToolResult::text(render_list(&summaries))),
                        Err(error) => Ok(AgentToolResult::error(error.to_string())),
                    }
                }
            },
        )
    };
    let remove = {
        let manager = Weak::clone(&manager);
        ToolRegistration::new::<NameParams, _, _>(
            CODEMODE_REMOVE,
            "从指定作用域（缺省为当前项目）删除 codemode 脚本、卸载它并注销其工具。",
            move |params, _request| {
                let manager = Weak::clone(&manager);
                async move {
                    let Some(manager) = manager.upgrade() else {
                        return Ok(host_gone());
                    };
                    let scope = match parse_scope(params.scope.as_deref()) {
                        Ok(scope) => scope,
                        Err(error) => return Ok(AgentToolResult::error(error)),
                    };
                    match manager.remove_script(&params.name, scope).await {
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
            "确保一个已加载的 codemode Python 脚本以启用状态写入当前项目的 ScriptStore。",
            move |params, _request| {
                let manager = Weak::clone(&manager);
                async move {
                    let Some(manager) = manager.upgrade() else {
                        return Ok(host_gone());
                    };
                    let scope = match parse_scope(params.scope.as_deref()) {
                        Ok(scope) => scope,
                        Err(error) => return Ok(AgentToolResult::error(error)),
                    };
                    match manager.persist_script(&params.name, scope).await {
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
            "从当前项目丢弃一个 codemode Python 脚本：删除 ScriptStore 记录并卸载它。",
            move |params, _request| {
                let manager = Weak::clone(&manager);
                async move {
                    let Some(manager) = manager.upgrade() else {
                        return Ok(host_gone());
                    };
                    match manager.discard_script(&params.name).await {
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

fn parse_scope(scope: Option<&str>) -> Result<Option<ConfigScope>, String> {
    match scope {
        None | Some("project") => Ok(None),
        Some("user") => Ok(Some(ConfigScope::User)),
        Some(other) => Err(format!(
            "unknown codemode scope `{other}` (expected `project` or `user`)"
        )),
    }
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

fn render_list(summaries: &[ScriptSummary]) -> String {
    if summaries.is_empty() {
        return "no codemode scripts".to_string();
    }
    summaries
        .iter()
        .map(|summary| {
            let scope = match &summary.scope {
                ConfigScope::User => "user".to_string(),
                ConfigScope::Project(project) => format!("project:{project}"),
            };
            let state = match (summary.enabled, summary.loaded) {
                (true, true) => "enabled, loaded",
                (true, false) => "enabled, not loaded",
                (false, _) => "disabled",
            };
            let tools = if summary.tools.is_empty() {
                String::new()
            } else {
                format!(" → {}", summary.tools.join(", "))
            };
            format!(
                "- {} ({}, {}, {} bytes, {state}){tools}",
                summary.name, summary.language, scope, summary.code_len
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
