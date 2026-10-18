//! 内置工具（对应 pi `coding-agent` 的 `src/core/tools`）。
//!
//! 每个工具都是 [`tower_agent_runtime::AgentTool`]：元数据 + 一个 `Service<ToolCallRequest, Response =
//! ToolResultStream>`，因此可以直接注册进 `tower_agent_runtime::Agent`，并复用 agent 循环的顺序/并行
//! 执行、`beforeToolCall` / `afterToolCall` 钩子与取消语义（design.md Q3 / D5）。
//!
//! 每个工具同时还实现 [`tower::Layer`]（见 `layer` 模块）：`create_*_tool` 把工具层叠到
//! [`ToolNotFound`] 基础服务上得到该工具的可执行服务，于是每个 tool 都是可组合的 tower 单元。
//!
//! 七个内置工具：`read` / `shell` / `edit` / `write` / `grep` / `find` / `ls`。
//! 它们同时各自对应一个 [`plugin`] 中的静态插件；注册和解析由宿主装配层负责。
//! `shell` 底层是**内嵌的 brush**（bash 兼容），在进程内解释执行，跨平台行为一致。

mod edit;
mod find;
mod grep;
mod layer;
mod ls;
mod read;
mod shell;
mod write;

pub mod edit_diff;
pub mod path_utils;
pub mod plugin;
pub mod queue;
pub mod support;
pub mod truncate;

pub use edit::{EditTool, EditToolOptions, create_edit_tool};
pub use edit_diff::{Edit, LineEnding};
pub use find::{FindTool, create_find_tool};
pub use grep::{GrepTool, create_grep_tool};
pub use layer::{ToolNotFound, ToolRoute};
pub use ls::{LsTool, create_ls_tool};
pub use plugin::{
    DEFAULT_TOOL_NAMES, EditPlugin, FindPlugin, GrepPlugin, LsPlugin, ReadPlugin, ShellPlugin,
    WritePlugin, builtin_plugins,
};
pub use queue::FileMutationQueue;
pub use read::{ReadTool, create_read_tool};
pub use shell::{ShellTool, ShellToolOptions, create_shell_tool};
pub use truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, TruncationResult,
    format_size, truncate_head, truncate_line, truncate_tail,
};
pub use write::{WriteTool, WriteToolOptions, create_write_tool};
