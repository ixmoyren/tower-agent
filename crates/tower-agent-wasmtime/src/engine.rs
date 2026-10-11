//! wasmtime 引擎与组件：编译 + WASI + 内存上限 + JIT 缓存。

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use wasmtime::{
    Config, Engine, Store,
    component::{Component, Linker},
};

use crate::HostState;

/// 默认 wasm 线性内存上限（256 MiB）。
pub const DEFAULT_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

/// epoch 中断的滴答间隔：引擎每这么长时间把 epoch +1 一次。
///
/// 单次调用的超时精度就是这个量级（够用，而且不用给每个 store 起定时器）。
const EPOCH_TICK: Duration = Duration::from_millis(100);

/// 沙箱引擎配置。
#[derive(Debug, Clone)]
pub struct SandboxOptions {
    /// 每个实例的 wasm 线性内存上限（字节）。
    pub memory_limit: usize,
    /// wasm 影子栈大小（字节）。QuickJS 那类「自己算栈上限」的 guest 需要它留足余量，
    /// 否则 guest 内的栈检查还没触发，wasm 栈就先耗尽了（表现是硬 trap 而不是可捕获异常）。
    pub max_wasm_stack: usize,
    /// 单次调用（一次 `Store` 上的调用）的墙钟上限；`None` = 不限。
    ///
    /// 到点用 wasmtime 的 epoch 中断把 guest trap 掉：**被中断的调用拿不到结果**，
    /// 调用方要当成「这次调用失败」并丢弃那个 `Store`。
    pub epoch_timeout: Option<Duration>,
    /// 用异步链路（`Config::async_support`）。
    ///
    /// 开了之后宿主 import 可以 `await`，于是**能被 `tokio::time::timeout` 取消**——
    /// epoch 只能打断「正在执行的 wasm 指令」，打不断卡在宿主调用里的 guest（`sleep`、
    /// 阻塞的读写），那类必须靠异步取消。代价：这个引擎上的调用必须走 `*_async` API。
    pub async_support: bool,
    /// JIT 缓存目录；`None` 表示用默认位置（`dirs::cache_dir()/tower-agent/wasmtime`）。
    pub cache_dir: Option<PathBuf>,
}

impl Default for SandboxOptions {
    fn default() -> Self {
        Self {
            memory_limit: DEFAULT_MEMORY_LIMIT,
            max_wasm_stack: DEFAULT_MAX_WASM_STACK,
            epoch_timeout: None,
            async_support: false,
            cache_dir: None,
        }
    }
}

/// wasmtime 自身的默认影子栈大小（512 KiB）。
pub const DEFAULT_MAX_WASM_STACK: usize = 512 * 1024;

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
    /// 默认单次调用超时（`None` = 不限）。
    default_timeout: Option<Duration>,
    /// epoch 滴答线程的存活开关（引擎 drop 时置 false，线程下一拍退出）。
    epoch_ticker: Option<Arc<AtomicBool>>,
}

impl SandboxEngine {
    /// 用 wasm 字节与选项构造：建引擎（含 JIT 缓存）、编译组件、装好 WASI linker。
    pub fn new(wasm: &[u8], options: SandboxOptions) -> Result<Self, SandboxError> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.max_wasm_stack(options.max_wasm_stack);
        if options.epoch_timeout.is_some() {
            config.epoch_interruption(true);
        }
        configure_cache(&mut config, options.cache_dir);
        let engine =
            Engine::new(&config).map_err(|error| SandboxError::Engine(error.to_string()))?;

        // 滴答线程只做 `increment_epoch`；截止点由每次调用自己设（`arm_deadline`）。
        let epoch_ticker = options.epoch_timeout.map(|_| {
            let alive = Arc::new(AtomicBool::new(true));
            let flag = Arc::clone(&alive);
            let ticker_engine = engine.clone();
            let _ = std::thread::Builder::new()
                .name("sandbox-epoch".to_owned())
                .spawn(move || {
                    while flag.load(Ordering::Relaxed) {
                        std::thread::sleep(EPOCH_TICK);
                        ticker_engine.increment_epoch();
                    }
                });
            alive
        });
        let component = Component::new(&engine, wasm)
            .map_err(|error| SandboxError::Compile(error.to_string()))?;
        let mut linker = Linker::new(&engine);
        let linked = if options.async_support {
            wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        } else {
            wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        };
        linked.map_err(|error| SandboxError::Linker(error.to_string()))?;
        Ok(Self {
            engine,
            component,
            linker,
            memory_limit: options.memory_limit,
            default_timeout: options.epoch_timeout,
            epoch_ticker,
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

    /// 可变 linker：调用方在共享引擎之前，往它上面加自己的 host import
    /// （例如 codemode 的 `tower-agent:codemode/host.exec`）。
    ///
    /// 只在构造期用；一旦包进 `Arc` 给多个实例共享，就不应该再改。
    pub fn linker_mut(&mut self) -> &mut Linker<HostState> {
        &mut self.linker
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
        self.arm_deadline(&mut store, None);
        store
    }

    /// 给一次调用设超时：`None` 用 `SandboxOptions::epoch_timeout`，`Some` 覆盖它。
    ///
    /// 引擎没开 epoch 中断时是空操作（deadline 设成 `u64::MAX`）。
    pub fn arm_deadline(&self, store: &mut Store<HostState>, timeout: Option<Duration>) {
        let deadline = timeout.or(self.default_timeout);
        let ticks = deadline.map_or(u64::MAX, |timeout| {
            (timeout.as_millis() as u64)
                .div_ceil(EPOCH_TICK.as_millis() as u64)
                .max(1)
        });
        store.set_epoch_deadline(ticks);
    }
}

impl Drop for SandboxEngine {
    fn drop(&mut self) {
        if let Some(alive) = &self.epoch_ticker {
            alive.store(false, Ordering::Relaxed);
        }
    }
}

/// 开启 wasmtime 的 JIT 缓存：产物写到平台缓存目录，缓存键含引擎配置与 wasmtime 版本
/// （升级 wasmtime 后自动失效）。缓存不可用时静默退回「每次都编译」。
fn configure_cache(config: &mut Config, cache_dir: Option<PathBuf>) {
    let directory =
        cache_dir.or_else(|| dirs::cache_dir().map(|dir| dir.join("tower-agent").join("wasmtime")));
    let Some(directory) = directory else {
        warn_no_cache("拿不到平台缓存目录");
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&directory) {
        warn_no_cache(&format!("建目录 {} 失败：{error}", directory.display()));
        return;
    }
    let mut cache_config = wasmtime::CacheConfig::new();
    cache_config.with_directory(directory);
    if let Ok(cache) = wasmtime::Cache::new(cache_config) {
        config.cache(Some(cache));
    } else {
        warn_no_cache("wasmtime 初始化缓存失败");
    }
}

/// 缓存不可用是**静默降级**：每次都重新编译（coreutils 7.4 MB，实测冷启动 ~10 s）。
/// 打一次警告，让人能察觉；要消掉就给 `SandboxOptions::cache_dir` 指个可写目录，
/// 或设 `XDG_CACHE_HOME`（`cargo test` 在受限环境里就踩过这个坑，见 docs §9.2）。
fn warn_no_cache(reason: &str) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        eprintln!(
            "tower-agent-wasmtime: JIT 缓存不可用（{reason}），组件每次都会重新编译（冷启动可达数秒到十几秒）"
        );
    });
}
