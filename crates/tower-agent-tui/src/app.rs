//! [`Tui`]：把前端无关的 [`AgentUi`] 与一个 ratatui 后端绑在一起的最小宿主。
//!
//! 后端是泛型参数，所以生产环境用 `TerminaBackend`，测试用 `TestBackend` 断言缓冲区，
//! 两边走同一套渲染代码。

use ratatui::{Terminal, backend::Backend, termina::event::Event};
use tower_agent_plugin_ui::{AgentEvent, AgentUi, UiAction, UiModel};

use crate::{
    editor::{Editor, EditorAction},
    plugin::UiPlugin,
    render,
};

/// 终端 UI 宿主：持有后端、视图状态与输入编辑器，负责把事件折进状态并重绘。
pub struct Tui<B: Backend> {
    terminal: Terminal<B>,
    ui: AgentUi,
    editor: Editor,
    plugins: Vec<Box<dyn UiPlugin>>,
}

impl<B: Backend> Tui<B> {
    /// 用一个已初始化的终端构造。
    pub fn new(terminal: Terminal<B>, state: UiModel) -> Self {
        Self::with_ui(terminal, AgentUi::from(state), Vec::new())
    }

    /// 用一个已初始化的终端与前端无关的 UI 控制器构造。
    pub fn with_ui(terminal: Terminal<B>, ui: AgentUi, plugins: Vec<Box<dyn UiPlugin>>) -> Self {
        Self {
            terminal,
            ui,
            editor: Editor::new(),
            plugins,
        }
    }

    /// 用一个已初始化的终端与一组 UI 插件构造。
    pub fn with_plugins(
        terminal: Terminal<B>,
        state: UiModel,
        plugins: Vec<Box<dyn UiPlugin>>,
    ) -> Self {
        Self::with_ui(terminal, AgentUi::from(state), plugins)
    }

    /// 追加一个 UI 插件。
    pub fn add_ui_plugin(&mut self, plugin: Box<dyn UiPlugin>) {
        self.plugins.push(plugin);
    }

    /// 当前已注册的 UI 插件。
    pub fn ui_plugins(&self) -> &[Box<dyn UiPlugin>] {
        &self.plugins
    }

    /// 当前前端无关的 UI 控制器。
    pub fn ui(&self) -> &AgentUi {
        &self.ui
    }

    /// 当前视图状态快照。
    pub fn state(&self) -> UiModel {
        self.ui.snapshot()
    }

    /// 输入编辑器。
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    /// 折入一个 agent 事件（不重绘）。
    pub fn handle(&mut self, event: &AgentEvent) {
        self.ui.apply_event(event);
    }

    /// 处理编辑器提交的文本；普通文本会返回给宿主提交给 agent。
    pub fn handle_text(&self, text: &str) -> UiAction {
        self.ui.handle_input(text)
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
        let state = self.ui.snapshot();
        let editor = &self.editor;
        self.terminal
            .draw(|frame| render::render_with_plugins(frame, &state, editor, &self.plugins))?;
        Ok(())
    }

    /// 向上滚动。
    pub fn scroll_up(&mut self, lines: u16) {
        self.ui.scroll_up(lines);
    }

    /// 向下滚动。
    pub fn scroll_down(&mut self, lines: u16) {
        self.ui.scroll_down(lines);
    }

    /// 回到最新内容。
    pub fn scroll_to_bottom(&mut self) {
        self.ui.scroll_to_bottom();
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
