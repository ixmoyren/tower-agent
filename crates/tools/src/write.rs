//! `write` 工具（对应 pi `coding-agent` 的 `core/tools/write.ts`）。

use std::{
    path::Path,
    sync::Arc,
    task::{Context, Poll},
};

use agent::{AgentError, ToolCallRequest, ToolResultStream};
use ai::JsonObject;
use tokio_util::sync::CancellationToken;
use tower::Service;

use super::{
    path_utils::resolve_to_cwd,
    queue::FileMutationQueue,
    support::{ToolFuture, arg_str, error_result, once, text_result},
};

/// `write` 工具的选项。
#[derive(Debug, Clone, Default)]
pub struct WriteToolOptions {
    /// 覆盖写文件时使用的队列（默认每个工具实例一个）。
    pub queue: Option<FileMutationQueue>,
}

/// 创建/覆盖文件的工具。
#[derive(Clone)]
pub struct WriteTool {
    cwd: Arc<Path>,
    queue: FileMutationQueue,
}

impl WriteTool {
    /// 用工作目录构造。
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self::with_options(cwd, WriteToolOptions::default())
    }

    /// 用工作目录与选项构造。
    pub fn with_options(cwd: impl AsRef<Path>, options: WriteToolOptions) -> Self {
        Self {
            cwd: Arc::from(cwd.as_ref()),
            queue: options.queue.unwrap_or_default(),
        }
    }

    /// 工具名（pi `write`）。
    pub const NAME: &'static str = "write";

    /// 发给模型的工具描述。
    pub const DESCRIPTION: &'static str = "Write content to a file. Creates the file if it doesn't exist, \
         overwrites if it does. Automatically creates parent directories.";

    /// 参数的 JSON Schema。
    pub fn parameters() -> ai::JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (relative or absolute)"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }
}

impl Service<ToolCallRequest> for WriteTool {
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = ToolFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        let tool = self.clone();
        Box::pin(async move {
            let (path, content) = match parse_write_args(&request.tool_call.arguments) {
                Ok(parsed) => parsed,
                Err(message) => return Ok(once(error_result(message))),
            };
            let signal = request.signal.clone();
            let absolute = resolve_to_cwd(&path, &tool.cwd);

            let result = tool
                .queue
                .run(&absolute, || async {
                    if cancelled(&signal) {
                        return error_result("Operation aborted");
                    }
                    if let Some(parent) = absolute.parent()
                        && let Err(error) = tokio::fs::create_dir_all(parent).await
                    {
                        return error_result(format!("Could not write to {path}: {error}."));
                    }
                    if cancelled(&signal) {
                        return error_result("Operation aborted");
                    }
                    if let Err(error) = tokio::fs::write(&absolute, content.as_bytes()).await {
                        return error_result(format!("Could not write to {path}: {error}."));
                    }
                    if cancelled(&signal) {
                        return error_result("Operation aborted");
                    }
                    text_result(format!("Successfully wrote to {path}"))
                })
                .await;
            Ok(once(result))
        })
    }
}

fn parse_write_args(args: &JsonObject) -> Result<(String, String), String> {
    Ok((arg_str(args, "path")?, arg_str(args, "content")?))
}

fn cancelled(signal: &Option<CancellationToken>) -> bool {
    signal.as_ref().is_some_and(CancellationToken::is_cancelled)
}

/// 供测试与内部使用：直接构造一个 `write` 服务。
pub fn create_write_tool(cwd: impl AsRef<Path>) -> agent::AgentTool {
    agent::AgentTool::new(WriteTool::NAME, WriteTool::new(cwd))
        .label(WriteTool::NAME)
        .description(WriteTool::DESCRIPTION)
        .parameters(WriteTool::parameters())
}
