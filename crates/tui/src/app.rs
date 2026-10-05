//! [`Tui`]：把 [`TuiState`] 与一个 ratatui 后端绑在一起的最小宿主。
//!
//! 后端是泛型参数，所以生产环境用 `TerminaBackend`，测试用 `TestBackend` 断言缓冲区，
//! 两边走同一套渲染代码。

use agent::AgentEvent;
use ratatui::{Terminal, backend::Backend, termina::event::Event};

use crate::{
    editor::{Editor, EditorAction},
    model::TuiState,
    plugin::UiPlugin,
    render,
};

/// 终端 UI 宿主：持有后端、视图状态与输入编辑器，负责把事件折进状态并重绘。
pub struct Tui<B: Backend> {
    terminal: Terminal<B>,
    state: TuiState,
    editor: Editor,
    plugins: Vec<Box<dyn UiPlugin>>,
}

impl<B: Backend> Tui<B> {
    /// 用一个已初始化的终端构造。
    pub fn new(terminal: Terminal<B>, state: TuiState) -> Self {
        Self {
            terminal,
            state,
            editor: Editor::new(),
            plugins: Vec::new(),
        }
    }

    /// 用一个已初始化的终端与一组 UI 插件构造。
    pub fn with_plugins(
        terminal: Terminal<B>,
        state: TuiState,
        plugins: Vec<Box<dyn UiPlugin>>,
    ) -> Self {
        Self {
            terminal,
            state,
            editor: Editor::new(),
            plugins,
        }
    }

    /// 追加一个 UI 插件。
    pub fn add_ui_plugin(&mut self, plugin: Box<dyn UiPlugin>) {
        self.plugins.push(plugin);
    }

    /// 当前已注册的 UI 插件。
    pub fn ui_plugins(&self) -> &[Box<dyn UiPlugin>] {
        &self.plugins
    }

    /// 当前视图状态。
    pub fn state(&self) -> &TuiState {
        &self.state
    }

    /// 可变地访问视图状态（用于从已有会话预填 transcript）。
    pub fn state_mut(&mut self) -> &mut TuiState {
        &mut self.state
    }

    /// 输入编辑器。
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    /// 折入一个 agent 事件（不重绘）。
    pub fn handle(&mut self, event: &AgentEvent) {
        self.state.apply(event);
    }

    /// 把一次终端输入交给编辑器并返回其动作。
    ///
    /// 输入类型是 termina 的事件（编辑器基于 `ratatui-textarea` 的 `termina` feature）；
    /// 后端泛型只影响渲染，不影响输入类型。
    pub fn handle_input(&mut self, event: &Event) -> EditorAction {
        self.editor.handle(event)
    }

    /// 重绘一次。ratatui 只会把变化的单元格写到后端。
    pub fn draw(&mut self) -> Result<(), B::Error> {
        let state = self.state.clone();
        let editor = &self.editor;
        self.terminal
            .draw(|frame| render::render_with_plugins(frame, &state, editor, &self.plugins))?;
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
