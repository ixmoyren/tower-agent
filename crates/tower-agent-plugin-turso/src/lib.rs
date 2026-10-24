//! tower-agent 的**默认存储后端**：内嵌 [Turso]（Rust 重写的 SQLite 兼容引擎）。
//!
//! 实现 [`tower_agent_plugin_sdk::storage`] 的六个子接口（配置 / 凭据 / 会话 / 历史 /
//! 记忆 / 脚本），并把它包成可被同名动态插件顶替的静态插件。
//!
//! # 为什么不是 SurrealDB
//!
//! 之前的默认后端是 SurrealDB（`tower-agent-surrealdb`）。改成 Turso 的原因：
//! Turso 是**单文件、进程内**的嵌入式引擎（无网络协议栈、无需守护进程），启动
//! 开销低；同时提供向量检索与全文检索（FTS）两套索引方法（index method）。
//!
//! # 中文全文检索
//!
//! Turso 的 FTS 只内建 `simple` / `whitespace` / `ngram` 等**不含中文分词**的
//! 分词器。中文直接索引会被当作整段匹配，几乎没有召回。因此本后端在应用层
//! 先分词、空格拼接，再交给 Turso 的 `simple` 分词器索引（实现见 [`tokenizer`]）：
//!
//! - 写入：`memory.content` 存**原文**，`memory.tokens` 存**分词后的检索文本**；
//! - 索引：`CREATE INDEX … USING fts (tokens) WITH (tokenizer = 'simple')`；
//! - 查询：`recall()` 用同一套分词器处理查询串，再走 `fts_match` / `fts_score`。
//!
//! # 写入队列（生产者 / 消费者）
//!
//! 写路径不直接落到 Turso，而是走一条**异步队列**：
//!
//! ```text
//!   调用方 / 分词阶段（生产者） ──enqueue──▶ 无界队列 ──▶ 后台 writer（消费者） ──▶ Turso
//! ```
//!
//! - 写入方法（`remember` / `append_message` / `set_config` …）在调用方完成
//!   **分词 / 序列化**（廉价、纯内存），把一条现成的 SQL + 绑定参数推进队列后
//!   **立即返回**——不等待、也不阻塞在 Turso 的磁盘 I/O 上；
//! - 一个后台任务独占**写连接**，把队列里的语句按序消费掉（单写者，天然有序）；
//! - 读走**另一条连接**，因此查询不会排在写连接的操作闸后面（见 [`TursoStorage::flush`]）。
//!
//! 代价是写是「最终一致」的：`remember().await` 返回时数据**可能还没落库**。
//! 需要读己之写、或需要在退出前确保落盘时，调用 [`TursoStorage::flush`] 等一下
//! 队列排空即可。
//!
//! [Turso]: https://github.com/tursodatabase/turso

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use semver::Version;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value as Json;
use tokio::sync::{mpsc, oneshot};
use tower_agent_core::AgentMessage;
use tower_agent_plugin_sdk::storage::{
    ConfigScope, ConfigStore, HistoryStore, MemoryRecord, MemoryStore, ProviderCredential,
    ProviderStore, ScriptRecord, ScriptStore, SessionRecord, SessionStore, StorageError,
    StorageFuture, StoragePlugin, StorageResult,
};
use turso::{Connection, Row, Value as TursoValue};

pub mod tokenizer;

pub use tokenizer::{Lang, Tokenized, Tokenizer, detect_language};

/// 后端（插件）名。
pub const BACKEND_NAME: &str = "turso";

