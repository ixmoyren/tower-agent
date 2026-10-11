//! codemode 宿主：驱动 `wasm/codemode-sandbox` 那份 JS 沙箱组件，并实现它的命令层
//! （`tower-agent:codemode/host.exec` → 起 `wasm/coreutils-wasm` 那份 multicall 组件）。
//!
//! 分工：
//! - **JS 在 guest**（rquickjs 编到 `wasm32-wasip2`），脚本的内存/栈/时间上限在组件内强制；
//! - **命令在宿主**：本 crate 实现 `exec`，把每个命令跑成一份新的 coreutils 组件实例
//!   （内存 stdio，会话根映射成 guest 的 `/`）；
//! - 引擎、WASI、内存上限、JIT 缓存复用 [`tower_agent_wasmtime`]。
//!
//! 每次 `run` 都新建 `Store`：组件只编译一次（JIT 缓存），实例用完即丢——所以 guest trap
//! （例如 JS 深递归、脚本超时）只影响那一次调用，不需要额外的「丢弃实例」逻辑。

mod exec;
mod plugin;
mod tool;

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
};

use tower_agent_wasmtime::{HostState, SandboxEngine, SandboxError, SandboxOptions};
use wasmtime::{Store, StoreLimitsBuilder};
use wasmtime_wasi::{FsPerms, ResourceTable, WasiCtxBuilder};

/// guest 的导出类型（`run` 的入参与结果）。
pub use self::component::exports::tower_agent::codemode::sandbox::{
    CommandLog, RunOpts, RunResult,
};
use self::component::tower_agent::codemode::host::{ExecOpts, ExecResult};
/// 命令层后端（宿主实现的那一半）。
pub use exec::ExecService;
/// codemode 的静态插件形态（名字与能力受保护）。
pub use plugin::{CodemodePlugin, PLUGIN_NAME};
/// 暴露给 agent 的 codemode 工具（`AgentTool`）。
pub use tool::{CodemodeTool, SessionRoot, TOOL_NAME, tool, tool_description, tool_parameters};

/// 命名会话的默认目录：`<config>/tower-agent/codemode/<name>`（与 mods 的持久目录同源）。
#[must_use]
pub fn default_session_dir(name: &str) -> Option<PathBuf> {
    sessions_root().map(|root| root.join(name))
}

/// 命名会话的基目录：`<config>/tower-agent/codemode`。
#[must_use]
pub fn sessions_root() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("tower-agent").join("codemode"))
}

/// 会话名合不合法：只允许一层普通名字（挡掉 `/`、`..` 之类，避免跑到基目录外面）。
#[must_use]
pub fn session_name_is_valid(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && Path::new(name).components().count() == 1
}

/// 列出 `root` 下的命名会话（目录名 + 路径，按名字排序）——不碰真实 config，便于测试。
#[must_use]
pub fn list_sessions_in(root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut sessions: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name, entry.path())
        })
        .collect();
    sessions.sort();
    sessions
}

/// 删掉 `root` 下的一个命名会话（连同里面的产物）；返回「本来就存在吗」。
pub fn clear_session_in(root: &Path, name: &str) -> std::io::Result<bool> {
    if !session_name_is_valid(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("会话名 `{name}` 不合法（只能是一层普通名字）"),
        ));
    }
    let dir = root.join(name);
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(true)
}

/// 内嵌的 JS 沙箱组件（由 `wasm/codemode-sandbox/generate.sh` 生成并入库）。
pub const EMBEDDED_SANDBOX: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/codemode.wasm"));

/// 内嵌的命令层组件（由 `wasm/coreutils-wasm/generate.sh` 生成并入库）。
pub const EMBEDDED_COREUTILS: &[u8] = include_bytes!(concat!(
env!("CARGO_MANIFEST_DIR"),
"/assets/coreutils.wasm"
));

/// `bindgen!` 生成的组件绑定（生成的项没有 doc，这里统一放行）。
#[allow(missing_docs)]
mod component {
    wasmtime::component::bindgen!({
        path: "../../wasm/codemode-sandbox/wit",
        world: "codemode",
        // 导出侧要异步绑定：这样 `call_run` 是可取消的 future（超时才能掐断）。
        exports: { default: async },
    });
}

/// 默认脚本总超时（毫秒），与组件内默认值一致。
pub const DEFAULT_TIMEOUT_MS: u32 = 30_000;

