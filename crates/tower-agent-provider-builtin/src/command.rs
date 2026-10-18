//! Language-agnostic sidecar provider plugin.
//!
//! Each provider request starts the configured command, writes one JSON request
//! line to stdin, and consumes JSON event lines from stdout.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::Stdio,
    task::{Context, Poll},
    time::Duration,
};

use futures_util::{future::BoxFuture, stream};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdout, Command},
    time::{Instant, sleep_until},
};
use tower::Service;
use tower_agent_plugin::provider::wire::{SidecarAssembler, SidecarEvent, SidecarRequest};
use tower_agent_provider::{
    AssistantEventStream, CredentialDescriptor, Model, PiError, ProviderBuild, ProviderConfig,
    ProviderDescriptor, ProviderPlugin, ProviderPluginError, ProviderServiceExt, StreamRequest,
};

const SIDECAR_API: &str = "command-sidecar";

/// Provider plugin that delegates requests to a subprocess.
#[derive(Debug, Clone)]
pub struct CommandProviderPlugin {
    descriptor: ProviderDescriptor,
    command: Vec<String>,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
}

impl CommandProviderPlugin {
    /// Construct a sidecar provider plugin.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        default_model: impl Into<String>,
        command: Vec<String>,
    ) -> Self {
        Self {
            descriptor: ProviderDescriptor::new(id, name, semver::Version::new(1, 0, 0))
                .api(SIDECAR_API)
                .default_model(default_model),
            command,
            cwd: None,
            env: BTreeMap::new(),
        }
    }

    /// Declare the environment variable used for API-key resolution.
    #[must_use]
    pub fn with_api_key_env(mut self, env: impl Into<String>) -> Self {
        self.descriptor.credentials = CredentialDescriptor::ApiKey {
            env: Some(env.into()),
        };
        self
    }

    /// Set the default-provider selection priority.
    #[must_use]
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.descriptor.priority = priority;
        self
    }

    /// Set the subprocess working directory.
    #[must_use]
    pub fn with_working_directory(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Add an environment variable for the subprocess.
    #[must_use]
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }
}

impl ProviderPlugin for CommandProviderPlugin {
    fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor.clone()
    }

    fn build(&self, config: &ProviderConfig) -> Result<ProviderBuild, ProviderPluginError> {
        if self.command.is_empty() {
            return Err(ProviderPluginError::InvalidConfig {
                provider: self.descriptor.id.clone(),
                message: "sidecar provider command must not be empty".into(),
            });
        }
        let model_id = config
            .model
            .clone()
            .or_else(|| self.descriptor.default_model.clone())
            .ok_or_else(|| ProviderPluginError::InvalidConfig {
                provider: self.descriptor.id.clone(),
                message: "sidecar provider declares no default model".into(),
            })?;
        let model = Model {
            id: model_id.clone(),
            name: model_id.clone(),
            api: SIDECAR_API.into(),
            provider: self.descriptor.id.clone(),
            input: vec![tower_agent_provider::InputModality::Text],
            context_window: 200_000,
            max_tokens: 8_192,
            ..Model::default()
        };
        let service = CommandProviderService {
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            api_key: config.api_key.clone(),
            provider: self.descriptor.id.clone(),
            timeout: Some(Duration::from_millis(config.timeout_ms.unwrap_or(600_000))),
        };
        Ok(ProviderBuild::new(
            BTreeMap::from([(SIDECAR_API.into(), service.boxed_provider())]),
            vec![model],
        )
        .with_default_model(model_id))
    }
}

#[derive(Clone)]
struct CommandProviderService {
    command: Vec<String>,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, String>,
    api_key: Option<String>,
    provider: String,
    timeout: Option<Duration>,
}

