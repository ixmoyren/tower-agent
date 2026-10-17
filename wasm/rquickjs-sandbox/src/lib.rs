//! codemode 的 JS 沙箱组件。
//!
//! 对外只有一个 `run(code, opts)`：在 rquickjs 里跑一段 JS。脚本能用的能力就两类——
//! **命令**（`exec()` → 宿主 import，宿主再去起 `wasm/coreutils-wasm`）和**会话目录里的文件**
//! （guest 内 `std::fs`，可见范围由 preopen 决定）。
//!
//! 上限照 `docs/codemode.md` §7.1：JS 栈 **64 KiB**（不是上游建议的 256 KiB——实测那样深递归会
//! 硬 trap，64 KiB 才是可捕获的 `RangeError`，见 §3.1）、堆 64 MiB、脚本总超时 30 s（可覆盖）。

#[allow(warnings)]
mod bindings;

use std::{
    cell::RefCell,
    time::{Duration, Instant},
};

use bindings::{
    exports::tower_agent::codemode::sandbox::{CommandLog, Guest, RunOpts, RunResult},
    tower_agent::codemode::host::{
        ExecOpts, ToolRegistration, exec, tool_call, tool_list, tool_register, tool_unregister,
    },
};
use rquickjs::{Context, Ctx, Function, Promise, Runtime, Value, promise::PromiseState};

/// 脚本总超时兜底（`opts.timeout-ms` 覆盖）。
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// JS 栈上限。见文件头注释：**不要**调到上游建议的 256 KiB。
const JS_STACK_SIZE: usize = 64 * 1024;
/// JS 堆上限。
const JS_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// 命令流水上限（审计用）。
const MAX_COMMAND_LOG: usize = 100;
/// `console.log` 收集上限（字节），超出截断并置 `truncated`。
const MAX_STDOUT_BYTES: usize = 1024 * 1024;

thread_local! {
    /// 本次 `run` 的命令流水（每次 `run` 开始时清空）。
    static COMMANDS: RefCell<Vec<CommandLog>> = const { RefCell::new(Vec::new()) };
}

/// 预载的 JS 门面：把原生的 `__exec` / `__readFile` / ... 包成好用的形状。
///
/// API 是 async 形状、实现是同步的——将来换成真正的异步 import 时脚本不用改。
///
/// 注意：这个字符串**会原样编进组件**，所以里面不写注释（只留运行需要的东西）：
/// - `console` 是宿主对象，QuickJS 默认没有 → 先兜一个空对象再挂方法；
/// - `__call` 用来把原生侧统一的 `{ok, data|error}` 翻译成「返回值 / 抛异常」
///   （rquickjs 的 `IntoJs` 不覆盖 `Result`）；
/// - `__run` 用 `AsyncFunction` 包脚本，这样既能用顶层 `await`、也能 `return`。
const PRELUDE: &str = r#"
globalThis.__stdout = [];
globalThis.console = globalThis.console || {};
console.log = (...args) => {
  __stdout.push(args.map((a) => (typeof a === "string" ? a : JSON.stringify(a))).join(" "));
};
console.error = console.log;
console.info = console.log;
globalThis.exec = async (name, args = [], opts = {}) => {
  const stdin = opts.stdin === undefined || opts.stdin === null ? "" : String(opts.stdin);
  const timeoutMs = opts.timeoutMs === undefined || opts.timeoutMs === null ? null : opts.timeoutMs;
  return JSON.parse(__exec(String(name), JSON.stringify(args), stdin, timeoutMs));
};
globalThis.__call = (payload) => {
  const r = JSON.parse(payload);
  if (!r.ok) throw new Error(r.error);
  return r.data;
};
globalThis.tool_call = async (name, args = {}) =>
  __call(__toolCall(String(name), JSON.stringify(args)));
globalThis.tool_list = () => __call(__toolList());
globalThis.tool_register = (name, handler, description = "", parameters = {}, outputSchema = null) =>
  __call(__toolRegister(
    String(name),
    String(handler),
    String(description),
    JSON.stringify(parameters),
    outputSchema === null || outputSchema === undefined ? null : JSON.stringify(outputSchema),
  ));
