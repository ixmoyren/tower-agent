//! `cli` — tower-pi-agent 的 CLI 入口层（对应 pi `packages/coding-agent`）。
//!
//! 本 crate 提供三层内容：
//!
//! - [`tools`]：pi 的八个内置工具（`read` / `bash` / `powershell` / `edit` / `write` /
//!   `grep` / `find` / `ls`）。
//! - [`session`]：会话入口，把 `agent::Agent`、内置工具与事件流（TUI 或纯文本）拼起来。
//! - [`args`]：命令行参数解析；[`faux`] 是确定性的离线 provider，让会话在没有真实模型时
//!   也能端到端运行与测试；[`provider`] 把 `--provider` / `--model` 映射到 `ai` crate 的
//!   真实 provider 栈（anthropic / openai / google）。
//!
//! 扩展（`coding-agent` 的 extensions/MCP/compaction）尚未移植。

pub mod args;
pub mod faux;
pub mod provider;
pub mod session;
pub mod tools;

pub use args::{CliArgs, USAGE, parse};
pub use faux::FauxProvider;
pub use provider::{ProviderError, ProviderKind, ProviderSetup, ProviderStackOptions};
pub use session::Session;
