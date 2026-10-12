//! mods 宿主：把 monty mod 作为 `PluginKind::Jit` 载荷送进 [`PluginRegistry`]，
//! 让静态 / 动态 / JIT 三种形态共用同一套**解析规则**（覆盖 / 依赖 / 版本 / 遮蔽）。
//!
//! 关键点：`ModManager` 持有注册表，工具目录的唯一来源是「解析 + 安装」的结果——
//! 内置静态插件与 JIT mod 一起解析、一起产出工具，JIT（优先级最高）可遮蔽同名静态插件。
//! 每次 mod 增删改都会重新解析并回写 `HostApi` 与工具目录。

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, Mutex, Weak},
};

use futures_util::{StreamExt as _, future::BoxFuture};
use monty_fs::{MountCallOutcome, MountMode, MountTable};
use monty_types::OsFunctionCall;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tower_agent_core::{
    AgentTool, AgentToolResult, Panel, PanelAnchor, StatusSegment, ToolRenderer, UiState,
};
use tower_agent_plugins::{
    CapabilityId, HostApi, PluginContext, PluginError, PluginId, PluginKind, PluginManifest,
    PluginRegistry, ShadowedPlugin, StaticPlugin,
};
use tower_agent_provider::{Arguments, ToolCall};

use crate::{
    host::{HostCall, MontyHost},
    plugin::MontyPlugin,
    sandbox::SandboxPool,
    store::{ModMeta, ModStore},
    tool::{ModTool, ToolSpec},
    wire,
};

/// 池里默认保留的空闲沙箱数。
const DEFAULT_POOL_SIZE: usize = 4;

/// JIT 运行时的保留名字（mods 作者工具）：作为整个 JIT 插件体系的地基，**不允许**
/// 被任何 JIT 插件遮蔽——出现这种插件直接阻断。
pub const RESERVED_NAMES: [&str; 6] = crate::author::AUTHORING_TOOL_NAMES;

/// JIT 运行时地基的插件名（注册表里带 `protected` 标记）。
pub const JIT_RUNTIME_PLUGIN: &str = "agent-monty";

/// 名字是否被 JIT 运行时保留。
#[must_use]
pub fn is_reserved_name(name: &str) -> bool {
    RESERVED_NAMES.contains(&name)
}

/// JIT 运行时地基的清单：protected，提供 `tool.mod_*` 与 `jit.run`。
#[must_use]
pub fn jit_runtime_manifest() -> PluginManifest {
    let mut manifest = PluginManifest::new(
        JIT_RUNTIME_PLUGIN,
        Version::new(1, 0, 0),
        PluginKind::Static,
    )
    .protected()
    .providing(CapabilityId::new("jit", "run"), Version::new(1, 0, 0));
    for name in crate::author::AUTHORING_TOOL_NAMES {
        manifest = manifest.providing(CapabilityId::new("tool", name), Version::new(1, 0, 0));
    }
    manifest
}

/// 一条工具级遮蔽记录（不同插件提供同名工具时）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowedTool {
    /// 被覆盖的工具名。
    pub tool: String,
    /// 原提供者。
    pub provider: PluginId,
    /// 覆盖者。
    pub shadowed_by: PluginId,
}

/// 一个 mod 的权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ModPermissions {
    /// 允许调用宿主工具（`tool_call` / `tool_list`）。
    pub tools: bool,
    /// 允许注册/注销工具（`tool_register` / `tool_unregister`）。
    pub register_tools: bool,
    /// 允许贡献 UI（`ui_*`）。
    pub ui: bool,
    /// 允许读写宿主状态（`state_*`）与 `log`。
    pub state: bool,
    /// 允许文件系统 / OS 调用（实际可写范围由挂载模式决定）。
    pub fs: bool,
}

impl Default for ModPermissions {
    fn default() -> Self {
        Self {
            tools: true,
            register_tools: true,
            ui: true,
            state: true,
            fs: true,
        }
    }
}

/// 写一个 mod 时的选项。
#[derive(Debug, Clone, Default)]
pub struct ModOptions {
    /// 权限（缺省用默认权限）。
    pub permissions: Option<ModPermissions>,
    /// 依赖的宿主能力，形如 `tool.read`（进注册表做依赖校验/拓扑排序）。
    pub requires: Vec<String>,
}

/// 一个已加载 mod 的摘要（用于 `mod_list` / UI 展示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModSummary {
    /// mod 名。
    pub name: String,
    /// 该 mod 当前提供的工具名（按字典序）。
    pub tools: Vec<String>,
    /// 源码字节数。
    pub code_len: usize,
}

