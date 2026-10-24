//! Storage contracts exposed by the plugin SDK's optional `storage` feature.
//!
//! 把「配置 / provider 凭据 / 会话 / 历史 / 记忆 / 脚本」抽成宿主无关的接口。
//!
//! 这是存储插件的**公共基础**：`tower-agent-plugin-turso` 这类静态插件实现这些 trait；
//! 未来同名动态插件 / JIT 插件只要实现同一套接口就能顶替它——覆盖走插件注册表的同名规则，
//! 因此 [`StoragePlugin`] **刻意不标 `protected`**：默认实现必须可被替换。
//!
//! 接口全部用 [`BoxFuture`] 表达 async（对象安全），宿主拿 `Arc<dyn Storage>` 即可使用；
//! 具体后端（Turso / 未来别的）只负责实现这六个子接口。
//!
//! 各接口的边界：
//!
//! - [`ConfigStore`]：按作用域（[`ConfigScope::User`] / [`ConfigScope::Project`]）存的
//!   key → JSON 配置项；读取用 [`resolve_config`]，项目级覆盖用户级；
//! - [`ProviderStore`]：登录过的 provider 凭据（API key / base URL / 默认模型）；
//! - [`SessionStore`]：会话**元数据**（标题、模型、时间戳），不含消息体；
//! - [`HistoryStore`]：会话的**原始对话历史**（逐条 [`AgentMessage`]，按序可重放）；
//! - [`MemoryStore`]：跨会话的长期记忆条目。

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

use futures_util::future::BoxFuture;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_agent_core::AgentMessage;
use tower_agent_plugin::{
    CapabilityId, PluginContext, PluginError, PluginKind, PluginManifest, StaticPlugin,
};

/// 存储操作的结果。
pub type StorageResult<T> = Result<T, StorageError>;

/// 存储操作的 Future（对象安全的 async 形式）。
pub type StorageFuture<'a, T> = BoxFuture<'a, StorageResult<T>>;

/// 存储错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// 后端错误（连接 / 查询 / 写入失败）。
    #[error("storage backend error: {0}")]
    Backend(String),
    /// 序列化 / 反序列化失败。
    #[error("storage serialization error: {0}")]
    Serialization(String),
    /// 目标不存在。
    #[error("storage record not found: {0}")]
    NotFound(String),
    /// 后端不存在或未启用。
    #[error("storage backend unavailable: {0}")]
    Unavailable(String),
    /// 迁移失败。
    #[error("storage migration failed: {0}")]
    Migration(String),
    /// 宿主需要用户选择一个 backend。
    #[error("storage backend selection required: {0}")]
    Selection(String),
}

impl StorageError {
    /// 便捷构造一个后端错误。
    pub fn backend(message: impl Into<String>) -> Self {
        Self::Backend(message.into())
    }
}

/// 配置作用域：同一批配置按作用域隔离存储，读取时**项目级覆盖用户级**。
///
/// 作用域是接口的一部分（不是某个后端的私事）：CLI、axum、webui 等任何消费端都通过
/// [`ConfigStore`] + [`resolve_config`] 读写同一套语义，后端只决定「一个库还是多个库」。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ConfigScope {
    /// 用户级：跨项目共享（对应 pi 的 agent 目录设置）。
    User,
    /// 项目级：按项目 id（通常是规范化后的工作目录）隔离。
    Project(String),
}

impl ConfigScope {
    /// 用项目 id 构造项目级作用域。
    #[must_use]
    pub fn project(id: impl Into<String>) -> Self {
        Self::Project(id.into())
    }

    /// 从工作目录构造项目作用域；能 `canonicalize` 就规范化，否则退回去掉尾分隔符的字面路径。
    #[must_use]
    pub fn from_working_dir(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        let resolved = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        Self::Project(normalize_project_id(&resolved))
    }

    /// 是否是用户级作用域。
    #[must_use]
    pub fn is_user(&self) -> bool {
        matches!(self, Self::User)
    }

    /// 项目 id（用户级返回 `None`）。
    #[must_use]
    pub fn project_id(&self) -> Option<&str> {
        match self {
            Self::User => None,
            Self::Project(id) => Some(id),
        }
    }