globalThis.tool_unregister = (name) => __call(__toolUnregister(String(name)));
globalThis.fs = {
  readFile: (path) => __call(__readFile(String(path))),
  writeFile: (path, data) => __call(__writeFile(String(path), String(data))),
  readdir: (path) => __call(__readdir(String(path))),
  exists: (path) => __call(__exists(String(path))),
  mkdir: (path) => __call(__mkdir(String(path))),
  rm: (path) => __call(__rm(String(path))),
};
globalThis.__stringify = (v) => JSON.stringify(v === undefined ? null : v);
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
globalThis.__run = (src) => new AsyncFunction(src)();
globalThis.__pipe = async (stages) => {
  let stdin = "";
  let last = { code: 0, stdout: "", stderr: "" };
  for (const [name, args] of stages) {
    last = await exec(name, args, { stdin });
    if (last.code !== 0) { break; }
    stdin = last.stdout;
  }
  return last;
};
class Cmd {
  constructor(stages) { this.__stages = stages; }
  __more(name, args) { return new Cmd(this.__stages.concat([[name, args]])); }
  result() { return __pipe(this.__stages); }
  then(onOk, onErr) {
    return __pipe(this.__stages).then((r) => {
      if (r.code !== 0) {
        throw new Error((r.stderr || "").trim() || ("command failed with code " + r.code));
      }
      return r.stdout;
    }).then(onOk, onErr);
  }
}
globalThis.Cmd = Cmd;
globalThis.__define = (names) => {
  for (const name of names) {
    Cmd.prototype[name] = function (...args) { return this.__more(name, args); };
    globalThis[name] = (...args) => new Cmd([[name, args]]);
  }
};
"#;

struct Component;

impl Guest for Component {
    fn run(code: String, opts: RunOpts) -> Result<RunResult, String> {
        COMMANDS.with(|c| c.borrow_mut().clear());

        let rt = Runtime::new().map_err(|e| format!("runtime: {e}"))?;
        rt.set_max_stack_size(JS_STACK_SIZE);
        rt.set_memory_limit(JS_MEMORY_LIMIT);

        let timeout_ms = opts.timeout_ms.map_or(DEFAULT_TIMEOUT_MS, u64::from);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        // 纯计算死循环也管：QuickJS 每次进入解释器/函数调用都会问一次。
        rt.set_interrupt_handler(Some(Box::new(move || Instant::now() > deadline)));

        let ctx = Context::full(&rt).map_err(|e| format!("context: {e}"))?;
        let cwd = opts.cwd.clone();
        let utils = serde_json::to_string(&opts.utils).map_err(|e| format!("utils: {e}"))?;

        let (value, stdout) = ctx.with(|ctx| -> Result<(Option<String>, String), String> {
            install(ctx.clone(), cwd)?;
            ctx.eval::<(), _>(PRELUDE)
                .map_err(|e| js_error(&ctx, "prelude", e))?;
            // 命令名由宿主提供（两边不各维护一份），在这里生成 `cat()` 与 `Cmd.prototype.cat`。
            ctx.eval::<(), _>(format!("__define({utils})"))
                .map_err(|e| js_error(&ctx, "define", e))?;

            // 源码经 JSON 转义成 JS 字符串字面量交给 `__run`，不拼字符串、避免注入/转义问题。
            let source = serde_json::to_string(&code).map_err(|e| format!("script: {e}"))?;
            let promise: Promise = ctx
                .eval(format!("__run({source})"))
                .map_err(|e| js_error(&ctx, "script", e))?;

            // 驱动 microtask 队列：原生函数是同步的，所以通常一两轮就结算。
            let mut rounds = 0usize;
            while promise.state() == PromiseState::Pending {
                if !ctx.execute_pending_job() || rounds > 100_000 {
                    break;
                }
                rounds += 1;
            }
            let value: Value = match promise.result::<Value>() {
                Some(Ok(value)) => value,
                Some(Err(e)) => return Err(js_error(&ctx, "script", e)),
                None => return Err("script: promise did not settle".to_owned()),
            };
            let stringify: Function = ctx
                .eval("__stringify")
                .map_err(|e| js_error(&ctx, "stringify", e))?;
            let value: String = stringify
                .call((value,))
                .map_err(|e| js_error(&ctx, "stringify", e))?;

            let stdout: String = ctx
                .eval("__stdout.join('\\n')")
                .map_err(|e| js_error(&ctx, "stdout", e))?;
            Ok((Some(value), stdout))
        })?;

        let (stdout, truncated) = truncate(stdout);
        let commands = COMMANDS.with(|c| c.borrow().clone());
        Ok(RunResult {
            value,
            stdout,
            commands,
            truncated,
        })
    }
}

