//! 工具层：把每个内置工具表达成一个 tower [`Layer`](tower::Layer)。
//!
//! 每个工具 `XTool` 除实现 `Service<ToolCallRequest>`（叶服务）外，还实现 `Layer<S>`：
//! 层叠到内层按名分派服务上后，请求的 `tool_call.name` 命中本工具就交给工具执行，否则
//! 原样委托内层。`create_*_tool` 正是把工具层叠到 [`ToolNotFound`] 基础服务上，产出
//! 发给 agent 的可执行服务——于是每个内置工具都是可组合的 tower 单元。
//!
//! 这套「工具即 Layer + 按名分派」的机制在 `tower_agent_core`（工具契约层），因此
//! 内置工具与 monty 的运行时命名工具（`ModTool`）走同一条路径；这里只保留内置工具的
//! `Layer` 实现宏，并重导出 core 的基础服务/路由类型。

use tower::{Layer, Service};
use tower_agent_runtime::{AgentError, ToolCallRequest, ToolResultStream};

pub use tower_agent_runtime::{ToolNotFound, ToolRoute};

use super::{EditTool, FindTool, GrepTool, LsTool, ReadTool, ShellTool, WriteTool};

/// 为「名字固定」的内置工具实现 `Layer<S>`（层叠到按名分派服务上）。
macro_rules! impl_tool_layer {
    ($tool:ty) => {
        impl<S> Layer<S> for $tool
        where
            S: Service<ToolCallRequest, Response = ToolResultStream, Error = AgentError>
                + Clone
                + Send
                + 'static,
            S::Future: Send + 'static,
        {
            type Service = ToolRoute<S, $tool>;

            fn layer(&self, inner: S) -> Self::Service {
                ToolRoute::new(inner, self.clone(), <$tool>::NAME)
            }
        }
    };
}

impl_tool_layer!(ReadTool);
impl_tool_layer!(ShellTool);
impl_tool_layer!(EditTool);
impl_tool_layer!(WriteTool);
impl_tool_layer!(GrepTool);
impl_tool_layer!(FindTool);
impl_tool_layer!(LsTool);
