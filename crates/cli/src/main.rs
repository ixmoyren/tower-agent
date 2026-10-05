//! `cli` — tower-pi-agent 的命令行入口（对应 pi `packages/coding-agent`）。
//!
//! 组装一次会话：内置工具 + agent 编排循环 + （TUI 或纯文本）事件渲染。
//! provider 由 `--provider` / `--model` 选择，可接真实的 anthropic / openai / google
//! provider（凭据来自环境变量）；没显式指定时按环境里的凭据挑一个。
//!
//! 本 crate 负责会话入口与命令行解析，也是 workspace 里**唯一**的可执行入口
//! （`src/main.rs`）：
//!
//! - [`session`]：会话入口，把 `agent::Agent`、`tools` crate 的内置工具与事件流
//!   （TUI 或纯文本）拼起来。
//! - [`args`]：命令行参数解析（`clap` derive，帮助/版本/错误由 clap 生成）；[`provider`] 把
//!   `--provider` / `--model` 映射到 `ai` crate 的真实 provider 栈
//!   （anthropic / openai / google）。测试用的离线 provider 在 `ai::provider::faux`
//!   （`test-util` feature）里，不进这个二进制。
//!
//! 七个内置工具（`read` / `shell` / `edit` / `write` / `grep` / `find` / `ls`）在独立的
//! `tools` crate 中；`shell` 在 Windows 上是 PowerShell，其余系统是 bash。
//!
//! 扩展（`coding-agent` 的 extensions/MCP/compaction）尚未移植。

pub mod args;
pub mod provider;
pub mod session;

use agent::AgentTool;
use args::{CliArgs, parse};
use provider::{ProviderKind, ProviderStackOptions};
use session::{Session, run_print, run_tui};
use std::{
    io::{IsTerminal as _, Read as _},
    path::{Path, PathBuf},
    process::ExitCode,
};

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            // clap 已经格式化好：`--help` / `--version` 走 stdout 且视为成功，
            // 参数错误走 stderr 且视为失败。
            let code = if error.use_stderr() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
            let _ = error.print();
            return code;
        }
    };

    if args.list_tools {
        print_tools();
        return ExitCode::SUCCESS;
    }

    let cwd = args
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let prompt = args.prompt_text().or_else(read_stdin_prompt);

    // pi 的行为：stdin/stdout 都是终端时默认进 TUI；`--print` 或任一路被重定向时
    // 走一次性的纯文本模式（那次必须有提示词）。
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let print_mode = args.print || !interactive;
    if print_mode && prompt.is_none() {
        eprintln!("没有提示词。传入提示词，或通过 stdin 提供；运行 `cli --help` 查看用法。");
        return ExitCode::FAILURE;
    }

    // `--provider` 优先；没给就按 pi 的默认顺序挑第一个在环境里有凭据的 provider。
    let kind = match args.provider.as_deref() {
        Some(value) => match ProviderKind::parse(value) {
            Some(kind) => kind,
            None => {
                eprintln!("{}", provider::ProviderError::Unknown(value.to_string()));
                return ExitCode::FAILURE;
            }
        },
        // 没有 provider 就没法知道 `--api-key` 是哪家的，让用户显式指定。
        None if args.api_key.is_some() => {
            eprintln!("`--api-key` 需要配合 `--provider` 使用：本移植版不会替你猜 provider。");
            return ExitCode::FAILURE;
        }
        None => match provider::default_kind_from_env() {
            Some(kind) => kind,
            None => {
                eprintln!(
                    "没有可用的 provider：请设置 ANTHROPIC_API_KEY / OPENAI_API_KEY / \
                     GEMINI_API_KEY，或用 `--provider` + `--api-key` 指定。"
                );
                return ExitCode::FAILURE;
            }
        },
    };

    // `--api-key` 覆盖环境变量；没给就照常从各家的环境变量取。
    let setup = match args.api_key.as_deref() {
        Some(api_key) => provider::build_with_api_key(
            kind,
            args.model.as_deref(),
            ProviderStackOptions::default(),
            Some(api_key),
        ),
        None => provider::build(kind, args.model.as_deref(), ProviderStackOptions::default()),
    };
    let setup = match setup {
        Ok(setup) => setup,
        Err(error) => {
            eprintln!("provider 配置失败：{error}");
            return ExitCode::FAILURE;
        }
    };

    let session = std::sync::Arc::new(Session::with_model(
        cwd.clone(),
        setup.stream_fn(),
        setup.model.clone(),
    ));

    if let Some(value) = &args.thinking {
        match session::parse_thinking_level(value) {
            Some(level) => session.set_thinking_level(level),
            None => eprintln!(
                "忽略未知思考级别：{value}（可用：{}）",
                session::THINKING_LEVELS
            ),
        }
    }
    session.set_tools(select_tools(&args, &cwd));

    let outcome = if print_mode {
        match prompt {
            Some(prompt) => run_print_blocking(&session, prompt),
            // 上面已经拦住了 print 模式下没有提示词的情况。
            None => return ExitCode::FAILURE,
        }
    } else {
        run_tui_blocking(&session, prompt)
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("运行失败：{error}");
            ExitCode::FAILURE
        }
    }
}

