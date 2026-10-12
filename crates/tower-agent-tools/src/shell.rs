//! `shell` 工具：统一的 shell 执行工具（对应 pi `coding-agent` 的 `core/tools/bash.ts` +
//! `output-accumulator.ts`）。
//!
//! 底层是**内嵌的 brush**（`brush-core` + `brush-builtins`，bash 兼容的 Rust 实现）：命令在
//! **当前进程内**解释执行，不再依赖外部的 `bash` / `pwsh` 可执行文件，因此 Windows / macOS /
//! Linux 行为一致。stdout/stderr 经 OS 管道合并流式收集；展示输出按尾截断（默认 2000 行 /
//! 50KB），一旦截断就把完整输出落到临时文件并在结果里给出路径。部分结果通过
//! [`ToolResultStream`] 的中间项推送（对应 pi 的节流 `onUpdate`，间隔 100ms）。
//!
//! 与 pi 的差别（见 design.md 第 9 节第 4.ii 步）：
//! - 不再区分 bash / PowerShell，也没有 PowerShell 分支（PI 的 `powershell.ts` 不再对应）。
//! - 不注入 `PI_*` 会话环境变量（`ToolCallRequest` 不携带会话上下文）。
//! - 同一个工具复用一条**持久 shell 会话**：变量 / 函数 / 别名跨命令保留。**工作目录不保留**
//!   ——每条命令开始前都把 shell 的 cwd 钉回 workspace，`cd` 只影响当前这条命令：既留下了
//!   shell 状态，又不会让 cwd 漂到别处、与 read / write / grep 这些锚在 workspace 的工具分歧。
//! - 超时 / 取消 / `exit` 会作废整条会话（专属线程 + 独立 runtime 一起拆掉），连带经
//!   `kill_external_commands_on_drop` 收掉 `cmd &` 留下的后台子进程；下一条命令重建会话。

