//! 静态插件：编译进宿主、构建在 tower `Layer` 之上的插件形态。

use std::fmt;

use tower_agent_runtime::{AgentEvent, AgentTool, BoxAgentService, BoxToolService};

use crate::{
    error::PluginError,
    layer::{AgentLayer, EventHandler, ToolLayer},
    model::PluginManifest,
};

/// 静态插件的安装上下文：插件把工具、中间件层与事件处理器登记进来。
#[derive(Default)]
pub struct PluginContext {
    tools: Vec<AgentTool>,
    agent_layers: Vec<Box<dyn AgentLayer>>,
    tool_layers: Vec<Box<dyn ToolLayer>>,
    event_handlers: Vec<Box<dyn EventHandler>>,
}

impl fmt::Debug for PluginContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginContext")
            .field("tools", &self.tools.len())
            .field("agent_layers", &self.agent_layers.len())
            .field("tool_layers", &self.tool_layers.len())
            .field("event_handlers", &self.event_handlers.len())
            .finish()
    }
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

    /// 登记一层 agent 级中间件（`tower::Layer` 形态，见 [`crate::layer`]）。
    pub fn register_agent_layer(&mut self, layer: Box<dyn AgentLayer>) {
        self.agent_layers.push(layer);
    }

    /// 登记一层工具级中间件。
    pub fn register_tool_layer(&mut self, layer: Box<dyn ToolLayer>) {
        self.tool_layers.push(layer);
    }

    /// 登记一个纯通知事件处理器。
    pub fn register_event_handler(&mut self, handler: Box<dyn EventHandler>) {
        self.event_handlers.push(handler);
    }

    /// 已登记的 agent 级中间件（按登记顺序）。
    #[must_use]
    pub fn agent_layers(&self) -> &[Box<dyn AgentLayer>] {
        &self.agent_layers
    }

    /// 已登记的工具级中间件（按登记顺序）。
    #[must_use]
    pub fn tool_layers(&self) -> &[Box<dyn ToolLayer>] {
        &self.tool_layers
    }

    /// 已登记的事件处理器。
    #[must_use]
    pub fn event_handlers(&self) -> &[Box<dyn EventHandler>] {
        &self.event_handlers
    }

    /// 把贡献的 agent 层叠到内层服务上；**先登记的在最外层**（与 `ServiceBuilder` 一致）。
    #[must_use]
    pub fn apply_agent_layers(&self, inner: BoxAgentService) -> BoxAgentService {
        self.agent_layers
            .iter()
            .rev()
            .fold(inner, |service, layer| layer.apply(service))
    }

    /// 把贡献的工具层叠到内层服务上；先登记的在最外层。
    #[must_use]
    pub fn apply_tool_layers(&self, inner: BoxToolService) -> BoxToolService {
        self.tool_layers
            .iter()
            .rev()
            .fold(inner, |service, layer| layer.apply(service))
    }

    /// 把事件派发给所有处理器。
    pub fn dispatch_event(&self, event: &AgentEvent) {
        for handler in &self.event_handlers {
            handler.handle(event);
        }
    }
}

/// 静态插件。
///
/// 静态插件编译进宿主：`manifest` 声明能力，`install` 在解析完成后被调用，
/// 把工具 / 中间件层 / 事件处理器登记进 [`PluginContext`]。
pub trait StaticPlugin: Send + Sync + 'static {
    /// 插件清单。
    fn manifest(&self) -> PluginManifest;

    /// 安装：登记工具 / 中间件层 / 事件处理器。
    fn install(&self, context: &mut PluginContext) -> Result<(), PluginError>;

    /// 贡献 agent 级中间件层（默认无）。会在 `install` 之前登记。
    fn agent_layers(&self) -> Vec<Box<dyn AgentLayer>> {
        Vec::new()
    }

    /// 贡献工具级中间件层（默认无）。
    fn tool_layers(&self) -> Vec<Box<dyn ToolLayer>> {
        Vec::new()
    }

    /// 贡献工具（默认无）。
    fn tools(&self) -> Vec<AgentTool> {
        Vec::new()
    }

    /// 贡献纯通知事件处理器（默认无）。
    fn event_handlers(&self) -> Vec<Box<dyn EventHandler>> {
        Vec::new()
    }
}
