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

use std::{collections::BTreeMap, path::Path, sync::Arc};

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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// 一个完整存储后端 = 上面六个子接口。
///
/// 宿主用 `Arc<dyn Storage>` 拿到一个后端；具体实现（Turso / 未来别的）只需实现这六个。
pub trait Storage:
    ConfigStore + ProviderStore + SessionStore + HistoryStore + MemoryStore + ScriptStore
{
}

impl<T> Storage for T where
    T: ConfigStore + ProviderStore + SessionStore + HistoryStore + MemoryStore + ScriptStore
{
}

/// 把任意 [`Storage`] 后端包装成**静态存储插件**。
///
/// 清单提供 `persist.<name>` 与 `memory.<name>` 两项能力，且**不标 `protected`**：
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
}

impl StaticPlugin for StoragePlugin {
    fn manifest(&self) -> PluginManifest {
        let version = self.version.clone();
        PluginManifest::new(self.name.clone(), version.clone(), PluginKind::Static)
            .providing(
                CapabilityId::new("persist", self.name.clone()),
                version.clone(),
            )
            .providing(CapabilityId::new("memory", self.name.clone()), version)
    }

    fn install(&self, _context: &mut PluginContext) -> Result<(), PluginError> {
        Ok(())
    }
}
