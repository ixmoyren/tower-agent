//! 内置工具（对应 pi `coding-agent` 的 `src/core/tools`）。
//!
//! 每个工具都是 [`agent::AgentTool`]：元数据 + 一个 `Service<ToolCallRequest, Response =
//! ToolResultStream>`，因此可以直接注册进 `agent::Agent`，并复用 agent 循环的顺序/并行
//! 执行、`beforeToolCall` / `afterToolCall` 钩子与取消语义（design.md Q3 / D5）。
//!
//! 七个内置工具：`read` / `shell` / `edit` / `write` / `grep` / `find` / `ls`。
//! `shell` 是 bash 与 PowerShell 合并后的统一工具：Windows 用 PowerShell，其余系统用 bash
//! （底层 shell 用 `cfg_select!` 按操作系统选择）。

mod edit;
mod find;
mod grep;
mod ls;
mod read;
mod shell;
mod write;

pub mod edit_diff;
pub mod path_utils;
pub mod queue;
pub mod support;
pub mod truncate;

pub use edit::{EditTool, EditToolOptions, create_edit_tool};
pub use edit_diff::{Edit, LineEnding};
pub use find::{FindTool, create_find_tool};
pub use grep::{GrepTool, create_grep_tool};
pub use ls::{LsTool, create_ls_tool};
pub use queue::FileMutationQueue;
pub use read::{ReadTool, create_read_tool};
pub use shell::{ShellTool, ShellToolOptions, create_shell_tool};
pub use truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, TruncationResult,
    format_size, truncate_head, truncate_line, truncate_tail,
};
pub use write::{WriteTool, WriteToolOptions, create_write_tool};

use agent::AgentTool;

/// pi 的内置工具集合中已经移植的部分。
///
/// 顺序沿用 pi `allToolNames`（把拆分出来的第二个 shell 合并掉）：`read` / `shell` /
/// `edit` / `write` / `grep` / `find` / `ls`。`shell` 在 Windows 上是 PowerShell，其余系统
/// 上是 bash。
pub fn default_tools(cwd: impl Into<std::path::PathBuf> + Clone) -> Vec<AgentTool> {
    vec![
        create_read_tool(cwd.clone()),
        create_shell_tool(cwd.clone()),
        create_edit_tool(cwd.clone()),
        create_write_tool(cwd.clone()),
        create_grep_tool(cwd.clone()),
        create_find_tool(cwd.clone()),
        create_ls_tool(cwd),
    ]
}

/// 内置工具名，顺序与 [`default_tools`] 一致。
pub fn tool_names() -> Vec<&'static str> {
    vec![
        ReadTool::NAME,
        ShellTool::NAME,
        EditTool::NAME,
        WriteTool::NAME,
        GrepTool::NAME,
        FindTool::NAME,
        LsTool::NAME,
    ]
}
