//! 把 mod 里的 Python 函数包成一个可注册进 agent 的工具（[`AgentTool`]）。

use std::task::{Context, Poll};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower::{Layer, Service};
use tower_agent_core::{
    AgentError, AgentTool, AgentToolResult, ToolCallRequest, ToolNotFound, ToolResultStream,
    ToolRoute,
};
use tower_agent_provider::JsonValue;

use crate::plugin::MontyPlugin;

/// mod 声明提供的一个工具。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// 暴露给 agent 的工具名。
    pub name: String,
    /// mod 内实现该工具的 Python 函数名。
    pub function: String,
    /// 给模型看的描述。
    pub description: String,
    /// 参数 JSON Schema。
    pub parameters: JsonValue,
}

impl ToolSpec {
    /// 用工具名与函数名构造，描述为空、参数为宽松空对象。
    #[must_use]
    pub fn new(name: impl Into<String>, function: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            function: function.into(),
            description: String::new(),
            parameters: serde_json::json!({ "type": "object" }),
        }
    }
}

/// 一个由 monty mod 实现的工具：调用 mod 里的 `function`。
#[derive(Clone)]
pub struct ModTool {
    plugin: std::sync::Arc<MontyPlugin>,
    spec: ToolSpec,
}

impl ModTool {
    /// 用承载它的插件与工具声明构造。
    #[must_use]
    pub fn new(plugin: std::sync::Arc<MontyPlugin>, spec: ToolSpec) -> Self {
        Self { plugin, spec }
    }

    /// 工具声明。
    #[must_use]
    pub fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    /// 转成可注册进 `ToolRouter` / agent 工具集的 [`AgentTool`]。
    ///
    /// 与内置工具同构：把工具层叠到 [`ToolNotFound`] 上，经 [`ToolRoute`] 按名分派。
    #[must_use]
    pub fn into_agent_tool(self) -> AgentTool {
        let spec = self.spec.clone();
        let service = self.layer(ToolNotFound);
        AgentTool::new(spec.name.clone(), service)
            .label(spec.name)
            .description(spec.description)
            .parameters(spec.parameters)
    }
}

/// 让 `ModTool` 像内置工具一样是一个 tower `Layer`：层叠到内层服务上后按名分派。
impl<S> Layer<S> for ModTool
where
    S: Service<ToolCallRequest, Response = ToolResultStream, Error = AgentError>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Service = ToolRoute<S, ModTool>;

    fn layer(&self, inner: S) -> Self::Service {
        ToolRoute::new(inner, self.clone(), self.spec.name.clone())
    }
}

impl Service<ToolCallRequest> for ModTool {
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        let plugin = self.plugin.clone();
        let function = self.spec.function.clone();
        Box::pin(async move {
            let arguments = Value::Object(request.tool_call.arguments.as_object().clone());
            let result = match plugin.call_function(&function, &arguments).await {
                Ok(value) => AgentToolResult::text(value_to_text(&value)),
                Err(error) => AgentToolResult::error(error.to_string()),
            };
            Ok(ToolResultStream::once(result))
        })
    }
}

/// 工具结果 → 模型可见文本。
fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}
