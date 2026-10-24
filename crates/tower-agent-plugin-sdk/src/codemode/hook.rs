//! Shared script hook registry for both codemode engines.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Hook event exposed to scripts as `tool_call` or `tool_result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptHookEvent {
    /// Runs before a tool call.
    ToolCall,
    /// Runs for each tool result.
    ToolResult,
}

impl ScriptHookEvent {
    /// Stable script-facing wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
        }
    }

    /// Parse a script-facing event name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "tool_call" => Some(Self::ToolCall),
            "tool_result" => Some(Self::ToolResult),
            _ => None,
        }
    }
}

impl fmt::Display for ScriptHookEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Engine adapter that invokes one script function.
pub trait ScriptHookHandler: Send + Sync + 'static {
    /// Invoke the script handler with a JSON payload.
    fn invoke(
        &self,
        event: ScriptHookEvent,
        payload: Value,
    ) -> BoxFuture<'static, Result<Value, String>>;
}

/// Failure while invoking a registered script hook.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScriptHookError {
    /// The handler exceeded its wall-clock budget.
    #[error("hook `{owner}` for `{event}` timed out after {timeout_ms} ms")]
    Timeout {
        /// Registered script owner.
        owner: String,
        /// Hook event.
        event: ScriptHookEvent,
        /// Configured timeout.
        timeout_ms: u64,
    },
    /// The caller cancelled hook execution.
    #[error("hook `{owner}` for `{event}` was cancelled")]
    Cancelled {
        /// Registered script owner.
        owner: String,
        /// Hook event.
        event: ScriptHookEvent,
    },
    /// The engine/script returned an error.
    #[error("hook `{owner}` for `{event}` failed: {message}")]
    Failed {
        /// Registered script owner.
        owner: String,
        /// Hook event.
        event: ScriptHookEvent,
        /// Engine error text.
        message: String,
    },
}

/// One hook result, annotated with its registered owner.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptHookResult {
    /// Script owner (usually `engine:name`).
    pub owner: String,
    /// JSON value returned by the script.
    pub value: Result<Value, ScriptHookError>,
}

#[derive(Clone)]
struct HookEntry {
    handler: Arc<dyn ScriptHookHandler>,
}

#[derive(Clone)]
struct RegistryInner {
    entries: BTreeMap<(String, ScriptHookEvent), HookEntry>,
}

/// Session-scoped registry shared by monty and JavaScript scripts.
#[derive(Clone)]
pub struct ScriptHookRegistry {
    inner: Arc<Mutex<RegistryInner>>,
    timeout: Duration,
}

impl Default for ScriptHookRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                entries: BTreeMap::new(),
            })),
            timeout: Duration::from_secs(5),
        }
    }
}

impl ScriptHookRegistry {
    /// Construct an empty registry with the default 5 second per-hook timeout.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the per-hook timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Register or replace the handler for one script owner and event.
    pub fn register(
        &self,
        owner: impl Into<String>,
        event: ScriptHookEvent,
        handler: Arc<dyn ScriptHookHandler>,
    ) {
        self.inner
            .lock()
            .expect("script hook registry poisoned")
            .entries
            .insert((owner.into(), event), HookEntry { handler });
    }

    /// Remove all hooks owned by one script. Returns the number removed.
    pub fn unregister_owner(&self, owner: &str) -> usize {
        let mut inner = self.inner.lock().expect("script hook registry poisoned");
        let before = inner.entries.len();
        inner
            .entries
            .retain(|(registered_owner, _), _| registered_owner != owner);
        before - inner.entries.len()
    }

