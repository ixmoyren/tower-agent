//! CLI 配置的持久化：把 pi 的 `auth.json`（provider 凭据）与 `settings.json`
//! （默认 provider / 默认模型 / 默认思考级别）落到
//! [`tower_agent_plugin_sdk::storage`]，默认插件后端是
//! [`tower_agent_plugin_turso`]（`~/.local/share/tower-agent/storage.db`）。
//!
//! 对齐 pi 的两份文件：
//!
//! | pi 文件                                   | 本模块                        |
//! |-------------------------------------------|-------------------------------|
//! | `~/.pi/agent/auth.json`（`{provider:{…}}`）| [`ProviderStore`] 里的凭据     |
//! | `~/.pi/agent/settings.json`               | [`ConfigStore`] 里同名 key     |
//!
//! 取值优先级对齐 pi：命令行（`--provider` / `--api-key` / `--model` / `--thinking`）
//! 覆盖一切；其次环境变量；再次存储里的配置。启动阶段用 [`CliStore::snapshot`] 一次性
//! 读出来；运行期由斜杠命令（`/login` `/model` `/thinking`）写回，写入是
//! fire-and-forget，退出前用 [`CliStore::flush`] 确保落盘。

use std::{
    collections::BTreeSet,
    future::Future,
    io::{BufRead as _, IsTerminal as _, Write as _},
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;
use tower_agent_core::AgentMessage;
use tower_agent_plugin_sdk::storage::{
    ConfigScope, ProviderCredential, ScriptRecord, ScriptStore, SessionRecord, Storage,
    StorageBackendRegistration, StorageError, StorageFuture, StorageManager,
};
use tower_agent_plugin_turso::TursoStorage;
use tower_agent_provider::{Message, UserContent};

/// pi `settings.json` 的 `defaultProvider`。
pub const DEFAULT_PROVIDER: &str = "defaultProvider";
/// pi `settings.json` 的 `defaultModel`。
pub const DEFAULT_MODEL: &str = "defaultModel";
/// pi `settings.json` 的 `defaultThinkingLevel`。
pub const DEFAULT_THINKING_LEVEL: &str = "defaultThinkingLevel";
/// codemode 的进程级默认引擎（`monty` / `js`）。
pub const CODEMODE_ENGINE: &str = "codemode.engine";

/// 压缩开关（pi `settings.json` 的 `compaction.enabled`）。
pub const COMPACTION_ENABLED: &str = "compaction.enabled";
/// 压缩预留 token（pi `compaction.reserveTokens`）。
pub const COMPACTION_RESERVE_TOKENS: &str = "compaction.reserveTokens";
/// 压缩保留窗口（pi `compaction.keepRecentTokens`）。
pub const COMPACTION_KEEP_RECENT_TOKENS: &str = "compaction.keepRecentTokens";

/// 覆盖存储数据库路径的环境变量（测试与自定义位置用）。
pub const STORAGE_PATH_ENV: &str = "TOWER_AGENT_STORAGE";
/// 覆盖 storage 控制配置路径的环境变量（测试与隔离环境用）。
pub const STORAGE_CONTROL_ENV: &str = "TOWER_AGENT_STORAGE_CONTROL";

/// 与具体 backend 数据分离的 storage 选择配置。
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageControl {
    /// 当前活跃的 backend；尚未选择时为空。
    #[serde(default)]
    pub active: Option<String>,
    /// 用户停用的 backend；重启后仍然生效。
    #[serde(default)]
    pub disabled: BTreeSet<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StorageControlFile {
    #[serde(default)]
    storage: StorageControl,
}

struct SelectedBackend {
    name: String,
    persist: bool,
}

/// 启动时一次性读到的配置快照。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StoreSnapshot {
    /// `settings.json` 的 `defaultProvider`。
    pub default_provider: Option<String>,
    /// `settings.json` 的 `defaultModel`。
    pub default_model: Option<String>,
    /// `settings.json` 的 `defaultThinkingLevel`。
    pub default_thinking_level: Option<String>,
    /// 有效配置里的默认 codemode 引擎。
    pub codemode_engine: Option<String>,
    /// `auth.json` 里的全部 provider 凭据。
    pub credentials: Vec<ProviderCredential>,
}

impl StoreSnapshot {
    /// 某个 provider 的存储凭据。
    #[must_use]
    pub fn credential(&self, provider: &str) -> Option<&ProviderCredential> {
        self.credentials
            .iter()
            .find(|credential| credential.provider == provider)
    }
}

/// CLI 的配置存储句柄。
///
/// 内部只持有一个当前活跃 storage backend。
pub struct CliStore {
    storage: Arc<dyn Storage>,
    manager: Option<Arc<StorageManager>>,
    handle: Handle,
    /// 尚未完成的写入任务；[`CliStore::flush`] 先等它们入队再落盘，保证「写后 flush」不丢写。
    inflight: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for CliStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliStore").finish_non_exhaustive()
    }
}

