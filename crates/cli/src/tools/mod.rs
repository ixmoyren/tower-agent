//! 内置工具（对应 pi `coding-agent` 的 `src/core/tools`）。
//!
//! 每个工具都是 [`agent::AgentTool`]：元数据 + 一个 `Service<ToolCallRequest, Response =
//! ToolResultStream>`，因此可以直接注册进 `agent::Agent`，并复用 agent 循环的顺序/并行
//! 执行、`beforeToolCall` / `afterToolCall` 钩子与取消语义（design.md Q3 / D5）。
//!
//! 已移植：`read` / `write` / `edit`（含精确 + 模糊匹配）与 `bash`（流式输出 + 截断 +
//! 临时文件）。尚未移植：`ls` / `find` / `grep` / `powershell`。

mod bash;
mod edit;
mod read;
mod write;

pub mod edit_diff;
pub mod path_utils;
pub mod queue;
pub mod support;
pub mod truncate;

pub use bash::{BashTool, BashToolOptions, create_bash_tool};
pub use edit::{EditTool, EditToolOptions, create_edit_tool};
pub use edit_diff::{Edit, LineEnding};
pub use queue::FileMutationQueue;
pub use read::{ReadTool, create_read_tool};
pub use truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, TruncationResult,
    format_size, truncate_head, truncate_line, truncate_tail,
};
pub use write::{WriteTool, WriteToolOptions, create_write_tool};

use agent::AgentTool;

/// pi 的内置工具集合中已经移植的部分。
///
/// 顺序沿用 pi `allToolNames` 的相对次序（`read` / `bash` / `edit` / `write`）。
pub fn default_tools(cwd: impl Into<std::path::PathBuf> + Clone) -> Vec<AgentTool> {
    vec![
        create_read_tool(cwd.clone()),
        create_bash_tool(cwd.clone()),
        create_edit_tool(cwd.clone()),
        create_write_tool(cwd),
    ]
}

/// 已移植的内置工具名。
pub fn tool_names() -> Vec<&'static str> {
    vec![
        ReadTool::NAME,
        BashTool::NAME,
        EditTool::NAME,
        WriteTool::NAME,
    ]
}
