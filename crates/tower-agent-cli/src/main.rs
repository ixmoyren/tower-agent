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
//! `tools` crate 中；`shell` 内嵌 brush（bash 兼容），跨平台行为一致。
//!
//! 扩展（`coding-agent` 的 extensions/MCP）尚未移植；上下文压缩是核心能力
//! （`tower_agent_runtime::Compactor`），由 agent 循环自动执行。

pub mod args;
pub mod provider;
pub mod session;
pub mod stack;
pub mod store;

use args::{CliArgs, ConfigScopeArg, parse};
use provider::{ProviderKind, ProviderStackOptions};
use session::{Session, run_print, run_tui};
use std::{
    io::{IsTerminal as _, Read as _},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};
use tower_agent_codemode_core::{EngineSelection, ScriptLanguage};
use tower_agent_storage::ConfigScope;

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
        print_tools(args.codemode);
        return ExitCode::SUCCESS;
    }

    // 会话管理与 agent 无关，`.arg` 一出现就干完退出。
    if args.codemode_sessions {
        return print_sessions();
    }
    if let Some(name) = args.codemode_session_clear.as_deref() {
        return clear_session(name);
    }

    let cwd = args
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // 配置查看 / 修改（`--config-*`）：配置一律走 `tower-agent-storage`，CLI 只是消费端。
    if args.config_list
        || args.config_get.is_some()
        || args.config_set.is_some()
        || args.config_unset.is_some()
    {
        return run_config_command(&args, &cwd);
    }

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

    // 打开配置存储（失败只警告：CLI 仍按环境变量 / 命令行工作，只是不持久化）。
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("无法启动运行时：{error}");
            return ExitCode::FAILURE;
        }
    };
    let store = match store::CliStore::open(&runtime) {
        Ok(store) => Some(Arc::new(store)),
        Err(error) => {
            eprintln!("配置存储不可用（{error}）；本次会话的配置不会持久化。");
            None
        }
    };
    let project = store::project_id(&cwd);
    let snapshot = match &store {
        Some(store) => runtime.block_on(store.snapshot(Some(&project))),
        None => store::StoreSnapshot::default(),
    };
    let configured_engine = snapshot
        .codemode_engine
        .as_deref()
        .and_then(ScriptLanguage::parse);
    if snapshot.codemode_engine.is_some() && configured_engine.is_none() {
        eprintln!(
            "忽略未知 codemode.engine 配置：{}（可用：monty / js）",
            snapshot.codemode_engine.as_deref().unwrap_or_default()
        );
    }
    let codemode_engine = EngineSelection {
        cli: args.codemode_engine.map(Into::into),
        config: configured_engine,
    }
    .process_default();

    // provider 选择：`--provider` 优先；没给就用存储里的 `defaultProvider`（必须确实
    // 有凭据），其次按 pi 的默认顺序挑第一个在环境里有凭据的，再次挑第一个有存储凭据的。
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
        None => default_kind(&snapshot),
    };

    let setup = match requested {
        Some(kind) => {
            // api key：`--api-key` > 环境变量 > 存储凭据；模型：`--model` > 存储默认 > 内置默认。
            let stored = snapshot.credential(kind.id()).map(|c| c.api_key.as_str());
            let api_key = provider::resolve_api_key(kind, args.api_key.as_deref(), stored);
            let model = args
                .model
                .as_deref()
                .or_else(|| default_model_for(kind, &snapshot));
            let result = provider::build_with_api_key(
                kind,
                model,
                ProviderStackOptions::default(),
                api_key.as_deref(),
            );
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
                 GEMINI_API_KEY / DEEPSEEK_API_KEY，或用 `--provider` + `--api-key` 指定，\
                 或先用 `/login` 登录。"
            );
            return ExitCode::FAILURE;
        }
        None => provider::unconfigured_setup(),
    };

    // 会话续接 / 新建：`--no-session` 关闭持久化；`--session <id>` / `--continue` 续接已有
    // 会话；否则开一个新会话（默认自动保存）。
    let (resume_id, initial_messages, resume_record) = match &store {
        Some(store) if !args.no_session => {
            let record = match &args.session {
                Some(id) => runtime.block_on(store.session_record(id)),
                None if args.continue_session => runtime.block_on(store.latest_session(&project)),
                None => None,
            };
            match record {
                Some(record) => {
                    let messages = runtime.block_on(store.load_transcript(&record.id));
                    (Some(record.id.clone()), messages, Some(record))
                }
                None => (None, Vec::new(), None),
            }
        }
        _ => (None, Vec::new(), None),
    };
    if !args.no_session && resume_record.is_none() {
        if let Some(id) = &args.session {
            eprintln!("找不到会话 {id}，已开始新会话。");
        } else if args.continue_session {
            eprintln!("没有可续接的会话（{project}），已开始新会话。");
        }
    }

    let transcript = match (&store, args.no_session) {
        (Some(store), false) => Some(Arc::new(store::Transcript::new(
            Arc::clone(store),
            project.clone(),
            resume_id.clone(),
            initial_messages.len(),
        ))),
        _ => None,
    };

    // 上下文压缩：核心能力，设置来自 storage（user / project 有效值，缺省对齐 pi）。
    let compaction = match &store {
        Some(store) => runtime.block_on(store.compaction_settings(Some(&project))),
        None => tower_agent_runtime::CompactionSettings::default(),
    };
    let compactor = Arc::new(tower_agent_runtime::Compactor::new(compaction));
    // 续接会话时恢复持久化的压缩状态，避免重新摘要一遍。
    if let Some(record) = &resume_record
        && let Some(restored) = store::compaction_from_record(record)
    {
        compactor.restore(restored);
    }

    let session = Session::with_compaction_and_codemode(
        cwd.clone(),
        setup.stream_fn(),
        setup.model.clone(),
        initial_messages,
        Arc::clone(&compactor),
        !args.no_codemode,
    );
    let session = match &transcript {
        Some(transcript) => session.with_transcript(Arc::clone(transcript)),
        None => session,
    };
    let session = Arc::new(match &store {
        Some(store) => session.with_store(Arc::clone(store)),
        None => session,
    });
    session.set_codemode_engine(codemode_engine);
    if args.codemode {
        #[cfg(feature = "codemode-js")]
        if let Err(error) = session.enable_codemode(
            &cwd,
            args.codemode_session.as_deref(),
            &args.codemode_mounts,
        ) {
            eprintln!("codemode 未启用：{error}");
        }
        #[cfg(not(feature = "codemode-js"))]
        eprintln!("codemode 未启用：本构建未包含 codemode（缺少 `codemode` feature）。");
    }
    // 还没有 provider 时在 TUI 里先给一句提示，告诉用户用 `/login` 配置。
    let tui_notice = (setup.model.provider == "unconfigured").then(|| {
        "No provider configured. Use /login <provider> <api-key> to set one up.".to_string()
    });

    // 思考级别：`--thinking` > 存储里的 `defaultThinkingLevel`。
    match args.thinking.as_deref() {
        Some(value) => match session::parse_thinking_level(value) {
            Some(level) => session.set_thinking_level(level),
            None => eprintln!(
                "忽略未知思考级别：{value}（可用：{}）",
                session::THINKING_LEVELS
            ),
        },
        None => {
            if let Some(level) = snapshot
                .default_thinking_level
                .as_deref()
                .and_then(session::parse_thinking_level)
            {
                session.set_thinking_level(level);
            }
        }
    }
    session.set_tool_filter(select_filter(&args));

    let outcome = if print_mode {
        match prompt {
            Some(prompt) => run_print_blocking(&session, prompt, &runtime),
            // 上面已经拦住了 print 模式下没有提示词的情况。
            None => return ExitCode::FAILURE,
        }
    } else {
        run_tui_blocking(&session, prompt, tui_notice, &runtime)
    };

    // 把 `/login` `/model` `/thinking` 写下的配置落盘（写入是 fire-and-forget）。
    if let Some(store) = &store
        && let Err(error) = runtime.block_on(store.flush())
    {
        eprintln!("配置落盘失败：{error}");
    }

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("运行失败：{error}");
            ExitCode::FAILURE
        }
    }
}

