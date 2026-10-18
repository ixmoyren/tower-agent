//! 输入编辑器：`ratatui-textarea` + vim 键位。
//!
//! `ratatui-textarea` 本身不带 vim 模式（官方 `examples/vim.rs` 是在它之上实现的一层 emulation），
//! 这里按同样的思路实现一个够用的子集：
//!
//! - **默认 Normal 模式**；`i` / `a` / `A` / `I` / `o` / `O` 进 Insert，`Esc` 回 Normal。
//! - Normal：`h j k l`、方向键、`w e b`、`0 ^ $`、`gg` / `G`、`x`、`D`、`C`、`p`、`u`、`Ctrl+r`、
//!   以及操作符前缀 `d` / `y` / `c` + 动作（`dd`、`dw`、`de`、`d$`、`yy`、`cw` …）。
//! - Visual：`v` 逐字符、`V` 整行；`y` 复制、`d` 删除、`c` 删除并进 Insert、`p` 用寄存器替换选区；
//!   `Esc` / `v` 取消选择。
//! - Replace：`r` 替换一次（回到 Normal）、`R` 覆盖输入（`Esc` 结束）。
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
use std::borrow::Cow;

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
    /// 可视模式（`v` 逐字符 / `V` 整行）。
    Visual,
    /// 替换模式；`true` 表示只替换一次（`r`），`false` 表示覆盖输入（`R`）。
    Replace(bool),
    /// 操作符待决（`d` / `y` / `c` 之后等待动作）。
    Operator(char),
}

impl Mode {
    /// 边框标题里显示的模式名。
    pub fn label(self) -> Cow<'static, str> {
        match self {
            Self::Normal => "NORMAL".into(),
            Self::Insert => "INSERT".into(),
            Self::Visual => "VISUAL".into(),
            Self::Replace(true) => "REPLACE(once)".into(),
            Self::Replace(false) => "REPLACE".into(),
            Self::Operator(op) => format!("OPERATOR({op})").into(),
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Normal => Style::default().fg(Color::Blue),
            Self::Insert => Style::default().fg(Color::Green),
            Self::Visual => Style::default().fg(Color::Magenta),
            Self::Replace(_) => Style::default().fg(Color::LightRed),
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
    /// 斜杠命令菜单里当前选中的项。
    menu_selection: usize,
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
            menu_selection: 0,
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

    /// 当前输入触发的斜杠命令菜单（`/` 开头且还没进入参数时为 `Some`）。
    pub fn command_menu(&self) -> Option<Vec<&'static crate::command::SlashCommand>> {
        crate::command::menu(&self.text())
    }

    /// 菜单里当前选中的下标（已按菜单长度收敛）。
    pub fn menu_selection(&self) -> usize {
        self.menu_selection
    }

