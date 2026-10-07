//! 暴露给 LLM 的 mods 作者工具（cc `mods` 的作者闭环）。
//!
//! 模型自己生成 monty Python 代码，通过 `mod_write` 校验/加载；mod 再用宿主函数
//! 调能力（`tool_call`）、提供工具（`tool_register`）、改界面（`ui_*`）。

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_agent_core::{AgentTool, AgentToolResult, ToolRegistration};

use crate::manager::{ModManager, ModSummary};

/// `mod_write` / `mod_reload` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
struct WriteParams {
    /// mod 名（唯一）。
    name: String,
    /// monty Python 源码。
    code: String,
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
            "mod_write",
            "创建或热重载一个 monty mod：模型生成 Python 代码，代码可调用宿主函数 \
             （tool_call/log/state_*）、注册工具（tool_register）、贡献 UI（ui_status/ui_panel/ui_tool_render）。",
            move |params, _request| {
                let manager = manager.clone();
                async move { Ok(render(manager.write(&params.name, &params.code).await)) }
            },
        )
    };
    let reload = {
        let manager = manager.clone();
        ToolRegistration::new::<WriteParams, _, _>(
            "mod_reload",
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
            "mod_list",
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
            "mod_remove",
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
    vec![write, reload, list, remove]
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