impl CliStore {
    /// 打开默认位置的存储（`TOWER_AGENT_STORAGE` 可覆盖路径）。
    pub fn open(runtime: &Runtime) -> Result<Self, StorageError> {
        Self::open_at(runtime, storage_path())
    }

    /// 打开指定路径的存储。
    pub fn open_at(runtime: &Runtime, path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_for_cli(runtime, path, None, std::io::stdin().is_terminal())
    }

    /// 按控制配置选择 backend 并打开 CLI 存储。
    pub fn open_for_cli(
        runtime: &Runtime,
        path: impl AsRef<Path>,
        requested: Option<&str>,
        interactive: bool,
    ) -> Result<Self, StorageError> {
        let backend = runtime.block_on(TursoStorage::open(path))?;
        Self::open_with_backends(
            runtime,
            vec![backend.plugin().registration()],
            storage_control_path(),
            requested,
            interactive,
        )
    }

    /// 从宿主发现的一组 backend 中按控制配置选择一个。
    ///
    /// 多 backend 且没有既有选择时，交互终端会让用户选择；非交互调用返回
    /// [`StorageError::Selection`]，其中包含候选列表。
    pub fn open_with_backends(
        runtime: &Runtime,
        backends: Vec<StorageBackendRegistration>,
        control_path: impl AsRef<Path>,
        requested: Option<&str>,
        interactive: bool,
    ) -> Result<Self, StorageError> {
        let control_path = control_path.as_ref();
        let mut control = load_storage_control(control_path)?;
        let selected = select_backend(&control, &backends, requested, interactive)?;
        if selected.persist {
            control.active = Some(selected.name.clone());
            save_storage_control(control_path, &control)?;
        }
        let names = backends
            .iter()
            .map(|backend| backend.name.clone())
            .collect::<BTreeSet<_>>();
        let manager = Arc::new(StorageManager::new(backends, selected.name)?);
        runtime.block_on(async {
            for name in control.disabled.iter().filter(|name| names.contains(*name)) {
                manager.set_backend_enabled(name, false).await?;
            }
            Ok::<_, StorageError>(())
        })?;
        Ok(Self::from_manager(manager, runtime.handle().clone()))
    }

    /// 打开纯内存存储（测试用）。
    pub fn in_memory(runtime: &Runtime) -> Result<Self, StorageError> {
        let storage = runtime.block_on(TursoStorage::in_memory())?;
        Ok(Self::from_storage(storage, runtime.handle().clone()))
    }

    /// 用现成的后端与运行时句柄构造（测试也走这个）。
    #[must_use]
    pub fn from_storage<T>(storage: T, handle: Handle) -> Self
    where
        T: Storage,
    {
        Self {
            storage: Arc::new(storage),
            manager: None,
            handle,
            inflight: Mutex::new(Vec::new()),
        }
    }

    /// Use the single active backend owned by a [`StorageManager`].
    #[must_use]
    pub fn from_manager(storage: Arc<StorageManager>, handle: Handle) -> Self {
        Self {
            storage: Arc::clone(&storage) as Arc<dyn Storage>,
            manager: Some(storage),
            handle,
            inflight: Mutex::new(Vec::new()),
        }
    }

    /// 由多 backend 管理器支撑时返回它，供上层执行排他式迁移切换。
    #[must_use]
    pub fn manager(&self) -> Option<&Arc<StorageManager>> {
        self.manager.as_ref()
    }

    /// 读一份启动配置快照：默认项取「项目覆盖用户」的有效值，凭据取全量。
    pub async fn snapshot(&self, project: Option<&str>) -> StoreSnapshot {
        StoreSnapshot {
            default_provider: self.effective_string(project, DEFAULT_PROVIDER).await,
            default_model: self.effective_string(project, DEFAULT_MODEL).await,
            default_thinking_level: self.effective_string(project, DEFAULT_THINKING_LEVEL).await,
            codemode_engine: self.effective_string(project, CODEMODE_ENGINE).await,
            credentials: self.storage.list_providers().await.unwrap_or_default(),
        }
    }

    /// 读取某个作用域下的一项配置。
    pub async fn get_config(&self, scope: &ConfigScope, key: &str) -> Option<Value> {
        self.storage.get_config(scope, key).await.unwrap_or(None)
    }

    /// 列出某个作用域下的所有 key。
    pub async fn list_config(&self, scope: &ConfigScope) -> Vec<String> {
        self.storage.list_config(scope).await.unwrap_or_default()
    }

    /// 写入一项配置（入队后返回；退出前 [`CliStore::flush`] 确保落盘）。
    pub async fn set_config(
        &self,
        scope: &ConfigScope,
        key: impl Into<String>,
        value: Value,
    ) -> Result<(), StorageError> {
        let key = key.into();
        self.storage.set_config(scope, &key, value).await
    }