/// 数据库 schema。所有 DDL 都幂等（`IF NOT EXISTS`），每次打开时执行一遍。
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS config (\
    scope_kind TEXT NOT NULL,\
    scope_id   TEXT NOT NULL,\
    key        TEXT NOT NULL,\
    value      TEXT NOT NULL,\
    PRIMARY KEY (scope_kind, scope_id, key)\
);\
CREATE TABLE IF NOT EXISTS provider (\
    provider   TEXT PRIMARY KEY,\
    api_key    TEXT NOT NULL,\
    model      TEXT,\
    base_url   TEXT,\
    updated_at INTEGER NOT NULL\
);\
CREATE TABLE IF NOT EXISTS session (\
    id         TEXT PRIMARY KEY,\
    title      TEXT,\
    model      TEXT,\
    created_at INTEGER NOT NULL,\
    updated_at INTEGER NOT NULL,\
    metadata   TEXT NOT NULL\
);\
CREATE TABLE IF NOT EXISTS message (\
    id         INTEGER PRIMARY KEY AUTOINCREMENT,\
    session_id TEXT NOT NULL,\
    payload    TEXT NOT NULL\
);\
CREATE INDEX IF NOT EXISTS message_session ON message (session_id, id);\
CREATE TABLE IF NOT EXISTS script (\
    id          TEXT NOT NULL,\
    scope_kind  TEXT NOT NULL,\
    scope_id    TEXT NOT NULL,\
    name        TEXT NOT NULL,\
    language    TEXT NOT NULL,\
    source      TEXT NOT NULL,\
    enabled     INTEGER NOT NULL,\
    permissions TEXT NOT NULL,\
    metadata    TEXT NOT NULL,\
    created_at  INTEGER NOT NULL,\
    updated_at  INTEGER NOT NULL,\
    source_hash TEXT NOT NULL,\
    PRIMARY KEY (scope_kind, scope_id, name)\
);\
CREATE INDEX IF NOT EXISTS script_scope_updated \
    ON script (scope_kind, scope_id, updated_at DESC, name ASC);\
CREATE TABLE IF NOT EXISTS memory (\
    id         TEXT PRIMARY KEY,\
    content    TEXT NOT NULL,\
    tokens     TEXT NOT NULL,\
    tags       TEXT NOT NULL,\
    scope      TEXT,\
    created_at INTEGER NOT NULL\
);\
CREATE INDEX IF NOT EXISTS memory_tokens_fts ON memory USING fts (tokens) WITH (tokenizer = 'simple');";

/// 记忆召回查询。
///
/// 形状与 Turso 内建的 FTS 查询模式一一对应（`WHERE fts_match … ORDER BY
/// fts_score … LIMIT`）：`?1` 是分词后的查询串，`?2` 是条数上限。选择列表里带
/// `fts_score(tokens, ?1) AS score`，`ORDER BY score` 就能命中索引。
const RECALL_SQL: &str = "\
SELECT id, content, tags, scope, created_at, fts_score(tokens, ?1) AS score \
FROM memory \
WHERE fts_match(tokens, ?1) \
ORDER BY score DESC \
LIMIT ?2";

const SQL_SET_CONFIG: &str = "INSERT OR REPLACE INTO config (scope_kind, scope_id, key, value) \
     VALUES (?1, ?2, ?3, ?4)";
const SQL_GET_CONFIG: &str = "SELECT value FROM config \
     WHERE scope_kind = ?1 AND scope_id = ?2 AND key = ?3";
const SQL_DELETE_CONFIG: &str = "DELETE FROM config \
     WHERE scope_kind = ?1 AND scope_id = ?2 AND key = ?3";
const SQL_LIST_CONFIG: &str = "SELECT key FROM config \
     WHERE scope_kind = ?1 AND scope_id = ?2 ORDER BY key";

const SQL_PUT_PROVIDER: &str = "INSERT OR REPLACE INTO provider \
     (provider, api_key, model, base_url, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)";
const SQL_GET_PROVIDER: &str = "SELECT provider, api_key, model, base_url, updated_at \
     FROM provider WHERE provider = ?1";
const SQL_LIST_PROVIDERS: &str = "SELECT provider, api_key, model, base_url, updated_at \
     FROM provider ORDER BY provider";
const SQL_REMOVE_PROVIDER: &str = "DELETE FROM provider WHERE provider = ?1";

const SQL_UPSERT_SESSION: &str = "INSERT OR REPLACE INTO session \
     (id, title, model, created_at, updated_at, metadata) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
const SQL_GET_SESSION: &str = "SELECT id, title, model, created_at, updated_at, metadata \
     FROM session WHERE id = ?1";
const SQL_LIST_SESSIONS: &str = "SELECT id, title, model, created_at, updated_at, metadata \
     FROM session ORDER BY updated_at DESC, id DESC";
const SQL_DELETE_SESSION: &str = "DELETE FROM session WHERE id = ?1";

