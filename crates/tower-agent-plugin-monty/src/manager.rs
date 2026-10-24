//! JIT 宿主：把 monty 脚本作为 `PluginKind::Jit` 载荷送进 [`PluginRegistry`]，
//! 让静态 / 动态 / JIT 三种形态共用同一套**解析规则**（覆盖 / 依赖 / 版本 / 遮蔽）。
//!
//! 关键点：`ModManager` 持有注册表，工具目录的唯一来源是「解析 + 安装」的结果——
//! 内置静态插件与 JIT mod 一起解析、一起产出工具，JIT（优先级最高）可遮蔽同名静态插件。
//! 每次 mod 增删改都会重新解析并回写 `HostApi` 与工具目录。

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use futures_util::{StreamExt as _, future::BoxFuture};
use monty_fs::{MountCallOutcome, MountMode, MountTable};
use monty_types::OsFunctionCall;
use semver::{Version, VersionReq};
use serde_json::{Value, json};
use tower_agent_core::{
    AgentTool, AgentToolResult, Panel, PanelAnchor, StatusSegment, ToolRenderer, UiState,
};
use tower_agent_plugin_sdk::codemode::{
    Capability, CapabilityError, CapabilitySurface, CodemodeRequest, CodemodeResult, ExecOptions,
    ScriptHookEvent, ScriptHookHandler, ScriptHookRegistry, ScriptLanguage, ScriptPermissions,
    ScriptRuntimeError,
};
use tower_agent_plugin_sdk::storage::{ConfigScope, ScriptRecord, ScriptStore};
use tower_agent_plugin_sdk::{
    CapabilityId, HostApi, Permission, PermissionPolicy, PluginContext, PluginError, PluginId,
    PluginKind, PluginManifest, PluginRegistry, PluginShutdownFailure, ShadowedPlugin,
    StaticPlugin,
};
use tower_agent_provider::{Arguments, ToolCall};

use crate::{
    host::{HostCall, MontyHost},
    plugin::MontyPlugin,
    sandbox::SandboxPool,
    tool::{ModTool, ToolSpec},
    wire,
};

/// 池里默认保留的空闲沙箱数。
const DEFAULT_POOL_SIZE: usize = 4;
static CODEMODE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

/// 一个 mod 的权限（共享能力层里的 `ScriptPermissions`）。
pub type ModPermissions = ScriptPermissions;

/// 写一个 mod 时的选项。
#[derive(Debug, Clone, Default)]
pub struct ModOptions {
    /// 权限（缺省用默认权限）。
    pub permissions: Option<ModPermissions>,
    /// 依赖的宿主能力，形如 `tool.read`（进注册表做依赖校验/拓扑排序）。
    pub requires: Vec<String>,
}

/// Options for writing a JIT plugin to the persistent store.
#[derive(Debug, Clone, Default)]
pub struct ScriptWriteOptions {
    /// Permissions to persist with the script.
    pub permissions: Option<ScriptPermissions>,
    /// Host capabilities required by the script.
    pub requires: Vec<String>,
    /// Script language. The Python manager currently accepts only [`ScriptLanguage::Monty`].
    pub language: Option<ScriptLanguage>,
    /// Whether the script should be loaded on startup.
    pub enabled: Option<bool>,
    /// Explicit storage scope. Defaults to the manager's project scope.
    pub scope: Option<ConfigScope>,
}

/// 一个已加载 JIT 脚本的摘要（用于 `jit_list` / UI 展示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModSummary {
    /// mod 名。
    pub name: String,
    /// 该 mod 当前提供的工具名（按字典序）。
    pub tools: Vec<String>,
    /// 源码字节数。
    pub code_len: usize,
}

/// One effective persisted script, including whether it is currently loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptSummary {
    /// Script name.
    pub name: String,
    /// Interpreter declared by the script.
    pub language: ScriptLanguage,
    /// User or project storage scope.
    pub scope: ConfigScope,
    /// Whether the script is loaded on startup.
    pub enabled: bool,
    /// Source size in bytes.
    pub code_len: usize,
    /// Whether this process currently has the script loaded.
    pub loaded: bool,
    /// Tools currently provided by the loaded script.
    pub tools: Vec<String>,
}

#[derive(Clone)]
struct ScriptStorage {
    store: Arc<dyn ScriptStore>,
    project: ConfigScope,
}