impl Service<StreamRequest> for CommandProviderService {
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

impl CommandProviderService {
    async fn start(&self, request: StreamRequest) -> Result<AssistantEventStream, PiError> {
        let (program, args) = self
            .command
            .split_first()
            .ok_or_else(|| PiError::Unsupported("sidecar command is empty".into()))?;
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env("TOWER_AGENT_PROVIDER_ID", &self.provider)
            .env("TOWER_AGENT_PROVIDER_MODEL", &request.model.id);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        command.envs(&self.env);
        if let Some(api_key) = &self.api_key {
            command.env("TOWER_AGENT_PROVIDER_API_KEY", api_key);
        }
        let mut child = command.spawn().map_err(|error| {
            PiError::Transport(format!("cannot start provider sidecar: {error}"))
        })?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| PiError::Transport("provider sidecar stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PiError::Transport("provider sidecar stdout unavailable".into()))?;
        let wire = SidecarRequest::from_request(&request);
        let mut payload =
            serde_json::to_vec(&wire).map_err(|error| PiError::Serialization(error.to_string()))?;
        payload.push(b'\n');
        stdin.write_all(&payload).await.map_err(|error| {
            PiError::Transport(format!("provider sidecar write failed: {error}"))
        })?;
        stdin.shutdown().await.map_err(|error| {
            PiError::Transport(format!("provider sidecar stdin close failed: {error}"))
        })?;
        drop(stdin);

        let state = SidecarState {
            child,
            lines: BufReader::new(stdout).lines(),
            assembler: SidecarAssembler::new(&request.model),
            deadline: self.timeout.map(|timeout| Instant::now() + timeout),
        };
        let events = stream::unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.assembler.pop() {
                    return Some((event, state));
                }
                if state.assembler.is_terminal() {
                    return None;
                }
                let next_line = match state.deadline {
                    Some(deadline) => {
                        tokio::select! {
                            line = state.lines.next_line() => line,
                            () = sleep_until(deadline) => {
                                state.assembler.fail("provider sidecar timed out".into());
                                continue;
                            }
                        }
                    }
                    None => state.lines.next_line().await,
                };
                match next_line {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<SidecarEvent>(&line) {
                            Ok(event) => state.assembler.apply(event),
                            Err(error) => state.assembler.fail(format!(
                                "invalid provider sidecar event: {error}; line: {line}"
                            )),
                        }
                    }
                    Ok(None) => {
                        state
                            .assembler
                            .fail("provider sidecar closed without a terminal event".into());
                    }
                    Err(error) => {
                        state
                            .assembler
                            .fail(format!("provider sidecar read failed: {error}"));
                    }
                }
            }
        });
        Ok(AssistantEventStream::new(events))
    }
}

struct SidecarState {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
    assembler: SidecarAssembler,
    deadline: Option<Instant>,
}

impl Drop for SidecarState {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt as _;
    use tower::ServiceExt as _;
    use tower_agent_provider::{AssistantMessageEvent, ContentBlock, Message, TextContent};

    use super::*;

    #[tokio::test]
    async fn command_provider_reads_jsonl_events() {
        let plugin = CommandProviderPlugin::new(
            "sidecar",
            "Sidecar",
            "sidecar-model",
            vec![
                "sh".into(),
                "-c".into(),
                "printf '%s\\n' '{\"type\":\"text_delta\",\"text\":\"hello\"}' \
                 '{\"type\":\"done\",\"text\":\"hello\"}'"
                    .into(),
            ],
        );
        let build = plugin
            .build(&ProviderConfig::new("sidecar"))
            .expect("build sidecar provider");
        let service = build.services[SIDECAR_API].clone();
        let request = StreamRequest {
            model: build.models[0].clone(),
            context: tower_agent_provider::TranscriptContext::new(vec![Message::User(
                tower_agent_provider::UserMessage {
                    content: tower_agent_provider::UserContent::Text("hi".into()),
                    timestamp: 0,
                },
            )]),
            options: Default::default(),
        };
        let mut events = service.oneshot(request).await.expect("start sidecar");
        let mut text = String::new();
        let mut done = None;
        while let Some(event) = events.next().await {
            match event {
                AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(&delta),
                event @ AssistantMessageEvent::Done { .. } => {
                    done = event.terminal_message().cloned();
                }
                _ => {}
            }
        }
        assert_eq!(text, "hello");
        assert_eq!(
            done.and_then(|message| message.content.first().cloned()),
            Some(ContentBlock::Text(TextContent {
                text: "hello".into(),
                text_signature: None,
            }))
        );
    }

    #[tokio::test]
    async fn command_provider_times_out_a_hung_sidecar() {
        let plugin = CommandProviderPlugin::new(
            "slow",
            "Slow",
            "slow-model",
            vec!["sh".into(), "-c".into(), "sleep 1".into()],
        );
        let mut config = ProviderConfig::new("slow");
        config.timeout_ms = Some(20);
        let build = plugin.build(&config).expect("build sidecar provider");
        let service = build.services[SIDECAR_API].clone();
        let request = StreamRequest {
            model: build.models[0].clone(),
            context: tower_agent_provider::TranscriptContext::new(Vec::new()),
            options: Default::default(),
        };
        let mut events = service.oneshot(request).await.expect("start sidecar");
        let mut error = None;
        while let Some(event) = events.next().await {
            if let AssistantMessageEvent::Error { error: message, .. } = event {
                error = message.error_message;
            }
        }
        assert_eq!(error.as_deref(), Some("provider sidecar timed out"));
    }
}
