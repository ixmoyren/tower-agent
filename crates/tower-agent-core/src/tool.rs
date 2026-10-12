//! 工具执行边界：`AgentTool.execute → Service`。

use std::{
    fmt,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures_core::Stream;
use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;
use tower::{Service, ServiceExt, util::BoxCloneService};
use tower_agent_provider::{
    ContentBlock, JsonObject, JsonValue, TextContent, Tool, ToolCall, Usage,
};

use crate::AgentError;

/// 一次工具调用请求。
///
/// pi 的 `AgentTool.execute(toolCallId, params, signal, onUpdate)` 拆成两部分：
/// 调用数据放在 [`ToolCallRequest::tool_call`]，取消信号放在
/// [`ToolCallRequest::signal`]；`onUpdate` 由 [`ToolResultStream`] 的中间项承载。
#[derive(Debug, Clone)]
pub struct ToolCallRequest {
    /// 原始工具调用（id、name、arguments）。
    pub tool_call: ToolCall,
    /// 取消信号，对应 pi 的 `AbortSignal`。
    pub signal: Option<CancellationToken>,
}

impl ToolCallRequest {
    /// 用给定的工具调用构造，取消信号为空。
    pub fn new(tool_call: ToolCall) -> Self {
        Self {
            tool_call,
            signal: None,
        }
    }
}

/// 工具产生的结果：部分结果或终态结果（pi `AgentToolResult`）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentToolResult {
    /// 返回给模型的文本/图片内容。
    pub content: Vec<ContentBlock>,
    /// 供日志或 UI 渲染的结构化 `details`。
    pub details: Option<JsonValue>,
    /// 匹配工具 `outputSchema` 的机器可读结果（不发送给模型）。
    pub structured_content: Option<JsonValue>,
    /// 该次工具执行自身的 usage（不参与主 LLM 上下文计费）。
    pub usage: Option<Usage>,
    /// 是否把结果视为错误（模型看到的是错误结果）。
    pub is_error: bool,
    /// 是否建议在本批工具结束后终止 agent；
    /// 仅当一批内所有终态结果都为 `true` 时才提前结束。
    pub terminate: Option<bool>,
}

impl AgentToolResult {
    /// 用一段文本构造成功结果。
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            ..Self::default()
        }
    }

    /// 用一段文本构造错误结果（`is_error: true`）。
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(text)
        }
    }
}

/// 工具结果流（Q3 决策）。
///
/// 中间项对应 pi 的回调式 `onUpdate` 部分结果，末项是终态结果。内部是 boxed
/// stream，因此本类型 `Unpin`，可直接配合 `StreamExt`。
pub struct ToolResultStream {
    inner: Pin<Box<dyn Stream<Item = AgentToolResult> + Send + 'static>>,
}

impl ToolResultStream {
    /// 用任意结果流构造。
    pub fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = AgentToolResult> + Send + 'static,
    {
        Self {
            inner: Box::pin(stream),
        }
    }

    /// 构造只产出单个终态结果的流。
    pub fn once(result: AgentToolResult) -> Self {
        Self::new(OnceStream {
            result: Some(result),
        })
    }
}

impl Stream for ToolResultStream {
    type Item = AgentToolResult;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl fmt::Debug for ToolResultStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolResultStream").finish_non_exhaustive()
    }
}

struct OnceStream {
    result: Option<AgentToolResult>,
}

impl Stream for OnceStream {
    type Item = AgentToolResult;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.result.take())
    }
}

/// 工具调用服务：`Request = ToolCallRequest`、`Response = ToolResultStream`。
///
/// 这是 pi `AgentTool.execute` 的 Rust 对应物。
pub trait AgentToolService:
    Service<ToolCallRequest, Response = ToolResultStream, Error = AgentError> + Send + 'static
{
}

impl<T> AgentToolService for T where
    T: Service<ToolCallRequest, Response = ToolResultStream, Error = AgentError> + Send + 'static
{
}

/// 类型擦除后的工具服务，便于按名存进路由表。
pub type BoxToolService = BoxCloneService<ToolCallRequest, ToolResultStream, AgentError>;

/// 便捷地把具体工具服务装箱成 [`BoxToolService`]。
pub trait AgentToolServiceExt: AgentToolService + Clone + Sized {
    /// 装箱为类型擦除的工具服务（避免与 tower `ServiceExt::boxed*` 重名）。
    fn boxed_tool(self) -> BoxToolService
    where
        <Self as Service<ToolCallRequest>>::Future: Send + 'static,
    {
        BoxCloneService::new(self)
    }
}

impl<T> AgentToolServiceExt for T where T: AgentToolService + Clone + Sized {}

/// 单条 assistant 消息里多个工具调用的执行方式（pi `ToolExecutionMode`）。
///
/// design.md Q2 决策：默认与 pi 一致为 [`Parallel`](Self::Parallel)，并发上限由可配置的
/// 中间件提供，而不是内建隐含上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolExecutionMode {
    /// 逐个准备、执行并定稿。
    Sequential,
    /// 顺序准备，允许的工具并发执行（默认）。
    #[default]
    Parallel,
}

impl ToolExecutionMode {
    /// pi 的 wire 字符串。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Parallel => "parallel",
        }
    }
}

