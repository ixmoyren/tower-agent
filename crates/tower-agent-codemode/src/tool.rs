//! codemode 工具：把 [`CodemodeHost`] 包成 agent 能调的 [`AgentTool`]。
//!
//! 沙箱跑在一条**专用大栈线程**上——wasmtime 的同步执行用的是调用方线程的栈，而 guest 的
//! wasm 影子栈是 1 MiB（见 [`crate::guest_sandbox_options`]），所以不能直接在 tokio 的
//! worker（默认 2 MiB）上跑。
//!
//! 工具本体是个异步壳：请求经 channel 交给 worker 线程；**命令级进度**经另一个 channel 变成
//! [`ToolResultStream`] 的中间项（TUI 用），终态结果作为最后一项。

use std::{
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

use futures_core::Stream;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tower::{Service, util::BoxCloneService};
use tower_agent_core::{AgentError, AgentTool, AgentToolResult, ToolCallRequest, ToolResultStream};

use crate::{CodemodeError, CodemodeHost, CodemodeOptions};

/// 暴露给 agent 的工具名。
pub const TOOL_NAME: &str = "codemode";

/// 会话根形态。
#[derive(Debug, Clone)]
pub enum SessionRoot {
    /// 每次新建的临时目录：会话结束即删（默认，对应 Q10「阶段一会话即焚」）。
    Ephemeral,
    /// 固定目录：跨进程复用（M7 持久化）。
    Persistent(std::path::PathBuf),
}

/// worker 线程栈大小：必须明显大于 `CodemodeOptions::sandbox.max_wasm_stack`。
const WORKER_STACK: usize = 32 * 1024 * 1024;

/// 工具参数 JSON Schema。
#[must_use]
pub fn tool_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "code": {
                "type": "string",
                "description": "一段 JavaScript。可用：`await exec(name, args, opts)`（跑 coreutils）、`await tool_call(name, args)` / `tool_list()`（宿主工具）、`tool_register(name, handler, description, parameters)` / `tool_unregister(name)`（扩展工具）、`fs.readFile/writeFile/readdir`（会话目录）、`console.log`；`return` 的值也会带回。"
            },
            "timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "description": "脚本总超时（毫秒），默认 30000。"
            }
        },
        "required": ["code"],
        "additionalProperties": false
    })
}

/// 工具描述（发给模型）：API 摘要 + 范例，范例只用已注册的命令。
#[must_use]
pub fn tool_description() -> String {
    [
        "在 wasm 沙箱里跑一段 JavaScript，用来做多步「文件 + 命令」操作；只把结果带回上下文。",
        "",
        "API：",
        "- **命令函数**：每个命令一个函数，参数与命令行一致，可链式（管道由 JS 侧顺序执行）。`await cat(\"/a.txt\").sort().head(\"-n\", \"3\")` 直接得 stdout，字符串；例如 `await wc(\"-l\", \"/a.txt\")`。",
        "- 想拿退出码/原始 stderr 就用 `.result()`：`const r = await cat(\"/missing\").result()` → `{ code, stdout, stderr }`（不抛）；`await <cmd>` 遇非零退出会抛异常。",
        "- 逃生口：`await exec(\"sort\", [\"-n\", \"/a.txt\"], { stdin: \"4\\n2\\n\" })` → `{ code, stdout, stderr }`；命令共 60 个（cat head tail tac nl fold expand unexpand od cut tr uniq comm join paste split shuf tsort sort wc seq ls cp mv rm mkdir touch mktemp dirname basename realpath readlink ln link unlink truncate true false echo printf test expr date sleep tee yes pwd numfmt factor printenv md5sum sha1sum sha256sum sha512sum b2sum cksum sum base64 base32 basenc）。",
        "- `fs.readFile(path)` / `fs.writeFile(path, text)` / `fs.readdir(path)` / `fs.exists(path)` / `fs.mkdir(path)` / `fs.rm(path)`：`/` 是会话根（越界路径会抛错）。",
        "- `await tool_call(name, args)` 调用宿主工具，返回文本结果；`tool_list()` 返回可用工具元数据。",
        "- `tool_register(name, handler, description, parameters[, outputSchema])` 注册一个脚本函数作为新工具；`tool_unregister(name)` 只注销脚本自己注册的工具。",
        "- 命令有**硬超时**（默认 10 s，`exec(..., { timeoutMs })` 可覆盖；脚本整体默认 30 s）：超时会被取消，不会挂住。",
        "- `console.log(...)` 的输出会作为 stdout 带回；`return <值>` 会以 JSON 带回。",
        "",
        "范例：",
        "```js",
        "// 1) 排序去重后写回去，并回报行数（链式就是管道）",
        "const text = await sort(\"/nums.txt\").uniq();",
        "fs.writeFile(\"/sorted.txt\", text);",
        "return { lines: text.trim().split(\"\\n\").length };",
        "```",
        "```js",
        "// 2) 遍历目录、挑出 .log、统计总行数（用 JS 做过滤，不要在 shell 里写管道）",
        "const names = fs.readdir(\"/logs\").filter((n) => n.endsWith(\".log\"));",
        "let lines = 0;",
        "for (const name of names) {",
        "  const r = await wc(\"-l\", \"/logs/\" + name).result();",
        "  if (r.code === 0) { lines += Number(r.stdout.trim().split(/\\s+/)[0]); }",
        "}",
        "return { files: names.length, lines };",
        "```",
    ]
    .join("\n")
}

