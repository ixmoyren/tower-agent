//! Optional wasm sandbox infrastructure.
//!
//! This module provides the interpreter-independent part of running a wasm
//! component with Wasmtime:
//!
//! - [`SandboxEngine`] compiles a component and configures WASI, resource limits
//!   and the Wasmtime JIT cache;
//! - [`HostState`] is the per-instance WASI and memory-limit state;
//! - [`Sandbox`] owns a store and a caller-defined component instance;
//! - [`SandboxPool`] lazily creates the engine and pools instances for reuse.
//!
//! Concrete interpreters such as codemode and monty still use `wasmtime`'s
//! `bindgen!` bindings for their own WIT worlds. They use this module only for
//! the shared sandbox infrastructure.
//!
//! Sandboxing is a compile-time SDK feature, not a plugin capability. Enabling
//! this feature does not reserve a plugin name or `sandbox.*` capability.

mod engine;
mod host;
mod pool;
mod sandbox;

pub use engine::{
    DEFAULT_MAX_WASM_STACK, DEFAULT_MEMORY_LIMIT, SandboxEngine, SandboxError, SandboxOptions,
};
pub use host::HostState;
pub use pool::SandboxPool;
pub use sandbox::Sandbox;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn pool_is_lazy_and_reuses_instances() {
        let created = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&created);
        let pool: SandboxPool<usize, String> = SandboxPool::new(
            2,
            move || Ok(counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst)),
            |engine| Ok(format!("instance-of-{engine}")),
        );

        assert_eq!(pool.idle_count(), 0);
        assert_eq!(created.load(std::sync::atomic::Ordering::SeqCst), 0);

        pool.warmup(2).expect("warmup");
        assert_eq!(pool.idle_count(), 2);
        assert_eq!(created.load(std::sync::atomic::Ordering::SeqCst), 1);

        let first = pool.acquire().expect("acquire");
        assert_eq!(pool.idle_count(), 1);
        pool.release(first.clone());
        assert_eq!(pool.acquire().expect("acquire again"), first);

        pool.release("extra".into());
        pool.release("extra2".into());
        assert_eq!(pool.idle_count(), 2);
    }
}
