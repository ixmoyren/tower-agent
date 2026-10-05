//! 内置工具静态插件：每个工具是独立插件，提供一项 `tool.<name>` 能力。
//!
//! 内置工具的实现仍在 `tools` crate，且每个工具都是一个 tower `Layer`
//! （见 `tools::layer`）：`tools::create_*_tool` 把工具层叠到「未找到」基础服务上，
//! 产出可执行服务。本模块把这些工具逐个注册为静态插件，并提供默认注册表/解析入口。
//!
//! 这里不再有 `core-tools` 这类整体打包插件，也不再涉及 provider 中间件——provider
//! 栈（retry / timeout / observe）由 `cli::provider` 直接编排。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use agent::AgentTool;
use semver::Version;

use crate::{
    error::PluginError,
    model::{CapabilityId, PluginKind, PluginManifest},
    registry::{PluginRegistry, Resolution},
    static_plugin::{PluginContext, StaticPlugin},
};

/// 内置插件与能力的初始版本。
const BUILTIN_VERSION: Version = Version::new(1, 0, 0);

/// 内置工具名，顺序沿用 pi `allToolNames`（拆出来的第二个 shell 已合并）。
///
/// `shell` 在 Windows 上是 PowerShell，其余系统上是 bash。
pub static DEFAULT_TOOL_NAMES: [&str; 7] = [
    tools::ReadTool::NAME,
    tools::ShellTool::NAME,
    tools::EditTool::NAME,
    tools::WriteTool::NAME,
    tools::GrepTool::NAME,
    tools::FindTool::NAME,
    tools::LsTool::NAME,
];

/// 一个内置工具的构造函数（工作目录 → 可执行工具）。
type ToolBuilder = fn(&Path) -> AgentTool;

/// `(工具名, 构造函数)` 表，顺序与 [`DEFAULT_TOOL_NAMES`] 一致。
const BUILTIN_TOOLS: [(&str, ToolBuilder); 7] = [
    (tools::ReadTool::NAME, read_tool),
    (tools::ShellTool::NAME, shell_tool),
    (tools::EditTool::NAME, edit_tool),
    (tools::WriteTool::NAME, write_tool),
    (tools::GrepTool::NAME, grep_tool),
    (tools::FindTool::NAME, find_tool),
    (tools::LsTool::NAME, ls_tool),
];

fn read_tool(cwd: &Path) -> AgentTool {
    tools::create_read_tool(cwd)
}

fn shell_tool(cwd: &Path) -> AgentTool {
    tools::create_shell_tool(cwd)
}

fn edit_tool(cwd: &Path) -> AgentTool {
    tools::create_edit_tool(cwd)
}

fn write_tool(cwd: &Path) -> AgentTool {
    tools::create_write_tool(cwd)
}

fn grep_tool(cwd: &Path) -> AgentTool {
    tools::create_grep_tool(cwd)
}

fn find_tool(cwd: &Path) -> AgentTool {
    tools::create_find_tool(cwd)
}

fn ls_tool(cwd: &Path) -> AgentTool {
    tools::create_ls_tool(cwd)
}

/// 单个内置工具的静态插件：提供 `tool.<name>` 能力，安装时登记对应工具。
struct BuiltinToolPlugin {
    name: &'static str,
    cwd: PathBuf,
    build: ToolBuilder,
}

impl BuiltinToolPlugin {
    fn new(cwd: &Path, name: &'static str, build: ToolBuilder) -> Self {
        Self {
            name,
            cwd: cwd.to_path_buf(),
            build,
        }
    }
}

impl StaticPlugin for BuiltinToolPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest::new(self.name, BUILTIN_VERSION.clone(), PluginKind::Static).providing(
            CapabilityId::new("tool", self.name),
            BUILTIN_VERSION.clone(),
        )
    }

    fn install(&self, context: &mut PluginContext) -> Result<(), PluginError> {
        context.register_tool((self.build)(&self.cwd));
        Ok(())
    }
}

/// 注册全部内置工具插件（每个工具一个静态插件）。
#[must_use]
pub fn default_registry(cwd: impl AsRef<Path>) -> PluginRegistry {
    let cwd = cwd.as_ref();
    let mut registry = PluginRegistry::new();
    for (name, build) in BUILTIN_TOOLS {
        registry
            .register_static(Arc::new(BuiltinToolPlugin::new(cwd, name, build)))
            .expect("内置工具插件清单固定且自洽");
    }
    registry
}

/// 解析全部内置工具插件。
pub fn resolve_default(cwd: impl AsRef<Path>) -> Result<Resolution, PluginError> {
    default_registry(cwd).resolve()
}

/// 经注册表解析后的内置工具集合（`DEFAULT_TOOL_NAMES` 的实现版）。
///
/// # Panics
///
/// 内置插件集合固定且自洽，解析/安装失败只可能来自本 crate 的编码错误。
#[must_use]
pub fn default_tools(cwd: impl AsRef<Path>) -> Vec<AgentTool> {
    resolve_default(cwd)
        .expect("内置插件必须能解析")
        .tools()
        .expect("内置工具插件必须能安装")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_builtin_tool_is_its_own_static_plugin() {
        let resolution = resolve_default(".").expect("内置插件可解析");
        let names: Vec<&str> = resolution
            .plugins()
            .iter()
            .map(|plugin| plugin.id.name.as_str())
            .collect();
        assert_eq!(names, DEFAULT_TOOL_NAMES.to_vec());
        assert!(resolution.shadows().is_empty());
        for plugin in resolution.plugins() {
            assert_eq!(plugin.kind, PluginKind::Static);
            assert!(plugin.bindings.is_empty());
            assert!(
                plugin
                    .manifest
                    .provides_capability(&CapabilityId::new("tool", plugin.id.name.as_str()))
                    .is_some(),
                "插件 {} 应提供 tool.{} 能力",
                plugin.id,
                plugin.id.name
            );
        }
    }

    #[test]
    fn resolved_tools_match_the_builtin_names_and_order() {
        let cwd = std::env::temp_dir();
        let names: Vec<String> = default_tools(&cwd)
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        let expected: Vec<String> = DEFAULT_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        assert_eq!(names, expected);
    }
}
