//! Frontend-neutral controller for agent-backed interactive sessions.

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU64, Ordering},
};

use semver::Version;
use tower_agent_core::{AgentEvent, AgentMessage};
use tower_agent_plugins::{
    CapabilityId, EventHandler, PluginContext, PluginError, PluginKind, PluginManifest,
    StaticPlugin, event_handler,
};

use crate::{
    command::{self, CommandHost},
    model::UiModel,
};

/// A frontend action produced from submitted editor text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    /// Send ordinary input to the agent.
    Submit(String),
    /// The UI backend consumed the input (for example, a slash command).
    Handled,
    /// The host should close the interactive session.
    Quit,
}

/// Result of executing a slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Close the frontend.
    Quit,
    /// Clear the transcript, then append the supplied notice.
    Clear(String),
    /// Append the supplied notice.
    Notice(String),
}

/// Shared, frontend-neutral agent UI state and session controls.
///
/// GUI and server hosts can apply events directly, call [`Self::handle_input`],
/// and serialize [`Self::snapshot`]. Terminal hosts add rendering on top.
#[derive(Clone)]
pub struct AgentUi {
    model: Arc<RwLock<UiModel>>,
    commands: Option<Arc<dyn CommandHost>>,
    version: Arc<AtomicU64>,
}

impl AgentUi {
    /// Construct a controller without session command capabilities.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            model: Arc::new(RwLock::new(UiModel::new(title))),
            commands: None,
            version: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Construct a controller backed by a host implementing [`CommandHost`].
    #[must_use]
    pub fn with_commands(title: impl Into<String>, commands: Arc<dyn CommandHost>) -> Self {
        Self {
            model: Arc::new(RwLock::new(UiModel::new(title))),
            commands: Some(commands),
            version: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Return the latest model snapshot.
    #[must_use]
    pub fn snapshot(&self) -> UiModel {
        self.model.read().expect("agent ui model poisoned").clone()
    }

    /// Return the monotonically increasing state version.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Fold one agent event into the view model.
    pub fn apply_event(&self, event: &AgentEvent) {
        self.update(|model| model.apply(event));
    }

    /// Prefill the transcript with an existing agent message.
    pub fn push_message(&self, message: &AgentMessage) {
        self.update(|model| model.push_message(message));
    }

    /// Consume a session subscription until it closes, folding every event.
    ///
    /// A GUI or axum host can spawn this alongside its request handling and
    /// query [`Self::snapshot`] whenever the version changes.
    pub async fn drain_events(&self, mut subscription: crate::Subscription) {
        while let Some(event) = subscription.recv().await {
            self.apply_event(&event);
        }
    }

    /// Append a local notice visible to every frontend.
    pub fn push_notice(&self, text: impl Into<String>) {
        self.update(|model| model.push_notice(text));
    }

    /// Clear the rendered transcript.
    pub fn clear(&self) {
        self.update(UiModel::clear);
    }

    /// Scroll the view upward by `lines`.
    pub fn scroll_up(&self, lines: u16) {
        self.update(|model| model.scroll_up(lines));
    }

    /// Scroll the view downward by `lines`.
    pub fn scroll_down(&self, lines: u16) {
        self.update(|model| model.scroll_down(lines));
    }

    /// Resume following the newest transcript content.
    pub fn scroll_to_bottom(&self) {
        self.update(UiModel::scroll_to_bottom);
    }

    /// Handle submitted text independently of terminal input handling.
    ///
    /// Slash commands mutate the shared model and return [`UiAction::Handled`].
    /// Ordinary text is returned as [`UiAction::Submit`] for the host to send to
    /// its agent service.
    pub fn handle_input(&self, input: &str) -> UiAction {
        let Some(command_input) = input.strip_prefix('/') else {
            return UiAction::Submit(input.to_owned());
        };
        match execute_command(self.commands.as_deref(), command_input) {
            CommandOutcome::Quit => UiAction::Quit,
            CommandOutcome::Clear(notice) => {
                self.update(|model| {
                    model.clear();
                    model.push_notice(notice);
                });
                UiAction::Handled
            }
            CommandOutcome::Notice(notice) => {
                self.push_notice(notice);
                UiAction::Handled
            }
        }
    }

    fn update(&self, edit: impl FnOnce(&mut UiModel)) {
        {
            let mut model = self.model.write().expect("agent ui model poisoned");
            edit(&mut model);
        }
        self.version.fetch_add(1, Ordering::SeqCst);
    }
}

impl From<UiModel> for AgentUi {
    fn from(model: UiModel) -> Self {
        Self {
            model: Arc::new(RwLock::new(model)),
            commands: None,
            version: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl StaticPlugin for AgentUi {
    fn manifest(&self) -> PluginManifest {
        let version =
            Version::parse(env!("CARGO_PKG_VERSION")).expect("workspace package version is semver");
        PluginManifest::new("tower-agent-ui", version.clone(), PluginKind::Static)
            .providing(CapabilityId::new("ui", "session"), version)
    }

    fn install(&self, _context: &mut PluginContext) -> Result<(), PluginError> {
        Ok(())
    }

    fn event_handlers(&self) -> Vec<Box<dyn EventHandler>> {
        let ui = self.clone();
        vec![event_handler(move |event| ui.apply_event(event))]
    }
}

/// Execute a slash command against an optional host.
///
/// Commands that only alter frontend state (`quit`, `hotkeys`, and unknown
/// command diagnostics) work without a host. Session commands return a notice
/// when no host is attached.
pub fn execute_command(host: Option<&dyn CommandHost>, input: &str) -> CommandOutcome {
    let mut parts = input.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let arg = parts.next().map(str::trim).filter(|arg| !arg.is_empty());

    let Some(command) = command::lookup(name) else {
        return CommandOutcome::Notice(format!(
            "unknown command: /{name} (type `/` for available commands)"
        ));
    };

    if command.name == "quit" {
        return CommandOutcome::Quit;
    }
    if command.name == "hotkeys" {
        return CommandOutcome::Notice(command::hotkeys().join("\n"));
    }
    let Some(host) = host else {
        return CommandOutcome::Notice(format!("/{name} requires a session command host"));
    };

    match command.name {
        "new" => match host.new_session() {
            Ok(()) => CommandOutcome::Clear("Started a new session.".into()),
            Err(error) => CommandOutcome::Notice(format!("/new failed: {error}")),
        },
        "session" => CommandOutcome::Notice(host.session_info().join("\n")),
        "model" => notice(host.model(arg)),
        "thinking" => notice(host.thinking(arg)),
        "login" => notice(host.login(arg)),
        "compact" => notice(host.compact(arg)),
        _ => CommandOutcome::Notice(format!(
            "/{name} is not ported to tower-agent yet (pi description: {}).",
            command.description
        )),
    }
}

fn notice(result: Result<String, String>) -> CommandOutcome {
    match result {
        Ok(text) => CommandOutcome::Notice(text),
        Err(error) => CommandOutcome::Notice(format!("error: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use tower_agent_provider::Message;
    use tower_agent_runtime::AgentMessage;

    use super::*;

    struct TestHost;

    impl CommandHost for TestHost {
        fn new_session(&self) -> Result<(), String> {
            Ok(())
        }

        fn session_info(&self) -> Vec<String> {
            vec!["session: test".into()]
        }

        fn model(&self, arg: Option<&str>) -> Result<String, String> {
            Ok(arg.unwrap_or("test-model").to_owned())
        }

        fn thinking(&self, arg: Option<&str>) -> Result<String, String> {
            Ok(arg.unwrap_or("medium").to_owned())
        }

        fn login(&self, _arg: Option<&str>) -> Result<String, String> {
            Ok("logged in".into())
        }

        fn compact(&self, _arg: Option<&str>) -> Result<String, String> {
            Ok("compaction requested".into())
        }
    }

    #[test]
    fn ordinary_input_is_returned_for_agent_submission() {
        let ui = AgentUi::new("test");
        assert_eq!(ui.handle_input("hello"), UiAction::Submit("hello".into()));
        assert_eq!(ui.snapshot().entries().len(), 0);
    }

    #[test]
    fn slash_commands_mutate_the_shared_model() {
        let ui = AgentUi::with_commands("test", Arc::new(TestHost));
        ui.push_notice("old");
        assert_eq!(ui.handle_input("/new"), UiAction::Handled);
        let snapshot = ui.snapshot();
        assert_eq!(snapshot.entries().len(), 1);
        assert_eq!(snapshot.entries()[0].text, "Started a new session.");

        assert_eq!(ui.handle_input("/session"), UiAction::Handled);
        assert_eq!(
            ui.snapshot().entries().last().unwrap().text,
            "session: test"
        );
        assert_eq!(ui.handle_input("/quit"), UiAction::Quit);
    }

    #[test]
    fn events_update_the_observable_snapshot_and_version() {
        let ui = AgentUi::new("test");
        let start = ui.version();
        ui.apply_event(&AgentEvent::AgentStart);
        assert!(ui.snapshot().is_running());
        assert!(ui.version() > start);
        ui.apply_event(&AgentEvent::MessageEnd {
            message: AgentMessage::Message(Message::User(tower_agent_provider::UserMessage {
                content: tower_agent_provider::UserContent::Text("hi".into()),
                timestamp: 0,
            })),
        });
        assert_eq!(ui.snapshot().entries()[0].text, "hi");
    }

    #[test]
    fn agent_ui_is_a_static_plugin_with_an_event_reducer() {
        use tower_agent_plugins::StaticPlugin as _;

        let ui = AgentUi::new("test");
        let manifest = ui.manifest();
        assert_eq!(manifest.name, "tower-agent-ui");
        assert!(
            manifest
                .provides_capability(&CapabilityId::new("ui", "session"))
                .is_some()
        );
        let handlers = ui.event_handlers();
        assert_eq!(handlers.len(), 1);
        handlers[0].handle(&AgentEvent::AgentStart);
        assert!(ui.snapshot().is_running());
    }

    #[test]
    fn unported_and_unknown_commands_become_notices() {
        let ui = AgentUi::with_commands("test", Arc::new(TestHost));
        assert_eq!(ui.handle_input("/export"), UiAction::Handled);
        assert!(
            ui.snapshot()
                .entries()
                .last()
                .unwrap()
                .text
                .contains("not ported")
        );
        assert_eq!(ui.handle_input("/nope"), UiAction::Handled);
        assert!(
            ui.snapshot()
                .entries()
                .last()
                .unwrap()
                .text
                .contains("unknown command")
        );
    }

    #[test]
    fn snapshot_is_serializable_for_server_frontends() {
        let ui = AgentUi::new("test");
        ui.push_notice("ready");
        let value = serde_json::to_value(ui.snapshot()).expect("ui snapshot is JSON");
        assert_eq!(value["title"], "test");
        assert_eq!(value["entries"][0]["text"], "ready");
    }
}