const SQL_UPSERT_SCRIPT: &str = "INSERT OR REPLACE INTO script \
     (id, scope_kind, scope_id, name, language, source, enabled, permissions, metadata, \
      created_at, updated_at, source_hash) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)";
const SQL_GET_SCRIPT: &str = "SELECT id, scope_kind, scope_id, name, language, source, enabled, \
     permissions, metadata, created_at, updated_at, source_hash \
     FROM script WHERE scope_kind = ?1 AND scope_id = ?2 AND name = ?3";
const SQL_LIST_SCRIPTS: &str = "SELECT id, scope_kind, scope_id, name, language, source, enabled, \
     permissions, metadata, created_at, updated_at, source_hash \
     FROM script WHERE scope_kind = ?1 AND scope_id = ?2 \
     ORDER BY updated_at DESC, name ASC";
const SQL_DELETE_SCRIPT: &str =
    "DELETE FROM script WHERE scope_kind = ?1 AND scope_id = ?2 AND name = ?3";

const SQL_APPEND_MESSAGE: &str = "INSERT INTO message (session_id, payload) VALUES (?1, ?2)";
const SQL_LOAD_MESSAGES: &str = "SELECT payload FROM message WHERE session_id = ?1 ORDER BY id ASC";
const SQL_CLEAR_MESSAGES: &str = "DELETE FROM message WHERE session_id = ?1";

const SQL_REMEMBER: &str = "INSERT OR REPLACE INTO memory \
     (id, content, tokens, tags, scope, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
const SQL_LIST_MEMORIES: &str = "SELECT id, content, tags, scope, created_at \
     FROM memory ORDER BY created_at DESC LIMIT ?1";
const SQL_FORGET: &str = "DELETE FROM memory WHERE id = ?1";

/// 排队中的写操作（写方法把「现成的 SQL + 参数」推进来，后台任务只负责执行）。
enum WriteOp {
    /// 单条参数化语句。
    Statement {
        sql: &'static str,
        params: Vec<TursoValue>,
    },
    /// 一个事务里的多条语句（`sql`, `params`）。
    Transaction(Vec<(String, Vec<TursoValue>)>),
    /// 队列屏障：消费者处理到它时回执，用于 [`TursoStorage::flush`]。
    Flush(oneshot::Sender<()>),
}

/// 内嵌 Turso 的存储后端。
///
/// 写走后台队列（见 crate 文档），读走独立的连接；两者都是 `Send + Sync`，
/// 因此后端可以放进 `Arc<dyn Storage>` 在多任务间共享。
pub struct TursoStorage {
    /// 读连接：查询不排在写连接后面。
    read: Connection,
    /// 应用层分词器（写入侧切检索文本、查询侧切查询串）。
    tokenizer: Arc<Tokenizer>,
    /// 写队列的发送端（生产者入口）。
    writes: mpsc::UnboundedSender<WriteOp>,
    /// 后台消费者记录的、尚未上报的第一个写入错误。
    write_error: Arc<Mutex<Option<StorageError>>>,
}

