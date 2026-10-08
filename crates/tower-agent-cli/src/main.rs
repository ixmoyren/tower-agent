//! `tower-agent` — tower-agent 的命令行入口（对应 pi `packages/coding-agent`）。
//!
//! 组装一次会话：内置工具 + agent 编排循环 + （TUI 或纯文本）事件渲染。
//! provider 由 `--provider` / `--model` 选择，可接真实的 anthropic / openai / google /
//! deepseek provider（凭据来自环境变量）；没显式指定时按环境里的凭据挑一个。
//!
//! 本 crate 负责会话入口与命令行解析，也是 workspace 里**唯一**的可执行入口
//! （`src/main.rs`）：
//!
//! - [`session`]：会话入口，把 `tower_agent_runtime::Agent`、`tools` crate 的内置工具与事件流
//!   （TUI 或纯文本）拼起来。
//! - [`args`]：命令行参数解析（`clap` derive，帮助/版本/错误由 clap 生成）；[`provider`] 把
//!   `--provider` / `--model` 映射到 `ai` crate 的真实 provider 栈
//!   （anthropic / openai / deepseek / google）。测试用的离线 provider 在 `tower_agent_provider::provider::faux`
//!   （`test-util` feature）里，不进这个二进制。
//!
//! 七个内置工具（`read` / `shell` / `edit` / `write` / `grep` / `find` / `ls`）在独立的
//! `tools` crate 中；`shell` 在 Windows 上是 PowerShell，其余系统是 bash。
//!
//! 扩展（`coding-agent` 的 extensions/MCP/compaction）尚未移植。

pub mod args;
pub mod provider;
pub mod session;
pub mod stack;

