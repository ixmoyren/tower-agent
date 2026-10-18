//! Provider plugin collection.
//!
//! This crate owns concrete vendor implementations and gives each provider an
//! independent plugin declaration. It also contains the configurable
//! OpenAI-compatible and JSONL sidecar plugins. Native dynamic provider loading
//! belongs to `tower-agent-plugin` because it adapts the generic plugin ABI.

mod command;
pub mod provider;

pub(crate) use tower_agent_provider::{
    AssistantEventStream, AssistantMessage, AssistantMessageEvent, BoxStreamService, ContentBlock,
    DoneReason, ErrorReason, ImageContent, InputModality, Message, Model, PiError, StopReason,
    StreamRequest, TextContent, ThinkingBudgets, ThinkingContent, ThinkingLevel, Tool, ToolCall,
    ToolResultMessage, Usage, UserContent, http, sse,
};
#[cfg(any(test, feature = "test-util"))]
pub(crate) use tower_agent_provider::{JsonObject, JsonValue};
#[cfg(test)]
pub(crate) use tower_agent_provider::{SimpleStreamOptions, TranscriptContext, UserMessage};

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use semver::Version;
use tower_agent_plugin::StaticPlugin;
use tower_agent_provider::{
    ANTHROPIC_MESSAGES_API, BoxProviderService, CredentialDescriptor, GOOGLE_GENERATIVE_AI_API,
    OPENAI_COMPLETIONS_API, OPENAI_RESPONSES_API, ProviderBuild, ProviderConfig,
    ProviderDescriptor, ProviderPlugin, ProviderPluginError, ProviderServiceExt, RetryTransport,
    TimeoutService, http::ReqwestTransport,
};

pub use provider::{
    anthropic::{AnthropicConfig, AnthropicProvider},
    google::{GoogleConfig, GoogleProvider},
    openai::{OpenAiConfig, OpenAiProvider},
    openai_responses::{OpenAiResponsesConfig, OpenAiResponsesProvider},
};
use tower_agent_plugin::ProviderPluginRegistration;

pub use command::CommandProviderPlugin;

const VERSION: Version = Version::new(1, 0, 0);
const DEFAULT_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_MAX_RETRIES: u32 = 2;
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuiltinKind {
    Anthropic,
    OpenAi,
    OpenAiResponses,
    Google,
    DeepSeek,
}

impl BuiltinKind {
    fn descriptor(self) -> ProviderDescriptor {
        let (id, name, api, default_model, env, priority) = match self {
            Self::Anthropic => (
                "anthropic",
                "Anthropic",
                ANTHROPIC_MESSAGES_API,
                "claude-opus-4-8",
                "ANTHROPIC_API_KEY",
                400,
            ),
            Self::OpenAi => (
                "openai",
                "OpenAI",
                OPENAI_COMPLETIONS_API,
                "gpt-5.5",
                "OPENAI_API_KEY",
                300,
            ),
            Self::OpenAiResponses => (
                "openai-responses",
                "OpenAI Responses",
                OPENAI_RESPONSES_API,
                "gpt-5.5",
                "OPENAI_API_KEY",
                50,
            ),
            Self::Google => (
                "google",
                "Google Generative AI",
                GOOGLE_GENERATIVE_AI_API,
                "gemini-3.1-pro-preview",
                "GEMINI_API_KEY",
                200,
            ),
            Self::DeepSeek => (
                "deepseek",
                "DeepSeek",
                OPENAI_COMPLETIONS_API,
                "deepseek-chat",
                "DEEPSEEK_API_KEY",
                100,
            ),
        };
        ProviderDescriptor::new(id, name, VERSION.clone())
            .api(api)
            .default_model(default_model)
            .credentials(CredentialDescriptor::ApiKey {
                env: Some(env.into()),
            })
            .priority(priority)
    }

    fn provider_id(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenAiResponses => "openai-responses",
            Self::Google => "google",
            Self::DeepSeek => "deepseek",
        }
    }

    fn api(self) -> &'static str {
        match self {
            Self::Anthropic => ANTHROPIC_MESSAGES_API,
            Self::OpenAi | Self::DeepSeek => OPENAI_COMPLETIONS_API,
            Self::OpenAiResponses => OPENAI_RESPONSES_API,
            Self::Google => GOOGLE_GENERATIVE_AI_API,
        }
    }

    fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-opus-4-8",
            Self::OpenAi | Self::OpenAiResponses => "gpt-5.5",
            Self::Google => "gemini-3.1-pro-preview",
            Self::DeepSeek => "deepseek-chat",
        }
    }
}

