//! 模型目录与 `Models::refresh`。

#[path = "provider_support/mod.rs"]
mod support;

use std::sync::Arc;

use serde_json::json;
use support::test_model;
use tower_agent_provider::{
    Models, PiError,
    models::{ModelSource, RefreshOptions},
};

fn model(provider: &str, id: &str, max_tokens: u64) -> tower_agent_provider::Model {
    tower_agent_provider::Model {
        provider: provider.into(),
        id: id.into(),
        name: id.into(),
        max_tokens,
        ..test_model()
    }
}

/// 返回固定目录的来源。
fn source(models: Vec<tower_agent_provider::Model>) -> ModelSource {
    Arc::new(move || {
        let models = models.clone();
        Box::pin(async move { Ok(models) })
    })
}

/// 总是失败的来源。
fn failing_source(message: &'static str) -> ModelSource {
    Arc::new(move || Box::pin(async move { Err(PiError::Transport(message.into())) }))
}

fn provider_summary(
    report: &tower_agent_provider::RefreshReport,
) -> Vec<(String, usize, usize, usize, bool)> {
    report
        .providers
        .iter()
        .map(|entry| {
            (
                entry.provider.clone(),
                entry.added,
                entry.updated,
                entry.removed,
                entry.error.is_some(),
            )
        })
        .collect()
}

#[tokio::test]
async fn refresh_replaces_the_catalog_and_counts_changes() {
    let mut models = Models::new();
    models.register_models(
        "anthropic",
        vec![
            model("anthropic", "claude-a", 100),
            model("anthropic", "claude-b", 200),
        ],
    );
    models.register_source(
        "anthropic",
        source(vec![
            // a 变化（updated）、b 不变、c 新增（added）。
            model("anthropic", "claude-a", 999),
            model("anthropic", "claude-b", 200),
            model("anthropic", "claude-c", 300),
        ]),
    );

    let report = models.refresh(RefreshOptions::new()).await;
    assert!(report.is_ok(), "got {report:?}");
    assert_eq!(
        provider_summary(&report),
        vec![("anthropic".to_string(), 1, 1, 0, false)]
    );
    assert_eq!(models.model_count(), 3);
    assert_eq!(
        models
            .get_model("anthropic", "claude-a")
            .map(|m| m.max_tokens),
        Some(999)
    );
    assert!(models.get_model("anthropic", "claude-c").is_some());
    assert_eq!(models.providers(), vec!["anthropic"]);
}

#[tokio::test]
async fn refresh_removes_models_missing_from_the_source() {
    let mut models = Models::new();
    models.register_models(
        "openai",
        vec![model("openai", "gpt-a", 1), model("openai", "gpt-gone", 2)],
    );
    models.register_source("openai", source(vec![model("openai", "gpt-a", 1)]));

    let report = models.refresh(RefreshOptions::new()).await;
    assert_eq!(
        provider_summary(&report),
        vec![("openai".to_string(), 0, 0, 1, false)]
    );
    assert_eq!(models.model_count(), 1);
    assert!(models.get_model("openai", "gpt-gone").is_none());
}

#[tokio::test]
async fn a_failing_source_keeps_the_previous_catalog() {
    let mut models = Models::new();
    models.register_models("google", vec![model("google", "gemini-a", 7)]);
    models.register_source("google", failing_source("catalog unreachable"));

    let report = models.refresh(RefreshOptions::new()).await;
    assert!(!report.is_ok(), "失败要体现在报告里");
    assert_eq!(
        provider_summary(&report),
        vec![("google".to_string(), 0, 0, 0, true)]
    );
    let errors = report.errors();
    assert_eq!(
        errors,
        vec![("google", "transport error: catalog unreachable")]
    );
    // 旧条目仍在。
    assert_eq!(
        models.get_model("google", "gemini-a").map(|m| m.max_tokens),
        Some(7)
    );
}

#[tokio::test]
async fn refresh_can_be_scoped_to_selected_providers() {
    let mut models = Models::new();
    models.register_models("anthropic", vec![model("anthropic", "old", 1)]);
    models.register_models("openai", vec![model("openai", "old", 1)]);
    models.register_source("anthropic", source(vec![model("anthropic", "new", 2)]));
    models.register_source("openai", source(vec![model("openai", "new", 2)]));

    let report = models
        .refresh(RefreshOptions {
            providers: Some(vec!["anthropic".into()]),
            ..RefreshOptions::new()
        })
        .await;
    assert_eq!(
        provider_summary(&report),
        vec![("anthropic".to_string(), 1, 0, 1, false)]
    );
    assert!(models.get_model("anthropic", "new").is_some());
    // 没被选中的 provider 保持原样。
    assert!(models.get_model("openai", "old").is_some());
}

#[tokio::test]
async fn allow_network_false_keeps_the_catalog_untouched() {
    let mut models = Models::new();
    models.register_models("anthropic", vec![model("anthropic", "old", 1)]);
    models.register_source("anthropic", source(vec![model("anthropic", "new", 2)]));

    let report = models
        .refresh(RefreshOptions {
            allow_network: false,
            ..RefreshOptions::new()
        })
        .await;
    assert!(report.providers.is_empty());
    assert!(models.get_model("anthropic", "old").is_some());
    assert!(models.get_model("anthropic", "new").is_none());
}

#[tokio::test]
async fn refresh_can_be_cancelled() {
    let mut models = Models::new();
    models.register_source("anthropic", source(vec![model("anthropic", "a", 1)]));
    models.register_source("openai", source(vec![model("openai", "b", 1)]));

    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();
    let report = models
        .refresh(RefreshOptions {
            signal: Some(signal),
            ..RefreshOptions::new()
        })
        .await;
    assert!(report.aborted);
    assert!(report.providers.is_empty(), "取消后不再刷新");
}

#[tokio::test]
async fn later_sources_override_earlier_ones_for_the_same_provider() {
    let mut models = Models::new();
    models.register_source("anthropic", source(vec![model("anthropic", "a", 1)]));
    models.register_source("anthropic", source(vec![model("anthropic", "a", 42)]));

    let report = models.refresh(RefreshOptions::new()).await;
    assert_eq!(
        provider_summary(&report),
        vec![("anthropic".to_string(), 1, 0, 0, false)]
    );
    assert_eq!(
        models.get_model("anthropic", "a").map(|m| m.max_tokens),
        Some(42)
    );
    let _ = json!({});
}
