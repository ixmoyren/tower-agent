//! 输入编辑器：`ratatui-textarea` + vim 键位。
//!
//! `ratatui-textarea` 本身不带 vim 模式（官方 `examples/vim.rs` 是在它之上实现的一层 emulation），
//! 这里按同样的思路实现一个够用的子集：
//!
//! - **默认 Normal 模式**；`i` / `a` / `A` / `I` / `o` / `O` 进 Insert，`Esc` 回 Normal。
//! - Normal：`h j k l`、方向键、`w e b`、`0 ^ $`、`gg` / `G`、`x`、`D`、`C`、`p`、`u`、`Ctrl+r`、
//!   以及操作符前缀 `d` / `y` / `c` + 动作（`dd`、`dw`、`de`、`d$`、`yy`、`cw` …）。
//! - Insert：所有按键（含 `Enter` 换行）原样交给编辑器。
//! - `Enter`（Normal）**提交**，`q`（Normal）**退出**；`Ctrl+C` 由外层统一处理。
//!
//! 默认开启**行号**（`set_line_number_style`），并在边框标题里显示当前模式。

use ratatui::{
    style::{Color, Modifier, Style},
    termina::event::Event,
    widgets::{Block, Borders},
};
use ratatui_textarea::{CursorMove, DataCursor, Input, Key, TextArea};

/// 编辑器处理一次按键后的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorAction {
    /// 编辑器内部消化了这次按键。
    None,
    /// 提交内容（已从编辑器取走并清空）。
    Submit(String),
    /// 请求退出应用。
    Quit,
}

/// 编辑模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 普通模式（vim 默认）。
    Normal,
    /// 插入模式。
    Insert,
    /// 操作符待决（`d` / `y` / `c` 之后等待动作）。
    Operator(char),
}

impl Mode {
    /// 边框标题里显示的模式名。
    pub fn label(self) -> String {
        match self {
            Self::Normal => "NORMAL".into(),
            Self::Insert => "INSERT".into(),
            Self::Operator(op) => format!("OPERATOR({op})"),
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Normal => Style::default().fg(Color::Blue),
            Self::Insert => Style::default().fg(Color::Green),
            Self::Operator(_) => Style::default().fg(Color::Yellow),
        }
    }
}

/// 输入编辑器。
pub struct Editor {
    textarea: TextArea<'static>,
    mode: Mode,
    /// 双键序列的前缀（目前只有 `g`）。
    pending_g: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    /// 构造一个空编辑器：vim 普通模式 + 默认行号。
    pub fn new() -> Self {
        let mut editor = Self {
            textarea: TextArea::default(),
            mode: Mode::Normal,
            pending_g: false,
        };
        editor.configure();
        editor
    }

