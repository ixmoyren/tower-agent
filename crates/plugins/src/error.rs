//! 插件体系的错误类型。

use std::fmt;

use semver::{Version, VersionReq};

use crate::model::{CapabilityId, PluginId, PluginKind};

/// 注册 / 解析 / 安装阶段的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginError {
    /// 清单非法。
    Manifest {
        /// 插件名。
        plugin: String,
        /// 具体问题。
        message: String,
    },
    /// 同名静态插件出现多份（静态插件不支持多版本）。
    DuplicatePlugin(Box<PluginId>),
    /// 静态插件依赖了动态 / JIT 插件提供的能力。
    KindViolation {
        /// 依赖方（静态）插件。
        plugin: Box<PluginId>,
        /// 被依赖的能力。
        capability: CapabilityId,
        /// 提供者的形态。
        provider_kind: PluginKind,
    },
    /// 必需能力在注册表里找不到任何提供者。
    MissingCapability {
        /// 依赖方。
        consumer: Box<PluginId>,
        /// 缺失的能力。
        capability: CapabilityId,
        /// 期望的版本要求。
        req: VersionReq,
    },
    /// 有提供者，但没有任何版本能同时满足所有消费者的要求。
    VersionConflict {
        /// 冲突的能力。
        capability: CapabilityId,
        /// 参与冲突的版本要求。
        requirements: Vec<String>,
    },
    /// 依赖图出现环（含完整路径）。
    Cycle(Vec<PluginId>),
    /// 能力级覆盖指向了不存在的实现。
    OverrideTargetMissing {
        /// 被覆盖的能力。
        capability: CapabilityId,
        /// 覆盖指定的插件名。
        provider: String,
    },
    /// 插件清单要求的宿主 ABI 与当前宿主不兼容（major 不一致）。
    AbiMismatch {
        /// 插件。
        plugin: Box<PluginId>,
        /// 宿主 ABI。
        expected: Box<Version>,
        /// 插件声明的 ABI。
        found: Box<Version>,
    },
    /// 插件安装失败。
    Install {
        /// 插件。
        plugin: String,
        /// 具体问题。
        message: String,
    },
    /// 动态插件的加载/入口/初始化失败（符号缺失、ABI 不符等）。
    Load {
        /// 插件。
        plugin: String,
        /// 具体问题。
        message: String,
    },
    /// 卸载时仍有 in-flight 调用（引用计数不为零）。
    UnloadBusy {
        /// 插件。
        plugin: String,
        /// 当前 in-flight 调用数。
        inflight: usize,
    },
    /// 能力调用被权限策略拒绝。
    PermissionDenied {
        /// 被拒绝的能力。
        capability: CapabilityId,
    },
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest { plugin, message } => {
                write!(f, "plugin `{plugin}` manifest is invalid: {message}")
            }
            Self::DuplicatePlugin(id) => write!(
                f,
                "duplicate static plugin `{id}`: static plugins do not support multiple versions"
            ),
            Self::KindViolation {
                plugin,
                capability,
                provider_kind,
            } => write!(
                f,
                "static plugin `{plugin}` cannot depend on `{capability}` provided by a {provider_kind} plugin"
            ),
            Self::MissingCapability {
                consumer,
                capability,
                req,
            } => write!(
                f,
                "plugin `{consumer}` requires `{capability} {req}`, but no provider is registered"
            ),
            Self::VersionConflict {
                capability,
                requirements,
            } => write!(
                f,
                "no version of `{capability}` satisfies all requirements: {}",
                requirements.join(", ")
            ),
            Self::Cycle(path) => {
                let path: Vec<String> = path.iter().map(ToString::to_string).collect();
                write!(f, "circular plugin dependency: {}", path.join(" -> "))
            }
            Self::OverrideTargetMissing {
                capability,
                provider,
            } => write!(
                f,
                "capability override for `{capability}` points at unknown provider `{provider}`"
            ),
            Self::AbiMismatch {
                plugin,
                expected,
                found,
            } => write!(
                f,
                "plugin `{plugin}` targets host ABI {found}, but host is {expected}"
            ),
            Self::Install { plugin, message } => {
                write!(f, "plugin `{plugin}` failed to install: {message}")
            }
            Self::Load { plugin, message } => {
                write!(f, "plugin `{plugin}` failed to load: {message}")
            }
            Self::UnloadBusy { plugin, inflight } => write!(
                f,
                "plugin `{plugin}` cannot be unloaded: {inflight} in-flight call(s) remain"
            ),
            Self::PermissionDenied { capability } => {
                write!(f, "permission denied for capability `{capability}`")
            }
        }
    }
}

impl std::error::Error for PluginError {}