fn print_sessions() -> ExitCode {
    #[cfg(feature = "codemode-js")]
    {
        let Some(root) = tower_agent_codemode::sessions_root() else {
            eprintln!("拿不到配置目录，无法列出会话。");
            return ExitCode::FAILURE;
        };
        let sessions = tower_agent_codemode::list_sessions_in(&root);
        println!(
            "codemode 会话（{} 个，{})：",
            sessions.len(),
            root.display()
        );
        for (name, path) in sessions {
            println!("  {:<16} {}", name, path.display());
        }
        ExitCode::SUCCESS
    }
    #[cfg(not(feature = "codemode-js"))]
    {
        eprintln!("本构建未包含 codemode（缺少 `codemode` feature）。");
        ExitCode::FAILURE
    }
}

fn clear_session(name: &str) -> ExitCode {
    #[cfg(feature = "codemode-js")]
    {
        let Some(root) = tower_agent_codemode::sessions_root() else {
            eprintln!("拿不到配置目录，无法清理会话。");
            return ExitCode::FAILURE;
        };
        match tower_agent_codemode::clear_session_in(&root, name) {
            Ok(true) => {
                println!("已删除 codemode 会话 `{name}`。");
                ExitCode::SUCCESS
            }
            Ok(false) => {
                println!("codemode 会话 `{name}` 不存在（无需清理）。");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("清理会话 `{name}` 失败：{error}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(feature = "codemode-js"))]
    {
        let _ = name;
        eprintln!("本构建未包含 codemode（缺少 `codemode` feature）。");
        ExitCode::FAILURE
    }
}

/// `--config-get` / `--config-set` / `--config-unset` / `--config-list`：配置查看与修改。
///
/// 配置一律经 `tower-agent-storage` 读写（不写 JSON、不走别的路子）；作用域由
/// `--config-scope` 决定。CLI 只是消费端，axum / webui 走同一套 storage 接口。
fn run_config_command(args: &CliArgs, cwd: &Path) -> ExitCode {
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("无法启动运行时：{error}");
            return ExitCode::FAILURE;
        }
    };
    let store = match store::CliStore::open(&runtime) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("配置存储不可用：{error}");
            return ExitCode::FAILURE;
        }
    };
    let project = store::project_id(cwd);

    match runtime.block_on(run_config_op(&store, args, &project)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// 执行一次配置操作（在 runtime 里跑，读走有效值 / 写走指定作用域）。
async fn run_config_op(
    store: &store::CliStore,
    args: &CliArgs,
    project: &str,
) -> Result<(), String> {
    if args.config_list {
        let entries = match args.config_scope {
            Some(scope) => store.scoped_entries(&scope_of(scope, project)).await,
            None => store.effective_config(Some(project)).await,
        };
        for (key, value) in entries {
            println!("{key} = {}", render_config_value(&value));
        }
        return Ok(());
    }
    if let Some(key) = &args.config_get {
        let value = match args.config_scope {
            Some(scope) => store.get_config(&scope_of(scope, project), key).await,
            None => store.resolve_config(Some(project), key).await,
        };
        return match value {
            Some(value) => {
                println!("{}", render_config_value(&value));
                Ok(())
            }
            None => Err(format!("未设置：{key}")),
        };
    }
    if let Some(spec) = &args.config_set {
        let (key, value) = parse_config_set(spec)?;
        let scope = scope_of(args.config_scope.unwrap_or(ConfigScopeArg::User), project);
        store
            .set_config(&scope, key, value)
            .await
            .map_err(|error| error.to_string())?;
        store.flush().await.map_err(|error| error.to_string())?;
        return Ok(());
    }
    if let Some(key) = &args.config_unset {
        let scope = scope_of(args.config_scope.unwrap_or(ConfigScopeArg::User), project);
        store
            .delete_config(&scope, key)
            .await
            .map_err(|error| error.to_string())?;
        store.flush().await.map_err(|error| error.to_string())?;
        return Ok(());
    }
    Ok(())
}

/// `--config-scope` → storage 作用域。
fn scope_of(scope: ConfigScopeArg, project: &str) -> ConfigScope {
    match scope {
        ConfigScopeArg::User => ConfigScope::User,
        ConfigScopeArg::Project => ConfigScope::Project(project.to_owned()),
    }
}

/// 解析 `--config-set` 的 `<key>=<value>`：value 先按 JSON 解析，失败则当字符串。
fn parse_config_set(spec: &str) -> Result<(String, serde_json::Value), String> {
    let (key, raw) = spec
        .split_once('=')
        .ok_or_else(|| "用法：--config-set <key>=<value>".to_owned())?;
    let key = key.trim();
    if key.is_empty() {
        return Err("配置 key 不能为空".to_owned());
    }
    let value = serde_json::from_str::<serde_json::Value>(raw)
        .unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()));
    Ok((key.to_owned(), value))
}