    /// 底层 textarea（渲染用）。
    pub fn textarea(&self) -> &TextArea<'static> {
        &self.textarea
    }

    /// 当前模式。
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 当前内容（按行拼接），用于测试与调试。
    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// 处理一次终端事件。
    pub fn handle(&mut self, event: &Event) -> EditorAction {
        let input = Input::from(event.clone());
        if input.key == Key::Null {
            return EditorAction::None;
        }
        match self.mode {
            Mode::Insert => self.handle_insert(input),
            Mode::Normal => self.handle_normal(input),
            Mode::Operator(op) => self.handle_operator(op, input),
        }
    }

    /// 行号 / 光标行 / 占位文案 / 边框标题等外观设置。
    fn configure(&mut self) {
        // 默认显示行号。
        self.textarea
            .set_line_number_style(Style::default().fg(Color::DarkGray));
        self.textarea
            .set_cursor_line_style(Style::default().add_modifier(Modifier::DIM));
        self.textarea
            .set_placeholder_text("输入提示词：i 插入 · Esc 普通 · Enter 提交");
        self.refresh_block();
    }

    fn refresh_block(&mut self) {
        self.textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .title_style(self.mode.style())
                .title(format!(" {} ", self.mode.label())),
        );
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.refresh_block();
    }

    fn handle_insert(&mut self, input: Input) -> EditorAction {
        if input.key == Key::Esc {
            self.set_mode(Mode::Normal);
            return EditorAction::None;
        }
        self.textarea.input_without_shortcuts(input);
        EditorAction::None
    }

    fn handle_normal(&mut self, input: Input) -> EditorAction {
        // `gg` 的双键序列。
        if self.pending_g {
            self.pending_g = false;
            if input.key == Key::Char('g') && !input.ctrl {
                self.textarea.move_cursor(CursorMove::Top);
                return EditorAction::None;
            }
        }

        match (input.key, input.ctrl) {
            (Key::Esc, _) => {
                self.textarea.cancel_selection();
            }
            (Key::Enter, false) => return self.submit(),
            (Key::Char('q'), false) => return EditorAction::Quit,
            // 进入插入模式的各种方式。
            (Key::Char('i'), false) => {
                self.textarea.cancel_selection();
                self.set_mode(Mode::Insert);
            }
            (Key::Char('a'), false) => {
                self.textarea.cancel_selection();
                if !self.at_line_end() {
                    self.textarea.move_cursor(CursorMove::Forward);
                }
                self.set_mode(Mode::Insert);
            }
            (Key::Char('A'), false) => {
                self.textarea.move_cursor(CursorMove::End);
                self.set_mode(Mode::Insert);
            }
            (Key::Char('I'), false) => {
                self.textarea.move_cursor(CursorMove::Head);
                self.set_mode(Mode::Insert);
            }
            (Key::Char('o'), false) => {
                self.textarea.move_cursor(CursorMove::End);
                self.textarea.insert_newline();
                self.set_mode(Mode::Insert);
            }
            (Key::Char('O'), false) => {
                self.textarea.move_cursor(CursorMove::Head);
                self.textarea.insert_newline();
                self.textarea.move_cursor(CursorMove::Up);
                self.set_mode(Mode::Insert);
            }
            // 移动。
            (Key::Char('h') | Key::Left, false) => self.textarea.move_cursor(CursorMove::Back),
            (Key::Char('j') | Key::Down, false) => self.textarea.move_cursor(CursorMove::Down),
            (Key::Char('k') | Key::Up, false) => self.textarea.move_cursor(CursorMove::Up),
            (Key::Char('l') | Key::Right, false) => {
                self.textarea.move_cursor(CursorMove::Forward);
            }
            (Key::Char('w'), false) => self.textarea.move_cursor(CursorMove::WordForward),
            (Key::Char('e'), false) => self.textarea.move_cursor(CursorMove::WordEnd),
            (Key::Char('b'), false) => self.textarea.move_cursor(CursorMove::WordBack),
            (Key::Char('0' | '^'), false) => self.textarea.move_cursor(CursorMove::Head),
            (Key::Char('$'), false) => self.textarea.move_cursor(CursorMove::End),
            (Key::Char('g'), false) => self.pending_g = true,
            (Key::Char('G'), false) => self.textarea.move_cursor(CursorMove::Bottom),
            // 编辑。
            (Key::Char('x'), false) => {
                self.textarea.delete_next_char();
            }
            (Key::Char('D'), false) => {
                self.textarea.delete_line_by_end();
            }
            (Key::Char('C'), false) => {
                self.textarea.delete_line_by_end();
                self.textarea.cancel_selection();
                self.set_mode(Mode::Insert);
            }
            (Key::Char('p'), false) => {
                self.textarea.paste();
            }
            (Key::Char('u'), false) => {
                self.textarea.undo();
            }
            (Key::Char('r'), true) => {
                self.textarea.redo();
            }
            // 操作符前缀：进入选择 + 待决状态，后续动作扩展选择。
            (Key::Char(op @ ('d' | 'y' | 'c')), false) => {
                self.textarea.start_selection();
                self.set_mode(Mode::Operator(op));
            }
            _ => {}
        }
        EditorAction::None
    }

    fn handle_operator(&mut self, op: char, input: Input) -> EditorAction {
        match input.key {
            // 同一个操作符再来一次 = 行级操作（dd / yy / cc）。
            Key::Char(c) if c == op => {
                self.textarea.cancel_selection();
                self.select_current_line();
            }
            // 动作键：继续扩展选择。
            Key::Char('w') => self.textarea.move_cursor(CursorMove::WordForward),
            Key::Char('e') => {
                self.textarea.move_cursor(CursorMove::WordEnd);
                self.textarea.move_cursor(CursorMove::Forward);
            }
            Key::Char('b') => self.textarea.move_cursor(CursorMove::WordBack),
            Key::Char('$') => {
                self.textarea.move_cursor(CursorMove::End);
                self.textarea.move_cursor(CursorMove::Forward);
            }
            Key::Char('0' | '^') => self.textarea.move_cursor(CursorMove::Head),
            Key::Char('j') | Key::Down => self.textarea.move_cursor(CursorMove::Down),
            Key::Char('k') | Key::Up => self.textarea.move_cursor(CursorMove::Up),
            Key::Char('h') | Key::Left => self.textarea.move_cursor(CursorMove::Back),
            Key::Char('l') | Key::Right => self.textarea.move_cursor(CursorMove::Forward),
            // 其他键取消这次操作。
            _ => {
                self.textarea.cancel_selection();
                self.set_mode(Mode::Normal);
                return EditorAction::None;
            }
        }

        match op {
            'd' => {
                self.textarea.cut();
                self.set_mode(Mode::Normal);
            }
            'y' => {
                self.textarea.copy();
                self.textarea.cancel_selection();
                self.set_mode(Mode::Normal);
            }
            'c' => {
                self.textarea.cut();
                self.set_mode(Mode::Insert);
            }
            _ => self.set_mode(Mode::Normal),
        }
        EditorAction::None
    }

    /// 选中整行（含行尾换行，最后一行取到行尾）。
    fn select_current_line(&mut self) {
        self.textarea.move_cursor(CursorMove::Head);
        self.textarea.start_selection();
        self.textarea.move_cursor(CursorMove::End);
        self.textarea.move_cursor(CursorMove::Forward);
    }

    fn at_line_end(&self) -> bool {
        let DataCursor(row, col) = self.textarea.cursor();
        let Some(line) = self.textarea.lines().get(row) else {
            return true;
        };
        col + 1 >= line.len()
    }

    fn submit(&mut self) -> EditorAction {
        let text = self.textarea.lines().join("\n");
        let text = text.trim().to_string();
        if text.is_empty() {
            return EditorAction::None;
        }
        self.clear();
        EditorAction::Submit(text)
    }

    /// 清空输入并回到普通模式。
    pub fn clear(&mut self) {
        self.textarea = TextArea::default();
        self.pending_g = false;
        self.mode = Mode::Normal;
        self.configure();
    }
}

