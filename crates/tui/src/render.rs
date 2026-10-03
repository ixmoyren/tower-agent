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
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    editor::Editor,
    model::{EntryKind, TuiState},
};

/// 输入区高度（3 行文本 + 上下边框）。
pub const EDITOR_HEIGHT: u16 = 5;

/// 把当前状态渲染到 `frame`。
pub fn render(frame: &mut Frame, state: &TuiState, editor: &Editor) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(EDITOR_HEIGHT),
        Constraint::Length(1),
    ])
    .split(area);

    render_header(frame, chunks[0], state);
    render_transcript(frame, chunks[1], state);
    render_editor(frame, chunks[2], editor);
    render_footer(frame, chunks[3], state);
}

/// 输入区：`ratatui-textarea` 自带边框标题（模式）与行号样式。
fn render_editor(frame: &mut Frame, area: Rect, editor: &Editor) {
    frame.render_widget(editor.textarea(), area);
}

fn render_header(frame: &mut Frame, area: Rect, state: &TuiState) {
    let line = Line::from(vec![
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
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// transcript 区域：按宽度折行后，从底部对齐并应用滚动偏移。
fn render_transcript(frame: &mut Frame, area: Rect, state: &TuiState) {
    let lines = transcript_lines(state, area.width as usize);
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
    let hints = "Enter 提交 · i 插入 · Esc 普通 · q 退出 · PgUp/PgDn 滚动记录";
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
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in state.entries() {
        if !lines.is_empty() {
            lines.push(Line::default());
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
    }
}

fn style_for(kind: EntryKind) -> Style {
    match kind {
        EntryKind::User => Style::default().fg(Color::Cyan),
        EntryKind::Assistant => Style::default(),
        EntryKind::System => Style::default().fg(Color::DarkGray),
        EntryKind::Tool => Style::default().fg(Color::Yellow),
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
        // "你好" is 4 columns wide, so only two characters fit in 4 columns.
        assert_eq!(wrap_line("你好世界", 4), vec!["你好", "世界"]);
    }

    #[test]
    fn layout_reserves_space_for_the_editor_with_line_numbers() {
        use ratatui::termina::event::{Event, KeyCode, KeyEvent, Modifiers};

        fn key(code: KeyCode) -> Event {
            Event::Key(KeyEvent::new(code, Modifiers::NONE))
        }

        let mut state = TuiState::new("t");
        state.push_message(&agent::AgentMessage::Message(ai::Message::User(
            ai::UserMessage {
                content: ai::UserContent::Text("hi".into()),
                timestamp: 0,
            },
        )));

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

        let backend = ratatui::backend::TestBackend::new(30, 14);
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

        assert!(rendered.contains("NORMAL"), "模式标题在输入区:\n{rendered}");
        assert!(rendered.contains("1 hello"), "行号 + 第一行:\n{rendered}");
        assert!(rendered.contains("2 world"), "行号 + 第二行:\n{rendered}");
        assert!(rendered.contains("Enter"), "提示栏:\n{rendered}");
        assert!(rendered.contains("Esc"), "提示栏含 Esc 提示:\n{rendered}");
        assert!(
            rendered.contains("› hi"),
            "transcript 仍在上面:\n{rendered}"
        );
    }

    #[test]
    fn transcript_lines_prefixes_user_messages() {
        let mut state = TuiState::new("t");
        state.push_message(&agent::AgentMessage::Message(ai::Message::User(
            ai::UserMessage {
                content: ai::UserContent::Text("hi".into()),
                timestamp: 0,
            },
        )));
        let lines = transcript_lines(&state, 40);
        assert_eq!(lines.len(), 1);
        let rendered: String = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(rendered, "› hi");
    }
}