/// worker 线程收到的一次请求。
struct Job {
    code: String,
    timeout_ms: Option<u32>,
    updates: mpsc::UnboundedSender<AgentToolResult>,
    reply: oneshot::Sender<Outcome>,
}

/// worker 线程跑完一次脚本的结果。
struct Outcome {
    result: Result<crate::RunResult, CodemodeError>,
    duration_ms: u64,
}

/// codemode 工具（`Clone` 之后可以进 `ToolRouter`）。
#[derive(Clone)]
pub struct CodemodeTool {
    tx: mpsc::UnboundedSender<Job>,
    description: Arc<String>,
    parameters: Arc<Value>,
    /// 由工具自己持有的临时会话根（`for_workspace` 建的）；drop 工具即删目录（会话即焚）。
    session: Option<Arc<tempfile::TempDir>>,
}

impl CodemodeTool {
    /// 起一条专用 worker 线程并在其上编译两个组件；返回时组件已编译好（避免第一次调用慢启动）。
    pub fn new(options: CodemodeOptions) -> Result<Self, CodemodeError> {
        Self::new_with_host(
            options,
            tower_agent_plugins::HostApi::from_tools(Vec::<AgentTool>::new()),
            tower_agent_codemode_core::ScriptPermissions::default(),
        )
    }

    /// 起 worker，并把宿主工具表与脚本权限交给 codemode 宿主。
    pub fn new_with_host(
        options: CodemodeOptions,
        host: tower_agent_plugins::HostApi,
        permissions: tower_agent_codemode_core::ScriptPermissions,
    ) -> Result<Self, CodemodeError> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        std::thread::Builder::new()
            .name("codemode".to_owned())
            .stack_size(WORKER_STACK)
            .spawn(move || {
                let host = match CodemodeHost::new_with_host(options, host, permissions) {
                    Ok(host) => {
                        let _ = ready_tx.send(Ok(()));
                        host
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                while let Some(job) = rx.blocking_recv() {
                    let updates = job.updates.clone();
                    crate::exec::set_progress_sink(Some(Box::new(move |name, args| {
                        let _ = updates.send(command_started(name, args));
                    })));
                    let started = Instant::now();
                    let result = host.run(&job.code, job.timeout_ms);
                    let duration_ms = started.elapsed().as_millis() as u64;
                    crate::exec::set_progress_sink(None);
                    let _ = job.reply.send(Outcome {
                        result,
                        duration_ms,
                    });
                }
            })
            .map_err(|error| CodemodeError::Worker(error.to_string()))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(CodemodeError::Worker(error)),
            Err(_) => return Err(CodemodeError::Worker("worker 线程提前退出".to_owned())),
        }

        Ok(Self {
            tx,
            description: Arc::new(tool_description()),
            parameters: Arc::new(tool_parameters()),
            session: None,
        })
    }

    /// 默认布局：**临时会话根**（可写，映射成 guest 的 `/`）+ **工作区只读挂载**到 `/work`。
    ///
    /// 也就是「模型能读工作区、但改不到工作区」；要写就写会话根，而会话根随工具销毁即删
    /// （对应 Q10「阶段一会话即焚」）。
    pub fn for_workspace(workspace: impl AsRef<std::path::Path>) -> Result<Self, CodemodeError> {
        let session = tempfile::tempdir()
            .map_err(|error| CodemodeError::Worker(format!("建会话根失败：{error}")))?;
        let options = CodemodeOptions::embedded(session.path())
            .mount(crate::Mount::read_only(workspace.as_ref(), "/work"));
        let mut tool = Self::new(options)?;
        tool.session = Some(Arc::new(session));
        Ok(tool)
    }

