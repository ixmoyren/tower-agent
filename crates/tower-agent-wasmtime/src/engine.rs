//! wasmtime 引擎与组件：编译 + WASI + 内存上限 + JIT 缓存。

use std::path::PathBuf;

use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

use crate::HostState;

/// 默认 wasm 线性内存上限（256 MiB）。
pub const DEFAULT_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

/// 沙箱引擎配置。
#[derive(Debug, Clone)]
pub struct SandboxOptions {
    /// 每个实例的 wasm 线性内存上限（字节）。
    pub memory_limit: usize,
    /// JIT 缓存目录；`None` 表示用默认位置（`dirs::cache_dir()/tower-agent/wasmtime`）。
    pub cache_dir: Option<PathBuf>,
}

impl Default for SandboxOptions {
    fn default() -> Self {
        Self {
            memory_limit: DEFAULT_MEMORY_LIMIT,
            cache_dir: None,
        }
    }
}

/// wasm 沙箱错误。
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// 配置/创建 wasmtime 引擎失败。
    #[error("failed to configure the wasmtime engine: {0}")]
    Engine(String),
    /// 编译 wasm 组件失败。
    #[error("failed to compile the wasm component: {0}")]
    Compile(String),
    /// 配置 WASI linker 失败。
    #[error("failed to set up the WASI linker: {0}")]
    Linker(String),
    /// 实例化/使用沙箱失败。
    #[error("failed to use the wasm sandbox: {0}")]
    Instance(String),
    /// 池的互斥锁被污染。
    #[error("sandbox pool poisoned")]
    Poisoned,
}

/// 已编译的组件 + 引擎 + WASI linker：进程内共享一份。
pub struct SandboxEngine {
    engine: Engine,
    component: Component,
    linker: Linker<HostState>,
    memory_limit: usize,
}

impl SandboxEngine {
    /// 用 wasm 字节与选项构造：建引擎（含 JIT 缓存）、编译组件、装好 WASI linker。
    pub fn new(wasm: &[u8], options: SandboxOptions) -> Result<Self, SandboxError> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        configure_cache(&mut config, options.cache_dir);
        let engine = Engine::new(&config).map_err(|error| SandboxError::Engine(error.to_string()))?;
        let component =
            Component::new(&engine, wasm).map_err(|error| SandboxError::Compile(error.to_string()))?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|error| SandboxError::Linker(error.to_string()))?;
        Ok(Self {
            engine,
            component,
            linker,
            memory_limit: options.memory_limit,
        })
    }

    /// 底层 wasmtime 引擎（调用方实例化组件时用）。
    #[must_use]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// 已编译的组件。
    #[must_use]
    pub fn component(&self) -> &Component {
        &self.component
    }

    /// 装好 WASI 的 linker。
    #[must_use]
    pub fn linker(&self) -> &Linker<HostState> {
        &self.linker
    }

    /// 配置的内存上限。
    #[must_use]
    pub fn memory_limit(&self) -> usize {
        self.memory_limit
    }

    /// 新建一个 `Store`（已应用内存上限），供调用方实例化自己的组件。
    #[must_use]
    pub fn new_store(&self) -> Store<HostState> {
        let mut store = Store::new(&self.engine, HostState::new(self.memory_limit));
        store.limiter(|state| &mut state.limits);
        store
    }
}

/// 开启 wasmtime 的 JIT 缓存：产物写到平台缓存目录，缓存键含引擎配置与 wasmtime 版本
/// （升级 wasmtime 后自动失效）。缓存不可用时静默退回「每次都编译」。
fn configure_cache(config: &mut Config, cache_dir: Option<PathBuf>) {
    let directory = cache_dir.or_else(|| {
        dirs::cache_dir().map(|dir| dir.join("tower-agent").join("wasmtime"))
    });
    let Some(directory) = directory else {
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