use std::{
    collections::HashMap,
    io::{self, Read as _},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use brush_builtins::{BuiltinSet, ShellBuilderExt as _};
use brush_core::{
    ProfileLoadBehavior, RcLoadBehavior, Shell, SourceInfo,
    openfiles::{self, OpenFile, OpenFiles},
};
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tower::{Layer, Service};
use tower_agent_provider::Arguments;
use tower_agent_runtime::{AgentError, AgentToolResult, ToolCallRequest, ToolResultStream};

use super::{
    layer::ToolNotFound,
    support::{ToolFuture, error_result, text_content, text_result},
    truncate::{
        DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, format_size,
        truncate_tail,
    },
};

/// pi 的 `BASH_UPDATE_THROTTLE_MS`。
const UPDATE_THROTTLE: Duration = Duration::from_millis(100);
/// `structuredContent.output` 的上限（pi `STRUCTURED_OUTPUT_MAX_BYTES`）。
const STRUCTURED_OUTPUT_MAX_BYTES: usize = 1024 * 1024;
/// `timeout` 参数（秒）的上限（pi `MAX_TIMEOUT_MS / 1000`）。
const MAX_TIMEOUT_SECONDS: f64 = 2_147_483.647;
/// 截断时完整输出落盘用的临时文件前缀。
const TEMP_PREFIX: &str = "brush-shell";
/// 命令结束后等待输出管道排空的上限（避免残留孙进程吊着写端）。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// 正常完成后的排空预算：此时 shell 已把写端换回空设备，管道里只剩已写入的缓冲；
/// 只有 `cmd &` 这类后台进程还攥着写端，没必要为它等满 [`DRAIN_TIMEOUT`]。
const POST_COMMAND_DRAIN: Duration = Duration::from_millis(500);

/// 发往会话线程的一条命令。
struct SessionRequest {
    /// 要执行的 shell 命令。
    command: String,
    /// 本次命令的 stdout / stderr 写端（每命令一根新管道）。
    stdout: io::PipeWriter,
    stderr: io::PipeWriter,
    /// 取消本次命令的信号（超时 / 用户中断）。
    cancel: oneshot::Receiver<()>,
    /// 命令终态回执。
    done: oneshot::Sender<SessionOutcome>,
}

/// 一条命令在会话里的终态。
enum SessionOutcome {
    /// 命令跑完，携带退出码；会话继续可用。
    Completed(i32),
    /// `exit`（或 `errexit`）让 shell 退出，携带退出码；会话随之结束。
    Exited(i32),
    /// 本次执行报错（解析 / 执行）；会话仍可用。
    Error(String),
    /// 被取消；会话随收尾一起作废。
    Cancelled,
}

/// 一个持久 shell 会话的句柄。
#[derive(Clone)]
struct SessionHandle {
    tx: mpsc::Sender<SessionRequest>,
}

/// `shell` 工具的选项。
#[derive(Debug, Clone, Default)]
pub struct ShellToolOptions {
    /// 每次执行前追加的命令前缀（例如 shell 初始化）。
    pub command_prefix: Option<String>,
}

/// 执行 shell 命令的工具（内嵌 brush，bash 兼容）。
///
/// 同一个工具（克隆之间共享）**复用一条持久 shell 会话**：环境变量、导出的变量、函数与
/// 别名等跨命令保留。**工作目录不保留**——每条命令开始前都把 shell 的 cwd 钉回工具的
/// `cwd`（workspace），因此 `cd` 只影响当前这一条命令，不会漂到别的目录、也不会让 shell
/// 与 read / write / grep 这些锚在 workspace 的工具产生分歧。
#[derive(Clone)]
pub struct ShellTool {
    cwd: Arc<Path>,
    command_prefix: Option<String>,
    /// 持久会话；惰性创建，超时 / `exit` 后作废，下一条命令重建。
    session: Arc<Mutex<Option<SessionHandle>>>,
}

impl ShellTool {
    /// `shell` 工具名。
    pub const NAME: &'static str = "shell";

    /// 用工作目录构造一个 `shell` 工具。
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self::with_options(cwd, ShellToolOptions::default())
    }

    /// 用工作目录与选项构造。
    pub fn with_options(cwd: impl AsRef<Path>, options: ShellToolOptions) -> Self {
        Self {
            cwd: Arc::from(cwd.as_ref()),
            command_prefix: options.command_prefix,
            session: Arc::new(Mutex::new(None)),
        }
    }

    /// 工具名。
    pub fn name(&self) -> &'static str {
        Self::NAME
    }

    /// 发给模型的工具描述。
    pub fn description(&self) -> String {
        format!(
            "Execute a bash-compatible shell command in the current working directory. Shell \
            state (variables, functions, aliases) persists across calls, but every command \
            starts in the working directory (`cd` only affects the current command). Returns \
            stdout and stderr. Output is truncated to last {} lines or {}KB (whichever is hit \
            first). If truncated, full output is saved to a temp file. Optionally provide a \
            timeout in seconds.",
            DEFAULT_MAX_LINES,
            DEFAULT_MAX_BYTES / 1024
        )
    }

    /// 参数的 JSON Schema。
    pub fn parameters() -> tower_agent_provider::JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" },
                "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
            },
            "required": ["command"]
        })
    }

    /// 运行一次命令，把部分结果与终态结果推入 `updates`。
    async fn run(
        &self,
        args: &Arguments,
        signal: Option<CancellationToken>,
        updates: mpsc::Sender<AgentToolResult>,
    ) {
        let (command, timeout) = match parse_shell_args(args) {
            Ok(parsed) => parsed,
            Err(message) => {
                let _ = updates.send(error_result(message)).await;
                return;
            }
        };
        // 调用方配置的命令前缀按行前置到命令本体。
        let command = match self.command_prefix.as_deref() {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command,
        };
        if tokio::fs::metadata(&self.cwd).await.is_err() {
            let _ = updates
                .send(error_result(format!(
                    "Working directory does not exist: {}\nCannot execute shell commands.",
                    self.cwd.display()
                )))
                .await;
            return;
        }

        // 对应 pi 的 `onUpdate({ content: [], details: undefined })`。
        let _ = updates
            .send(AgentToolResult {
                content: Vec::new(),
                ..AgentToolResult::default()
            })
            .await;

        // 同一条会话上的命令必须串行；锁在整个命令期间持有（超时 / 取消会作废会话）。
        let mut session = self.session.lock().await;
        if session.is_none() {
            match spawn_session(self.cwd.to_path_buf()) {
                Ok(handle) => *session = Some(handle),
                Err(error) => {
                    let _ = updates
                        .send(error_result(format!(
                            "Failed to start shell session: {error}"
                        )))
                        .await;
                    return;
                }
            }
        }

        // stdout/stderr 各接一根 OS 管道：内建命令与外部命令都写这里；阻塞读线程把
        // chunk 送回本任务，合并进累加器（顺序与旧实现一致）。
        let (stdout_reader, stdout_writer) = match io::pipe() {
            Ok(pipe) => pipe,
            Err(error) => {
                let _ = updates
                    .send(error_result(format!(
                        "Failed to create output pipe: {error}"
                    )))
                    .await;
                return;
            }
        };
        let (stderr_reader, stderr_writer) = match io::pipe() {
            Ok(pipe) => pipe,
            Err(error) => {
                let _ = updates
                    .send(error_result(format!(
                        "Failed to create output pipe: {error}"
                    )))
                    .await;
                return;
            }
        };
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(16);
        spawn_pipe_reader(stdout_reader, tx.clone());
        spawn_pipe_reader(stderr_reader, tx.clone());
        drop(tx);

        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let (done_tx, mut done_rx) = oneshot::channel::<SessionOutcome>();
        let request = SessionRequest {
            command,
            stdout: stdout_writer,
            stderr: stderr_writer,
            cancel: cancel_rx,
            done: done_tx,
        };
        let sender = session.as_ref().expect("session ensured above").tx.clone();
        if let Err(error) = sender.send(request).await {
            // 会话线程已经退出（例如上一条命令让 shell `exit` 了）：丢掉这次请求，
            // 句柄作废，下一次调用重建。
            drop(error.0);
            *session = None;
            let _ = updates
                .send(error_result(
                    "Shell session is no longer available".to_owned(),
                ))
                .await;
            return;
        }

        let mut accumulator = OutputAccumulator::new(TEMP_PREFIX);
        let started = Instant::now();
        let deadline = timeout.map(|seconds| started + Duration::from_secs_f64(seconds));
        let mut last_update = started;
        let mut terminated: Option<String> = None;
        let mut exit_code: i32 = 0;
        let mut rx_open = true;
        let mut done_received = false;
        let mut session_dead = false;

        loop {
            tokio::select! {
                chunk = rx.recv(), if rx_open => {
                    match chunk {
                        Some(chunk) => accumulator.append(&chunk),
                        None => rx_open = false,
                    }
                }
                outcome = &mut done_rx => {
                    done_received = true;
                    match outcome {
                        Ok(SessionOutcome::Completed(code)) => exit_code = code,
                        Ok(SessionOutcome::Exited(code)) => {
                            exit_code = code;
                            session_dead = true;
                        }
                        Ok(SessionOutcome::Error(message)) => terminated = Some(message),
                        Ok(SessionOutcome::Cancelled) => {
                            terminated = Some("Command aborted".into());
                            session_dead = true;
                        }
                        Err(_) => {
                            terminated = Some("Shell session exited unexpectedly".into());
                            session_dead = true;
                        }
                    }
                    break;
                }
                () = sleep_until(deadline) => {
                    terminated = Some(format!(
                        "Command timed out after {} seconds",
                        timeout.unwrap_or_default()
                    ));
                    break;
                }
                () = wait_for_cancel(&signal) => {
                    terminated = Some("Command aborted".into());
                    break;
                }
            }

            if last_update.elapsed() >= UPDATE_THROTTLE {
                last_update = Instant::now();
                let snapshot = accumulator.snapshot();
                if snapshot.truncated {
                    let _ = updates.send(partial_result(&snapshot)).await;
                }
            }
        }

        if terminated.is_some() && !done_received {
            // 被打断的命令可能把会话留在未知状态：作废整个会话（会话线程会拆掉
            // runtime，连带收掉 `cmd &` 留下的后台子进程）。
            let _ = cancel_tx.send(());
            let _ = tokio::time::timeout(DRAIN_TIMEOUT, &mut done_rx).await;
            session_dead = true;
        }
        if session_dead {
            *session = None;
        }
        // 写端随会话 / shell 拆除而关闭；读线程随之读到 EOF。给排空一个上限，
        // 万一还有残留进程吊着写端，也不拖住本轮结果。
        let drain = async {
            while let Some(chunk) = rx.recv().await {
                accumulator.append(&chunk);
            }
        };
        let drain_budget = if terminated.is_some() {
            DRAIN_TIMEOUT
        } else {
            POST_COMMAND_DRAIN
        };
        let _ = tokio::time::timeout(drain_budget, drain).await;
        accumulator.finish();
        let snapshot = accumulator.snapshot();
        let (mut text, details) = format_output(&snapshot, &mut accumulator);

        if let Some(message) = terminated {
            let _ = updates
                .send(error_result(append_status(&text, &message)))
                .await;
            return;
        }

        let wall_time = started.elapsed();
        let structured = structured_content(&snapshot, &mut accumulator, exit_code, wall_time);
        if exit_code == 0 {
            let mut result = text_result(text);
            result.details = details;
            result.structured_content = Some(structured);
            let _ = updates.send(result).await;
        } else {
            text = append_status(&text, &format!("Command exited with code {exit_code}"));
            let mut result = error_result(text);
            result.details = details;
            result.structured_content = Some(structured);
            let _ = updates.send(result).await;
        }
    }
}