    /// 临时会话根的路径（`for_workspace` 之外构造时为 `None`）。
    #[must_use]
    pub fn session_root(&self) -> Option<&std::path::Path> {
        self.session.as_ref().map(|dir| dir.path())
    }

    /// 命名会话（M7 持久化）：会话根固定为 `dir`（不存在则创建），**不随工具销毁**，
    /// 于是脚本产物可以跨进程复用（`/` 就是 `dir`）。
    ///
    /// 其余与 [`Self::for_workspace`] 相同：工作区只读挂到 `/work`。
    pub fn with_session_root(
        workspace: impl AsRef<std::path::Path>,
        dir: impl AsRef<std::path::Path>,
    ) -> Result<Self, CodemodeError> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(|error| {
            CodemodeError::Worker(format!("建会话目录 {} 失败：{error}", dir.display()))
        })?;
        let options = CodemodeOptions::embedded(dir.to_path_buf())
            .mount(crate::Mount::read_only(workspace.as_ref(), "/work"));
        // 会话根由调用方持有（它不是临时目录），所以这里不设 `session`。
        Self::new(options)
    }

    /// 完整版：指定会话根形态与额外挂载。
    ///
    /// 默认布局始终包含「工作区只读挂到 `/work`」；`session` 决定会话根是**临时**（即焚）
    /// 还是**持久**（跨进程复用），`mounts` 追加更多挂载。
    pub fn for_workspace_with(
        workspace: impl AsRef<std::path::Path>,
        session: SessionRoot,
        mounts: &[crate::Mount],
    ) -> Result<Self, CodemodeError> {
        Self::for_workspace_with_host(
            workspace,
            session,
            mounts,
            tower_agent_plugins::HostApi::from_tools(Vec::<AgentTool>::new()),
            tower_agent_codemode_core::ScriptPermissions::default(),
        )
    }

    /// 完整版：在 [`Self::for_workspace_with`] 基础上注入宿主工具与权限。
    pub fn for_workspace_with_host(
        workspace: impl AsRef<std::path::Path>,
        session: SessionRoot,
        mounts: &[crate::Mount],
        host: tower_agent_plugins::HostApi,
        permissions: tower_agent_codemode_core::ScriptPermissions,
    ) -> Result<Self, CodemodeError> {
        let (root, temp) = match session {
            SessionRoot::Ephemeral => {
                let dir = tempfile::tempdir()
                    .map_err(|error| CodemodeError::Worker(format!("建会话根失败：{error}")))?;
                (dir.path().to_path_buf(), Some(Arc::new(dir)))
            }
            SessionRoot::Persistent(dir) => {
                std::fs::create_dir_all(&dir).map_err(|error| {
                    CodemodeError::Worker(format!("建会话目录 {} 失败：{error}", dir.display()))
                })?;
                (dir, None)
            }
        };
        let mut options = CodemodeOptions::embedded(root)
            .mount(crate::Mount::read_only(workspace.as_ref(), "/work"));
        options.mounts.extend(mounts.iter().cloned());
        let mut tool = Self::new_with_host(options, host, permissions)?;
        tool.session = temp;
        Ok(tool)
    }

    /// 便捷入口：直接拿到可注册进 agent 工具集的 [`AgentTool`]。
    ///
    /// 注意：调用方要把工具跑在栈够大的地方——worker 线程已经承担了这件事，所以从 tokio
    /// 任务里调用是安全的。
    #[must_use]
    pub fn into_agent_tool(self) -> AgentTool {
        let description = self.description.as_str().to_owned();
        let parameters = self.parameters.as_ref().clone();
        AgentTool::new(TOOL_NAME, self)
            .label("codemode")
            .description(description)
            .parameters(parameters)
    }
}

/// 用给定选项构造 codemode 的 [`AgentTool`]。
pub fn tool(options: CodemodeOptions) -> Result<AgentTool, CodemodeError> {
    Ok(CodemodeTool::new(options)?.into_agent_tool())
}

