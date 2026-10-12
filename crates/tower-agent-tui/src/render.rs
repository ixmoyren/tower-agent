//! ratatui 渲染：把 [`TuiState`] 画成一个头部状态栏 + transcript + 底部提示的三段布局。
//!
//! 渲染是纯函数（`&TuiState` → 缓冲区），实际写终端的差异由 ratatui 的
//! [`ratatui::Terminal::draw`] 负责：它只把变化的单元格写到后端，
//! 这就是 design.md 要求的"差分渲染"。

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    editor::Editor,
    model::{EntryKind, TuiState},
    plugin::UiPlugin,
};

/// 输入区高度（3 行文本 + 上下边框）。
pub const EDITOR_HEIGHT: u16 = 5;

/// 斜杠命令菜单里最多同时显示几项（pi 的 `autocompleteMaxVisible` 默认是 5）。
const MENU_MAX_ITEMS: usize = 5;

/// 把当前状态渲染到 `frame`。
pub fn render(frame: &mut Frame, state: &TuiState, editor: &Editor) {
    render_with_plugins(frame, state, editor, &[]);
}

/// 带 UI 插件的渲染：插件可追加状态栏片段、覆盖工具结果渲染并绘制浮层。
pub fn render_with_plugins(
    frame: &mut Frame,
    state: &TuiState,
    editor: &Editor,
    plugins: &[Box<dyn UiPlugin>],
) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(EDITOR_HEIGHT),
        Constraint::Length(1),
    ])
    .split(area);

    render_header(frame, chunks[0], state, plugins);
    render_transcript(frame, chunks[1], state, plugins);
    render_editor(frame, chunks[2], editor);
    render_command_menu(frame, chunks[2], editor);
    render_footer(frame, chunks[3], state);

    for plugin in plugins {
        plugin.render_overlay(area, frame.buffer_mut());
    }
}

/// 输入区：`ratatui-textarea` 自带边框标题（模式）与行号样式。
fn render_editor(frame: &mut Frame, area: Rect, editor: &Editor) {
    frame.render_widget(editor.textarea(), area);
}

/// 斜杠命令菜单：悬浮在输入区正上方，最多显示 [`MENU_MAX_ITEMS`] 项。
fn render_command_menu(frame: &mut Frame, area: Rect, editor: &Editor) {
    let Some(items) = editor.command_menu() else {
        return;
    };
    if items.is_empty() || area.height == 0 || area.width < 8 {
        return;
    }

    let visible = items.len().min(MENU_MAX_ITEMS);
    let height = u16::try_from(visible).unwrap_or(1) + 2;
    let height = height.min(area.height);
    let rect = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(height),
        width: area.width.min(80),
        height,
    };

    // 选中项始终留在可见窗口里（与 pi 的 autocomplete 列表一致）。
    let selected = editor.menu_selection().min(items.len() - 1);
    let start = selected
        .saturating_sub(MENU_MAX_ITEMS - 1)
        .min(items.len().saturating_sub(MENU_MAX_ITEMS));
    let rows: Vec<ListItem<'_>> = items
        .iter()
        .enumerate()
        .skip(start)
        .take(MENU_MAX_ITEMS)
        .map(|(index, command)| {
            let mut spans = vec![Span::styled(
                format!("/{}", command.name),
                Style::default().fg(Color::Cyan),
            )];
            if let Some(hint) = command.argument_hint {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(hint, Style::default().fg(Color::DarkGray)));
            }
            if !command.ported {
                spans.push(Span::styled(
                    " · not ported",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                command.description,
                Style::default().fg(Color::DarkGray),
            ));
            let item = ListItem::new(Line::from(spans));
            if index == selected {
                item.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                item
            }
        })
        .collect();

    let menu = List::new(rows).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Commands (Tab complete · Enter select) "),
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(menu, rect);
}

