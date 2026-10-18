//! codemode 宿主：驱动 `wasm/rquickjs-sandbox` 那份 JS 沙箱组件，并实现它的命令层
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

mod capability;
mod exec;
mod plugin;
mod tool;

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll},
};

use futures_util::{StreamExt as _, future::BoxFuture};
use serde_json::Value;
use tower::Service;
use tower_agent_codemode_core::{Capability, CapabilityError, ScriptPermissions};
use tower_agent_core::{
    AgentError, AgentTool, AgentToolResult, Panel, PanelAnchor, StatusSegment, ToolCallRequest,
    ToolRenderer, ToolResultStream, UiState,
};
use tower_agent_plugins::HostApi;
use tower_agent_provider::{Arguments, ContentBlock, ToolCall};
use tower_agent_wasmtime::{HostState, SandboxEngine, SandboxError, SandboxOptions};
use wasmtime::{Store, StoreLimitsBuilder};
use wasmtime_wasi::{FsPerms, ResourceTable, WasiCtxBuilder};

/// guest 的导出类型（`run` 的入参与结果）。
pub use self::component::exports::tower_agent::codemode::sandbox::{
    CommandLog, RunOpts, RunResult,
};
use self::component::tower_agent::codemode::host::{
    ExecOpts, ExecResult, ToolDescriptor as WireToolDescriptor,
    ToolRegistration as WireToolRegistration,
};
/// 把 coreutils 命令层包装成两个引擎共用的 `CapabilitySurface`。
pub use capability::ExecCapabilitySurface;
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

/// 内嵌的 JS 沙箱组件（由 `wasm/rquickjs-sandbox/generate.sh` 生成并入库）。
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
        path: "../../wasm/rquickjs-sandbox/wit",
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

struct HostBackend {
    exec: Arc<ExecService>,
    host: HostApi,
    permissions: ScriptPermissions,
    current_source: Mutex<Option<String>>,
    script_tools: Mutex<BTreeSet<String>>,
    state: Mutex<BTreeMap<String, Value>>,
    ui: UiState,
    this: Mutex<Weak<CodemodeHost>>,
}

type HostFuture<'a, T> = Box<dyn std::future::Future<Output = wasmtime::Result<(T,)>> + Send + 'a>;

/// codemode 宿主。
pub struct CodemodeHost {
    engine: Arc<SandboxEngine>,
    backend: Arc<HostBackend>,
    root: PathBuf,
    mounts: Vec<Mount>,
}

impl CodemodeHost {
    /// 编译两个组件、装好命令层，得到一个可反复调用的宿主。
    pub fn new(options: CodemodeOptions) -> Result<Arc<Self>, CodemodeError> {
        Self::new_with_host(
            options,
            HostApi::from_tools(Vec::<AgentTool>::new()),
            ScriptPermissions::default(),
        )
    }

    /// 构造带宿主工具表与脚本权限的 codemode 宿主。
    pub fn new_with_host(
        options: CodemodeOptions,
        host: HostApi,
        permissions: ScriptPermissions,
    ) -> Result<Arc<Self>, CodemodeError> {
        Self::new_with_host_and_ui(options, host, permissions, UiState::new())
    }