/// mods 宿主。
pub struct ModManager {
    host: HostApi,
    ui: UiState,
    pool: Arc<SandboxPool>,
    mounts: Mutex<MountTable>,
    permissions: Mutex<BTreeMap<String, ModPermissions>>,
    /// 三形态共用的注册表（调用方通常塞入内置静态插件）。
    registry: Mutex<PluginRegistry>,
    /// 最近一次成功解析 + 安装后的工具目录（唯一来源）。
    catalog: Mutex<Vec<AgentTool>>,
    /// 最近一次解析的遮蔽记录（诊断用）。
    shadows: Mutex<Vec<ShadowedPlugin>>,
    /// 工具级遮蔽记录（同名工具被更高优先级/后写者覆盖）。
    shadowed_tools: Mutex<Vec<ShadowedTool>>,
    mods: Mutex<BTreeMap<String, Arc<MontyPlugin>>>,
    specs: Mutex<BTreeMap<String, Vec<ToolSpec>>>,
    requires: Mutex<BTreeMap<String, Vec<String>>>,
    kv: Mutex<BTreeMap<String, Value>>,
    /// 落盘位置（持久化 = dirs config，临时 = dirs cache）。
    store: Mutex<ModStore>,
}

impl std::fmt::Debug for ModManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModManager")
            .field("mods", &self.list())
            .field("idle_sandboxes", &self.pool.idle_count())
            .finish_non_exhaustive()
    }
}

impl ModManager {
    /// 用共享 UI 状态与初始注册表构造，并立刻解析一次。
    ///
    /// 初始注册表由**宿主组装层**提供（`tower_agent::assembly::HostPlugins`）：内置工具 +
    /// `wasmtime` / `codemode` 这类宿主能力；`ModManager` 只再叠加自己的 JIT 运行时地基。
    /// `HostApi` 的工具表由解析结果填充，因此宿主能力与工具目录同源。
    pub fn new(ui: UiState, registry: PluginRegistry) -> Result<Arc<Self>, PluginError> {
        let manager = Arc::new(Self {
            host: HostApi::from_tools(Vec::<AgentTool>::new()),
            ui,
            pool: crate::sandbox::new_pool(DEFAULT_POOL_SIZE),
            mounts: Mutex::new(MountTable::new()),
            permissions: Mutex::new(BTreeMap::new()),
            registry: Mutex::new(registry),
            catalog: Mutex::new(Vec::new()),
            shadows: Mutex::new(Vec::new()),
            shadowed_tools: Mutex::new(Vec::new()),
            mods: Mutex::new(BTreeMap::new()),
            specs: Mutex::new(BTreeMap::new()),
            requires: Mutex::new(BTreeMap::new()),
            kv: Mutex::new(BTreeMap::new()),
            store: Mutex::new(ModStore::from_dirs()),
        });
        // JIT 运行时以 protected 载荷注册，并在 install 时把作者工具装进工具目录。
        // 工具句柄用 Weak 指回本管理器，避免强引用环。
        let foundation = FoundationPlugin {
            manifest: jit_runtime_manifest(),
            tools: Mutex::new(crate::author::authoring_tools(Arc::downgrade(&manager))),
        };
        manager
            .registry
            .lock()
            .expect("registry poisoned")
            .register_plugin(Arc::new(foundation))?;
        manager.resolve_and_apply()?;
        Ok(manager)
    }

    /// 注册一个静态插件（宿主能力 / 地基），并立刻重新解析安装。
    ///
    /// 与直接操作 [`PluginRegistry`] 不同，这里会立即回写工具目录，因此插件的工具与
    /// **受保护的名字 / 能力**随即对后续 JIT / 动态插件生效；解析失败会回滚候选。
    pub fn register_static_plugin(&self, plugin: Arc<dyn StaticPlugin>) -> Result<(), PluginError> {
        let name = plugin.manifest().name;
        self.registry
            .lock()
            .expect("registry poisoned")
            .register_static(plugin)?;
        if let Err(error) = self.resolve_and_apply() {
            self.registry
                .lock()
                .expect("registry poisoned")
                .remove(&name);
            return Err(error);
        }
        Ok(())
    }

    /// 共享的 UI 状态（前端据此渲染 mod 贡献）。
    #[must_use]
    pub fn ui(&self) -> UiState {
        self.ui.clone()
    }

    /// 宿主能力表（含内置工具与解析出来的 mod 工具）。
    #[must_use]
    pub fn host_api(&self) -> &HostApi {
        &self.host
    }

    /// 解析并安装当前注册表：回写 `HostApi` 与工具目录，返回工具集。
    pub fn resolve_and_apply(&self) -> Result<Vec<AgentTool>, PluginError> {
        let (tools, sources, shadows, protected_plugins) = {
            let registry = self.registry.lock().expect("registry poisoned");
            let resolution = registry.resolve()?;
            let shadows = resolution.shadows().to_vec();
            let protected_plugins: BTreeSet<String> =
                registry.protected_names().into_iter().collect();
            let context = resolution.install()?;
            (
                context.tools().to_vec(),
                context.tool_sources().to_vec(),
                shadows,
                protected_plugins,
            )
        };
        let (catalog, shadowed_tools) = dedupe_tools(&tools, &sources, &protected_plugins);
        self.host.replace_tools(catalog.clone());
        *self.catalog.lock().expect("catalog poisoned") = catalog.clone();
        *self.shadows.lock().expect("shadows poisoned") = shadows;
        *self.shadowed_tools.lock().expect("shadows poisoned") = shadowed_tools;
        Ok(catalog)
    }

    /// 当前工具目录（最近一次成功解析 + 安装的结果）。
    #[must_use]
    pub fn tools(&self) -> Vec<AgentTool> {
        self.catalog.lock().expect("catalog poisoned").clone()
    }

