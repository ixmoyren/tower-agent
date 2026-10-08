//! `tower-agent-wasmtime` — 通用 wasm 沙箱基础设施。
//!
//! 把「用 wasmtime 在组件模型里跑一个 wasm 组件，并给它 WASI + 内存上限 + JIT 缓存 +
//! 实例池」这套**与具体解释器无关**的部分独立出来，供不同的 wasm 解释器复用：
//!
//! - [`SandboxEngine`]：按 wasm 字节编译出组件，配好 WASI（默认无 preopen）与
//!   `StoreLimits`，并开启 wasmtime 的 JIT 缓存（缓存键包含 wasmtime 版本，升级自动失效）；
//! - [`HostState`]：`Store` 的宿主状态（`WasiCtx` + `ResourceTable` + `StoreLimits`）；
//! - [`Sandbox`]：**通用**实例（拥有 `Store` + 组件实例），按引擎实例化并暴露借用；
//! - [`SandboxPool`]：**通用**的沙箱实例池（惰性建引擎、可选预热、借出/归还）。
//!
//! 具体解释器（如 `tower-agent-monty` 的 monty 组件）自己用
//! `wasmtime::component::bindgen!` 生成绑定、驱动状态机，并复用这里的引擎与池。

mod engine;
mod host;
mod pool;
mod sandbox;

pub use engine::{DEFAULT_MEMORY_LIMIT, SandboxEngine, SandboxError, SandboxOptions};
pub use host::HostState;
pub use pool::SandboxPool;
pub use sandbox::Sandbox;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn pool_is_lazy_and_reuses_instances() {
        // 用「自增 id」当假引擎/假实例，验证池的惰性创建、预热、借出与归还（不涉及 wasm）。
        let created = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&created);
        let pool: SandboxPool<usize, String> = SandboxPool::new(
            2,
            move || Ok(counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst)),
            |engine| Ok(format!("instance-of-{engine}")),
        );

        assert_eq!(pool.idle_count(), 0);
        assert_eq!(created.load(std::sync::atomic::Ordering::SeqCst), 0, "引擎应按需创建");

        pool.warmup(2).expect("warmup");
        assert_eq!(pool.idle_count(), 2);
        assert_eq!(created.load(std::sync::atomic::Ordering::SeqCst), 1, "引擎只建一次");

        let first = pool.acquire().expect("acquire");
        assert_eq!(pool.idle_count(), 1);
        pool.release(first.clone());
        assert_eq!(pool.acquire().expect("acquire again"), first, "实例被复用");

        // 池上限之外归还的实例被丢弃。
        pool.release("extra".into());
        pool.release("extra2".into());
        assert_eq!(pool.idle_count(), 2);
    }
}