/// mods 宿主。
pub struct ModManager {
    host: HostApi,
    ui: UiState,
    pool: Arc<SandboxPool>,
    capability_surface: Mutex<Option<Arc<dyn CapabilitySurface>>>,
    mounts: Mutex<MountTable>,
    permissions: Mutex<BTreeMap<String, ModPermissions>>,
    /// Runtime grants for capabilities outside the persisted permission set.
    permission_policy: Mutex<PermissionPolicy>,
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
    /// Persistent JIT plugin store and the current project scope.
    script_storage: Mutex<Option<ScriptStorage>>,
    /// Session-scoped script hook registry shared with other codemode engines.
    hook_registry: Mutex<Option<Arc<ScriptHookRegistry>>>,
    closed: AtomicBool,
    self_weak: OnceLock<Weak<ModManager>>,
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
    /// Execute one generated monty/Python program without installing it as a
    /// persistent JIT plugin.
    pub async fn execute_codemode(
        self: &Arc<Self>,
        request: CodemodeRequest,
    ) -> Result<CodemodeResult, ScriptRuntimeError> {
        if !matches!(request.language.as_str(), "python" | "monty") {
            return Err(ScriptRuntimeError::Unsupported {
                mode: tower_agent_plugin_sdk::codemode::ScriptMode::Codemode,
                language: request.language,
            });
        }
        let sequence = CODEMODE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("__codemode_{sequence}");
        self.set_permissions(&name, request.permissions);
        let adapter = Arc::new(ModHostAdapter {
            manager: Arc::downgrade(self),
            plugin: name.clone(),
        });
        let manifest = PluginManifest::new(name.clone(), Version::new(0, 0, 0), PluginKind::Jit);
        let plugin = MontyPlugin::load(manifest, request.source, adapter, Arc::clone(&self.pool))
            .map_err(monty_runtime_error)?;
        let result = match request.timeout_ms {
            Some(timeout_ms) => {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    plugin.run_codemode(plugin.code().as_str()),
                )
                .await
                {
                    Ok(result) => result.map_err(monty_runtime_error),
                    Err(_) => Err(ScriptRuntimeError::Execution {
                        interpreter: "monty".to_owned(),
                        message: format!("codemode timed out after {timeout_ms} ms"),
                    }),
                }
            }
            None => plugin
                .run_codemode(plugin.code().as_str())
                .await
                .map_err(monty_runtime_error),
        };
        let _ = plugin.stop();
        let _ = plugin.unload();
        self.permissions
            .lock()
            .expect("permissions poisoned")
            .remove(&name);
        result.map(|value| CodemodeResult {
            value,
            diagnostics: Vec::new(),
        })
    }

    /// 用共享 UI 状态与初始注册表构造，并立刻解析一次。
    ///
    /// 初始注册表由**宿主组装层**提供（`tower_agent::assembly::HostPlugins`）：内置工具 +
    /// `codemode` 这类宿主能力；`ModManager` 只再叠加自己的 JIT 运行时地基。
    /// `HostApi` 的工具表由解析结果填充，因此宿主能力与工具目录同源。
    pub fn new(ui: UiState, registry: PluginRegistry) -> Result<Arc<Self>, PluginError> {
        let manager = Arc::new(Self {
            host: HostApi::from_tools(Vec::<AgentTool>::new()),
            ui,
            pool: crate::sandbox::new_pool(DEFAULT_POOL_SIZE),
            capability_surface: Mutex::new(None),
            mounts: Mutex::new(MountTable::new()),
            permissions: Mutex::new(BTreeMap::new()),
            permission_policy: Mutex::new(PermissionPolicy::unattended()),
            registry: Mutex::new(registry),
            catalog: Mutex::new(Vec::new()),
            shadows: Mutex::new(Vec::new()),
            shadowed_tools: Mutex::new(Vec::new()),
            mods: Mutex::new(BTreeMap::new()),
            specs: Mutex::new(BTreeMap::new()),
            requires: Mutex::new(BTreeMap::new()),
            kv: Mutex::new(BTreeMap::new()),
            script_storage: Mutex::new(None),
            hook_registry: Mutex::new(None),
            closed: AtomicBool::new(false),
            self_weak: OnceLock::new(),
        });
        let _ = manager.self_weak.set(Arc::downgrade(&manager));
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
        self.ensure_open()?;
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

    /// 注入两个脚本引擎共用的宿主能力面。
    ///
    /// monty 只依赖 `tower-agent-plugin-sdk` 的接口；实际命令执行由宿主实现
    /// （例如 `tower-agent-plugin-rquickjs::ExecCapabilitySurface`），避免 monty → codemode
    /// 的反向依赖。
    pub fn set_capability_surface(&self, surface: Arc<dyn CapabilitySurface>) {
        *self
            .capability_surface
            .lock()
            .expect("capability surface poisoned") = Some(surface);
    }

    /// 移除当前注入的宿主能力面（后续能力调用会明确失败）。
    pub fn clear_capability_surface(&self) {
        *self
            .capability_surface
            .lock()
            .expect("capability surface poisoned") = None;
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

    /// Attach the persistent script store and the current project scope.
    pub fn set_script_store(&self, store: Arc<dyn ScriptStore>, project: ConfigScope) {
        *self.script_storage.lock().expect("script storage poisoned") =
            Some(ScriptStorage { store, project });
    }

    /// Detach persistent script storage; scripts then live only in this process.
    pub fn clear_script_store(&self) {
        *self.script_storage.lock().expect("script storage poisoned") = None;
    }

    fn script_storage(&self) -> Option<ScriptStorage> {
        self.script_storage
            .lock()
            .expect("script storage poisoned")
            .clone()
    }

    /// Current project scope used by authoring tools, when storage is attached.
    #[must_use]
    pub fn script_project_scope(&self) -> Option<ConfigScope> {
        self.script_storage().map(|storage| storage.project)
    }

    /// List effective stored scripts plus process-only scripts.
    pub async fn list_scripts(&self) -> Result<Vec<ScriptSummary>, PluginError> {
        let loaded = self.list();
        let Some(storage) = self.script_storage() else {
            return Ok(loaded
                .into_iter()
                .map(|summary| ScriptSummary {
                    name: summary.name,
                    language: ScriptLanguage::Monty,
                    scope: ConfigScope::User,
                    enabled: true,
                    code_len: summary.code_len,
                    loaded: true,
                    tools: summary.tools,
                })
                .collect());
        };
        let records = effective_script_records(&storage).await?;
        let mut summaries = BTreeMap::new();
        for record in records {
            let Some(language) = ScriptLanguage::parse(&record.language) else {
                eprintln!(
                    "跳过 codemode 脚本 `{}`：未知语言 `{}`",
                    record.name, record.language
                );
                continue;
            };
            let summary = loaded.iter().find(|summary| summary.name == record.name);
            summaries.insert(
                record.name.clone(),
                ScriptSummary {
                    name: record.name,
                    language,
                    scope: record.scope,
                    enabled: record.enabled,
                    code_len: record.source.len(),
                    loaded: summary.is_some(),
                    tools: summary
                        .map(|summary| summary.tools.clone())
                        .unwrap_or_default(),
                },
            );
        }
        for summary in loaded {
            summaries
                .entry(summary.name.clone())
                .or_insert(ScriptSummary {
                    name: summary.name,
                    language: ScriptLanguage::Monty,
                    scope: storage.project.clone(),
                    enabled: true,
                    code_len: summary.code_len,
                    loaded: true,
                    tools: summary.tools,
                });
        }
        Ok(summaries.into_values().collect())
    }

    /// Mark an already loaded script as enabled in persistent storage.
    pub async fn persist_script(
        &self,
        name: &str,
        scope: Option<ConfigScope>,
    ) -> Result<(), PluginError> {
        let Some(storage) = self.script_storage() else {
            return Err(mod_error(name, "script storage is not configured"));
        };
        let plugin = self
            .mods
            .lock()
            .expect("mods poisoned")
            .get(name)
            .cloned()
            .ok_or_else(|| mod_error(name, "unknown mod"))?;
        let requires = self
            .requires
            .lock()
            .expect("mod requires poisoned")
            .get(name)
            .cloned()
            .unwrap_or_default();
        let scope = scope.unwrap_or(storage.project.clone());
        self.persist_script_record(
            &storage,
            scope,
            name,
            &plugin.code(),
            self.permissions(name),
            requires,
            ScriptLanguage::Monty,
            true,
        )
        .await
    }

    /// Remove a script from the selected scope and unload it.
    pub async fn remove_script(
        &self,
        name: &str,
        scope: Option<ConfigScope>,
    ) -> Result<bool, PluginError> {
        let removed = self.remove(name)?;
        let Some(storage) = self.script_storage() else {
            return Ok(removed);
        };
        let scope = scope.unwrap_or(storage.project);
        let existed = storage
            .store
            .get_script(&scope, name)
            .await
            .map_err(|error| mod_error(name, error.to_string()))?
            .is_some();
        if existed {
            storage
                .store
                .delete_script(&scope, name)
                .await
                .map_err(|error| mod_error(name, error.to_string()))?;
        }
        Ok(removed || existed)
    }

    /// Discard a script from the current project and unload it.
    pub async fn discard_script(&self, name: &str) -> Result<bool, PluginError> {
        self.remove_script(name, None).await
    }

    /// Load enabled scripts from storage, with project records overriding user records.
    pub async fn load_persisted(self: &Arc<Self>) -> usize {
        if self.closed.load(Ordering::SeqCst) {
            return 0;
        }
        let Some(storage) = self.script_storage() else {
            return 0;
        };
        let records = match effective_script_records(&storage).await {
            Ok(records) => records,
            Err(error) => {
                eprintln!("加载 codemode 脚本失败：{error}");
                return 0;
            }
        };
        let mut loaded = 0;
        for record in records {
            if !record.enabled {
                continue;
            }
            let Some(language) = ScriptLanguage::parse(&record.language) else {
                eprintln!(
                    "跳过 codemode 脚本 `{}`：未知语言 `{}`",
                    record.name, record.language
                );
                continue;
            };
            if language != ScriptLanguage::Monty {
                eprintln!(
                    "跳过 codemode 脚本 `{}`：Python 宿主不支持 {}",
                    record.name, language
                );
                continue;
            }
            let permissions =
                match serde_json::from_value::<ScriptPermissions>(record.permissions.clone()) {
                    Ok(permissions) => permissions,
                    Err(error) => {
                        eprintln!(
                            "跳过 codemode 脚本 `{}`：权限元数据无效：{error}",
                            record.name
                        );
                        continue;
                    }
                };
            if self
                .mods
                .lock()
                .expect("mods poisoned")
                .contains_key(&record.name)
            {
                continue;
            }
            self.set_permissions(&record.name, permissions);
            let requires = requires_from_record(&record);
            let options = ModOptions {
                permissions: Some(permissions),
                requires,
            };
            match self
                .write_with_options(&record.name, &record.source, options)
                .await
            {
                Ok(_) => loaded += 1,
                Err(error) => eprintln!("加载 codemode 脚本 `{}` 失败：{error}", record.name),
            }
        }
        loaded
    }

    #[allow(clippy::too_many_arguments)]
    async fn persist_script_record(
        &self,
        storage: &ScriptStorage,
        scope: ConfigScope,
        name: &str,
        source: &str,
        permissions: ScriptPermissions,
        requires: Vec<String>,
        language: ScriptLanguage,
        enabled: bool,
    ) -> Result<(), PluginError> {
        let existing = storage
            .store
            .get_script(&scope, name)
            .await
            .map_err(|error| mod_error(name, error.to_string()))?;
        let now = tower_agent_runtime::agent_loop::now_ms();
        let mut metadata = existing
            .as_ref()
            .map(|record| record.metadata.clone())
            .unwrap_or_else(|| json!({}));
        if !metadata.is_object() {
            metadata = json!({});
        }
        metadata["requires"] = json!(requires);
        let permissions = serde_json::to_value(permissions)
            .map_err(|error| mod_error(name, error.to_string()))?;
        let record = ScriptRecord {
            id: existing
                .as_ref()
                .map(|record| record.id.clone())
                .unwrap_or_else(|| new_script_id(name, now)),
            name: name.to_owned(),
            language: language.as_str().to_owned(),
            scope,
            source: source.to_owned(),
            enabled,
            permissions,
            metadata,
            created_at: existing
                .as_ref()
                .map(|record| record.created_at)
                .unwrap_or(now),
            updated_at: now,
            source_hash: source_hash(source),
        };
        storage
            .store
            .upsert_script(record)
            .await
            .map_err(|error| mod_error(name, error.to_string()))
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
        self.ensure_open()?;
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

    /// Replace the session permission policy used for capabilities not persisted
    /// in a script's [`ScriptPermissions`].
    pub fn set_permission_policy(&self, policy: PermissionPolicy) {
        *self
            .permission_policy
            .lock()
            .expect("permission policy poisoned") = policy;
    }

    /// Attach the session hook registry used by script `on(...)` registrations.
    pub fn set_hook_registry(&self, registry: Arc<ScriptHookRegistry>) {
        *self.hook_registry.lock().expect("hook registry poisoned") = Some(registry);
    }

    fn hook_registry(&self) -> Option<Arc<ScriptHookRegistry>> {
        self.hook_registry
            .lock()
            .expect("hook registry poisoned")
            .clone()
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

    fn authorize(&self, script: &str, capability: Capability) -> Result<(), String> {
        if self.permissions(script).allows(capability) {
            return Ok(());
        }
        let capability_id = CapabilityId::new("codemode", script.to_owned());
        let permission = Permission::new(capability.as_str());
        self.permission_policy
            .lock()
            .expect("permission policy poisoned")
            .check(&capability_id, &[permission])
            .map_err(|_| {
                let error = CapabilityError::Unauthorized { capability };
                format!(
                    "{} for JIT plugin `{script}`; permission was not granted",
                    capability_error_text(&error)
                )
            })
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
        self.write_script(name, code, ScriptWriteOptions::default())
            .await
    }

    /// Write a script through the persistent store and install it when enabled.
    pub async fn write_script(
        self: &Arc<Self>,
        name: &str,
        code: &str,
        options: ScriptWriteOptions,
    ) -> Result<ModSummary, PluginError> {
        self.ensure_open()?;
        let ScriptWriteOptions {
            permissions,
            requires,
            language,
            enabled,
            scope,
        } = options;
        let language = language.unwrap_or(ScriptLanguage::Monty);
        if language != ScriptLanguage::Monty {
            return Err(mod_error(
                name,
                format!("the JIT host cannot load a {language} script"),
            ));
        }
        let enabled = enabled.unwrap_or(true);
        let storage = self.script_storage();
        let scope = scope
            .or_else(|| storage.as_ref().map(|storage| storage.project.clone()))
            .unwrap_or(ConfigScope::User);
        let existing = match &storage {
            Some(storage) => storage
                .store
                .get_script(&scope, name)
                .await
                .map_err(|error| mod_error(name, error.to_string()))?,
            None => None,
        };
        let permissions = permissions
            .or_else(|| {
                existing.as_ref().and_then(|record| {
                    serde_json::from_value::<ScriptPermissions>(record.permissions.clone()).ok()
                })
            })
            .unwrap_or_else(|| self.permissions(name));
        let summary = if enabled {
            self.write_with_options(
                name,
                code,
                ModOptions {
                    permissions: Some(permissions),
                    requires: requires.clone(),
                },
            )
            .await?
        } else {
            if self.mods.lock().expect("mods poisoned").contains_key(name) {
                self.remove(name)?;
            }
            ModSummary {
                name: name.to_owned(),
                tools: Vec::new(),
                code_len: code.len(),
            }
        };
        if let Some(storage) = storage {
            self.persist_script_record(
                &storage,
                scope,
                name,
                code,
                permissions,
                requires,
                language,
                enabled,
            )
            .await?;
        }
        Ok(summary)
    }

    /// 写一个 mod，并指定权限与依赖。
    pub async fn write_with_options(
        self: &Arc<Self>,
        name: &str,
        code: &str,
        options: ModOptions,
    ) -> Result<ModSummary, PluginError> {
        self.ensure_open()?;
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
        self.write_script(name, code, ScriptWriteOptions::default())
            .await
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
        if let Some(registry) = self.hook_registry() {
            registry.unregister_owner(&format!("monty:{name}"));
        }
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
            Ok(()) => Ok(self.summary(name)),
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
        if let Some(registry) = self.hook_registry() {
            registry.unregister_owner(&format!("monty:{name}"));
        }
        let _ = plugin.stop();
        let _ = plugin.unload();
        self.resolve_and_apply()?;
        Ok(true)
    }

    /// 停止 JIT 与动态插件，释放宿主后台资源。
    ///
    /// 调用方应先完成 agent 请求排空。本方法幂等，返回全部插件关闭失败。
    pub fn shutdown(&self) -> Vec<PluginShutdownFailure> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Vec::new();
        }
        let names = self
            .mods
            .lock()
            .expect("mods poisoned")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut failures = Vec::new();
        for name in names {
            if let Err(error) = self.remove(&name) {
                failures.push(PluginShutdownFailure {
                    plugin: name,
                    message: error.to_string(),
                });
            }
        }
        failures.extend(self.registry.lock().expect("registry poisoned").shutdown());
        self.host.replace_tools(Vec::<AgentTool>::new());
        self.catalog.lock().expect("catalog poisoned").clear();
        failures
    }

    fn ensure_open(&self) -> Result<(), PluginError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(PluginError::Install {
                plugin: "agent-monty".to_owned(),
                message: "JIT runtime is shutting down".to_owned(),
            });
        }
        Ok(())
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
            if let Some(registry) = self.hook_registry() {
                registry.unregister_owner(&format!("monty:{name}"));
            }
            return Err(error);
        }
        if let Err(error) = self.check_shadowing(name) {
            self.take_specs(name);
            if let Some(registry) = self.hook_registry() {
                registry.unregister_owner(&format!("monty:{name}"));
            }
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
            if let Some(registry) = self.hook_registry() {
                registry.unregister_owner(&format!("monty:{name}"));
            }
            return Err(error);
        }
        self.mods
            .lock()
            .expect("mods poisoned")
            .insert(name.to_string(), plugin);
        Ok(self.summary(name))
    }

    /// 阻断会遮蔽「JIT 运行时地基」或其它 mod 工具的 JIT 插件。
    ///
    /// - 插件名 / 工具名命中保留名（`jit_write` 等）→ 阻断；
    /// - 工具名与**另一个 mod** 提供的工具重名 → 阻断（JIT 不能遮蔽 JIT 的基础设施）。
    fn check_shadowing(&self, name: &str) -> Result<(), PluginError> {
        let protected_tools: BTreeSet<String> = {
            let registry = self.registry.lock().expect("registry poisoned");
            // 受保护插件名（JIT 运行时地基 + codemode 等宿主能力）统一走注册表规则。
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
        match call.function.as_str() {
            "exec" | "__codemode_exec" => {
                self.authorize(&call.plugin, Capability::Exec)?;
                self.host_exec(&call).await
            }
            "tool_call" | "tool_list" => {
                self.authorize(&call.plugin, Capability::Tools)?;
                if call.function == "tool_list" {
                    Ok(self.tool_names())
                } else {
                    self.host_tool_call(&call).await
                }
            }
            "tool_register" | "tool_unregister" => {
                self.authorize(&call.plugin, Capability::RegisterTools)?;
                if call.function == "tool_register" {
                    self.host_tool_register(&call)
                } else {
                    self.host_tool_unregister(&call)
                }
            }
            "on" | "register_hook" => {
                self.authorize(&call.plugin, Capability::Hooks)?;
                if !call.setup {
                    return Ok(Value::Null);
                }
                let event = arg_str(&call, 0, "event")
                    .and_then(ScriptHookEvent::parse)
                    .ok_or("on requires event `tool_call` or `tool_result`")?;
                let function =
                    arg_str(&call, 1, "handler").ok_or("on requires (event, handler)")?;
                if !is_python_identifier(function) {
                    return Err(format!(
                        "hook handler `{function}` must be a Python identifier"
                    ));
                }
                let registry = self
                    .hook_registry()
                    .ok_or("script hook registry is not configured by the host")?;
                registry.register(
                    format!("monty:{}", call.plugin),
                    event,
                    Arc::new(MontyScriptHook {
                        manager: self.self_weak.get().cloned().unwrap_or_default(),
                        plugin: call.plugin.clone(),
                        function: function.to_owned(),
                    }),
                );
                Ok(Value::Null)
            }
            "log" => {
                self.authorize(&call.plugin, Capability::State)?;
                let message = arg_str(&call, 0, "message").unwrap_or_default();
                eprintln!("[mod {}] {message}", call.plugin);
                Ok(Value::Null)
            }
            "state_set" => {
                self.authorize(&call.plugin, Capability::State)?;
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
                self.authorize(&call.plugin, Capability::State)?;
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
                self.authorize(&call.plugin, Capability::Ui)?;
                serde_json::to_value(self.ui.snapshot()).map_err(|error| error.to_string())
            }
            name if name.starts_with("ui_") => {
                self.authorize(&call.plugin, Capability::Ui)?;
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
        if let Err(error) = self.authorize(plugin, Capability::Fs) {
            return wire::permission_denied(error).to_string();
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

    async fn host_exec(&self, call: &HostCall) -> Result<Value, String> {
        let name = arg_str(call, 0, "name").ok_or("exec requires (name, args, opts)")?;
        let args = match call.positional_or_kwarg(1, "args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(values)) => values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "exec args must be an array of strings".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(other) => return Err(format!("exec args must be an array, got {other}")),
        };
        let options = match call.positional_or_kwarg(2, "opts") {
            None | Some(Value::Null) => ExecOptions::default(),
            Some(value) => serde_json::from_value(value.clone())
                .map_err(|error| format!("invalid exec options: {error}"))?,
        };
        let surface = self
            .capability_surface
            .lock()
            .expect("capability surface poisoned")
            .clone()
            .ok_or_else(|| "exec capability is not configured by the host".to_owned())?;
        surface
            .authorize(Capability::Exec)
            .map_err(|error| capability_error_text(&error))?;
        let result = surface
            .exec(name, args, options)
            .await
            .map_err(|error| capability_error_text(&error))?;
        serde_json::to_value(result).map_err(|error| error.to_string())
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

    fn shutdown(&self) -> Result<(), PluginError> {
        self.plugin.stop()
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

struct MontyScriptHook {
    manager: Weak<ModManager>,
    plugin: String,
    function: String,
}

impl ScriptHookHandler for MontyScriptHook {
    fn invoke(
        &self,
        _event: ScriptHookEvent,
        payload: Value,
    ) -> BoxFuture<'static, Result<Value, String>> {
        let manager = self.manager.upgrade();
        let plugin = self.plugin.clone();
        let function = self.function.clone();
        Box::pin(async move {
            let manager = manager.ok_or_else(|| "JIT host is gone".to_owned())?;
            let plugin = manager
                .mods
                .lock()
                .expect("mods poisoned")
                .get(&plugin)
                .cloned()
                .ok_or_else(|| format!("JIT plugin `{plugin}` is unloaded"))?;
            plugin
                .call_function(&function, &payload)
                .await
                .map_err(|error| error.to_string())
        })
    }
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

fn monty_runtime_error(error: PluginError) -> ScriptRuntimeError {
    ScriptRuntimeError::Execution {
        interpreter: "monty".to_owned(),
        message: error.to_string(),
    }
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

async fn effective_script_records(
    storage: &ScriptStorage,
) -> Result<Vec<ScriptRecord>, PluginError> {
    let mut records = BTreeMap::new();
    for record in storage
        .store
        .list_scripts(&ConfigScope::User)
        .await
        .map_err(|error| mod_error("<script-store>", error.to_string()))?
    {
        records.insert(record.name.clone(), record);
    }
    for record in storage
        .store
        .list_scripts(&storage.project)
        .await
        .map_err(|error| mod_error("<script-store>", error.to_string()))?
    {
        records.insert(record.name.clone(), record);
    }
    Ok(records.into_values().collect())
}

fn requires_from_record(record: &ScriptRecord) -> Vec<String> {
    record
        .metadata
        .get("requires")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn source_hash(source: &str) -> String {
    format!("{:016x}", fnv1a(source.as_bytes()))
}

fn new_script_id(name: &str, now: i64) -> String {
    format!("{now:x}-{:016x}", fnv1a(name.as_bytes()))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 位置参数或关键字参数里的字符串。
fn arg_str<'a>(call: &'a HostCall, index: usize, name: &str) -> Option<&'a str> {
    call.positional_or_kwarg(index, name)
        .and_then(Value::as_str)
}

fn capability_error_text(error: &CapabilityError) -> String {
    serde_json::to_string(error).unwrap_or_else(|_| error.to_string())
}

fn mod_error(name: &str, message: impl Into<String>) -> PluginError {
    PluginError::Load {
        plugin: name.to_string(),
        message: message.into(),
    }
}

fn is_python_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}