fn render_header(frame: &mut Frame, area: Rect, state: &TuiState, plugins: &[Box<dyn UiPlugin>]) {
    let mut spans = vec![
        Span::styled(
            state.title().to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            state.status_line(),
            Style::default().fg(if state.is_running() {
                Color::Green
            } else {
                Color::DarkGray
            }),
        ),
    ];
    for plugin in plugins {
        if let Some(segment) = plugin.status_segment() {
            spans.push(Span::raw("  "));
            spans.extend(segment.spans);
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// transcript 区域：按宽度折行后，从底部对齐并应用滚动偏移。
fn render_transcript(
    frame: &mut Frame,
    area: Rect,
    state: &TuiState,
    plugins: &[Box<dyn UiPlugin>],
) {
    let lines = transcript_lines_with_plugins(state, area.width as usize, plugins);
    let height = area.height as usize;
    let max_scroll = lines.len().saturating_sub(height);
    // `scroll` 以"距离底部"计，Paragraph 的偏移以"距离顶部"计。
    let offset = max_scroll.saturating_sub(state.scroll() as usize);
    let paragraph = Paragraph::new(Text::from(lines));
    frame.render_widget(
        paragraph.scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0)),
        area,
    );
}

fn render_footer(frame: &mut Frame, area: Rect, state: &TuiState) {
    let hints = "Enter submit · / commands · i insert · Esc normal · q quit · PgUp/PgDn scroll";
    let mut spans = vec![Span::styled(hints, Style::default().fg(Color::DarkGray))];
    if let Some(error) = state.error() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("error: {error}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// 把状态折成按宽度折行后的渲染行。
pub fn transcript_lines(state: &TuiState, width: usize) -> Vec<Line<'static>> {
    transcript_lines_with_plugins(state, width, &[])
}

/// 带 UI 插件的 [`transcript_lines`]：工具结果先问插件，再退回内置渲染。
pub fn transcript_lines_with_plugins(
    state: &TuiState,
    width: usize,
    plugins: &[Box<dyn UiPlugin>],
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in state.entries() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        if entry.kind == EntryKind::Tool
            && let Some(tool) = &entry.tool
            && let Some(result) = &tool.result
            && let Some(custom) = plugins
                .iter()
                .find_map(|plugin| plugin.render_tool_result(&tool.name, result, width))
        {
            lines.extend(custom);
            continue;
        }
        lines.extend(entry_lines(entry.kind, &entry.text, width));
    }
    if let Some(streaming) = state.streaming() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let mut streaming_lines = entry_lines(EntryKind::Assistant, streaming, width);
        if let Some(last) = streaming_lines.last_mut() {
            last.spans
                .push(Span::styled("▌", Style::default().fg(Color::Green)));
        }
        lines.extend(streaming_lines);
    }
    lines
}

fn entry_lines(kind: EntryKind, text: &str, width: usize) -> Vec<Line<'static>> {
    let style = style_for(kind);
    let prefix = prefix_for(kind);
    let indent = " ".repeat(UnicodeWidthStr::width(prefix));
    let body_width = width.saturating_sub(UnicodeWidthStr::width(prefix)).max(1);

    let logical: Vec<&str> = if text.is_empty() {
        vec![""]
    } else {
        text.split('\n').collect()
    };

    let mut lines = Vec::new();
    for (index, logical_line) in logical.iter().enumerate() {
        let wrapped = wrap_line(logical_line, body_width);
        for (wrapped_index, piece) in wrapped.iter().enumerate() {
            let lead = if index == 0 && wrapped_index == 0 {
                prefix.to_string()
            } else {
                indent.clone()
            };
            lines.push(Line::from(vec![
                Span::styled(lead, style),
                Span::styled(piece.clone(), style),
            ]));
        }
    }
    lines
}

fn prefix_for(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::User => "› ",
        EntryKind::Assistant => "",
        EntryKind::System => "· ",
        EntryKind::Tool => "",
        EntryKind::Notice => "» ",
    }
}

fn style_for(kind: EntryKind) -> Style {
    match kind {
        EntryKind::User => Style::default().fg(Color::Cyan),
        EntryKind::Assistant => Style::default(),
        EntryKind::System => Style::default().fg(Color::DarkGray),
        EntryKind::Tool => Style::default().fg(Color::Yellow),
        EntryKind::Notice => Style::default().fg(Color::LightBlue),
    }
}

