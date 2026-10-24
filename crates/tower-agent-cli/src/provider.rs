//! Provider plugin selection and session setup for the CLI host.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures_util::future::BoxFuture;
use serde::Deserialize;
use tower::Service;
use tower_agent_provider::{
    AssistantEventStream, BoxProviderService, BoxStreamService, Model, Models, PiError,
    ProviderConfig, ProviderPlugin, ProviderPluginError, ProviderRegistry, ProviderServiceExt,
    StreamRequest,
};

const PROVIDERS_PATH_ENV: &str = "TOWER_AGENT_PROVIDERS";

#[derive(Debug, Default, Deserialize)]
struct ProviderConfigFile {
    #[serde(default)]
    providers: BTreeMap<String, UserProviderConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct UserProviderConfig {
    kind: String,
    name: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    default_model: Option<String>,
    api_key_env: Option<String>,
    priority: Option<i32>,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    working_directory: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    library: Option<PathBuf>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

struct ConfiguredProvider {
    plugin: Arc<dyn ProviderPlugin>,
    kind: tower_agent::plugin::PluginKind,
}

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
pub fn builtin_provider_registry(cwd: impl AsRef<Path>) -> ProviderRegistry {
    tower_agent::assembly::HostPlugins::new(cwd.as_ref().to_path_buf())
        .providers()
        .expect("built-in provider plugins must assemble")
}

/// Provider registry including providers declared in user/project config.
pub fn provider_registry(cwd: impl AsRef<Path>) -> Result<ProviderRegistry, String> {
    let cwd = cwd.as_ref();
    let configs = load_provider_configs(cwd)?;
    registry_with_configs(cwd, configs)
}

fn registry_with_configs(
    cwd: &Path,
    configs: Vec<(String, UserProviderConfig)>,
) -> Result<ProviderRegistry, String> {
    let mut host = tower_agent::assembly::HostPlugins::new(cwd.to_path_buf());
    for (id, config) in configs {
        let provider = configured_provider_plugin(&id, config)?;
        host = host.with_provider_kind(provider.plugin, provider.kind);
    }
    host.providers().map_err(|error| error.to_string())
}

#[allow(unsafe_code)] // Dynamic provider libraries execute in-process.
fn configured_provider_plugin(
    id: &str,
    config: UserProviderConfig,
) -> Result<ConfiguredProvider, String> {
    match config.kind.as_str() {
        "openai-compatible" => {
            let base_url = config.base_url.ok_or_else(|| {
                format!("provider `{id}` requires `base_url` for openai-compatible")
            })?;
            let default_model = config.default_model.ok_or_else(|| {
                format!("provider `{id}` requires `default_model` for openai-compatible")
            })?;
            let mut plugin = tower_agent::providers::OpenAiCompatibleProviderPlugin::new(
                id,
                config.name.unwrap_or_else(|| id.to_owned()),
                default_model,
                config.api_key_env,
            )
            .with_base_url(base_url);
            if let Some(priority) = config.priority {
                plugin = plugin.with_priority(priority);
            }
            Ok(ConfiguredProvider {
                plugin: Arc::new(plugin),
                kind: tower_agent::plugin::PluginKind::Static,
            })
        }
        "sidecar" => {
            if config.command.is_empty() {
                return Err(format!("provider `{id}` requires a non-empty `command`"));
            }
            let default_model = config
                .default_model
                .ok_or_else(|| format!("provider `{id}` requires `default_model` for sidecar"))?;
            let mut plugin = tower_agent::providers::CommandProviderPlugin::new(
                id,
                config.name.unwrap_or_else(|| id.to_owned()),
                default_model,
                config.command,
            );
            if let Some(env) = config.api_key_env {
                plugin = plugin.with_api_key_env(env);
            }
            if let Some(priority) = config.priority {
                plugin = plugin.with_priority(priority);
            }
            if let Some(cwd) = config.working_directory {
                plugin = plugin.with_working_directory(cwd);
            }
            for (name, value) in config.env {
                plugin = plugin.with_env(name, value);
            }
            Ok(ConfiguredProvider {
                plugin: Arc::new(plugin),
                kind: tower_agent::plugin::PluginKind::Static,
            })
        }
        "dynamic" => {
            let library = config
                .library
                .ok_or_else(|| format!("provider `{id}` requires `library` for dynamic"))?;
            let mut plugin = unsafe {
                tower_agent_plugin_sdk::provider::dynamic::DynamicProviderPlugin::load(library)
            }
            .map_err(|error| error.to_string())?;
            if let Some(model) = config.default_model {
                plugin = plugin.with_default_model(model);
            }
            if let Some(env) = config.api_key_env {
                plugin = plugin.with_api_key_env(env);
            }
            if let Some(priority) = config.priority {
                plugin = plugin.with_priority(priority);
            }
            if let Some(metadata) = config.metadata {
                plugin = plugin.with_metadata(metadata);
            }
            Ok(ConfiguredProvider {
                plugin: Arc::new(plugin),
                kind: tower_agent::plugin::PluginKind::Dynamic,
            })
        }
        other => Err(format!(
            "provider `{id}` uses unsupported kind `{other}` (available: openai-compatible, sidecar, dynamic)"
        )),
    }
}

fn load_provider_configs(cwd: &Path) -> Result<Vec<(String, UserProviderConfig)>, String> {
    load_provider_configs_from_paths(provider_config_paths(cwd))
}

fn load_provider_configs_from_paths(
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<Vec<(String, UserProviderConfig)>, String> {
    let mut merged = BTreeMap::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        let source = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read provider config {}: {error}", path.display()))?;
        let config: ProviderConfigFile = serde_json::from_str(&source)
            .map_err(|error| format!("invalid provider config {}: {error}", path.display()))?;
        merged.extend(config.providers);
    }
    Ok(merged.into_iter().collect())
}

fn provider_config_paths(cwd: &Path) -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os(PROVIDERS_PATH_ENV) {
        return vec![PathBuf::from(path)];
    }
    let mut paths = Vec::new();
    if let Some(config_dir) = dirs::config_dir() {
        paths.push(config_dir.join("tower-agent/providers.json"));
    }
    paths.push(cwd.join(".tower-agent/providers.json"));
    paths
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
        let registry = builtin_provider_registry(".");
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
        let registry = builtin_provider_registry(".");
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
        let registry = builtin_provider_registry(".");
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
        let registry = builtin_provider_registry(".");
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

    #[test]
    fn project_config_registers_an_openai_compatible_provider() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_dir = dir.path().join(".tower-agent");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        let path = config_dir.join("providers.json");
        std::fs::write(
            &path,
            r#"{
                "providers": {
                    "local": {
                        "kind": "openai-compatible",
                        "name": "Local",
                        "base_url": "http://127.0.0.1:8080/v1",
                        "default_model": "local-model",
                        "priority": 900
                    }
                }
            }"#,
        )
        .expect("write provider config");