impl Service<ToolCallRequest> for ShellTool {
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = ToolFuture;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        let tool = self.clone();
        let args = request.tool_call.arguments.clone();
        let signal = request.signal.clone();
        Box::pin(async move {
            let (tx, rx) = mpsc::channel(16);
            tokio::spawn(async move {
                tool.run(&args, signal, tx).await;
            });
            let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            });
            Ok(ToolResultStream::new(stream))
        })
    }
}

/// 起一条持久 shell 会话：专属 OS 线程 + 独立 runtime 持有一个 brush `Shell`，
/// 通过 channel 逐条接收命令。
///
/// 用独立 runtime 是为了超时 / 取消时能**整块拆掉**它——brush 里 `cmd &` 跑在
/// detached 的 tokio 任务里（句柄摸不到进程），只有丢弃 runtime 才会触发
/// `kill_external_commands_on_drop` 收掉它们的子进程。
fn spawn_session(cwd: PathBuf) -> Result<SessionHandle, std::io::Error> {
    let (tx, mut rx) = mpsc::channel::<SessionRequest>(1);
    std::thread::Builder::new()
        .name("brush-shell-session".to_owned())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            else {
                // runtime 都建不起来：让在等的请求立刻收到错误，别干等。
                while let Some(request) = rx.blocking_recv() {
                    let _ = request.done.send(SessionOutcome::Error(
                        "Failed to start shell runtime".to_owned(),
                    ));
                }
                return;
            };

            runtime.block_on(async move {
                let mut shell: Option<Shell> = None;
                while let Some(request) = rx.recv().await {
                    let SessionRequest {
                        command,
                        stdout,
                        stderr,
                        cancel,
                        done,
                    } = request;
                    if shell.is_none() {
                        match build_shell(&cwd).await {
                            Ok(built) => shell = Some(built),
                            Err(error) => {
                                let _ = done.send(SessionOutcome::Error(format!(
                                    "Failed to start shell: {error}"
                                )));
                                continue;
                            }
                        }
                    }
                    let shell = shell.as_mut().expect("shell built above");
                    // 每条命令：换成本次的输出管道，并把工作目录钉回 workspace。
                    shell
                        .open_files_mut()
                        .set_fd(OpenFiles::STDOUT_FD, OpenFile::from(stdout));
                    shell
                        .open_files_mut()
                        .set_fd(OpenFiles::STDERR_FD, OpenFile::from(stderr));
                    if let Err(error) = shell.set_working_dir(&cwd) {
                        let _ = done.send(SessionOutcome::Error(format!(
                            "Failed to reset working directory: {error}"
                        )));
                        continue;
                    }

                    let params = shell.default_exec_params();
                    let source_info = SourceInfo::default();
                    let outcome = tokio::select! {
                        result = shell.run_string(command, &source_info, &params) => {
                            match result {
                                Ok(result) => {
                                    let code = i32::from(u8::from(result.exit_code));
                                    if result.is_exit() {
                                        SessionOutcome::Exited(code)
                                    } else {
                                        SessionOutcome::Completed(code)
                                    }
                                }
                                Err(error) => SessionOutcome::Error(format!("Shell error: {error}")),
                            }
                        }
                        _ = cancel => SessionOutcome::Cancelled,
                    };
                    // 把本次命令的写端换回空设备：调用方的读线程会立刻读到 EOF，
                    // 不必等下一次命令覆盖 fd（否则每条命令都要空等排空超时）。
                    if let Ok(null) = openfiles::null() {
                        shell
                            .open_files_mut()
                            .set_fd(OpenFiles::STDOUT_FD, null);
                    }
                    if let Ok(null) = openfiles::null() {
                        shell
                            .open_files_mut()
                            .set_fd(OpenFiles::STDERR_FD, null);
                    }
                    let stop = matches!(
                        outcome,
                        SessionOutcome::Exited(_) | SessionOutcome::Cancelled
                    );
                    let _ = done.send(outcome);
                    if stop {
                        break;
                    }
                }
            });
            // 拆掉 runtime：收掉本次会话留下的后台子进程（kill_on_drop）。
            drop(runtime);
        })?;
    Ok(SessionHandle { tx })
}

