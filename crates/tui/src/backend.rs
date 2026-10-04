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

use crate::{
    app::Tui,
    command::{self, CommandHost},
    editor::EditorAction,
    model::TuiState,
};

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
///
/// 只有与编辑器无关的全局按键会走到这里；其余按键交给 [`crate::Editor`]（vim 键位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 退出。
    Quit,
    /// 向上翻一屏。
    PageUp,
    /// 向下翻一屏。
    PageDown,
    /// 交给编辑器处理。
    None,
}

/// 全局按键 → 动作：`Ctrl+C` 退出，`PageUp`/`PageDown` 滚动会话记录。
///
/// `q`、`Esc`、方向键等都归编辑器（vim），不在这里处理。
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
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        _ => Action::None,
    }
}

/// 订阅 agent 事件并渲染 TUI，直到用户退出（生产入口）。
///
/// `submissions` 把用户在输入区提交的文本交给调用方，由调用方驱动 agent。
pub async fn run_agent_tui(
    mut subscription: Subscription,
    submissions: mpsc::UnboundedSender<String>,
    host: Arc<dyn CommandHost>,
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
    let result = run_tui(rx, submissions, host, title).await;
    pump.abort();
    result
}

/// 用一条事件通道驱动 TUI（[`run_agent_tui`] 与调用方自定义驱动都走这里）。
pub async fn run_tui(
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    submissions: mpsc::UnboundedSender<String>,
    host: Arc<dyn CommandHost>,
    title: impl Into<String>,
) -> io::Result<()> {
    let (terminal, reader) = init_terminal()?;
    let mut tui = Tui::new(terminal, TuiState::new(title));
    let mut input = spawn_input_reader(reader);

    let result = run_loop(
        &mut tui,
        &mut events,
        &mut input,
        &submissions,
        host.as_ref(),
    )
        .await;

    let mut terminal = tui.into_terminal();
    let restored = restore_terminal(&mut terminal);
    result.and(restored)
}

async fn run_loop(
    tui: &mut Tui<TerminaBackend<PlatformTerminal>>,
    events: &mut mpsc::UnboundedReceiver<AgentEvent>,
    input: &mut InputReader,
    submissions: &mpsc::UnboundedSender<String>,
    host: &dyn CommandHost,
) -> io::Result<()> {
    tui.draw()?;
    loop {
        tokio::select! {
            Some(event) = events.recv() => tui.handle(&event),
            Some(key) = input.recv() => {
                match action_for(&key) {
                    Action::Quit => return Ok(()),
                    Action::PageUp => tui.scroll_up(10),
                    Action::PageDown => tui.scroll_down(10),
                    Action::None => match tui.handle_input(&key) {
                        EditorAction::Quit => return Ok(()),
                        EditorAction::Submit(text) => {
                            match text.strip_prefix('/') {
                                // 斜杠命令由 TUI 自己执行（pi 的内置命令在这里生效）。
                                Some(command) => {
                                    if !run_slash_command(tui, host, command) {
                                        return Ok(());
                                    }
                                }
                                // 普通输入交给调用方驱动 agent；接收端没了就忽略。
                                None => {
                                    let _ = submissions.send(text);
                                }
                            }
                        }
                        EditorAction::None => {}
                    },
                }
            }
            // 两个通道都关闭（agent 结束且输入线程退出）时收尾。
            else => return Ok(()),
        }
        tui.draw()?;
    }
}

/// 执行一条斜杠命令；返回 `false` 表示用户要退出 TUI（`/quit`）。
///
/// 命令名来自 [`command::BUILTIN_SLASH_COMMANDS`]，因此菜单里能看到的命令这里一定认；
/// 已经实现的会真的执行，其余给出明确的“尚未移植”提示而不是静默失败。
fn run_slash_command(
    tui: &mut Tui<TerminaBackend<PlatformTerminal>>,
    host: &dyn CommandHost,
    input: &str,
) -> bool {
    match evaluate_command(host, input) {
        CommandOutcome::Quit => false,
        CommandOutcome::Clear(notice) => {
            tui.state_mut().clear();
            tui.state_mut().push_notice(notice);
            true
        }
        CommandOutcome::Notice(notice) => {
            tui.state_mut().push_notice(notice);
            true
        }
    }
}

/// 斜杠命令的执行结果（与终端状态无关，便于单测）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum CommandOutcome {
    /// 退出 TUI（`/quit`）。
    Quit,
    /// 清空 transcript 后给出提示（`/new`）。
    Clear(String),
    /// 追加一条提示。
    Notice(String),
}

