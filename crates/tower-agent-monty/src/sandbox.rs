//! monty 的沙箱包装：通用实例（`tower-agent-wasmtime`）+ monty 自己的组件绑定与协议。
//!
//! 引擎、WASI、内存上限、JIT 缓存、通用实例与实例池都在 `tower-agent-wasmtime`；这里只有
//! monty 专属的两样东西：
//!
//! - 内嵌组件字节 [`MONTY_SANDBOX`] 与 `bindgen!` 生成的绑定 `MontySandbox`；
//! - 把 `progress` 状态机包装成 [`Sandbox`]（`start` / `resume_*`）。

use std::sync::Arc;

use tower_agent_plugins::PluginError;
use tower_agent_wasmtime::{
    HostState, Sandbox as GenericSandbox, SandboxEngine, SandboxError, SandboxOptions,
};
use wasmtime::Store;

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

/// monty 沙箱实例：通用实例之上暴露 monty 的协议方法。
pub struct Sandbox(GenericSandbox<MontySandbox>);

impl Sandbox {
    /// 用共享引擎实例化出 monty 组件实例。
    pub fn new(engine: &SandboxEngine) -> Result<Self, SandboxError> {
        Ok(Self(GenericSandbox::instantiate(engine, |store| {
            MontySandbox::instantiate(store, engine.component(), engine.linker())
        })?))
    }

    /// 在「可变 `Store` + monty 实例」上跑一次协议调用，并把错误映射成插件错误。
    fn drive(
        &mut self,
        call: impl FnOnce(&mut Store<HostState>, &MontySandbox) -> Result<Progress, wasmtime::Error>,
    ) -> Result<Progress, PluginError> {
        self.0.with(call).map_err(to_plugin_error)
    }

    /// 喂入源码，返回首个挂起点/完成。
    pub fn start(&mut self, source: &str) -> Result<Progress, PluginError> {
        self.drive(|store, bindings| {
            bindings
                .tower_agent_monty_sandbox()
                .call_start(store, source)
        })
    }

    /// 用 JSON 结果恢复一次 function-call。
    pub fn resume_function(
        &mut self,
        call_id: u32,
        result_json: &str,
    ) -> Result<Progress, PluginError> {
        self.drive(|store, bindings| {
            bindings
                .tower_agent_monty_sandbox()
                .call_resume_function(store, call_id, result_json)
        })
    }

    /// 让一次 function-call 抛 `RuntimeError`。
    pub fn reject_function(
        &mut self,
        call_id: u32,
        message: &str,
    ) -> Result<Progress, PluginError> {
        self.drive(|store, bindings| {
            bindings
                .tower_agent_monty_sandbox()
                .call_reject_function(store, call_id, message)
        })
    }

    /// 未定义名字 -> `NameError`。
    pub fn resume_name(&mut self, name: &str) -> Result<Progress, PluginError> {
        self.drive(|store, bindings| {
            bindings
                .tower_agent_monty_sandbox()
                .call_resume_name(store, name)
        })
    }

    /// 用宿主给的 `result-json` 恢复一次 os-call。
    pub fn resume_os(&mut self, call_id: u32, result_json: &str) -> Result<Progress, PluginError> {
        self.drive(|store, bindings| {
            bindings
                .tower_agent_monty_sandbox()
                .call_resume_os(store, call_id, result_json)
        })
    }
}

/// monty 沙箱实例池（引擎 = `SandboxEngine`，实例 = `Sandbox`）。
pub type SandboxPool = tower_agent_wasmtime::SandboxPool<SandboxEngine, Sandbox>;

/// 建一个 monty 沙箱池：惰性编译内嵌组件，最多保留 `max_idle` 个空闲实例。
#[must_use]
pub fn new_pool(max_idle: usize) -> Arc<SandboxPool> {
    Arc::new(SandboxPool::new(
        max_idle,
        || SandboxEngine::new(MONTY_SANDBOX, SandboxOptions::default()),
        Sandbox::new,
    ))
}

/// 把沙箱/引擎错误映射成插件错误（`wasmtime::Error` 与 [`SandboxError`] 都实现了 `Display`）。
pub(crate) fn to_plugin_error(error: impl std::fmt::Display) -> PluginError {
    PluginError::Load {
        plugin: "monty-sandbox".to_string(),
        message: error.to_string(),
    }
}
