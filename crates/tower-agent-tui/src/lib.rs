//! `tui` — Rust 对应 pi 的 `packages/tui`。
//!
//! 本 crate 只负责终端页面：`ratatui` 布局、编辑器和 termina 事件循环。
//! 前端无关的事件归约、斜杠命令与宿主控制协议位于 `tower-agent-plugin-ui`。
//!
//! - [`UiModel`] 是唯一的状态来源，由 [`tower_agent_plugin_ui::AgentEvent`] 归约而来
//!   （`MessageUpdate` 驱动流式文本、`ToolExecution*` 驱动工具条目原地更新）。
//! - [`render`] 是纯渲染函数：`&UiModel` → 缓冲区，因此在 `TestBackend` 上可以直接断言。
//! - [`Tui`] 把状态与一个 ratatui 后端绑在一起；差分渲染由 `Terminal::draw` 承担，
//!   它只把变化的单元格写到后端（对应 design.md 4.2 的"TUI 差分渲染"）。
//! - [`backend::run_agent_tui`] 是生产入口：订阅 [`tower_agent_plugin_ui::Subscription`]，处理键盘输入并重绘。
//! - [`command`] 是斜杠命令表（逐条对齐 pi 的 `BUILTIN_SLASH_COMMANDS`）；真正需要
//!   会话能力的命令通过 [`command::CommandHost`] 交给调用方实现。
//!
//! 后端：ratatui 0.30 把具体后端拆成了独立 crate。当前用的是 **termina**
//! （`ratatui::backend::TerminaBackend`，由 manifest 里的 `termina` feature 引入）。
//! 要换 crossterm/termion/termwiz，只需改 [`backend`] 模块，其余代码与后端无关。
//!
//! 本 crate 不直接依赖 runtime/provider/core，也不实现任何 agent 调用逻辑。

pub mod app;
pub mod backend;
pub mod command;
pub mod editor;
pub mod model;
pub mod plugin;
pub mod render;

pub use app::Tui;
pub use backend::{
    Action, AppTerminal, action_for, init_terminal, restore_terminal, run_agent_tui,
    run_agent_tui_with_shutdown, run_tui, run_tui_with_shutdown,
};
pub use command::{BUILTIN_SLASH_COMMANDS, CommandHost, SlashCommand};
pub use editor::{Editor, EditorAction, Mode};
pub use model::{Entry, EntryKind, ToolEntry, ToolResultView, TuiState, UiModel, render_message};
pub use plugin::{ContributionPlugin, TuiPlugin, UiPlugin, frontend_plugins};
pub use render::{EDITOR_HEIGHT, render, render_with_plugins, transcript_lines};
pub use tower_agent_plugin_ui::{AgentUi, UiAction};