fn evaluate_command(host: &dyn CommandHost, input: &str) -> CommandOutcome {
    let mut parts = input.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let arg = parts.next().map(str::trim).filter(|arg| !arg.is_empty());

    let Some(command) = command::lookup(name) else {
        return CommandOutcome::Notice(format!(
            "unknown command: /{name} (type `/` for available commands)"
        ));
    };

    match command.name {
        "quit" => CommandOutcome::Quit,
        "new" => match host.new_session() {
            Ok(()) => CommandOutcome::Clear("Started a new session.".into()),
            Err(error) => CommandOutcome::Notice(format!("/new failed: {error}")),
        },
        "session" => CommandOutcome::Notice(host.session_info().join("\n")),
        "model" => notice(host.model(arg)),
        "thinking" => notice(host.thinking(arg)),
        "hotkeys" => CommandOutcome::Notice(command::hotkeys().join("\n")),
        _ => CommandOutcome::Notice(format!(
            "/{name} is not ported to tower-pi-agent yet (pi description: {}).",
            command.description
        )),
    }
}

fn notice(result: Result<String, String>) -> CommandOutcome {
    match result {
        Ok(text) => CommandOutcome::Notice(text),
        Err(error) => CommandOutcome::Notice(format!("error: {error}")),
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
    fn ctrl_c_is_the_only_quit_key() {
        assert_eq!(
            action_for(&Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                Modifiers::CONTROL
            ))),
            Action::Quit
        );
        // `q` / `Esc` 归编辑器（vim），不再当作全局退出键。
        assert_eq!(action_for(&key(KeyCode::Char('q'))), Action::None);
        assert_eq!(action_for(&key(KeyCode::Escape)), Action::None);
    }

    #[test]
    fn page_keys_scroll_the_transcript() {
        assert_eq!(action_for(&key(KeyCode::PageUp)), Action::PageUp);
        assert_eq!(action_for(&key(KeyCode::PageDown)), Action::PageDown);
        // 方向键归编辑器。
        assert_eq!(action_for(&key(KeyCode::Up)), Action::None);
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

    /// 一个记录调用的假宿主；命令执行只依赖 [`CommandHost`]，不需要真会话。
    struct FakeHost {
        new_session: Result<(), String>,
    }

    impl CommandHost for FakeHost {
        fn new_session(&self) -> Result<(), String> {
            self.new_session.clone()
        }

        fn session_info(&self) -> Vec<String> {
            vec!["cwd: /tmp".into(), "model: faux".into()]
        }

        fn model(&self, arg: Option<&str>) -> Result<String, String> {
            Ok(match arg {
                Some(arg) => format!("model switched to: {arg}"),
                None => "current model: faux".into(),
            })
        }

        fn thinking(&self, arg: Option<&str>) -> Result<String, String> {
            Ok(match arg {
                Some(arg) => format!("thinking level set to: {arg}"),
                None => "current thinking level: off".into(),
            })
        }
    }

    fn fake_host() -> FakeHost {
        FakeHost {
            new_session: Ok(()),
        }
    }

    #[test]
    fn quit_asks_the_loop_to_exit() {
        assert_eq!(evaluate_command(&fake_host(), "quit"), CommandOutcome::Quit);
    }

    #[test]
    fn new_clears_the_transcript_or_reports_the_failure() {
        assert_eq!(
            evaluate_command(&fake_host(), "new"),
            CommandOutcome::Clear("Started a new session.".into())
        );
        let busy = FakeHost {
            new_session: Err("Agent is already processing".into()),
        };
        assert_eq!(
            evaluate_command(&busy, "new"),
            CommandOutcome::Notice("/new failed: Agent is already processing".into())
        );
    }

    #[test]
    fn implemented_commands_pass_arguments_to_the_host() {
        assert_eq!(
            evaluate_command(&fake_host(), "model anthropic/claude-x"),
            CommandOutcome::Notice("model switched to: anthropic/claude-x".into())
        );
        assert_eq!(
            evaluate_command(&fake_host(), "thinking high"),
            CommandOutcome::Notice("thinking level set to: high".into())
        );
        assert_eq!(
            evaluate_command(&fake_host(), "session"),
            CommandOutcome::Notice("cwd: /tmp\nmodel: faux".into())
        );
        match evaluate_command(&fake_host(), "hotkeys") {
            CommandOutcome::Notice(text) => {
                assert!(text.contains("command menu"), "got: {text}");
            }
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    #[test]
    fn unported_and_unknown_commands_are_reported() {
        match evaluate_command(&fake_host(), "compact") {
            CommandOutcome::Notice(text) => {
                assert!(text.contains("not ported"), "got: {text}");
            }
            other => panic!("expected a notice, got {other:?}"),
        }
        match evaluate_command(&fake_host(), "nope") {
            CommandOutcome::Notice(text) => {
                assert!(text.contains("unknown command"), "got: {text}");
            }
            other => panic!("expected a notice, got {other:?}"),
        }
    }
}
