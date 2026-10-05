//! 插件生命周期与 in-flight 记账（`docs/plugins.md` 第 7 节）。
//!
//! 状态机主干：`Discovered → Loaded → Validated → Resolved → Started`；`Started ↔ Stopped`
//! 支持 JIT 重载，任意非 `Unloaded` 状态都能因失败回滚到 `Unloaded`。动态插件在 `Unload`
//! 前必须 `Stop` 且 in-flight 归零，否则报 [`PluginError::UnloadBusy`]。

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use crate::PluginError;

/// 插件生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PluginState {
    /// 已发现，尚未加载。
    Discovered,
    /// 已加载（动态插件已 `dlopen`）。
    Loaded,
    /// 清单 / ABI / 权限校验通过。
    Validated,
    /// 依赖与版本解析完成。
    Resolved,
    /// 已初始化并开始服务。
    Started,
    /// 已停止，等待 in-flight 归零后卸载。
    Stopped,
    /// 已卸载（终态）。
    Unloaded,
}

impl PluginState {
    /// 合法迁移。
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        use PluginState::{Discovered, Loaded, Resolved, Started, Stopped, Unloaded, Validated};
        if next == Unloaded {
            return self != Unloaded;
        }
        matches!(
            (self, next),
            (Discovered, Loaded)
                | (Loaded, Validated)
                | (Validated, Resolved)
                | (Resolved, Started)
                | (Started, Stopped)
                | (Stopped, Started)
        )
    }
}

/// 一个插件的生命周期与 in-flight 引用计数。
///
/// 克隆共享同一份状态与计数，便于把句柄发给能力调用方。
#[derive(Debug, Clone)]
pub struct Lifecycle {
    name: Arc<str>,
    state: Arc<Mutex<PluginState>>,
    inflight: Arc<AtomicUsize>,
}

impl Lifecycle {
    /// 新建（`Discovered`）。
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: Arc::from(name.into()),
            state: Arc::new(Mutex::new(PluginState::Discovered)),
            inflight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 插件名。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 当前状态。
    #[must_use]
    pub fn state(&self) -> PluginState {
        *self.state.lock().expect("lifecycle mutex poisoned")
    }

    /// 当前 in-flight 调用数。
    #[must_use]
    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }

    /// 迁移到下一个状态；非法迁移返回 [`PluginError::Load`]。
    pub fn transition(&self, next: PluginState) -> Result<(), PluginError> {
        let mut state = self.state.lock().expect("lifecycle mutex poisoned");
        if !state.can_transition_to(next) {
            return Err(PluginError::Load {
                plugin: self.name.to_string(),
                message: format!("illegal lifecycle transition {state:?} -> {next:?}"),
            });
        }
        *state = next;
        Ok(())
    }

    /// 记一次调用开始；只有 `Started` 才允许，返回的 guard 在 drop 时归还计数。
    #[must_use]
    pub fn begin_call(&self) -> Option<InflightGuard> {
        if self.state() != PluginState::Started {
            return None;
        }
        self.inflight.fetch_add(1, Ordering::SeqCst);
        Some(InflightGuard {
            inflight: Arc::clone(&self.inflight),
        })
    }

    /// 停止：`Started → Stopped`。
    pub fn stop(&self) -> Result<(), PluginError> {
        self.transition(PluginState::Stopped)
    }

    /// 启动/重载：`Resolved | Stopped → Started`。
    pub fn start(&self) -> Result<(), PluginError> {
        self.transition(PluginState::Started)
    }

    /// 卸载：要求已 `Stopped` 且 in-flight 归零，否则 [`PluginError::UnloadBusy`]。
    pub fn unload(&self) -> Result<(), PluginError> {
        let state = self.state();
        if state != PluginState::Stopped {
            return Err(PluginError::Load {
                plugin: self.name.to_string(),
                message: format!("cannot unload from {state:?}; stop the plugin first"),
            });
        }
        let inflight = self.inflight();
        if inflight > 0 {
            return Err(PluginError::UnloadBusy {
                plugin: self.name.to_string(),
                inflight,
            });
        }
        self.transition(PluginState::Unloaded)
    }
}

/// `begin_call` 返回的 in-flight guard：drop 时归还计数。
#[derive(Debug)]
pub struct InflightGuard {
    inflight: Arc<AtomicUsize>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(name: &str) -> Lifecycle {
        let lifecycle = Lifecycle::new(name);
        for state in [
            PluginState::Loaded,
            PluginState::Validated,
            PluginState::Resolved,
            PluginState::Started,
        ] {
            lifecycle.transition(state).unwrap();
        }
        lifecycle
    }

    #[test]
    fn legal_transitions_and_rollback_are_allowed() {
        let lifecycle = Lifecycle::new("p");
        assert!(lifecycle.transition(PluginState::Started).is_err());
        assert!(lifecycle.transition(PluginState::Loaded).is_ok());
        assert!(
            lifecycle.transition(PluginState::Unloaded).is_ok(),
            "failure rollback"
        );
    }

    #[test]
    fn unload_requires_stop_and_zero_inflight() {
        let lifecycle = started("p");
        lifecycle.stop().unwrap();
        let guard = lifecycle.begin_call();
        assert!(guard.is_none(), "stopped plugins reject new calls");
        lifecycle.start().unwrap();
        let guard = lifecycle
            .begin_call()
            .expect("started plugins accept calls");
        lifecycle.stop().unwrap();
        assert!(matches!(
            lifecycle.unload(),
            Err(PluginError::UnloadBusy { inflight: 1, .. })
        ));
        drop(guard);
        lifecycle.unload().unwrap();
        assert_eq!(lifecycle.state(), PluginState::Unloaded);
    }

    #[test]
    fn unload_before_stop_is_rejected() {
        let lifecycle = started("p");
        assert!(matches!(lifecycle.unload(), Err(PluginError::Load { .. })));
    }
}
