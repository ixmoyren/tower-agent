//! 与具体网络框架无关的优雅停机协调器。
//!
//! Web 宿主应先停止接收新的传输层任务，再等待正在执行的 agent 操作完成。
//! [`ShutdownCoordinator`] 管理以下生命周期：
//!
//! `Running → Draining →（按需强制取消）→ Stopped`。
//!
//! 本模块不依赖具体传输层。Axum 可以把
//! [`ShutdownCoordinator::wait_for_shutdown_start`] 交给
//! `with_graceful_shutdown`，同时由本 crate 统计在途 agent 请求并运行资源关闭 Hook。

use std::{
    fmt,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::{future::BoxFuture, future::join_all};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;
use tower_agent_core::AgentError;

/// 当前停机阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownState {
    /// 接受新任务，并允许在途操作继续运行。
    Running,
    /// 拒绝新任务，等待活跃操作排空。
    Draining,
    /// 本轮停机流程已经结束。
    Stopped,
}

/// [`ShutdownCoordinator::shutdown_with`] 使用的等待时限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownOptions {
    /// 在途操作自行完成的最长等待时间。
    pub grace_period: Duration,
    /// 宽限期结束后，等待强制取消和停机 Hook 完成的最长时间。
    pub cancellation_timeout: Duration,
}

impl Default for ShutdownOptions {
    fn default() -> Self {
        Self {
            grace_period: Duration::from_secs(30),
            cancellation_timeout: Duration::from_secs(5),
        }
    }
}

/// 一次停机流程的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    /// 所有受跟踪操作是否在最终超时前结束。
    pub drained: bool,
    /// 宽限期结束后是否触发了强制取消。
    pub forced: bool,
    /// 停机返回时仍未结束的操作数。
    pub remaining_operations: usize,
    /// 资源 Hook 的失败信息或超时诊断。
    pub hook_errors: Vec<String>,
}

/// agent 请求排空后需要关闭的资源。
pub trait ShutdownHook: Send + Sync + 'static {
    /// 在停机流程中运行一次关闭逻辑。
    fn shutdown(&self) -> BoxFuture<'static, Result<(), String>>;
}

enum Phase {
    Running,
    Draining,
    Stopped(ShutdownReport),
}

impl Phase {
    fn state(&self) -> ShutdownState {
        match self {
            Self::Running => ShutdownState::Running,
            Self::Draining => ShutdownState::Draining,
            Self::Stopped(_) => ShutdownState::Stopped,
        }
    }
}

struct HookEntry {
    name: String,
    hook: Arc<dyn ShutdownHook>,
}

struct Inner {
    phase: StdMutex<Phase>,
    inflight: AtomicUsize,
    zero: Notify,
    stopped: Notify,
    start_signal: CancellationToken,
    force_signal: CancellationToken,
    hooks: StdMutex<Vec<HookEntry>>,
    shutdown_lock: Mutex<()>,
}

/// 可克隆的停机协调器，由所有 agent 服务共享。
#[derive(Clone)]
pub struct ShutdownCoordinator {
    inner: Arc<Inner>,
}

impl fmt::Debug for ShutdownCoordinator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShutdownCoordinator")
            .field("state", &self.state())
            .field("inflight", &self.inflight())
            .finish()
    }
}