    /// 删除一项配置。
    pub async fn delete_config(&self, scope: &ConfigScope, key: &str) -> Result<(), StorageError> {
        self.storage.delete_config(scope, key).await
    }

    /// 读某 key 的有效值（项目级覆盖用户级）。
    pub async fn resolve_config(&self, project: Option<&str>, key: &str) -> Option<Value> {
        tower_agent_plugin_sdk::storage::resolve_config(&*self.storage, project, key)
            .await
            .unwrap_or(None)
    }

    /// 列出「项目覆盖用户」后的有效配置（按 key 排序）。
    pub async fn effective_config(&self, project: Option<&str>) -> Vec<(String, Value)> {
        tower_agent_plugin_sdk::storage::effective_config(&*self.storage, project)
            .await
            .unwrap_or_default()
    }

    /// 列出某个作用域下的全部 `(key, value)`（不合并）。
    pub async fn scoped_entries(&self, scope: &ConfigScope) -> Vec<(String, Value)> {
        let mut entries = Vec::new();
        for key in self.list_config(scope).await {
            if let Some(value) = self.get_config(scope, &key).await {
                entries.push((key, value));
            }
        }
        entries
    }

    /// 读一个会话的 transcript（不存在时返回空）。
    pub async fn load_transcript(&self, session_id: &str) -> Vec<AgentMessage> {
        self.storage
            .load_messages(session_id)
            .await
            .unwrap_or_default()
    }

    /// 暴露脚本仓库接口给 codemode 宿主；写入完成后等待 Turso 队列排空。
    #[must_use]
    pub fn script_store(self: &Arc<Self>) -> Arc<dyn ScriptStore> {
        Arc::new(CliScriptStore {
            store: Arc::clone(self),
        })
    }

    /// 写 / 覆盖一个会话的元数据。
    pub async fn upsert_session(&self, record: SessionRecord) -> Result<(), StorageError> {
        self.storage.upsert_session(record).await
    }

    /// 向一个会话追加一条消息。
    pub async fn append_message(
        &self,
        session_id: &str,
        message: AgentMessage,
    ) -> Result<(), StorageError> {
        self.storage.append_message(session_id, message).await
    }

    /// 某个项目（工作目录）下的全部会话，按 `updated_at` 倒序。
    pub async fn list_sessions(&self, project: &str) -> Vec<SessionRecord> {
        self.storage
            .list_sessions()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|record| record.metadata.get("cwd").and_then(Value::as_str) == Some(project))
            .collect()
    }

    /// 某个项目下最近更新的会话（`--continue` 用）。
    pub async fn latest_session(&self, project: &str) -> Option<SessionRecord> {
        self.list_sessions(project).await.into_iter().next()
    }

    /// 按 id 取一个会话记录。
    pub async fn session_record(&self, id: &str) -> Option<SessionRecord> {
        self.storage.get_session(id).await.unwrap_or(None)
    }

    /// 读一项有效字符串配置（非字符串或缺席都返回 `None`）。
    async fn effective_string(&self, project: Option<&str>, key: &str) -> Option<String> {
        match self.resolve_config(project, key).await {
            Some(Value::String(value)) => Some(value),
            _ => None,
        }
    }

    /// 读上下文压缩设置（缺省值对齐 pi）。
    pub async fn compaction_settings(
        &self,
        project: Option<&str>,
    ) -> tower_agent_runtime::CompactionSettings {
        let mut settings = tower_agent_runtime::CompactionSettings::default();
        if let Some(enabled) = self
            .resolve_config(project, COMPACTION_ENABLED)
            .await
            .and_then(|value| value.as_bool())
        {
            settings.enabled = enabled;
        }
        if let Some(reserve) = self
            .resolve_config(project, COMPACTION_RESERVE_TOKENS)
            .await
            .and_then(|value| value.as_u64())
        {
            settings.reserve_tokens = reserve;
        }
        if let Some(keep) = self
            .resolve_config(project, COMPACTION_KEEP_RECENT_TOKENS)
            .await
            .and_then(|value| value.as_u64())
        {
            settings.keep_recent_tokens = keep;
        }
        settings
    }

    /// fire-and-forget 写入一项配置（`CommandHost` 的同步回调用）。
    pub fn queue_set_config(&self, scope: ConfigScope, key: impl Into<String>, value: Value) {
        let storage = Arc::clone(&self.storage);
        let key = key.into();
        self.spawn(async move {
            let _ = storage.set_config(&scope, &key, value).await;
        });
    }

    /// fire-and-forget 写入 / 覆盖一个 provider 凭据。
    pub fn queue_put_credential(&self, credential: ProviderCredential) {
        let storage = Arc::clone(&self.storage);
        self.spawn(async move {
            let _ = storage.put_provider(credential).await;
        });
    }

    /// 等待写队列落盘（进程退出前调用）。
    pub async fn flush(&self) -> Result<(), StorageError> {
        // 先等所有 fire-and-forget 写入把语句推入后端队列，再 flush；否则 flush 可能
        // 早于它们入队，导致「刚写的配置没落盘」。
        let pending = {
            let mut inflight = self.inflight.lock().expect("store inflight poisoned");
            std::mem::take(&mut *inflight)
        };
        for task in pending {
            let _ = task.await;
        }
        self.storage.flush().await
    }

    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut inflight = self.inflight.lock().expect("store inflight poisoned");
        // 顺手清掉已完成的任务，避免长会话里无限增长。
        inflight.retain(|task| !task.is_finished());
        inflight.push(self.handle.spawn(future));
    }
}

