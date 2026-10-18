//! 会话消息与内容块（pi `Message` 及其成员类型）。

use crate::{
    json::{JsonObject, JsonValue},
    thinking::ModelThinkingLevel,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, ops::Deref};

/// transcript 中某个时间点的工具声明（pi `Tool` 的子集）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// 工具名，用于按名分发。
    pub name: String,
    /// 供 UI 展示的工具标签。
    pub label: String,
    /// 工具用途描述。
    pub description: String,
    /// 工具参数的 JSON Schema（pi `TSchema`）。
    pub parameters: JsonValue,
}

/// 对已声明的工具的引用（pi `ToolReference`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolReference {
    /// 被引用工具的名字。
    pub name: String,
}

/// 文本内容块（pi `TextContent`）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TextContent {
    /// 文本本身。
    pub text: String,
    /// provider 特定签名（pi `textSignature`）。
    pub text_signature: Option<String>,
}

/// 思考内容块（pi `ThinkingContent`）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ThinkingContent {
    /// 思考文本。
    pub thinking: String,
    /// provider 特定的思考签名或序列化回放数据。
    pub thinking_signature: Option<String>,
    /// 为 `true` 时表示思考内容被安全过滤，密文载荷存于 `thinking_signature`。
    pub redacted: Option<bool>,
}

/// 图片内容块（pi `ImageContent`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageContent {
    /// base64 编码的图片数据。
    pub data: String,
    /// 图片 MIME 类型，例如 `"image/png"`。
    pub mime_type: String,
}

/// 模型发起的工具调用（pi `ToolCall`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// 工具调用 id。
    pub id: String,
    /// 被调用的工具名。
    pub name: String,
    /// 调用参数（JSON 对象）。
    pub arguments: Arguments,
    /// Google 特定：复用思考上下文用的不透明签名。
    pub thought_signature: Option<String>,
    /// OpenAI Responses 命名空间。
    pub namespace: Option<String>,
}

/// 工具调用参数：JSON 对象的薄包装，附带类型化读取助手。
///
/// 通过 [`Deref`] 可以直接当 `JsonObject` 用；需要按类型取值时用
/// [`Arguments::arg_str`] / [`Arguments::opt_str`] / [`Arguments::opt_u64`] /
/// [`Arguments::opt_f64`]。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Arguments(JsonObject);

impl Arguments {
    /// 借用底层 JSON 对象。
    pub fn as_object(&self) -> &JsonObject {
        &self.0
    }

    /// 取出底层 JSON 对象。
    pub fn into_inner(self) -> JsonObject {
        self.0
    }

    /// 取出必填字符串参数。
    pub fn arg_str(&self, key: &str) -> Result<String, String> {
        match self.get(key) {
            Some(JsonValue::String(value)) => Ok(value.clone()),
            Some(_) => Err(format!("Invalid argument `{key}`: expected a string")),
            None => Err(format!("Missing required argument `{key}`")),
        }
    }

    /// 取出可选字符串参数。
    pub fn opt_str(&self, key: &str) -> Result<Option<String>, String> {
        match self.get(key) {
            None | Some(JsonValue::Null) => Ok(None),
            Some(JsonValue::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(format!("Invalid argument `{key}`: expected a string")),
        }
    }

    /// 取出可选正整数参数。
    pub fn opt_u64(&self, key: &str) -> Result<Option<u64>, String> {
        match self.get(key) {
            None | Some(JsonValue::Null) => Ok(None),
            Some(JsonValue::Number(value)) => value.as_u64().map(Some).ok_or_else(|| {
                format!("Invalid argument `{key}`: expected a non-negative integer")
            }),
            Some(_) => Err(format!("Invalid argument `{key}`: expected a number")),
        }
    }

    /// 取出可选浮点参数。
    pub fn opt_f64(&self, key: &str) -> Result<Option<f64>, String> {
        match self.get(key) {
            None | Some(JsonValue::Null) => Ok(None),
            Some(JsonValue::Number(value)) => value
                .as_f64()
                .map(Some)
                .ok_or_else(|| format!("Invalid argument `{key}`: expected a number")),
            Some(_) => Err(format!("Invalid argument `{key}`: expected a number")),
        }
    }
}

impl Deref for Arguments {
    type Target = JsonObject;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<JsonObject> for Arguments {
    fn from(value: JsonObject) -> Self {
        Self(value)
    }
}

/// 内容块联合类型，对应 pi 的
/// `TextContent | ThinkingContent | ImageContent | ToolCall`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentBlock {
    /// 文本块。
    Text(TextContent),
    /// 思考块。
    Thinking(ThinkingContent),
    /// 图片块。
    Image(ImageContent),
    /// 工具调用块。
    ToolCall(ToolCall),
}

/// 一次 assistant 响应的 token 用量与成本（pi `Usage`）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// 输入 token 数。
    pub input: u64,
    /// 输出 token 数。
    pub output: u64,
    /// 缓存读取 token 数。
    pub cache_read: u64,
    /// 缓存写入 token 数。
    pub cache_write: u64,
    /// `cache_write` 中以 1h retention 写入的子集（仅 Anthropic 报告）。
    pub cache_write_1h: Option<u64>,
    /// reasoning/thinking token 数，是 `output` 的子集。
    pub reasoning: Option<u64>,
    /// 总 token 数。
    pub total_tokens: u64,
    /// 成本明细。
    pub cost: UsageCost,
}

