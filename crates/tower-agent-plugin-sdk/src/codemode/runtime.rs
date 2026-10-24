//! Interpreter discovery and codemode execution routing.
//!
//! An interpreter advertises its stable id, language and supported script modes.
//! The catalog is model-visible; execution routes a generated script back to the
//! concrete interpreter selected by mode, language and optional interpreter id.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ScriptPermissions;

/// How a generated script is intended to affect the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptMode {
    /// Run one generated program to complete an operation.
    Codemode,
    /// Load generated source as a persistent agent-behavior plugin.
    Jit,
}

impl ScriptMode {
    /// Stable wire and model-facing name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codemode => "codemode",
            Self::Jit => "jit",
        }
    }
}

impl fmt::Display for ScriptMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One interpreter capability advertised to the model and runtime router.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterpreterDescriptor {
    /// Stable implementation id, for example `rquickjs` or `monty`.
    pub id: String,
    /// Human-readable implementation name.
    pub name: String,
    /// Script language, for example `javascript` or `python`.
    pub language: String,
    /// Modes this interpreter can execute.
    pub modes: BTreeSet<ScriptMode>,
    /// Short model-facing description.
    pub description: String,
}

impl InterpreterDescriptor {
    /// Construct an interpreter descriptor with no modes.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        language: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            language: language.into(),
            modes: BTreeSet::new(),
            description: String::new(),
        }
    }

    /// Add one supported mode.
    #[must_use]
    pub fn supporting(mut self, mode: ScriptMode) -> Self {
        self.modes.insert(mode);
        self
    }

    /// Attach a model-facing description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Whether this interpreter supports `mode`.
    #[must_use]
    pub fn supports(&self, mode: ScriptMode) -> bool {
        self.modes.contains(&mode)
    }
}

/// Common interpreter metadata contract.
pub trait ScriptInterpreter: Send + Sync + 'static {
    /// Return the model-visible metadata for this interpreter.
    fn interpreter(&self) -> InterpreterDescriptor;
}

/// A generated codemode program routed to one interpreter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodeRequest {
    /// Optional explicit interpreter id. Required when multiple interpreters
    /// support the same language and mode.
    pub interpreter: Option<String>,
    /// Generated script language.
    pub language: String,
    /// Generated source.
    pub source: String,
    /// Capabilities granted to the program.
    pub permissions: ScriptPermissions,
    /// Optional total execution timeout.
    pub timeout_ms: Option<u64>,
}

impl CodemodeRequest {
    /// Construct a request with default permissions.
    #[must_use]
    pub fn new(language: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            interpreter: None,
            language: language.into(),
            source: source.into(),
            permissions: ScriptPermissions::default(),
            timeout_ms: None,
        }
    }

    /// Require a specific interpreter implementation.
    #[must_use]
    pub fn on_interpreter(mut self, interpreter: impl Into<String>) -> Self {
        self.interpreter = Some(interpreter.into());
        self
    }
}

/// Result of one codemode execution.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CodemodeResult {
    /// Structured result returned to the agent.
    pub value: Value,
    /// Optional human-readable diagnostics.
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

/// A concrete codemode interpreter.
pub trait CodemodeExecutor: ScriptInterpreter {
    /// Execute one generated program.
    fn execute_codemode(
        &self,
        request: CodemodeRequest,
    ) -> BoxFuture<'_, Result<CodemodeResult, ScriptRuntimeError>>;
}

/// Routing or execution failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScriptRuntimeError {
    /// No interpreter supports the requested mode and language.
    #[error("no `{mode}` interpreter supports language `{language}`")]
    Unsupported {
        /// Requested mode.
        mode: ScriptMode,
        /// Requested language.
        language: String,
    },
    /// More than one interpreter matches and no interpreter id was provided.
    #[error(
        "multiple `{mode}` interpreters support language `{language}`: {candidates}; select one explicitly"
    )]
    Ambiguous {
        /// Requested mode.
        mode: ScriptMode,
        /// Requested language.
        language: String,
        /// Matching interpreter ids.
        candidates: String,
    },
    /// The requested interpreter id is unknown.
    #[error("unknown interpreter `{0}`")]
    UnknownInterpreter(String),
    /// The selected interpreter failed to execute the script.
    #[error("interpreter `{interpreter}` failed: {message}")]
    Execution {
        /// Interpreter implementation.
        interpreter: String,
        /// Failure message.
        message: String,
    },
}

/// Registry of concrete codemode interpreters.
#[derive(Default)]
pub struct CodemodeRegistry {
    interpreters: BTreeMap<String, Arc<dyn CodemodeExecutor>>,
}

