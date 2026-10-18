//! Provider plugin selection and session setup for the CLI host.

use std::path::Path;
use std::time::Duration;

use futures_util::future::BoxFuture;
use tower::Service;
use tower_agent_provider::{
    AssistantEventStream, BoxProviderService, BoxStreamService, Model, Models, PiError,
    ProviderConfig, ProviderPluginError, ProviderRegistry, ProviderServiceExt, StreamRequest,
};

/// Provider stack defaults from the pi reference implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderStackOptions {
    /// Request-establishment timeout; `None` disables the timeout layer.
    pub timeout: Option<Duration>,
    /// Maximum retry count.
    pub max_retries: u32,
    /// Maximum accepted server retry delay.
    pub max_retry_delay: Duration,
}

impl Default for ProviderStackOptions {
    fn default() -> Self {
        Self {
            timeout: Some(Duration::from_secs(600)),
            max_retries: 2,
            max_retry_delay: Duration::from_secs(60),
        }
    }
}

/// One provider build ready for a CLI session.
pub struct ProviderSetup {
    /// Routes and model catalog for this build.
    pub models: Models,
    /// Selected model.
    pub model: Model,
    /// Selected provider service.
    pub provider: BoxStreamService,
}

impl ProviderSetup {
    /// Clone the stream service for the session.
    #[must_use]
    pub fn stream_fn(&self) -> BoxStreamService {
        self.provider.clone()
    }
}

/// Built-in provider registry assembled through static provider plugins.
#[must_use]
pub fn provider_registry(cwd: impl AsRef<Path>) -> ProviderRegistry {
    tower_agent::assembly::HostPlugins::new(cwd.as_ref().to_path_buf())
        .providers()
        .expect("built-in provider plugins must assemble")
}

/// Resolve credentials using the descriptor supplied by a provider plugin.
#[must_use]
pub fn resolve_api_key_for_provider(
    registry: &ProviderRegistry,
    provider: &str,
    explicit: Option<&str>,
    stored: Option<&str>,
) -> Option<String> {
    let descriptor = registry.descriptor(provider)?;
    let env = descriptor.credentials.env();
    let clean = |value: &str| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    };
    explicit
        .and_then(clean)
        .or_else(|| {
            env.and_then(|name| std::env::var(name).ok())
                .and_then(|value| clean(&value))
        })
        .or_else(|| stored.and_then(clean))
}

/// Build a provider session through the plugin registry.
pub fn build_from_registry(
    registry: &ProviderRegistry,
    provider: &str,
    model: Option<&str>,
    options: ProviderStackOptions,
    api_key: Option<&str>,
) -> Result<ProviderSetup, ProviderPluginError> {
    let mut config = ProviderConfig::new(provider);
    config.model = model.map(str::to_owned);
    config.api_key = api_key.map(str::to_owned);
    config.timeout_ms = options.timeout.map(|duration| duration.as_millis() as u64);
    config.max_retries = Some(options.max_retries);
    config.max_retry_delay_ms = Some(options.max_retry_delay.as_millis() as u64);
    let session = registry.build_session(provider, model, config)?;
    let stream_fn = session.stream_fn();
    Ok(ProviderSetup {
        models: session.models,
        model: session.model,
        provider: stream_fn,
    })
}

/// Provider ids with a credential visible in the environment, in descriptor priority order.
#[must_use]
pub fn providers_with_env_credentials(registry: &ProviderRegistry) -> Vec<String> {
    registry
        .descriptors()
        .into_iter()
        .filter(|descriptor| descriptor.credentials.env().is_some_and(env_has_api_key))
        .map(|descriptor| descriptor.id)
        .collect()
}

fn env_has_api_key(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
}

/// Placeholder provider used while waiting for `/login`.
#[derive(Clone, Copy, Default)]
struct UnconfiguredProvider;

impl Service<StreamRequest> for UnconfiguredProvider {
    type Response = AssistantEventStream;
    type Error = PiError;
    type Future = BoxFuture<'static, Result<AssistantEventStream, PiError>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _request: StreamRequest) -> Self::Future {
        Box::pin(async {
            Err(PiError::Unsupported(
                "no provider configured; use /login <provider> <api-key> to configure one".into(),
            ))
        })
    }
}

/// Provider setup used when the TUI starts before `/login`.
#[must_use]
pub fn unconfigured_setup() -> ProviderSetup {
    let model = Model {
        id: "unconfigured".into(),
        name: "unconfigured".into(),
        api: "unconfigured".into(),
        provider: "unconfigured".into(),
        ..Model::default()
    };
    let mut models = Models::new();
    models.register_models("unconfigured", vec![model.clone()]);
    let provider: BoxProviderService = UnconfiguredProvider.boxed_provider();
    models.register_boxed("unconfigured", "unconfigured", provider.clone());
    ProviderSetup {
        models,
        model,
        provider,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_lists_plugin_descriptors() {
        let registry = provider_registry(".");
        let ids: Vec<&str> = registry.provider_ids();
        assert_eq!(
            ids,
            vec![
                "anthropic",
                "deepseek",
                "google",
                "openai",
                "openai-responses",
            ]
        );
        let anthropic = registry.descriptor("anthropic").expect("descriptor");
        assert_eq!(anthropic.default_model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(anthropic.credentials.env(), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn registry_builds_a_builtin_provider_without_network_access() {
        let registry = provider_registry(".");
        let setup = build_from_registry(
            &registry,
            "anthropic",
            Some("claude-test"),
            ProviderStackOptions::default(),
            Some("test-key"),
        )
        .expect("build provider");
        assert_eq!(setup.model.id, "claude-test");
        assert_eq!(setup.model.provider, "anthropic");
        assert!(setup.models.supports(&setup.model));
    }

    #[test]
    fn missing_credentials_include_the_descriptor_environment() {
        let registry = provider_registry(".");
        let Err(error) = build_from_registry(
            &registry,
            "anthropic",
            None,
            ProviderStackOptions::default(),
            None,
        ) else {
            panic!("missing key must fail");
        };
        assert!(error.to_string().contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn unknown_provider_is_reported() {
        let registry = provider_registry(".");
        let Err(error) = build_from_registry(
            &registry,
            "missing",
            None,
            ProviderStackOptions::default(),
            Some("test"),
        ) else {
            panic!("unknown provider must fail");
        };
        assert!(matches!(
            error,
            ProviderPluginError::UnknownProvider(provider) if provider == "missing"
        ));
    }

    #[test]
    fn unconfigured_provider_exposes_a_placeholder_model() {
        let setup = unconfigured_setup();
        assert_eq!(setup.model.provider, "unconfigured");
        assert!(setup.models.supports(&setup.model));
    }
}