impl Default for ShutdownCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl ShutdownCoordinator {
    /// 构造一个仍可接受新任务的协调器。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                phase: StdMutex::new(Phase::Running),
                inflight: AtomicUsize::new(0),
                zero: Notify::new(),
                stopped: Notify::new(),
                start_signal: CancellationToken::new(),
                force_signal: CancellationToken::new(),
                hooks: StdMutex::new(Vec::new()),
                shutdown_lock: Mutex::new(()),
            }),
        }
    }

    /// 当前生命周期阶段。
    #[must_use]
    pub fn state(&self) -> ShutdownState {
        self.inner
            .phase
            .lock()
            .expect("shutdown phase poisoned")
            .state()
    }

    /// 当前仍在执行的受跟踪操作数。
    #[must_use]
    pub fn inflight(&self) -> usize {
        self.inner.inflight.load(Ordering::SeqCst)
    }

    /// 是否仍允许接收新任务。
    #[must_use]
    pub fn is_accepting(&self) -> bool {
        self.state() == ShutdownState::Running
    }

    /// 拒绝新任务，并发送传输层停机信号。
    ///
    /// 只有第一次调用返回 `true`。
    pub fn begin_shutdown(&self) -> bool {
        let mut phase = self.inner.phase.lock().expect("shutdown phase poisoned");
        if !matches!(*phase, Phase::Running) {
            return false;
        }
        *phase = Phase::Draining;
        drop(phase);
        self.inner.start_signal.cancel();
        true
    }

    /// 跟踪一个请求、工具任务或后台操作。
    pub fn track_operation(&self) -> Result<ShutdownGuard, AgentError> {
        let phase = self.inner.phase.lock().expect("shutdown phase poisoned");
        if !matches!(*phase, Phase::Running) {
            return Err(AgentError::ShuttingDown);
        }
        self.inner.inflight.fetch_add(1, Ordering::SeqCst);
        Ok(ShutdownGuard {
            inner: Arc::clone(&self.inner),
        })
    }

    /// 进入排空阶段时取消。Axum 可等待该信号并停止接收新连接。
    #[must_use]
    pub fn stop_signal(&self) -> CancellationToken {
        self.inner.start_signal.clone()
    }

    /// 仅在优雅排空超过宽限期时取消。
    #[must_use]
    pub fn force_signal(&self) -> CancellationToken {
        self.inner.force_signal.clone()
    }

    /// 可直接交给 `axum::serve(...).with_graceful_shutdown(...)` 的 Future。
    pub async fn wait_for_shutdown_start(&self) {
        self.inner.start_signal.cancelled().await;
    }

    /// 等待一次完整的停机结果。
    pub async fn wait_for_stopped(&self) -> ShutdownReport {
        loop {
            let stopped = self.inner.stopped.notified();
            if let Phase::Stopped(report) =
                &*self.inner.phase.lock().expect("shutdown phase poisoned")
            {
                return report.clone();
            }
            stopped.await;
        }
    }

    /// 在停机开始前注册资源关闭 Hook。
    pub fn register_hook(
        &self,
        name: impl Into<String>,
        hook: Arc<dyn ShutdownHook>,
    ) -> Result<(), AgentError> {
        let phase = self.inner.phase.lock().expect("shutdown phase poisoned");
        if !matches!(*phase, Phase::Running) {
            return Err(AgentError::ShuttingDown);
        }
        self.inner
            .hooks
            .lock()
            .expect("shutdown hooks poisoned")
            .push(HookEntry {
                name: name.into(),
                hook,
            });
        Ok(())
    }

    /// 使用默认时限开始排空并等待完成。
    pub async fn shutdown(&self) -> ShutdownReport {
        self.shutdown_with(ShutdownOptions::default()).await
    }

    /// 开始排空；宽限期后按需强制取消，最后运行资源关闭 Hook。
    pub async fn shutdown_with(&self, options: ShutdownOptions) -> ShutdownReport {
        let _serial = self.inner.shutdown_lock.lock().await;
        if let Phase::Stopped(report) = &*self.inner.phase.lock().expect("shutdown phase poisoned")
        {
            return report.clone();
        }
        self.begin_shutdown();

        let mut drained = self.wait_until_zero(options.grace_period).await;
        let forced = !drained;
        if forced {
            self.inner.force_signal.cancel();
            drained = self.wait_until_zero(options.cancellation_timeout).await;
        }

        let hooks = self
            .inner
            .hooks
            .lock()
            .expect("shutdown hooks poisoned")
            .drain(..)
            .collect::<Vec<_>>();
        let hook_futures = hooks.into_iter().map(|entry| async move {
            let name = entry.name;
            match entry.hook.shutdown().await {
                Ok(()) => None,
                Err(error) => Some(format!("shutdown hook `{name}` failed: {error}")),
            }
        });
        let hook_errors = match tokio::time::timeout(
            options.cancellation_timeout,
            join_all(hook_futures),
        )
        .await
        {
            Ok(results) => results.into_iter().flatten().collect(),
            Err(_) => vec![format!(
                "shutdown hooks timed out after {} ms",
                options.cancellation_timeout.as_millis()
            )],
        };

        let report = ShutdownReport {
            drained: drained && self.inflight() == 0,
            forced,
            remaining_operations: self.inflight(),
            hook_errors,
        };
        *self.inner.phase.lock().expect("shutdown phase poisoned") = Phase::Stopped(report.clone());
        self.inner.stopped.notify_waiters();
        report
    }

    async fn wait_until_zero(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                if self.inflight() == 0 {
                    return;
                }
                let notified = self.inner.zero.notified();
                if self.inflight() == 0 {
                    return;
                }
                notified.await;
            }
        })
        .await
        .is_ok()
    }
}

/// 一个在途操作的 RAII 计数守卫。
pub struct ShutdownGuard {
    inner: Arc<Inner>,
}

impl fmt::Debug for ShutdownGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShutdownGuard")
            .field("inflight", &self.inner.inflight.load(Ordering::SeqCst))
            .finish()
    }
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        if self.inner.inflight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.inner.zero.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn begins_by_rejecting_new_work() {
        let coordinator = ShutdownCoordinator::new();
        assert!(coordinator.is_accepting());
        assert!(coordinator.begin_shutdown());
        assert!(!coordinator.begin_shutdown());
        assert_eq!(coordinator.state(), ShutdownState::Draining);
        assert!(matches!(
            coordinator.track_operation(),
            Err(AgentError::ShuttingDown)
        ));
    }

    #[tokio::test]
    async fn drain_waits_for_inflight_operation() {
        let coordinator = ShutdownCoordinator::new();
        let guard = coordinator.track_operation().expect("track");
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(guard);
        });
        let report = coordinator
            .shutdown_with(ShutdownOptions {
                grace_period: Duration::from_secs(1),
                cancellation_timeout: Duration::from_millis(100),
            })
            .await;
        release.await.expect("release task");
        assert!(report.drained);
        assert!(!report.forced);
        assert_eq!(report.remaining_operations, 0);
    }

    #[tokio::test]
    async fn forces_cancellation_after_grace_period() {
        let coordinator = ShutdownCoordinator::new();
        let _guard = coordinator.track_operation().expect("track");
        let report = coordinator
            .shutdown_with(ShutdownOptions {
                grace_period: Duration::from_millis(10),
                cancellation_timeout: Duration::from_millis(10),
            })
            .await;
        assert!(report.forced);
        assert!(!report.drained);
        assert_eq!(report.remaining_operations, 1);
        assert!(coordinator.force_signal().is_cancelled());
    }

    #[tokio::test]
    async fn runs_shutdown_hooks() {
        struct Hook(Arc<AtomicUsize>);

        impl ShutdownHook for Hook {
            fn shutdown(&self) -> BoxFuture<'static, Result<(), String>> {
                let calls = Arc::clone(&self.0);
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }
        }

        let coordinator = ShutdownCoordinator::new();
        let calls = Arc::new(AtomicUsize::new(0));
        coordinator
            .register_hook("plugin", Arc::new(Hook(Arc::clone(&calls))))
            .expect("register hook");
        let report = coordinator.shutdown().await;
        assert!(report.hook_errors.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
