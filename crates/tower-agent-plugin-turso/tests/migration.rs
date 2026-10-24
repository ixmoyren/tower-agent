//! Storage manager migration tests across two independent Turso backends.

use std::sync::Arc;

use serde_json::json;
use tower_agent_core::AgentMessage;
use tower_agent_plugin_sdk::storage::{
    ConfigScope, ConfigStore, HistoryStore, MemoryRecord, MemoryStore, ProviderCredential,
    ProviderStore, ScriptRecord, ScriptStore, SessionRecord, SessionStore,
    StorageBackendRegistration, StorageError, StorageMaintenance, StorageManager,
};
use tower_agent_plugin_turso::TursoStorage;
use tower_agent_provider::{Message, UserContent, UserMessage};

fn backend(name: &str, storage: &Arc<TursoStorage>) -> StorageBackendRegistration {
    StorageBackendRegistration {
        name: name.to_owned(),
        version: semver::Version::new(1, 0, 0),
        storage: Arc::clone(storage) as Arc<dyn tower_agent_plugin_sdk::storage::Storage>,
    }
}

async fn populate(storage: &TursoStorage) {
    let project = ConfigScope::project("/work/a");
    storage
        .set_config(&ConfigScope::User, "theme", json!("dark"))
        .await
        .unwrap();
    storage
        .set_config(&project, "model", json!("m1"))
        .await
        .unwrap();
    storage
        .put_provider(ProviderCredential {
            provider: "openai".to_owned(),
            api_key: "secret".to_owned(),
            model: Some("m1".to_owned()),
            base_url: None,
            updated_at: 1,
        })
        .await
        .unwrap();
    storage
        .upsert_session(SessionRecord {
            id: "session-1".to_owned(),
            title: Some("test".to_owned()),
            model: Some("m1".to_owned()),
            created_at: 1,
            updated_at: 2,
            metadata: json!({ "cwd": "/work/a" }),
        })
        .await
        .unwrap();
    storage
        .append_message(
            "session-1",
            AgentMessage::message(Message::User(UserMessage {
                content: UserContent::Text("hello".to_owned()),
                timestamp: 1,
            })),
        )
        .await
        .unwrap();
    storage
        .remember(MemoryRecord {
            id: "memory-1".to_owned(),
            content: "remember this".to_owned(),
            tags: vec!["test".to_owned()],
            scope: Some("global".to_owned()),
            created_at: 1,
        })
        .await
        .unwrap();
    storage
        .upsert_script(ScriptRecord {
            id: "script-1".to_owned(),
            name: "script".to_owned(),
            language: "monty".to_owned(),
            scope: project,
            source: "value = 1\n".to_owned(),
            enabled: true,
            permissions: json!({ "state": true }),
            metadata: json!({}),
            created_at: 1,
            updated_at: 2,
            source_hash: "hash".to_owned(),
        })
        .await
        .unwrap();
    storage.flush().await.unwrap();
}

#[tokio::test]
async fn switches_all_domains_and_verifies_target() {
    let source = Arc::new(TursoStorage::in_memory().await.unwrap());
    let target = Arc::new(TursoStorage::in_memory().await.unwrap());
    populate(&source).await;
    let expected = source
        .export_snapshot()
        .await
        .expect("export source snapshot");

    let manager = StorageManager::new(
        [backend("source", &source), backend("target", &target)],
        "source",
    )
    .unwrap();
    manager.switch_to("target").await.expect("switch backend");

    assert_eq!(manager.active_backend().await, "target");
    let actual = target.export_snapshot().await.expect("export target");
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn selection_commit_failure_rolls_back_to_source() {
    let source = Arc::new(TursoStorage::in_memory().await.unwrap());
    let target = Arc::new(TursoStorage::in_memory().await.unwrap());
    populate(&source).await;
    let manager = StorageManager::new(
        [backend("source", &source), backend("target", &target)],
        "source",
    )
    .unwrap();

    let error = manager
        .switch_to_with("target", |_| {
            Err(StorageError::Migration("selection write failed".to_owned()))
        })
        .await
        .expect_err("selection failure must abort switch");
    assert!(error.to_string().contains("selection write failed"));
    assert_eq!(manager.active_backend().await, "source");
    assert!(
        target
            .export_snapshot()
            .await
            .expect("target snapshot")
            .is_empty(),
        "failed staging target must be cleaned up"
    );
}

#[tokio::test]
async fn non_empty_target_is_rejected_without_changing_active() {
    let source = Arc::new(TursoStorage::in_memory().await.unwrap());
    let target = Arc::new(TursoStorage::in_memory().await.unwrap());
    populate(&source).await;
    target
        .set_config(&ConfigScope::User, "existing", json!(true))
        .await
        .unwrap();
    target.flush().await.unwrap();

    let manager = StorageManager::new(
        [backend("source", &source), backend("target", &target)],
        "source",
    )
    .unwrap();
    let error = manager
        .switch_to("target")
        .await
        .expect_err("non-empty target must fail");
    assert!(error.to_string().contains("not empty"));
    assert_eq!(manager.active_backend().await, "source");
}
