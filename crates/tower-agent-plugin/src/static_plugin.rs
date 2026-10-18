//! 静态插件：编译进宿主、构建在 tower `Layer` 之上的插件形态。

use std::{
    any::{Any, TypeId},
    fmt,
    sync::Arc,
};

use tower_agent_runtime::{AgentEvent, AgentTool, BoxAgentService, BoxToolService};

use crate::{
    error::PluginError,
    layer::{AgentLayer, EventHandler, ToolLayer},
    model::{PluginId, PluginKind, PluginManifest},
};

/// 静态插件的安装上下文：插件把工具、中间件层与事件处理器登记进来。
#[derive(Default)]
pub struct PluginContext {
    tools: Vec<AgentTool>,
    /// 与 `tools` 平行：每个工具由哪个插件（id + 形态）提供。
    tool_sources: Vec<Option<(PluginId, PluginKind)>>,
    current_source: Option<(PluginId, PluginKind)>,
    agent_layers: Vec<Box<dyn AgentLayer>>,
    tool_layers: Vec<Box<dyn ToolLayer>>,
    event_handlers: Vec<Box<dyn EventHandler>>,
    extensions: Vec<PluginExtensionEntry>,
}

struct PluginExtensionEntry {
    type_id: TypeId,
    value: Arc<dyn Any + Send + Sync>,
    source: Option<(PluginId, PluginKind)>,
}

/// A typed, type-erased plugin contribution.
pub struct PluginExtensionRef<'a, T> {
    /// The contributed value.
    pub value: &'a T,
    /// Plugin that contributed the value.
    pub source: Option<&'a (PluginId, PluginKind)>,
}

impl fmt::Debug for PluginContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginContext")
            .field("tools", &self.tools.len())
            .field("agent_layers", &self.agent_layers.len())
            .field("tool_layers", &self.tool_layers.len())
            .field("event_handlers", &self.event_handlers.len())
            .field("extensions", &self.extensions.len())
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
        self.tool_sources.push(self.current_source.clone());
    }

    /// 已登记的工具（按登记顺序）。
    #[must_use]
    pub fn tools(&self) -> &[AgentTool] {
        &self.tools
    }

    /// 设置「后续登记的工具」的来源（由 `Resolution::install` 在安装每个插件前调用）。
    pub fn set_current_source(&mut self, source: Option<(PluginId, PluginKind)>) {
        self.current_source = source;
    }

    /// 每个工具的来源（与 [`Self::tools`] 平行）。
    #[must_use]
    pub fn tool_sources(&self) -> &[Option<(PluginId, PluginKind)>] {
        &self.tool_sources
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

    /// Register a typed, type-erased contribution for an upper assembly layer.
    ///
    /// The plugin host deliberately does not know concrete extension domains.
    /// Consumers retrieve their own type with [`Self::extensions`].
    pub fn register_extension<T>(&mut self, value: T)
    where
        T: Any + Send + Sync,
    {
        self.extensions.push(PluginExtensionEntry {
            type_id: TypeId::of::<T>(),
            value: Arc::new(value),
            source: self.current_source.clone(),
        });
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

    /// Typed contributions registered by plugins.
    pub fn extensions<T>(&self) -> impl Iterator<Item = PluginExtensionRef<'_, T>>
    where
        T: Any + Send + Sync,
    {
        let type_id = TypeId::of::<T>();
        self.extensions.iter().filter_map(move |entry| {
            if entry.type_id != type_id {
                return None;
            }
            entry
                .value
                .downcast_ref::<T>()
                .map(|value| PluginExtensionRef {
                    value,
                    source: entry.source.as_ref(),
                })
        })
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
/// 把工具 / 中间件层 / 事件处理器 / provider 插件登记进 [`PluginContext`]。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct First(u32);

    #[derive(Debug, PartialEq, Eq)]
    struct Second(&'static str);

    #[test]
    fn typed_extensions_are_isolated_and_keep_their_source() {
        let mut context = PluginContext::new();
        context.set_current_source(Some((
            PluginId::new("provider.test", semver::Version::new(1, 0, 0)),
            PluginKind::Dynamic,
        )));
        context.register_extension(First(7));
        context.register_extension(Second("x"));
        context.register_extension(First(9));

        let first: Vec<_> = context.extensions::<First>().collect();
        assert_eq!(
            first.iter().map(|item| item.value.0).collect::<Vec<_>>(),
            [7, 9]
        );
        assert_eq!(
            first[0].source.map(|(id, kind)| (id.name.as_str(), *kind)),
            Some(("provider.test", PluginKind::Dynamic))
        );

        let second: Vec<_> = context.extensions::<Second>().collect();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].value.0, "x");
        assert!(context.extensions::<String>().next().is_none());
    }
}
