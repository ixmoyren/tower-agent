//! `tui` — Rust 对应 pi 的 `packages/tui`。
//!
//! 本 crate 用 `ratatui` 把 agent 的事件流渲染成交互式终端界面：
//!
//! - [`TuiState`] 是唯一的状态来源，由 [`agent::AgentEvent`] 归约而来
//!   （`MessageUpdate` 驱动流式文本、`ToolExecution*` 驱动工具条目原地更新）。
//! - [`render`] 是纯渲染函数：`&TuiState` → 缓冲区，因此在 `TestBackend` 上可以直接断言。
//! - [`Tui`] 把状态与一个 ratatui 后端绑在一起；差分渲染由 `Terminal::draw` 承担，
//!   它只把变化的单元格写到后端（对应 design.md 4.2 的"TUI 差分渲染"）。
//! - [`backend::run_agent_tui`] 是生产入口：订阅 [`agent::Subscription`]，处理键盘输入并重绘。
//!
//! 后端：ratatui 0.30 把具体后端拆成了独立 crate。当前用的是 **termina**
//! （`ratatui::backend::TerminaBackend`，由 manifest 里的 `termina` feature 引入）。
//! 要换 crossterm/termion/termwiz，只需改 [`backend`] 模块，其余代码与后端无关。
//!
//! 本 crate **不使用 tower**：TUI 订阅的是事件流，不是"请求 → 响应"边界（design.md 4.1）。

pub mod app;
pub mod backend;
pub mod model;
pub mod render;

pub use app::Tui;
pub use backend::{Action, action_for, init_terminal, restore_terminal, run_agent_tui};
pub use model::{Entry, EntryKind, TuiState, render_message};
pub use render::{render, transcript_lines};