    /// 处理一次终端事件。
    pub fn handle(&mut self, event: &Event) -> EditorAction {
        let input = Input::from(event.clone());
        if input.key == Key::Null {
            return EditorAction::None;
        }
        match self.mode {
            Mode::Insert => self.handle_insert(input),
            Mode::Visual => self.handle_visual(input),
            Mode::Replace(once) => self.handle_replace(once, input),
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
            .set_placeholder_text("Prompt: / commands · i insert · Esc normal · Enter submit");
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
        if let Some(items) = self.command_menu() {
            let len = items.len();
            match (input.key, input.ctrl) {
                (Key::Up, false) | (Key::Char('p'), true) => {
                    self.menu_selection = menu_index(self.menu_selection, len, -1);
                    return EditorAction::None;
                }
                (Key::Down, false) | (Key::Char('n'), true) => {
                    self.menu_selection = menu_index(self.menu_selection, len, 1);
                    return EditorAction::None;
                }
                (Key::Tab, false) => {
                    self.complete_menu_selection(&items);
                    return EditorAction::None;
                }
                (Key::Enter, false) => {
                    let command = items[menu_index(self.menu_selection, len, 0)];
                    if command.argument_hint.is_some() {
                        // 带参数的命令：Enter 先补全到输入框（保持 Insert 模式），
                        // 让用户接着输入参数，而不是直接执行。
                        self.complete_command(command);
                        return EditorAction::None;
                    }
                    let text = format!("/{}", command.name);
                    self.clear();
                    return EditorAction::Submit(text);
                }
                _ => {}
            }
        }
        if self.apply_insert_motion(&input) {
            return EditorAction::None;
        }
        self.textarea.input_without_shortcuts(input);
        self.clamp_menu_selection();
        EditorAction::None
    }

    /// Tab 补全：把选中的命令写进输入框（带参数的补一个空格）。
    fn complete_menu_selection(&mut self, items: &[&'static crate::command::SlashCommand]) {
        let command = items[menu_index(self.menu_selection, items.len(), 0)];
        self.complete_command(command);
    }

    /// 把一条命令补全到输入框（带参数的命令补一个空格），保持当前 Insert 模式。
    fn complete_command(&mut self, command: &crate::command::SlashCommand) {
        let mut completed = format!("/{}", command.name);
        if command.argument_hint.is_some() {
            completed.push(' ');
        }
        self.textarea = TextArea::default();
        self.configure();
        self.textarea.insert_str(completed);
        self.menu_selection = 0;
    }

    fn clamp_menu_selection(&mut self) {
        let len = self.command_menu().map_or(0, |items| items.len());
        self.menu_selection = menu_index(self.menu_selection, len, 0);
    }

    /// 共享的移动键：Normal 与 Visual 都用它（Visual 处于选择状态，移动即扩展选区）。
    ///
    /// 返回 `true` 表示这次按键被当作移动处理。
    fn apply_motion(&mut self, input: &Input) -> bool {
        // `gg` 的双键序列。
        if self.pending_g {
            self.pending_g = false;
            if input.key == Key::Char('g') && !input.ctrl {
                self.textarea.move_cursor(CursorMove::Top);
                return true;
            }
        }
        match (input.key, input.ctrl) {
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
            (Key::Char('g'), false) => {
                self.pending_g = true;
            }
            (Key::Char('G'), false) => self.textarea.move_cursor(CursorMove::Bottom),
            _ => return false,
        }
        true
    }

    /// Insert 模式下的方向键移动。
    ///
    /// `input_without_shortcuts` 只完整处理了普通 Left，Right/Up/Down 会被忽略；
    /// 这里显式映射，保证对话框编辑器中的方向键与 Home/End 行为一致。
    fn apply_insert_motion(&mut self, input: &Input) -> bool {
        if input.ctrl || input.alt || input.shift {
            return false;
        }
        let motion = match input.key {
            Key::Left => CursorMove::Back,
            Key::Right => CursorMove::Forward,
            Key::Up => CursorMove::Up,
            Key::Down => CursorMove::Down,
            Key::Home => CursorMove::Head,
            Key::End => CursorMove::End,
            _ => return false,
        };
        self.textarea.move_cursor(motion);
        true
    }

    fn handle_normal(&mut self, input: Input) -> EditorAction {
        if self.apply_motion(&input) {
            return EditorAction::None;
        }

        match (input.key, input.ctrl) {
            (Key::Esc, _) => {
                self.textarea.cancel_selection();
            }
            (Key::Enter, false) => return self.submit(),
            (Key::Char('q'), false) => return EditorAction::Quit,
            // `/` 打开命令菜单（本移植版没有 vim 搜索，这个键位给斜杠命令用）。
            (Key::Char('/'), false) => {
                self.textarea.cancel_selection();
                self.set_mode(Mode::Insert);
                self.textarea.insert_char('/');
                self.menu_selection = 0;
            }
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
            // 可视模式：`v` 逐字符，`V` 整行。
            (Key::Char('v'), false) => {
                self.textarea.start_selection();
                self.set_mode(Mode::Visual);
            }
            (Key::Char('V'), false) => {
                self.textarea.move_cursor(CursorMove::Head);
                self.textarea.start_selection();
                self.textarea.move_cursor(CursorMove::End);
                self.set_mode(Mode::Visual);
            }
            // 替换模式：`r` 替换一次，`R` 覆盖输入。
            (Key::Char('r'), false) => self.set_mode(Mode::Replace(true)),
            (Key::Char('R'), false) => self.set_mode(Mode::Replace(false)),
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

    /// 可视模式：移动扩展选区，`y`/`d`/`c`/`p` 作用在选区上。
    fn handle_visual(&mut self, input: Input) -> EditorAction {
        // `Esc` 或再按一次 `v` 取消选择。
        if input.key == Key::Esc || (input.key == Key::Char('v') && !input.ctrl) {
            self.textarea.cancel_selection();
            self.set_mode(Mode::Normal);
            return EditorAction::None;
        }
        if self.apply_motion(&input) {
            return EditorAction::None;
        }
        match (input.key, input.ctrl) {
            // vim 的选区是「含端点」的：先补上光标所在字符再操作。
            (Key::Char('y'), false) => {
                self.textarea.move_cursor(CursorMove::Forward);
                self.textarea.copy();
                self.set_mode(Mode::Normal);
            }
            (Key::Char('d'), false) => {
                self.textarea.move_cursor(CursorMove::Forward);
                self.textarea.cut();
                self.set_mode(Mode::Normal);
            }
            (Key::Char('c'), false) => {
                self.textarea.move_cursor(CursorMove::Forward);
                self.textarea.cut();
                self.set_mode(Mode::Insert);
            }
            (Key::Char('p'), false) => {
                self.textarea.move_cursor(CursorMove::Forward);
                self.textarea.cut();
                self.textarea.paste();
                self.set_mode(Mode::Normal);
            }
            _ => {}
        }
        EditorAction::None
    }

    /// 替换模式：`once` 为 `r`（替换一个字符后回 Normal），否则为 `R`（覆盖直到 `Esc`）。
    fn handle_replace(&mut self, once: bool, input: Input) -> EditorAction {
        if input.key == Key::Esc {
            self.set_mode(Mode::Normal);
            return EditorAction::None;
        }
        let stay = if once {
            Mode::Normal
        } else {
            Mode::Replace(false)
        };

        let Key::Char(ch) = input.key else {
            self.set_mode(stay);
            return EditorAction::None;
        };
        if input.ctrl || input.alt {
            self.set_mode(stay);
            return EditorAction::None;
        }

        let has_char_under_cursor = self.has_char_under_cursor();
        // `r` 只在光标下有字符时替换（vim 里行尾按 r 不做事）；
        // `R` 在行尾则直接追加。
        if has_char_under_cursor || !once {
            if has_char_under_cursor {
                self.textarea.delete_next_char();
            }
            self.textarea.insert_char(ch);
        }
        self.set_mode(stay);
        EditorAction::None
    }

    /// 某一行的字符数（`DataCursor` 的列是字符下标，不是字节）。
    fn line_char_len(&self, row: usize) -> usize {
        self.textarea
            .lines()
            .get(row)
            .map_or(0, |line| line.chars().count())
    }

    /// 光标下是否有字符（行尾/空行时为 false）。
    fn has_char_under_cursor(&self) -> bool {
        let DataCursor(row, col) = self.textarea.cursor();
        col < self.line_char_len(row)
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
        col + 1 >= self.line_char_len(row)
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
        self.menu_selection = 0;
        self.configure();
    }
}

/// 在长度为 `len` 的菜单里移动选中项：`delta` 为 -1 向上 / 1 向下 / 0 只收敛；
/// 越界会绕回另一端。
fn menu_index(selection: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let current = selection.min(len - 1) as isize;
    (current + delta).rem_euclid(len as isize) as usize
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
        type_text(&mut editor, "look at this");
        let action = editor.handle(&key(KeyCode::Enter));
        assert_eq!(action, EditorAction::Submit("look at this".into()));
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
        assert_eq!(
            action,
            EditorAction::None,
            "Enter only inserts a newline in insert mode"
        );
        for ch in "two".chars() {
            editor.handle(&key(KeyCode::Char(ch)));
        }
        assert_eq!(editor.text(), "one\ntwo");
    }

    #[test]
    fn insert_mode_arrow_keys_move_the_cursor() {
        let mut editor = Editor::new();
        editor.handle(&key(KeyCode::Char('i')));
        for ch in "ab".chars() {
            editor.handle(&key(KeyCode::Char(ch)));
        }
        editor.handle(&key(KeyCode::Enter));
        for ch in "cd".chars() {
            editor.handle(&key(KeyCode::Char(ch)));
        }
        assert_eq!(editor.textarea().cursor(), (1, 2));

        editor.handle(&key(KeyCode::Up));
        assert_eq!(editor.textarea().cursor(), (0, 2));
        editor.handle(&key(KeyCode::Down));
        assert_eq!(editor.textarea().cursor(), (1, 2));
        editor.handle(&key(KeyCode::Left));
        assert_eq!(editor.textarea().cursor(), (1, 1));
        editor.handle(&key(KeyCode::Right));
        assert_eq!(editor.textarea().cursor(), (1, 2));

        editor.handle(&key(KeyCode::Left));
        editor.handle(&key(KeyCode::Char('X')));
        assert_eq!(editor.text(), "ab\ncXd");
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

    /// 输入多行文本（`\n` 走 Enter 键，模拟 INSERT 模式下的换行）。
    fn type_lines(editor: &mut Editor, lines: &[&str]) {
        editor.handle(&key(KeyCode::Char('i')));
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                editor.handle(&key(KeyCode::Enter));
            }
            for ch in line.chars() {
                editor.handle(&key(KeyCode::Char(ch)));
            }
        }
        editor.handle(&key(KeyCode::Escape));
    }

    #[test]
    fn visual_mode_deletes_the_selection() {
        let mut editor = Editor::new();
        type_text(&mut editor, "hello world");
        editor.handle(&key(KeyCode::Char('0')));
        editor.handle(&key(KeyCode::Char('v')));
        assert_eq!(editor.mode(), Mode::Visual);
        editor.handle(&key(KeyCode::Char('e')));
        editor.handle(&key(KeyCode::Char('d')));
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.text(), " world");
    }

    #[test]
    fn visual_yank_then_paste_duplicates_the_selection() {
        let mut editor = Editor::new();
        type_lines(&mut editor, &["one", "two"]);
        editor.handle(&key(KeyCode::Char('g')));
        editor.handle(&key(KeyCode::Char('g')));
        editor.handle(&key(KeyCode::Char('V')));
        assert_eq!(editor.mode(), Mode::Visual);
        editor.handle(&key(KeyCode::Char('y')));
        assert_eq!(editor.mode(), Mode::Normal);
        // y 之后光标停在下一行行首，p 把复制到的整行插回去。
        editor.handle(&key(KeyCode::Char('p')));
        assert_eq!(editor.text(), "one\none\ntwo");
    }

    #[test]
    fn visual_line_mode_deletes_the_whole_line() {
        let mut editor = Editor::new();
        type_lines(&mut editor, &["one", "two", "three"]);
        editor.handle(&key(KeyCode::Char('g')));
        editor.handle(&key(KeyCode::Char('g')));
        editor.handle(&key(KeyCode::Char('V')));
        editor.handle(&key(KeyCode::Char('d')));
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.text(), "two\nthree");
    }