impl CodemodeRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one interpreter.
    pub fn register(
        &mut self,
        interpreter: Arc<dyn CodemodeExecutor>,
    ) -> Result<(), ScriptRuntimeError> {
        let descriptor = interpreter.interpreter();
        if !descriptor.supports(ScriptMode::Codemode) {
            return Err(ScriptRuntimeError::Execution {
                interpreter: descriptor.id,
                message: "does not advertise the codemode mode".to_owned(),
            });
        }
        if self.interpreters.contains_key(&descriptor.id) {
            return Err(ScriptRuntimeError::Execution {
                interpreter: descriptor.id,
                message: "interpreter id is already registered".to_owned(),
            });
        }
        self.interpreters.insert(descriptor.id.clone(), interpreter);
        Ok(())
    }

    /// Model-visible descriptors in stable interpreter-id order.
    #[must_use]
    pub fn descriptors(&self) -> Vec<InterpreterDescriptor> {
        self.interpreters
            .values()
            .map(|engine| engine.interpreter())
            .collect()
    }

    /// Execute a request through the matching interpreter.
    pub async fn execute(
        &self,
        request: CodemodeRequest,
    ) -> Result<CodemodeResult, ScriptRuntimeError> {
        let engine = if let Some(requested) = request.interpreter.as_deref() {
            let engine = self
                .interpreters
                .get(requested)
                .cloned()
                .ok_or_else(|| ScriptRuntimeError::UnknownInterpreter(requested.to_owned()))?;
            let descriptor = engine.interpreter();
            if !descriptor.supports(ScriptMode::Codemode)
                || !descriptor.language.eq_ignore_ascii_case(&request.language)
            {
                return Err(ScriptRuntimeError::Unsupported {
                    mode: ScriptMode::Codemode,
                    language: request.language,
                });
            }
            engine
        } else {
            let matches = self
                .interpreters
                .values()
                .filter(|engine| {
                    let descriptor = engine.interpreter();
                    descriptor.supports(ScriptMode::Codemode)
                        && descriptor.language.eq_ignore_ascii_case(&request.language)
                })
                .cloned()
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [only] => Arc::clone(only),
                [] => {
                    return Err(ScriptRuntimeError::Unsupported {
                        mode: ScriptMode::Codemode,
                        language: request.language,
                    });
                }
                _ => {
                    return Err(ScriptRuntimeError::Ambiguous {
                        mode: ScriptMode::Codemode,
                        language: request.language,
                        candidates: matches
                            .iter()
                            .map(|engine| engine.interpreter().id)
                            .collect::<Vec<_>>()
                            .join(" / "),
                    });
                }
            }
        };
        engine.execute_codemode(request).await
    }

    /// Render model-facing interpreter choices.
    #[must_use]
    pub fn describe_for_llm(&self) -> String {
        describe_interpreters(&self.descriptors())
    }
}

/// Render interpreter choices for an LLM prompt.
#[must_use]
pub fn describe_interpreters(interpreters: &[InterpreterDescriptor]) -> String {
    let mut output = String::from(
        "可用脚本解释器如下。先选择脚本模式（codemode 或 jit），再选择支持该模式的语言与解释器：\n",
    );
    for interpreter in interpreters {
        let modes = interpreter
            .modes
            .iter()
            .map(|mode| mode.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!(
            "- `{}`: {}；语言 `{}`；模式 {}；{}\n",
            interpreter.id, interpreter.name, interpreter.language, modes, interpreter.description
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Engine(InterpreterDescriptor);

    impl ScriptInterpreter for Engine {
        fn interpreter(&self) -> InterpreterDescriptor {
            self.0.clone()
        }
    }

    impl CodemodeExecutor for Engine {
        fn execute_codemode(
            &self,
            request: CodemodeRequest,
        ) -> BoxFuture<'_, Result<CodemodeResult, ScriptRuntimeError>> {
            Box::pin(async move {
                Ok(CodemodeResult {
                    value: serde_json::json!({
                        "interpreter": self.0.id,
                        "source": request.source,
                    }),
                    diagnostics: Vec::new(),
                })
            })
        }
    }

    #[test]
    fn descriptor_and_prompt_include_interpreter_language_and_modes() {
        let mut registry = CodemodeRegistry::new();
        registry
            .register(Arc::new(Engine(
                InterpreterDescriptor::new("demo", "Demo", "demo-lang")
                    .supporting(ScriptMode::Codemode)
                    .with_description("demo interpreter"),
            )))
            .expect("register");
        let prompt = registry.describe_for_llm();
        for expected in ["demo", "demo-lang", "codemode", "demo interpreter"] {
            assert!(prompt.contains(expected), "missing {expected}: {prompt}");
        }
    }

    #[tokio::test]
    async fn execution_routes_to_the_matching_interpreter() {
        let mut registry = CodemodeRegistry::new();
        registry
            .register(Arc::new(Engine(
                InterpreterDescriptor::new("demo", "Demo", "demo-lang")
                    .supporting(ScriptMode::Codemode),
            )))
            .expect("register");
        let result = registry
            .execute(CodemodeRequest::new("demo-lang", "return 1"))
            .await
            .expect("execute");
        assert_eq!(result.value["interpreter"], "demo");
    }
}
