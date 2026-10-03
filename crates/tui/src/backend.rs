//! termina 后端与事件循环。
//!
//! **这是整个 crate 里唯一与具体后端耦合的地方。** 当前用 ratatui 0.30 的
//! `ratatui-termina` 后端（`ratatui::backend::TerminaBackend`，对应 manifest 里的 `termina`
//! feature）；换 crossterm/termion/termwiz 只需改本文件，上层 [`crate::Tui`] 与渲染代码
//! 完全不用动。
//!
//! 终端设置沿用 `ratatui-termina` 官方示例的做法：应用自己负责 raw mode、备用屏幕与光标
//! （backend 不代劳）。`termina` 的 `Drop` 会恢复 cooked mode，所以退出时只需离开备用屏幕
//! 并显示光标。
//!
//! 输入读取放在独立的阻塞线程里（`EventReader::poll` 带超时，便于响应停止标志），再把事件
//! 送进 tokio 通道，这样事件循环可以同时 select agent 事件流与键盘输入。

use std::{
    io::{self, Write as _},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agent::{AgentEvent, Subscription};
use ratatui::{
    Terminal,
    backend::TerminaBackend,
    termina::{
        Event, EventReader, PlatformTerminal, Terminal as _,
        escape::csi::{Csi, DecPrivateMode, DecPrivateModeCode, Mode},
        event::{KeyCode, KeyEventKind, Modifiers},
    },
};
use tokio::sync::mpsc;

use crate::{app::Tui, model::TuiState};

/// 本 crate 使用的终端类型（termina 后端 + 平台终端）。
pub type AppTerminal = Terminal<TerminaBackend<PlatformTerminal>>;

/// 输入线程轮询间隔：既让停止标志被及时看到，也不至于空转烧 CPU。
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);

fn set_dec_mode(code: DecPrivateModeCode) -> Csi {
    Csi::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(code)))
}

fn reset_dec_mode(code: DecPrivateModeCode) -> Csi {
    Csi::Mode(Mode::ResetDecPrivateMode(DecPrivateMode::Code(code)))
}

/// 进入 raw mode、切到备用屏幕，返回终端与事件读取器。
pub fn init_terminal() -> io::Result<(AppTerminal, EventReader)> {
    let mut output = PlatformTerminal::new()?;
    output.enter_raw_mode()?;
    write!(
        output,
        "{}{}",
        set_dec_mode(DecPrivateModeCode::ClearAndEnableAlternateScreen),
        set_dec_mode(DecPrivateModeCode::ShowCursor)
    )?;
    output.flush()?;

    // 事件读取器要在终端被 backend 接管前取出来。
    let events = output.event_reader();
    let terminal = Terminal::new(TerminaBackend::new(output))?;
    Ok((terminal, events))
}

/// 离开备用屏幕并显示光标（cooked mode 由 `termina` 的 `Drop` 恢复）。
pub fn restore_terminal(terminal: &mut AppTerminal) -> io::Result<()> {
    let backend = terminal.backend_mut();
    write!(
        backend,
        "{}{}",
        reset_dec_mode(DecPrivateModeCode::ClearAndEnableAlternateScreen),
        set_dec_mode(DecPrivateModeCode::ShowCursor)
    )?;
    backend.flush()
}

/// 只接受按键事件（鼠标/尺寸事件留给后续步骤）。
fn key_filter(event: &Event) -> bool {
    matches!(event, Event::Key(_))
}

/// 键盘输入的接收端；drop 时通知读取线程退出。
pub struct InputReader {
    rx: mpsc::UnboundedReceiver<Event>,
    stop: Arc<AtomicBool>,
}

