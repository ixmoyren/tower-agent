//! Native dynamic provider loading through the generic plugin ABI.
//!
//! Provider libraries are normal `tower-agent-plugin` dynamic plugins. Their
//! manifest must provide `provider.<id>`, and their call entry receives an
//! `AbiToolCall` whose arguments contain `provider_config` and `request`.
//! JSONL provider events are returned in `AbiToolResult.content`.

#![allow(unsafe_code)]

use std::{
    path::Path,
    sync::Arc,
    task::{Context, Poll},
};

use crate::{
    CapabilityId, PluginError,
    dynamic::{AbiToolCall, AbiValue, ArchivedAbiToolResult, LoadedPlugin},
};
use futures_util::{future::BoxFuture, stream};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};
use tower::Service;
use tower_agent_provider::{
    AssistantEventStream, InputModality, Model, PiError, ProviderBuild, ProviderConfig,
    ProviderDescriptor, ProviderPlugin, ProviderPluginError, ProviderServiceExt, StreamRequest,
};

use crate::provider::wire::{SidecarAssembler, SidecarEvent, SidecarRequest};

const DYNAMIC_API: &str = "dynamic-provider";

/// Provider plugin backed by a generic tower-agent dynamic plugin.
#[derive(Clone)]
pub struct DynamicProviderPlugin {
    loaded: Arc<LoadedPlugin>,
    descriptor: ProviderDescriptor,
    metadata: Value,
}

impl std::fmt::Debug for DynamicProviderPlugin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DynamicProviderPlugin")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

impl DynamicProviderPlugin {
    /// Load a provider implemented as a generic tower-agent dynamic plugin.
    ///
    /// # Safety
    ///
    /// The library is executed in-process and has the same privileges as the
    /// host. Only load trusted plugin binaries built with the matching SDK ABI.
    pub unsafe fn load(path: impl AsRef<Path>) -> Result<Self, ProviderPluginError> {
        let loaded =
            unsafe { LoadedPlugin::load(path.as_ref(), |_| None) }.map_err(map_plugin_error)?;
        let manifest = loaded.manifest();
        let provider_id = manifest
            .name
            .strip_prefix("provider.")
            .unwrap_or(&manifest.name)
            .to_owned();
        let capability = CapabilityId::new("provider", provider_id.clone());
        if manifest.provides_capability(&capability).is_none() {
            return Err(ProviderPluginError::InvalidConfig {
                provider: provider_id,
                message: format!(
                    "dynamic plugin `{}` must provide `{capability}`",
                    manifest.name
                ),
            });
        }
        let descriptor =
            ProviderDescriptor::new(provider_id, manifest.name.clone(), manifest.version.clone())
                .api(DYNAMIC_API);
        Ok(Self {
            descriptor,
            loaded: Arc::new(loaded),
            metadata: Value::Null,
        })
    }

    /// Override the plugin priority.
    #[must_use]
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.descriptor.priority = priority;
        self
    }

    /// Override the provider default model.
    #[must_use]
    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.descriptor.default_model = Some(model.into());
        self
    }

    /// Override the credential environment variable.
    #[must_use]
    pub fn with_api_key_env(mut self, env: impl Into<String>) -> Self {
        self.descriptor.credentials = tower_agent_provider::CredentialDescriptor::ApiKey {
            env: Some(env.into()),
        };
        self
    }

    /// Attach provider-specific metadata forwarded to the plugin.
    #[must_use]
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }
}

impl ProviderPlugin for DynamicProviderPlugin {
    fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor.clone()
    }

    fn build(&self, config: &ProviderConfig) -> Result<ProviderBuild, ProviderPluginError> {
        let model_id = config
            .model
            .clone()
            .or_else(|| self.descriptor.default_model.clone())
            .ok_or_else(|| ProviderPluginError::InvalidConfig {
                provider: self.descriptor.id.clone(),
                message: "dynamic provider declares no default model".into(),
            })?;
        let model = Model {
            id: model_id.clone(),
            name: model_id.clone(),
            api: DYNAMIC_API.into(),
            provider: self.descriptor.id.clone(),
            input: vec![InputModality::Text],
            context_window: 200_000,
            max_tokens: 8_192,
            ..Model::default()
        };
        let mut config = config.clone();
        if config.metadata.is_null() {
            config.metadata = self.metadata.clone();
        }
        let service = DynamicProviderService {
            loaded: Arc::clone(&self.loaded),
            config,
        };
        Ok(ProviderBuild::new(
            std::collections::BTreeMap::from([(DYNAMIC_API.into(), service.boxed_provider())]),
            vec![model],
        )
        .with_default_model(model_id))
    }
}

fn map_plugin_error(error: PluginError) -> ProviderPluginError {
    ProviderPluginError::Build {
        provider: "dynamic".into(),
        message: error.to_string(),
    }
}

#[derive(Clone)]
struct DynamicProviderService {
    loaded: Arc<LoadedPlugin>,
    config: ProviderConfig,
}

impl Service<StreamRequest> for DynamicProviderService {
    type Response = AssistantEventStream;
    type Error = PiError;
    type Future = BoxFuture<'static, Result<AssistantEventStream, PiError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: StreamRequest) -> Self::Future {
        let service = self.clone();
        Box::pin(async move { service.start(request).await })
    }
}