    /// 当前解析产生的遮蔽记录（诊断 `/plugins` 用）。
    #[must_use]
    pub fn shadows(&self) -> Vec<ShadowedPlugin> {
        self.shadows.lock().expect("shadows poisoned").clone()
    }

    /// 当前解析产生的**工具级**遮蔽记录（同名工具被覆盖）。
    #[must_use]
    pub fn shadowed_tools(&self) -> Vec<ShadowedTool> {
        self.shadowed_tools
            .lock()
            .expect("shadows poisoned")
            .clone()
    }

    /// 挂载一个宿主目录到沙箱虚拟路径。
    pub fn mount(
        &self,
        virtual_path: &str,
        host_path: impl AsRef<Path>,
        mode: MountMode,
    ) -> Result<(), PluginError> {
        self.mounts
            .lock()
            .expect("mount table poisoned")
            .mount(virtual_path, host_path, mode, None)
            .map_err(|error| PluginError::Load {
                plugin: "mount".to_string(),
                message: error.to_string(),
            })
    }

    /// 只读挂载宿主目录（写操作抛 `PermissionError`）。
    pub fn mount_readonly(
        &self,
        virtual_path: &str,
        host_path: impl AsRef<Path>,
    ) -> Result<(), PluginError> {
        self.mount(virtual_path, host_path, MountMode::ReadOnly)
    }

    /// 以内存 overlay 挂载宿主目录：读取穿透到宿主，写入留在内存（不会改宿主）。
    pub fn mount_overlay(
        &self,
        virtual_path: &str,
        host_path: impl AsRef<Path>,
    ) -> Result<(), PluginError> {
        self.mount(
            virtual_path,
            host_path,
            MountMode::OverlayMemory(monty_fs::OverlayState::new()),
        )
    }

    /// 预热沙箱池：编译组件并预先创建 `instances` 个实例。
    pub fn warmup(&self, instances: usize) -> Result<(), PluginError> {
        self.pool
            .warmup(instances)
            .map_err(crate::sandbox::to_plugin_error)
    }