impl InputReader {
    /// 取下一个按键事件；输入线程退出后返回 `None`。
    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// 启动阻塞的按键读取线程，把事件送进通道。
pub fn spawn_input_reader(reader: EventReader) -> InputReader {
    let (tx, rx) = mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    std::thread::spawn(move || {
        while !thread_stop.load(Ordering::SeqCst) {
            match reader.poll(Some(INPUT_POLL_INTERVAL), key_filter) {
                Ok(true) => match reader.read(key_filter) {
                    Ok(event) => {
                        if tx.send(event).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                // 超时：回到循环顶部检查停止标志。
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
    InputReader { rx, stop }
}

/// 一次按键映射成的 UI 动作。
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
pub fn action_for(event: &Event) -> Action {
    let Event::Key(key) = event else {
        return Action::None;
    };
    // Windows 旧控制台与增强键盘协议会同时上报按下与抬起，只处理按下。
    if key.kind != KeyEventKind::Press {
        return Action::None;
    }
    if key.modifiers.contains(Modifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Escape => Action::Quit,
        KeyCode::Up => Action::ScrollUp,
        KeyCode::Down => Action::ScrollDown,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Home | KeyCode::End => Action::Bottom,
        _ => Action::None,
    }
}

/// 订阅 agent 事件并渲染 TUI，直到用户退出（生产入口）。
pub async fn run_agent_tui(
    mut subscription: Subscription,
    title: impl Into<String>,
) -> io::Result<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    let pump = tokio::spawn(async move {
        while let Some(event) = subscription.recv().await {
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    let result = run_tui(rx, title).await;
    pump.abort();
    result
}

/// 用一条事件通道驱动 TUI（[`run_agent_tui`] 与 `tui` 二进制都走这里）。
pub async fn run_tui(
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    title: impl Into<String>,
) -> io::Result<()> {
    let (terminal, reader) = init_terminal()?;
    let mut tui = Tui::new(terminal, TuiState::new(title));
    let mut input = spawn_input_reader(reader);

    let result = run_loop(&mut tui, &mut events, &mut input).await;

    let mut terminal = tui.into_terminal();
    let restored = restore_terminal(&mut terminal);
    result.and(restored)
}

async fn run_loop(
    tui: &mut Tui<TerminaBackend<PlatformTerminal>>,
    events: &mut mpsc::UnboundedReceiver<AgentEvent>,
    input: &mut InputReader,
) -> io::Result<()> {
    tui.draw()?;
    loop {
        tokio::select! {
            Some(event) = events.recv() => tui.handle(&event),
            Some(key) = input.recv() => {
                match action_for(&key) {
                    Action::Quit => return Ok(()),
                    Action::ScrollUp => tui.scroll_up(1),
                    Action::ScrollDown => tui.scroll_down(1),
                    Action::PageUp => tui.scroll_up(10),
                    Action::PageDown => tui.scroll_down(10),
                    Action::Bottom => tui.scroll_to_bottom(),
                    Action::None => continue,
                }
            }
            // 两个通道都关闭（agent 结束且输入线程退出）时收尾。
            else => return Ok(()),
        }
        tui.draw()?;
    }
}

#[cfg(test)]
mod tests {
    use ratatui::termina::event::{KeyEvent, KeyEventState};

    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, Modifiers::NONE))
    }

    #[test]
    fn quit_keys_map_to_quit() {
        assert_eq!(action_for(&key(KeyCode::Char('q'))), Action::Quit);
        assert_eq!(action_for(&key(KeyCode::Escape)), Action::Quit);
        assert_eq!(
            action_for(&Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                Modifiers::CONTROL
            ))),
            Action::Quit
        );
    }

    #[test]
    fn scroll_keys_map_to_scroll_actions() {
        assert_eq!(action_for(&key(KeyCode::Up)), Action::ScrollUp);
        assert_eq!(action_for(&key(KeyCode::Down)), Action::ScrollDown);
        assert_eq!(action_for(&key(KeyCode::PageUp)), Action::PageUp);
        assert_eq!(action_for(&key(KeyCode::PageDown)), Action::PageDown);
        assert_eq!(action_for(&key(KeyCode::Home)), Action::Bottom);
        assert_eq!(action_for(&key(KeyCode::Char('x'))), Action::None);
    }

    #[test]
    fn non_press_events_are_ignored() {
        let release = Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: Modifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        });
        assert_eq!(action_for(&release), Action::None);
        assert_eq!(action_for(&Event::FocusIn), Action::None);
    }
}
