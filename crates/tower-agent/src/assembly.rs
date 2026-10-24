//! 宿主插件组装层：把「这台宿主提供哪些静态插件」收敛到一处。
//!
//! CLI、axum（以及任何依赖 `tower-agent` 的宿主）都从这里取插件集合，而不是各自在
//! 会话装配里硬编码。组装只认工作目录与能力开关，**不掺会话语义**——codemode 的会话根 /
//! 挂载由调用方作为参数传入。
//!
//! 产出的 [`PluginRegistry`] 固定包含：
//!
//! - 六个内置工具插件（`read` / `edit` / …，来自 [`tower_agent_plugin_tools::builtin_plugins`]）；
//!   `shell` 由独立的动态插件 `tower-agent-plugin-shell` 提供；
//! - 五个内置 provider 插件（Anthropic / OpenAI / OpenAI Responses / Google / DeepSeek）；
//! - `codemode`（feature `codemode`，需显式 [`HostPlugins::with_codemode`]）：受保护静态插件，
//!   提供 `tool.codemode`。
//!
//! 宿主可用 [`HostPlugins::with_provider`] 注入自定义 provider，并用
//! [`HostPlugins::assemble`] 同时取得工具与 provider 注册表。需要 JIT mods 的宿主把
//! 注册表交给 `tower_agent_plugin_monty::ModManager`。

use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

use tower_agent_core::AgentTool;
use tower_agent_plugin::{PluginError, PluginKind, PluginRegistry, StaticPlugin};
use tower_agent_plugin_sdk::ProviderPluginRegistration;
#[cfg(feature = "jit")]
use tower_agent_plugin_sdk::jit::ScriptRuntimeCatalog;
use tower_agent_provider::{ProviderPlugin, ProviderRegistry};

#[cfg(feature = "monty")]
use tower_agent_core::UiState;
#[cfg(feature = "codemode")]
pub use tower_agent_plugin_sdk::codemode::{Mount, SessionRoot};

/// 宿主静态插件集合的组装器。
pub struct HostPlugins {
    cwd: PathBuf,
    extra: Vec<Arc<dyn StaticPlugin>>,
    excluded_plugins: BTreeSet<String>,
    #[cfg(feature = "rquickjs")]
    codemode: Option<CodemodeSetup>,
}

/// Result of resolving and installing the host plugin set.
pub struct HostAssembly {
    /// Installed tools.
    pub tools: Vec<AgentTool>,
    /// Installed provider plugins.
    pub providers: ProviderRegistry,
    /// Storage backends contributed by installed plugins.
    #[cfg(feature = "storage-turso")]
    pub storage_backends: Vec<tower_agent_plugin_sdk::storage::StorageBackendRegistration>,
    /// Combined codemode/JIT interpreter catalog exposed to the model.
    #[cfg(feature = "jit")]
    pub script_catalog: ScriptRuntimeCatalog,
}

#[cfg(feature = "rquickjs")]
struct CodemodeSetup {
    session: SessionRoot,
    mounts: Vec<Mount>,
}

