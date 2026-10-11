//! `tower-agent-wasmtime` 的**静态插件**占位。
//!
//! 这个 crate 本身是基础设施（引擎 / WASI / 上限 / JIT 缓存 / 池），没有自己的工具；
//! 它作为插件存在的意义是**把名字与能力占住并标成 protected**：动态（dylib）与 JIT
//! （monty mods）插件不能自称 `wasmtime` 或提供 `sandbox.wasmtime` 来顶替沙箱能力。
//!
//! 与上层的静态插件（如 `CodemodePlugin`）一样，它将来会被 cargo feature 门控，
//! 以便「tower-agent 提供哪些能力」在编译期定制。

use semver::Version;
use tower_agent_plugins::{
    CapabilityId, PluginContext, PluginError, PluginKind, PluginManifest, StaticPlugin,
};

/// 插件名（受保护，不可被遮蔽）。
pub const PLUGIN_NAME: &str = "wasmtime";

/// 沙箱能力标识（`sandbox.wasmtime`）。
pub const CAPABILITY: &str = "wasmtime";

/// wasmtime 沙箱静态插件（无工具载荷，纯占名 + 占能力）。
#[derive(Debug, Default, Clone, Copy)]
pub struct WasmtimePlugin;

impl WasmtimePlugin {
    /// 构造。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl StaticPlugin for WasmtimePlugin {
    fn manifest(&self) -> PluginManifest {
        let version = Version::new(1, 0, 0);
        PluginManifest::new(PLUGIN_NAME, version.clone(), PluginKind::Static)
            .providing(CapabilityId::new("sandbox", CAPABILITY), version)
            .protected()
    }

    fn install(&self, _context: &mut PluginContext) -> Result<(), PluginError> {
        Ok(())
    }
}
