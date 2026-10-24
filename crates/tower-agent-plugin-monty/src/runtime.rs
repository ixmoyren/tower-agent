//! SDK-facing monty implementation for codemode and JIT modes.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use futures_util::future::BoxFuture;
use tower_agent_plugin_sdk::codemode::{
    CodemodeExecutor, CodemodeRequest, CodemodeResult, InterpreterDescriptor, ScriptInterpreter,
    ScriptLanguage, ScriptMode, ScriptRuntimeError,
};
use tower_agent_plugin_sdk::jit::{JitRequest, JitResult, JitRuntime};

use crate::{ModManager, ScriptWriteOptions};

const INTERPRETER_ID: &str = "monty";
const LANGUAGE: &str = "python";

/// monty interpreter implementing both SDK runtime modes.
pub struct MontyRuntime {
    manager: Arc<ModManager>,
    generations: Mutex<BTreeMap<String, u64>>,
}

impl MontyRuntime {
    /// Model-visible interpreter metadata independent of a constructed manager.
    #[must_use]
    pub fn interpreter_descriptor() -> InterpreterDescriptor {
        InterpreterDescriptor::new(INTERPRETER_ID, "monty", LANGUAGE)
            .supporting(ScriptMode::Codemode)
            .supporting(ScriptMode::Jit)
            .with_description("Python-subset engine for programs and persistent JIT plugins")
    }

    /// Wrap a configured JIT manager.
    #[must_use]
    pub fn new(manager: Arc<ModManager>) -> Arc<Self> {
        Arc::new(Self {
            manager,
            generations: Mutex::new(BTreeMap::new()),
        })
    }

    /// Shared JIT manager.
    #[must_use]
    pub fn manager(&self) -> &Arc<ModManager> {
        &self.manager
    }

    async fn write(
        &self,
        request: JitRequest,
        generation: u64,
    ) -> Result<JitResult, ScriptRuntimeError> {
        let summary = self
            .manager
            .write_script(
                &request.name,
                &request.source,
                ScriptWriteOptions {
                    permissions: Some(request.permissions),
                    requires: request.requires,
                    language: Some(ScriptLanguage::Monty),
                    enabled: Some(true),
                    scope: None,
                },
            )
            .await
            .map_err(failure)?;
        self.generations
            .lock()
            .expect("monty generation state poisoned")
            .insert(request.name.clone(), generation);
        Ok(JitResult {
            name: request.name,
            generation,
            tools: summary.tools,
        })
    }
}

impl ScriptInterpreter for MontyRuntime {
    fn interpreter(&self) -> InterpreterDescriptor {
        Self::interpreter_descriptor()
    }
}

impl CodemodeExecutor for MontyRuntime {
    fn execute_codemode(
        &self,
        request: CodemodeRequest,
    ) -> BoxFuture<'_, Result<CodemodeResult, ScriptRuntimeError>> {
        Box::pin(self.manager.execute_codemode(request))
    }
}

impl JitRuntime for MontyRuntime {
    fn load_jit(
        &self,
        request: JitRequest,
    ) -> BoxFuture<'_, Result<JitResult, ScriptRuntimeError>> {
        Box::pin(async move {
            if self
                .generations
                .lock()
                .expect("monty generation state poisoned")
                .contains_key(&request.name)
            {
                return Err(failure(format!(
                    "JIT plugin `{}` is already loaded",
                    request.name
                )));
            }
            self.write(request, 1).await
        })
    }

    fn reload_jit(
        &self,
        name: &str,
        request: JitRequest,
    ) -> BoxFuture<'_, Result<JitResult, ScriptRuntimeError>> {
        let name = name.to_owned();
        Box::pin(async move {
            let generation = self
                .generations
                .lock()
                .expect("monty generation state poisoned")
                .get(&name)
                .map(|generation| generation + 1)
                .ok_or_else(|| failure(format!("unknown JIT plugin `{name}`")))?;
            self.write(request, generation).await
        })
    }

    fn unload_jit(&self, name: &str) -> BoxFuture<'_, Result<bool, ScriptRuntimeError>> {
        let name = name.to_owned();
        Box::pin(async move {
            let removed = self
                .manager
                .remove_script(&name, None)
                .await
                .map_err(failure)?;
            self.generations
                .lock()
                .expect("monty generation state poisoned")
                .remove(&name);
            Ok(removed)
        })
    }
}

fn failure(error: impl std::fmt::Display) -> ScriptRuntimeError {
    ScriptRuntimeError::Execution {
        interpreter: INTERPRETER_ID.to_owned(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_python_for_codemode_and_jit() {
        let descriptor = MontyRuntime::interpreter_descriptor();
        assert_eq!(descriptor.id, "monty");
        assert_eq!(descriptor.language, "python");
        assert!(descriptor.supports(ScriptMode::Codemode));
        assert!(descriptor.supports(ScriptMode::Jit));
    }
}