    /// 构造 codemode 宿主，并注入共享 UI 状态。
    pub fn new_with_host_and_ui(
        options: CodemodeOptions,
        host: HostApi,
        permissions: ScriptPermissions,
        ui: UiState,
    ) -> Result<Arc<Self>, CodemodeError> {
        let exec = Arc::new(ExecService::new(
            &options.coreutils_wasm,
            &options.session_root,
            &options.mounts,
            options.command_timeout,
            &options.sandbox,
        )?);
        let backend = Arc::new(HostBackend {
            exec,
            host,
            permissions,
            current_source: Mutex::new(None),
            script_tools: Mutex::new(BTreeSet::new()),
            state: Mutex::new(BTreeMap::new()),
            ui,
            this: Mutex::new(Weak::new()),
        });

        let mut engine = SandboxEngine::new(&options.sandbox_wasm, options.sandbox.clone())?;
        let backend_for_exec = Arc::clone(&backend);
        engine
            .linker_mut()
            .instance("tower-agent:codemode/host@0.1.0")
            .map_err(|e| CodemodeError::Link(e.to_string()))?
            .func_wrap_async(
                "exec",
                move |_store: wasmtime::StoreContextMut<'_, HostState>,
                      (name, args, opts): (String, Vec<String>, ExecOpts)|
                      -> HostFuture<'_, Result<ExecResult, String>> {
                    let backend = Arc::clone(&backend_for_exec);
                    Box::new(async move {
                        if let Err(error) = backend.permissions.authorize(Capability::Exec) {
                            return Ok((Err(capability_error_text(&error)),));
                        }
                        Ok((backend.exec.exec(&name, &args, &opts).await,))
                    })
                },
            )
            .map_err(|e| CodemodeError::Link(e.to_string()))?;