    /// 落库用的两部分：`(scope_kind, scope_id)`。实现层用它拼主键，消费端不关心。
    #[must_use]
    pub fn storage_parts(&self) -> (&'static str, &str) {
        match self {
            Self::User => ("user", ""),
            Self::Project(id) => ("project", id.as_str()),
        }
    }
}

/// 项目 id 规范化：去掉尾部的路径分隔符（`/a/b/` → `/a/b`）。
fn normalize_project_id(path: &Path) -> String {
    let text = path.to_string_lossy();
    let trimmed = text.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        text.into_owned()
    } else {
        trimmed.to_owned()
    }
}

/// 配置存储：按「作用域 + key」存取 JSON 值（默认模型、UI 偏好、上次会话等）。
pub trait ConfigStore: Send + Sync + 'static {
    /// 读取一项配置；不存在返回 `None`。
    fn get_config(&self, scope: &ConfigScope, key: &str) -> StorageFuture<'_, Option<Value>>;

    /// 写入一项配置（覆盖旧值）。
    fn set_config(&self, scope: &ConfigScope, key: &str, value: Value) -> StorageFuture<'_, ()>;

    /// 删除一项配置。
    fn delete_config(&self, scope: &ConfigScope, key: &str) -> StorageFuture<'_, ()>;

    /// 列出某个作用域下的所有配置 key。
    fn list_config(&self, scope: &ConfigScope) -> StorageFuture<'_, Vec<String>>;

    /// 列出包含配置记录的全部作用域。
    fn list_config_scopes(&self) -> StorageFuture<'_, Vec<ConfigScope>>;
}

/// 读取某个 key 的**有效值**：项目级覆盖用户级，都没有则返回 `None`。
///
/// 消费端（CLI / axum / webui）都走这个函数，保证作用域优先级只有一处实现。
pub async fn resolve_config(
    store: &dyn ConfigStore,
    project: Option<&str>,
    key: &str,
) -> StorageResult<Option<Value>> {
    if let Some(project) = project
        && let Some(value) = store
            .get_config(&ConfigScope::Project(project.to_owned()), key)
            .await?
    {
        return Ok(Some(value));
    }
    store.get_config(&ConfigScope::User, key).await
}

/// 列出**合并后**的有效配置：用户级打底，项目级覆盖，按 key 排序。
pub async fn effective_config(
    store: &dyn ConfigStore,
    project: Option<&str>,
) -> StorageResult<Vec<(String, Value)>> {
    let mut merged: BTreeMap<String, Value> = BTreeMap::new();
    for key in store.list_config(&ConfigScope::User).await? {
        if let Some(value) = store.get_config(&ConfigScope::User, &key).await? {
            merged.insert(key, value);
        }
    }
    if let Some(project) = project {
        let scope = ConfigScope::Project(project.to_owned());
        for key in store.list_config(&scope).await? {
            if let Some(value) = store.get_config(&scope, &key).await? {
                merged.insert(key, value);
            }
        }
    }
    Ok(merged.into_iter().collect())
}

/// 一个登录过的 provider 的凭据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCredential {
    /// provider 标识（`anthropic` / `openai` / `deepseek` / `google` …）。
    pub provider: String,
    /// API key。
    pub api_key: String,
    /// 该 provider 下次默认使用的模型 id。
    pub model: Option<String>,
    /// 自定义端点（兼容端点 / 代理）。
    pub base_url: Option<String>,
    /// 最后更新时间（Unix 毫秒）。
    pub updated_at: i64,
}

/// 凭据存储：登录过的 provider。
pub trait ProviderStore: Send + Sync + 'static {
    /// 写入 / 覆盖一个 provider 凭据。
    fn put_provider(&self, credential: ProviderCredential) -> StorageFuture<'_, ()>;

    /// 取一个 provider 凭据。
    fn get_provider(&self, provider: &str) -> StorageFuture<'_, Option<ProviderCredential>>;

    /// 列出全部 provider 凭据（按 provider 名排序）。
    fn list_providers(&self) -> StorageFuture<'_, Vec<ProviderCredential>>;

    /// 删除一个 provider 凭据。
    fn remove_provider(&self, provider: &str) -> StorageFuture<'_, ()>;
}

