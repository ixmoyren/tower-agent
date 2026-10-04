//! `cli` — tower-pi-agent 的 CLI 入口层（对应 pi `packages/coding-agent`）。
//!
//! 本 crate 负责会话入口与命令行解析，也是 workspace 里**唯一**的可执行入口
//! （`src/main.rs`）：
//!
//! - [`session`]：会话入口，把 `agent::Agent`、`tools` crate 的内置工具与事件流
//!   （TUI 或纯文本）拼起来。
//! - [`args`]：命令行参数解析（`clap` derive，帮助/版本/错误由 clap 生成）；[`faux`] 是
//!   确定性的离线 provider，让会话在没有真实模型时也能端到端运行与测试；[`provider`] 把
//!   `--provider` / `--model` 映射到 `ai` crate 的真实 provider 栈
//!   （anthropic / openai / google）。
//!
//! pi 的八个内置工具（`read` / `bash` / `powershell` / `edit` / `write` / `grep` /
//! `find` / `ls`）在独立的 `tools` crate 中。
//!
//! 扩展（`coding-agent` 的 extensions/MCP/compaction）尚未移植。

pub mod args;
pub mod faux;
pub mod provider;
pub mod session;

pub use args::{CliArgs, parse};
pub use faux::FauxProvider;
pub use provider::{ProviderError, ProviderKind, ProviderSetup, ProviderStackOptions};
pub use session::Session;