impl DynamicProviderService {
    async fn start(&self, request: StreamRequest) -> Result<AssistantEventStream, PiError> {
        let event_model = request.model.clone();
        let config = serde_json::to_value(&self.config)
            .map_err(|error| PiError::Serialization(error.to_string()))?;
        let request = serde_json::to_value(SidecarRequest::from_request(&request))
            .map_err(|error| PiError::Serialization(error.to_string()))?;
        let call = AbiToolCall {
            id: format!("provider-call-{}", tower_agent_provider::provider::now_ms()),
            tool: "provider.call".into(),
            args: AbiValue::from_json(&json!({
                "provider_config": config,
                "request": request,
            })),
        };
        let payload = rkyv::to_bytes::<rkyv::rancor::Error>(&call)
            .map_err(|error| PiError::Serialization(error.to_string()))?
            .into_vec();
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let loaded = Arc::clone(&self.loaded);
        let task = tokio::task::spawn_blocking(move || {
            let response = loaded.call_archived(&payload);
            match response {
                Ok(response) => {
                    let result =
                        rkyv::access::<ArchivedAbiToolResult, rkyv::rancor::Error>(&response);
                    match result {
                        Ok(result) if result.ok => {
                            for line in result.content.as_str().lines() {
                                let _ = tx.send(line.to_owned());
                            }
                        }
                        Ok(result) => {
                            let _ = tx.send(
                                json!({
                                    "type": "error",
                                    "message": result.content.as_str(),
                                })
                                .to_string(),
                            );
                        }
                        Err(error) => {
                            let _ = tx.send(
                                json!({
                                    "type": "error",
                                    "message": format!("invalid dynamic provider result: {error}"),
                                })
                                .to_string(),
                            );
                        }
                    }
                }
                Err(error) => {
                    let _ = tx.send(
                        json!({
                            "type": "error",
                            "message": error.to_string(),
                        })
                        .to_string(),
                    );
                }
            }
        });
        let state = DynamicState {
            assembler: SidecarAssembler::new(&event_model),
            rx,
            task,
        };
        let events = stream::unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.assembler.pop() {
                    return Some((event, state));
                }
                if state.assembler.is_terminal() {
                    return None;
                }
                match state.rx.recv().await {
                    Some(line) => match serde_json::from_str::<SidecarEvent>(&line) {
                        Ok(event) => state.assembler.apply(event),
                        Err(error) => state.assembler.fail(format!(
                            "invalid dynamic provider event: {error}; line: {line}"
                        )),
                    },
                    None => state
                        .assembler
                        .fail("dynamic provider closed without a terminal event".into()),
                }
            }
        });
        Ok(AssistantEventStream::new(events))
    }
}

struct DynamicState {
    assembler: SidecarAssembler,
    rx: mpsc::UnboundedReceiver<String>,
    task: JoinHandle<()>,
}

impl Drop for DynamicState {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use futures_util::StreamExt as _;
    use tower::ServiceExt as _;

    use super::*;

    fn example_path() -> Option<PathBuf> {
        std::env::var_os("TPA_PROVIDER_DYNAMIC_EXAMPLE").map(PathBuf::from)
    }

    #[test]
    fn missing_library_is_reported() {
        let error = unsafe { DynamicProviderPlugin::load("/definitely/not/a/provider.so") }
            .expect_err("missing library must fail");
        assert!(matches!(error, ProviderPluginError::Build { .. }));
    }

    #[test]
    fn example_library_has_a_descriptor() {
        let Some(path) = example_path() else {
            eprintln!("skipping: set TPA_PROVIDER_DYNAMIC_EXAMPLE to the example cdylib path");
            return;
        };
        let plugin = unsafe { DynamicProviderPlugin::load(path) }
            .expect("load dynamic provider")
            .with_default_model("dynamic-model");
        assert_eq!(plugin.descriptor().id, "dynamic");
    }

    #[tokio::test]
    async fn example_library_streams_a_response() {
        let Some(path) = example_path() else {
            eprintln!("skipping: set TPA_PROVIDER_DYNAMIC_EXAMPLE to the example cdylib path");
            return;
        };
        let plugin = unsafe { DynamicProviderPlugin::load(path) }
            .expect("load dynamic provider")
            .with_default_model("dynamic-model");
        let build = plugin
            .build(&ProviderConfig::new("dynamic"))
            .expect("build dynamic provider");
        let service = build.services[DYNAMIC_API].clone();
        let request = StreamRequest {
            model: build.models[0].clone(),
            context: tower_agent_provider::TranscriptContext::new(Vec::new()),
            options: Default::default(),
        };
        let mut stream = service
            .oneshot(request)
            .await
            .expect("start dynamic provider");
        let mut text = None;
        while let Some(event) = stream.next().await {
            if let Some(message) = event.terminal_message() {
                text = message.content.first().and_then(|content| match content {
                    tower_agent_provider::ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                });
            }
        }
        assert_eq!(text.as_deref(), Some("dynamic reply"));
    }
}