/// 会话元数据（不含消息体；消息体在 [`HistoryStore`]）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// 会话 id。
    pub id: String,
    /// 人类可读标题（首条用户消息或模型生成）。
    pub title: Option<String>,
    /// 该会话使用的模型 id。
    pub model: Option<String>,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 最后更新时间（Unix 毫秒）。
    pub updated_at: i64,
    /// 应用自定义元数据。
    #[serde(default)]
    pub metadata: Value,
}

/// 会话存储：会话元数据的增删查。
pub trait SessionStore: Send + Sync + 'static {
    /// 新建 / 覆盖一个会话记录。
    fn upsert_session(&self, session: SessionRecord) -> StorageFuture<'_, ()>;

    /// 取一个会话记录。
    fn get_session(&self, id: &str) -> StorageFuture<'_, Option<SessionRecord>>;

    /// 列出全部会话（按 `updated_at` 倒序）。
    fn list_sessions(&self) -> StorageFuture<'_, Vec<SessionRecord>>;

    /// 删除一个会话（连它的历史一起删，由实现负责）。
    fn delete_session(&self, id: &str) -> StorageFuture<'_, ()>;
}

///一个持久化的代码脚本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptRecord {
    /// 脚本id。
    pub id: String,
    /// 脚本名称，在其作用域中唯一。
    pub name: String,
    /// 解释器标识符（例如‘ monty ’或‘ javascript ’）。
    pub language: String,
    /// 脚本范围。
    pub scope: ConfigScope,
    /// 脚本源码。
    pub source: String,
    /// 是否应该在启动时加载脚本。
    pub enabled: bool,
    /// 应用程序定义的功能权限与脚本保持一致。
    pub permissions: Value,
    /// 应用程序特定的元数据。
    #[serde(default)]
    pub metadata: Value,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 上次更新时间（Unix 毫秒）。
    pub updated_at: i64,
    /// 用于变更检测的源的散列。
    pub source_hash: String,
}

/// 脚本存储：codemode脚本键值为`(scope, name)`。
pub trait ScriptStore: Send + Sync + 'static {
    /// 在其作用域中插入或替换脚本。
    fn upsert_script(&self, script: ScriptRecord) -> StorageFuture<'_, ()>;

    /// 根据作用域和名称读取脚本。
    fn get_script(
        &self,
        scope: &ConfigScope,
        name: &str,
    ) -> StorageFuture<'_, Option<ScriptRecord>>;

    /// 列出一个作用域内的脚本，首先列出最新的‘ updated_at ’。
    fn list_scripts(&self, scope: &ConfigScope) -> StorageFuture<'_, Vec<ScriptRecord>>;

    /// 通过范围和名称删除脚本。
    fn delete_script(&self, scope: &ConfigScope, name: &str) -> StorageFuture<'_, ()>;

    /// 列出包含脚本记录的全部作用域。
    fn list_script_scopes(&self) -> StorageFuture<'_, Vec<ConfigScope>>;
}

/// 原始对话历史：会话 transcript 的追加 / 读回 / 替换 / 清空。
pub trait HistoryStore: Send + Sync + 'static {
    /// 追加一条消息。
    fn append_message(&self, session_id: &str, message: AgentMessage) -> StorageFuture<'_, ()>;

    /// 读回某会话的完整 transcript（按写入顺序）。
    fn load_messages(&self, session_id: &str) -> StorageFuture<'_, Vec<AgentMessage>>;

    /// 用给定 transcript 覆盖某会话的历史（compaction 用）。
    fn replace_messages(
        &self,
        session_id: &str,
        messages: Vec<AgentMessage>,
    ) -> StorageFuture<'_, ()>;

    /// 清空某会话的历史。
    fn clear_messages(&self, session_id: &str) -> StorageFuture<'_, ()>;
}