impl Service<ToolCallRequest> for CodemodeTool {
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        let tx = self.tx.clone();
        Box::pin(async move {
            let arguments = &request.tool_call.arguments;
            let code = match arguments.arg_str("code") {
                Ok(code) if !code.trim().is_empty() => code,
                Ok(_) => {
                    return Ok(ToolResultStream::once(AgentToolResult::error(
                        "codemode: `code` 不能为空",
                    )));
                }
                Err(error) => {
                    return Ok(ToolResultStream::once(AgentToolResult::error(format!(
                        "codemode: {error}"
                    ))));
                }
            };
            let timeout_ms = match arguments.opt_u64("timeout_ms") {
                Ok(Some(value)) => u32::try_from(value).ok(),
                Ok(None) => None,
                Err(error) => {
                    return Ok(ToolResultStream::once(AgentToolResult::error(format!(
                        "codemode: {error}"
                    ))));
                }
            };

            let (updates, updates_rx) = mpsc::unbounded_channel();
            let (reply, reply_rx) = oneshot::channel();
            let job = Job {
                code,
                timeout_ms,
                updates,
                reply,
            };
            if tx.send(job).is_err() {
                return Ok(ToolResultStream::once(AgentToolResult::error(
                    "codemode: worker 已退出",
                )));
            }
            Ok(ToolResultStream::new(ToolStream {
                updates: updates_rx,
                reply: reply_rx,
                done: false,
            }))
        })
    }
}

/// 中间项（TUI）：一条命令即将开始。
fn command_started(name: &str, args: &[String]) -> AgentToolResult {
    let mut result = AgentToolResult::text(if args.is_empty() {
        format!("$ {name}")
    } else {
        format!("$ {name} {}", args.join(" "))
    });
    result.details = Some(json!({ "kind": "command-start", "name": name, "args": args }));
    result
}

/// 终态结果。
fn final_result(outcome: Outcome) -> AgentToolResult {
    let duration_ms = outcome.duration_ms;
    match outcome.result {
        Ok(run) => {
            let payload = json!({
                "ok": true,
                "value": run.value,
                "stdout": run.stdout,
                "commands": run.commands.iter().map(|c| json!({
                    "name": c.name, "args": c.args, "code": c.code,
                })).collect::<Vec<_>>(),
                "truncated": run.truncated,
                "duration_ms": duration_ms,
            });
            let mut result = AgentToolResult::text(model_text(&run));
            result.structured_content = Some(payload.clone());
            result.details = Some(payload);
            result
        }
        Err(error) => {
            let mut result = AgentToolResult::error(format!("codemode 脚本失败：{error}"));
            result.details = Some(json!({
                "ok": false,
                "error": error.to_string(),
                "duration_ms": duration_ms,
            }));
            result
        }
    }
}

/// 给模型看的文本：stdout 优先，附上 `return` 的值。
fn model_text(run: &crate::RunResult) -> String {
    let mut text = run.stdout.trim_end().to_owned();
    if let Some(value) = &run.value
        && value != "null"
    {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("return: ");
        text.push_str(value);
    }
    if run.truncated {
        text.push_str("\n[输出被截断]");
    }
    if text.is_empty() {
        text.push_str("(脚本没有输出)");
    }
    text
}

/// 把 worker 的两个 channel 拼成一个流：进度项先出，终态结果最后。
struct ToolStream {
    updates: mpsc::UnboundedReceiver<AgentToolResult>,
    reply: oneshot::Receiver<Outcome>,
    done: bool,
}

impl Stream for ToolStream {
    type Item = AgentToolResult;

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        // 先把已到的进度项吐完（channel 关闭后这里会稳定返回 None）。
        match this.updates.poll_recv(cx) {
            Poll::Ready(Some(item)) => return Poll::Ready(Some(item)),
            Poll::Ready(None) | Poll::Pending => {}
        }
        match std::pin::Pin::new(&mut this.reply).poll(cx) {
            Poll::Ready(Ok(outcome)) => {
                this.done = true;
                Poll::Ready(Some(final_result(outcome)))
            }
            Poll::Ready(Err(_)) => {
                this.done = true;
                Poll::Ready(Some(AgentToolResult::error("codemode: worker 未返回结果")))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// 让 `BoxCloneService` 的别名可用（`AgentTool::new` 内部要求）。
#[allow(dead_code)]
type BoxedTool = BoxCloneService<ToolCallRequest, ToolResultStream, AgentError>;
