//! 静态插件：编译进宿主、构建在 tower `Layer` 之上的插件形态。

use agent::AgentTool;

use crate::{error::PluginError, model::PluginManifest};

/// 静态插件的安装上下文：插件把工具登记进来。
#[derive(Debug, Default)]
pub struct PluginContext {
    tools: Vec<AgentTool>,
}

impl PluginContext {
    /// 空上下文。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个内置工具。
    pub fn register_tool(&mut self, tool: AgentTool) {
        self.tools.push(tool);
    }

    /// 已登记的工具（按登记顺序）。
    #[must_use]
    pub fn tools(&self) -> &[AgentTool] {
        &self.tools
    }
}

/// 静态插件。
///
/// 静态插件编译进宿主：`manifest` 声明能力，`install` 在解析完成后被调用，
/// 把工具登记进 [`PluginContext`]（内置工具本身是 tower `Layer`，见 `tools::layer`）。
pub trait StaticPlugin: Send + Sync + 'static {
    /// 插件清单。
    fn manifest(&self) -> PluginManifest;

    /// 安装：登记工具。
    fn install(&self, context: &mut PluginContext) -> Result<(), PluginError>;
}