        let configs = load_provider_configs_from_paths([path]).expect("load provider config");
        let registry = registry_with_configs(dir.path(), configs).expect("build provider registry");
        let descriptor = registry.descriptor("local").expect("local provider");
        assert_eq!(descriptor.default_model.as_deref(), Some("local-model"));
        assert_eq!(descriptor.priority, 900);
        let setup = build_from_registry(
            &registry,
            "local",
            None,
            ProviderStackOptions::default(),
            None,
        )
        .expect("build local provider");
        assert_eq!(setup.model.provider, "local");
    }

    #[test]
    fn project_config_registers_a_sidecar_provider() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_dir = dir.path().join(".tower-agent");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        let path = config_dir.join("providers.json");
        std::fs::write(
            &path,
            r#"{
                "providers": {
                    "sidecar": {
                        "kind": "sidecar",
                        "name": "Sidecar",
                        "default_model": "sidecar-model",
                        "command": ["my-provider", "--stdio"]
                    }
                }
            }"#,
        )
        .expect("write provider config");

        let configs = load_provider_configs_from_paths([path]).expect("load provider config");
        let registry = registry_with_configs(dir.path(), configs).expect("build provider registry");
        let descriptor = registry.descriptor("sidecar").expect("sidecar provider");
        assert_eq!(descriptor.default_model.as_deref(), Some("sidecar-model"));
        assert_eq!(descriptor.default_api.as_deref(), Some("command-sidecar"));
    }
}
