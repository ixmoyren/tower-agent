//! 权限模型与运行时提示（`docs/plugins.md` 第 12 节）。
//!
//! 清单声明 `permissions`（`net` / `fs.write` / `process.spawn` …）；授权在运行时按需
//! 提示：某能力首次被调用且需要权限时询问用户，`AllowSession` 按会话缓存，`AllowOnce`
//! 不缓存，`Deny` 直接拒绝；无人值守模式（`--print`）默认拒绝。

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use crate::{CapabilityId, Permission, PluginError};

/// 一次权限提示的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// 允许本次调用（不缓存）。
    AllowOnce,
    /// 允许本会话（写入缓存，后续同名权限不再提示）。
    AllowSession,
    /// 拒绝。
    Deny,
}

/// 运行时权限提示接口（TUI / 命令行实现）。
pub trait PermissionPrompter: Send + Sync + 'static {
    /// 为某个能力的某项权限给出决定。
    fn decide(&self, capability: &CapabilityId, permission: &Permission) -> PermissionDecision;
}

/// 按会话缓存授权决定的权限策略。
#[derive(Clone)]
pub struct PermissionPolicy {
    granted: Arc<Mutex<BTreeSet<Permission>>>,
    prompter: Option<Arc<dyn PermissionPrompter>>,
    unattended: bool,
}

impl PermissionPolicy {
    /// 无人值守：不提示，未预授权一律拒绝。
    #[must_use]
    pub fn unattended() -> Self {
        Self {
            granted: Arc::new(Mutex::new(BTreeSet::new())),
            prompter: None,
            unattended: true,
        }
    }

    /// 交互式：首次需要权限时提示，`AllowSession` 按会话缓存。
    #[must_use]
    pub fn prompting(prompter: Arc<dyn PermissionPrompter>) -> Self {
        Self {
            granted: Arc::new(Mutex::new(BTreeSet::new())),
            prompter: Some(prompter),
            unattended: false,
        }
    }

    /// 预授权一组权限（来自配置/CLI）。
    #[must_use]
    pub fn preauthorize(self, permissions: impl IntoIterator<Item = Permission>) -> Self {
        self.granted
            .lock()
            .expect("permission mutex poisoned")
            .extend(permissions);
        self
    }

    /// 该权限是否已授予。
    #[must_use]
    pub fn is_granted(&self, permission: &Permission) -> bool {
        self.granted
            .lock()
            .expect("permission mutex poisoned")
            .contains(permission)
    }

    /// 检查某能力所需的全部权限；任一项被拒绝即 [`PluginError::PermissionDenied`]。
    pub fn check(
        &self,
        capability: &CapabilityId,
        permissions: &[Permission],
    ) -> Result<(), PluginError> {
        for permission in permissions {
            if self.is_granted(permission) {
                continue;
            }
            let allowed = match &self.prompter {
                Some(prompter) if !self.unattended => match prompter.decide(capability, permission)
                {
                    PermissionDecision::AllowOnce => true,
                    PermissionDecision::AllowSession => {
                        self.granted
                            .lock()
                            .expect("permission mutex poisoned")
                            .insert(permission.clone());
                        true
                    }
                    PermissionDecision::Deny => false,
                },
                _ => false,
            };
            if !allowed {
                return Err(PluginError::PermissionDenied {
                    capability: capability.clone(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn cap() -> CapabilityId {
        CapabilityId::new("sandbox", "exec")
    }

    fn net() -> Permission {
        Permission::new("net")
    }

    struct CountingPrompter {
        decision: PermissionDecision,
        calls: AtomicUsize,
    }

    impl PermissionPrompter for CountingPrompter {
        fn decide(
            &self,
            _capability: &CapabilityId,
            _permission: &Permission,
        ) -> PermissionDecision {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.decision
        }
    }

    #[test]
    fn unattended_denies_unless_preauthorized() {
        let policy = PermissionPolicy::unattended();
        assert!(matches!(
            policy.check(&cap(), &[net()]),
            Err(PluginError::PermissionDenied { .. })
        ));
        let policy = PermissionPolicy::unattended().preauthorize([net()]);
        assert!(policy.check(&cap(), &[net()]).is_ok());
    }

    #[test]
    fn allow_session_is_cached_and_allow_once_is_not() {
        let prompter = Arc::new(CountingPrompter {
            decision: PermissionDecision::AllowOnce,
            calls: AtomicUsize::new(0),
        });
        let policy = PermissionPolicy::prompting(prompter.clone());
        policy.check(&cap(), &[net()]).unwrap();
        policy.check(&cap(), &[net()]).unwrap();
        assert_eq!(
            prompter.calls.load(Ordering::SeqCst),
            2,
            "AllowOnce is not cached"
        );

        let prompter = Arc::new(CountingPrompter {
            decision: PermissionDecision::AllowSession,
            calls: AtomicUsize::new(0),
        });
        let policy = PermissionPolicy::prompting(prompter.clone());
        policy.check(&cap(), &[net()]).unwrap();
        policy.check(&cap(), &[net()]).unwrap();
        assert_eq!(
            prompter.calls.load(Ordering::SeqCst),
            1,
            "AllowSession is cached"
        );
    }
}
