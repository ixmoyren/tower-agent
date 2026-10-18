//! Engine-independent contracts for codemode.
//!
//! Both the monty/Python and JavaScript engines expose the same capability
//! surface to scripts. This crate contains that shared vocabulary without
//! depending on either engine or on wasmtime.

use std::{fmt, str::FromStr};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod engine;
mod hook;
mod prompt;

pub use engine::{EngineSelection, ScriptMetadata};
pub use hook::{
    ScriptHookError, ScriptHookEvent, ScriptHookHandler, ScriptHookRegistry, ScriptHookResult,
};
pub use prompt::{PromptBuilder, PromptDocument, PromptLimits, PromptMount, PromptMountMode};

/// A boxed capability operation.
pub type CapabilityFuture<'a, T> = BoxFuture<'a, Result<T, CapabilityError>>;

/// The language used to execute a codemode script.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptLanguage {
    /// The monty Python-subset engine.
    Monty,
    /// The JavaScript engine.
    #[default]
    #[serde(rename = "javascript", alias = "js")]
    JavaScript,
}

impl ScriptLanguage {
    /// Return the stable wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Monty => "monty",
            Self::JavaScript => "javascript",
        }
    }

    /// Parse a language name accepted by configuration and script metadata.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "monty" => Some(Self::Monty),
            "javascript" | "js" => Some(Self::JavaScript),
            _ => None,
        }
    }
}

impl fmt::Display for ScriptLanguage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ScriptLanguage {
    type Err = ParseScriptLanguageError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::parse(name).ok_or_else(|| ParseScriptLanguageError(name.to_owned()))
    }
}

/// An unknown script language name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown script language `{0}`")]
pub struct ParseScriptLanguageError(String);

/// A capability that a script may use.
///
/// Each capability maps to exactly one permission switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Run coreutils-wasm commands.
    Exec,
    /// Access the mounted script filesystem.
    Fs,
    /// Call or list host tools.
    Tools,
    /// Register or unregister host tools.
    RegisterTools,
    /// Register tool lifecycle hooks.
    Hooks,
    /// Register agent commands.
    Commands,
    /// Contribute UI state.
    Ui,
    /// Read or write private script state and emit logs.
    State,
}

impl Capability {
    /// Every capability in stable order.
    pub const ALL: [Self; 8] = [
        Self::Exec,
        Self::Fs,
        Self::Tools,
        Self::RegisterTools,
        Self::Hooks,
        Self::Commands,
        Self::Ui,
        Self::State,
    ];

    /// Return the stable wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Fs => "fs",
            Self::Tools => "tools",
            Self::RegisterTools => "register_tools",
            Self::Hooks => "hooks",
            Self::Commands => "commands",
            Self::Ui => "ui",
            Self::State => "state",
        }
    }

    /// Resolve a capability from its wire name.
    pub fn from_name(name: &str) -> Result<Self, CapabilityError> {
        match name {
            "exec" => Ok(Self::Exec),
            "fs" => Ok(Self::Fs),
            "tools" => Ok(Self::Tools),
            "register_tools" => Ok(Self::RegisterTools),
            "hooks" => Ok(Self::Hooks),
            "commands" => Ok(Self::Commands),
            "ui" => Ok(Self::Ui),
            "state" => Ok(Self::State),
            _ => Err(CapabilityError::UnknownCapability {
                name: name.to_owned(),
            }),
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Capability {
    type Err = CapabilityError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::from_name(name)
    }
}

/// Permission switches for one codemode script.
///
/// A script is allowed to use a capability only when its corresponding switch
/// is enabled. The actual filesystem write scope is still governed by mounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ScriptPermissions {
    /// Allow running coreutils-wasm commands.
    pub exec: bool,
    /// Allow filesystem operations through the mounted capability surface.
    pub fs: bool,
    /// Allow `tool_call` and `tool_list`.
    pub tools: bool,
    /// Allow `tool_register` and `tool_unregister`.
    pub register_tools: bool,
    /// Allow registering tool lifecycle hooks.
    pub hooks: bool,
    /// Allow registering agent commands.
    pub commands: bool,
    /// Allow UI contributions.
    pub ui: bool,
    /// Allow private state access and logging.
    pub state: bool,
}

impl ScriptPermissions {
    /// Permissions with every capability enabled, matching the legacy monty
    /// defaults.
    #[must_use]
    pub const fn allow_all() -> Self {
        Self {
            exec: true,
            fs: true,
            tools: true,
            register_tools: true,
            hooks: true,
            commands: true,
            ui: true,
            state: true,
        }
    }

    /// Permissions with every capability denied.
    #[must_use]
    pub const fn deny_all() -> Self {
        Self {
            exec: false,
            fs: false,
            tools: false,
            register_tools: false,
            hooks: false,
            commands: false,
            ui: false,
            state: false,
        }
    }

