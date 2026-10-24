//! SDK-facing rquickjs implementation for codemode and JIT modes.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tower_agent_plugin_sdk::codemode::{
    CodemodeExecutor, CodemodeRequest, CodemodeResult, InterpreterDescriptor, ScriptInterpreter,
    ScriptMode, ScriptRuntimeError,
};
use tower_agent_plugin_sdk::jit::{JitRequest, JitResult, JitRuntime};

use crate::CodemodeHost;

const INTERPRETER_ID: &str = "rquickjs";
const LANGUAGE: &str = "javascript";

/// rquickjs interpreter implementing both SDK runtime modes.
pub struct RquickjsRuntime {
    host: Arc<CodemodeHost>,
    plugins: Mutex<BTreeMap<String, JitState>>,
}

struct JitState {
    generation: u64,
    tools: BTreeSet<String>,
}

impl RquickjsRuntime {
    /// Model-visible interpreter metadata independent of a constructed host.
    #[must_use]
    pub fn interpreter_descriptor() -> InterpreterDescriptor {
        InterpreterDescriptor::new(INTERPRETER_ID, "rquickjs", LANGUAGE)
            .supporting(ScriptMode::Codemode)
            .supporting(ScriptMode::Jit)
            .with_description("JavaScript engine for one-shot programs and persistent JIT plugins")
    }

    /// Wrap a configured codemode host.
    #[must_use]
    pub fn new(host: Arc<CodemodeHost>) -> Arc<Self> {
        Arc::new(Self {
            host,
            plugins: Mutex::new(BTreeMap::new()),
        })
    }

    /// Shared codemode host.
    #[must_use]
    pub fn host(&self) -> &Arc<CodemodeHost> {
        &self.host
    }

    async fn run(
        &self,
        source: &str,
        timeout_ms: Option<u64>,
    ) -> Result<CodemodeResult, ScriptRuntimeError> {
        let timeout = timeout_ms.and_then(|timeout| u32::try_from(timeout).ok());
        let result = self
            .host
            .run_async(source, timeout)
            .await
            .map_err(|error| failure(error.to_string()))?;
        let commands = result
            .commands
            .iter()
            .map(|command| {
                json!({
                    "name": command.name,
                    "args": command.args,
                    "code": command.code,
                })
            })
            .collect::<Vec<_>>();
        Ok(CodemodeResult {
            value: json!({
                "value": result.value.as_deref().and_then(|value| serde_json::from_str::<Value>(value).ok()),
                "stdout": result.stdout,
                "commands": commands,
                "truncated": result.truncated,
            }),
            diagnostics: Vec::new(),
        })
    }

    async fn install_source(&self, request: JitRequest) -> Result<JitResult, ScriptRuntimeError> {
        if self
            .plugins
            .lock()
            .expect("rquickjs JIT state poisoned")
            .contains_key(&request.name)
        {
            return Err(failure(format!(
                "JIT plugin `{}` is already loaded",
                request.name
            )));
        }
        let before = self
            .host
            .host_api()
            .capabilities()
            .into_iter()
            .collect::<BTreeSet<_>>();
        self.run(&request.source, None).await?;
        let after = self
            .host
            .host_api()
            .capabilities()
            .into_iter()
            .collect::<BTreeSet<_>>();
        let tools = after.difference(&before).cloned().collect::<BTreeSet<_>>();
        let tool_names = tools.iter().cloned().collect::<Vec<_>>();
        self.plugins
            .lock()
            .expect("rquickjs JIT state poisoned")
            .insert(
                request.name.clone(),
                JitState {
                    generation: 1,
                    tools,
                },
            );
        Ok(JitResult {
            name: request.name,
            generation: 1,
            tools: tool_names,
        })
    }
}

impl ScriptInterpreter for RquickjsRuntime {
    fn interpreter(&self) -> InterpreterDescriptor {
        Self::interpreter_descriptor()
    }
}