/// 建一个 brush shell：工作目录 = 工具的 cwd，三个标准 fd 先指向空设备（每条命令再
/// 换成本次的输出管道）。不加载用户的 profile / rc，保证行为可预测。
async fn build_shell(cwd: &Path) -> Result<Shell, brush_core::Error> {
    let mut fds = HashMap::new();
    fds.insert(OpenFiles::STDIN_FD, openfiles::null()?);
    fds.insert(OpenFiles::STDOUT_FD, openfiles::null()?);
    fds.insert(OpenFiles::STDERR_FD, openfiles::null()?);
    Shell::builder()
        .working_dir(cwd.to_path_buf())
        .fds(fds)
        .profile(ProfileLoadBehavior::Skip)
        .rc(RcLoadBehavior::Skip)
        .kill_external_commands_on_drop(true)
        .default_builtins(BuiltinSet::BashMode)
        .build()
        .await
}

/// 在独立线程里把管道内容按 chunk 送回执行任务。
///
/// 用普通线程而不是 `spawn_blocking`：读端会一直阻塞到写端关闭，不该占用调用方
/// runtime 的阻塞线程池。
fn spawn_pipe_reader(mut reader: io::PipeReader, tx: mpsc::Sender<Vec<u8>>) {
    let _ = std::thread::Builder::new()
        .name("brush-shell-pipe".to_owned())
        .spawn(move || {
            let mut buffer = vec![0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if tx.blocking_send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn wait_for_cancel(signal: &Option<CancellationToken>) {
    match signal {
        Some(token) => token.cancelled().await,
        None => std::future::pending().await,
    }
}

fn parse_shell_args(args: &Arguments) -> Result<(String, Option<f64>), String> {
    let command = args.arg_str("command")?;
    let timeout = args.opt_f64("timeout")?;
    if let Some(timeout) = timeout {
        if !timeout.is_finite() || timeout <= 0.0 {
            return Err("Invalid timeout: must be a finite number of seconds".into());
        }
        if timeout > MAX_TIMEOUT_SECONDS {
            return Err(format!(
                "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS} seconds"
            ));
        }
    }
    Ok((command, timeout))
}

fn partial_result(snapshot: &Snapshot) -> AgentToolResult {
    AgentToolResult {
        content: vec![text_content(snapshot.content.clone())],
        details: Some(serde_json::json!({
            "truncation": snapshot.truncated.then(|| truncation_json(snapshot)),
            "fullOutputPath": snapshot.full_output_path,
        })),
        ..AgentToolResult::default()
    }
}

fn format_output(
    snapshot: &Snapshot,
    accumulator: &mut OutputAccumulator,
) -> (String, Option<tower_agent_provider::JsonValue>) {
    let mut text = if snapshot.content.is_empty() {
        "(no output)".to_string()
    } else {
        snapshot.content.clone()
    };
    if !snapshot.truncated {
        return (text, None);
    }

    let start_line = snapshot.total_lines - snapshot.output_lines + 1;
    let end_line = snapshot.total_lines;
    let path = snapshot.full_output_path.clone().unwrap_or_default();
    if snapshot.last_line_partial {
        let last_line_size = format_size(accumulator.current_line_bytes());
        text.push_str(&format!(
            "\n\n[Showing last {} of line {end_line} (line is {last_line_size}). Full output: {path}]",
            format_size(snapshot.output_bytes)
        ));
    } else if snapshot.truncated_by_lines {
        text.push_str(&format!(
            "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {path}]",
            snapshot.total_lines
        ));
    } else {
        text.push_str(&format!(
            "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {path}]",
            snapshot.total_lines,
            format_size(DEFAULT_MAX_BYTES)
        ));
    }
    (
        text,
        Some(serde_json::json!({
            "truncation": truncation_json(snapshot),
            "fullOutputPath": snapshot.full_output_path,
        })),
    )
}

fn structured_content(
    snapshot: &Snapshot,
    accumulator: &mut OutputAccumulator,
    exit_code: i32,
    wall_time: Duration,
) -> tower_agent_provider::JsonValue {
    let full = accumulator.read_full_output(STRUCTURED_OUTPUT_MAX_BYTES);
    serde_json::json!({
        "output": full.content,
        "truncated": full.truncated,
        "full_output_path": if full.truncated { snapshot.full_output_path.clone() } else { None },
        "exit_code": exit_code,
        "wall_time_seconds": (wall_time.as_millis() as f64 / 100.0).round() / 10.0,
    })
}

fn truncation_json(snapshot: &Snapshot) -> tower_agent_provider::JsonValue {
    serde_json::json!({
        "truncated": snapshot.truncated,
        "truncatedBy": if snapshot.truncated_by_lines { "lines" } else { "bytes" },
        "totalLines": snapshot.total_lines,
        "totalBytes": snapshot.total_bytes,
        "outputLines": snapshot.output_lines,
        "outputBytes": snapshot.output_bytes,
        "lastLinePartial": snapshot.last_line_partial,
        "maxLines": DEFAULT_MAX_LINES,
        "maxBytes": DEFAULT_MAX_BYTES,
    })
}

fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_string()
    } else {
        format!("{text}\n\n{status}")
    }
}

/// 输出快照（pi `OutputSnapshot`）。
struct Snapshot {
    content: String,
    truncated: bool,
    truncated_by_lines: bool,
    total_lines: usize,
    total_bytes: usize,
    output_lines: usize,
    output_bytes: usize,
    last_line_partial: bool,
    full_output_path: Option<String>,
}

/// 流式输出累加器（pi `OutputAccumulator` 的简化版）。
///
/// 只保留有界的解码尾部用于展示；一旦超过阈值就把完整输出写入临时文件，
/// 不再在内存里累积原始字节。
struct OutputAccumulator {
    prefix: String,
    pending: Vec<u8>,
    tail: String,
    tail_starts_at_line_boundary: bool,
    raw: Vec<u8>,
    spill: Option<tempfile::NamedTempFile>,
    total_raw_bytes: usize,
    total_decoded_bytes: usize,
    completed_lines: usize,
    current_line_bytes: usize,
    has_open_line: bool,
    total_lines: usize,
    finished: bool,
}

impl OutputAccumulator {
    fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
            pending: Vec::new(),
            tail: String::new(),
            tail_starts_at_line_boundary: true,
            raw: Vec::new(),
            spill: None,
            total_raw_bytes: 0,
            total_decoded_bytes: 0,
            completed_lines: 0,
            current_line_bytes: 0,
            has_open_line: false,
            total_lines: 0,
            finished: false,
        }
    }

    fn append(&mut self, data: &[u8]) {
        if data.is_empty() || self.finished {
            return;
        }
        self.total_raw_bytes += data.len();
        let mut decoded = String::new();
        decode_stream(&mut self.pending, data, &mut decoded);
        self.push_text(&decoded);

        if self.spill.is_some() || self.should_use_temp_file() {
            self.ensure_spill();
            if let Some(spill) = self.spill.as_mut() {
                use std::io::Write as _;
                let _ = spill.write_all(data);
            }
        } else {
            self.raw.extend_from_slice(data);
        }
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let pending = std::mem::take(&mut self.pending);
        let decoded = String::from_utf8_lossy(&pending).into_owned();
        self.push_text(&decoded);
    }

    fn snapshot(&mut self) -> Snapshot {
        let snapshot_text = self.snapshot_text();
        let truncation = truncate_tail(
            &snapshot_text,
            TruncationOptions {
                max_lines: Some(DEFAULT_MAX_LINES),
                max_bytes: Some(DEFAULT_MAX_BYTES),
            },
        );
        let truncated =
            self.total_lines > DEFAULT_MAX_LINES || self.total_decoded_bytes > DEFAULT_MAX_BYTES;
        if truncated {
            self.ensure_spill();
        }
        let truncated_by_lines = match truncation.truncated_by {
            Some(TruncatedBy::Lines) => true,
            Some(TruncatedBy::Bytes) => false,
            None => self.total_decoded_bytes <= DEFAULT_MAX_BYTES,
        };
        Snapshot {
            content: truncation.content,
            truncated,
            truncated_by_lines,
            total_lines: self.total_lines,
            total_bytes: self.total_decoded_bytes,
            output_lines: truncation.output_lines,
            output_bytes: truncation.output_bytes,
            last_line_partial: truncation.last_line_partial,
            full_output_path: self
                .spill
                .as_ref()
                .map(|file| file.path().to_string_lossy().into_owned()),
        }
    }

    fn read_full_output(&mut self, max_bytes: usize) -> FullOutput {
        let Some(spill) = self.spill.as_ref() else {
            return FullOutput {
                content: String::from_utf8_lossy(&self.raw).into_owned(),
                truncated: false,
            };
        };
        let bytes = std::fs::read(spill.path()).unwrap_or_default();
        if bytes.len() <= max_bytes {
            return FullOutput {
                content: String::from_utf8_lossy(&bytes).into_owned(),
                truncated: false,
            };
        }
        let head_bytes = max_bytes / 2;
        let tail_bytes = max_bytes - head_bytes;
        let head = String::from_utf8_lossy(&bytes[..head_bytes]).into_owned();
        let mut tail_start = bytes.len() - tail_bytes;
        while tail_start < bytes.len() && !is_char_boundary_byte(bytes[tail_start]) {
            tail_start += 1;
        }
        let tail = String::from_utf8_lossy(&bytes[tail_start..]).into_owned();
        let omitted = bytes.len() - head_bytes - tail_bytes;
        FullOutput {
            content: format!("{head}\n\n[... {omitted} bytes omitted ...]\n\n{tail}"),
            truncated: true,
        }
    }

    fn current_line_bytes(&self) -> usize {
        self.current_line_bytes
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.total_decoded_bytes += text.len();
        self.tail.push_str(text);
        if self.tail.len() > DEFAULT_MAX_BYTES * 4 {
            self.trim_tail();
        }

        let newlines = text.matches('\n').count();
        if newlines == 0 {
            self.current_line_bytes += text.len();
            self.has_open_line = true;
        } else {
            self.completed_lines += newlines;
            let tail = text.rsplit('\n').next().unwrap_or_default();
            self.current_line_bytes = tail.len();
            self.has_open_line = !tail.is_empty();
        }
        self.total_lines = self.completed_lines + usize::from(self.has_open_line);
    }

    fn trim_tail(&mut self) {
        let target = DEFAULT_MAX_BYTES * 2;
        if self.tail.len() <= target {
            return;
        }
        let mut start = self.tail.len() - target;
        while start < self.tail.len() && !self.tail.is_char_boundary(start) {
            start += 1;
        }
        self.tail_starts_at_line_boundary = start == 0 || self.tail.as_bytes()[start - 1] == b'\n';
        self.tail = self.tail[start..].to_string();
    }

    fn snapshot_text(&self) -> String {
        if self.tail_starts_at_line_boundary {
            return self.tail.clone();
        }
        match self.tail.find('\n') {
            Some(index) => self.tail[index + 1..].to_string(),
            None => self.tail.clone(),
        }
    }

    fn should_use_temp_file(&self) -> bool {
        self.total_raw_bytes > DEFAULT_MAX_BYTES
            || self.total_decoded_bytes > DEFAULT_MAX_BYTES
            || self.total_lines > DEFAULT_MAX_LINES
    }

    fn ensure_spill(&mut self) {
        if self.spill.is_some() {
            return;
        }
        let Ok(mut file) = tempfile::Builder::new()
            .prefix(&format!("{}-", self.prefix))
            .suffix(".log")
            .tempfile()
        else {
            return;
        };
        {
            use std::io::Write as _;
            let _ = file.write_all(&self.raw);
        }
        self.raw = Vec::new();
        self.spill = Some(file);
    }
}