    /// Number of registered hooks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("script hook registry poisoned")
            .entries
            .len()
    }

    /// Whether no hooks are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Invoke all hooks for `event` in stable owner order.
    pub async fn invoke(
        &self,
        event: ScriptHookEvent,
        payload: Value,
        cancellation: Option<CancellationToken>,
    ) -> Vec<ScriptHookResult> {
        let entries = self
            .inner
            .lock()
            .expect("script hook registry poisoned")
            .entries
            .iter()
            .filter(|((_, registered_event), _)| *registered_event == event)
            .map(|((owner, _), entry)| (owner.clone(), Arc::clone(&entry.handler)))
            .collect::<Vec<_>>();
        let mut results = Vec::with_capacity(entries.len());
        for (owner, handler) in entries {
            let future = handler.invoke(event, payload.clone());
            let timeout = tokio::time::sleep(self.timeout);
            tokio::pin!(future, timeout);
            let value = match cancellation.as_ref() {
                Some(cancel) => {
                    tokio::select! {
                        _ = cancel.cancelled() => Err(ScriptHookError::Cancelled {
                            owner: owner.clone(),
                            event,
                        }),
                        _ = &mut timeout => Err(ScriptHookError::Timeout {
                            owner: owner.clone(),
                            event,
                            timeout_ms: self.timeout.as_millis() as u64,
                        }),
                        result = &mut future => result.map_err(|message| ScriptHookError::Failed {
                            owner: owner.clone(),
                            event,
                            message,
                        }),
                    }
                }
                None => {
                    tokio::select! {
                        _ = &mut timeout => Err(ScriptHookError::Timeout {
                            owner: owner.clone(),
                            event,
                            timeout_ms: self.timeout.as_millis() as u64,
                        }),
                        result = &mut future => result.map_err(|message| ScriptHookError::Failed {
                            owner: owner.clone(),
                            event,
                            message,
                        }),
                    }
                }
            };
            results.push(ScriptHookResult { owner, value });
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    struct Handler {
        value: Value,
        calls: Arc<AtomicUsize>,
    }

    impl ScriptHookHandler for Handler {
        fn invoke(
            &self,
            _event: ScriptHookEvent,
            _payload: Value,
        ) -> BoxFuture<'static, Result<Value, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let value = self.value.clone();
            Box::pin(async move { Ok(value) })
        }
    }

    #[tokio::test]
    async fn invokes_only_matching_events_and_unregisters_by_owner() {
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = ScriptHookRegistry::new();
        registry.register(
            "monty:demo",
            ScriptHookEvent::ToolCall,
            Arc::new(Handler {
                value: serde_json::json!({ "ok": true }),
                calls: Arc::clone(&calls),
            }),
        );
        registry.register(
            "js:demo",
            ScriptHookEvent::ToolResult,
            Arc::new(Handler {
                value: serde_json::json!("result"),
                calls: Arc::clone(&calls),
            }),
        );

        let results = registry
            .invoke(ScriptHookEvent::ToolCall, serde_json::json!({}), None)
            .await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].owner, "monty:demo");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        assert_eq!(registry.unregister_owner("monty:demo"), 1);
        assert_eq!(registry.len(), 1);
    }

    #[tokio::test]
    async fn timeout_and_cancellation_are_explicit() {
        struct Slow;
        impl ScriptHookHandler for Slow {
            fn invoke(
                &self,
                _event: ScriptHookEvent,
                _payload: Value,
            ) -> BoxFuture<'static, Result<Value, String>> {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(Value::Null)
                })
            }
        }

        let registry = ScriptHookRegistry::new().with_timeout(Duration::from_millis(5));
        registry.register("slow", ScriptHookEvent::ToolCall, Arc::new(Slow));
        let timed = registry
            .invoke(ScriptHookEvent::ToolCall, Value::Null, None)
            .await;
        assert!(matches!(
            timed[0].value,
            Err(ScriptHookError::Timeout { .. })
        ));

        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled = registry
            .invoke(ScriptHookEvent::ToolCall, Value::Null, Some(cancel))
            .await;
        assert!(matches!(
            cancelled[0].value,
            Err(ScriptHookError::Cancelled { .. })
        ));
    }
}
