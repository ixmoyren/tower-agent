//! Provider plugin contract and registry.
//!
//! A provider plugin converts host configuration into one or more provider
//! services plus the model definitions those services support. The runtime
//! never needs to know whether the plugin wraps an HTTP API, a local process,
//! or an in-process mock.

use std::collections::BTreeMap;
use std::sync::Arc;

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{BoxProviderService, JsonValue, Model, Models, StreamServiceExt};

/// How a provider expects the host to locate credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialDescriptor {
    /// The provider does not require credentials.
    None,
    /// The host should resolve an API key, preferably from the named environment variable.
    ApiKey {
        /// Conventional environment variable name.
        env: Option<String>,
    },
}

impl CredentialDescriptor {
    /// Conventional environment variable, when one is known.
    #[must_use]
    pub fn env(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::ApiKey { env } => env.as_deref(),
        }
    }

    /// Whether the provider requires a credential.
    #[must_use]
    pub const fn required(&self) -> bool {
        matches!(self, Self::ApiKey { .. })
    }
}

/// Stable description of a user-selectable provider plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    /// Stable provider identifier used by `--provider`, models, and config.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Plugin/provider semantic version.
    pub version: Version,
    /// API identifiers supported by this provider.
    pub apis: Vec<String>,
    /// API used when configuration does not select one.
    pub default_api: Option<String>,
    /// Model used when configuration does not select one.
    pub default_model: Option<String>,
    /// Credential requirements.
    pub credentials: CredentialDescriptor,
    /// Default-provider ordering hint; larger values are preferred.
    pub priority: i32,
}

impl ProviderDescriptor {
    /// Construct a descriptor with no APIs or models.
    #[must_use]
    pub fn new(id: impl Into<String>, name: impl Into<String>, version: Version) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            version,
            apis: Vec::new(),
            default_api: None,
            default_model: None,
            credentials: CredentialDescriptor::None,
            priority: 0,
        }
    }

    /// Add a supported API.
    #[must_use]
    pub fn api(mut self, api: impl Into<String>) -> Self {
        let api = api.into();
        if self.default_api.is_none() {
            self.default_api = Some(api.clone());
        }
        self.apis.push(api);
        self
    }

    /// Set the default API.
    #[must_use]
    pub fn default_api(mut self, api: impl Into<String>) -> Self {
        self.default_api = Some(api.into());
        self
    }

    /// Set the default model.
    #[must_use]
    pub fn default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Set credential requirements.
    #[must_use]
    pub fn credentials(mut self, credentials: CredentialDescriptor) -> Self {
        self.credentials = credentials;
        self
    }

    /// Set the default-provider priority.
    #[must_use]
    pub fn priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }
}

/// Host configuration passed to a provider plugin.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Selected provider id.
    pub provider: String,
    /// Selected API; `None` uses the descriptor default.
    pub api: Option<String>,
    /// Selected model; `None` uses the build/descriptor default.
    pub model: Option<String>,
    /// Optional API base URL.
    pub base_url: Option<String>,
    /// Resolved secret. The host is responsible for precedence and storage.
    pub api_key: Option<String>,
    /// Additional request headers.
    pub headers: BTreeMap<String, String>,
    /// Request-establishment timeout.
    pub timeout_ms: Option<u64>,
    /// Maximum provider retries.
    pub max_retries: Option<u32>,
    /// Maximum accepted retry delay.
    pub max_retry_delay_ms: Option<u64>,
    /// Provider-specific configuration.
    pub metadata: JsonValue,
}

impl ProviderConfig {
    /// Construct configuration for a provider.
    #[must_use]
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            ..Self::default()
        }
    }
}

/// Result of building a provider plugin.
pub struct ProviderBuild {
    /// Provider services keyed by API id.
    pub services: BTreeMap<String, BoxProviderService>,
    /// Models supported by this provider build.
    pub models: Vec<Model>,
    /// Default model for this build.
    pub default_model: Option<String>,
}

impl ProviderBuild {
    /// Construct a provider build.
    #[must_use]
    pub fn new(services: BTreeMap<String, BoxProviderService>, models: Vec<Model>) -> Self {
        Self {
            services,
            models,
            default_model: None,
        }
    }