impl TursoStorage {
    /// 打开（不存在则新建）指定路径的数据库，建好 schema 并拉起后台写任务。
    pub async fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                StorageError::Backend(format!("create storage directory {parent:?}: {error}"))
            })?;
        }
        let database = turso::Builder::new_local(&path.to_string_lossy())
            .experimental_index_method(true)
            .build()
            .await
            .map_err(backend)?;
        Self::from_database(database).await
    }

    /// 打开一个纯内存数据库（测试 / 临时会话用）。
    pub async fn in_memory() -> StorageResult<Self> {
        let database = turso::Builder::new_local(":memory:")
            .experimental_index_method(true)
            .build()
            .await
            .map_err(backend)?;
        Self::from_database(database).await
    }

    /// 打开默认位置的数据库（`dirs::data_local_dir()/tower-agent/storage.db`）。
    pub async fn open_default() -> StorageResult<Self> {
        Self::open(Self::default_path()).await
    }

    /// 默认数据库文件路径。
    #[must_use]
    pub fn default_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tower-agent")
            .join("storage.db")
    }

    /// 把后端包成静态存储插件（清单里给 `persist.turso` / `memory.turso` 两项能力）。
    ///
    /// 插件**不标 `protected`**，因此同名的动态 / JIT 插件可以顶替它——这正是
    /// 「Turso 只是默认实现」的机制保证。
    #[must_use]
    pub fn plugin(self) -> StoragePlugin {
        StoragePlugin::new(BACKEND_NAME, Version::new(1, 0, 0), Arc::new(self))
    }

    /// 等写队列排空，然后返回这段时间里**第一个**写入错误（没有就返回 `Ok`）。
    ///
    /// 写入是异步的（不阻塞调用方），所以要「读己之写」或要在退出前确保落盘时，
    /// 先 `flush().await`。它也是唯一能拿到后台写入错误的地方。
    pub fn flush(&self) -> StorageFuture<'_, ()> {
        Box::pin(async move {
            let (reply, wait) = oneshot::channel();
            self.enqueue(WriteOp::Flush(reply))?;
            // 消费者关闭时 `wait` 会直接报错；那说明写任务已经没了，按队列关闭处理。
            wait.await
                .map_err(|_| StorageError::Backend("turso write queue is closed".to_owned()))?;
            self.take_write_error()
        })
    }

    /// 生产者入口：把一条现成的写操作推进队列，立即返回。
    fn enqueue(&self, op: WriteOp) -> StorageResult<()> {
        self.writes
            .send(op)
            .map_err(|_| StorageError::Backend("turso write queue is closed".to_owned()))
    }

    /// 取出并清空后台记录的写入错误。
    fn take_write_error(&self) -> StorageResult<()> {
        let mut slot = self
            .write_error
            .lock()
            .expect("turso write error slot is never poisoned");
        match slot.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// 建读 / 写两条连接、初始化 schema、拉起后台写任务。
    async fn from_database(database: turso::Database) -> StorageResult<Self> {
        let read = database.connect().map_err(backend)?;
        let write = database.connect().map_err(backend)?;
        write.execute_batch(SCHEMA).await.map_err(backend)?;

        let (writes, queue) = mpsc::unbounded_channel();
        let write_error = Arc::new(Mutex::new(None));
        tokio::spawn(consume_writes(queue, write, Arc::clone(&write_error)));

        Ok(Self {
            read,
            tokenizer: Arc::new(Tokenizer::new()),
            writes,
            write_error,
        })
    }
}

/// 后台消费者：按序执行写队列里的语句，把第一个错误记进 `write_error`。
async fn consume_writes(
    mut queue: mpsc::UnboundedReceiver<WriteOp>,
    conn: Connection,
    write_error: Arc<Mutex<Option<StorageError>>>,
) {
    while let Some(op) = queue.recv().await {
        let outcome = match op {
            WriteOp::Statement { sql, params } => {
                conn.execute(sql, params).await.map(|_| ()).map_err(backend)
            }
            WriteOp::Transaction(statements) => transaction(&conn, statements).await,
            WriteOp::Flush(reply) => {
                let _ = reply.send(());
                continue;
            }
        };
        if let Err(error) = outcome {
            let mut slot = write_error
                .lock()
                .expect("turso write error slot is never poisoned");
            // 只保留第一个未上报的错误，避免后到的错误盖掉根因。
            if slot.is_none() {
                *slot = Some(error);
            }
        }
    }
}

/// 在一个事务里执行多条语句。
async fn transaction(
    conn: &Connection,
    statements: Vec<(String, Vec<TursoValue>)>,
) -> StorageResult<()> {
    let mut batch = Vec::with_capacity(statements.len());
    for (sql, params) in statements {
        batch.push(turso::BatchStatement::new(sql, params).map_err(backend)?);
    }
    conn.transactional_batch(batch, turso::transaction::TransactionBehavior::Immediate)
        .await
        .map(|_| ())
        .map_err(backend)
}

