//! `ls` 工具（对应 pi `coding-agent` 的 `core/tools/ls.ts`）。

use std::{
    path::Path,
    sync::Arc,
    task::{Context, Poll},
};

use agent::{AgentError, AgentToolResult, ToolCallRequest, ToolResultStream};
use ai::Arguments;
use tokio_util::sync::CancellationToken;
use tower::Service;

use super::{
    path_utils::resolve_to_cwd,
    support::{ToolFuture, error_result, once, text_result},
    truncate::{DEFAULT_MAX_BYTES, TruncationOptions, format_size, truncate_head},
};

/// 默认最大条目数（pi `DEFAULT_LIMIT`）。
const DEFAULT_LIMIT: u64 = 500;

/// 列目录工具。
#[derive(Clone)]
pub struct LsTool {
    cwd: Arc<Path>,
}

impl LsTool {
    /// 用工作目录构造。
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self {
            cwd: Arc::from(cwd.as_ref()),
        }
    }

    /// 工具名。
    pub const NAME: &'static str = "ls";

    /// 发给模型的工具描述（与 pi 一致）。
    pub fn description() -> String {
        format!(
            "List directory contents. Returns entries sorted alphabetically, with '/' suffix for \
             directories. Includes dotfiles. Output is truncated to {DEFAULT_LIMIT} entries or {}KB \
             (whichever is hit first).",
            DEFAULT_MAX_BYTES / 1024
        )
    }

    /// 参数的 JSON Schema。
    pub fn parameters() -> ai::JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (default: current directory)" },
                "limit": { "type": "number", "description": "Maximum number of entries to return (default: 500)" }
            }
        })
    }

    async fn execute(
        &self,
        args: &Arguments,
        signal: &Option<CancellationToken>,
    ) -> AgentToolResult {
        let path = match args.opt_str("path") {
            Ok(path) => path.unwrap_or_else(|| ".".into()),
            Err(message) => return error_result(message),
        };
        let limit = match args.opt_u64("limit") {
            Ok(limit) => limit.unwrap_or(DEFAULT_LIMIT),
            Err(message) => return error_result(message),
        };

        if cancelled(signal) {
            return error_result("Operation aborted");
        }

        let dir_path = resolve_to_cwd(&path, &self.cwd);
        let metadata = match tokio::fs::metadata(&dir_path).await {
            Ok(metadata) => metadata,
            Err(_) => return error_result(format!("Path not found: {}", dir_path.display())),
        };
        if !metadata.is_dir() {
            return error_result(format!("Not a directory: {}", dir_path.display()));
        }

        let mut entries = match tokio::fs::read_dir(&dir_path).await {
            Ok(mut read_dir) => {
                let mut names = Vec::new();
                while let Ok(Some(entry)) = read_dir.next_entry().await {
                    names.push(entry.file_name().to_string_lossy().into_owned());
                }
                names
            }
            Err(error) => {
                return error_result(format!("Cannot read directory: {error}"));
            }
        };
        if cancelled(signal) {
            return error_result("Operation aborted");
        }

        // 与 pi 一致：大小写不敏感排序。
        entries.sort_by_key(|name| name.to_lowercase());

        let mut results: Vec<String> = Vec::new();
        let mut entry_limit_reached = false;
        for entry in entries {
            if results.len() as u64 >= limit {
                entry_limit_reached = true;
                break;
            }
            let full_path = dir_path.join(&entry);
            match tokio::fs::metadata(&full_path).await {
                // 无法 stat 的条目跳过（与 pi 一致）。
                Err(_) => continue,
                Ok(entry_metadata) => {
                    let suffix = if entry_metadata.is_dir() { "/" } else { "" };
                    results.push(format!("{entry}{suffix}"));
                }
            }
        }

        if results.is_empty() {
            return text_result("(empty directory)");
        }

        let raw_output = results.join("\n");
        // 条目数已经单独设限，这里只做字节截断。
        let truncation = truncate_head(
            &raw_output,
            TruncationOptions {
                max_lines: Some(usize::MAX),
                max_bytes: None,
            },
        );
        let mut output = truncation.content.clone();

        let mut notices: Vec<String> = Vec::new();
        if entry_limit_reached {
            notices.push(format!(
                "{limit} entries limit reached. Use limit={} for more",
                limit * 2
            ));
        }
        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        let mut result = text_result(output);
        if entry_limit_reached || truncation.truncated {
            result.details = Some(serde_json::json!({
                "entryLimitReached": entry_limit_reached.then_some(limit),
                "truncation": truncation.truncated.then(|| super::support::truncation_json(&truncation)),
            }));
        }
        result
    }
}

impl Service<ToolCallRequest> for LsTool {
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = ToolFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        let tool = self.clone();
        Box::pin(async move {
            Ok(once(
                tool.execute(&request.tool_call.arguments, &request.signal)
                    .await,
            ))
        })
    }
}

fn cancelled(signal: &Option<CancellationToken>) -> bool {
    signal.as_ref().is_some_and(CancellationToken::is_cancelled)
}

/// 构造 `ls` 工具。
pub fn create_ls_tool(cwd: impl AsRef<Path>) -> agent::AgentTool {
    agent::AgentTool::new(LsTool::NAME, LsTool::new(cwd))
        .label(LsTool::NAME)
        .description(LsTool::description())
        .parameters(LsTool::parameters())
}