/// 单条命令的默认超时（见 §7.1）。命令超时由 wasmtime 的 epoch 中断实现。
pub const DEFAULT_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 命令层可用的命令名，按 `wasm/coreutils-wasm` 里的 multicall 登记表排列。
///
/// 这份列表会随 `run-opts` 传给 guest，由它生成 JS 侧的 `cat()` / `Cmd.prototype.cat`——
/// 所以**两端必须对齐**；`tests/smoke.rs` 里有一个「拿 wrapper 源码比对」的漂移测试守着。
pub const COMMANDS: &[&str] = &[
    // 文本
    "cat",
    "head",
    "tail",
    "tac",
    "nl",
    "fold",
    "expand",
    "unexpand",
    "od",
    "cut",
    "tr",
    "uniq",
    "comm",
    "join",
    "paste",
    "split",
    "shuf",
    "tsort",
    "sort",
    "wc",
    "seq",
    // 文件 / 路径
    "ls",
    "cp",
    "mv",
    "rm",
    "mkdir",
    "touch",
    "mktemp",
    "dirname",
    "basename",
    "realpath",
    "readlink",
    "ln",
    "link",
    "unlink",
    "truncate",
    // 杂项
    "true",
    "false",
    "echo",
    "printf",
    "test",
    "expr",
    "date",
    "sleep",
    "tee",
    "yes",
    "pwd",
    "numfmt",
    "factor",
    "printenv",
    // 校验 / 编码
    "md5sum",
    "sha1sum",
    "sha256sum",
    "sha512sum",
    "b2sum",
    "cksum",
    "sum",
    "base64",
    "base32",
    "basenc",
];

/// codemode 的构造选项。
#[derive(Debug, Clone)]
pub struct CodemodeOptions {
    /// 会话根目录：映射成 guest 的 `/`，也是脚本里 `fs.*` 的可见范围。
    pub session_root: PathBuf,
    /// 除会话根之外的挂载（例如把工作区只读挂到 `/work`）。
    pub mounts: Vec<Mount>,
    /// JS 沙箱组件字节（默认取内嵌的 [`EMBEDDED_SANDBOX`]）。
    pub sandbox_wasm: Cow<'static, [u8]>,
    /// 命令层组件字节（默认取内嵌的 [`EMBEDDED_COREUTILS`]）。
    pub coreutils_wasm: Cow<'static, [u8]>,
    /// wasmtime 侧选项（内存上限、JIT 缓存目录）。
    pub sandbox: SandboxOptions,
    /// 单条命令的默认墙钟上限（`exec` 的 `timeoutMs` 可按次覆盖）。
    pub command_timeout: std::time::Duration,
}

impl CodemodeOptions {
    /// 用内嵌的两个组件构造（正式路径）。
    #[must_use]
    pub fn embedded(session_root: impl Into<PathBuf>) -> Self {
        Self::with_artifacts(
            session_root,
            Cow::Borrowed(EMBEDDED_SANDBOX),
            Cow::Borrowed(EMBEDDED_COREUTILS),
        )
    }

    /// 用指定的组件字节构造（测试造特殊产物、或将来想换成外部文件时用）。
    #[must_use]
    pub fn with_artifacts(
        session_root: impl Into<PathBuf>,
        sandbox_wasm: Cow<'static, [u8]>,
        coreutils_wasm: Cow<'static, [u8]>,
    ) -> Self {
        Self {
            session_root: session_root.into(),
            mounts: Vec::new(),
            sandbox_wasm,
            coreutils_wasm,
            sandbox: guest_sandbox_options(),
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
        }
    }

    /// 追加载一次挂载（链式构造）。
    #[must_use]
    pub fn mount(mut self, mount: Mount) -> Self {
        self.mounts.push(mount);
        self
    }

    /// 指定 wasmtime 的 JIT 缓存目录（默认 `~/.cache/tower-agent/wasmtime`）。
    ///
    /// **缓存不可写时会静默退回「每次重新编译」**（coreutils 7.4 MB，实测冷启动 ~10 s），
    /// 所以沙箱 / CI 里要么在这里指一个可写目录，要么设 `XDG_CACHE_HOME`（见 docs §9.2）。
    #[must_use]
    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.sandbox.cache_dir = Some(dir.into());
        self
    }
}

/// 一次挂载：宿主目录 → guest 路径。
///
/// 会话根之外的东西一律显式挂载，并且默认**只读**——脚本要写就只能写会话根（`/`）。
#[derive(Debug, Clone)]
pub struct Mount {
    /// 宿主侧目录。
    pub host: PathBuf,
    /// guest 侧路径（例如 `/work`）。
    pub guest: String,
    /// 只读挂载（默认 `true`）。
    pub read_only: bool,
}