impl ConfigStore for TursoStorage {
    fn get_config(&self, scope: &ConfigScope, key: &str) -> StorageFuture<'_, Option<Json>> {
        let (kind, id) = scope.storage_parts();
        let id = id.to_owned();
        let key = key.to_owned();
        Box::pin(async move {
            let mut rows = self
                .read
                .query(SQL_GET_CONFIG, vec![text(kind), text(id), text(key)])
                .await
                .map_err(backend)?;
            let Some(row) = rows.next().await.map_err(backend)? else {
                return Ok(None);
            };
            Ok(Some(deserialize(&column_text(&row, 0)?)?))
        })
    }

    fn set_config(&self, scope: &ConfigScope, key: &str, value: Json) -> StorageFuture<'_, ()> {
        let (kind, id) = scope.storage_parts();
        let id = id.to_owned();
        let key = key.to_owned();
        Box::pin(async move {
            let value = serialize(&value)?;
            self.enqueue(WriteOp::Statement {
                sql: SQL_SET_CONFIG,
                params: vec![text(kind), text(id), text(key), text(value)],
            })
        })
    }

    fn delete_config(&self, scope: &ConfigScope, key: &str) -> StorageFuture<'_, ()> {
        let (kind, id) = scope.storage_parts();
        let id = id.to_owned();
        let key = key.to_owned();
        Box::pin(async move {
            self.enqueue(WriteOp::Statement {
                sql: SQL_DELETE_CONFIG,
                params: vec![text(kind), text(id), text(key)],
            })
        })
    }

    fn list_config(&self, scope: &ConfigScope) -> StorageFuture<'_, Vec<String>> {
        let (kind, id) = scope.storage_parts();
        let id = id.to_owned();
        Box::pin(async move {
            let mut rows = self
                .read
                .query(SQL_LIST_CONFIG, vec![text(kind), text(id)])
                .await
                .map_err(backend)?;
            let mut keys = Vec::new();
            while let Some(row) = rows.next().await.map_err(backend)? {
                keys.push(column_text(&row, 0)?);
            }
            Ok(keys)
        })
    }
}

impl ProviderStore for TursoStorage {
    fn put_provider(&self, credential: ProviderCredential) -> StorageFuture<'_, ()> {
        Box::pin(async move {
            self.enqueue(WriteOp::Statement {
                sql: SQL_PUT_PROVIDER,
                params: vec![
                    text(credential.provider),
                    text(credential.api_key),
                    optional(credential.model),
                    optional(credential.base_url),
                    integer(credential.updated_at),
                ],
            })
        })
    }

    fn get_provider(&self, provider: &str) -> StorageFuture<'_, Option<ProviderCredential>> {
        let provider = provider.to_owned();
        Box::pin(async move {
            let mut rows = self
                .read
                .query(SQL_GET_PROVIDER, vec![text(provider)])
                .await
                .map_err(backend)?;
            let Some(row) = rows.next().await.map_err(backend)? else {
                return Ok(None);
            };
            Ok(Some(provider_from_row(&row)?))
        })
    }

    fn list_providers(&self) -> StorageFuture<'_, Vec<ProviderCredential>> {
        Box::pin(async move {
            let mut rows = self
                .read
                .query(SQL_LIST_PROVIDERS, ())
                .await
                .map_err(backend)?;
            let mut providers = Vec::new();
            while let Some(row) = rows.next().await.map_err(backend)? {
                providers.push(provider_from_row(&row)?);
            }
            Ok(providers)
        })
    }

    fn remove_provider(&self, provider: &str) -> StorageFuture<'_, ()> {
        let provider = provider.to_owned();
        Box::pin(async move {
            self.enqueue(WriteOp::Statement {
                sql: SQL_REMOVE_PROVIDER,
                params: vec![text(provider)],
            })
        })
    }
}