impl CodemodeExecutor for RquickjsRuntime {
    fn execute_codemode(
        &self,
        request: CodemodeRequest,
    ) -> BoxFuture<'_, Result<CodemodeResult, ScriptRuntimeError>> {
        Box::pin(async move { self.run(&request.source, request.timeout_ms).await })
    }
}

impl JitRuntime for RquickjsRuntime {
    fn load_jit(
        &self,
        request: JitRequest,
    ) -> BoxFuture<'_, Result<JitResult, ScriptRuntimeError>> {
        Box::pin(self.install_source(request))
    }

    fn reload_jit(
        &self,
        name: &str,
        request: JitRequest,
    ) -> BoxFuture<'_, Result<JitResult, ScriptRuntimeError>> {
        let name = name.to_owned();
        Box::pin(async move {
            let generation = self
                .plugins
                .lock()
                .expect("rquickjs JIT state poisoned")
                .get(&name)
                .map(|state| state.generation + 1)
                .ok_or_else(|| failure(format!("unknown JIT plugin `{name}`")))?;
            let before = self
                .host
                .host_api()
                .capabilities()
                .into_iter()
                .collect::<BTreeSet<_>>();
            self.run(&request.source, None).await?;
            let after = self
                .host
                .host_api()
                .capabilities()
                .into_iter()
                .collect::<BTreeSet<_>>();
            let tools = after.difference(&before).cloned().collect::<BTreeSet<_>>();
            let old_tools = self
                .plugins
                .lock()
                .expect("rquickjs JIT state poisoned")
                .get(&name)
                .map(|state| state.tools.clone())
                .unwrap_or_default();
            for tool in old_tools.difference(&tools) {
                self.host.host_api().remove_tool(tool);
            }
            self.plugins
                .lock()
                .expect("rquickjs JIT state poisoned")
                .insert(
                    name.clone(),
                    JitState {
                        generation,
                        tools: tools.clone(),
                    },
                );
            Ok(JitResult {
                name,
                generation,
                tools: tools.into_iter().collect(),
            })
        })
    }

    fn unload_jit(&self, name: &str) -> BoxFuture<'_, Result<bool, ScriptRuntimeError>> {
        let name = name.to_owned();
        Box::pin(async move {
            let Some(state) = self
                .plugins
                .lock()
                .expect("rquickjs JIT state poisoned")
                .remove(&name)
            else {
                return Ok(false);
            };
            for tool in state.tools {
                self.host.host_api().remove_tool(&tool);
            }
            Ok(true)
        })
    }
}

fn failure(message: impl Into<String>) -> ScriptRuntimeError {
    ScriptRuntimeError::Execution {
        interpreter: INTERPRETER_ID.to_owned(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_javascript_for_codemode_and_jit() {
        let descriptor = RquickjsRuntime::interpreter_descriptor();
        assert_eq!(descriptor.id, "rquickjs");
        assert_eq!(descriptor.language, "javascript");
        assert!(descriptor.supports(ScriptMode::Codemode));
        assert!(descriptor.supports(ScriptMode::Jit));
    }

    #[tokio::test]
    async fn executes_a_codemode_program_through_the_sdk_contract() {
        let root = tempfile::tempdir().expect("session root");
        let host = CodemodeHost::new(crate::CodemodeOptions::embedded(root.path()))
            .expect("codemode host");
        let runtime = RquickjsRuntime::new(host);
        let result = runtime
            .execute_codemode(CodemodeRequest::new("javascript", "return 1 + 2;"))
            .await
            .expect("execute codemode");
        assert_eq!(result.value["value"], 3);

        let loaded = runtime
            .load_jit(JitRequest::new(
                "javascript",
                "demo-plugin",
                r#"
function greet(args) { return "hello " + args.name; }
tool_register("demo_greet", "greet", "demo", { type: "object" });
"#,
            ))
            .await
            .expect("load JIT plugin");
        assert_eq!(loaded.tools, vec!["demo_greet"]);
        assert!(runtime.host().host_api().tool("demo_greet").is_some());
    }
}
