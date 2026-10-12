//! Turso 存储后端的端到端测试：五种子接口 + 中文全文检索 + 重开库持久化。
//!
//! 写入走后台队列、不阻塞调用方，因此「写后读」前要 `flush()`。

use serde_json::json;
use tower_agent_core::AgentMessage;
use tower_agent_provider::{Message, UserContent, UserMessage};
use tower_agent_storage::{
    ConfigStore, HistoryStore, MemoryRecord, MemoryStore, ProviderCredential, ProviderStore,
    SessionRecord, SessionStore,
};
use tower_agent_turso::TursoStorage;

fn memory(id: &str, content: &str) -> MemoryRecord {
    MemoryRecord {
        id: id.to_owned(),
        content: content.to_owned(),
        tags: vec!["test".to_owned()],
        scope: Some("global".to_owned()),
        created_at: 1,
    }
}

fn user_message(text: &str, timestamp: i64) -> AgentMessage {
    AgentMessage::message(Message::User(UserMessage {
        content: UserContent::Text(text.to_owned()),
        timestamp,
    }))
}

#[tokio::test]
async fn config_round_trips_json() {
    let storage = TursoStorage::in_memory().await.expect("open in-memory");

    assert_eq!(storage.get_config("model").await.unwrap(), None);
    storage
        .set_config("model", json!({ "id": "gpt", "temp": 0.2 }))
        .await
        .unwrap();
    storage.flush().await.unwrap();
    assert_eq!(
        storage.get_config("model").await.unwrap(),
        Some(json!({ "id": "gpt", "temp": 0.2 }))
    );

    storage.set_config("theme", json!("dark")).await.unwrap();
    storage.flush().await.unwrap();
    assert_eq!(
        storage.list_config().await.unwrap(),
        vec!["model".to_owned(), "theme".to_owned()]
    );

    storage.delete_config("model").await.unwrap();
    storage.flush().await.unwrap();
    assert_eq!(storage.get_config("model").await.unwrap(), None);
}

#[tokio::test]
async fn provider_credentials_round_trip() {
    let storage = TursoStorage::in_memory().await.expect("open in-memory");
    let credential = ProviderCredential {
        provider: "deepseek".to_owned(),
        api_key: "sk-secret".to_owned(),
        model: Some("deepseek-chat".to_owned()),
        base_url: None,
        updated_at: 42,
    };
    storage.put_provider(credential.clone()).await.unwrap();
    storage.flush().await.unwrap();

    assert_eq!(
        storage.get_provider("deepseek").await.unwrap(),
        Some(credential.clone())
    );
    assert_eq!(storage.list_providers().await.unwrap(), vec![credential]);

    storage.remove_provider("deepseek").await.unwrap();
    storage.flush().await.unwrap();
    assert_eq!(storage.get_provider("deepseek").await.unwrap(), None);
}

#[tokio::test]
async fn sessions_and_history_round_trip() {
    let storage = TursoStorage::in_memory().await.expect("open in-memory");
    let session = SessionRecord {
        id: "s1".to_owned(),
        title: Some("中文会话".to_owned()),
        model: Some("deepseek-chat".to_owned()),
        created_at: 1,
        updated_at: 2,
        metadata: json!({ "pinned": true }),
    };
    storage.upsert_session(session.clone()).await.unwrap();
    storage.flush().await.unwrap();
    assert_eq!(storage.get_session("s1").await.unwrap(), Some(session));

    storage
        .append_message("s1", user_message("你好", 10))
        .await
        .unwrap();
    // 自定义消息（不透明 JSON）也要能原样存取。
    storage
        .append_message(
            "s1",
            AgentMessage::custom(json!({ "role": "note", "v": 1 })),
        )
        .await
        .unwrap();
    storage.flush().await.unwrap();
    let loaded = storage.load_messages("s1").await.unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0], user_message("你好", 10));
    assert_eq!(
        loaded[1],
        AgentMessage::custom(json!({ "role": "note", "v": 1 }))
    );

    // 覆盖式替换（compaction）。
    storage
        .replace_messages("s1", vec![user_message("只有一条", 20)])
        .await
        .unwrap();
    storage.flush().await.unwrap();
    assert_eq!(
        storage.load_messages("s1").await.unwrap(),
        vec![user_message("只有一条", 20)]
    );

    // 删除会话要连历史一起删。
    storage.delete_session("s1").await.unwrap();
    storage.flush().await.unwrap();
    assert_eq!(storage.get_session("s1").await.unwrap(), None);
    assert!(storage.load_messages("s1").await.unwrap().is_empty());
}

#[tokio::test]
async fn memory_recall_is_chinese_aware() {
    let storage = TursoStorage::in_memory().await.expect("open in-memory");
    storage
        .remember(memory("a", "我们用 turso 做中文全文检索"))
        .await
        .unwrap();
    storage
        .remember(memory("b", "今天天气很好，适合散步"))
        .await
        .unwrap();
    storage
        .remember(memory("c", "the quick brown fox jumps"))
        .await
        .unwrap();
    storage.flush().await.unwrap();

    // 中文查询词按「词」而不是「字」命中：检索 → a。
    let hits = storage.recall("全文检索", 10).await.unwrap();
    assert_eq!(
        hits.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["a"]
    );
    // 原文保留。
    assert_eq!(hits[0].content, "我们用 turso 做中文全文检索");

    let hits = storage.recall("天气", 10).await.unwrap();
    assert_eq!(
        hits.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["b"]
    );

    // 英文查询走同一套分词（词干化后可命中）。
    let hits = storage.recall("fox", 10).await.unwrap();
    assert_eq!(
        hits.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["c"]
    );

    // 全是停用词的查询退化成「最近记忆」，不 panic、不空手而归。
    assert_eq!(storage.recall("the and of", 10).await.unwrap().len(), 3);

    assert_eq!(storage.list_memories(10).await.unwrap().len(), 3);
    storage.forget("b").await.unwrap();
    storage.flush().await.unwrap();
    assert_eq!(storage.list_memories(10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn writes_apply_in_order_after_flush() {
    let storage = TursoStorage::in_memory().await.expect("open in-memory");
    // 先塞一批写，再一次性 flush：队列按序消费，最终状态是最后一次写入。
    for value in 0..32 {
        storage.set_config("counter", json!(value)).await.unwrap();
    }
    storage.flush().await.unwrap();
    assert_eq!(
        storage.get_config("counter").await.unwrap(),
        Some(json!(31))
    );
}

#[tokio::test]
async fn storage_persists_across_reopen() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("tower-agent.db");

    {
        let storage = TursoStorage::open(&path).await.expect("open file db");
        storage.set_config("language", json!("zh")).await.unwrap();
        storage
            .remember(memory("m1", "记得这次中文检索的结论"))
            .await
            .unwrap();
        // 退出前排空队列，确保落盘。
        storage.flush().await.unwrap();
    }

    let storage = TursoStorage::open(&path).await.expect("reopen file db");
    assert_eq!(
        storage.get_config("language").await.unwrap(),
        Some(json!("zh"))
    );
    let hits = storage.recall("检索", 10).await.unwrap();
    assert_eq!(
        hits.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["m1"]
    );
}