impl SessionStore for TursoStorage {
    fn upsert_session(&self, session: SessionRecord) -> StorageFuture<'_, ()> {
        Box::pin(async move {
            let metadata = serialize(&session.metadata)?;
            self.enqueue(WriteOp::Statement {
                sql: SQL_UPSERT_SESSION,
                params: vec![
                    text(session.id),
                    optional(session.title),
                    optional(session.model),
                    integer(session.created_at),
                    integer(session.updated_at),
                    text(metadata),
                ],
            })
        })
    }

    fn get_session(&self, id: &str) -> StorageFuture<'_, Option<SessionRecord>> {
        let id = id.to_owned();
        Box::pin(async move {
            let mut rows = self
                .read
                .query(SQL_GET_SESSION, vec![text(id)])
                .await
                .map_err(backend)?;
            let Some(row) = rows.next().await.map_err(backend)? else {
                return Ok(None);
            };
            Ok(Some(session_from_row(&row)?))
        })
    }

    fn list_sessions(&self) -> StorageFuture<'_, Vec<SessionRecord>> {
        Box::pin(async move {
            let mut rows = self
                .read
                .query(SQL_LIST_SESSIONS, ())
                .await
                .map_err(backend)?;
            let mut sessions = Vec::new();
            while let Some(row) = rows.next().await.map_err(backend)? {
                sessions.push(session_from_row(&row)?);
            }
            Ok(sessions)
        })
    }

    fn delete_session(&self, id: &str) -> StorageFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move {
            // 删会话要连它的历史一起删：放进同一个事务。
            self.enqueue(WriteOp::Transaction(vec![
                (SQL_CLEAR_MESSAGES.to_owned(), vec![text(id.clone())]),
                (SQL_DELETE_SESSION.to_owned(), vec![text(id)]),
            ]))
        })
    }
}

impl ScriptStore for TursoStorage {
    fn upsert_script(&self, script: ScriptRecord) -> StorageFuture<'_, ()> {
        let (scope_kind, scope_id) = script.scope.storage_parts();
        let scope_kind = scope_kind.to_owned();
        let scope_id = scope_id.to_owned();
        Box::pin(async move {
            let permissions = serialize(&script.permissions)?;
            let metadata = serialize(&script.metadata)?;
            self.enqueue(WriteOp::Statement {
                sql: SQL_UPSERT_SCRIPT,
                params: vec![
                    text(script.id),
                    text(scope_kind),
                    text(scope_id),
                    text(script.name),
                    text(script.language),
                    text(script.source),
                    integer(i64::from(script.enabled)),
                    text(permissions),
                    text(metadata),
                    integer(script.created_at),
                    integer(script.updated_at),
                    text(script.source_hash),
                ],
            })
        })
    }

    fn get_script(
        &self,
        scope: &ConfigScope,
        name: &str,
    ) -> StorageFuture<'_, Option<ScriptRecord>> {
        let (kind, id) = scope.storage_parts();
        let kind = kind.to_owned();
        let id = id.to_owned();
        let name = name.to_owned();
        Box::pin(async move {
            let mut rows = self
                .read
                .query(SQL_GET_SCRIPT, vec![text(kind), text(id), text(name)])
                .await
                .map_err(backend)?;
            let Some(row) = rows.next().await.map_err(backend)? else {
                return Ok(None);
            };
            Ok(Some(script_from_row(&row)?))
        })
    }

    fn list_scripts(&self, scope: &ConfigScope) -> StorageFuture<'_, Vec<ScriptRecord>> {
        let (kind, id) = scope.storage_parts();
        let kind = kind.to_owned();
        let id = id.to_owned();
        Box::pin(async move {
            let mut rows = self
                .read
                .query(SQL_LIST_SCRIPTS, vec![text(kind), text(id)])
                .await
                .map_err(backend)?;
            let mut scripts = Vec::new();
            while let Some(row) = rows.next().await.map_err(backend)? {
                scripts.push(script_from_row(&row)?);
            }
            Ok(scripts)
        })
    }

    fn delete_script(&self, scope: &ConfigScope, name: &str) -> StorageFuture<'_, ()> {
        let (kind, id) = scope.storage_parts();
        let kind = kind.to_owned();
        let id = id.to_owned();
        let name = name.to_owned();
        Box::pin(async move {
            self.enqueue(WriteOp::Statement {
                sql: SQL_DELETE_SCRIPT,
                params: vec![text(kind), text(id), text(name)],
            })
        })
    }
}

