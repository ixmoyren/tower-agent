//! 具体终端后端与事件循环。
//!
//! **这是整个 crate 里唯一与具体后端耦合的地方。** 当前用 ratatui 的 crossterm 后端
//! （`ratatui::backend::CrosstermBackend`，ratatui 0.30 的默认后端）。换成 termion/termwiz
//! 只需要改本文件的 `init_terminal` / `run_agent_tui`，上层 [`crate::Tui`] 与渲染代码
//! 完全不用动。
//!
//! 输入读取放在独立的阻塞线程里（`crossterm::event::read`），再把事件送进 tokio 通道，
//! 这样事件循环可以同时 select agent 事件流与键盘输入，而不用依赖 crossterm 的
//! `event-stream` 特性。

use std::io::{self, Stdout};

use agent::Subscription;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    crossterm::{
        event::{Event, KeyCode, KeyEventKind, KeyModifiers},
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
};
use tokio::sync::mpsc;

use crate::{app::Tui, model::TuiState};

/// 进入备用屏幕并打开 raw mode。
pub fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

/// 恢复终端（离开备用屏幕、关闭 raw mode）。
pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}

/// 启动一个阻塞的键盘读取线程，把事件送进通道。
pub fn spawn_input_reader() -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = ratatui::crossterm::event::read() {
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    rx
}

/// 把一次按键映射成 UI 动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 退出。
    Quit,
    /// 向上滚动一行。
    ScrollUp,
    /// 向下滚动一行。
    ScrollDown,
    /// 向上翻一屏。
    PageUp,
    /// 向下翻一屏。
    PageDown,
    /// 回到最新内容。
    Bottom,
    /// 忽略。
    None,
}

/// 按键 → 动作（`q` / `Esc` / `Ctrl+C` 退出，方向键与 PageUp/PageDown 滚动）。
pub fn action_for(event: Event) -> Action {
    let Event::Key(key) = event else {
        return Action::None;
    };
    // Windows 会同时上报按下与抬起，只处理按下。
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Up => Action::ScrollUp,
        KeyCode::Down => Action::ScrollDown,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Home | KeyCode::End => Action::Bottom,
        _ => Action::None,
    }
}

/// 订阅 agent 事件、渲染 TUI，直到用户退出。
pub async fn run_agent_tui(subscription: Subscription, title: impl Into<String>) -> io::Result<()> {
    let terminal = init_terminal()?;
    let mut tui = Tui::new(terminal, TuiState::new(title));
    let result = run_loop(&mut tui, subscription).await;
    let mut terminal = tui.into_terminal();
    restore_terminal(&mut terminal)?;
    result
}

async fn run_loop(
    tui: &mut Tui<CrosstermBackend<Stdout>>,
    mut subscription: Subscription,
) -> io::Result<()> {
    let mut input = spawn_input_reader();
    tui.draw()?;

    loop {
        tokio::select! {
            Some(event) = input.recv() => {
                match action_for(event) {
                    Action::Quit => return Ok(()),
                    Action::ScrollUp => tui.scroll_up(1),
                    Action::ScrollDown => tui.scroll_down(1),
                    Action::PageUp => tui.scroll_up(10),
                    Action::PageDown => tui.scroll_down(10),
                    Action::Bottom => tui.scroll_to_bottom(),
                    Action::None => continue,
                }
            }
            event = subscription.recv() => {
                match event {
                    Some(event) => tui.handle(&event),
                    None => return Ok(()),
                }
            }
        }
        tui.draw()?;
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyEvent, KeyEventState};

    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn quit_keys_map_to_quit() {
        assert_eq!(action_for(key(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(action_for(key(KeyCode::Esc)), Action::Quit);
        assert_eq!(
            action_for(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            ))),
            Action::Quit
        );
    }

    #[test]
    fn scroll_keys_map_to_scroll_actions() {
        assert_eq!(action_for(key(KeyCode::Up)), Action::ScrollUp);
        assert_eq!(action_for(key(KeyCode::Down)), Action::ScrollDown);
        assert_eq!(action_for(key(KeyCode::PageUp)), Action::PageUp);
        assert_eq!(action_for(key(KeyCode::PageDown)), Action::PageDown);
        assert_eq!(action_for(key(KeyCode::Char('x'))), Action::None);
    }

    #[test]
    fn key_release_events_are_ignored() {
        let release = Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        });
        assert_eq!(action_for(release), Action::None);
    }
}
