//! wasmtime 沙箱：加载内嵌的 monty 组件并驱动它。
//!
//! 组件（`wasm/monty-sandbox`，经 wasm-opt 优化后内嵌）把 monty 解释器跑在 wasm
//! 线性内存里；宿主只通过 WIT 接口驱动它的 `feed`/`resume` 状态机。mods 代码的任何
//! 越界/栈溢出/内存超限都会被 wasmtime 转成 trap 错误，不会带走宿主进程。
//!
//! 每个 mod 一个 `Sandbox`（独立 `Store` + 组件实例），互不共享状态。

use std::sync::{Arc, Mutex, OnceLock};

use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Linker},
};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use tower_agent_plugins::PluginError;

wasmtime::component::bindgen!({
    path: "../../wasm/monty-sandbox/wit",
    world: "monty-sandbox",
});

pub use self::exports::tower_agent::monty::sandbox::Progress;

/// 内嵌的沙箱组件（由 `wasm/monty-sandbox/generate.sh` 生成）。
const MONTY_SANDBOX: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/monty-sandbox.wasm"
));

/// WASI 上下文：不挂载任何 preopen，因此沙箱内没有文件系统访问。
struct HostState {
    ctx: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
}

/// 单个沙箱实例的 wasm 线性内存上限（256 MiB）。
const SANDBOX_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// 共享的引擎与已编译组件：一个进程一份（JIT 结果由 wasmtime 缓存）。
pub struct SandboxEngine {
    engine: Engine,
    component: Component,
    linker: Linker<HostState>,
}

impl SandboxEngine {
    /// 建引擎（开启 JIT 缓存）并编译内嵌组件。
    pub fn new() -> Result<Self, PluginError> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        configure_cache(&mut config);
        let engine = Engine::new(&config).map_err(sandbox_error)?;
        let component = Component::new(&engine, MONTY_SANDBOX).map_err(sandbox_error)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(sandbox_error)?;
        Ok(Self {
            engine,
            component,
            linker,
        })
    }

    /// 新建一个独立沙箱实例（一个 mod 一个）。
    pub fn checkout(&self) -> Result<Sandbox, PluginError> {
        let state = HostState {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(SANDBOX_MEMORY_LIMIT)
                .build(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        let bindings = MontySandbox::instantiate(&mut store, &self.component, &self.linker)
            .map_err(sandbox_error)?;
        Ok(Sandbox { store, bindings })
    }
}

/// 一个 mod 的沙箱实例。
pub struct Sandbox {
    store: Store<HostState>,
    bindings: MontySandbox,
}

impl Sandbox {
    /// 喂入源码，返回首个挂起点/完成。
    pub fn start(&mut self, source: &str) -> Result<Progress, PluginError> {
        self.bindings
            .tower_agent_monty_sandbox()
            .call_start(&mut self.store, source)
            .map_err(sandbox_error)
    }

    /// 用 JSON 结果恢复一次 function-call。
    pub fn resume_function(
        &mut self,
        call_id: u32,
        result_json: &str,
    ) -> Result<Progress, PluginError> {
        self.bindings
            .tower_agent_monty_sandbox()
            .call_resume_function(&mut self.store, call_id, result_json)
            .map_err(sandbox_error)
    }

    /// 让一次 function-call 抛 `RuntimeError`。
    pub fn reject_function(
        &mut self,
        call_id: u32,
        message: &str,
    ) -> Result<Progress, PluginError> {
        self.bindings
            .tower_agent_monty_sandbox()
            .call_reject_function(&mut self.store, call_id, message)
            .map_err(sandbox_error)
    }

    /// 未定义名字 -> `NameError`。
    pub fn resume_name(&mut self, name: &str) -> Result<Progress, PluginError> {
        self.bindings
            .tower_agent_monty_sandbox()
            .call_resume_name(&mut self.store, name)
            .map_err(sandbox_error)
    }

    /// 用宿主给的 `result-json` 恢复一次 os-call。
    pub fn resume_os(&mut self, call_id: u32, result_json: &str) -> Result<Progress, PluginError> {
        self.bindings
            .tower_agent_monty_sandbox()
            .call_resume_os(&mut self.store, call_id, result_json)
            .map_err(sandbox_error)
    }
}

/// 沙箱实例池。
///
/// 组件只编译一次（JIT 缓存）；实例（`Store` + 组件实例）可以复用，因为每次运行都
/// 以 `start()` 开头会重置组件内的挂起状态。池用于摊薄实例创建成本，并支持预热。
pub struct SandboxPool {
    engine: OnceLock<Arc<SandboxEngine>>,
    idle: Mutex<Vec<Sandbox>>,
    max_idle: usize,
}

impl SandboxPool {
    /// 建池，最多保留 `max_idle` 个空闲实例。
    #[must_use]
    pub fn new(max_idle: usize) -> Self {
        Self {
            engine: OnceLock::new(),
            idle: Mutex::new(Vec::new()),
            max_idle,
        }
    }

    /// 引擎（首次调用时编译内嵌组件）。
    pub fn engine(&self) -> Result<Arc<SandboxEngine>, PluginError> {
        if let Some(engine) = self.engine.get() {
            return Ok(Arc::clone(engine));
        }
        let engine = Arc::new(SandboxEngine::new()?);
        let _ = self.engine.set(Arc::clone(&engine));
        Ok(self.engine.get().map(Arc::clone).unwrap_or(engine))
    }

    /// 预热：编译组件并预先创建 `instances` 个实例放进池子。
    pub fn warmup(&self, instances: usize) -> Result<(), PluginError> {
        let engine = self.engine()?;
        let mut idle = self.idle.lock().expect("sandbox pool poisoned");
        while idle.len() < instances.min(self.max_idle) {
            idle.push(engine.checkout()?);
        }
        Ok(())
    }

    /// 取一个实例（池空则新建）。
    pub fn acquire(&self) -> Result<Sandbox, PluginError> {
        if let Some(sandbox) = self.idle.lock().expect("sandbox pool poisoned").pop() {
            return Ok(sandbox);
        }
        self.engine()?.checkout()
    }

    /// 归还一个实例（超过上限则丢弃）。
    pub fn release(&self, sandbox: Sandbox) {
        let mut idle = self.idle.lock().expect("sandbox pool poisoned");
        if idle.len() < self.max_idle {
            idle.push(sandbox);
        }
    }

    /// 当前空闲实例数（用于测试/诊断）。
    #[must_use]
    pub fn idle_count(&self) -> usize {
        self.idle.lock().map(|idle| idle.len()).unwrap_or(0)
    }
}

fn sandbox_error(error: impl std::fmt::Display) -> PluginError {
    PluginError::Load {
        plugin: "monty-sandbox".to_string(),
        message: error.to_string(),
    }
}

/// 开启 wasmtime 的 JIT 缓存。
///
/// 编译产物写到平台缓存目录，缓存键包含引擎配置与 wasmtime 版本，升级 wasmtime
/// 后条目自动失效，无需手工清理。缓存不可用时静默退回「每次都编译」。
fn configure_cache(config: &mut Config) {
    let Some(directory) = dirs::cache_dir().map(|dir| dir.join("tower-agent").join("wasmtime"))
    else {
        return;
    };
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let mut cache_config = wasmtime::CacheConfig::new();
    cache_config.with_directory(directory);
    if let Ok(cache) = wasmtime::Cache::new(cache_config) {
        config.cache(Some(cache));
    }
}