impl HistoryStore for TursoStorage {
    fn append_message(&self, session_id: &str, message: AgentMessage) -> StorageFuture<'_, ()> {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            let payload = serialize(&message)?;
            self.enqueue(WriteOp::Statement {
                sql: SQL_APPEND_MESSAGE,
                params: vec![text(session_id), text(payload)],
            })
        })
    }

    fn load_messages(&self, session_id: &str) -> StorageFuture<'_, Vec<AgentMessage>> {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            let mut rows = self
                .read
                .query(SQL_LOAD_MESSAGES, vec![text(session_id)])
                .await
                .map_err(backend)?;
            let mut messages = Vec::new();
            while let Some(row) = rows.next().await.map_err(backend)? {
                messages.push(deserialize(&column_text(&row, 0)?)?);
            }
            Ok(messages)
        })
    }

    fn replace_messages(
        &self,
        session_id: &str,
        messages: Vec<AgentMessage>,
    ) -> StorageFuture<'_, ()> {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            let mut statements = Vec::with_capacity(messages.len() + 1);
            statements.push((
                SQL_CLEAR_MESSAGES.to_owned(),
                vec![text(session_id.clone())],
            ));
            for message in &messages {
                statements.push((
                    SQL_APPEND_MESSAGE.to_owned(),
                    vec![text(session_id.clone()), text(serialize(message)?)],
                ));
            }
            self.enqueue(WriteOp::Transaction(statements))
        })
    }

    fn clear_messages(&self, session_id: &str) -> StorageFuture<'_, ()> {
        let session_id = session_id.to_owned();
        Box::pin(async move {
            self.enqueue(WriteOp::Statement {
                sql: SQL_CLEAR_MESSAGES,
                params: vec![text(session_id)],
            })
        })
    }
}

impl MemoryStore for TursoStorage {
    fn remember(&self, record: MemoryRecord) -> StorageFuture<'_, ()> {
        Box::pin(async move {
            // 生产者阶段：在这里把原文切成检索文本（纯内存、廉价），再推队列。
            let tokenized = self.tokenizer.tokenize(&record.content);
            let tags = serialize(&record.tags)?;
            self.enqueue(WriteOp::Statement {
                sql: SQL_REMEMBER,
                params: vec![
                    text(record.id),
                    text(tokenized.original()),
                    text(tokenized.indexed()),
                    text(tags),
                    optional(record.scope),
                    integer(record.created_at),
                ],
            })
        })
    }

    fn recall(&self, query: &str, limit: usize) -> StorageFuture<'_, Vec<MemoryRecord>> {
        let query = query.to_owned();
        Box::pin(async move {
            let terms = self.tokenizer.tokenize_query(&query);
            if terms.is_empty() {
                // 查询全被停用词 / 标点吃掉了：退化成「最近记忆」。
                return self.list_memories(limit).await;
            }
            let mut rows = self
                .read
                .query(RECALL_SQL, vec![text(terms), integer(limit as i64)])
                .await
                .map_err(backend)?;
            let mut memories = Vec::new();
            while let Some(row) = rows.next().await.map_err(backend)? {
                memories.push(memory_from_row(&row)?);
            }
            Ok(memories)
        })
    }

    fn list_memories(&self, limit: usize) -> StorageFuture<'_, Vec<MemoryRecord>> {
        Box::pin(async move {
            let mut rows = self
                .read
                .query(SQL_LIST_MEMORIES, vec![integer(limit as i64)])
                .await
                .map_err(backend)?;
            let mut memories = Vec::new();
            while let Some(row) = rows.next().await.map_err(backend)? {
                memories.push(memory_from_row(&row)?);
            }
            Ok(memories)
        })
    }

    fn forget(&self, id: &str) -> StorageFuture<'_, ()> {
        let id = id.to_owned();
        Box::pin(async move {
            self.enqueue(WriteOp::Statement {
                sql: SQL_FORGET,
                params: vec![text(id)],
            })
        })
    }
}

/// 把一个 `turso::Error` 包成后端错误。
fn backend(error: turso::Error) -> StorageError {
    StorageError::Backend(error.to_string())
}

/// JSON 编码；失败即序列化错误。
fn serialize<T: Serialize>(value: &T) -> StorageResult<String> {
    serde_json::to_string(value).map_err(|error| StorageError::Serialization(error.to_string()))
}

/// JSON 解码；失败即序列化错误。
fn deserialize<T: DeserializeOwned>(raw: &str) -> StorageResult<T> {
    serde_json::from_str(raw).map_err(|error| StorageError::Serialization(error.to_string()))
}

/// 文本绑定值。
fn text(value: impl Into<String>) -> TursoValue {
    TursoValue::Text(value.into())
}

