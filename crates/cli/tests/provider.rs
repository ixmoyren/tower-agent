//! provider 组装（`cli::provider`）的测试：不触网、不依赖进程环境变量。
//!
//! 真实的凭据读取走 `build`（读环境变量）；这里用 `build_with_api_key` 显式注入 key，
//! 因此断言是确定性的。二进制层面的环境变量行为在 `tests/binary.rs` 里用子进程覆盖。

use cli::provider::{
    ProviderError, ProviderKind, ProviderStackOptions, build_with_api_key, model_for,
};

const OPTIONS: ProviderStackOptions = ProviderStackOptions {
    timeout: Some(std::time::Duration::from_secs(600)),
    max_retries: 2,
    max_retry_delay: std::time::Duration::from_secs(60),
};

#[test]
fn default_kind_is_the_offline_faux_provider() {
    let setup =
        build_with_api_key(ProviderKind::default(), None, OPTIONS, None).expect("faux 不需要凭据");
    assert_eq!(setup.model.id, "faux");
    assert!(setup.models.has_route("faux", "faux"));
    assert_eq!(setup.models.model_count(), 1);
    // 流服务可以取出来交给会话（能克隆即说明装箱成功）。
    let _stream = setup.stream_fn();
}

#[test]
fn unknown_provider_is_rejected_with_the_available_list() {
    assert_eq!(ProviderKind::parse("mistral"), None);
    let message = ProviderError::Unknown("mistral".into()).to_string();
    assert!(message.contains("未知 provider：mistral"), "got: {message}");
    for name in ["faux", "anthropic", "openai", "google"] {
        assert!(message.contains(name), "缺少 `{name}`：{message}");
    }
}

#[test]
fn missing_api_key_is_reported_per_provider() {
    let cases = [
        (ProviderKind::Anthropic, "anthropic", "ANTHROPIC_API_KEY"),
        (ProviderKind::OpenAi, "openai", "OPENAI_API_KEY"),
        (ProviderKind::OpenAiResponses, "openai", "OPENAI_API_KEY"),
        (ProviderKind::Google, "google", "GEMINI_API_KEY"),
    ];
    for (kind, provider, env) in cases {
        let result = build_with_api_key(kind, None, OPTIONS, None);
        match result {
            Ok(_) => panic!("{kind:?} 缺少 api key 时不应组装成功"),
            Err(error) => assert_eq!(
                error,
                ProviderError::MissingApiKey { provider, env },
                "{kind:?} 的错误信息不符"
            ),
        }
    }
}

#[test]
fn real_providers_register_the_route_and_use_the_requested_model() {
    let cases = [
        (ProviderKind::Anthropic, "anthropic", "anthropic-messages"),
        (ProviderKind::OpenAi, "openai", "openai-completions"),
        (ProviderKind::OpenAiResponses, "openai", "openai-responses"),
        (ProviderKind::Google, "google", "google-generative-ai"),
    ];
    for (kind, provider, api) in cases {
        let setup = build_with_api_key(kind, Some("my-model"), OPTIONS, Some("test-key"))
            .expect("带 api key 即可组装");
        assert!(setup.models.has_route(provider, api), "{kind:?} 缺少路由");
        assert_eq!(setup.model.id, "my-model");
        assert_eq!(setup.model.provider, provider);
        assert_eq!(setup.model.api, api);
        assert_eq!(
            setup
                .models
                .get_model(provider, "my-model")
                .map(|m| m.api.as_str()),
            Some(api),
            "{kind:?} 的模型目录没写到正确的 (provider, id)"
        );
    }
}

#[test]
fn default_model_falls_back_to_the_provider_default() {
    let setup = build_with_api_key(ProviderKind::Google, None, OPTIONS, Some("test-key"))
        .expect("带 api key 即可组装");
    assert_eq!(setup.model.id, ProviderKind::Google.default_model_id());
    assert!(
        setup
            .models
            .get_model("google", setup.model.id.as_str())
            .is_some()
    );
}

#[test]
fn model_metadata_is_conservative_for_real_providers() {
    let model = model_for(ProviderKind::Anthropic, "claude-x");
    assert_eq!(model.provider, "anthropic");
    assert!(model.reasoning);
    assert_eq!(model.context_window, 200_000);
    assert_eq!(model.max_tokens, 8_192);
    assert_eq!(model.input.len(), 2);

    let faux = model_for(ProviderKind::Faux, "ignored");
    assert_eq!(faux.id, "faux");
    assert_eq!(faux.api, "faux");
}