fn print_tools() {
    println!("内置工具（{} 个）：", tools::DEFAULT_TOOL_NAMES.len());
    for tool in tools::default_tools(PathBuf::from(".")) {
        println!(
            "  {:<12} {}",
            tool.name(),
            tool.tool_description().lines().next().unwrap_or_default()
        );
    }
}

/// 按 pi 的工具选项算出本次会话启用的内置工具。
///
/// `--tools` 是白名单（替换默认全集），`--exclude-tools` 是黑名单，`--no-tools` /
/// `--no-builtin-tools` 直接清空（本移植版还没有扩展/自定义工具，两者等价）。
fn select_tools(args: &CliArgs, cwd: &Path) -> Vec<AgentTool> {
    if args.no_tools || args.no_builtin_tools {
        return Vec::new();
    }
    let mut selected = tools::default_tools(cwd);
    if !args.tools.is_empty() {
        for name in &args.tools {
            warn_unknown_tool(name);
        }
        selected.retain(|tool| args.tools.iter().any(|name| name == tool.name()));
    }
    if !args.exclude_tools.is_empty() {
        for name in &args.exclude_tools {
            warn_unknown_tool(name);
        }
        selected.retain(|tool| !args.exclude_tools.iter().any(|name| name == tool.name()));
    }
    selected
}

/// `--tools` / `--exclude-tools` 里写了不存在的工具名时给出提示（pi 会忽略它们）。
fn warn_unknown_tool(name: &str) {
    if !name.is_empty() && !tools::DEFAULT_TOOL_NAMES.contains(&name) {
        eprintln!(
            "忽略未知工具：{name}（可用：{}）",
            tools::DEFAULT_TOOL_NAMES.join(", ")
        );
    }
}

/// 没有位置参数时从 stdin 读取提示词（便于管道使用）。
fn read_stdin_prompt() -> Option<String> {
    let mut buffer = String::new();
    if std::io::stdin().read_to_string(&mut buffer).is_err() {
        return None;
    }
    let trimmed = buffer.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `--print`：跑一轮并把事件打成纯文本。
fn run_print_blocking(session: &Session, prompt: String) -> std::io::Result<()> {
    let runtime = single_thread_runtime()?;
    runtime.block_on(async {
        let subscription = session.subscribe();
        let ask = session.ask(prompt);
        let (result, printed) =
            tokio::join!(ask, run_print(subscription, std::io::stdout().lock()));
        printed?;
        result.map_err(|error| std::io::Error::other(error.to_string()))
    })
}

/// 默认模式：跑一轮并交给 TUI 渲染（只读视图，`q`/`Esc`/`Ctrl+C` 退出）。
fn run_tui_blocking(
    session: &std::sync::Arc<Session>,
    prompt: Option<String>,
) -> std::io::Result<()> {
    let runtime = single_thread_runtime()?;
    runtime.block_on(run_tui(session, prompt, "tower-pi-agent"))
}

fn single_thread_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::write_event;

    #[test]
    fn stdin_prompt_reader_is_available() {
        // 这个测试进程的 stdin 在 cargo test 下通常是空/非 TTY，两种结果都合法；
        // 关键是调用不会 panic。
        let _ = read_stdin_prompt();
    }

    #[test]
    fn print_renderer_formats_core_events() {
        use agent::{AgentEvent, AgentMessage};
        let mut out: Vec<u8> = Vec::new();
        write_event(
            &mut out,
            &AgentEvent::MessageEnd {
                message: AgentMessage::Message(ai::Message::User(ai::UserMessage {
                    content: ai::UserContent::Text("你好".into()),
                    timestamp: 0,
                })),
            },
        )
            .expect("write succeeds");
        assert_eq!(String::from_utf8(out).expect("utf-8"), "[user] 你好\n");
    }
}