impl Mount {
    /// 解析 CLI 风格的挂载规格：`<host>[:<guest>][:ro|rw]`。
    ///
    /// 缺省 guest 是 `/work`，缺省模式是 **只读**（写权限默认只给会话根）。
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut parts = spec.split(':');
        let host = parts
            .next()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| {
                format!("挂载规格 `{spec}` 缺 host（应为 `<dir>[:<guest>][:ro|rw]`）")
            })?;
        let mut guest = "/work".to_owned();
        let mut read_only = true;
        for part in parts {
            match part {
                "" => {}
                "ro" => read_only = true,
                "rw" => read_only = false,
                other if other.starts_with('/') => guest = other.to_owned(),
                other => {
                    return Err(format!(
                        "挂载规格 `{spec}` 里 `{other}` 不合法（guest 要以 / 开头，模式只能是 ro/rw）"
                    ));
                }
            }
        }
        Ok(Self {
            host: PathBuf::from(host),
            guest,
            read_only,
        })
    }

    /// 只读挂载。
    #[must_use]
    pub fn read_only(host: impl Into<PathBuf>, guest: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            guest: guest.into(),
            read_only: true,
        }
    }

    /// 可写挂载（少用；写权限默认只给会话根）。
    #[must_use]
    pub fn read_write(host: impl Into<PathBuf>, guest: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            guest: guest.into(),
            read_only: false,
        }
    }
}

/// 把「会话根 + 额外挂载」装成 preopen，供 guest 与命令组件共用同一套可见范围。
pub(crate) fn preopen(
    builder: &mut WasiCtxBuilder,
    root: &Path,
    mounts: &[Mount],
) -> Result<(), CodemodeError> {
    builder
        .preopened_dir(root, "/", FsPerms::ReadWrite)
        .map_err(|e| CodemodeError::Link(format!("preopen {}: {e}", root.display())))?;
    for mount in mounts {
        let perms = if mount.read_only {
            FsPerms::ReadOnly
        } else {
            FsPerms::ReadWrite
        };
        builder
            .preopened_dir(&mount.host, &mount.guest, perms)
            .map_err(|e| {
                CodemodeError::Link(format!(
                    "preopen {} → {}: {e}",
                    mount.host.display(),
                    mount.guest
                ))
            })?;
    }
    Ok(())
}

/// guest 引擎的默认 wasmtime 选项。
fn guest_sandbox_options() -> SandboxOptions {
    SandboxOptions {
        // QuickJS 的 64 KiB 栈上限要「可捕获」，就得让 wasm 影子栈明显大于它：
        // 512 KiB（wasmtime 默认）下深递归会先撞 wasm 栈、变成硬 trap（实测）。
        //
        // 但 wasmtime 的**同步**执行用的是调用方线程的栈，所以这个值必须明显小于宿主
        // 线程的栈——[`CodemodeTool`] 因此把沙箱放进一条栈够大的专用线程里跑。
        max_wasm_stack: 1024 * 1024,
        async_support: true,
        ..SandboxOptions::default()
    }
}

/// codemode 宿主。
pub struct CodemodeHost {
    engine: Arc<SandboxEngine>,
    exec: Arc<ExecService>,
    root: PathBuf,
    mounts: Vec<Mount>,
}

impl CodemodeHost {
    /// 编译两个组件、装好命令层，得到一个可反复调用的宿主。
    pub fn new(options: CodemodeOptions) -> Result<Self, CodemodeError> {
        let exec = Arc::new(ExecService::new(
            &options.coreutils_wasm,
            &options.session_root,
            &options.mounts,
            options.command_timeout,
            &options.sandbox,
        )?);

        let mut engine = SandboxEngine::new(&options.sandbox_wasm, options.sandbox.clone())?;
        let backend = Arc::clone(&exec);
        engine
            .linker_mut()
            .instance("tower-agent:codemode/host@0.1.0")
            .map_err(|e| CodemodeError::Link(e.to_string()))?
            .func_wrap_async(
                "exec",
                move |_store: wasmtime::StoreContextMut<'_, HostState>,
                      (name, args, opts): (String, Vec<String>, ExecOpts)|
                      -> Box<
                          dyn std::future::Future<
                              Output = wasmtime::Result<(Result<ExecResult, String>,)>,
                          > + Send
                          + '_,
                      > {
                    // 宿主 import 走异步：命令被 `await`，于是它的超时能**取消**整个调用，
                    // 而不是把宿主线程一起卡住（epoch 只能打断正在执行的 wasm 指令）。
                    let backend = Arc::clone(&backend);
                    Box::new(async move {
                        // 命令层自己把错误变成 `result<_, string>`，不让它升级成 wasm trap。
                        Ok((backend.exec(&name, &args, &opts).await,))
                    })
                },
            )
            .map_err(|e| CodemodeError::Link(e.to_string()))?;

        Ok(Self {
            engine: Arc::new(engine),
            exec,
            root: options.session_root,
            mounts: options.mounts,
        })
    }