/// `ScriptStore` 适配器：把 CLI 的写队列语义暴露给 codemode。
///
/// Turso 的普通写入是 fire-and-forget；作者工具必须在返回前让 upsert/delete 可见，
/// 否则紧接着的 `codemode_list` 会读到旧状态，后台错误也会被吞掉。
struct CliScriptStore {
    store: Arc<CliStore>,
}

impl ScriptStore for CliScriptStore {
    fn upsert_script(&self, script: ScriptRecord) -> StorageFuture<'_, ()> {
        Box::pin(async move {
            self.store.storage.upsert_script(script).await?;
            self.store.storage.flush().await
        })
    }

    fn get_script(
        &self,
        scope: &ConfigScope,
        name: &str,
    ) -> StorageFuture<'_, Option<ScriptRecord>> {
        let scope = scope.clone();
        let name = name.to_owned();
        Box::pin(async move { self.store.storage.get_script(&scope, &name).await })
    }

    fn list_scripts(&self, scope: &ConfigScope) -> StorageFuture<'_, Vec<ScriptRecord>> {
        let scope = scope.clone();
        Box::pin(async move { self.store.storage.list_scripts(&scope).await })
    }

    fn delete_script(&self, scope: &ConfigScope, name: &str) -> StorageFuture<'_, ()> {
        let scope = scope.clone();
        let name = name.to_owned();
        Box::pin(async move {
            self.store.storage.delete_script(&scope, &name).await?;
            self.store.storage.flush().await
        })
    }

    fn list_script_scopes(&self) -> StorageFuture<'_, Vec<ConfigScope>> {
        Box::pin(async move { self.store.storage.list_script_scopes().await })
    }
}

/// storage 控制配置路径：`TOWER_AGENT_STORAGE_CONTROL` 优先，否则放在用户配置目录。
#[must_use]
pub fn storage_control_path() -> PathBuf {
    match std::env::var_os(STORAGE_CONTROL_ENV) {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tower-agent")
            .join("storage.toml"),
    }
}

/// 读取 storage 控制配置；文件不存在时返回默认值。
pub fn load_storage_control(path: impl AsRef<Path>) -> Result<StorageControl, StorageError> {
    let path = path.as_ref();
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StorageControl::default());
        }
        Err(error) => {
            return Err(StorageError::Backend(format!(
                "read storage control {}: {error}",
                path.display()
            )));
        }
    };
    toml::from_str::<StorageControlFile>(&source)
        .map(|file| file.storage)
        .map_err(|error| {
            StorageError::Serialization(format!(
                "parse storage control {}: {error}",
                path.display()
            ))
        })
}