struct FullOutput {
    content: String,
    truncated: bool,
}

fn is_char_boundary_byte(byte: u8) -> bool {
    (byte & 0b1100_0000) != 0b1000_0000
}

/// 增量 UTF-8 解码（pi 用 `TextDecoder` 的 streaming 模式）。
fn decode_stream(pending: &mut Vec<u8>, data: &[u8], out: &mut String) {
    pending.extend_from_slice(data);
    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                out.push_str(text);
                pending.clear();
                return;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    if let Ok(text) = std::str::from_utf8(&pending[..valid]) {
                        out.push_str(text);
                    }
                    pending.drain(..valid);
                }
                match error.error_len() {
                    Some(len) => {
                        out.push('\u{FFFD}');
                        pending.drain(..len);
                    }
                    // 尾部是被截断的多字节序列，等下一个 chunk。
                    None => return,
                }
            }
        }
    }
}

/// 构造内置 `shell` 工具（内嵌 brush，跨平台行为一致）。
pub fn create_shell_tool(cwd: impl AsRef<Path>) -> tower_agent_runtime::AgentTool {
    let tool = ShellTool::new(cwd);
    tower_agent_runtime::AgentTool::new(ShellTool::NAME, tool.layer(ToolNotFound))
        .label(ShellTool::NAME)
        .description(tool.description())
        .parameters(ShellTool::parameters())
        .output_schema(shell_output_schema())
}

