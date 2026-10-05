//! `shell` 工具：统一的 shell 执行工具（对应 pi `coding-agent` 的 `core/tools/bash.ts` +
//! `output-accumulator.ts` 与 `core/tools/powershell.ts`）。
//!
//! 底层 shell 用 `cfg_select!` 按操作系统选择：Windows 用 PowerShell，其余系统用 bash。
//! 命令在 `bash -c` / `pwsh -c` 中执行，stdout/stderr 合并流式收集；展示输出按尾截断
//! （默认 2000 行 / 50KB），一旦截断就把完整输出落到临时文件并在结果里给出路径。部分结果
//! 通过 [`ToolResultStream`] 的中间项推送（对应 pi 的节流 `onUpdate`，间隔 100ms）。
//!
//! 与 pi 的差别（见 design.md 第 9 节第 4.ii 步）：
//! - bash 与 PowerShell 合并为一个 `shell` 工具（Windows → powershell，其余 → bash），
//!   不再分别注册两个工具。
//! - 不注入 `PI_*` 会话环境变量（`ToolCallRequest` 不携带会话上下文）。
//! - 进程组杀死只在类 Unix 平台启用；其他平台退化为只杀直接子进程。

use std::{path::Path, process::Stdio, sync::Arc, time::Duration};

use agent::{AgentError, AgentToolResult, ToolCallRequest, ToolResultStream};
use ai::JsonObject;
use tokio::{io::AsyncReadExt as _, process::Command, sync::mpsc, time::Instant};
use tokio_util::sync::CancellationToken;
use tower::Service;