/// 按显示宽度折行，优先在空格处断开（宽字符按 2 列算）。
fn wrap_line(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in text.split(' ') {
        let mut word = word;
        // 比整行还宽的词直接硬切。
        while display_width(word) > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            let (head, tail) = split_at_width(word, width);
            lines.push(head);
            word = tail;
        }
        if word.is_empty() {
            continue;
        }
        let word_width = display_width(word);
        if current_width == 0 {
            current.push_str(word);
            current_width = word_width;
        } else if current_width + 1 + word_width <= width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
            current_width = word_width;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// 在不超过 `width` 列处切分，返回（头、尾）。
fn split_at_width(text: &str, width: usize) -> (String, &str) {
    let mut taken = 0usize;
    for (index, ch) in text.char_indices() {
        let ch_width = UnicodeWidthStr::width(ch.to_string().as_str());
        if taken + ch_width > width {
            return (text[..index].to_string(), &text[index..]);
        }
        taken += ch_width;
    }
    (text.to_string(), "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_line_breaks_on_spaces() {
        assert_eq!(wrap_line("hello world", 5), vec!["hello", "world"]);
        assert_eq!(wrap_line("hello", 5), vec!["hello"]);
        assert_eq!(wrap_line("", 5), vec![""]);
    }

    #[test]
    fn wrap_line_hard_breaks_long_words() {
        assert_eq!(wrap_line("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn wrap_line_counts_wide_characters_as_two_columns() {
        // Each CJK char is 2 columns wide, so only two of them fit in 4 columns.
        let text = "\u{4f60}\u{597d}\u{4e16}\u{754c}";
        assert_eq!(
            wrap_line(text, 4),
            vec!["\u{4f60}\u{597d}", "\u{4e16}\u{754c}"]
        );
    }

    #[test]
    fn layout_reserves_space_for_the_editor_with_line_numbers() {
        use ratatui::termina::event::{Event, KeyCode, KeyEvent, Modifiers};

        fn key(code: KeyCode) -> Event {
            Event::Key(KeyEvent::new(code, Modifiers::NONE))
        }

        let mut state = TuiState::new("t");
        state.push_message(&tower_agent_runtime::AgentMessage::Message(
            tower_agent_provider::Message::User(tower_agent_provider::UserMessage {
                content: tower_agent_provider::UserContent::Text("hi".into()),
                timestamp: 0,
            }),
        ));

        // 用真实按键输入两行，验证行号与模式标题。
        let mut editor = Editor::new();
        editor.handle(&key(KeyCode::Char('i')));
        for ch in "hello".chars() {
            editor.handle(&key(KeyCode::Char(ch)));
        }
        editor.handle(&key(KeyCode::Enter));
        for ch in "world".chars() {
            editor.handle(&key(KeyCode::Char(ch)));
        }
        editor.handle(&key(KeyCode::Escape));

        // 提示栏现在还要放 `/ 命令` 的提示，用宽一点的终端保证整条提示都能渲染出来。
        let backend = ratatui::backend::TestBackend::new(72, 14);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &state, &editor))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                rendered.push_str(buffer[(x, y)].symbol());
            }
            rendered.push('\n');
        }

        assert!(
            rendered.contains("NORMAL"),
            "mode title in the input area:\n{rendered}"
        );
        assert!(
            rendered.contains("1 hello"),
            "line number + first line:\n{rendered}"
        );
        assert!(
            rendered.contains("2 world"),
            "line number + second line:\n{rendered}"
        );
        assert!(rendered.contains("Enter"), "footer hint bar:\n{rendered}");
        assert!(
            rendered.contains("Esc"),
            "footer hint bar mentions Esc:\n{rendered}"
        );
        assert!(
            rendered.contains("› hi"),
            "transcript is still above:\n{rendered}"
        );
    }

    #[test]
    fn transcript_lines_prefixes_user_messages() {
        let mut state = TuiState::new("t");
        state.push_message(&tower_agent_runtime::AgentMessage::Message(
            tower_agent_provider::Message::User(tower_agent_provider::UserMessage {
                content: tower_agent_provider::UserContent::Text("hi".into()),
                timestamp: 0,
            }),
        ));
        let lines = transcript_lines(&state, 40);
        assert_eq!(lines.len(), 1);
        let rendered: String = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(rendered, "› hi");
    }

    #[test]
    fn slash_menu_lists_pi_commands_above_the_editor() {
        use ratatui::termina::event::{Event, KeyCode, KeyEvent, Modifiers};

        let state = TuiState::new("t");
        let mut editor = Editor::new();
        // 普通模式下打 `/` 打开菜单（等价于 pi 里在输入框敲斜杠）。
        editor.handle(&Event::Key(KeyEvent::new(
            KeyCode::Char('/'),
            Modifiers::NONE,
        )));

        let backend = ratatui::backend::TestBackend::new(72, 16);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &state, &editor))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                rendered.push_str(buffer[(x, y)].symbol());
            }
            rendered.push('\n');
        }

        // 宽字符在缓冲区里各占两列，断言只看 ASCII 的命令名。
        assert!(rendered.contains("/settings"), "first command:\n{rendered}");
        assert!(
            rendered.contains("/model"),
            "menu contains model:\n{rendered}"
        );
    }

    #[test]
    fn notices_are_rendered_with_their_own_prefix() {
        let mut state = TuiState::new("t");
        state.push_notice("Started a new session.");
        let rendered: String = transcript_lines(&state, 40)[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(rendered, "» Started a new session.");
    }
}