/// 一条长期记忆。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// 记忆 id。
    pub id: String,
    /// 记忆正文。
    pub content: String,
    /// 标签（便于过滤 / 召回）。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 作用域（`global` / 项目路径 / 会话 id …）。
    pub scope: Option<String>,
    /// 创建时间（Unix 毫秒）。
    pub created_at: i64,
}

/// 记忆存储：跨会话的长期事实。
pub trait MemoryStore: Send + Sync + 'static {
    /// 写入 / 覆盖一条记忆。
    fn remember(&self, record: MemoryRecord) -> StorageFuture<'_, ()>;

    /// 按关键词召回记忆（实现可做子串 / 全文匹配），最多 `limit` 条。
    fn recall(&self, query: &str, limit: usize) -> StorageFuture<'_, Vec<MemoryRecord>>;

    /// 列出最近 `limit` 条记忆。
    fn list_memories(&self, limit: usize) -> StorageFuture<'_, Vec<MemoryRecord>>;

    /// 删除一条记忆。
    fn forget(&self, id: &str) -> StorageFuture<'_, ()>;
}

/// One scoped configuration value in a storage snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigRecord {
    /// Configuration scope.
    pub scope: ConfigScope,
    /// Configuration key.
    pub key: String,
    /// JSON value.
    pub value: Value,
}

/// A session and its complete transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Session metadata.
    pub record: SessionRecord,
    /// Ordered transcript.
    pub messages: Vec<AgentMessage>,
}

/// Complete engine-independent storage snapshot used for backend migration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StorageSnapshot {
    /// Format version, currently `1`.
    #[serde(default = "snapshot_format_version")]
    pub format_version: u32,
    /// Configuration values.
    #[serde(default)]
    pub config: Vec<ConfigRecord>,
    /// Provider credentials.
    #[serde(default)]
    pub credentials: Vec<ProviderCredential>,
    /// Sessions with transcripts.
    #[serde(default)]
    pub sessions: Vec<SessionSnapshot>,
    /// Long-term memories.
    #[serde(default)]
    pub memories: Vec<MemoryRecord>,
    /// Codemode scripts.
    #[serde(default)]
    pub scripts: Vec<ScriptRecord>,
}

const fn snapshot_format_version() -> u32 {
    1
}

impl StorageSnapshot {
    /// Whether every data domain is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.config.is_empty()
            && self.credentials.is_empty()
            && self.sessions.is_empty()
            && self.memories.is_empty()
            && self.scripts.is_empty()
    }

    /// Sort all collections into a stable order for equality checks.
    pub fn normalize(&mut self) {
        self.format_version = snapshot_format_version();
        self.config
            .sort_by(|left, right| (&left.scope, &left.key).cmp(&(&right.scope, &right.key)));
        self.credentials
            .sort_by(|left, right| left.provider.cmp(&right.provider));
        self.sessions
            .sort_by(|left, right| left.record.id.cmp(&right.record.id));
        self.memories.sort_by(|left, right| left.id.cmp(&right.id));
        self.scripts.sort_by(|left, right| {
            (&left.scope, &left.name, &left.id).cmp(&(&right.scope, &right.name, &right.id))
        });
    }
}