impl HostPlugins {
    /// 以工作目录新建：默认只包含内置工具与 provider 插件。
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            extra: Vec::new(),
            excluded_plugins: BTreeSet::new(),
            #[cfg(feature = "rquickjs")]
            codemode: None,
        }
    }

    /// Interpreter catalog advertised to the model.
    #[cfg(feature = "jit")]
    #[must_use]
    pub fn script_catalog(&self) -> ScriptRuntimeCatalog {
        #[cfg(all(feature = "rquickjs", feature = "monty"))]
        let interpreters = vec![
            tower_agent_plugin_rquickjs::RquickjsRuntime::interpreter_descriptor(),
            tower_agent_plugin_monty::MontyRuntime::interpreter_descriptor(),
        ];
        #[cfg(all(feature = "rquickjs", not(feature = "monty")))]
        let interpreters =
            vec![tower_agent_plugin_rquickjs::RquickjsRuntime::interpreter_descriptor()];
        #[cfg(all(not(feature = "rquickjs"), feature = "monty"))]
        let interpreters = vec![tower_agent_plugin_monty::MontyRuntime::interpreter_descriptor()];
        #[cfg(all(not(feature = "rquickjs"), not(feature = "monty")))]
        let interpreters: Vec<tower_agent_plugin_sdk::codemode::InterpreterDescriptor> = Vec::new();
        ScriptRuntimeCatalog::from_interpreters(interpreters)
    }

    /// 追加 codemode：JS + coreutils 沙箱工具，作为受保护静态插件（`tool.codemode`）。
    ///
    /// `session` 决定沙箱里 `/` 是临时目录（会话即焚）还是持久目录；`mounts` 是会话根之外
    /// 的额外挂载。构造本身很便宜——组件要到安装时才编译（由插件内部缓存）。
    #[cfg(feature = "rquickjs")]
    #[must_use]
    pub fn with_codemode(mut self, session: SessionRoot, mounts: Vec<Mount>) -> Self {
        self.codemode = Some(CodemodeSetup { session, mounts });
        self
    }

    /// 追加任意宿主静态插件（宿主自己的扩展）。
    #[must_use]
    pub fn with_plugin(mut self, plugin: Arc<dyn StaticPlugin>) -> Self {
        self.extra.push(plugin);
        self
    }

    /// Register one provider plugin through the generic plugin host.
    #[must_use]
    pub fn with_provider(self, plugin: Arc<dyn ProviderPlugin>) -> Self {
        self.with_provider_kind(plugin, PluginKind::Static)
    }

    /// Register one provider plugin with an explicit plugin kind.
    #[must_use]
    pub fn with_provider_kind(mut self, plugin: Arc<dyn ProviderPlugin>, kind: PluginKind) -> Self {
        let descriptor = plugin.descriptor();
        if kind == PluginKind::Static {
            self.excluded_plugins
                .insert(format!("provider.{}", descriptor.id));
        }
        self.extra.push(Arc::new(
            ProviderPluginRegistration::new(plugin).with_kind(kind),
        ));
        self
    }

    /// 组装成注册表（只注册，不解析、不安装；JIT / 动态插件之后再叠加）。
    #[must_use]
    pub fn registry(self) -> PluginRegistry {
        let mut registry = PluginRegistry::new();
        for plugin in tower_agent_plugin_tools::builtin_plugins(&self.cwd) {
            registry
                .register_plugin(plugin)
                .expect("内置工具插件清单固定且自洽");
        }
        for plugin in tower_agent_plugin_providers::plugins() {
            if self.excluded_plugins.contains(&plugin.manifest().name) {
                continue;
            }
            registry
                .register_plugin(plugin)
                .expect("内置 provider 插件清单固定且自洽");
        }
        #[cfg(feature = "rquickjs")]
        if let Some(setup) = self.codemode {
            let plugin =
                tower_agent_plugin_rquickjs::CodemodePlugin::new(self.cwd.clone(), setup.session)
                    .with_mounts(setup.mounts);
            registry
                .register_plugin(Arc::new(plugin))
                .expect("codemode 清单固定且自洽");
        }
        for plugin in self.extra {
            registry
                .register_plugin(plugin)
                .expect("宿主插件清单应当自洽");
        }
        registry
    }

    /// 组装、解析并安装，直接得到工具目录（不需要 mods 的宿主用这个）。
    pub fn tools(self) -> Result<Vec<AgentTool>, PluginError> {
        Ok(self.assemble()?.tools)
    }

    /// Resolve and install the host plugin set.
    pub fn assemble(self) -> Result<HostAssembly, PluginError> {
        #[cfg(feature = "jit")]
        let script_catalog = self.script_catalog();
        let context = self.registry().resolve()?.install()?;
        let mut providers = ProviderRegistry::new();
        for extension in context.extensions::<Arc<dyn ProviderPlugin>>() {
            providers
                .register(Arc::clone(extension.value))
                .map_err(|error| PluginError::Install {
                    plugin: "provider".into(),
                    message: error.to_string(),
                })?;
        }
        Ok(HostAssembly {
            tools: context.tools().to_vec(),
            providers,
            #[cfg(feature = "storage-turso")]
            storage_backends: context
                .extensions::<tower_agent_plugin_sdk::storage::StorageBackendRegistration>()
                .map(|extension| extension.value.clone())
                .collect(),
            #[cfg(feature = "jit")]
            script_catalog,
        })
    }

    /// Assemble and return only the provider registry.
    pub fn providers(self) -> Result<ProviderRegistry, PluginError> {
        Ok(self.assemble()?.providers)
    }

    /// 组装注册表并建 JIT mods 宿主（feature `mods`）。
    ///
    /// 宿主静态插件集合定下来后，`tower_agent_plugin_monty` 在其上叠加自己的运行时地基
    /// （受保护静态插件 `agent-monty` + 作者工具 `mod_*`）。会热重载 / 动态插件的宿主
    /// 用这个；只要内置工具 + 宿主能力的宿主用 [`HostPlugins::tools`]。
    #[cfg(feature = "monty")]
    pub fn mods(
        self,
        ui: UiState,
    ) -> Result<Arc<tower_agent_plugin_monty::ModManager>, PluginError> {
        #[cfg(feature = "rquickjs")]
        let capability_surface = self
            .codemode
            .as_ref()
            .map(|setup| {
                tower_agent_plugin_rquickjs::ExecCapabilitySurface::new(
                    setup.session.clone(),
                    setup.mounts.clone(),
                )
                .map(Arc::new)
                .map_err(|error| PluginError::Load {
                    plugin: "codemode-exec".to_owned(),
                    message: error.to_string(),
                })
            })
            .transpose()?;
        let manager = tower_agent_plugin_monty::ModManager::new(ui, self.registry())?;
        #[cfg(feature = "rquickjs")]
        if let Some(surface) = capability_surface {
            manager.set_capability_surface(surface);
        }
        Ok(manager)
    }
}