        let backend_for_tools = Arc::clone(&backend);
        engine
            .linker_mut()
            .instance("tower-agent:codemode/host@0.1.0")
            .map_err(|e| CodemodeError::Link(e.to_string()))?
            .func_wrap_async(
                "tool-call",
                move |_store: wasmtime::StoreContextMut<'_, HostState>,
                      (name, args_json): (String, String)|
                      -> HostFuture<'_, Result<String, String>> {
                    let backend = Arc::clone(&backend_for_tools);
                    Box::new(async move { Ok((backend.tool_call(&name, &args_json).await,)) })
                },
            )
            .map_err(|e| CodemodeError::Link(e.to_string()))?;

        let backend_for_list = Arc::clone(&backend);
        engine
            .linker_mut()
            .instance("tower-agent:codemode/host@0.1.0")
            .map_err(|e| CodemodeError::Link(e.to_string()))?
            .func_wrap_async(
                "tool-list",
                move |_store: wasmtime::StoreContextMut<'_, HostState>,
                      (): ()|
                      -> HostFuture<'_, Result<Vec<WireToolDescriptor>, String>> {
                    let backend = Arc::clone(&backend_for_list);
                    Box::new(async move { Ok((backend.tool_list(),)) })
                },
            )
            .map_err(|e| CodemodeError::Link(e.to_string()))?;

        let backend_for_register = Arc::clone(&backend);
        engine
            .linker_mut()
            .instance("tower-agent:codemode/host@0.1.0")
            .map_err(|e| CodemodeError::Link(e.to_string()))?
            .func_wrap_async(
                "tool-register",
                move |_store: wasmtime::StoreContextMut<'_, HostState>,
                      (registration,): (WireToolRegistration,)|
                      -> HostFuture<'_, Result<bool, String>> {
                    let backend = Arc::clone(&backend_for_register);
                    Box::new(async move { Ok((backend.tool_register(registration),)) })
                },
            )
            .map_err(|e| CodemodeError::Link(e.to_string()))?;

        let backend_for_unregister = Arc::clone(&backend);
        engine
            .linker_mut()
            .instance("tower-agent:codemode/host@0.1.0")
            .map_err(|e| CodemodeError::Link(e.to_string()))?
            .func_wrap_async(
                "tool-unregister",
                move |_store: wasmtime::StoreContextMut<'_, HostState>,
                      (name,): (String,)|
                      -> HostFuture<'_, Result<bool, String>> {
                    let backend = Arc::clone(&backend_for_unregister);
                    Box::new(async move { Ok((backend.tool_unregister(&name),)) })
                },
            )
            .map_err(|e| CodemodeError::Link(e.to_string()))?;

        let backend_for_state_get = Arc::clone(&backend);
        engine
            .linker_mut()
            .instance("tower-agent:codemode/host@0.1.0")
            .map_err(|e| CodemodeError::Link(e.to_string()))?
            .func_wrap_async(
                "state-get",
                move |_store: wasmtime::StoreContextMut<'_, HostState>,
                      (key,): (String,)|
                      -> HostFuture<'_, Result<Option<String>, String>> {
                    let backend = Arc::clone(&backend_for_state_get);
                    Box::new(async move { Ok((backend.state_get(&key),)) })
                },
            )
            .map_err(|e| CodemodeError::Link(e.to_string()))?;

        let backend_for_state_set = Arc::clone(&backend);
        engine
            .linker_mut()
            .instance("tower-agent:codemode/host@0.1.0")
            .map_err(|e| CodemodeError::Link(e.to_string()))?
            .func_wrap_async(
                "state-set",
                move |_store: wasmtime::StoreContextMut<'_, HostState>,
                      (key, value_json): (String, String)|
                      -> HostFuture<'_, Result<bool, String>> {
                    let backend = Arc::clone(&backend_for_state_set);
                    Box::new(async move { Ok((backend.state_set(&key, &value_json),)) })
                },
            )
            .map_err(|e| CodemodeError::Link(e.to_string()))?;

        let backend_for_log = Arc::clone(&backend);
        engine
            .linker_mut()
            .instance("tower-agent:codemode/host@0.1.0")
            .map_err(|e| CodemodeError::Link(e.to_string()))?
            .func_wrap_async(
                "log",
                move |_store: wasmtime::StoreContextMut<'_, HostState>,
                      (level, message): (String, String)|
                      -> HostFuture<'_, Result<bool, String>> {
                    let backend = Arc::clone(&backend_for_log);
                    Box::new(async move { Ok((backend.log(&level, &message),)) })
                },
            )
            .map_err(|e| CodemodeError::Link(e.to_string()))?;

        let backend_for_ui_status = Arc::clone(&backend);
        engine
            .linker_mut()
            .instance("tower-agent:codemode/host@0.1.0")
            .map_err(|e| CodemodeError::Link(e.to_string()))?
            .func_wrap_async(
                "ui-status",
                move |_store: wasmtime::StoreContextMut<'_, HostState>,
                      (segment_json,): (String,)|
                      -> HostFuture<'_, Result<bool, String>> {
                    let backend = Arc::clone(&backend_for_ui_status);
                    Box::new(async move { Ok((backend.ui_status(&segment_json),)) })
                },
            )
            .map_err(|e| CodemodeError::Link(e.to_string()))?;

        let backend_for_ui_panel = Arc::clone(&backend);
        engine
            .linker_mut()
            .instance("tower-agent:codemode/host@0.1.0")
            .map_err(|e| CodemodeError::Link(e.to_string()))?
            .func_wrap_async(
                "ui-panel",
                move |_store: wasmtime::StoreContextMut<'_, HostState>,
                      (panel_json,): (String,)|
                      -> HostFuture<'_, Result<bool, String>> {
                    let backend = Arc::clone(&backend_for_ui_panel);
                    Box::new(async move { Ok((backend.ui_panel(&panel_json),)) })
                },
            )
            .map_err(|e| CodemodeError::Link(e.to_string()))?;

        let backend_for_ui_clear = Arc::clone(&backend);
        engine
            .linker_mut()
            .instance("tower-agent:codemode/host@0.1.0")
            .map_err(|e| CodemodeError::Link(e.to_string()))?
            .func_wrap_async(
                "ui-clear",
                move |_store: wasmtime::StoreContextMut<'_, HostState>,
                      (id,): (Option<String>,)|
                      -> HostFuture<'_, Result<bool, String>> {
                    let backend = Arc::clone(&backend_for_ui_clear);
                    Box::new(async move { Ok((backend.ui_clear(id.as_deref()),)) })
                },
            )
            .map_err(|e| CodemodeError::Link(e.to_string()))?;

        let backend_for_ui_tool_render = Arc::clone(&backend);
        engine
            .linker_mut()
            .instance("tower-agent:codemode/host@0.1.0")
            .map_err(|e| CodemodeError::Link(e.to_string()))?
            .func_wrap_async(
                "ui-tool-render",
                move |_store: wasmtime::StoreContextMut<'_, HostState>,
                      (tool, lines_json): (String, String)|
                      -> HostFuture<'_, Result<bool, String>> {
                    let backend = Arc::clone(&backend_for_ui_tool_render);
                    Box::new(async move { Ok((backend.ui_tool_render(&tool, &lines_json),)) })
                },
            )
            .map_err(|e| CodemodeError::Link(e.to_string()))?;

        let host = Arc::new(Self {
            engine: Arc::new(engine),
            backend: Arc::clone(&backend),
            root: options.session_root,
            mounts: options.mounts,
        });
        *backend.this.lock().expect("codemode host weak poisoned") = Arc::downgrade(&host);
        Ok(host)
    }

    /// 会话根目录。
    #[must_use]
    pub fn session_root(&self) -> &Path {
        &self.root
    }

    /// 命令层后端（想直接跑命令、或做冒烟测试时用）。
    #[must_use]
    pub fn exec(&self) -> &ExecService {
        &self.backend.exec
    }

    /// 宿主工具表（脚本注册的工具也会写回这里）。
    #[must_use]
    pub fn host_api(&self) -> &HostApi {
        &self.backend.host
    }

    /// 共享 UI 状态（脚本 UI 贡献写在这里）。
    #[must_use]
    pub fn ui_state(&self) -> UiState {
        self.backend.ui.clone()
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
        let previous_source = self
            .backend
            .current_source
            .lock()
            .expect("codemode source poisoned")
            .replace(code.to_owned());
        let result = async {
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
                fs: self.backend.permissions.fs,
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
        .await;
        *self
            .backend
            .current_source
            .lock()
            .expect("codemode source poisoned") = previous_source;
        result
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

impl HostBackend {
    async fn tool_call(&self, name: &str, args_json: &str) -> Result<String, String> {
        self.permissions
            .authorize(Capability::Tools)
            .map_err(|error| capability_error_text(&error))?;
        let args: Value = serde_json::from_str(args_json)
            .map_err(|error| format!("invalid tool args: {error}"))?;
        let arguments = match args {
            Value::Object(object) => object,
            Value::Null => serde_json::Map::new(),
            other => return Err(format!("tool args must be an object, got {other}")),
        };
        let request = ToolCallRequest::new(ToolCall {
            id: format!("codemode:{name}"),
            name: name.to_owned(),
            arguments: Arguments::from(arguments),
            thought_signature: None,
            namespace: None,
        });
        let mut stream = self
            .host
            .call_tool(name, request)
            .await
            .map_err(|error| error.to_string())?;
        let mut result = None;
        while let Some(item) = stream.next().await {
            result = Some(item);
        }
        let result = result.ok_or_else(|| format!("tool `{name}` produced no result"))?;
        if result.is_error {
            return Err(result_text(&result));
        }
        serde_json::to_string(&Value::String(result_text(&result)))
            .map_err(|error| format!("serialize tool result: {error}"))
    }

    fn tool_list(&self) -> Result<Vec<WireToolDescriptor>, String> {
        self.permissions
            .authorize(Capability::Tools)
            .map_err(|error| capability_error_text(&error))?;
        let mut names = self.host.capabilities();
        names.sort();
        Ok(names
            .into_iter()
            .map(|name| {
                let tool = self.host.tool(&name);
                let description = tool
                    .as_ref()
                    .map(|tool| tool.tool_description().to_owned())
                    .unwrap_or_default();
                let parameters = tool
                    .as_ref()
                    .and_then(|tool| serde_json::to_string(tool.tool_parameters()).ok())
                    .unwrap_or_else(|| "{\"type\":\"object\"}".to_owned());
                let output_schema = tool
                    .as_ref()
                    .and_then(|tool| tool.declared_output_schema().cloned())
                    .and_then(|schema| serde_json::to_string(&schema).ok());
                WireToolDescriptor {
                    name,
                    description,
                    parameters,
                    output_schema,
                }
            })
            .collect())
    }

    fn tool_register(&self, registration: WireToolRegistration) -> Result<bool, String> {
        self.permissions
            .authorize(Capability::RegisterTools)
            .map_err(|error| capability_error_text(&error))?;
        if registration.name.trim().is_empty() {
            return Err("tool_register requires a non-empty tool name".to_owned());
        }
        if !is_js_identifier(&registration.handler) {
            return Err(format!(
                "tool handler `{}` must be a JavaScript identifier",
                registration.handler
            ));
        }
        let parameters: Value = serde_json::from_str(&registration.parameters)
            .map_err(|error| format!("invalid parameter schema: {error}"))?;
        if !parameters.is_object() {
            return Err("tool parameter schema must be a JSON object".to_owned());
        }
        let output_schema = registration
            .output_schema
            .map(|schema| {
                serde_json::from_str::<Value>(&schema)
                    .map_err(|error| format!("invalid output schema: {error}"))
            })
            .transpose()?;
        let source = self
            .current_source
            .lock()
            .expect("codemode source poisoned")
            .clone()
            .ok_or_else(|| "tool_register must run inside a codemode script".to_owned())?;

        let mut registered = self
            .script_tools
            .lock()
            .expect("codemode script tools poisoned");
        if !registered.contains(&registration.name) && self.host.tool(&registration.name).is_some()
        {
            return Err(format!(
                "tool `{}` already exists and is not owned by a script",
                registration.name
            ));
        }
        let script_tool = ScriptTool {
            host: self
                .this
                .lock()
                .expect("codemode host weak poisoned")
                .clone(),
            source,
            function: registration.handler,
        };
        let mut tool = AgentTool::new(registration.name.clone(), script_tool)
            .label(registration.name.clone())
            .description(registration.description)
            .parameters(parameters);
        if let Some(schema) = output_schema {
            tool = tool.output_schema(schema);
        }
        self.host.register_tool(tool);
        registered.insert(registration.name);
        Ok(true)
    }

    fn tool_unregister(&self, name: &str) -> Result<bool, String> {
        self.permissions
            .authorize(Capability::RegisterTools)
            .map_err(|error| capability_error_text(&error))?;
        let owned = self
            .script_tools
            .lock()
            .expect("codemode script tools poisoned")
            .remove(name);
        if !owned {
            return Ok(false);
        }
        self.host.remove_tool(name);
        Ok(true)
    }

    fn state_get(&self, key: &str) -> Result<Option<String>, String> {
        self.permissions
            .authorize(Capability::State)
            .map_err(|error| capability_error_text(&error))?;
        self.state
            .lock()
            .expect("codemode state poisoned")
            .get(key)
            .map(|value| serde_json::to_string(value).map_err(|error| error.to_string()))
            .transpose()
    }

    fn state_set(&self, key: &str, value_json: &str) -> Result<bool, String> {
        self.permissions
            .authorize(Capability::State)
            .map_err(|error| capability_error_text(&error))?;
        let value = serde_json::from_str(value_json)
            .map_err(|error| format!("invalid state value: {error}"))?;
        self.state
            .lock()
            .expect("codemode state poisoned")
            .insert(key.to_owned(), value);
        Ok(true)
    }

    fn log(&self, level: &str, message: &str) -> Result<bool, String> {
        self.permissions
            .authorize(Capability::State)
            .map_err(|error| capability_error_text(&error))?;
        eprintln!("[codemode {level}] {message}");
        Ok(true)
    }

    fn ui_status(&self, segment_json: &str) -> Result<bool, String> {
        self.permissions
            .authorize(Capability::Ui)
            .map_err(|error| capability_error_text(&error))?;
        let value: Value = serde_json::from_str(segment_json)
            .map_err(|error| format!("invalid UI status: {error}"))?;
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or("ui_status requires id")?;
        let text = value
            .get("text")
            .and_then(Value::as_str)
            .ok_or("ui_status requires text")?;
        self.ui.set_status(StatusSegment {
            id: id.to_owned(),
            text: text.to_owned(),
            color: value
                .get("color")
                .and_then(Value::as_str)
                .map(str::to_owned),
            priority: value
                .get("priority")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
        });
        Ok(true)
    }

    fn ui_panel(&self, panel_json: &str) -> Result<bool, String> {
        self.permissions
            .authorize(Capability::Ui)
            .map_err(|error| capability_error_text(&error))?;
        let value: Value = serde_json::from_str(panel_json)
            .map_err(|error| format!("invalid UI panel: {error}"))?;
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or("ui_panel requires id")?;
        let title = value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let lines = value
            .get("lines")
            .map(json_string_lines)
            .transpose()?
            .unwrap_or_default();
        let mut panel = Panel::new(id, title, lines);
        if let Some(anchor) = value.get("anchor").and_then(Value::as_str) {
            panel.anchor = PanelAnchor::parse(anchor);
        }
        panel.width = value
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok());
        panel.height = value
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok());
        self.ui.set_panel(panel);
        Ok(true)
    }

    fn ui_clear(&self, id: Option<&str>) -> Result<bool, String> {
        self.permissions
            .authorize(Capability::Ui)
            .map_err(|error| capability_error_text(&error))?;
        match id {
            Some(id) => {
                self.ui.remove_panel(id);
                self.ui.remove_status(id);
            }
            None => self.ui.clear(),
        }
        Ok(true)
    }

    fn ui_tool_render(&self, tool: &str, lines_json: &str) -> Result<bool, String> {
        self.permissions
            .authorize(Capability::Ui)
            .map_err(|error| capability_error_text(&error))?;
        let value: Value = serde_json::from_str(lines_json)
            .map_err(|error| format!("invalid UI renderer: {error}"))?;
        self.ui
            .set_tool_renderer(tool, ToolRenderer::new(json_string_lines(&value)?));
        Ok(true)
    }
}

