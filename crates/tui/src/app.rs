//! [`Tui`]：把 [`TuiState`] 与一个 ratatui 后端绑在一起的最小宿主。
//!
//! 后端是泛型参数，所以生产环境用 `CrosstermBackend`，测试用 `TestBackend` 断言缓冲区，
//! 两边走同一套渲染代码。

use agent::AgentEvent;
use ratatui::{Terminal, backend::Backend};

use crate::{model::TuiState, render};

/// 终端 UI 宿主：持有后端与视图状态，负责把事件折进状态并重绘。
pub struct Tui<B: Backend> {
    terminal: Terminal<B>,
    state: TuiState,
}

impl<B: Backend> Tui<B> {
    /// 用一个已初始化的终端构造。
    pub fn new(terminal: Terminal<B>, state: TuiState) -> Self {
        Self { terminal, state }
    }

    /// 当前视图状态。
    pub fn state(&self) -> &TuiState {
        &self.state
    }

    /// 可变地访问视图状态（用于从已有会话预填 transcript）。
    pub fn state_mut(&mut self) -> &mut TuiState {
        &mut self.state
    }

    /// 折入一个 agent 事件（不重绘）。
    pub fn handle(&mut self, event: &AgentEvent) {
        self.state.apply(event);
    }

    /// 重绘一次。ratatui 只会把变化的单元格写到后端。
    pub fn draw(&mut self) -> Result<(), B::Error> {
        let state = self.state.clone();
        self.terminal.draw(|frame| render::render(frame, &state))?;
        Ok(())
    }

    /// 向上滚动。
    pub fn scroll_up(&mut self, lines: u16) {
        self.state.scroll_up(lines);
    }

    /// 向下滚动。
    pub fn scroll_down(&mut self, lines: u16) {
        self.state.scroll_down(lines);
    }

    /// 回到最新内容。
    pub fn scroll_to_bottom(&mut self) {
        self.state.scroll_to_bottom();
    }

    /// 消费终端，取回内部后端（测试里用来读缓冲区）。
    pub fn into_terminal(self) -> Terminal<B> {
        self.terminal
    }

    /// 只读访问底层终端（测试里用来读缓冲区，不消耗 `Tui`）。
    pub fn terminal(&self) -> &Terminal<B> {
        &self.terminal
    }
}
