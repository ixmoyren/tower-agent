//! `cli` — tower-pi-agent 的命令行入口（对应 pi `packages/coding-agent`）。
//!
//! 组装一次会话：内置工具 + agent 编排循环 + （TUI 或纯文本）事件渲染。
//! provider 由 `--provider` / `--model` 选择：默认离线 `faux`，也可以接真实的
//! anthropic / openai / google provider（凭据来自环境变量）。
//!
//! 本 crate 负责会话入口与命令行解析，也是 workspace 里**唯一**的可执行入口
//! （`src/main.rs`）：
//!
//! - [`session`]：会话入口，把 `agent::Agent`、`tools` crate 的内置工具与事件流
//!   （TUI 或纯文本）拼起来。
//! - [`args`]：命令行参数解析（`clap` derive，帮助/版本/错误由 clap 生成）；[`faux`] 是
//!   确定性的离线 provider，让会话在没有真实模型时也能端到端运行与测试；[`provider`] 把
//!   `--provider` / `--model` 映射到 `ai` crate 的真实 provider 栈
//!   （anthropic / openai / google）。
//!
//! pi 的八个内置工具（`read` / `bash` / `powershell` / `edit` / `write` / `grep` /
//! `find` / `ls`）在独立的 `tools` crate 中。
//!
//! 扩展（`coding-agent` 的 extensions/MCP/compaction）尚未移植。

pub mod args;
pub mod faux;
pub mod provider;
pub mod session;

use args::parse;
use provider::{ProviderKind, ProviderStackOptions};
use session::{Session, run_print, run_tui};
use std::{io::Read as _, path::PathBuf, process::ExitCode};

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
    let prompt = match args.prompt_text().or_else(read_stdin_prompt) {
        Some(prompt) => prompt,
        None => {
            eprintln!("没有提示词。传入提示词，或通过 stdin 提供；运行 `cli --help` 查看用法。");
            return ExitCode::FAILURE;
        }
    };

    // `--provider` 优先；`--faux` 是没有显式 provider 时的离线简写（默认也是 faux）。
    let kind = match args.provider.as_deref() {
        Some(value) => match ProviderKind::parse(value) {
            Some(kind) => kind,
            None => {
                eprintln!("{}", provider::ProviderError::Unknown(value.to_string()));
                return ExitCode::FAILURE;
            }
        },
        None => ProviderKind::Faux,
    };

    let setup = match provider::build(kind, args.model.as_deref(), ProviderStackOptions::default())
    {
        Ok(setup) => setup,
        Err(error) => {
            eprintln!("provider 配置失败：{error}");
            return ExitCode::FAILURE;
        }
    };

    let session = std::sync::Arc::new(Session::with_model(
        cwd,
        setup.stream_fn(),
        setup.model.clone(),
    ));

    let outcome = if args.print {
        run_print_blocking(&session, prompt)
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
    println!("内置工具（{} 个）：", tools::tool_names().len());
    for tool in tools::default_tools(PathBuf::from(".")) {
        println!(
            "  {:<12} {}",
            tool.name(),
            tool.tool_description().lines().next().unwrap_or_default()
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
fn run_tui_blocking(session: &std::sync::Arc<Session>, prompt: String) -> std::io::Result<()> {
    let runtime = single_thread_runtime()?;
    runtime.block_on(run_tui(session, Some(prompt), "tower-pi-agent"))
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
    fn tool_listing_covers_all_builtin_tools() {
        assert_eq!(
            tools::tool_names(),
            vec![
                "read",
                "bash",
                "powershell",
                "edit",
                "write",
                "grep",
                "find",
                "ls"
            ]
        );
    }

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