/// 用量成本明细（pi `Usage.cost`）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct UsageCost {
    /// 输入成本。
    pub input: f64,
    /// 输出成本。
    pub output: f64,
    /// 缓存读取成本。
    pub cache_read: f64,
    /// 缓存写入成本。
    pub cache_write: f64,
    /// 总成本。
    pub total: f64,
}

/// 响应的终止原因（pi `StopReason`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StopReason {
    /// 响应尚未结束。
    Pending,
    /// 模型自然结束。
    Stop,
    /// 达到长度上限。
    Length,
    /// 模型要求调用工具。
    ToolUse,
    /// 以错误结束。
    Error,
    /// 被取消（pi `aborted`）。
    Aborted,
    /// 延迟响应（pi `deferred`）。
    Deferred,
}

impl StopReason {
    /// pi 的 wire 字符串（`"toolUse"`，而非 `"tool_use"`）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolUse => "toolUse",
            Self::Error => "error",
            Self::Aborted => "aborted",
            Self::Deferred => "deferred",
        }
    }
}

/// 用户消息内容：纯字符串或内容块数组（pi union）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UserContent {
    /// 纯文本内容。
    Text(String),
    /// 内容块数组（可含文本与图片）。
    Blocks(Vec<ContentBlock>),
}

/// system 指令与工具声明（pi `SystemMessage`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemMessage {
    /// 指令文本；开头消息里是 base prompt，之后是追加指令。
    pub content: Vec<ContentBlock>,
    /// 具名、有序的 prompt section；`None` 表示移除同名 section。
    pub sections: BTreeMap<String, Option<String>>,
    /// 从此刻起可用的工具完整定义。
    pub tools_added: Vec<Tool>,
    /// 从此刻起不可用的工具。
    pub tools_removed: Vec<ToolReference>,
    /// Unix 毫秒时间戳。
    pub timestamp: i64,
}

/// 用户消息（pi `UserMessage`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    /// 用户消息内容。
    pub content: UserContent,
    /// Unix 毫秒时间戳。
    pub timestamp: i64,
}

/// 延迟响应的持久句柄（pi `DeferredHandle`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeferredHandle {
    /// provider 标识。
    pub provider: String,
    /// 模型 id。
    pub model_id: String,
    /// API 标识。
    pub api: String,
    /// provider token，例如 response id 或 batch id + row id。
    pub id: String,
    /// 句柄的过期时间（Unix 毫秒）。
    pub expires_at: Option<u64>,
    /// 建议的轮询间隔，单位毫秒。
    pub poll_after_ms: Option<u64>,
    /// 重建最终 assistant message 所需的 provider 转换数据。
    pub data: Option<JsonValue>,
}

/// assistant 消息（pi `AssistantMessage`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// 响应内容块（文本、思考、工具调用）。
    pub content: Vec<ContentBlock>,
    /// 生成响应的 API 标识。
    pub api: String,
    /// 生成响应的 provider 标识。
    pub provider: String,
    /// 请求使用的模型 id。
    pub model: String,
    /// provider 报告的具体模型（与请求的 `model` 不同时才有值）。
    pub response_model: Option<String>,
    /// provider 暴露的 response/message 标识。
    pub response_id: Option<String>,
    /// provider 原生的思考力度。
    pub provider_thinking_level: Option<String>,
    /// agent loop 请求的 pi 思考级别。
    pub thinking_level: Option<ModelThinkingLevel>,
    /// 本次响应的 token 用量与成本。
    pub usage: Usage,
    /// 响应的终止原因。
    pub stop_reason: StopReason,
    /// 延迟响应句柄（stop reason 为 `deferred` 时存在）。
    pub deferred: Option<DeferredHandle>,
    /// 失败时的错误信息。
    pub error_message: Option<String>,
    /// provider 原始终止原因字符串。
    pub raw_stop_reason: Option<String>,
    /// provider 表示的"模型是否显式结束本轮"。
    pub end_turn: Option<bool>,
    /// Unix 毫秒时间戳。
    pub timestamp: i64,
}

/// 工具结果消息（pi `ToolResultMessage`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    /// 对应的工具调用 id。
    pub tool_call_id: String,
    /// 产生结果的工具名。
    pub tool_name: String,
    /// 结果内容，支持文本与图片。
    pub content: Vec<ContentBlock>,
    /// 工具自己的结构化结果载荷（pi `details`）。
    pub details: Option<JsonValue>,
    /// 该结果是否表示错误。
    pub is_error: bool,
    /// Unix 毫秒时间戳。
    pub timestamp: i64,
}

/// 会话消息联合类型（pi `Message`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// system 消息。
    System(SystemMessage),
    /// 用户消息。
    User(UserMessage),
    /// assistant 消息。装箱以缩小枚举尺寸。
    Assistant(Box<AssistantMessage>),
    /// 工具结果消息。
    ToolResult(ToolResultMessage),
}

#[cfg(test)]
mod tests {
    use crate::{JsonObject, JsonValue, message::Arguments};
    use serde_json::json;

    fn object(value: JsonValue) -> JsonObject {
        match value {
            JsonValue::Object(map) => map,
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn argument_readers_validate_types() {
        let args = object(json!({ "path": "a.txt", "offset": 3, "flag": null }));
        let args = Arguments::from(args);
        assert_eq!(args.arg_str("path").unwrap(), "a.txt");
        assert_eq!(args.opt_u64("offset").unwrap(), Some(3));
        assert_eq!(args.opt_str("flag").unwrap(), None);
        assert!(args.arg_str("missing").is_err());
        assert!(args.opt_u64("path").is_err());
        assert!(args.arg_str("offset").is_err());
    }
}
