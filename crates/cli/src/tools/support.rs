//! 内置工具的共享小工具：future 别名、结果构造与入参读取。

use agent::{AgentError, AgentToolResult, ToolResultStream};
use ai::{ContentBlock, JsonObject, JsonValue, TextContent};
use futures_util::future::BoxFuture;

use super::truncate::{TruncatedBy, TruncationResult};

/// 工具服务统一的 future 类型。
pub type ToolFuture = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

/// 构造一段文本内容块。
pub fn text_content(value: impl Into<String>) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: value.into(),
        text_signature: None,
    })
}

/// 构造成功结果（单段文本）。
pub fn text_result(value: impl Into<String>) -> AgentToolResult {
    AgentToolResult {
        content: vec![text_content(value)],
        ..AgentToolResult::default()
    }
}

/// 构造错误结果（`is_error: true`，单段文本）。
pub fn error_result(value: impl Into<String>) -> AgentToolResult {
    AgentToolResult {
        is_error: true,
        ..text_result(value)
    }
}

/// 取出必填字符串参数。
pub fn arg_str(args: &JsonObject, key: &str) -> Result<String, String> {
    match args.get(key) {
        Some(JsonValue::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("Invalid argument `{key}`: expected a string")),
        None => Err(format!("Missing required argument `{key}`")),
    }
}

/// 取出可选字符串参数。
pub fn opt_str(args: &JsonObject, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("Invalid argument `{key}`: expected a string")),
    }
}

/// 取出可选正整数参数。
pub fn opt_u64(args: &JsonObject, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::Number(value)) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("Invalid argument `{key}`: expected a non-negative integer")),
        Some(_) => Err(format!("Invalid argument `{key}`: expected a number")),
    }
}

/// 取出可选浮点参数。
pub fn opt_f64(args: &JsonObject, key: &str) -> Result<Option<f64>, String> {
    match args.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::Number(value)) => value
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("Invalid argument `{key}`: expected a number")),
        Some(_) => Err(format!("Invalid argument `{key}`: expected a number")),
    }
}

/// 用一个终态结果构造 [`ToolResultStream`]。
pub fn once(result: AgentToolResult) -> ToolResultStream {
    ToolResultStream::once(result)
}

/// 把 [`TruncationResult`] 转成 pi `TruncationResult` 的 JSON 形状（用于 `details`）。
pub fn truncation_json(truncation: &TruncationResult) -> JsonValue {
    serde_json::json!({
        "content": truncation.content,
        "truncated": truncation.truncated,
        "truncatedBy": truncation.truncated_by.map(|by| match by {
            TruncatedBy::Lines => "lines",
            TruncatedBy::Bytes => "bytes",
        }),
        "totalLines": truncation.total_lines,
        "totalBytes": truncation.total_bytes,
        "outputLines": truncation.output_lines,
        "outputBytes": truncation.output_bytes,
        "lastLinePartial": truncation.last_line_partial,
        "firstLineExceedsLimit": truncation.first_line_exceeds_limit,
        "maxLines": truncation.max_lines,
        "maxBytes": truncation.max_bytes,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(value: JsonValue) -> JsonObject {
        match value {
            JsonValue::Object(map) => map,
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn argument_readers_validate_types() {
        let args = object(json!({ "path": "a.txt", "offset": 3, "flag": null }));
        assert_eq!(arg_str(&args, "path").unwrap(), "a.txt");
        assert_eq!(opt_u64(&args, "offset").unwrap(), Some(3));
        assert_eq!(opt_str(&args, "flag").unwrap(), None);
        assert!(arg_str(&args, "missing").is_err());
        assert!(opt_u64(&args, "path").is_err());
        assert!(arg_str(&args, "offset").is_err());
    }

    #[test]
    fn error_results_are_flagged() {
        assert!(error_result("boom").is_error);
        assert!(!text_result("ok").is_error);
    }
}