fn build_builtin(
    kind: BuiltinKind,
    config: &ProviderConfig,
) -> Result<ProviderBuild, ProviderPluginError> {
    let descriptor = kind.descriptor();
    let api_key =
        config
            .api_key
            .as_deref()
            .ok_or_else(|| ProviderPluginError::MissingCredentials {
                provider: descriptor.id.clone(),
                env: descriptor.credentials.env().map(str::to_owned),
            })?;
    let base_url = config
        .base_url
        .clone()
        .or_else(|| (kind == BuiltinKind::DeepSeek).then(|| DEEPSEEK_BASE_URL.to_owned()));
    let max_retries = config.max_retries.unwrap_or(DEFAULT_MAX_RETRIES);
    let max_retry_delay = Duration::from_millis(
        config
            .max_retry_delay_ms
            .unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS),
    );

    let service = match kind {
        BuiltinKind::Anthropic => {
            let mut provider = AnthropicProvider::new(RetryTransport::new(
                ReqwestTransport::new(),
                max_retries,
                max_retry_delay,
            ))
            .with_api_key(api_key);
            if let Some(base_url) = base_url {
                provider = provider.with_base_url(base_url);
            }
            provider.boxed_provider()
        }
        BuiltinKind::OpenAi => {
            let mut provider = OpenAiProvider::new(RetryTransport::new(
                ReqwestTransport::new(),
                max_retries,
                max_retry_delay,
            ))
            .with_api_key(api_key);
            if let Some(base_url) = base_url {
                provider = provider.with_base_url(base_url);
            }
            provider.boxed_provider()
        }
        BuiltinKind::OpenAiResponses => {
            let mut provider = OpenAiResponsesProvider::new(RetryTransport::new(
                ReqwestTransport::new(),
                max_retries,
                max_retry_delay,
            ))
            .with_api_key(api_key);
            if let Some(base_url) = base_url {
                provider = provider.with_base_url(base_url);
            }
            provider.boxed_provider()
        }
        BuiltinKind::Google => {
            let mut provider = GoogleProvider::new(RetryTransport::new(
                ReqwestTransport::new(),
                max_retries,
                max_retry_delay,
            ))
            .with_api_key(api_key);
            if let Some(base_url) = base_url {
                provider = provider.with_base_url(base_url);
            }
            provider.boxed_provider()
        }
        BuiltinKind::DeepSeek => {
            let mut provider = OpenAiProvider::new(RetryTransport::new(
                ReqwestTransport::new(),
                max_retries,
                max_retry_delay,
            ))
            .with_api_key(api_key);
            if let Some(base_url) = base_url {
                provider = provider.with_base_url(base_url);
            }
            provider.boxed_provider()
        }
    };

    let timeout = config.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let service: BoxProviderService =
        TimeoutService::new(service, Duration::from_millis(timeout)).boxed_provider();
    let model_id = config
        .model
        .as_deref()
        .unwrap_or_else(|| kind.default_model());
    let model = model_for(kind, model_id);
    let services = BTreeMap::from([(kind.api().to_owned(), service)]);
    Ok(ProviderBuild::new(services, vec![model]).with_default_model(kind.default_model()))
}

fn model_for(kind: BuiltinKind, id: &str) -> Model {
    Model {
        id: id.to_owned(),
        name: id.to_owned(),
        api: kind.api().to_owned(),
        provider: kind.provider_id().to_owned(),
        input: vec![InputModality::Text, InputModality::Image],
        reasoning: true,
        context_window: 200_000,
        max_tokens: 8_192,
        ..Model::default()
    }
}

/// Configurable OpenAI Chat Completions-compatible provider plugin.
///
/// This covers custom gateways and local OpenAI-compatible servers without a
/// separate Rust crate. Fully custom protocols implement [`ProviderPlugin`].
#[derive(Debug, Clone)]
pub struct OpenAiCompatibleProviderPlugin {
    descriptor: ProviderDescriptor,
    base_url: Option<String>,
}

impl OpenAiCompatibleProviderPlugin {
    /// Construct a provider plugin.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        default_model: impl Into<String>,
        api_key_env: Option<String>,
    ) -> Self {
        let credentials = api_key_env.map_or(CredentialDescriptor::None, |env| {
            CredentialDescriptor::ApiKey { env: Some(env) }
        });
        Self {
            descriptor: ProviderDescriptor::new(id, name, VERSION.clone())
                .api(OPENAI_COMPLETIONS_API)
                .default_model(default_model)
                .credentials(credentials),
            base_url: None,
        }
    }

    /// Set the default base URL used when host configuration does not override it.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Set the default-provider selection priority.
    #[must_use]
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.descriptor.priority = priority;
        self
    }
}