/// 工具结果的持久化重放策略（pi `AgentTool.replay`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replay {
    /// 有副作用，不得自动重放。
    Never,
    /// 重放安全。
    Safe,
}

impl Replay {
    /// pi 的 wire 字符串。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Safe => "safe",
        }
    }
}

/// 调用参数兼容垫片（pi `AgentTool.prepareArguments`）。
///
/// 在校验/执行前把原始工具参数改写成符合 `parameters` schema 的对象。
pub type PrepareArguments = Arc<dyn Fn(JsonObject) -> JsonObject + Send + Sync>;

/// 运行时工具：元数据 + 已装箱的工具服务（pi `AgentTool`）。
///
/// pi 把「声明给模型的接口」和「执行体」放在同一个对象上；Rust 侧执行体是
/// [`AgentToolService`]，因此本类型把它们打包在一起。`label` 只供 UI 展示，
/// 不参与模型可见接口的比较（pi 的 `toToolDeclaration` 只保留
/// `name` / `description` / `parameters`）。
#[derive(Clone)]
pub struct AgentTool {
    name: String,
    label: String,
    description: String,
    parameters: JsonValue,
    output_schema: Option<JsonValue>,
    execution_mode: Option<ToolExecutionMode>,
    replay: Option<Replay>,
    prepare_arguments: Option<PrepareArguments>,
    service: BoxToolService,
}

impl AgentTool {
    /// 用名字和一个具体工具服务构造；`label` 默认等于名字，其余字段为空。
    pub fn new<S>(name: impl Into<String>, service: S) -> Self
    where
        S: AgentToolService + Clone + 'static,
        <S as Service<ToolCallRequest>>::Future: Send + 'static,
    {
        Self::from_boxed(name, service.boxed_tool())
    }

    /// 用名字和一个已经装箱的工具服务构造。
    pub fn from_boxed(name: impl Into<String>, service: BoxToolService) -> Self {
        let name = name.into();
        Self {
            label: name.clone(),
            name,
            description: String::new(),
            parameters: JsonValue::Object(JsonObject::default()),
            output_schema: None,
            execution_mode: None,
            replay: None,
            prepare_arguments: None,
            service,
        }
    }

    /// 设置 UI 展示标签。
    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// 设置工具用途描述（发给模型）。
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// 设置参数 JSON Schema（发给模型）。
    #[must_use]
    pub fn parameters(mut self, parameters: JsonValue) -> Self {
        self.parameters = parameters;
        self
    }

    /// 设置 `structured_content` 的 JSON Schema。
    #[must_use]
    pub fn output_schema(mut self, schema: JsonValue) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// 覆盖此工具的执行方式（不设时使用 loop 的默认值）。
    #[must_use]
    pub fn execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.execution_mode = Some(mode);
        self
    }

    /// 设置持久化重放策略。
    #[must_use]
    pub fn replay(mut self, replay: Replay) -> Self {
        self.replay = Some(replay);
        self
    }

    /// 设置调用参数兼容垫片。
    #[must_use]
    pub fn prepare_arguments(mut self, prepare: PrepareArguments) -> Self {
        self.prepare_arguments = Some(prepare);
        self
    }

    /// 工具名。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// UI 展示标签。
    pub fn display_label(&self) -> &str {
        &self.label
    }

    /// 工具用途描述。
    pub fn tool_description(&self) -> &str {
        &self.description
    }

    /// 参数 JSON Schema。
    pub fn tool_parameters(&self) -> &JsonValue {
        &self.parameters
    }

    /// `structured_content` 的 JSON Schema。
    pub fn declared_output_schema(&self) -> Option<&JsonValue> {
        self.output_schema.as_ref()
    }

    /// 此工具声明的执行方式。
    pub fn declared_execution_mode(&self) -> Option<ToolExecutionMode> {
        self.execution_mode
    }

    /// 解析本工具在当前默认值下的执行方式。
    pub fn resolved_execution_mode(&self, default: ToolExecutionMode) -> ToolExecutionMode {
        self.execution_mode.unwrap_or(default)
    }

    /// 持久化重放策略。
    pub fn declared_replay(&self) -> Option<Replay> {
        self.replay
    }

    /// 已装箱的工具服务。
    pub fn service(&self) -> &BoxToolService {
        &self.service
    }

    /// 声明给模型的工具接口（pi `toToolDeclaration`）。
    pub fn declaration(&self) -> Tool {
        Tool {
            name: self.name.clone(),
            label: self.label.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    /// 应用参数兼容垫片（如有）。
    pub fn prepare(&self, arguments: JsonObject) -> JsonObject {
        match &self.prepare_arguments {
            Some(prepare) => prepare(arguments),
            None => arguments,
        }
    }

    /// 调用工具服务。
    ///
    /// 返回装箱的 `Send` future（`BoxCloneService` 是 `Send` 但不是 `Sync`，
    /// `&AgentTool` 因此不是 `Send`，而并行工具执行要求 future 可跨任务移动）。
    pub fn call(
        &self,
        request: ToolCallRequest,
    ) -> BoxFuture<'static, Result<ToolResultStream, AgentError>> {
        let service = self.service.clone();
        Box::pin(async move { service.oneshot(request).await })
    }
}

impl fmt::Debug for AgentTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentTool")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("execution_mode", &self.execution_mode)
            .field("replay", &self.replay)
            .finish_non_exhaustive()
    }
}