    /// 会话根目录。
    #[must_use]
    pub fn session_root(&self) -> &Path {
        &self.root
    }

    /// 命令层后端（想直接跑命令、或做冒烟测试时用）。
    #[must_use]
    pub fn exec(&self) -> &ExecService {
        &self.exec
    }

    /// 跑一段 JS，返回脚本的 `return` 值（JSON）、`console.log` 收集与命令流水。
    ///
    /// 整段脚本也有硬时限（`timeout_ms`，默认 30 s）：超时是**取消**（异步链路），
    /// 连卡在宿主调用里的脚本也能掐掉。
    pub async fn run_async(
        &self,
        code: &str,
        timeout_ms: Option<u32>,
    ) -> Result<RunResult, CodemodeError> {
        let mut store = self.store()?;
        let bindings = component::Codemode::instantiate_async(
            &mut store,
            self.engine.component(),
            self.engine.linker(),
        )
            .await
            .map_err(|e| CodemodeError::Link(e.to_string()))?;
        let opts = RunOpts {
            cwd: None,
            timeout_ms: Some(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
            utils: COMMANDS.iter().map(|name| (*name).to_owned()).collect(),
        };
        // 三层：超时 → wasm 级失败（trap 之类）→ 脚本自己返回的 `Err(message)`。
        let budget = std::time::Duration::from_millis(u64::from(
            opts.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
        ));
        let outcome = tokio::time::timeout(
            budget,
            bindings
                .tower_agent_codemode_sandbox()
                .call_run(&mut store, code, &opts),
        )
            .await
            .map_err(|_| CodemodeError::Script(format!("脚本超时（{} ms）", budget.as_millis())))?
            .map_err(|e| CodemodeError::Script(describe(&e)))?;
        outcome.map_err(CodemodeError::Script)
    }

    /// 同步便利入口：内部起一个 current-thread runtime 跑 [`Self::run_async`]。
    ///
    /// **不要在 async 上下文里调用**（会 panic）——async 上下文请直接用 `run_async`。
    pub fn run(&self, code: &str, timeout_ms: Option<u32>) -> Result<RunResult, CodemodeError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CodemodeError::Link(format!("runtime: {e}")))?;
        runtime.block_on(self.run_async(code, timeout_ms))
    }

    /// 新建 guest 的 `Store`：会话根 preopen 成 guest 的 `/`。
    fn store(&self) -> Result<Store<HostState>, CodemodeError> {
        let mut builder = WasiCtxBuilder::new();
        builder.args(&["codemode"]);
        preopen(&mut builder, &self.root, &self.mounts)?;
        let mut store = Store::new(
            self.engine.engine(),
            HostState {
                ctx: builder.build(),
                table: ResourceTable::new(),
                limits: StoreLimitsBuilder::new()
                    .memory_size(self.engine.memory_limit())
                    .build(),
            },
        );
        store.limiter(|state| &mut state.limits);
        Ok(store)
    }
}

/// 把 wasmtime 的错误整理成一句给上层看的话（trap 也走这里）。
fn describe(error: &wasmtime::Error) -> String {
    // `wasmtime::Error` 是 anyhow 的别名，用它的 `chain()` 把 trap 的原因串起来。
    let mut text = String::new();
    for (index, cause) in error.chain().enumerate() {
        if index > 0 {
            text.push_str(": ");
        }
        text.push_str(&cause.to_string());
    }
    text
}

/// codemode 宿主错误。
#[derive(Debug, thiserror::Error)]
pub enum CodemodeError {
    /// 工具 worker 线程创建失败或中途退出。
    #[error("codemode worker: {0}")]
    Worker(String),
    /// 组装引擎/linker/实例化失败。
    #[error("codemode link error: {0}")]
    Link(String),
    /// 沙箱基础设施失败（引擎、编译、内存上限）。
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    /// 脚本本身失败：JS 异常、超时、或 guest trap（深递归、内存超限）。
    #[error("script failed: {0}")]
    Script(String),
}