    /// 在当前运行时后台预热（没有运行时则跳过，首次使用再惰性编译）。
    pub fn spawn_warmup(self: &Arc<Self>) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let this = Arc::clone(self);
            handle.spawn_blocking(move || {
                let _ = this.warmup(2);
            });
        }
    }

    /// 当前空闲沙箱数（测试/诊断用）。
    #[must_use]
    pub fn idle_sandboxes(&self) -> usize {
        self.pool.idle_count()
    }

    /// 替换落盘位置（测试用；默认是 `dirs` 的 config/cache）。
    pub fn set_store(&self, store: ModStore) {
        *self.store.lock().expect("mod store poisoned") = store;
    }

    /// 当前落盘位置。
    #[must_use]
    pub fn store(&self) -> ModStore {
        self.store.lock().expect("mod store poisoned").clone()
    }

    /// 把某个 mod 的源码 + 元数据写到**临时**（cache）目录。
    fn save_ephemeral(&self, name: &str) {
        let Some(plugin) = self.mods.lock().expect("mods poisoned").get(name).cloned() else {
            return;
        };
        let meta = self.meta(name);
        self.store
            .lock()
            .expect("mod store poisoned")
            .save_ephemeral(name, &plugin.code(), &meta);
    }

    /// 某个 mod 的落盘元数据。
    fn meta(&self, name: &str) -> ModMeta {
        ModMeta {
            permissions: self.permissions(name),
            requires: self
                .requires
                .lock()
                .expect("mod requires poisoned")
                .get(name)
                .cloned()
                .unwrap_or_default(),
        }
    }

    /// 持久化某个 mod（写到 `dirs` 的 config 目录）；mod 不存在时报错。
    pub fn persist(&self, name: &str) -> Result<(), PluginError> {
        let plugin = self
            .mods
            .lock()
            .expect("mods poisoned")
            .get(name)
            .cloned()
            .ok_or_else(|| mod_error(name, "unknown mod"))?;
        let meta = self.meta(name);
        self.store
            .lock()
            .expect("mod store poisoned")
            .persist(name, &plugin.code(), &meta)
            .map_err(|message| mod_error(name, message))
    }

    /// 丢弃某个 mod：卸载实例并从 config/cache 两个目录删除。
    pub fn discard(&self, name: &str) -> Result<bool, PluginError> {
        let removed = self.remove(name)?;
        self.store.lock().expect("mod store poisoned").discard(name);
        Ok(removed)
    }

    /// 启动时加载持久化的 mod（`dirs` 的 config 目录），按名字顺序写入。
    pub async fn load_persisted(self: &Arc<Self>) -> usize {
        let stored = self
            .store
            .lock()
            .expect("mod store poisoned")
            .load_persisted();
        let mut loaded = 0;
        for stored in stored {
            let options = ModOptions {
                permissions: Some(stored.meta.permissions),
                requires: stored.meta.requires,
            };
            match self
                .write_with_options(&stored.name, &stored.code, options)
                .await
            {
                Ok(_) => loaded += 1,
                Err(error) => eprintln!("加载持久化 mod `{}` 失败：{error}", stored.name),
            }
        }
        loaded
    }

    /// 从目录加载动态插件（`.so` / `.dll` / `.dylib`）并注册进同一张注册表。
    ///
    /// # Safety
    ///
    /// 目录里的库会被 dlopen 并执行，调用方必须保证它们可信。
    #[allow(unsafe_code)]
    pub unsafe fn load_dynamic_plugins(
        self: &Arc<Self>,
        dir: impl AsRef<Path>,
    ) -> Result<Vec<String>, PluginError> {
        let loaded = {
            let mut registry = self.registry.lock().expect("registry poisoned");
            unsafe { crate::dynamic::load_from_dir(&mut registry, dir, self) }?
        };
        if !loaded.is_empty() {
            self.resolve_and_apply()?;
        }
        Ok(loaded)
    }

    /// 从 `dirs` 的 config 目录（`tower-agent/plugins`）加载动态插件。
    ///
    /// # Safety
    ///
    /// 同上：库会被加载并执行。
    #[allow(unsafe_code)]
    pub unsafe fn load_dynamic_plugins_from_config(
        self: &Arc<Self>,
    ) -> Result<Vec<String>, PluginError> {
        let Some(dir) = dirs::config_dir().map(|dir| dir.join("tower-agent").join("plugins"))
        else {
            return Ok(Vec::new());
        };
        unsafe { self.load_dynamic_plugins(dir) }
    }

    /// 设置某个 mod 的权限（默认在安装时写入）。
    pub fn set_permissions(&self, name: &str, permissions: ModPermissions) {
        self.permissions
            .lock()
            .expect("permissions poisoned")
            .insert(name.to_string(), permissions);
    }

    /// 取某个 mod 的权限（没有记录时返回默认权限）。
    #[must_use]
    pub fn permissions(&self, name: &str) -> ModPermissions {
        self.permissions
            .lock()
            .expect("permissions poisoned")
            .get(name)
            .copied()
            .unwrap_or_default()
    }

    /// 当前所有 mod 的摘要（按名字排序）。
    #[must_use]
    pub fn list(&self) -> Vec<ModSummary> {
        let mods = self.mods.lock().expect("mods poisoned");
        let specs = self.specs.lock().expect("mod specs poisoned");
        mods.iter()
            .map(|(name, plugin)| {
                let mut tool_names: Vec<String> = specs
                    .get(name)
                    .map(|specs| specs.iter().map(|spec| spec.name.clone()).collect())
                    .unwrap_or_default();
                tool_names.sort();
                ModSummary {
                    name: name.clone(),
                    tools: tool_names,
                    code_len: plugin.code().len(),
                }
            })
            .collect()
    }

    /// 写一个 mod：不存在则创建，已存在则热重载。
    pub async fn write(
        self: &Arc<Self>,
        name: &str,
        code: &str,
    ) -> Result<ModSummary, PluginError> {
        self.write_with_options(name, code, ModOptions::default())
            .await
    }

    /// 写一个 mod，并指定权限与依赖。
    pub async fn write_with_options(
        self: &Arc<Self>,
        name: &str,
        code: &str,
        options: ModOptions,
    ) -> Result<ModSummary, PluginError> {
        if let Some(permissions) = options.permissions {
            self.set_permissions(name, permissions);
        }
        if self.mods.lock().expect("mods poisoned").contains_key(name) {
            return self
                .reload_with_requires(name, code, options.requires)
                .await;
        }
        self.install(name, code, options.requires).await
    }

    /// 热重载一个已存在的 mod（失败保留旧版本）。
    pub async fn reload(
        self: &Arc<Self>,
        name: &str,
        code: &str,
    ) -> Result<ModSummary, PluginError> {
        self.reload_with_requires(name, code, Vec::new()).await
    }

    async fn reload_with_requires(
        self: &Arc<Self>,
        name: &str,
        code: &str,
        requires: Vec<String>,
    ) -> Result<ModSummary, PluginError> {
        let plugin = self
            .mods
            .lock()
            .expect("mods poisoned")
            .get(name)
            .cloned()
            .ok_or_else(|| mod_error(name, "unknown mod"))?;

        // 先撤下旧候选并清空旧工具声明；失败时恢复。
        let old_specs = self.take_specs(name);
        let old_requires = self.take_requires(name);
        let old_candidate = self
            .registry
            .lock()
            .expect("registry poisoned")
            .remove(name);
        if let Err(error) = plugin.reload_checked(code).await {
            self.push_specs(name, old_specs);
            self.set_requires(name, old_requires);
            self.register_candidate(name, &plugin);
            return Err(error);
        }
        if let Err(error) = self.check_shadowing(name) {
            self.push_specs(name, old_specs);
            self.set_requires(name, old_requires);
            self.register_candidate(name, &plugin);
            return Err(error);
        }
        self.set_requires(name, requires);
        match self.register_and_apply(name, &plugin) {
            Ok(()) => {
                self.save_ephemeral(name);
                Ok(self.summary(name))
            }
            Err(error) => {
                self.registry
                    .lock()
                    .expect("registry poisoned")
                    .remove(name);
                self.push_specs(name, old_specs);
                self.set_requires(name, old_requires);
                if old_candidate > 0 {
                    self.register_candidate(name, &plugin);
                }
                Err(error)
            }
        }
    }

    /// 移除一个 mod：注销候选与工具、卸载插件。返回是否真的移除了。
    pub fn remove(&self, name: &str) -> Result<bool, PluginError> {
        let plugin = self.mods.lock().expect("mods poisoned").remove(name);
        let Some(plugin) = plugin else {
            return Ok(false);
        };
        self.registry
            .lock()
            .expect("registry poisoned")
            .remove(name);
        self.take_specs(name);
        self.take_requires(name);
        self.permissions
            .lock()
            .expect("permissions poisoned")
            .remove(name);
        let _ = plugin.stop();
        let _ = plugin.unload();
        self.resolve_and_apply()?;
        Ok(true)
    }

    /// 创建并安装一个新 mod（构造 → `activate` → 进注册表 → 解析安装）。
    async fn install(
        self: &Arc<Self>,
        name: &str,
        code: &str,
        requires: Vec<String>,
    ) -> Result<ModSummary, PluginError> {
        let manifest = PluginManifest::new(name, Version::new(1, 0, 0), PluginKind::Jit);
        let adapter = Arc::new(ModHostAdapter {
            manager: Arc::downgrade(self),
            plugin: name.to_string(),
        });
        let plugin = Arc::new(MontyPlugin::load(
            manifest,
            code,
            adapter,
            Arc::clone(&self.pool),
        )?);
        self.take_specs(name);
        if let Err(error) = plugin.activate().await {
            self.take_specs(name);
            return Err(error);
        }
        if let Err(error) = self.check_shadowing(name) {
            self.take_specs(name);
            return Err(error);
        }
        self.set_requires(name, requires);
        if let Err(error) = self.register_and_apply(name, &plugin) {
            self.registry
                .lock()
                .expect("registry poisoned")
                .remove(name);
            self.take_specs(name);
            self.take_requires(name);
            return Err(error);
        }
        self.mods
            .lock()
            .expect("mods poisoned")
            .insert(name.to_string(), plugin);
        self.save_ephemeral(name);
        Ok(self.summary(name))
    }

    /// 阻断会遮蔽「JIT 运行时地基」或其它 mod 工具的 JIT 插件。
    ///
    /// - 插件名 / 工具名命中保留名（`mod_write` 等）→ 阻断；
    /// - 工具名与**另一个 mod** 提供的工具重名 → 阻断（JIT 不能遮蔽 JIT 的基础设施）。
    fn check_shadowing(&self, name: &str) -> Result<(), PluginError> {
        let protected_tools: BTreeSet<String> = {
            let registry = self.registry.lock().expect("registry poisoned");
            // 受保护插件名（JIT 运行时地基 + wasmtime / codemode 等宿主能力）统一走注册表规则。
            registry.ensure_not_protected(name)?;
            registry
                .protected_capabilities()
                .into_iter()
                .filter(|capability| capability.namespace == "tool")
                .map(|capability| capability.name)
                .collect()
        };
        if is_reserved_name(name) {
            return Err(PluginError::Install {
                plugin: name.to_string(),
                message: format!(
                    "plugin name `{name}` is reserved by the JIT runtime and cannot be shadowed"
                ),
            });
        }
        let specs = self
            .specs
            .lock()
            .expect("mod specs poisoned")
            .get(name)
            .cloned()
            .unwrap_or_default();

        for spec in &specs {
            if protected_tools.contains(&spec.name) || is_reserved_name(&spec.name) {
                return Err(PluginError::Install {
                    plugin: name.to_string(),
                    message: format!(
                        "tool name `{}` is reserved by the JIT runtime and cannot be shadowed",
                        spec.name
                    ),
                });
            }
        }
        Ok(())
    }

    /// 把插件注册成 JIT 候选并重新解析安装。
    fn register_and_apply(&self, name: &str, plugin: &Arc<MontyPlugin>) -> Result<(), PluginError> {
        self.register_candidate(name, plugin);
        match self.resolve_and_apply() {
            Ok(_) => Ok(()),
            Err(error) => {
                self.registry
                    .lock()
                    .expect("registry poisoned")
                    .remove(name);
                Err(error)
            }
        }
    }

    fn register_candidate(&self, name: &str, plugin: &Arc<MontyPlugin>) {
        let specs = self
            .specs
            .lock()
            .expect("mod specs poisoned")
            .get(name)
            .cloned()
            .unwrap_or_default();
        let requires = self
            .requires
            .lock()
            .expect("mod requires poisoned")
            .get(name)
            .cloned()
            .unwrap_or_default();
        let payload = Arc::new(ModJitPlugin {
            manifest: jit_manifest(name, &requires, &specs),
            plugin: Arc::clone(plugin),
            specs,
        });
        self.registry
            .lock()
            .expect("registry poisoned")
            .register_plugin(payload)
            .expect("JIT manifest 固定且自洽");
    }

    fn set_requires(&self, name: &str, requires: Vec<String>) {
        self.requires
            .lock()
            .expect("mod requires poisoned")
            .insert(name.to_string(), requires);
    }

    fn take_requires(&self, name: &str) -> Vec<String> {
        self.requires
            .lock()
            .expect("mod requires poisoned")
            .remove(name)
            .unwrap_or_default()
    }

    fn take_specs(&self, name: &str) -> Vec<ToolSpec> {
        self.specs
            .lock()
            .expect("mod specs poisoned")
            .remove(name)
            .unwrap_or_default()
    }

    fn push_specs(&self, name: &str, specs: Vec<ToolSpec>) {
        self.specs
            .lock()
            .expect("mod specs poisoned")
            .insert(name.to_string(), specs);
    }

    fn summary(&self, name: &str) -> ModSummary {
        self.list()
            .into_iter()
            .find(|summary| summary.name == name)
            .unwrap_or(ModSummary {
                name: name.to_string(),
                tools: Vec::new(),
                code_len: 0,
            })
    }

    /// 派发一次宿主调用（由 [`ModHostAdapter`] 调用）。
    pub(crate) async fn dispatch(&self, call: HostCall) -> Result<Value, String> {
        let permissions = self.permissions(&call.plugin);
        let denied = |what: &str| Err(format!("mod `{}` is not allowed to {what}", call.plugin));

        match call.function.as_str() {
            "tool_call" | "tool_list" => {
                if !permissions.tools {
                    return denied("call host tools");
                }
                if call.function == "tool_list" {
                    Ok(self.tool_names())
                } else {
                    self.host_tool_call(&call).await
                }
            }
            "tool_register" | "tool_unregister" => {
                if !permissions.register_tools {
                    return denied("register tools");
                }
                if call.function == "tool_register" {
                    self.host_tool_register(&call)
                } else {
                    self.host_tool_unregister(&call)
                }
            }
            "log" => {
                if !permissions.state {
                    return denied("log");
                }
                let message = arg_str(&call, 0, "message").unwrap_or_default();
                eprintln!("[mod {}] {message}", call.plugin);
                Ok(Value::Null)
            }
            "state_set" => {
                if !permissions.state {
                    return denied("write host state");
                }
                let key = arg_str(&call, 0, "key").ok_or("state_set requires (key, value)")?;
                let value = call
                    .positional_or_kwarg(1, "value")
                    .cloned()
                    .unwrap_or(Value::Null);
                self.kv
                    .lock()
                    .expect("mod kv poisoned")
                    .insert(key.to_string(), value);
                Ok(Value::Null)
            }
            "state_get" => {
                if !permissions.state {
                    return denied("read host state");
                }
                let key = arg_str(&call, 0, "key").ok_or("state_get requires (key)")?;
                Ok(self
                    .kv
                    .lock()
                    .expect("mod kv poisoned")
                    .get(key)
                    .cloned()
                    .unwrap_or(Value::Null))
            }
            "ui_state" => {
                if !permissions.ui {
                    return denied("read UI state");
                }
                serde_json::to_value(self.ui.snapshot()).map_err(|error| error.to_string())
            }
            name if name.starts_with("ui_") => {
                if !permissions.ui {
                    return denied("change the UI");
                }
                match name {
                    "ui_status" => self.host_ui_status(&call),
                    "ui_panel" => self.host_ui_panel(&call),
                    "ui_clear" => self.host_ui_clear(&call),
                    "ui_tool_render" => self.host_ui_tool_render(&call),
                    other => Err(format!("unknown host function `{other}`")),
                }
            }
            other => Err(format!("unknown host function `{other}`")),
        }
    }

    /// 服务一次 OS 调用（由 [`ModHostAdapter::handle_os_call`] 调用）。
    fn handle_os_call(&self, plugin: &str, call_json: &str) -> String {
        if !self.permissions(plugin).fs {
            return wire::permission_denied("filesystem access is not allowed for this mod")
                .to_string();
        }
        let call: OsFunctionCall = match serde_json::from_str(call_json) {
            Ok(call) => call,
            Err(error) => {
                return wire::error_result(monty_types::MontyException::new(
                    monty_types::ExcType::RuntimeError,
                    Some(format!("invalid os-call payload: {error}")),
                ))
                .to_string();
            }
        };
        let outcome = self
            .mounts
            .lock()
            .expect("mount table poisoned")
            .handle_os_call(call);
        match outcome {
            MountCallOutcome::Handled(Ok(value)) => wire::return_result(&value).to_string(),
            MountCallOutcome::Handled(Err(error)) => {
                wire::error_result(error.into_exception()).to_string()
            }
            MountCallOutcome::NotHandled(_) => wire::NOT_HANDLED.to_string(),
        }
    }

    fn tool_names(&self) -> Value {
        let mut names = self.host.capabilities();
        names.sort();
        json!(names)
    }

    async fn host_tool_call(&self, call: &HostCall) -> Result<Value, String> {
        let name = arg_str(call, 0, "name").ok_or("tool_call requires (name, args)")?;
        let arguments = match call.positional_or_kwarg(1, "args") {
            Some(Value::Object(object)) => object.clone(),
            Some(Value::Null) | None => serde_json::Map::new(),
            Some(other) => return Err(format!("tool_call args must be an object, got {other}")),
        };
        let request = tower_agent_core::ToolCallRequest::new(ToolCall {
            id: format!("mod:{}:{name}", call.plugin),
            name: name.to_string(),
            arguments: Arguments::from(arguments),
            thought_signature: None,
            namespace: None,
        });
        let stream = self
            .host
            .call_tool(name, request)
            .await
            .map_err(|error| error.to_string())?;
        let mut stream = stream;
        let mut last: Option<AgentToolResult> = None;
        while let Some(result) = stream.next().await {
            last = Some(result);
        }
        let result = last.ok_or_else(|| format!("tool `{name}` produced no result"))?;
        if result.is_error {
            return Err(result_text(&result));
        }
        Ok(Value::String(result_text(&result)))
    }

    fn host_tool_register(&self, call: &HostCall) -> Result<Value, String> {
        if !call.setup {
            // 非安装期调用（每次工具调用都会重跑脚本）直接忽略，避免重复注册。
            return Ok(Value::Null);
        }
        let name = arg_str(call, 0, "name").ok_or("tool_register requires (name, function)")?;
        let function =
            arg_str(call, 1, "function").ok_or("tool_register requires (name, function)")?;
        let description = arg_str(call, 2, "description").unwrap_or_default();
        let parameters = call
            .positional_or_kwarg(3, "parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" }));
        let spec = ToolSpec {
            name: name.to_string(),
            function: function.to_string(),
            description: description.to_string(),
            parameters,
        };
        let mut specs = self.specs.lock().expect("mod specs poisoned");
        let entry = specs.entry(call.plugin.clone()).or_default();
        entry.retain(|existing| existing.name != spec.name);
        entry.push(spec);
        Ok(Value::Null)
    }

    fn host_tool_unregister(&self, call: &HostCall) -> Result<Value, String> {
        if !call.setup {
            return Ok(Value::Null);
        }
        let name = arg_str(call, 0, "name").ok_or("tool_unregister requires (name)")?;
        let mut specs = self.specs.lock().expect("mod specs poisoned");
        if let Some(entry) = specs.get_mut(&call.plugin) {
            entry.retain(|spec| spec.name != name);
        }
        Ok(Value::Null)
    }

    fn host_ui_status(&self, call: &HostCall) -> Result<Value, String> {
        let id = arg_str(call, 0, "id").ok_or("ui_status requires (id, text)")?;
        let text = arg_str(call, 1, "text").ok_or("ui_status requires (id, text)")?;
        let color = arg_str(call, 2, "color").map(str::to_string);
        let priority = call
            .positional_or_kwarg(3, "priority")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        self.ui.set_status(StatusSegment {
            id: id.to_string(),
            text: text.to_string(),
            color,
            priority,
        });
        Ok(Value::Null)
    }

    fn host_ui_panel(&self, call: &HostCall) -> Result<Value, String> {
        let id = arg_str(call, 0, "id").ok_or("ui_panel requires (id, title, lines)")?;
        let title = arg_str(call, 1, "title").unwrap_or_default();
        let lines = call
            .positional_or_kwarg(2, "lines")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| match item {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
            })
            .ok_or("ui_panel requires lines as an array of strings")?;
        let mut panel = Panel::new(id, title, lines);
        if let Some(anchor) = arg_str(call, 3, "anchor") {
            panel.anchor = PanelAnchor::parse(anchor);
        }
        panel.width = call
            .positional_or_kwarg(4, "width")
            .and_then(Value::as_u64)
            .map(|value| value as u16);
        panel.height = call
            .positional_or_kwarg(5, "height")
            .and_then(Value::as_u64)
            .map(|value| value as u16);
        self.ui.set_panel(panel);
        Ok(Value::Null)
    }

    fn host_ui_clear(&self, call: &HostCall) -> Result<Value, String> {
        match arg_str(call, 0, "id") {
            Some(id) => {
                self.ui.remove_panel(id);
                self.ui.remove_status(id);
            }
            None => self.ui.clear(),
        }
        Ok(Value::Null)
    }

    fn host_ui_tool_render(&self, call: &HostCall) -> Result<Value, String> {
        let tool = arg_str(call, 0, "tool").ok_or("ui_tool_render requires (tool, lines)")?;
        let lines = call
            .positional_or_kwarg(1, "lines")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| match item {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
            })
            .ok_or("ui_tool_render requires lines as an array of strings")?;
        self.ui
            .set_tool_renderer(tool.to_string(), ToolRenderer::new(lines));
        Ok(Value::Null)
    }
}

