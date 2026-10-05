//! 插件 / 扩展体系宿主核心（M1：静态插件 + 注册表 + 解析）。
//!
//! 设计见 `docs/plugins.md`。这里的职责边界：
//!
//! - [`PluginManifest`] / [`PluginKind`] / [`CapabilityId`] 描述插件与能力；
//! - [`PluginRegistry`] 负责注册、同名覆盖（`静态 < 动态 < JIT`）、能力级覆盖、
//!   版本收敛（静态单实例；动态/JIT 按 major 多版本）、依赖图与环检测；
//! - [`StaticPlugin`] 是 M1 唯一有载荷的形态：编译进宿主，`install` 时把工具登记到
//!   [`PluginContext`]；
//! - [`default_registry`] / [`resolve_default`] / [`default_tools`]：内置工具插件集合，
//!   每个内置工具（`tools` crate 里的 tower `Layer`）都是独立插件，`cli` 不再自己构造。

mod builtin;
mod error;
mod model;
mod registry;
mod static_plugin;

pub use builtin::{DEFAULT_TOOL_NAMES, default_registry, default_tools, resolve_default};
pub use error::PluginError;
pub use model::{
    CapabilityId, CapabilityOverride, Dependency, Permission, PluginId, PluginKind, PluginManifest,
    ProvidedCapability,
};
pub use registry::{Binding, PluginRegistry, Resolution, ResolvedPlugin, ShadowedPlugin};
pub use static_plugin::{PluginContext, StaticPlugin};

/// 宿主 SDK/ABI 版本：静态插件清单默认使用它；major 不兼容即拒绝加载。
pub const HOST_ABI: semver::Version = semver::Version::new(0, 1, 0);