#[derive(Clone)]
struct ScriptTool {
    host: Weak<CodemodeHost>,
    source: String,
    function: String,
}

impl Service<ToolCallRequest> for ScriptTool {
    type Response = ToolResultStream;
    type Error = AgentError;
    type Future = BoxFuture<'static, Result<ToolResultStream, AgentError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ToolCallRequest) -> Self::Future {
        let host = self.host.clone();
        let source = self.source.clone();
        let function = self.function.clone();
        Box::pin(async move {
            let Some(host) = host.upgrade() else {
                return Ok(ToolResultStream::once(AgentToolResult::error(
                    "codemode host has been dropped",
                )));
            };
            let args = Value::Object(request.tool_call.arguments.as_object().clone());
            let result = host.call_registered_tool(&source, &function, args).await;
            Ok(ToolResultStream::once(match result {
                Ok(value) => AgentToolResult::text(value_to_text(&value)),
                Err(error) => AgentToolResult::error(error),
            }))
        })
    }
}

impl CodemodeHost {
    async fn call_registered_tool(
        &self,
        source: &str,
        function: &str,
        args: Value,
    ) -> Result<Value, String> {
        let args = serde_json::to_string(&args).map_err(|error| error.to_string())?;
        let code = format!("{source}\n\nreturn await {function}({args});");
        let result = self
            .run_async(&code, None)
            .await
            .map_err(|error| error.to_string())?;
        let value = result.value.unwrap_or_else(|| "null".to_owned());
        Ok(serde_json::from_str(&value).unwrap_or(Value::String(value)))
    }
}

fn capability_error_text(error: &CapabilityError) -> String {
    serde_json::to_string(error).unwrap_or_else(|_| error.to_string())
}

fn is_js_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || matches!(first, '_' | '$'))
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$'))
}

fn json_string_lines(value: &Value) -> Result<Vec<String>, String> {
    let lines = value
        .as_array()
        .ok_or_else(|| "UI lines must be an array".to_owned())?;
    lines
        .iter()
        .map(|line| {
            line.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "UI lines must be strings".to_owned())
        })
        .collect()
}

fn result_text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
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