/// 打印配置值：字符串直接打印（不加引号），其余用紧凑 JSON。
fn render_config_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn print_tools(codemode: bool) {
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
    // codemode 只在 `--codemode` 打开时才进工具集；这里只列名字与描述，**不构造工具**
    // （构造要编译 7.4 MB 组件，为了列个名字不值）。
    if codemode {
        #[cfg(feature = "codemode-js")]
        {
            println!(
                "  {:<12} {}",
                tower_agent_codemode::TOOL_NAME,
                tower_agent_codemode::tool_description()
                    .lines()
                    .next()
                    .unwrap_or_default()
            );
            println!("  （--codemode-session <name> 可把会话根持久化到 config 目录）");
        }
        #[cfg(not(feature = "codemode-js"))]
        println!("  （本构建未包含 codemode，`--codemode` 无效）");
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

/// 没显式给 `--provider` 时的 provider 选择（对齐 pi 的优先级）：
///
/// 1. 存储里的 `defaultProvider`（只有当它确实有凭据可用时才采纳）；
/// 2. 环境变量里的默认顺序（[`provider::default_kind_from_env`]）；
/// 3. 第一个有存储凭据的 provider。
fn default_kind(snapshot: &store::StoreSnapshot) -> Option<ProviderKind> {
    snapshot
        .default_provider
        .as_deref()
        .and_then(ProviderKind::parse)
        .filter(|kind| kind.has_api_key() || snapshot.credential(kind.id()).is_some())
        .or_else(provider::default_kind_from_env)
        .or_else(|| {
            ProviderKind::ALL
                .into_iter()
                .find(|kind| snapshot.credential(kind.id()).is_some())
        })
}

/// 存储里的 `defaultModel`：只有 `defaultProvider` 与当前 provider 一致时才套用
/// （pi 把 `defaultProvider` / `defaultModel` 成对使用）。
fn default_model_for(kind: ProviderKind, snapshot: &store::StoreSnapshot) -> Option<&str> {
    if snapshot.default_provider.as_deref() == Some(kind.id()) {
        snapshot.default_model.as_deref()
    } else {
        None
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
fn run_print_blocking(
    session: &Session,
    prompt: String,
    runtime: &tokio::runtime::Runtime,
) -> std::io::Result<()> {
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
    session: &Arc<Session>,
    prompt: Option<String>,
    notice: Option<String>,
    runtime: &tokio::runtime::Runtime,
) -> std::io::Result<()> {
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