/// 一个 JIT（monty）插件的注册表载荷：声明 `tool.<name>` 并把工具装进上下文。
struct ModJitPlugin {
    manifest: PluginManifest,
    plugin: Arc<MontyPlugin>,
    specs: Vec<ToolSpec>,
}

/// JIT 运行时地基插件：`protected`，`install` 时装出 mods 作者工具。
///
/// `AgentTool` 非 `Sync`，用 `Mutex` 包一层让它能作为 `StaticPlugin`。
struct FoundationPlugin {
    manifest: PluginManifest,
    tools: Mutex<Vec<AgentTool>>,
}

impl StaticPlugin for FoundationPlugin {
    fn manifest(&self) -> PluginManifest {
        self.manifest.clone()
    }

    fn install(&self, context: &mut PluginContext) -> Result<(), PluginError> {
        let tools = self.tools.lock().map_err(|_| PluginError::Install {
            plugin: self.manifest.name.clone(),
            message: "foundation tools poisoned".to_string(),
        })?;
        for tool in tools.iter() {
            context.register_tool(tool.clone());
        }
        Ok(())
    }
}

impl StaticPlugin for ModJitPlugin {
    fn manifest(&self) -> PluginManifest {
        self.manifest.clone()
    }

    fn install(&self, context: &mut PluginContext) -> Result<(), PluginError> {
        for spec in &self.specs {
            let tool = ModTool::new(Arc::clone(&self.plugin), spec.clone()).into_agent_tool();
            context.register_tool(tool);
        }
        Ok(())
    }
}