#[cfg(test)]
mod tests {
    use ratatui::termina::event::{KeyCode, KeyEvent, Modifiers};

    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, Modifiers::NONE))
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), Modifiers::CONTROL))
    }

    fn type_text(editor: &mut Editor, text: &str) {
        editor.handle(&key(KeyCode::Char('i')));
        for ch in text.chars() {
            editor.handle(&key(KeyCode::Char(ch)));
        }
        editor.handle(&key(KeyCode::Escape));
    }

    #[test]
    fn starts_in_normal_mode_and_inserts_on_i() {
        let mut editor = Editor::new();
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.text(), "");

        editor.handle(&key(KeyCode::Char('i')));
        assert_eq!(editor.mode(), Mode::Insert);
        for ch in "hello".chars() {
            editor.handle(&key(KeyCode::Char(ch)));
        }
        editor.handle(&key(KeyCode::Escape));
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.text(), "hello");
    }

    #[test]
    fn enter_submits_and_clears_the_buffer() {
        let mut editor = Editor::new();
        type_text(&mut editor, "帮我看看");
        let action = editor.handle(&key(KeyCode::Enter));
        assert_eq!(action, EditorAction::Submit("帮我看看".into()));
        assert_eq!(editor.text(), "");
        assert_eq!(editor.mode(), Mode::Normal);
    }

    #[test]
    fn enter_inside_insert_mode_inserts_a_newline_instead_of_submitting() {
        let mut editor = Editor::new();
        editor.handle(&key(KeyCode::Char('i')));
        for ch in "one".chars() {
            editor.handle(&key(KeyCode::Char(ch)));
        }
        let action = editor.handle(&key(KeyCode::Enter));
        assert_eq!(action, EditorAction::None, "Enter 在插入模式只换行");
        for ch in "two".chars() {
            editor.handle(&key(KeyCode::Char(ch)));
        }
        assert_eq!(editor.text(), "one\ntwo");
    }

    #[test]
    fn empty_submit_is_ignored() {
        let mut editor = Editor::new();
        assert_eq!(editor.handle(&key(KeyCode::Enter)), EditorAction::None);
        editor.handle(&key(KeyCode::Char('i')));
        editor.handle(&key(KeyCode::Char(' ')));
        editor.handle(&key(KeyCode::Escape));
        assert_eq!(editor.handle(&key(KeyCode::Enter)), EditorAction::None);
    }

    #[test]
    fn q_quits_from_normal_mode() {
        let mut editor = Editor::new();
        assert_eq!(editor.handle(&key(KeyCode::Char('q'))), EditorAction::Quit);
    }

    #[test]
    fn x_deletes_the_character_under_the_cursor() {
        let mut editor = Editor::new();
        type_text(&mut editor, "abc");
        editor.handle(&key(KeyCode::Char('0')));
        editor.handle(&key(KeyCode::Char('x')));
        assert_eq!(editor.text(), "bc");
    }

    #[test]
    fn dd_deletes_the_current_line() {
        let mut editor = Editor::new();
        editor.handle(&key(KeyCode::Char('i')));
        for ch in "one\ntwo\nthree".chars() {
            if ch == '\n' {
                editor.handle(&key(KeyCode::Enter));
            } else {
                editor.handle(&key(KeyCode::Char(ch)));
            }
        }
        editor.handle(&key(KeyCode::Escape));
        editor.handle(&key(KeyCode::Char('g')));
        editor.handle(&key(KeyCode::Char('g')));
        editor.handle(&key(KeyCode::Char('j')));
        editor.handle(&key(KeyCode::Char('d')));
        editor.handle(&key(KeyCode::Char('d')));
        assert_eq!(editor.text(), "one\nthree");
    }

    #[test]
    fn yy_then_p_pastes_the_yanked_line() {
        let mut editor = Editor::new();
        editor.handle(&key(KeyCode::Char('i')));
        for ch in "one\ntwo".chars() {
            if ch == '\n' {
                editor.handle(&key(KeyCode::Enter));
            } else {
                editor.handle(&key(KeyCode::Char(ch)));
            }
        }
        editor.handle(&key(KeyCode::Escape));
        editor.handle(&key(KeyCode::Char('g')));
        editor.handle(&key(KeyCode::Char('g')));
        editor.handle(&key(KeyCode::Char('y')));
        editor.handle(&key(KeyCode::Char('y')));
        editor.handle(&key(KeyCode::Char('p')));
        assert_eq!(editor.text(), "one\none\ntwo");
    }

    #[test]
    fn ctrl_r_redoes_after_undo() {
        let mut editor = Editor::new();
        type_text(&mut editor, "abc");
        editor.handle(&key(KeyCode::Char('0')));
        editor.handle(&key(KeyCode::Char('x')));
        assert_eq!(editor.text(), "bc");
        editor.handle(&key(KeyCode::Char('u')));
        assert_eq!(editor.text(), "abc");
        editor.handle(&ctrl('r'));
        assert_eq!(editor.text(), "bc");
    }

    #[test]
    fn operator_with_motion_deletes_a_word() {
        let mut editor = Editor::new();
        type_text(&mut editor, "alpha beta");
        editor.handle(&key(KeyCode::Char('0')));
        editor.handle(&key(KeyCode::Char('d')));
        editor.handle(&key(KeyCode::Char('w')));
        assert_eq!(editor.text(), "beta");
    }

    #[test]
    fn a_appends_after_the_cursor() {
        let mut editor = Editor::new();
        type_text(&mut editor, "ab");
        editor.handle(&key(KeyCode::Char('0')));
        editor.handle(&key(KeyCode::Char('a')));
        editor.handle(&key(KeyCode::Char('X')));
        editor.handle(&key(KeyCode::Escape));
        assert_eq!(editor.text(), "aXb");
    }

    #[test]
    fn shift_g_jumps_to_the_last_line() {
        let mut editor = Editor::new();
        editor.handle(&key(KeyCode::Char('i')));
        for ch in "one\ntwo\nthree".chars() {
            if ch == '\n' {
                editor.handle(&key(KeyCode::Enter));
            } else {
                editor.handle(&key(KeyCode::Char(ch)));
            }
        }
        editor.handle(&key(KeyCode::Escape));
        editor.handle(&key(KeyCode::Char('g')));
        editor.handle(&key(KeyCode::Char('g')));
        assert_eq!(editor.textarea().cursor().0, 0);
        editor.handle(&key(KeyCode::Char('G')));
        assert_eq!(editor.textarea().cursor().0, 2);
    }

    #[test]
    fn mode_label_is_shown_in_the_block_title() {
        let mut editor = Editor::new();
        assert_eq!(editor.mode().label(), "NORMAL");
        editor.handle(&key(KeyCode::Char('i')));
        assert_eq!(editor.mode().label(), "INSERT");
        editor.handle(&key(KeyCode::Escape));
        editor.handle(&key(KeyCode::Char('d')));
        assert_eq!(editor.mode().label(), "OPERATOR(d)");
    }
}
