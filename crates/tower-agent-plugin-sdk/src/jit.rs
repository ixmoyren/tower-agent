//! Generic JIT plugin runtime contracts.
//!
//! A JIT interpreter turns generated source into a persistent plugin that can
//! register tools, hooks, UI contributions and capabilities. Concrete
//! interpreters such as monty or rquickjs implement [`JitRuntime`]; lifecycle,
//! storage and agent behavior stay on the SDK-facing contract rather than on a
//! particular implementation crate.

use std::{collections::BTreeMap, sync::Arc};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::codemode::{
    CodemodeRegistry, InterpreterDescriptor, ScriptInterpreter, ScriptMode, ScriptPermissions,
    ScriptRuntimeError,
};

/// Request to install generated source as a JIT plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JitRequest {
    /// Optional explicit interpreter id.
    pub interpreter: Option<String>,
    /// Generated script language.
    pub language: String,
    /// Stable plugin/script name.
    pub name: String,
    /// Generated source.
    pub source: String,
    /// Capabilities granted to the plugin.
    pub permissions: ScriptPermissions,
    /// Host capabilities required by the plugin.
    #[serde(default)]
    pub requires: Vec<String>,
}

impl JitRequest {
    /// Construct a JIT request with default permissions and no dependencies.
    #[must_use]
    pub fn new(
        language: impl Into<String>,
        name: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            interpreter: None,
            language: language.into(),
            name: name.into(),
            source: source.into(),
            permissions: ScriptPermissions::default(),
            requires: Vec::new(),
        }
    }

    /// Require a specific interpreter implementation.
    #[must_use]
    pub fn on_interpreter(mut self, interpreter: impl Into<String>) -> Self {
        self.interpreter = Some(interpreter.into());
        self
    }
}

/// Model-facing result after a JIT plugin is installed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JitResult {
    /// Installed plugin name.
    pub name: String,
    /// Monotonic implementation-local generation.
    pub generation: u64,
    /// Tools registered by this plugin.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// A concrete JIT interpreter.
pub trait JitRuntime: ScriptInterpreter {
    /// Load generated source as a plugin and atomically publish it.
    fn load_jit(&self, request: JitRequest)
    -> BoxFuture<'_, Result<JitResult, ScriptRuntimeError>>;

    /// Reload an existing plugin; the old generation remains active on failure.
    fn reload_jit(
        &self,
        name: &str,
        request: JitRequest,
    ) -> BoxFuture<'_, Result<JitResult, ScriptRuntimeError>>;

    /// Unload and unregister one plugin.
    fn unload_jit(&self, name: &str) -> BoxFuture<'_, Result<bool, ScriptRuntimeError>>;
}

/// Registry of concrete JIT runtimes.
#[derive(Default)]
pub struct JitRegistry {
    runtimes: BTreeMap<String, Arc<dyn JitRuntime>>,
}