/// Maintenance operations required for a switchable storage backend.
pub trait StorageMaintenance:
    ConfigStore + ProviderStore + SessionStore + HistoryStore + MemoryStore + ScriptStore
{
    /// Wait until all currently accepted writes are durable.
    fn flush(&self) -> StorageFuture<'_, ()>;

    /// Export a complete consistent snapshot.
    fn export_snapshot(&self) -> StorageFuture<'_, StorageSnapshot> {
        Box::pin(async move {
            let mut config = Vec::new();
            for scope in self.list_config_scopes().await? {
                for key in self.list_config(&scope).await? {
                    if let Some(value) = self.get_config(&scope, &key).await? {
                        config.push(ConfigRecord {
                            scope: scope.clone(),
                            key,
                            value,
                        });
                    }
                }
            }

            let mut sessions = Vec::new();
            for record in self.list_sessions().await? {
                let messages = self.load_messages(&record.id).await?;
                sessions.push(SessionSnapshot { record, messages });
            }

            let mut scripts = Vec::new();
            for scope in self.list_script_scopes().await? {
                scripts.extend(self.list_scripts(&scope).await?);
            }

            let mut snapshot = StorageSnapshot {
                format_version: snapshot_format_version(),
                config,
                credentials: self.list_providers().await?,
                sessions,
                memories: self.list_memories(i64::MAX as usize).await?,
                scripts,
            };
            snapshot.normalize();
            Ok(snapshot)
        })
    }

    /// Import a snapshot and flush the target before returning.
    fn import_snapshot(&self, snapshot: StorageSnapshot) -> StorageFuture<'_, ()> {
        Box::pin(async move {
            if snapshot.format_version != snapshot_format_version() {
                return Err(StorageError::Migration(format!(
                    "unsupported storage snapshot version {}",
                    snapshot.format_version
                )));
            }
            for record in snapshot.config {
                self.set_config(&record.scope, &record.key, record.value)
                    .await?;
            }
            for credential in snapshot.credentials {
                self.put_provider(credential).await?;
            }
            for session in snapshot.sessions {
                let id = session.record.id.clone();
                self.upsert_session(session.record).await?;
                self.replace_messages(&id, session.messages).await?;
            }
            for memory in snapshot.memories {
                self.remember(memory).await?;
            }
            for script in snapshot.scripts {
                self.upsert_script(script).await?;
            }
            self.flush().await
        })
    }

    /// Remove all records from this backend.
    fn clear_all(&self) -> StorageFuture<'_, ()> {
        Box::pin(async move {
            for scope in self.list_config_scopes().await? {
                for key in self.list_config(&scope).await? {
                    self.delete_config(&scope, &key).await?;
                }
            }
            for credential in self.list_providers().await? {
                self.remove_provider(&credential.provider).await?;
            }
            for session in self.list_sessions().await? {
                self.delete_session(&session.id).await?;
            }
            for memory in self.list_memories(i64::MAX as usize).await? {
                self.forget(&memory.id).await?;
            }
            for scope in self.list_script_scopes().await? {
                for script in self.list_scripts(&scope).await? {
                    self.delete_script(&scope, &script.name).await?;
                }
            }
            self.flush().await
        })
    }
}

/// 一个完整、可切换的存储后端。
pub trait Storage: StorageMaintenance {}

impl<T> Storage for T where T: StorageMaintenance {}

/// 把任意 [`Storage`] 后端包装成**静态存储插件**。
///
/// 清单提供 `storage.<name>` 能力，且**不标 `protected`**：
/// 同名动态插件 / JIT 插件按注册表的覆盖规则（`静态 < 动态 < JIT`）顶替它，
/// 这正是「Turso 只是默认实现、可被替换」的机制保证。
///
/// `install` 不做任何事——存储能力靠 [`StoragePlugin::storage`] 交给宿主，
/// 不通过工具注册表分发（未来动态插件走宿主 ABI 时再补跨形态取值）。
pub struct StoragePlugin {
    name: String,
    version: Version,
    backend: Arc<dyn Storage>,
}

impl StoragePlugin {
    /// 用后端名、版本与实现构造。
    #[must_use]
    pub fn new(name: impl Into<String>, version: Version, backend: Arc<dyn Storage>) -> Self {
        Self {
            name: name.into(),
            version,
            backend,
        }
    }

    /// 插件（后端）名。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 取出底层存储实现，交给宿主使用。
    #[must_use]
    pub fn storage(&self) -> Arc<dyn Storage> {
        Arc::clone(&self.backend)
    }

    /// Backend descriptor registered into the host context.
    #[must_use]
    pub fn registration(&self) -> StorageBackendRegistration {
        StorageBackendRegistration {
            name: self.name.clone(),
            version: self.version.clone(),
            storage: Arc::clone(&self.backend),
        }
    }
}

impl StaticPlugin for StoragePlugin {
    fn manifest(&self) -> PluginManifest {
        let version = self.version.clone();
        PluginManifest::new(self.name.clone(), version.clone(), PluginKind::Static).providing(
            CapabilityId::new("storage", self.name.clone()),
            version.clone(),
        )
    }

