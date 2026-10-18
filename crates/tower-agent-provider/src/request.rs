//! provider 流服务的请求形状。

use std::collections::BTreeMap;

use tokio_util::sync::CancellationToken;

use crate::{json::JsonObject, message::Message, model::Model, thinking::ThinkingLevel};

/// provider 作用域的请求头。值为 `None` 表示抑制同名 provider 默认头（pi `ProviderHeaders`）。
pub type ProviderHeaders = BTreeMap<String, Option<String>>;

/// pi `ToolChoice`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolChoice {
    /// 由模型自行决定是否调用工具。
    Auto,
    /// 禁止调用工具。
    None,
}

/// 各思考级别的自定义 token 预算（pi `ThinkingBudgets`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThinkingBudgets {
    /// `minimal` 级别的 token 预算。
    pub minimal: Option<u64>,
    /// `low` 级别的 token 预算。
    pub low: Option<u64>,
    /// `medium` 级别的 token 预算。
    pub medium: Option<u64>,
    /// `high` 级别的 token 预算。
    pub high: Option<u64>,
    /// `xhigh` 级别的 token 预算。
    pub xhigh: Option<u64>,
    /// `max` 级别的 token 预算。
    pub max: Option<u64>,
}

/// 延迟响应窗口（pi `"15m" | "1h" | "24h"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeferredWindow {
    /// 15 分钟窗口。
    M15,
    /// 1 小时窗口。
    H1,
    /// 24 小时窗口。
    H24,
}

/// `deferred` 请求选项（pi `boolean | { window }`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeferredRequest {
    /// 启用延迟响应，窗口由 provider 决定。
    Enabled,
    /// 使用指定的延迟窗口。
    Window(DeferredWindow),
}

/// provider 请求共享的认证/传输/生命周期选项（pi `ProviderRequestOptions`）。
///
/// 注意：pi 的 `onPayload` / `onResponse` 回调不放在这里，它们由 tower Layer 承载。
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    /// 取消信号，对应 pi 的 `AbortSignal`。
    pub signal: Option<CancellationToken>,
    /// 显式 API key；为 `None` 时由 provider 自己的凭据存储解析。
    pub api_key: Option<String>,
    /// 自定义请求头，覆盖 provider 默认值（`None` 抑制同名默认头）。
    pub headers: ProviderHeaders,
    /// HTTP 超时，单位毫秒（pi `timeoutMs`）。
    pub timeout_ms: Option<u64>,
    /// 客户端最大重试次数（pi `maxRetries`）。
    pub max_retries: Option<u32>,
    /// 服务器要求的长重试延迟上限（pi `maxRetryDelayMs`，默认 60000）。
    pub max_retry_delay_ms: Option<u64>,
    /// 采样温度。
    pub temperature: Option<f64>,
    /// 最大输出 token 数。
    pub max_tokens: Option<u32>,
    /// 支持 session 缓存的 provider 所用的会话标识。
    pub session_id: Option<String>,
    /// provider 特定元数据（只识别它认识的字段）。
    pub metadata: Option<JsonObject>,
}

/// `streamSimple` 的统一选项（pi `SimpleStreamOptions`）。
#[derive(Debug, Clone, Default)]
pub struct SimpleStreamOptions {
    /// 所有 provider 请求共享的基础选项。
    pub base: StreamOptions,
    /// 供给 simple 请求的 provider 中立工具选择。
    pub tool_choice: Option<ToolChoice>,
    /// 思考级别。
    pub reasoning: Option<ThinkingLevel>,
    /// 请求 provider 返回持久句柄并异步续跑。
    pub deferred: Option<DeferredRequest>,
    /// 各思考级别的自定义 token 预算。
    pub thinking_budgets: Option<ThinkingBudgets>,
}

/// 传给 provider 的规范化 transcript（pi `TranscriptContext`）。
///
/// system prompt 与工具声明位于开头的 system 消息里。只有规范化步骤可以构造本类型，
/// 因此裸消息列表无法意外地进入 provider 代码。
#[derive(Debug, Clone)]
pub struct TranscriptContext {
    messages: Vec<Message>,
}

impl TranscriptContext {
    /// 从已规范化的消息序列构造。
    pub fn new(messages: Vec<Message>) -> Self {
        Self { messages }
    }

    /// 只读地访问消息序列。
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// 取出底层消息序列。
    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }
}

/// 一次 provider 流式调用：pi `StreamFunction(model, context, options)`。
#[derive(Debug, Clone)]
pub struct StreamRequest {
    /// 本次调用使用的模型。
    pub model: Model,
    /// 已规范化的会话上下文。
    pub context: TranscriptContext,
    /// 本次调用的选项。
    pub options: SimpleStreamOptions,
}
