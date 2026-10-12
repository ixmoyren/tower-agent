//! 存储抽象：把「配置 / provider 凭据 / 会话 / 原始历史 / 记忆」抽成宿主无关的接口。
//!
//! 这是存储插件的**公共基础**：`tower-agent-turso` 这类静态插件实现这些 trait；
//! 未来同名动态插件 / JIT 插件只要实现同一套接口就能顶替它——覆盖走插件注册表的同名规则，
//! 因此 [`StoragePlugin`] **刻意不标 `protected`**：默认实现必须可被替换。
//!
//! 接口全部用 [`BoxFuture`] 表达 async（对象安全），宿主拿 `Arc<dyn Storage>` 即可使用；
//! 具体后端（Turso / 未来别的）只负责实现这五个子接口。
//!
//! 各接口的边界：
//!
//! - [`ConfigStore`]：通用 key → JSON 配置项；
//! - [`ProviderStore`]：登录过的 provider 凭据（API key / base URL / 默认模型）；
//! - [`SessionStore`]：会话**元数据**（标题、模型、时间戳），不含消息体；
//! - [`HistoryStore`]：会话的**原始对话历史**（逐条 [`AgentMessage`]，按序可重放）；
//! - [`MemoryStore`]：跨会话的长期记忆条目。

use std::sync::Arc;

use futures_util::future::BoxFuture;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_agent_core::AgentMessage;
use tower_agent_plugins::{
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

/// 配置存储：按 key 存取 JSON 值（默认模型、UI 偏好、上次会话等）。
pub trait ConfigStore: Send + Sync + 'static {
    /// 读取一项配置；不存在返回 `None`。
    fn get_config(&self, key: &str) -> StorageFuture<'_, Option<Value>>;

    /// 写入一项配置（覆盖旧值）。
    fn set_config(&self, key: &str, value: Value) -> StorageFuture<'_, ()>;

    /// 删除一项配置。
    fn delete_config(&self, key: &str) -> StorageFuture<'_, ()>;

    /// 列出所有配置 key。
    fn list_config(&self) -> StorageFuture<'_, Vec<String>>;
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

/// 一个完整存储后端 = 上面五个子接口。
///
/// 宿主用 `Arc<dyn Storage>` 拿到一个后端；具体实现（Turso / 未来别的）只需实现这五个。
pub trait Storage: ConfigStore + ProviderStore + SessionStore + HistoryStore + MemoryStore {}

impl<T> Storage for T where
    T: ConfigStore + ProviderStore + SessionStore + HistoryStore + MemoryStore
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