    fn install(&self, context: &mut PluginContext) -> Result<(), PluginError> {
        context.register_extension(self.registration());
        Ok(())
    }
}

/// A storage implementation contributed by a plugin.
#[derive(Clone)]
pub struct StorageBackendRegistration {
    /// Stable backend/plugin name.
    pub name: String,
    /// Backend version.
    pub version: Version,
    /// Storage implementation.
    pub storage: Arc<dyn Storage>,
}

struct StorageManagerState {
    active: String,
    switching: Option<String>,
    enabled: BTreeSet<String>,
}

/// Runtime owner of multiple registered storage backends.
///
/// Exactly one backend receives all storage operations. Switching takes an
/// exclusive lock, so no operation can read or write while the snapshot is
/// copied. The active pointer changes only after target import and verification
/// succeed, which provides automatic rollback on failure.
pub struct StorageManager {
    backends: BTreeMap<String, StorageBackendRegistration>,
    state: tokio::sync::RwLock<StorageManagerState>,
}

impl StorageManager {
    /// Build a manager and select one active backend.
    pub fn new(
        backends: impl IntoIterator<Item = StorageBackendRegistration>,
        active: impl Into<String>,
    ) -> StorageResult<Self> {
        let mut map = BTreeMap::new();
        for backend in backends {
            if map.insert(backend.name.clone(), backend).is_some() {
                return Err(StorageError::Migration(
                    "duplicate storage backend name".to_owned(),
                ));
            }
        }
        let active = active.into();
        if !map.contains_key(&active) {
            return Err(StorageError::Unavailable(active));
        }
        let enabled = map.keys().cloned().collect();
        Ok(Self {
            backends: map,
            state: tokio::sync::RwLock::new(StorageManagerState {
                active,
                switching: None,
                enabled,
            }),
        })
    }

    /// Registered backend names in stable order.
    pub async fn backend_names(&self) -> Vec<String> {
        let state = self.state.read().await;
        state.enabled.iter().cloned().collect()
    }

    /// Current active backend.
    pub async fn active_backend(&self) -> String {
        self.state.read().await.active.clone()
    }

    /// Export the complete active backend snapshot.
    pub async fn snapshot(&self) -> StorageResult<StorageSnapshot> {
        let state = self.state.read().await;
        let backend = self.backend(&state, &state.active)?;
        backend.export_snapshot().await
    }

    /// Mark a backend enabled or disabled.
    pub async fn set_backend_enabled(&self, name: &str, enabled: bool) -> StorageResult<()> {
        let mut state = self.state.write().await;
        if !self.backends.contains_key(name) {
            return Err(StorageError::Unavailable(name.to_owned()));
        }
        if !enabled && state.active == name {
            return Err(StorageError::Unavailable(format!(
                "backend `{name}` is active; switch to another backend first"
            )));
        }
        if enabled {
            state.enabled.insert(name.to_owned());
        } else {
            state.enabled.remove(name);
        }
        Ok(())
    }

