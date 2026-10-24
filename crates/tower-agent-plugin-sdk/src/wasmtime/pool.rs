//! 通用沙箱实例池：惰性建引擎、可选预热、借出/归还。

use std::sync::{Arc, Mutex, OnceLock};

use super::SandboxError;

/// 引擎工厂：惰性创建共享引擎（如「编译内嵌组件」）。
type EngineFactory<E> = Box<dyn Fn() -> Result<E, SandboxError> + Send + Sync>;
/// 实例工厂：按共享引擎创建一个实例。
type InstanceFactory<E, T> = Box<dyn Fn(&E) -> Result<T, SandboxError> + Send + Sync>;

/// 通用实例池。
///
/// 泛型参数 `E` 是共享的引擎，`T` 是可借还的实例（例如「`Store` + 组件实例」）。
/// 引擎**惰性创建**（第一次用到才编译组件），因此构造池本身很便宜；`warmup` 可提前
/// 编译并预建实例。
pub struct SandboxPool<E, T> {
    engine: EngineFactory<E>,
    create: InstanceFactory<E, T>,
    shared: OnceLock<Arc<E>>,
    idle: Mutex<Vec<T>>,
    max_idle: usize,
}

impl<E, T> std::fmt::Debug for SandboxPool<E, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let idle = self.idle.lock().map(|idle| idle.len()).unwrap_or(0);
        f.debug_struct("SandboxPool")
            .field("max_idle", &self.max_idle)
            .field("idle", &idle)
            .finish_non_exhaustive()
    }
}

impl<E, T> SandboxPool<E, T>
where
    E: Send + Sync + 'static,
    T: Send + 'static,
{
    /// 用「引擎工厂」与「实例工厂」构造，最多保留 `max_idle` 个空闲实例。
    #[must_use]
    pub fn new(
        max_idle: usize,
        engine: impl Fn() -> Result<E, SandboxError> + Send + Sync + 'static,
        create: impl Fn(&E) -> Result<T, SandboxError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            engine: Box::new(engine),
            create: Box::new(create),
            shared: OnceLock::new(),
            idle: Mutex::new(Vec::new()),
            max_idle,
        }
    }

    /// 共享引擎（第一次调用时创建并编译组件）。
    pub fn engine(&self) -> Result<Arc<E>, SandboxError> {
        if let Some(engine) = self.shared.get() {
            return Ok(Arc::clone(engine));
        }
        let engine = Arc::new((self.engine)()?);
        // 并发下可能有人抢先 `set`；失败就用已存在的那份。
        let _ = self.shared.set(Arc::clone(&engine));
        Ok(self.shared.get().map(Arc::clone).unwrap_or(engine))
    }

    /// 预热：编译引擎并预先创建 `instances` 个实例放进池子。
    pub fn warmup(&self, instances: usize) -> Result<(), SandboxError> {
        let engine = self.engine()?;
        let mut idle = self.idle.lock().map_err(|_| SandboxError::Poisoned)?;
        while idle.len() < instances.min(self.max_idle) {
            idle.push((self.create)(&engine)?);
        }
        Ok(())
    }

    /// 取一个实例（池空则新建）。
    pub fn acquire(&self) -> Result<T, SandboxError> {
        if let Some(item) = self.idle.lock().map_err(|_| SandboxError::Poisoned)?.pop() {
            return Ok(item);
        }
        let engine = self.engine()?;
        (self.create)(&engine)
    }

    /// 归还一个实例（超过上限则丢弃）。
    pub fn release(&self, item: T) {
        let Ok(mut idle) = self.idle.lock() else {
            return;
        };
        if idle.len() < self.max_idle {
            idle.push(item);
        }
    }

    /// 当前空闲实例数（测试/诊断用）。
    #[must_use]
    pub fn idle_count(&self) -> usize {
        self.idle.lock().map(|idle| idle.len()).unwrap_or(0)
    }
}