/// 注册原生能力。`__exec` 之外的文件能力直接走 guest 的 `std::fs`（受 preopen 限制）。
fn install(ctx: Ctx<'_>, cwd: Option<String>) -> Result<(), String> {
    let exec_fn = Function::new(
        ctx.clone(),
        move |name: String, args_json: String, stdin: String, timeout_ms: Option<u32>| -> String {
            let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
            let opts = ExecOpts {
                stdin: Some(stdin.into_bytes()),
                cwd: cwd.clone(),
                timeout_ms,
            };
            // 宿主的错误（例如命令名没注册）也走同一条 JSON 通道，脚本侧可判断。
            let payload = match exec(&name, &args, &opts) {
                Ok(r) => {
                    COMMANDS.with(|c| {
                        let mut log = c.borrow_mut();
                        if log.len() < MAX_COMMAND_LOG {
                            log.push(CommandLog {
                                name: name.clone(),
                                args: args.clone(),
                                code: r.code,
                            });
                        }
                    });
                    serde_json::json!({
                        "code": r.code,
                        "stdout": String::from_utf8_lossy(&r.stdout),
                        "stderr": String::from_utf8_lossy(&r.stderr),
                    })
                }
                Err(e) => serde_json::json!({ "code": -1, "stdout": "", "stderr": e }),
            };
            payload.to_string()
        },
    )
    .map_err(|e| format!("__exec: {e}"))?;
    ctx.globals()
        .set("__exec", exec_fn)
        .map_err(|e| format!("__exec: {e}"))?;

    let call_tool = Function::new(
        ctx.clone(),
        |name: String, args_json: String| -> String {
            capability_payload(tool_call(&name, &args_json))
        },
    )
    .map_err(|e| format!("__toolCall: {e}"))?;
    ctx.globals()
        .set("__toolCall", call_tool)
        .map_err(|e| format!("__toolCall: {e}"))?;

    let list_tools = Function::new(ctx.clone(), || -> String {
        let result = tool_list().map(|tools| {
            tools
                .into_iter()
                .map(|tool| {
                    serde_json::json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": serde_json::from_str::<serde_json::Value>(&tool.parameters)
                            .unwrap_or_else(|_| serde_json::json!({ "type": "object" })),
                        "output_schema": tool.output_schema.and_then(|schema| {
                            serde_json::from_str::<serde_json::Value>(&schema).ok()
                        }),
                    })
                })
                .collect::<Vec<_>>()
        });
        capability_payload(result)
    })
    .map_err(|e| format!("__toolList: {e}"))?;
    ctx.globals()
        .set("__toolList", list_tools)
        .map_err(|e| format!("__toolList: {e}"))?;

    let register_tool = Function::new(
        ctx.clone(),
        |name: String,
         handler: String,
         description: String,
         parameters: String,
         output_schema: Option<String>|
         -> String {
            capability_payload(tool_register(&ToolRegistration {
                name,
                handler,
                description,
                parameters,
                output_schema,
            }))
        },
    )
    .map_err(|e| format!("__toolRegister: {e}"))?;
    ctx.globals()
        .set("__toolRegister", register_tool)
        .map_err(|e| format!("__toolRegister: {e}"))?;

    let unregister_tool = Function::new(ctx.clone(), |name: String| -> String {
        capability_payload(tool_unregister(&name))
    })
    .map_err(|e| format!("__toolUnregister: {e}"))?;
    ctx.globals()
        .set("__toolUnregister", unregister_tool)
        .map_err(|e| format!("__toolUnregister: {e}"))?;

    let read = Function::new(ctx.clone(), |path: String| -> String {
        match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::json!({ "ok": true, "data": data }).to_string(),
            Err(e) => {
                serde_json::json!({ "ok": false, "error": format!("{path}: {e}") }).to_string()
            }
        }
    })
    .map_err(|e| format!("__readFile: {e}"))?;
    ctx.globals()
        .set("__readFile", read)
        .map_err(|e| format!("__readFile: {e}"))?;

    let write = Function::new(ctx.clone(), |path: String, data: String| -> String {
        match std::fs::write(&path, data) {
            Ok(()) => serde_json::json!({ "ok": true, "data": null }).to_string(),
            Err(e) => {
                serde_json::json!({ "ok": false, "error": format!("{path}: {e}") }).to_string()
            }
        }
    })
    .map_err(|e| format!("__writeFile: {e}"))?;
    ctx.globals()
        .set("__writeFile", write)
        .map_err(|e| format!("__writeFile: {e}"))?;

    let readdir = Function::new(ctx.clone(), |path: String| -> String {
        let listing = (|| -> std::io::Result<Vec<String>> {
            let mut names = Vec::new();
            for entry in std::fs::read_dir(&path)? {
                names.push(entry?.file_name().to_string_lossy().into_owned());
            }
            Ok(names)
        })();
        match listing {
            Ok(names) => serde_json::json!({ "ok": true, "data": names }).to_string(),
            Err(e) => {
                serde_json::json!({ "ok": false, "error": format!("{path}: {e}") }).to_string()
            }
        }
    })
    .map_err(|e| format!("__readdir: {e}"))?;
    ctx.globals()
        .set("__readdir", readdir)
        .map_err(|e| format!("__readdir: {e}"))?;

    let exists = Function::new(ctx.clone(), |path: String| -> String {
        serde_json::json!({ "ok": true, "data": std::path::Path::new(&path).exists() }).to_string()
    })
    .map_err(|e| format!("__exists: {e}"))?;
    ctx.globals()
        .set("__exists", exists)
        .map_err(|e| format!("__exists: {e}"))?;

    let mkdir = Function::new(ctx.clone(), |path: String| -> String {
        match std::fs::create_dir_all(&path) {
            Ok(()) => serde_json::json!({ "ok": true, "data": null }).to_string(),
            Err(e) => {
                serde_json::json!({ "ok": false, "error": format!("{path}: {e}") }).to_string()
            }
        }
    })
    .map_err(|e| format!("__mkdir: {e}"))?;
    ctx.globals()
        .set("__mkdir", mkdir)
        .map_err(|e| format!("__mkdir: {e}"))?;

    let rm = Function::new(ctx.clone(), |path: String| -> String {
        let outcome = if std::path::Path::new(&path).is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match outcome {
            Ok(()) => serde_json::json!({ "ok": true, "data": null }).to_string(),
            Err(e) => {
                serde_json::json!({ "ok": false, "error": format!("{path}: {e}") }).to_string()
            }
        }
    })
    .map_err(|e| format!("__rm: {e}"))?;
    ctx.globals()
        .set("__rm", rm)
        .map_err(|e| format!("__rm: {e}"))?;

    Ok(())
}