/// 构造 JIT 插件清单：kind = Jit，provides = 它注册的工具，requires = 声明的宿主能力。
fn jit_manifest(name: &str, requires: &[String], specs: &[ToolSpec]) -> PluginManifest {
    let mut manifest = PluginManifest::new(name, Version::new(1, 0, 0), PluginKind::Jit);
    for spec in specs {
        manifest = manifest.providing(
            CapabilityId::new("tool", spec.name.clone()),
            Version::new(1, 0, 0),
        );
    }
    for required in requires {
        if let Some(capability) = CapabilityId::parse(required) {
            manifest = manifest.requiring(capability, VersionReq::STAR);
        }
    }
    manifest
}

/// 每个 mod 的宿主适配器：把调用转交给 [`ModManager`]。
struct ModHostAdapter {
    manager: Weak<ModManager>,
    plugin: String,
}

impl MontyHost for ModHostAdapter {
    fn dispatch(&self, call: HostCall) -> BoxFuture<'static, Result<Value, String>> {
        let manager = self.manager.upgrade();
        let plugin = self.plugin.clone();
        Box::pin(async move {
            let Some(manager) = manager else {
                return Err(format!("mod `{plugin}` host is gone"));
            };
            manager.dispatch(call).await
        })
    }

    fn handle_os_call(&self, call_json: &str) -> String {
        match self.manager.upgrade() {
            Some(manager) => manager.handle_os_call(&self.plugin, call_json),
            None => wire::NOT_HANDLED.to_string(),
        }
    }
}