/// 原子写入 storage 控制配置（临时文件 → fsync → rename → 目录 fsync）。
pub fn save_storage_control(
    path: impl AsRef<Path>,
    control: &StorageControl,
) -> Result<(), StorageError> {
    let path = path.as_ref();
    let source = toml::to_string_pretty(&StorageControlFile {
        storage: control.clone(),
    })
    .map_err(|error| StorageError::Serialization(error.to_string()))?;
    atomic_write(path, source.as_bytes()).map_err(|error| {
        StorageError::Backend(format!("write storage control {}: {error}", path.display()))
    })
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let previous = match std::fs::read(path) {
        Ok(previous) => Some(previous),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    stage_and_rename(path, parent, bytes)?;
    if let Err(sync_error) = sync_directory(parent) {
        // The rename already happened but durability could not be confirmed.
        // Restore the old selection so an aborted switch cannot leave the
        // control file pointing at a backend that was never committed.
        let restore = match &previous {
            Some(previous) => stage_and_rename(path, parent, previous),
            None => std::fs::remove_file(path),
        };
        let _ = sync_directory(parent);
        return match restore {
            Ok(()) => Err(sync_error),
            Err(restore_error) => Err(std::io::Error::other(format!(
                "directory sync failed ({sync_error}) and selection rollback failed ({restore_error})"
            ))),
        };
    }
    Ok(())
}

fn stage_and_rename(path: &Path, parent: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("storage.toml");
    let temp = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn select_backend(
    control: &StorageControl,
    backends: &[StorageBackendRegistration],
    requested: Option<&str>,
    interactive: bool,
) -> Result<SelectedBackend, StorageError> {
    let mut names = BTreeSet::new();
    for backend in backends {
        if !names.insert(backend.name.clone()) {
            return Err(StorageError::Migration(format!(
                "duplicate storage backend name `{}`",
                backend.name
            )));
        }
    }
    let enabled = names
        .difference(&control.disabled)
        .cloned()
        .collect::<Vec<_>>();
    let candidates = enabled.join(" / ");

    if let Some(active) = &control.active {
        if control.disabled.contains(active) || !names.contains(active) {
            return Err(StorageError::Selection(format!(
                "configured active backend `{active}` is unavailable (enabled candidates: {candidates})"
            )));
        }
        if let Some(requested) = requested
            && requested != active
        {
            return Err(StorageError::Selection(format!(
                "`--storage {requested}` conflicts with active backend `{active}`; use `tower-agent storage switch {requested}` to migrate"
            )));
        }
        return Ok(SelectedBackend {
            name: active.clone(),
            persist: false,
        });
    }

    if let Some(requested) = requested {
        if control.disabled.contains(requested) || !names.contains(requested) {
            return Err(StorageError::Selection(format!(
                "storage backend `{requested}` is unavailable (enabled candidates: {candidates})"
            )));
        }
        return Ok(SelectedBackend {
            name: requested.to_owned(),
            persist: enabled.len() > 1,
        });
    }

    match enabled.as_slice() {
        [only] => Ok(SelectedBackend {
            name: only.clone(),
            persist: true,
        }),
        [] => Err(StorageError::Selection(
            "no enabled storage backend is available".to_owned(),
        )),
        _ if interactive => Ok(SelectedBackend {
            name: prompt_backend(&enabled)?,
            persist: true,
        }),
        _ => Err(StorageError::Selection(format!(
            "multiple storage backends are available ({candidates}); pass `--storage <name>`"
        ))),
    }
}

fn prompt_backend(candidates: &[String]) -> Result<String, StorageError> {
    eprintln!("发现多个 storage backend，请选择一个：");
    for (index, candidate) in candidates.iter().enumerate() {
        eprintln!("  {}. {candidate}", index + 1);
    }
    let stdin = std::io::stdin();
    loop {
        eprint!("输入序号：");
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        if stdin
            .lock()
            .read_line(&mut input)
            .map_err(|error| StorageError::Backend(format!("read selection: {error}")))?
            == 0
        {
            return Err(StorageError::Selection(
                "storage selection was cancelled".to_owned(),
            ));
        }
        if let Ok(index) = input.trim().parse::<usize>()
            && let Some(candidate) = candidates.get(index.saturating_sub(1))
            && index > 0
        {
            return Ok(candidate.clone());
        }
        eprintln!("无效序号，请输入 1..={}。", candidates.len());
    }
}

/// 存储数据库路径：`TOWER_AGENT_STORAGE` 优先，否则 Turso 的默认位置。
#[must_use]
pub fn storage_path() -> PathBuf {
    match std::env::var_os(STORAGE_PATH_ENV) {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => TursoStorage::default_path(),
    }
}

/// 当前工作目录对应的**项目 id**（与 [`ConfigScope::from_working_dir`] 一套规范化）。
#[must_use]
pub fn project_id(cwd: impl AsRef<Path>) -> String {
    ConfigScope::from_working_dir(cwd)
        .project_id()
        .unwrap_or_default()
        .to_owned()
}

/// 会话 transcript 的持久化句柄：项目作用域 + 当前会话 id + 已落盘条数。
///
/// 会话按项目（工作目录）归属，元数据写在 `session.metadata.cwd`；`sync` 只追加
/// 「还没落盘的尾巴」，因此可以在一轮结束时无脑调用（幂等）。
pub struct Transcript {
    store: Arc<CliStore>,
    project: String,
    state: Mutex<TranscriptState>,
}

struct TranscriptState {
    id: String,
    created_at: i64,
    /// 已经写进 `HistoryStore` 的消息条数。
    persisted: usize,
}

impl Transcript {
    /// 续接（`id = Some`）或新开（`id = None`）一个会话；`persisted` 是续接时已存在的条数。
    #[must_use]
    pub fn new(
        store: Arc<CliStore>,
        project: impl Into<String>,
        id: Option<String>,
        persisted: usize,
    ) -> Self {
        Self {
            store,
            project: project.into(),
            state: Mutex::new(TranscriptState {
                id: id.unwrap_or_else(new_session_id),
                created_at: now_ms(),
                persisted,
            }),
        }
    }

    /// 当前会话 id。
    #[must_use]
    pub fn id(&self) -> String {
        self.state.lock().expect("transcript poisoned").id.clone()
    }

    /// `/new`：换一个新会话 id，从空白 transcript 重新计数。
    pub fn reset(&self) {
        let mut state = self.state.lock().expect("transcript poisoned");
        state.id = new_session_id();
        state.created_at = now_ms();
        state.persisted = 0;
    }

    /// 把 `messages` 里尚未落盘的尾巴追加进存储，并刷新会话元数据。
    pub async fn sync(
        &self,
        messages: &[AgentMessage],
        model: Option<&str>,
        compaction: Option<&tower_agent_runtime::Compaction>,
    ) -> Result<(), StorageError> {
        let (id, created_at, skip) = {
            let state = self.state.lock().expect("transcript poisoned");
            (state.id.clone(), state.created_at, state.persisted)
        };
        let mut metadata = serde_json::json!({ "cwd": self.project });
        if let Some(compaction) = compaction
            && let Ok(value) = serde_json::to_value(compaction)
        {
            metadata["compaction"] = value;
        }
        self.store
            .upsert_session(SessionRecord {
                id: id.clone(),
                title: session_title(messages),
                model: model.map(str::to_owned),
                created_at,
                updated_at: now_ms(),
                metadata,
            })
            .await?;
        if skip < messages.len() {
            for message in &messages[skip..] {
                self.store.append_message(&id, message.clone()).await?;
            }
            self.state.lock().expect("transcript poisoned").persisted = messages.len();
        }
        Ok(())
    }
}

/// 从会话记录里读回持久化的压缩状态（续接会话时用，避免重新摘要）。
#[must_use]
pub fn compaction_from_record(record: &SessionRecord) -> Option<tower_agent_runtime::Compaction> {
    let value = record.metadata.get("compaction")?.clone();
    serde_json::from_value(value).ok()
}

/// 新会话 id：毫秒时间戳 + 进程内自增序号（可读、够唯一）。
fn new_session_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!("{:x}-{:x}", now_ms(), SEQ.fetch_add(1, Ordering::Relaxed))
}

/// 会话标题：取第一条用户消息的文本，截到 60 个字符。
fn session_title(messages: &[AgentMessage]) -> Option<String> {
    messages.iter().find_map(|message| match message {
        AgentMessage::Message(Message::User(user)) => match &user.content {
            UserContent::Text(text) => Some(truncate_title(text)),
            _ => None,
        },
        _ => None,
    })
}

fn truncate_title(text: &str) -> String {
    let text = text.trim();
    let mut title: String = text.chars().take(60).collect();
    if text.chars().count() > 60 {
        title.push('…');
    }
    title
}

fn now_ms() -> i64 {
    tower_agent_runtime::agent_loop::now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;
    use tempfile::TempDir;
    use tower_agent_plugin_sdk::storage::StoragePlugin;
    use tower_agent_provider::UserMessage;

    fn backend(name: &str, storage: Arc<dyn Storage>) -> StorageBackendRegistration {
        StoragePlugin::new(name, Version::new(1, 0, 0), storage).registration()
    }

    fn open_backend(runtime: &Runtime) -> Arc<dyn Storage> {
        let storage = runtime
            .block_on(TursoStorage::in_memory())
            .expect("open in-memory backend");
        Arc::new(storage)
    }

    #[test]
    fn single_backend_is_selected_without_configuration() {
        let runtime = Runtime::new().expect("runtime");
        let dir = TempDir::new().expect("temp dir");
        let store = CliStore::open_with_backends(
            &runtime,
            vec![backend("only", open_backend(&runtime))],
            dir.path().join("storage.toml"),
            None,
            false,
        )
        .expect("single backend");
        assert_eq!(
            runtime.block_on(store.manager().expect("manager").active_backend()),
            "only"
        );
        assert_eq!(
            load_storage_control(dir.path().join("storage.toml"))
                .expect("load control")
                .active
                .as_deref(),
            Some("only")
        );
    }

    #[test]
    fn multiple_backends_require_an_explicit_noninteractive_choice() {
        let runtime = Runtime::new().expect("runtime");
        let dir = TempDir::new().expect("temp dir");
        let error = CliStore::open_with_backends(
            &runtime,
            vec![
                backend("alpha", open_backend(&runtime)),
                backend("beta", open_backend(&runtime)),
            ],
            dir.path().join("storage.toml"),
            None,
            false,
        )
        .expect_err("ambiguous selection");
        assert!(matches!(error, StorageError::Selection(_)));
        assert!(error.to_string().contains("alpha"));
        assert!(error.to_string().contains("beta"));
    }

    #[test]
    fn explicit_multiple_backend_choice_is_persisted_atomically() {
        let runtime = Runtime::new().expect("runtime");
        let dir = TempDir::new().expect("temp dir");
        let control_path = dir.path().join("storage.toml");
        let store = CliStore::open_with_backends(
            &runtime,
            vec![
                backend("alpha", open_backend(&runtime)),
                backend("beta", open_backend(&runtime)),
            ],
            &control_path,
            Some("beta"),
            false,
        )
        .expect("explicit selection");
        assert_eq!(
            runtime.block_on(store.manager().expect("manager").active_backend()),
            "beta"
        );
        assert_eq!(
            load_storage_control(&control_path)
                .expect("load control")
                .active
                .as_deref(),
            Some("beta")
        );
        assert_eq!(
            std::fs::read_dir(dir.path())
                .expect("read control dir")
                .count(),
            1,
            "temporary control file must be removed after rename"
        );
    }

    #[test]
    fn switching_backend_requires_the_switch_command() {
        let runtime = Runtime::new().expect("runtime");
        let dir = TempDir::new().expect("temp dir");
        let control_path = dir.path().join("storage.toml");
        save_storage_control(
            &control_path,
            &StorageControl {
                active: Some("alpha".to_owned()),
                ..StorageControl::default()
            },
        )
        .expect("save control");
        let error = CliStore::open_with_backends(
            &runtime,
            vec![
                backend("alpha", open_backend(&runtime)),
                backend("beta", open_backend(&runtime)),
            ],
            control_path,
            Some("beta"),
            false,
        )
        .expect_err("must migrate, not silently switch");
        assert!(matches!(error, StorageError::Selection(_)));
        assert!(error.to_string().contains("storage switch beta"));
    }

    #[test]
    fn snapshot_finds_credentials_by_provider() {
        let credential = ProviderCredential {
            provider: "deepseek".to_owned(),
            api_key: "sk-test".to_owned(),
            model: None,
            base_url: None,
            updated_at: 1,
        };
        let snapshot = StoreSnapshot {
            credentials: vec![credential.clone()],
            ..StoreSnapshot::default()
        };
        assert_eq!(snapshot.credential("deepseek"), Some(&credential));
        assert_eq!(snapshot.credential("openai"), None);
    }

    #[tokio::test]
    async fn config_and_credentials_round_trip() {
        let storage = TursoStorage::in_memory().await.expect("open in-memory");
        let store = CliStore::from_storage(storage, tokio::runtime::Handle::current());
        let user = ConfigScope::User;

        // 空库：没有默认值，也没有凭据。
        assert_eq!(store.snapshot(None).await, StoreSnapshot::default());

        store
            .set_config(&user, DEFAULT_PROVIDER, serde_json::json!("deepseek"))
            .await
            .expect("set provider");
        store
            .set_config(&user, DEFAULT_MODEL, serde_json::json!("deepseek-chat"))
            .await
            .expect("set model");
        store
            .set_config(&user, DEFAULT_THINKING_LEVEL, serde_json::json!("high"))
            .await
            .expect("set thinking");
        store
            .set_config(&user, CODEMODE_ENGINE, serde_json::json!("monty"))
            .await
            .expect("set codemode engine");
        store.queue_put_credential(ProviderCredential {
            provider: "deepseek".to_owned(),
            api_key: "sk-secret".to_owned(),
            model: Some("deepseek-chat".to_owned()),
            base_url: None,
            updated_at: 7,
        });

        // 写入是 fire-and-forget；flush 会先等它们入队再落盘。
        store.flush().await.expect("flush");

        let snapshot = store.snapshot(None).await;
        assert_eq!(snapshot.default_provider.as_deref(), Some("deepseek"));
        assert_eq!(snapshot.default_model.as_deref(), Some("deepseek-chat"));
        assert_eq!(snapshot.default_thinking_level.as_deref(), Some("high"));
        assert_eq!(snapshot.codemode_engine.as_deref(), Some("monty"));
        assert_eq!(
            snapshot.credential("deepseek").map(|c| c.api_key.as_str()),
            Some("sk-secret")
        );

        // 项目级覆盖用户级：同一个 key 在项目作用域里写一份，读出来是项目值。
        let project = ConfigScope::project("/work/demo");
        store
            .set_config(
                &project,
                DEFAULT_MODEL,
                serde_json::json!("deepseek-reasoner"),
            )
            .await
            .expect("set project model");
        store
            .set_config(&project, CODEMODE_ENGINE, serde_json::json!("js"))
            .await
            .expect("set project codemode engine");
        store.flush().await.expect("flush");

        assert_eq!(
            store
                .resolve_config(Some("/work/demo"), DEFAULT_MODEL)
                .await,
            Some(serde_json::json!("deepseek-reasoner"))
        );
        assert_eq!(
            store
                .snapshot(Some("/work/demo"))
                .await
                .default_model
                .as_deref(),
            Some("deepseek-reasoner")
        );
        assert_eq!(
            store
                .snapshot(Some("/work/demo"))
                .await
                .codemode_engine
                .as_deref(),
            Some("js")
        );
        // 另一个项目看不到这份覆盖，退回用户级。
        assert_eq!(
            store
                .snapshot(Some("/work/other"))
                .await
                .default_model
                .as_deref(),
            Some("deepseek-chat")
        );
        assert_eq!(
            store
                .snapshot(Some("/work/other"))
                .await
                .codemode_engine
                .as_deref(),
            Some("monty")
        );
    }

    #[tokio::test]
    async fn compaction_settings_come_from_scoped_config() {
        let storage = TursoStorage::in_memory().await.expect("open in-memory");
        let store = CliStore::from_storage(storage, tokio::runtime::Handle::current());

        // 缺省值对齐 pi。
        let defaults = store.compaction_settings(None).await;
        assert!(defaults.enabled);
        assert_eq!(defaults.reserve_tokens, 16_384);
        assert_eq!(defaults.keep_recent_tokens, 20_000);

        let user = ConfigScope::User;
        store
            .set_config(&user, COMPACTION_ENABLED, serde_json::json!(false))
            .await
            .expect("set enabled");
        store
            .set_config(&user, COMPACTION_RESERVE_TOKENS, serde_json::json!(100))
            .await
            .expect("set reserve");
        store
            .set_config(&user, COMPACTION_KEEP_RECENT_TOKENS, serde_json::json!(50))
            .await
            .expect("set keep");
        store.flush().await.expect("flush");

        let settings = store.compaction_settings(None).await;
        assert!(!settings.enabled);
        assert_eq!(settings.reserve_tokens, 100);
        assert_eq!(settings.keep_recent_tokens, 50);

        // 项目级覆盖用户级。
        store
            .set_config(
                &ConfigScope::project("/work/a"),
                COMPACTION_ENABLED,
                serde_json::json!(true),
            )
            .await
            .expect("set project enabled");
        store.flush().await.expect("flush");
        assert!(store.compaction_settings(Some("/work/a")).await.enabled);
        assert!(!store.compaction_settings(Some("/work/b")).await.enabled);
    }

    #[tokio::test]
    async fn session_metadata_round_trips_compaction() {
        let storage = TursoStorage::in_memory().await.expect("open in-memory");
        let store = Arc::new(CliStore::from_storage(
            storage,
            tokio::runtime::Handle::current(),
        ));
        let transcript = Transcript::new(Arc::clone(&store), "/work/a", None, 0);
        let messages = vec![
            AgentMessage::message(Message::User(UserMessage {
                content: UserContent::Text("第一条".into()),
                timestamp: 1,
            })),
            AgentMessage::message(Message::User(UserMessage {
                content: UserContent::Text("第二条".into()),
                timestamp: 2,
            })),
        ];
        let compaction = tower_agent_runtime::Compaction {
            summary: "结构化摘要".into(),
            first_kept: 1,
            tokens_before: 4242,
        };

        transcript
            .sync(&messages, Some("gpt-5.5"), Some(&compaction))
            .await
            .expect("sync");
        store.flush().await.expect("flush");

        let record = store
            .session_record(&transcript.id())
            .await
            .expect("record");
        assert_eq!(compaction_from_record(&record), Some(compaction));
        // 项目作用域照旧写进 metadata。
        assert_eq!(
            record.metadata.get("cwd").and_then(Value::as_str),
            Some("/work/a")
        );
    }

    #[tokio::test]
    async fn transcript_syncs_and_sessions_are_project_scoped() {
        let storage = TursoStorage::in_memory().await.expect("open in-memory");
        let store = Arc::new(CliStore::from_storage(
            storage,
            tokio::runtime::Handle::current(),
        ));

        let transcript = Transcript::new(Arc::clone(&store), "/work/a", None, 0);
        let messages = vec![AgentMessage::message(Message::User(UserMessage {
            content: UserContent::Text("帮我看看这个仓库".into()),
            timestamp: 1,
        }))];
        transcript
            .sync(&messages, Some("gpt-5.5"), None)
            .await
            .expect("sync");
        store.flush().await.expect("flush");

        // 会话记录带项目作用域 + 标题 + 模型。
        let record = store.latest_session("/work/a").await.expect("latest");
        assert_eq!(record.id, transcript.id());
        assert_eq!(record.title.as_deref(), Some("帮我看看这个仓库"));
        assert_eq!(record.model.as_deref(), Some("gpt-5.5"));
        assert!(store.latest_session("/work/b").await.is_none());

        // transcript 读回。
        let loaded = store.load_transcript(&transcript.id()).await;
        assert_eq!(loaded, messages);

        // 再 sync 一次不会重复追加（只写尾巴）。
        transcript
            .sync(&messages, Some("gpt-5.5"), None)
            .await
            .expect("sync again");
        store.flush().await.expect("flush");
        assert_eq!(store.load_transcript(&transcript.id()).await.len(), 1);

        // `/new` 换 id，下一次 sync 写到新会话。
        transcript.reset();
        let new_id = transcript.id();
        assert_ne!(new_id, record.id);
        transcript
            .sync(&messages, Some("gpt-5.5"), None)
            .await
            .expect("sync new session");
        store.flush().await.expect("flush");
        assert_eq!(store.load_transcript(&new_id).await, messages);
    }
}
