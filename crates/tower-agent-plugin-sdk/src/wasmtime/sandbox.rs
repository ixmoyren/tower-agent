//! 通用的沙箱实例：拥有 `Store` 与某个组件实例，并允许调用方按自己的 WIT 协议驱动它。

use wasmtime::Store;

use super::{HostState, SandboxEngine, SandboxError};

/// 一个沙箱实例：`Store<HostState>` + 组件实例 `I`。
///
/// `I` 是调用方用 `bindgen!` 生成的 world 类型。这样「拥有 `Store`、按共享引擎实例化、
/// 与 [`crate::SandboxPool`] 池化」这些通用部分只实现一次；具体解释器（如 monty）
/// 只需要包一层薄 newtype 暴露自己的协议方法。
pub struct Sandbox<I> {
    store: Store<HostState>,
    instance: I,
}

impl<I> Sandbox<I> {
    /// 用共享引擎实例化：`make` 拿 `&mut Store` 造出组件实例（通常是
    /// `World::instantiate(store, engine.component(), engine.linker())`）。
    pub fn instantiate(
        engine: &SandboxEngine,
        make: impl FnOnce(&mut Store<HostState>) -> Result<I, wasmtime::Error>,
    ) -> Result<Self, SandboxError> {
        let mut store = engine.new_store();
        let instance =
            make(&mut store).map_err(|error| SandboxError::Instance(error.to_string()))?;
        Ok(Self { store, instance })
    }

    /// 用已有的 `Store` 与实例构造（调用方自行实例化时用）。
    #[must_use]
    pub fn from_parts(store: Store<HostState>, instance: I) -> Self {
        Self { store, instance }
    }

    /// 拆回 `(Store, 实例)`。
    #[must_use]
    pub fn into_parts(self) -> (Store<HostState>, I) {
        (self.store, self.instance)
    }

    /// 可变借用 `Store`。
    pub fn store(&mut self) -> &mut Store<HostState> {
        &mut self.store
    }

    /// 借用组件实例。
    #[must_use]
    pub fn instance(&self) -> &I {
        &self.instance
    }

    /// 同时借用 `Store`（可变）与实例（共享）——两者是不同字段，借用不冲突。
    pub fn parts_mut(&mut self) -> (&mut Store<HostState>, &I) {
        (&mut self.store, &self.instance)
    }

    /// 在「可变 `Store` + 共享实例」上跑一段逻辑，省得每处都写 `parts_mut`。
    pub fn with<R>(&mut self, f: impl FnOnce(&mut Store<HostState>, &I) -> R) -> R {
        let (store, instance) = self.parts_mut();
        f(store, instance)
    }
}

#[cfg(test)]
mod tests {
    use wasmtime::{Config, Engine};

    use super::HostState;
    use super::*;

    #[test]
    fn accessors_expose_store_and_instance_separately() {
        // 不需要 wasm：建一个普通 Engine 就能拿到 Store。
        let engine = Engine::new(&Config::new()).expect("engine");
        let mut store = Store::new(&engine, HostState::new(1024));
        store.limiter(|state| &mut state.limits);
        let mut sandbox = Sandbox::from_parts(store, 7u32);

        assert_eq!(*sandbox.instance(), 7);
        let (store, instance) = sandbox.parts_mut();
        assert_eq!(*instance, 7);
        let _: &mut Store<HostState> = store;
        let doubled = sandbox.with(|_store, instance| instance * 2);
        assert_eq!(doubled, 14);
        let (_store, instance) = sandbox.into_parts();
        assert_eq!(instance, 7);
    }
}