impl ProviderPlugin for OpenAiCompatibleProviderPlugin {
    fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor.clone()
    }

    fn build(&self, config: &ProviderConfig) -> Result<ProviderBuild, ProviderPluginError> {
        let base_url = config
            .base_url
            .clone()
            .or_else(|| self.base_url.clone())
            .ok_or_else(|| ProviderPluginError::InvalidConfig {
                provider: self.descriptor.id.clone(),
                message: "OpenAI-compatible providers require `base_url`".into(),
            })?;
        if self.descriptor.credentials.required() && config.api_key.is_none() {
            return Err(ProviderPluginError::MissingCredentials {
                provider: self.descriptor.id.clone(),
                env: self.descriptor.credentials.env().map(str::to_owned),
            });
        }
        let max_retries = config.max_retries.unwrap_or(DEFAULT_MAX_RETRIES);
        let max_retry_delay = Duration::from_millis(
            config
                .max_retry_delay_ms
                .unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS),
        );
        let mut provider = OpenAiProvider::new(RetryTransport::new(
            ReqwestTransport::new(),
            max_retries,
            max_retry_delay,
        ))
        .with_base_url(base_url);
        if let Some(api_key) = config.api_key.as_deref() {
            provider = provider.with_api_key(api_key);
        }
        let timeout = Duration::from_millis(config.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let service = TimeoutService::new(provider.boxed_provider(), timeout).boxed_provider();
        let default_model = config
            .model
            .clone()
            .or_else(|| self.descriptor.default_model.clone())
            .ok_or_else(|| ProviderPluginError::InvalidConfig {
                provider: self.descriptor.id.clone(),
                message: "OpenAI-compatible provider has no default model".into(),
            })?;
        let model = Model {
            id: default_model.clone(),
            name: default_model.clone(),
            api: OPENAI_COMPLETIONS_API.into(),
            provider: self.descriptor.id.clone(),
            input: vec![InputModality::Text, InputModality::Image],
            reasoning: true,
            context_window: 200_000,
            max_tokens: 8_192,
            ..Model::default()
        };
        Ok(ProviderBuild::new(
            BTreeMap::from([(OPENAI_COMPLETIONS_API.into(), service)]),
            vec![model],
        )
        .with_default_model(default_model))
    }
}

macro_rules! builtin_plugin {
    ($plugin:ident, $kind:expr, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, Default)]
        pub struct $plugin;

        impl ProviderPlugin for $plugin {
            fn descriptor(&self) -> ProviderDescriptor {
                $kind.descriptor()
            }

            fn build(&self, config: &ProviderConfig) -> Result<ProviderBuild, ProviderPluginError> {
                build_builtin($kind, config)
            }
        }
    };
}

builtin_plugin!(
    AnthropicProviderPlugin,
    BuiltinKind::Anthropic,
    "Anthropic Messages provider plugin."
);
builtin_plugin!(
    OpenAiProviderPlugin,
    BuiltinKind::OpenAi,
    "OpenAI Chat Completions provider plugin."
);
builtin_plugin!(
    OpenAiResponsesProviderPlugin,
    BuiltinKind::OpenAiResponses,
    "OpenAI Responses provider plugin."
);
builtin_plugin!(
    GoogleProviderPlugin,
    BuiltinKind::Google,
    "Google Generative AI provider plugin."
);
builtin_plugin!(
    DeepSeekProviderPlugin,
    BuiltinKind::DeepSeek,
    "DeepSeek OpenAI-compatible provider plugin."
);

/// Built-in provider plugin instances.
#[must_use]
pub fn provider_plugins() -> Vec<Arc<dyn ProviderPlugin>> {
    vec![
        Arc::new(AnthropicProviderPlugin),
        Arc::new(OpenAiProviderPlugin),
        Arc::new(OpenAiResponsesProviderPlugin),
        Arc::new(GoogleProviderPlugin),
        Arc::new(DeepSeekProviderPlugin),
    ]
}

/// Built-in provider static-plugin registrations.
#[must_use]
pub fn plugins() -> Vec<Arc<dyn StaticPlugin>> {
    provider_plugins()
        .into_iter()
        .map(|plugin| Arc::new(ProviderPluginRegistration::new(plugin)) as Arc<dyn StaticPlugin>)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_provider_has_a_static_plugin_descriptor() {
        let plugins = plugins();
        assert_eq!(plugins.len(), 5);
        let names: Vec<String> = plugins
            .iter()
            .map(|plugin| plugin.manifest().name)
            .collect();
        assert_eq!(
            names,
            vec![
                "provider.anthropic",
                "provider.openai",
                "provider.openai-responses",
                "provider.google",
                "provider.deepseek",
            ]
        );
    }

    #[test]
    fn builtin_provider_builds_without_network_access() {
        let plugin = OpenAiProviderPlugin;
        let mut config = ProviderConfig::new("openai");
        config.api_key = Some("test-key".into());
        let build = plugin.build(&config).expect("build provider service");
        assert_eq!(build.models[0].id, "gpt-5.5");
        assert!(build.services.contains_key(OPENAI_COMPLETIONS_API));
    }

    #[test]
    fn missing_credentials_are_reported() {
        let Err(error) = AnthropicProviderPlugin.build(&ProviderConfig::new("anthropic")) else {
            panic!("missing credentials must fail");
        };
        assert!(matches!(
            error,
            ProviderPluginError::MissingCredentials { provider, env }
                if provider == "anthropic"
                    && env.as_deref() == Some("ANTHROPIC_API_KEY")
        ));
    }

    #[test]
    fn openai_compatible_plugin_uses_host_base_url() {
        let plugin = OpenAiCompatibleProviderPlugin::new("local", "Local", "local-model", None)
            .with_base_url("http://127.0.0.1:8080/v1");
        let config = ProviderConfig::new("local");
        let build = plugin.build(&config).expect("build compatible provider");
        assert_eq!(build.models[0].provider, "local");
        assert_eq!(build.models[0].api, OPENAI_COMPLETIONS_API);
    }
}