    #[test]
    fn visual_change_cuts_and_enters_insert_mode() {
        let mut editor = Editor::new();
        type_text(&mut editor, "hello world");
        editor.handle(&key(KeyCode::Char('0')));
        editor.handle(&key(KeyCode::Char('v')));
        editor.handle(&key(KeyCode::Char('e')));
        editor.handle(&key(KeyCode::Char('c')));
        assert_eq!(editor.mode(), Mode::Insert);
        assert_eq!(editor.text(), " world");
    }

    #[test]
    fn esc_cancels_the_visual_selection_without_editing() {
        let mut editor = Editor::new();
        type_text(&mut editor, "hello");
        editor.handle(&key(KeyCode::Char('0')));
        editor.handle(&key(KeyCode::Char('v')));
        editor.handle(&key(KeyCode::Char('e')));
        editor.handle(&key(KeyCode::Escape));
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.text(), "hello");
    }

    /// vim 的选区含光标所在字符：`v w w d` 会连第三个词的首字母一起删掉。
    #[test]
    fn visual_selection_is_inclusive_at_the_cursor() {
        let mut editor = Editor::new();
        type_text(&mut editor, "one two three");
        editor.handle(&key(KeyCode::Char('0')));
        editor.handle(&key(KeyCode::Char('v')));
        editor.handle(&key(KeyCode::Char('w')));
        editor.handle(&key(KeyCode::Char('w')));
        editor.handle(&key(KeyCode::Char('d')));
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.text(), "hree");
    }

    #[test]
    fn replace_once_replaces_one_character_and_returns_to_normal() {
        let mut editor = Editor::new();
        type_text(&mut editor, "abc");
        editor.handle(&key(KeyCode::Char('0')));
        editor.handle(&key(KeyCode::Char('r')));
        assert_eq!(editor.mode(), Mode::Replace(true));
        editor.handle(&key(KeyCode::Char('X')));
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.text(), "Xbc");
    }

    #[test]
    fn replace_overtype_replaces_until_esc() {
        let mut editor = Editor::new();
        type_text(&mut editor, "abc");
        editor.handle(&key(KeyCode::Char('0')));
        editor.handle(&key(KeyCode::Char('R')));
        assert_eq!(editor.mode(), Mode::Replace(false));
        editor.handle(&key(KeyCode::Char('X')));
        assert_eq!(
            editor.text(),
            "Xbc",
            "stays in replace mode while overwriting"
        );
        editor.handle(&key(KeyCode::Char('Y')));
        editor.handle(&key(KeyCode::Escape));
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.text(), "XYc");
    }

    #[test]
    fn replace_at_an_empty_line_appends() {
        let mut editor = Editor::new();
        type_lines(&mut editor, &["ab", ""]);
        editor.handle(&key(KeyCode::Char('G')));
        editor.handle(&key(KeyCode::Char('R')));
        editor.handle(&key(KeyCode::Char('Z')));
        editor.handle(&key(KeyCode::Escape));
        assert_eq!(editor.text(), "ab\nZ");
    }

    #[test]
    fn replace_once_on_an_empty_line_does_nothing() {
        let mut editor = Editor::new();
        type_lines(&mut editor, &["ab", ""]);
        editor.handle(&key(KeyCode::Char('G')));
        editor.handle(&key(KeyCode::Char('r')));
        editor.handle(&key(KeyCode::Char('Z')));
        assert_eq!(editor.mode(), Mode::Normal, "r replaces only once");
        assert_eq!(editor.text(), "ab\n", "nothing to replace on an empty line");
    }

    #[test]
    fn append_works_on_multibyte_lines() {
        let mut editor = Editor::new();
        type_text(&mut editor, "\u{4f60}\u{597d}");
        editor.handle(&key(KeyCode::Char('0')));
        editor.handle(&key(KeyCode::Char('a')));
        editor.handle(&key(KeyCode::Char('X')));
        editor.handle(&key(KeyCode::Escape));
        // 列是字符下标：a 之后应插在第一个字符之后，而不是跳到行尾。
        assert_eq!(editor.text(), "\u{4f60}X\u{597d}");
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
        editor.handle(&key(KeyCode::Escape));
        editor.handle(&key(KeyCode::Char('v')));
        assert_eq!(editor.mode().label(), "VISUAL");
        editor.handle(&key(KeyCode::Escape));
        editor.handle(&key(KeyCode::Char('r')));
        assert_eq!(editor.mode().label(), "REPLACE(once)");
        editor.handle(&key(KeyCode::Char('z')));
        editor.handle(&key(KeyCode::Char('R')));
        assert_eq!(editor.mode().label(), "REPLACE");
        editor.handle(&key(KeyCode::Escape));
        assert_eq!(editor.mode().label(), "NORMAL");
    }

    #[test]
    fn slash_in_normal_mode_opens_the_command_menu() {
        let mut editor = Editor::new();
        assert!(editor.command_menu().is_none());
        editor.handle(&key(KeyCode::Char('/')));
        assert_eq!(editor.mode(), Mode::Insert);
        assert_eq!(editor.text(), "/");
        // 菜单列出 pi 的全部内置命令。
        assert_eq!(editor.command_menu().map(|items| items.len()), Some(24));
    }

    #[test]
    fn typing_filters_the_command_menu_and_closes_on_arguments() {
        let mut editor = Editor::new();
        editor.handle(&key(KeyCode::Char('/')));
        for ch in "mod".chars() {
            editor.handle(&key(KeyCode::Char(ch)));
        }
        let items = editor.command_menu().expect("menu is still open");
        assert_eq!(
            items[0].name, "model",
            "prefix match comes first: {items:?}"
        );

        // 空格之后进入参数，菜单关闭。
        editor.handle(&key(KeyCode::Char(' ')));
        assert!(editor.command_menu().is_none());
    }

    #[test]
    fn tab_completes_and_enter_runs_the_selected_command() {
        let mut editor = Editor::new();
        editor.handle(&key(KeyCode::Char('/')));
        editor.handle(&key(KeyCode::Down));
        assert_eq!(editor.text(), "/");
        // 第二条是 `model`（带参数提示，补全时留一个空格）。
        editor.handle(&key(KeyCode::Tab));
        assert_eq!(editor.text(), "/model ");

        // Enter 执行不带参数的命令。
        let mut editor = Editor::new();
        editor.handle(&key(KeyCode::Char('/')));
        for ch in "quit".chars() {
            editor.handle(&key(KeyCode::Char(ch)));
        }
        assert_eq!(
            editor.handle(&key(KeyCode::Enter)),
            EditorAction::Submit("/quit".into())
        );
        assert_eq!(editor.mode(), Mode::Normal);
    }

    #[test]
    fn enter_on_a_command_with_arguments_keeps_insert_mode() {
        let mut editor = Editor::new();
        editor.handle(&key(KeyCode::Char('/')));
        editor.handle(&key(KeyCode::Down)); // `model`
        assert_eq!(editor.mode(), Mode::Insert);

        // Enter 只补全、不执行：留在 Insert 模式并留出参数位置。
        assert_eq!(editor.handle(&key(KeyCode::Enter)), EditorAction::None);
        assert_eq!(editor.text(), "/model ");
        assert_eq!(editor.mode(), Mode::Insert);

        // 接着直接输入参数即可（首字符不会被当成 vim 普通模式键位）。
        for ch in "anthropic/claude".chars() {
            editor.handle(&key(KeyCode::Char(ch)));
        }
        assert_eq!(editor.text(), "/model anthropic/claude");
        assert_eq!(editor.mode(), Mode::Insert);
    }

    #[test]
    fn unknown_slash_text_is_submitted_verbatim() {
        let mut editor = Editor::new();
        editor.handle(&key(KeyCode::Char('/')));
        for ch in "nope x".chars() {
            editor.handle(&key(KeyCode::Char(ch)));
        }
        assert!(editor.command_menu().is_none());
        editor.handle(&key(KeyCode::Escape));
        assert_eq!(
            editor.handle(&key(KeyCode::Enter)),
            EditorAction::Submit("/nope x".into())
        );
    }
}