fn capability_payload<T: serde::Serialize>(result: Result<T, String>) -> String {
    match result {
        Ok(data) => serde_json::json!({ "ok": true, "data": data }).to_string(),
        Err(error) => serde_json::json!({ "ok": false, "error": error }).to_string(),
    }
}

/// 把 JS 异常整理成一句给宿主看的话。
///
/// rquickjs 的 `Error` 只给「Exception generated by QuickJS」这种笼统文案，真正的信息在
/// 挂起的 JS 异常对象里——用 `Ctx::catch()` 取出来，带上 message（和 stack）。
fn js_error(ctx: &Ctx<'_>, what: &str, err: rquickjs::Error) -> String {
    let caught = ctx.catch();
    let exception = caught
        .into_object()
        .and_then(rquickjs::Exception::from_object);
    let message = exception
        .as_ref()
        .and_then(|e| e.message())
        .unwrap_or_else(|| err.to_string());
    match exception.and_then(|e| e.stack()) {
        Some(stack) => format!("{what}: {message}\n{stack}"),
        None => format!("{what}: {message}"),
    }
}

/// `console.log` 输出截断。
fn truncate(mut stdout: String) -> (String, bool) {
    if stdout.len() <= MAX_STDOUT_BYTES {
        return (stdout, false);
    }
    let cut = (0..=MAX_STDOUT_BYTES)
        .rev()
        .find(|&i| stdout.is_char_boundary(i))
        .unwrap_or(0);
    stdout.truncate(cut);
    (stdout, true)
}

bindings::export!(Component with_types_in bindings);