    /// Return whether `capability` is enabled.
    #[must_use]
    pub const fn allows(self, capability: Capability) -> bool {
        match capability {
            Capability::Exec => self.exec,
            Capability::Fs => self.fs,
            Capability::Tools => self.tools,
            Capability::RegisterTools => self.register_tools,
            Capability::Hooks => self.hooks,
            Capability::Commands => self.commands,
            Capability::Ui => self.ui,
            Capability::State => self.state,
        }
    }

    /// Set one permission switch and return whether it was changed.
    pub fn set(&mut self, capability: Capability, enabled: bool) -> bool {
        let slot = match capability {
            Capability::Exec => &mut self.exec,
            Capability::Fs => &mut self.fs,
            Capability::Tools => &mut self.tools,
            Capability::RegisterTools => &mut self.register_tools,
            Capability::Hooks => &mut self.hooks,
            Capability::Commands => &mut self.commands,
            Capability::Ui => &mut self.ui,
            Capability::State => &mut self.state,
        };
        let changed = *slot != enabled;
        *slot = enabled;
        changed
    }

    /// Check authorization and return the script-visible error when denied.
    pub fn authorize(self, capability: Capability) -> Result<(), CapabilityError> {
        if self.allows(capability) {
            Ok(())
        } else {
            Err(CapabilityError::Unauthorized { capability })
        }
    }

    /// Return every enabled capability in stable order.
    #[must_use]
    pub fn enabled(self) -> Vec<Capability> {
        Capability::ALL
            .into_iter()
            .filter(|capability| self.allows(*capability))
            .collect()
    }
}

impl Default for ScriptPermissions {
    fn default() -> Self {
        Self::allow_all()
    }
}

/// A structured capability failure that can be serialized back to a script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapabilityError {
    /// The script does not have the required permission.
    #[error("capability `{capability}` is not permitted")]
    Unauthorized {
        /// The denied capability.
        capability: Capability,
    },
    /// The requested capability name is not part of the shared surface.
    #[error("unknown capability `{name}`")]
    UnknownCapability {
        /// The unrecognized name supplied by the script.
        name: String,
    },
    /// The engine or host failed while executing a capability.
    #[error("engine error in `{capability}`: {message}")]
    Engine {
        /// The capability being executed.
        capability: Capability,
        /// Diagnostic suitable for returning to the script.
        message: String,
    },
    /// The capability exceeded its deadline.
    #[error("capability `{capability}` timed out after {timeout_ms} ms")]
    Timeout {
        /// The capability being executed.
        capability: Capability,
        /// The deadline that elapsed.
        timeout_ms: u64,
    },
    /// The capability was cancelled.
    #[error("capability `{capability}` was cancelled")]
    Cancelled {
        /// The cancelled capability.
        capability: Capability,
    },
    /// The request did not match the capability's contract.
    #[error("invalid request for `{capability}`: {message}")]
    InvalidRequest {
        /// The capability being invoked.
        capability: Capability,
        /// Why the request was rejected.
        message: String,
    },
}

/// Options for an `exec` capability call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecOptions {
    /// Standard input passed to the command.
    pub stdin: Option<String>,
    /// Optional per-command timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Working directory inside the script filesystem.
    pub cwd: Option<String>,
}

/// Result of a coreutils-wasm command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResult {
    /// Process exit code.
    pub code: i32,
    /// UTF-8 decoded standard output.
    pub stdout: String,
    /// UTF-8 decoded standard error.
    pub stderr: String,
}

/// The kind of an entry returned by `fs_read_dir`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsEntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link.
    Symlink,
    /// A platform-specific entry kind.
    Other,
}

/// One entry returned by `fs_read_dir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsEntry {
    /// Entry name relative to the listed directory.
    pub name: String,
    /// Entry kind.
    pub kind: FsEntryKind,
}

/// A tool visible to a codemode script.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    /// Tool name used by `tool_call`.
    pub name: String,
    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for tool arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    /// JSON Schema for the tool result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

/// A tool registration requested by a script.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolRegistration {
    /// Public tool metadata.
    pub tool: ToolDescriptor,
    /// Engine-specific identifier of the script callback implementing the tool.
    pub handler: String,
}

/// A status message contributed by a script.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiPanel {
    /// Stable panel identifier.
    pub id: String,
    /// Panel title.
    pub title: String,
    /// Renderer-neutral panel content.
    pub content: Value,
}

/// Log severity used by the shared `log` capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Diagnostic detail.
    Debug,
    /// Normal information.
    Info,
    /// Recoverable problem.
    Warn,
    /// Operation failure.
    Error,
}

