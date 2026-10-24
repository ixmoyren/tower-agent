//! 插件作者 SDK（`docs/plugins.md` 第 8.2 节）。
//!
//! - 静态插件用 [`declare_plugin!`] / [`DeclaredPlugin`] 声明清单与安装逻辑；
//! - 插件回调宿主能力时用 [`HostApi`] / [`PluginAccess`]（跨形态调用不直连，见第 8 节）；
//! - [`ToolRegistration`] 是 `tower_agent_runtime::ToolRegistration` 的再导出，供插件把 `AgentTool`
//!   暴露成 `tool.<name>` 能力。
//! - `provider` feature 提供 Provider 能力适配；Provider 本身仍是普通 Static / Dynamic / JIT
//!   插件，通过通用 [`PluginContext`] registration slot 注册 `ProviderPlugin`。
//! - `wasmtime` feature 提供与具体解释器无关的 Wasmtime 组件沙箱、资源限制、
//!   JIT 缓存和实例池。
//!
//! 依赖方向：`plugin-sdk → plugin`（共享类型与宿主句柄），插件 crate 依赖本 SDK。
//! 动态插件的 stabby 入口（`declare_plugin!` 的动态形态）随 `plugin` 的 `dynamic`
//! feature 一起落地；当前提供的是静态插件路径与共用的宿主句柄类型。

pub mod hook;
#[cfg(feature = "provider")]
pub mod provider;
#[cfg(feature = "storage")]
pub mod storage;
pub mod tool;
#[cfg(feature = "wasmtime")]
pub mod wasmtime;

#[cfg(all(feature = "provider", feature = "dynamic"))]
pub use provider::DynamicProviderPlugin;
#[cfg(feature = "provider")]
pub use provider::ProviderPluginRegistration;
pub use tool::{AgentToolMetadataExt, ToolMetadata};
pub use tower_agent_plugin::{
    AgentLayer, Binding, CapabilityId, CapabilityOverride, Dependency, EventHandler, HOST_ABI,
    HostApi, InflightGuard, Lifecycle, Permission, PermissionDecision, PermissionPolicy,
    PermissionPrompter, PluginAccess, PluginContext, PluginError, PluginId, PluginKind,
    PluginManifest, PluginRegistry, PluginState, ProvidedCapability, Resolution, ResolvedPlugin,
    ShadowedPlugin, StaticPlugin, ToolDescriptor, ToolLayer, agent_layer, event_handler,
    tool_layer, tower_agent_layer, tower_tool_layer,
};
#[cfg(feature = "provider")]
pub use tower_agent_provider::{
    CredentialDescriptor, ProviderBuild, ProviderConfig, ProviderDescriptor, ProviderPlugin,
    ProviderPluginError, ProviderRegistry, ProviderSession,
};
pub use tower_agent_runtime::{ToolAnnotations, ToolExposure, ToolNamespace, ToolRegistration};

#[cfg(feature = "storage")]
pub use storage::{
    ConfigRecord, ConfigScope, ConfigStore, HistoryStore, MemoryRecord, MemoryStore,
    ProviderCredential, ProviderStore, ScriptRecord, ScriptStore, SessionRecord, SessionSnapshot,
    SessionStore, Storage, StorageBackendRegistration, StorageError, StorageFuture,
    StorageMaintenance, StorageManager, StoragePlugin, StorageResult, StorageSnapshot,
    effective_config, resolve_config,
};

/// 插件作者需要与宿主同一版本的 `rkyv`（归档格式与之绑定）。
#[cfg(feature = "dynamic")]
pub use rkyv;
#[cfg(feature = "dynamic")]
pub use tower_agent_plugin::dynamic;
#[cfg(feature = "dynamic")]
pub use tower_agent_plugin::dynamic::{
    AbiBuffer, AbiCapability, AbiHostCall, AbiHostResult, AbiKind, AbiManifest, AbiNode,
    AbiToolCall, AbiToolDescriptor, AbiToolResult, AbiValue, AbiVersion, ArchivedAbiHostCall,
    ArchivedAbiHostResult, ArchivedAbiKind, ArchivedAbiNode, ArchivedAbiToolCall,
    ArchivedAbiToolResult, ArchivedAbiValue, HostDispatch, HostVTable, LoadedPlugin, PluginVTable,
};

/// 导出动态插件的 `__tpa_plugin_entry`（`docs/plugins.md` 第 8.2 节）。
///
/// 四个参数是 `extern "C"` 函数：清单、初始化、调用、关闭。用它在 `cdylib` 里生成入口：
///
/// ```ignore
/// tower_agent_plugin_sdk::declare_dynamic_plugin!(my_manifest, my_init, my_call, my_shutdown);
/// ```
#[cfg(feature = "dynamic")]
#[macro_export]
macro_rules! declare_dynamic_plugin {
    ($manifest:path, $init:path, $call:path, $shutdown:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn __tpa_plugin_entry() -> $crate::PluginVTable {
            $crate::PluginVTable {
                manifest: $manifest,
                init: $init,
                call: $call,
                shutdown: $shutdown,
            }
        }
    };
}

/// 用一个清单 + 安装闭包声明的静态插件。
pub struct DeclaredPlugin<F> {
    manifest: PluginManifest,
    install: F,
}

impl<F> DeclaredPlugin<F> {
    /// 用清单与安装闭包构造。
    pub fn new(manifest: PluginManifest, install: F) -> Self {
        Self { manifest, install }
    }
}

impl<F> StaticPlugin for DeclaredPlugin<F>
where
    F: Fn(&mut PluginContext) -> Result<(), PluginError> + Send + Sync + 'static,
{
    fn manifest(&self) -> PluginManifest {
        self.manifest.clone()
    }

    fn install(&self, context: &mut PluginContext) -> Result<(), PluginError> {
        (self.install)(context)
    }
}

/// 声明一个静态插件：`declare_plugin!(manifest, |context| { ... })`。
#[macro_export]
macro_rules! declare_plugin {
    ($manifest:expr, $install:expr) => {
        $crate::DeclaredPlugin::new($manifest, $install)
    };
}

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::*;

    #[test]
    fn declare_plugin_builds_a_static_plugin() {
        let plugin = declare_plugin!(
            PluginManifest::new("demo", Version::new(1, 0, 0), PluginKind::Static)
                .providing(CapabilityId::new("tool", "demo"), Version::new(1, 0, 0)),
            |_context: &mut PluginContext| -> Result<(), PluginError> { Ok(()) }
        );
        assert_eq!(plugin.manifest().name, "demo");
        assert!(
            plugin
                .manifest()
                .provides_capability(&CapabilityId::new("tool", "demo"))
                .is_some()
        );
        let mut context = PluginContext::new();
        plugin.install(&mut context).expect("install succeeds");
        assert!(context.tools().is_empty());
    }
}