fn shell_output_schema() -> tower_agent_provider::JsonValue {
    serde_json::json!({
        "type": "object",
        "properties": {
            "output": { "type": "string" },
            "truncated": { "type": "boolean" },
            "full_output_path": { "type": "string" },
            "exit_code": { "type": "number" },
            "wall_time_seconds": { "type": "number" }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: tower_agent_provider::JsonValue) -> Arguments {
        match value {
            tower_agent_provider::JsonValue::Object(map) => map.into(),
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn timeout_argument_is_validated() {
        assert!(parse_shell_args(&object(serde_json::json!({ "command": "ls" }))).is_ok());
        assert!(
            parse_shell_args(&object(
                serde_json::json!({ "command": "ls", "timeout": 5 })
            ))
            .is_ok()
        );
        assert!(
            parse_shell_args(&object(
                serde_json::json!({ "command": "ls", "timeout": 0 })
            ))
            .is_err()
        );
        assert!(
            parse_shell_args(&object(
                serde_json::json!({ "command": "ls", "timeout": -1 })
            ))
            .is_err()
        );
    }

    #[test]
    fn incremental_decoder_handles_split_multibyte_characters() {
        let mut pending = Vec::new();
        let mut out = String::new();
        let bytes = "é".as_bytes();
        decode_stream(&mut pending, &bytes[..1], &mut out);
        assert_eq!(out, "");
        decode_stream(&mut pending, &bytes[1..], &mut out);
        assert_eq!(out, "é");
    }

    #[test]
    fn accumulator_reports_truncation_and_totals() {
        let mut accumulator = OutputAccumulator::new("test");
        let content = (1..=3000)
            .map(|i| format!("line {i}\n"))
            .collect::<String>();
        accumulator.append(content.as_bytes());
        accumulator.finish();
        let snapshot = accumulator.snapshot();
        assert!(snapshot.truncated);
        assert!(snapshot.truncated_by_lines);
        assert_eq!(snapshot.total_lines, 3000);
        assert_eq!(snapshot.output_lines, DEFAULT_MAX_LINES);
        assert!(snapshot.full_output_path.is_some());
    }
}