impl JitRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one JIT interpreter.
    pub fn register(&mut self, runtime: Arc<dyn JitRuntime>) -> Result<(), ScriptRuntimeError> {
        let descriptor = runtime.interpreter();
        if !descriptor.supports(ScriptMode::Jit) {
            return Err(ScriptRuntimeError::Execution {
                interpreter: descriptor.id,
                message: "does not advertise the jit mode".to_owned(),
            });
        }
        if self.runtimes.contains_key(&descriptor.id) {
            return Err(ScriptRuntimeError::Execution {
                interpreter: descriptor.id,
                message: "JIT interpreter id is already registered".to_owned(),
            });
        }
        self.runtimes.insert(descriptor.id.clone(), runtime);
        Ok(())
    }

    /// Model-visible descriptors in stable interpreter-id order.
    #[must_use]
    pub fn descriptors(&self) -> Vec<InterpreterDescriptor> {
        self.runtimes
            .values()
            .map(|runtime| runtime.interpreter())
            .collect()
    }

    /// Load a plugin through the matching interpreter.
    pub async fn load(&self, request: JitRequest) -> Result<JitResult, ScriptRuntimeError> {
        let runtime = self.resolve(&request)?;
        runtime.load_jit(request).await
    }

    /// Reload a plugin through the matching interpreter.
    pub async fn reload(
        &self,
        name: &str,
        request: JitRequest,
    ) -> Result<JitResult, ScriptRuntimeError> {
        let runtime = self.resolve(&request)?;
        runtime.reload_jit(name, request).await
    }

    /// Unload a plugin from an explicit interpreter.
    pub async fn unload(&self, interpreter: &str, name: &str) -> Result<bool, ScriptRuntimeError> {
        let runtime = self
            .runtimes
            .get(interpreter)
            .ok_or_else(|| ScriptRuntimeError::UnknownInterpreter(interpreter.to_owned()))?;
        runtime.unload_jit(name).await
    }

    /// Render model-facing interpreter choices.
    #[must_use]
    pub fn describe_for_llm(&self) -> String {
        crate::codemode::describe_interpreters(&self.descriptors())
    }

    fn resolve(&self, request: &JitRequest) -> Result<Arc<dyn JitRuntime>, ScriptRuntimeError> {
        if let Some(requested) = request.interpreter.as_deref() {
            let runtime = self
                .runtimes
                .get(requested)
                .cloned()
                .ok_or_else(|| ScriptRuntimeError::UnknownInterpreter(requested.to_owned()))?;
            let descriptor = runtime.interpreter();
            if !descriptor.supports(ScriptMode::Jit)
                || !descriptor.language.eq_ignore_ascii_case(&request.language)
            {
                return Err(ScriptRuntimeError::Unsupported {
                    mode: ScriptMode::Jit,
                    language: request.language.clone(),
                });
            }
            return Ok(runtime);
        }

        let matches = self
            .runtimes
            .values()
            .filter(|runtime| {
                let descriptor = runtime.interpreter();
                descriptor.supports(ScriptMode::Jit)
                    && descriptor.language.eq_ignore_ascii_case(&request.language)
            })
            .cloned()
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [only] => Ok(Arc::clone(only)),
            [] => Err(ScriptRuntimeError::Unsupported {
                mode: ScriptMode::Jit,
                language: request.language.clone(),
            }),
            _ => Err(ScriptRuntimeError::Ambiguous {
                mode: ScriptMode::Jit,
                language: request.language.clone(),
                candidates: matches
                    .iter()
                    .map(|runtime| runtime.interpreter().id)
                    .collect::<Vec<_>>()
                    .join(" / "),
            }),
        }
    }
}

/// Combined model-facing catalog of codemode and JIT interpreters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptRuntimeCatalog {
    /// Available interpreters, with mode support merged by interpreter id.
    pub interpreters: Vec<InterpreterDescriptor>,
}

/// Combined codemode/JIT dispatcher for hosts that expose both modes.
#[derive(Default)]
pub struct ScriptRuntimeRouter {
    codemode: CodemodeRegistry,
    jit: JitRegistry,
}

impl ScriptRuntimeRouter {
    /// Construct an empty router.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a codemode executor.
    pub fn register_codemode(
        &mut self,
        executor: Arc<dyn crate::codemode::CodemodeExecutor>,
    ) -> Result<(), ScriptRuntimeError> {
        self.codemode.register(executor)
    }

    /// Register a JIT runtime.
    pub fn register_jit(&mut self, runtime: Arc<dyn JitRuntime>) -> Result<(), ScriptRuntimeError> {
        self.jit.register(runtime)
    }

    /// Combined model-facing catalog.
    #[must_use]
    pub fn catalog(&self) -> ScriptRuntimeCatalog {
        ScriptRuntimeCatalog::from_registries(&self.codemode, &self.jit)
    }

    /// Execute one codemode program.
    pub async fn execute_codemode(
        &self,
        request: crate::codemode::CodemodeRequest,
    ) -> Result<crate::codemode::CodemodeResult, ScriptRuntimeError> {
        self.codemode.execute(request).await
    }

    /// Load one JIT plugin.
    pub async fn load_jit(&self, request: JitRequest) -> Result<JitResult, ScriptRuntimeError> {
        self.jit.load(request).await
    }

