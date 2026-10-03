//! `cli` — tower-pi-agent 的 CLI 入口层（对应 pi `packages/coding-agent`）。
//!
//! 本 crate 目前提供 pi 内置工具的 Rust 实现（[`tools`]）；会话管理、扩展与交互式
//! 命令行在后续步骤加入。

pub mod tools;