/// 工具结果的文本内容（拼接所有文本块）。
fn result_text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .map(|block| match block {
            tower_agent_provider::ContentBlock::Text(text) => text.text.clone(),
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 按工具名去重，得到最终目录与工具级遮蔽记录。
///
/// 胜出规则：**受保护 > 形态优先级（静态 < 动态 < JIT） > 后写者**。同名工具被覆盖时
/// 记一条 [`ShadowedTool`]；受保护的提供者永不被覆盖。
fn dedupe_tools(
    tools: &[AgentTool],
    sources: &[Option<(PluginId, PluginKind)>],
    protected_plugins: &BTreeSet<String>,
) -> (Vec<AgentTool>, Vec<ShadowedTool>) {
    let mut winners: Vec<AgentTool> = Vec::new();
    let mut winner_sources: Vec<(PluginId, PluginKind, bool)> = Vec::new();
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    let mut shadowed: Vec<ShadowedTool> = Vec::new();

    for (tool, source) in tools.iter().zip(sources.iter()) {
        let name = tool.name().to_string();
        let (id, kind) = match source {
            Some((id, kind)) => (id.clone(), *kind),
            None => (
                PluginId::new("<unknown>", Version::new(0, 0, 0)),
                PluginKind::Static,
            ),
        };
        let is_protected = protected_plugins.contains(&id.name);
        match index.get(&name).copied() {
            None => {
                index.insert(name, winners.len());
                winners.push(tool.clone());
                winner_sources.push((id, kind, is_protected));
            }
            Some(existing) => {
                let (existing_id, existing_kind, existing_protected) =
                    winner_sources[existing].clone();
                let new_rank = (is_protected, kind.priority());
                let old_rank = (existing_protected, existing_kind.priority());
                if new_rank >= old_rank {
                    // 同优先级时「后写覆盖先写」。
                    shadowed.push(ShadowedTool {
                        tool: name,
                        provider: existing_id,
                        shadowed_by: id.clone(),
                    });
                    winners[existing] = tool.clone();
                    winner_sources[existing] = (id, kind, is_protected);
                } else {
                    shadowed.push(ShadowedTool {
                        tool: name,
                        provider: id,
                        shadowed_by: existing_id,
                    });
                }
            }
        }
    }
    (winners, shadowed)
}

/// 位置参数或关键字参数里的字符串。
fn arg_str<'a>(call: &'a HostCall, index: usize, name: &str) -> Option<&'a str> {
    call.positional_or_kwarg(index, name)
        .and_then(Value::as_str)
}

fn mod_error(name: &str, message: impl Into<String>) -> PluginError {
    PluginError::Load {
        plugin: name.to_string(),
        message: message.into(),
    }
}