    /// Reload one JIT plugin.
    pub async fn reload_jit(
        &self,
        name: &str,
        request: JitRequest,
    ) -> Result<JitResult, ScriptRuntimeError> {
        self.jit.reload(name, request).await
    }

    /// Unload one JIT plugin.
    pub async fn unload_jit(
        &self,
        interpreter: &str,
        name: &str,
    ) -> Result<bool, ScriptRuntimeError> {
        self.jit.unload(interpreter, name).await
    }
}

impl ScriptRuntimeCatalog {
    /// Build a catalog from precomputed descriptors.
    #[must_use]
    pub fn from_interpreters(
        interpreters: impl IntoIterator<Item = InterpreterDescriptor>,
    ) -> Self {
        let mut by_id = std::collections::BTreeMap::<String, InterpreterDescriptor>::new();
        for descriptor in interpreters {
            match by_id.entry(descriptor.id.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(descriptor);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().modes.extend(descriptor.modes);
                }
            }
        }
        Self {
            interpreters: by_id.into_values().collect(),
        }
    }

    /// Merge codemode and JIT registry advertisements into one model-facing
    /// catalog.
    #[must_use]
    pub fn from_registries(codemode: &CodemodeRegistry, jit: &JitRegistry) -> Self {
        Self::from_interpreters(codemode.descriptors().into_iter().chain(jit.descriptors()))
    }

    /// Render interpreter, language and supported modes for an LLM prompt.
    #[must_use]
    pub fn describe_for_llm(&self) -> String {
        crate::codemode::describe_interpreters(&self.interpreters)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Runtime(InterpreterDescriptor);

    impl ScriptInterpreter for Runtime {
        fn interpreter(&self) -> InterpreterDescriptor {
            self.0.clone()
        }
    }

    impl JitRuntime for Runtime {
        fn load_jit(
            &self,
            request: JitRequest,
        ) -> BoxFuture<'_, Result<JitResult, ScriptRuntimeError>> {
            Box::pin(async move {
                Ok(JitResult {
                    name: request.name,
                    generation: 1,
                    tools: Vec::new(),
                })
            })
        }

        fn reload_jit(
            &self,
            name: &str,
            _request: JitRequest,
        ) -> BoxFuture<'_, Result<JitResult, ScriptRuntimeError>> {
            let name = name.to_owned();
            Box::pin(async move {
                Ok(JitResult {
                    name,
                    generation: 2,
                    tools: Vec::new(),
                })
            })
        }

        fn unload_jit(&self, _name: &str) -> BoxFuture<'_, Result<bool, ScriptRuntimeError>> {
            Box::pin(async { Ok(true) })
        }
    }

    #[tokio::test]
    async fn routes_jit_requests_by_language() {
        let mut registry = JitRegistry::new();
        registry
            .register(Arc::new(Runtime(
                InterpreterDescriptor::new("demo", "Demo", "demo-lang").supporting(ScriptMode::Jit),
            )))
            .expect("register");
        let result = registry
            .load(JitRequest::new("demo-lang", "demo-plugin", "source"))
            .await
            .expect("load");
        assert_eq!(result.name, "demo-plugin");
    }

    #[test]
    fn combined_catalog_tells_the_model_both_modes_and_language() {
        let catalog = ScriptRuntimeCatalog::from_interpreters([
            InterpreterDescriptor::new("demo", "Demo", "demo-lang")
                .supporting(ScriptMode::Codemode),
            InterpreterDescriptor::new("demo", "Demo", "demo-lang").supporting(ScriptMode::Jit),
        ]);
        assert_eq!(catalog.interpreters.len(), 1);
        assert!(catalog.interpreters[0].supports(ScriptMode::Codemode));
        assert!(catalog.interpreters[0].supports(ScriptMode::Jit));
        let prompt = catalog.describe_for_llm();
        for expected in ["demo", "demo-lang", "codemode", "jit"] {
            assert!(prompt.contains(expected), "missing {expected}: {prompt}");
        }
    }
}