/// The host capabilities exposed to both script engines.
///
/// Implementations are object-safe so an engine can hold
/// `Arc<dyn CapabilitySurface>`. Implementations must authorize each operation
/// with [`CapabilitySurface::authorize`] before performing side effects.
pub trait CapabilitySurface: Send + Sync + 'static {
    /// Return the permissions attached to this script.
    fn permissions(&self) -> ScriptPermissions;

    /// Return the enabled capabilities in stable order.
    fn capabilities(&self) -> Vec<Capability> {
        self.permissions().enabled()
    }

    /// Authorize one capability against this surface's permission set.
    fn authorize(&self, capability: Capability) -> Result<(), CapabilityError> {
        self.permissions().authorize(capability)
    }

    /// Run a coreutils-wasm command.
    fn exec(
        &self,
        name: &str,
        args: Vec<String>,
        options: ExecOptions,
    ) -> CapabilityFuture<'_, ExecResult>;

    /// Read a file as bytes.
    fn fs_read(&self, path: &str) -> CapabilityFuture<'_, Vec<u8>>;

    /// Write bytes to a file.
    fn fs_write(&self, path: &str, contents: Vec<u8>) -> CapabilityFuture<'_, ()>;

    /// List a directory.
    fn fs_read_dir(&self, path: &str) -> CapabilityFuture<'_, Vec<FsEntry>>;

    /// Check whether a path exists.
    fn fs_exists(&self, path: &str) -> CapabilityFuture<'_, bool>;

    /// Create a directory.
    fn fs_mkdir(&self, path: &str, recursive: bool) -> CapabilityFuture<'_, ()>;

    /// Remove a file or directory.
    fn fs_remove(&self, path: &str, recursive: bool) -> CapabilityFuture<'_, ()>;

    /// Invoke a host tool and return its JSON result.
    fn tool_call(&self, name: &str, args: Value) -> CapabilityFuture<'_, Value>;

    /// List host tools visible to the script.
    fn tool_list(&self) -> CapabilityFuture<'_, Vec<ToolDescriptor>>;

    /// Register a script-provided host tool.
    fn tool_register(&self, registration: ToolRegistration) -> CapabilityFuture<'_, ()>;

    /// Unregister a script-provided host tool.
    fn tool_unregister(&self, name: &str) -> CapabilityFuture<'_, ()>;

    /// Read a private script state value.
    fn state_get(&self, key: &str) -> CapabilityFuture<'_, Option<Value>>;

    /// Write a private script state value.
    fn state_set(&self, key: &str, value: Value) -> CapabilityFuture<'_, ()>;

    /// Emit a script log record.
    fn log(&self, level: LogLevel, message: &str) -> CapabilityFuture<'_, ()>;

    /// Update a short status line.
    fn ui_status(&self, message: &str) -> CapabilityFuture<'_, ()>;

    /// Set or replace a UI panel.
    fn ui_panel(&self, panel: UiPanel) -> CapabilityFuture<'_, ()>;

    /// Clear script-contributed UI state.
    fn ui_clear(&self) -> CapabilityFuture<'_, ()>;

    /// Install a renderer-neutral custom tool renderer.
    fn ui_tool_render(&self, tool: &str, renderer: Value) -> CapabilityFuture<'_, ()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_matrix_gates_each_capability_independently() {
        for granted in Capability::ALL {
            let mut permissions = ScriptPermissions::deny_all();
            assert!(permissions.set(granted, true));

            for capability in Capability::ALL {
                assert_eq!(
                    permissions.allows(capability),
                    capability == granted,
                    "unexpected gate for {capability}"
                );
            }
        }
    }

    #[test]
    fn denied_capability_returns_structured_error() {
        let error = ScriptPermissions::deny_all()
            .authorize(Capability::Exec)
            .expect_err("exec must be denied");

        assert_eq!(
            error,
            CapabilityError::Unauthorized {
                capability: Capability::Exec
            }
        );
    }

    #[test]
    fn unknown_capability_returns_structured_error() {
        let error = Capability::from_name("network").expect_err("network is not shared");

        assert_eq!(
            error,
            CapabilityError::UnknownCapability {
                name: "network".to_owned()
            }
        );
    }

    #[test]
    fn capability_errors_round_trip_through_script_json() {
        let errors = [
            CapabilityError::Unauthorized {
                capability: Capability::Fs,
            },
            CapabilityError::UnknownCapability {
                name: "network".to_owned(),
            },
            CapabilityError::Engine {
                capability: Capability::Tools,
                message: "tool failed".to_owned(),
            },
            CapabilityError::Timeout {
                capability: Capability::Exec,
                timeout_ms: 30_000,
            },
            CapabilityError::Cancelled {
                capability: Capability::Hooks,
            },
            CapabilityError::InvalidRequest {
                capability: Capability::Ui,
                message: "panel id is empty".to_owned(),
            },
        ];

        for error in errors {
            let json = serde_json::to_value(&error).expect("serialize capability error");
            let decoded =
                serde_json::from_value::<CapabilityError>(json).expect("deserialize error");
            assert_eq!(decoded, error);
        }
    }

    #[test]
    fn surface_is_object_safe() {
        fn accepts_surface(_: &dyn CapabilitySurface) {}

        let _ = accepts_surface;
    }

    #[test]
    fn language_wire_names_accept_js_alias() {
        assert_eq!(
            "js".parse::<ScriptLanguage>().expect("parse js"),
            ScriptLanguage::JavaScript
        );
        assert_eq!(
            serde_json::from_str::<ScriptLanguage>("\"js\"").expect("deserialize js"),
            ScriptLanguage::JavaScript
        );
        assert_eq!(ScriptLanguage::JavaScript.to_string(), "javascript");
    }
}