    /// Set the default model.
    #[must_use]
    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }
}

/// Errors produced while registering or building provider plugins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderPluginError {
    /// The requested provider id is unknown.
    UnknownProvider(String),
    /// The plugin rejected its host configuration.
    InvalidConfig {
        /// Provider id.
        provider: String,
        /// Validation failure.
        message: String,
    },
    /// Required credentials were not supplied.
    MissingCredentials {
        /// Provider id.
        provider: String,
        /// Conventional environment variable.
        env: Option<String>,
    },
    /// The provider failed to build its services.
    Build {
        /// Provider id.
        provider: String,
        /// Failure detail.
        message: String,
    },
    /// The requested model is not provided.
    UnknownModel {
        /// Provider id.
        provider: String,
        /// Requested model.
        model: String,
    },
    /// The selected API has no built service.
    MissingApi {
        /// Provider id.
        provider: String,
        /// API id.
        api: String,
    },
}

impl std::fmt::Display for ProviderPluginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownProvider(provider) => {
                write!(formatter, "unknown provider plugin `{provider}`")
            }
            Self::InvalidConfig { provider, message } => {
                write!(
                    formatter,
                    "invalid provider configuration for `{provider}`: {message}"
                )
            }
            Self::MissingCredentials {
                provider,
                env: Some(env),
            } => write!(
                formatter,
                "missing credentials for provider `{provider}`; set `{env}`"
            ),
            Self::MissingCredentials {
                provider,
                env: None,
            } => {
                write!(formatter, "missing credentials for provider `{provider}`")
            }
            Self::Build { provider, message } => {
                write!(formatter, "provider plugin `{provider}` failed: {message}")
            }
            Self::UnknownModel { provider, model } => {
                write!(
                    formatter,
                    "provider `{provider}` does not expose model `{model}`"
                )
            }
            Self::MissingApi { provider, api } => {
                write!(formatter, "provider `{provider}` did not build api `{api}`")
            }
        }
    }
}

impl std::error::Error for ProviderPluginError {}

/// A provider extension.
pub trait ProviderPlugin: Send + Sync + 'static {
    /// Stable metadata exposed to hosts and configuration UIs.
    fn descriptor(&self) -> ProviderDescriptor;

    /// Build provider services and model definitions for one session.
    fn build(&self, config: &ProviderConfig) -> Result<ProviderBuild, ProviderPluginError>;
}

/// A provider build ready to be handed to a tower-agent session.
pub struct ProviderSession {
    /// Routes and model catalog for this build.
    pub models: Models,
    /// Selected model.
    pub model: Model,
    stream_fn: crate::BoxStreamService,
}

impl ProviderSession {
    /// Clone the stream service for [`tower_agent_runtime::AgentOptions`].
    #[must_use]
    pub fn stream_fn(&self) -> crate::BoxStreamService {
        self.stream_fn.clone()
    }
}

/// Registry of provider plugins.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    plugins: BTreeMap<String, Arc<dyn ProviderPlugin>>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderRegistry")
            .field("providers", &self.provider_ids())
            .finish()
    }
}

impl ProviderRegistry {
    /// Construct an empty provider registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider plugin.
    pub fn register(&mut self, plugin: Arc<dyn ProviderPlugin>) -> Result<(), ProviderPluginError> {
        let descriptor = plugin.descriptor();
        if descriptor.id.trim().is_empty() {
            return Err(ProviderPluginError::InvalidConfig {
                provider: String::new(),
                message: "provider id must not be empty".into(),
            });
        }
        self.plugins.insert(descriptor.id, plugin);
        Ok(())
    }

    /// Provider ids in lexical order.
    #[must_use]
    pub fn provider_ids(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }

    /// Descriptors in default-provider priority order.
    #[must_use]
    pub fn descriptors(&self) -> Vec<ProviderDescriptor> {
        let mut descriptors: Vec<_> = self
            .plugins
            .values()
            .map(|plugin| plugin.descriptor())
            .collect();
        descriptors.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.id.cmp(&right.id))
        });
        descriptors
    }

    /// Look up a provider descriptor.
    #[must_use]
    pub fn descriptor(&self, provider: &str) -> Option<ProviderDescriptor> {
        self.plugins.get(provider).map(|plugin| plugin.descriptor())
    }

    /// Build one provider session.
    pub fn build_session(
        &self,
        provider: &str,
        model: Option<&str>,
        mut config: ProviderConfig,
    ) -> Result<ProviderSession, ProviderPluginError> {
        let plugin = self
            .plugins
            .get(provider)
            .ok_or_else(|| ProviderPluginError::UnknownProvider(provider.to_owned()))?;
        let descriptor = plugin.descriptor();
        config.provider = descriptor.id.clone();
        config.model = model.map(str::to_owned).or(config.model);
        if config.api.is_none() {
            config.api.clone_from(&descriptor.default_api);
        }
        let api = config
            .api
            .clone()
            .or_else(|| descriptor.apis.first().cloned())
            .ok_or_else(|| ProviderPluginError::InvalidConfig {
                provider: descriptor.id.clone(),
                message: "provider declares no APIs".into(),
            })?;
        let build = plugin.build(&config)?;

        let mut models = Models::new();
        models.register_models(&descriptor.id, build.models.clone());
        for (service_api, service) in build.services {
            models.register_boxed(&descriptor.id, &service_api, service);
        }

        let model_id = config
            .model
            .clone()
            .or(build.default_model)
            .or(descriptor.default_model)
            .ok_or_else(|| ProviderPluginError::InvalidConfig {
                provider: descriptor.id.clone(),
                message: "provider declares no default model".into(),
            })?;
        let model = models
            .get_model(&descriptor.id, &model_id)
            .cloned()
            .ok_or_else(|| ProviderPluginError::UnknownModel {
                provider: descriptor.id.clone(),
                model: model_id,
            })?;
        if models.resolve(&descriptor.id, &api).is_none() {
            return Err(ProviderPluginError::MissingApi {
                provider: descriptor.id,
                api,
            });
        }

        let stream_fn = models.clone().boxed_stream();
        Ok(ProviderSession {
            models,
            model,
            stream_fn,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Poll};

    use futures_util::{future::BoxFuture, stream};
    use tower::Service;

    use super::*;
    use crate::{AssistantEventStream, PiError, ProviderServiceExt, StreamRequest};

    #[derive(Clone)]
    struct TestStreamService;

    impl Service<StreamRequest> for TestStreamService {
        type Response = AssistantEventStream;
        type Error = PiError;
        type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: StreamRequest) -> Self::Future {
            Box::pin(async { Ok(AssistantEventStream::new(stream::iter(Vec::new()))) })
        }
    }

    struct TestProvider;

    impl ProviderPlugin for TestProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor::new("test", "Test", Version::new(1, 0, 0))
                .api("test-api")
                .default_model("test-model")
                .credentials(CredentialDescriptor::ApiKey {
                    env: Some("TEST_API_KEY".into()),
                })
        }

        fn build(&self, config: &ProviderConfig) -> Result<ProviderBuild, ProviderPluginError> {
            if config.api_key.as_deref() != Some("secret") {
                return Err(ProviderPluginError::MissingCredentials {
                    provider: config.provider.clone(),
                    env: Some("TEST_API_KEY".into()),
                });
            }
            let model = Model {
                id: "test-model".into(),
                name: "test-model".into(),
                provider: "test".into(),
                api: "test-api".into(),
                ..Model::default()
            };
            let mut services = BTreeMap::new();
            services.insert("test-api".into(), TestStreamService.boxed_provider());
            Ok(ProviderBuild::new(services, vec![model]))
        }
    }

    #[test]
    fn registry_builds_a_session_and_routes_the_model() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(TestProvider)).expect("register");
        let mut config = ProviderConfig::new("test");
        config.api_key = Some("secret".into());
        let session = registry
            .build_session("test", None, config)
            .expect("build session");
        assert_eq!(session.model.id, "test-model");
        assert!(session.models.supports(&session.model));
        let _ = session.stream_fn();
    }

    #[test]
    fn unknown_provider_and_model_are_reported() {
        let registry = ProviderRegistry::new();
        assert!(matches!(
            registry.build_session("missing", None, ProviderConfig::new("missing")),
            Err(ProviderPluginError::UnknownProvider(provider)) if provider == "missing"
        ));
    }
}