use args::{CliArgs, parse};
use provider::{ProviderKind, ProviderStackOptions};
use session::{Session, run_print, run_tui};
use std::{
    io::{IsTerminal as _, Read as _},
    path::PathBuf,
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

    // pi 的行为：stdin/stdout 都是终端时默认进 TUI；`--print` 或任一路被重定向时
    // 走一次性的纯文本模式（那次必须有提示词）。
    //
    // 模式判断要在读 stdin 之前完成：TUI 模式下绝不能碰 stdin，否则会在终端上一直
    // 等 EOF（看起来就是“没有进入 TUI”）。只有 print 模式才从 stdin 取提示词。
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let print_mode = args.print || !interactive;
    let prompt = if print_mode {
        args.prompt_text().or_else(read_stdin_prompt)
    } else {
        // TUI 只接受位置参数作为一次性提示词；没有参数就直接进交互界面。
        args.prompt_text()
    };
    if print_mode && prompt.is_none() {
        eprintln!(
            "没有提示词。传入提示词，或通过 stdin 提供；运行 `tower-agent --help` 查看用法。"
        );
        return ExitCode::FAILURE;
    }

    // `--provider` 优先；没给就按 pi 的默认顺序挑第一个在环境里有凭据的 provider。
    // 都没有时：print 模式直接报错；TUI 模式先进界面，用 `/login` 配置。
    let requested = match args.provider.as_deref() {
        Some(value) => match ProviderKind::parse(value) {
            Some(kind) => Some(kind),
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
        None => provider::default_kind_from_env(),
    };

    let setup = match requested {
        Some(kind) => {
            // `--api-key` 覆盖环境变量；没给就照常从各家的环境变量取。
            let result = match args.api_key.as_deref() {
                Some(api_key) => provider::build_with_api_key(
                    kind,
                    args.model.as_deref(),
                    ProviderStackOptions::default(),
                    Some(api_key),
                ),
                None => {
                    provider::build(kind, args.model.as_deref(), ProviderStackOptions::default())
                }
            };
            match result {
                Ok(setup) => setup,
                // TUI 里缺凭据可以用 `/login` 补，先进界面而不是直接退出。
                Err(provider::ProviderError::MissingApiKey { .. }) if !print_mode => {
                    provider::unconfigured_setup()
                }
                Err(error) => {
                    eprintln!("provider 配置失败：{error}");
                    return ExitCode::FAILURE;
                }
            }
        }
        None if print_mode => {
            eprintln!(
                "没有可用的 provider：请设置 ANTHROPIC_API_KEY / OPENAI_API_KEY / \
                 GEMINI_API_KEY / DEEPSEEK_API_KEY，或用 `--provider` + `--api-key` 指定。"
            );
            return ExitCode::FAILURE;
        }
        None => provider::unconfigured_setup(),
    };

    let session = std::sync::Arc::new(Session::with_model(
        cwd.clone(),
        setup.stream_fn(),
        setup.model.clone(),
    ));
    // 还没有 provider 时在 TUI 里先给一句提示，告诉用户用 `/login` 配置。
    let tui_notice = (setup.model.provider == "unconfigured").then(|| {
        "No provider configured. Use /login <provider> <api-key> to set one up.".to_string()
    });

    if let Some(value) = &args.thinking {
        match session::parse_thinking_level(value) {
            Some(level) => session.set_thinking_level(level),
            None => eprintln!(
                "忽略未知思考级别：{value}（可用：{}）",
                session::THINKING_LEVELS
            ),
        }
    }
    session.set_tool_filter(select_filter(&args));

    let outcome = if print_mode {
        match prompt {
            Some(prompt) => run_print_blocking(&session, prompt),
            // 上面已经拦住了 print 模式下没有提示词的情况。
            None => return ExitCode::FAILURE,
        }
    } else {
        run_tui_blocking(&session, prompt, tui_notice)
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
    println!(
        "内置工具（{} 个）：",
        tower_agent_plugins::DEFAULT_TOOL_NAMES.len()
    );
    for tool in tower_agent_plugins::default_tools(PathBuf::from(".")) {
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
/// `--no-builtin-tools` 直接清空；过滤作用在工具目录（静态 + JIT 统一解析结果）上。
fn select_filter(args: &CliArgs) -> session::ToolFilter {
    for name in args.tools.iter().chain(args.exclude_tools.iter()) {
        warn_unknown_tool(name);
    }
    session::ToolFilter {
        none: args.no_tools || args.no_builtin_tools,
        allow: args.tools.clone(),
        exclude: args.exclude_tools.clone(),
    }
}

/// `--tools` / `--exclude-tools` 里写了不存在的工具名时给出提示（pi 会忽略它们）。
fn warn_unknown_tool(name: &str) {
    if !name.is_empty() && !tower_agent_plugins::DEFAULT_TOOL_NAMES.contains(&name) {
        eprintln!(
            "忽略未知工具：{name}（可用：{}）",
            tower_agent_plugins::DEFAULT_TOOL_NAMES.join(", ")
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
    let runtime = runtime()?;
    runtime.block_on(async {
        session.load_persisted_mods().await;
        session.load_dynamic_plugins();
        session.prewarm_mods();
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
    notice: Option<String>,
) -> std::io::Result<()> {
    let runtime = runtime()?;
    runtime.block_on(run_tui(session, prompt, "tower-agent", notice))
}

/// 构造 CLI 使用的 tokio runtime。
///
/// 用多线程：给「主任务（turn / TUI）」和「动态插件宿主回调的派发任务」留并行头寸，
/// 避免一段没有 `.await` 的 CPU 工作把派发拖住（通道桥下不会死锁，只是延迟）。
/// worker 数由 `TPA_WORKERS` 覆盖，默认取 `std::thread::available_parallelism()`。
fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_count())
        .enable_all()
        .build()
}

/// worker 线程数：`TPA_WORKERS`（正整数）优先，否则用可用并行度。
fn worker_count() -> usize {
    parse_workers(std::env::var("TPA_WORKERS").ok().as_deref()).unwrap_or_else(default_workers)
}

/// 解析 `TPA_WORKERS`：只接受正整数，其余（含 0、非法值）返回 `None`。
fn parse_workers(value: Option<&str>) -> Option<usize> {
    value
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|count| *count > 0)
}

/// 默认 worker 数：可用并行度（取不到时退化为 1）。
fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::write_event;

    #[test]
    fn worker_count_parses_overrides() {
        assert_eq!(parse_workers(Some("4")), Some(4));
        assert_eq!(parse_workers(Some(" 8 ")), Some(8));
        assert_eq!(parse_workers(Some("0")), None, "0 不是合法 worker 数");
        assert_eq!(parse_workers(Some("-1")), None);
        assert_eq!(parse_workers(Some("abc")), None);
        assert_eq!(parse_workers(None), None);
        assert!(default_workers() >= 1);
    }

    #[test]
    fn stdin_prompt_reader_is_available() {
        // 这个测试进程的 stdin 在 cargo test 下通常是空/非 TTY，两种结果都合法；
        // 关键是调用不会 panic。
        let _ = read_stdin_prompt();
    }

    #[test]
    fn print_renderer_formats_core_events() {
        use tower_agent_runtime::{AgentEvent, AgentMessage};
        let mut out: Vec<u8> = Vec::new();
        write_event(
            &mut out,
            &AgentEvent::MessageEnd {
                message: AgentMessage::Message(tower_agent_provider::Message::User(
                    tower_agent_provider::UserMessage {
                        content: tower_agent_provider::UserContent::Text("你好".into()),
                        timestamp: 0,
                    },
                )),
            },
        )
        .expect("write succeeds");
        assert_eq!(String::from_utf8(out).expect("utf-8"), "[user] 你好\n");
    }
}