use super::{
    support::{ToolFuture, arg_str, error_result, opt_f64, text_content, text_result},
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
/// PowerShell 的 UTF-8 输出设置（pi `UTF8_OUTPUT_PREFIX`）。
#[cfg(windows)]
const POWERSHELL_UTF8_PREFIX: &str =
    "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}\n";

/// `shell` 工具的选项。
#[derive(Debug, Clone, Default)]
pub struct ShellToolOptions {
    /// 每次执行前追加的命令前缀（例如 shell 初始化）。
    pub command_prefix: Option<String>,
    /// 指定 bash 可执行文件（仅在非 Windows 上有意义），默认 `bash`。
    pub shell_path: Option<String>,
}

/// 执行 shell 命令的工具。
#[derive(Clone)]
pub struct ShellTool {
    cwd: Arc<Path>,
    spec: ShellSpec,
    command_prefix: Option<String>,
}

/// 一个 shell 后端的静态配置：bash 与 PowerShell 共用同一套执行逻辑。
#[derive(Debug, Clone)]
struct ShellSpec {
    /// 人类可读的 shell 名（`bash` / `PowerShell`），用于描述与错误文案。
    label: &'static str,
    shell: String,
    temp_prefix: &'static str,
    /// 每次执行前固定注入的脚本片段（PowerShell 用来设置 UTF-8 输出编码）。
    init_script: Option<&'static str>,
    description: String,
}

impl ShellSpec {
    /// 描述文案与 pi 的 `createShellToolDefinition` 一致：`Execute a {shell_name} command ...`。
    fn new(
        label: &'static str,
        shell: String,
        temp_prefix: &'static str,
        init_script: Option<&'static str>,
    ) -> Self {
        let description = format!(
            "Execute a {label} command in the current working directory. Returns stdout and stderr. \
            Output is truncated to last {} lines or {}KB (whichever is hit first). If truncated, \
            full output is saved to a temp file. Optionally provide a timeout in seconds.",
            DEFAULT_MAX_LINES,
            DEFAULT_MAX_BYTES / 1024
        );
        Self {
            label,
            shell,
            temp_prefix,
            init_script,
            description,
        }
    }

    /// 按操作系统选择默认 shell：Windows 用 PowerShell，其余系统用 bash。
    fn from_path(_path: Option<String>) -> Self {
        cfg_select! {
            windows => {
                let shell = find_on_path(&["pwsh", "pwsh.exe", "powershell.exe", "powershell"])
                    .unwrap_or_else(|| "pwsh".into());
                Self::new(
                    "PowerShell",
                    shell,
                    "pi-powershell",
                    Some(POWERSHELL_UTF8_PREFIX),
                )
            }
            _ => Self::new(
                "bash",
                _path.unwrap_or_else(|| "bash".into()),
                "pi-bash",
                None,
            ),
        }
    }
}

impl ShellTool {
    /// `shell` 工具名。
    pub const NAME: &'static str = "shell";

    /// 用工作目录构造一个 `shell` 工具。
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self::with_options(cwd, ShellToolOptions::default())
    }

    /// 用工作目录与选项构造。底层 shell 按操作系统选择（Windows → PowerShell，其余 → bash）。
    pub fn with_options(cwd: impl AsRef<Path>, options: ShellToolOptions) -> Self {
        Self {
            cwd: Arc::from(cwd.as_ref()),
            spec: ShellSpec::from_path(options.shell_path),
            command_prefix: options.command_prefix,
        }
    }

    /// 工具名。
    pub fn name(&self) -> &'static str {
        Self::NAME
    }

    /// 发给模型的工具描述（与 pi 一致）。
    pub fn description(&self) -> String {
        self.spec.description.clone()
    }

    /// 参数的 JSON Schema（bash 与 PowerShell 相同）。
    pub fn parameters() -> ai::JsonValue {
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
        args: &JsonObject,
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
        // 顺序与 pi 一致：先注入工具自身的初始化脚本（PowerShell 的 UTF-8 设置），
        // 再拼接调用方配置的命令前缀，最后才是命令本体。
        let mut prefixes: Vec<&str> = Vec::new();
        if let Some(init) = self.spec.init_script {
            prefixes.push(init);
        }
        if let Some(prefix) = self.command_prefix.as_deref() {
            prefixes.push(prefix);
        }
        let command = match prefixes.is_empty() {
            true => command,
            false => format!("{}\n{command}", prefixes.join("\n")),
        };
        if tokio::fs::metadata(&self.cwd).await.is_err() {
            let _ = updates
                .send(error_result(format!(
                    "Working directory does not exist: {}\nCannot execute {} commands.",
                    self.cwd.display(),
                    self.spec.label
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

        let mut spawn = Command::new(&self.spec.shell);
        spawn
            .arg("-c")
            .arg(&command)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // `process_group` 只在类 Unix 平台存在；Windows 上退化为只杀直接子进程。
        #[cfg(unix)]
        spawn.process_group(0);
        let mut child = match spawn.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = updates
                    .send(error_result(format!("Failed to spawn command: {error}")))
                    .await;
                return;
            }
        };
        let pgid = child.id().map(|id| id as i32);
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let mut out_buf = vec![0u8; 8192];
        let mut err_buf = vec![0u8; 8192];
        let mut stdout_open = stdout.is_some();
        let mut stderr_open = stderr.is_some();

        let mut accumulator = OutputAccumulator::new(self.spec.temp_prefix);
        let started = Instant::now();
        let deadline = timeout.map(|seconds| started + Duration::from_secs_f64(seconds));
        let mut last_update = started;
        let mut terminated: Option<String> = None;

        while stdout_open || stderr_open {
            tokio::select! {
                result = read_chunk(&mut stdout, &mut out_buf), if stdout_open => {
                    match result {
                        Some(Ok(0)) => stdout_open = false,
                        Some(Ok(count)) => accumulator.append(&out_buf[..count]),
                        Some(Err(error)) => {
                            terminated = Some(format!("Failed to read command output: {error}"));
                            break;
                        }
                        None => stdout_open = false,
                    }
                }
                result = read_chunk(&mut stderr, &mut err_buf), if stderr_open => {
                    match result {
                        Some(Ok(0)) => stderr_open = false,
                        Some(Ok(count)) => accumulator.append(&err_buf[..count]),
                        Some(Err(error)) => {
                            terminated = Some(format!("Failed to read command output: {error}"));
                            break;
                        }
                        None => stderr_open = false,
                    }
                }
                () = sleep_until(deadline) => {
                    terminated = Some(format!("Command timed out after {} seconds", timeout.unwrap_or_default()));
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

        if terminated.is_some()
            && let Some(pgid) = pgid
        {
            kill_process_group(pgid);
        }
        let status = child.wait().await;
        accumulator.finish();
        let snapshot = accumulator.snapshot();
        let (mut text, details) = format_output(&snapshot, &mut accumulator);

        if let Some(message) = terminated {
            let _ = updates
                .send(error_result(append_status(&text, &message)))
                .await;
            return;
        }

        let exit_code = match status {
            Ok(status) => exit_code_of(&status),
            Err(error) => {
                let _ = updates
                    .send(error_result(append_status(
                        &text,
                        &format!("Failed to wait for command: {error}"),
                    )))
                    .await;
                return;
            }
        };
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

async fn read_chunk<S>(stream: &mut Option<S>, buffer: &mut [u8]) -> Option<std::io::Result<usize>>
where
    S: tokio::io::AsyncRead + Unpin,
{
    match stream.as_mut() {
        Some(stream) => Some(stream.read(buffer).await),
        None => None,
    }
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

fn parse_shell_args(args: &JsonObject) -> Result<(String, Option<f64>), String> {
    let command = arg_str(args, "command")?;
    let timeout = opt_f64(args, "timeout")?;
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

fn kill_process_group(_pgid: i32) {
    cfg_select! {
        unix => {
            use rustix::process::{Pid, Signal, kill_process_group};
            // `kill(-pgid, SIGKILL)`：杀掉整棵进程树，而不只是直接子进程。
            if let Some(pid) = Pid::from_raw(_pgid) {
                let _ = kill_process_group(pid, Signal::KILL);
            }
        }
        _ => {}
    }
}

fn exit_code_of(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
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
) -> (String, Option<ai::JsonValue>) {
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
) -> ai::JsonValue {
    let full = accumulator.read_full_output(STRUCTURED_OUTPUT_MAX_BYTES);
    serde_json::json!({
        "output": full.content,
        "truncated": full.truncated,
        "full_output_path": if full.truncated { snapshot.full_output_path.clone() } else { None },
        "exit_code": exit_code,
        "wall_time_seconds": (wall_time.as_millis() as f64 / 100.0).round() / 10.0,
    })
}

fn truncation_json(snapshot: &Snapshot) -> ai::JsonValue {
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

/// 构造统一的 `shell` 工具（Windows 上是 PowerShell，其余系统是 bash）。
pub fn create_shell_tool(cwd: impl AsRef<Path>) -> agent::AgentTool {
    let tool = ShellTool::new(cwd);
    agent::AgentTool::new(ShellTool::NAME, tool.clone())
        .label(ShellTool::NAME)
        .description(tool.description())
        .parameters(ShellTool::parameters())
        .output_schema(shell_output_schema())
}

fn shell_output_schema() -> ai::JsonValue {
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

/// 在 `PATH` 上查找第一个存在的可执行文件（用于在 Windows 上挑选 PowerShell 可执行文件名）。
#[cfg(windows)]
fn find_on_path(names: &[&str]) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: ai::JsonValue) -> JsonObject {
        match value {
            ai::JsonValue::Object(map) => map,
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