    /// Switch to another backend through a full snapshot copy and verification.
    ///
    /// `persist_selection` is called only after the target has been populated
    /// and verified. If it fails, the active backend remains unchanged.
    pub async fn switch_to_with<F>(&self, target: &str, persist_selection: F) -> StorageResult<()>
    where
        F: FnOnce(&str) -> StorageResult<()> + Send,
    {
        let mut state = self.state.write().await;
        if state.active == target {
            return Ok(());
        }
        if !state.enabled.contains(target) {
            return Err(StorageError::Unavailable(target.to_owned()));
        }
        let source_name = state.active.clone();
        let source = self.backend(&state, &source_name)?.clone();
        let target_backend = self.backend(&state, target)?.clone();
        state.switching = Some(target.to_owned());

        let mut staging_started = false;
        let result = async {
            source.flush().await?;
            let existing = target_backend.export_snapshot().await?;
            if !existing.is_empty() {
                return Err(StorageError::Migration(format!(
                    "target backend `{target}` is not empty"
                )));
            }
            staging_started = true;
            let mut expected = source.export_snapshot().await?;
            expected.normalize();
            target_backend.import_snapshot(expected.clone()).await?;
            let mut actual = target_backend.export_snapshot().await?;
            actual.normalize();
            if actual != expected {
                return Err(StorageError::Migration(format!(
                    "target backend `{target}` failed verification"
                )));
            }
            persist_selection(target)?;
            Ok(())
        }
        .await;

        state.switching = None;
        if result.is_err() && staging_started {
            // Best effort: the active route has already rolled back; cleaning
            // staging prevents a failed target from blocking a later retry.
            let _ = target_backend.clear_all().await;
        }
        match result {
            Ok(()) => {
                state.active = target.to_owned();
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Switch without persisting an external selection file.
    pub async fn switch_to(&self, target: &str) -> StorageResult<()> {
        self.switch_to_with(target, |_| Ok(())).await
    }

    fn backend<'a>(
        &'a self,
        state: &StorageManagerState,
        name: &str,
    ) -> StorageResult<&'a Arc<dyn Storage>> {
        if !state.enabled.contains(name) {
            return Err(StorageError::Unavailable(name.to_owned()));
        }
        self.backends
            .get(name)
            .map(|backend| &backend.storage)
            .ok_or_else(|| StorageError::Unavailable(name.to_owned()))
    }
}

macro_rules! manager_read {
    ($self:ident, $state:ident, |$backend:ident| $body:expr) => {{
        let $state = $self.state.read().await;
        let $backend = $self.backend(&$state, &$state.active)?.clone();
        $body
    }};
}

impl ConfigStore for StorageManager {
    fn get_config(&self, scope: &ConfigScope, key: &str) -> StorageFuture<'_, Option<Value>> {
        let scope = scope.clone();
        let key = key.to_owned();
        Box::pin(async move {
            manager_read!(self, state, |backend| backend
                .get_config(&scope, &key)
                .await)
        })
    }

    fn set_config(&self, scope: &ConfigScope, key: &str, value: Value) -> StorageFuture<'_, ()> {
        let scope = scope.clone();
        let key = key.to_owned();
        Box::pin(async move {
            manager_read!(self, state, |backend| backend
                .set_config(&scope, &key, value)
                .await)
        })
    }

    fn delete_config(&self, scope: &ConfigScope, key: &str) -> StorageFuture<'_, ()> {
        let scope = scope.clone();
        let key = key.to_owned();
        Box::pin(async move {
            manager_read!(self, state, |backend| backend
                .delete_config(&scope, &key)
                .await)
        })
    }

    fn list_config(&self, scope: &ConfigScope) -> StorageFuture<'_, Vec<String>> {
        let scope = scope.clone();
        Box::pin(
            async move { manager_read!(self, state, |backend| backend.list_config(&scope).await) },
        )
    }

    fn list_config_scopes(&self) -> StorageFuture<'_, Vec<ConfigScope>> {
        Box::pin(
            async move { manager_read!(self, state, |backend| backend.list_config_scopes().await) },
        )
    }
}

impl ProviderStore for StorageManager {
    fn put_provider(&self, credential: ProviderCredential) -> StorageFuture<'_, ()> {
        Box::pin(async move {
            manager_read!(self, state, |backend| backend
                .put_provider(credential)
                .await)
        })
    }

    fn get_provider(&self, provider: &str) -> StorageFuture<'_, Option<ProviderCredential>> {
        let provider = provider.to_owned();
        Box::pin(async move {
            manager_read!(self, state, |backend| backend.get_provider(&provider).await)
        })
    }

    fn list_providers(&self) -> StorageFuture<'_, Vec<ProviderCredential>> {
        Box::pin(
            async move { manager_read!(self, state, |backend| backend.list_providers().await) },
        )
    }

    fn remove_provider(&self, provider: &str) -> StorageFuture<'_, ()> {
        let provider = provider.to_owned();
        Box::pin(async move {
            manager_read!(self, state, |backend| backend
                .remove_provider(&provider)
                .await)
        })
    }
}

