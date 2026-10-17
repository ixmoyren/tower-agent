//! Shared capability-surface adapter backed by the coreutils command service.

use std::{path::Path, sync::Arc};

use serde_json::Value;
use tower_agent_codemode_core::{
    Capability, CapabilityError, CapabilityFuture, CapabilitySurface, ExecOptions,
    ExecResult as CapabilityExecResult, FsEntry, LogLevel, ScriptPermissions, ToolDescriptor,
    ToolRegistration, UiPanel,
};

use crate::{
    CodemodeError, DEFAULT_COMMAND_TIMEOUT, EMBEDDED_COREUTILS, ExecService, Mount, SessionRoot,
    component::tower_agent::codemode::host::{ExecOpts, ExecResult},
};

/// The shared capability surface implemented by the codemode host.
///
/// This adapter currently exposes the `exec` capability. The remaining methods
/// are present because both script engines share one object-safe contract; they
/// fail closed until the corresponding host capability is wired.
pub struct ExecCapabilitySurface {
    exec: Arc<ExecService>,
    permissions: ScriptPermissions,
    _session: Option<Arc<tempfile::TempDir>>,
}

impl ExecCapabilitySurface {
    /// Build an `exec` surface over a codemode session root and mount table.
    ///
    /// An ephemeral session owns its temporary directory for as long as the
    /// surface lives. Command stdout/stderr are decoded as UTF-8 when returned
    /// through the shared JSON capability contract.
    pub fn new(session: SessionRoot, mounts: Vec<Mount>) -> Result<Self, CodemodeError> {
        let (root, owner) = match session {
            SessionRoot::Ephemeral => {
                let directory = Arc::new(
                    tempfile::tempdir()
                        .map_err(|error| CodemodeError::Worker(error.to_string()))?,
                );
                (directory.path().to_path_buf(), Some(directory))
            }
            SessionRoot::Persistent(root) => (root, None),
        };
        let options = crate::CodemodeOptions::embedded(&root);
        let exec = Arc::new(ExecService::new(
            EMBEDDED_COREUTILS,
            &root,
            &mounts,
            DEFAULT_COMMAND_TIMEOUT,
            &options.sandbox,
        )?);
        let mut permissions = ScriptPermissions::deny_all();
        permissions.exec = true;
        Ok(Self {
            exec,
            permissions,
            _session: owner,
        })
    }

    /// Session root mapped to guest `/`.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.exec.root()
    }

    fn unsupported<T>(
        &self,
        capability: Capability,
        operation: &'static str,
    ) -> CapabilityFuture<'_, T> {
        let authorization = self.authorize(capability);
        Box::pin(async move {
            authorization?;
            Err(CapabilityError::Engine {
                capability,
                message: format!("{operation} is not implemented by the exec capability surface"),
            })
        })
    }
}

impl CapabilitySurface for ExecCapabilitySurface {
    fn permissions(&self) -> ScriptPermissions {
        self.permissions
    }

    fn exec(
        &self,
        name: &str,
        args: Vec<String>,
        options: ExecOptions,
    ) -> CapabilityFuture<'_, CapabilityExecResult> {
        let authorization = self.authorize(Capability::Exec);
        let exec = Arc::clone(&self.exec);
        let name = name.to_owned();
        Box::pin(async move {
            authorization?;
            if options.cwd.is_some() {
                return Err(CapabilityError::InvalidRequest {
                    capability: Capability::Exec,
                    message: "cwd is not supported by the coreutils-wasm adapter".to_owned(),
                });
            }
            let timeout_ms = options
                .timeout_ms
                .map(u32::try_from)
                .transpose()
                .map_err(|_| CapabilityError::InvalidRequest {
                    capability: Capability::Exec,
                    message: "timeout_ms exceeds the wasm command limit".to_owned(),
                })?;
            let opts = ExecOpts {
                stdin: options.stdin.map(String::into_bytes),
                cwd: None,
                timeout_ms,
            };
            let result: ExecResult = exec.exec(&name, &args, &opts).await.map_err(|message| {
                CapabilityError::Engine {
                    capability: Capability::Exec,
                    message,
                }
            })?;
            Ok(CapabilityExecResult {
                code: result.code,
                stdout: String::from_utf8_lossy(&result.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&result.stderr).into_owned(),
            })
        })
    }

    fn fs_read(&self, _path: &str) -> CapabilityFuture<'_, Vec<u8>> {
        self.unsupported(Capability::Fs, "fs_read")
    }

    fn fs_write(&self, _path: &str, _contents: Vec<u8>) -> CapabilityFuture<'_, ()> {
        self.unsupported(Capability::Fs, "fs_write")
    }

    fn fs_read_dir(&self, _path: &str) -> CapabilityFuture<'_, Vec<FsEntry>> {
        self.unsupported(Capability::Fs, "fs_read_dir")
    }

    fn fs_exists(&self, _path: &str) -> CapabilityFuture<'_, bool> {
        self.unsupported(Capability::Fs, "fs_exists")
    }

    fn fs_mkdir(&self, _path: &str, _recursive: bool) -> CapabilityFuture<'_, ()> {
        self.unsupported(Capability::Fs, "fs_mkdir")
    }

    fn fs_remove(&self, _path: &str, _recursive: bool) -> CapabilityFuture<'_, ()> {
        self.unsupported(Capability::Fs, "fs_remove")
    }

    fn tool_call(&self, _name: &str, _args: Value) -> CapabilityFuture<'_, Value> {
        self.unsupported(Capability::Tools, "tool_call")
    }

    fn tool_list(&self) -> CapabilityFuture<'_, Vec<ToolDescriptor>> {
        self.unsupported(Capability::Tools, "tool_list")
    }

    fn tool_register(&self, _registration: ToolRegistration) -> CapabilityFuture<'_, ()> {
        self.unsupported(Capability::RegisterTools, "tool_register")
    }

    fn tool_unregister(&self, _name: &str) -> CapabilityFuture<'_, ()> {
        self.unsupported(Capability::RegisterTools, "tool_unregister")
    }

    fn state_get(&self, _key: &str) -> CapabilityFuture<'_, Option<Value>> {
        self.unsupported(Capability::State, "state_get")
    }

    fn state_set(&self, _key: &str, _value: Value) -> CapabilityFuture<'_, ()> {
        self.unsupported(Capability::State, "state_set")
    }

    fn log(&self, _level: LogLevel, _message: &str) -> CapabilityFuture<'_, ()> {
        self.unsupported(Capability::State, "log")
    }

    fn ui_status(&self, _message: &str) -> CapabilityFuture<'_, ()> {
        self.unsupported(Capability::Ui, "ui_status")
    }

    fn ui_panel(&self, _panel: UiPanel) -> CapabilityFuture<'_, ()> {
        self.unsupported(Capability::Ui, "ui_panel")
    }

    fn ui_clear(&self) -> CapabilityFuture<'_, ()> {
        self.unsupported(Capability::Ui, "ui_clear")
    }

    fn ui_tool_render(&self, _tool: &str, _renderer: Value) -> CapabilityFuture<'_, ()> {
        self.unsupported(Capability::Ui, "ui_tool_render")
    }
}
