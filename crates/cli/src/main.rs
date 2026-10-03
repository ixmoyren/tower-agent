//! `cli` — tower-pi-agent 的命令行入口（对应 pi `packages/coding-agent`）。
//!
//! 组装一次会话：内置工具 + agent 编排循环 + （TUI 或纯文本）事件渲染。
//! provider 目前只有离线 faux 实现（真实 provider 属于 `ai` crate，尚未移植）。

use std::{io::Read as _, path::PathBuf, process::ExitCode};

use cli::{
    args::{USAGE, parse},
    faux::{FauxProvider, model},
    session::{Session, run_print, run_tui},
    tools,
};

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("参数错误：{error}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    if args.help {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.version {
        println!("cli {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if args.list_tools {
        print_tools();
        return ExitCode::SUCCESS;
    }

    let cwd = args
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let prompt = match args.prompt.clone().or_else(read_stdin_prompt) {
        Some(prompt) => prompt,
        None => {
            eprintln!("没有提示词。传入提示词，或通过 stdin 提供。\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    // 目前只有 faux provider；`--faux` 是显式写法，便于脚本自解释。
    let provider = FauxProvider::new().boxed();
    let session = std::sync::Arc::new(Session::with_model(cwd, provider, model()));

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
        cli::session::write_event(
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