/// `Option<String>` → 可绑定的 SQL 值。
fn optional(value: Option<String>) -> TursoValue {
    match value {
        Some(text) => TursoValue::Text(text),
        None => TursoValue::Null,
    }
}

/// 整数绑定值。
fn integer(value: i64) -> TursoValue {
    TursoValue::Integer(value)
}

/// 读一列文本（`NOT NULL` 列）。
fn column_text(row: &Row, index: usize) -> StorageResult<String> {
    match row.get_value(index).map_err(backend)? {
        TursoValue::Text(text) => Ok(text),
        other => Err(StorageError::Serialization(format!(
            "column {index} is not text: {other:?}"
        ))),
    }
}

/// 读一列可空文本。
fn column_opt_text(row: &Row, index: usize) -> StorageResult<Option<String>> {
    match row.get_value(index).map_err(backend)? {
        TursoValue::Text(text) => Ok(Some(text)),
        TursoValue::Null => Ok(None),
        other => Err(StorageError::Serialization(format!(
            "column {index} is not text: {other:?}"
        ))),
    }
}

/// 读一列整数。
fn column_i64(row: &Row, index: usize) -> StorageResult<i64> {
    match row.get_value(index).map_err(backend)? {
        TursoValue::Integer(value) => Ok(value),
        other => Err(StorageError::Serialization(format!(
            "column {index} is not an integer: {other:?}"
        ))),
    }
}

/// `provider` 表一行 → 凭据。列顺序与 `SQL_GET_PROVIDER` / `SQL_LIST_PROVIDERS` 一致。
fn provider_from_row(row: &Row) -> StorageResult<ProviderCredential> {
    Ok(ProviderCredential {
        provider: column_text(row, 0)?,
        api_key: column_text(row, 1)?,
        model: column_opt_text(row, 2)?,
        base_url: column_opt_text(row, 3)?,
        updated_at: column_i64(row, 4)?,
    })
}

/// `session` 表一行 → 会话记录。列顺序与 `SQL_GET_SESSION` / `SQL_LIST_SESSIONS` 一致。
fn session_from_row(row: &Row) -> StorageResult<SessionRecord> {
    let metadata = deserialize(&column_text(row, 5)?)?;
    Ok(SessionRecord {
        id: column_text(row, 0)?,
        title: column_opt_text(row, 1)?,
        model: column_opt_text(row, 2)?,
        created_at: column_i64(row, 3)?,
        updated_at: column_i64(row, 4)?,
        metadata,
    })
}

/// `script` 表一行 → 脚本记录。
fn script_from_row(row: &Row) -> StorageResult<ScriptRecord> {
    let scope_kind = column_text(row, 1)?;
    let scope_id = column_text(row, 2)?;
    let scope = match (scope_kind.as_str(), scope_id.as_str()) {
        ("user", "") => ConfigScope::User,
        ("project", id) => ConfigScope::Project(id.to_owned()),
        _ => {
            return Err(StorageError::Serialization(format!(
                "invalid script scope ({scope_kind}, {scope_id})"
            )));
        }
    };
    let language = column_text(row, 4)?;
    let enabled = match column_i64(row, 6)? {
        0 => false,
        1 => true,
        other => {
            return Err(StorageError::Serialization(format!(
                "invalid script enabled flag: {other}"
            )));
        }
    };
    Ok(ScriptRecord {
        id: column_text(row, 0)?,
        name: column_text(row, 3)?,
        language,
        scope,
        source: column_text(row, 5)?,
        enabled,
        permissions: deserialize(&column_text(row, 7)?)?,
        metadata: deserialize(&column_text(row, 8)?)?,
        created_at: column_i64(row, 9)?,
        updated_at: column_i64(row, 10)?,
        source_hash: column_text(row, 11)?,
    })
}

/// `memory` 表一行 → 记忆。列顺序与 `RECALL_SQL` / `SQL_LIST_MEMORIES` 一致。
fn memory_from_row(row: &Row) -> StorageResult<MemoryRecord> {
    Ok(MemoryRecord {
        id: column_text(row, 0)?,
        content: column_text(row, 1)?,
        tags: deserialize(&column_text(row, 2)?)?,
        scope: column_opt_text(row, 3)?,
        created_at: column_i64(row, 4)?,
    })
}
