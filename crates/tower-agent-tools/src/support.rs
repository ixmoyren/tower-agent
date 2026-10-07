//! 内置工具的共享小工具：future 别名、结果构造与入参读取。

use futures_util::future::BoxFuture;
use tower_agent_provider::{ContentBlock, JsonValue, TextContent};
use tower_agent_runtime::{AgentError, AgentToolResult, ToolResultStream};

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
    use super::*;

    #[test]
    fn error_results_are_flagged() {
        assert!(error_result("boom").is_error);
        assert!(!text_result("ok").is_error);
    }
}