impl SessionStore for StorageManager {
    fn upsert_session(&self, session: SessionRecord) -> StorageFuture<'_, ()> {
        Box::pin(async move {
            manager_read!(self, state, |backend| backend.upsert_session(session).await)
        })
    }

    fn get_session(&self, id: &str) -> StorageFuture<'_, Option<SessionRecord>> {
        let id = id.to_owned();
        Box::pin(
            async move { manager_read!(self, state, |backend| backend.get_session(&id).await) },
        )
    }

    fn list_sessions(&self) -> StorageFuture<'_, Vec<SessionRecord>> {
        Box::pin(async move { manager_read!(self, state, |backend| backend.list_sessions().await) })
    }

    fn delete_session(&self, id: &str) -> StorageFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(
            async move { manager_read!(self, state, |backend| backend.delete_session(&id).await) },
        )
    }
}

impl HistoryStore for StorageManager {
    fn append_message(&self, session_id: &str, message: AgentMessage) -> StorageFuture<'_, ()> {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            manager_read!(self, state, |backend| backend
                .append_message(&session_id, message)
                .await)
        })
    }

    fn load_messages(&self, session_id: &str) -> StorageFuture<'_, Vec<AgentMessage>> {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            manager_read!(self, state, |backend| backend
                .load_messages(&session_id)
                .await)
        })
    }

    fn replace_messages(
        &self,
        session_id: &str,
        messages: Vec<AgentMessage>,
    ) -> StorageFuture<'_, ()> {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            manager_read!(self, state, |backend| backend
                .replace_messages(&session_id, messages)
                .await)
        })
    }

    fn clear_messages(&self, session_id: &str) -> StorageFuture<'_, ()> {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            manager_read!(self, state, |backend| backend
                .clear_messages(&session_id)
                .await)
        })
    }
}

impl MemoryStore for StorageManager {
    fn remember(&self, record: MemoryRecord) -> StorageFuture<'_, ()> {
        Box::pin(
            async move { manager_read!(self, state, |backend| backend.remember(record).await) },
        )
    }

    fn recall(&self, query: &str, limit: usize) -> StorageFuture<'_, Vec<MemoryRecord>> {
        let query = query.to_owned();
        Box::pin(async move {
            manager_read!(self, state, |backend| backend.recall(&query, limit).await)
        })
    }

    fn list_memories(&self, limit: usize) -> StorageFuture<'_, Vec<MemoryRecord>> {
        Box::pin(
            async move { manager_read!(self, state, |backend| backend.list_memories(limit).await) },
        )
    }

    fn forget(&self, id: &str) -> StorageFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move { manager_read!(self, state, |backend| backend.forget(&id).await) })
    }
}

impl ScriptStore for StorageManager {
    fn upsert_script(&self, script: ScriptRecord) -> StorageFuture<'_, ()> {
        Box::pin(async move {
            manager_read!(self, state, |backend| backend.upsert_script(script).await)
        })
    }

    fn get_script(
        &self,
        scope: &ConfigScope,
        name: &str,
    ) -> StorageFuture<'_, Option<ScriptRecord>> {
        let scope = scope.clone();
        let name = name.to_owned();
        Box::pin(async move {
            manager_read!(self, state, |backend| backend
                .get_script(&scope, &name)
                .await)
        })
    }

    fn list_scripts(&self, scope: &ConfigScope) -> StorageFuture<'_, Vec<ScriptRecord>> {
        let scope = scope.clone();
        Box::pin(
            async move { manager_read!(self, state, |backend| backend.list_scripts(&scope).await) },
        )
    }

    fn delete_script(&self, scope: &ConfigScope, name: &str) -> StorageFuture<'_, ()> {
        let scope = scope.clone();
        let name = name.to_owned();
        Box::pin(async move {
            manager_read!(self, state, |backend| backend
                .delete_script(&scope, &name)
                .await)
        })
    }

    fn list_script_scopes(&self) -> StorageFuture<'_, Vec<ConfigScope>> {
        Box::pin(
            async move { manager_read!(self, state, |backend| backend.list_script_scopes().await) },
        )
    }
}

impl StorageMaintenance for StorageManager {
    fn flush(&self) -> StorageFuture<'_, ()> {
        Box::pin(async move { manager_read!(self, state, |backend| backend.flush().await) })
    }
}
