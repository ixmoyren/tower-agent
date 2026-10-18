//! 插件模型：形态、标识、能力、依赖、清单与覆盖。

use std::fmt;

use semver::{Version, VersionReq};

/// 插件形态。枚举顺序即覆盖优先级：`Static < Dynamic < Jit`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PluginKind {
    /// 静态插件：编译进宿主，构建在 tower `Layer` 之上，不支持多版本。
    Static,
    /// 动态插件：stabby ABI 的 `cdylib`（`.so` / `.dll`），可加载可卸载，按 major 多版本。
    Dynamic,
    /// JIT / 热重载插件：monty（Python 子集）沙箱，可热加载，按 major 多版本。
    Jit,
}

impl PluginKind {
    /// 覆盖优先级，数值越大越优先。
    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::Static => 0,
            Self::Dynamic => 1,
            Self::Jit => 2,
        }
    }
}

impl fmt::Display for PluginKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Static => "static",
            Self::Dynamic => "dynamic",
            Self::Jit => "jit",
        })
    }
}

/// 插件标识：名字 + 语义版本。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginId {
    /// 插件名（同名插件按形态优先级覆盖）。
    pub name: String,
    /// 插件版本。
    pub version: Version,
}

impl PluginId {
    /// 构造。
    #[must_use]
    pub fn new(name: impl Into<String>, version: Version) -> Self {
        Self {
            name: name.into(),
            version,
        }
    }

    /// 主版本号。
    #[must_use]
    pub fn major(&self) -> u64 {
        self.version.major
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

/// 能力标识：`namespace.name`。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapabilityId {
    /// 命名空间（`tool` / `provider` / `sandbox` / `memory` / `persist` / `code` …）。
    pub namespace: String,
    /// 能力名。
    pub name: String,
}

impl CapabilityId {
    /// 构造。
    #[must_use]
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
        }
    }

    /// 解析 `namespace.name`。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let (namespace, name) = value.split_once('.')?;
        (!namespace.is_empty() && !name.is_empty()).then(|| Self::new(namespace, name))
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.namespace, self.name)
    }
}

/// 插件提供的一项能力及其版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvidedCapability {
    /// 能力标识。
    pub id: CapabilityId,
    /// 该能力实现的版本。
    pub version: Version,
}

impl ProvidedCapability {
    /// 构造。
    #[must_use]
    pub fn new(id: CapabilityId, version: Version) -> Self {
        Self { id, version }
    }
}

/// 插件对某项能力的依赖。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// 需要的能力。
    pub id: CapabilityId,
    /// 版本要求（`^` 语义）。
    pub req: VersionReq,
    /// 可选依赖：缺失时不阻塞解析。
    pub optional: bool,
}

impl Dependency {
    /// 必需依赖。
    #[must_use]
    pub fn required(id: CapabilityId, req: VersionReq) -> Self {
        Self {
            id,
            req,
            optional: false,
        }
    }

    /// 可选依赖。
    #[must_use]
    pub fn optional(id: CapabilityId, req: VersionReq) -> Self {
        Self {
            id,
            req,
            optional: true,
        }
    }
}

/// 权限声明（`net` / `fs.write` / `process.spawn` …）。授权在运行时按需提示。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Permission(String);

impl Permission {
    /// 构造。
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// 权限名。
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 能力级覆盖：把某个能力显式绑定到指定实现（覆盖插件级优先级）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityOverride {
    /// 被覆盖的能力。
    pub capability: CapabilityId,
    /// 目标插件名。
    pub provider: String,
    /// 目标版本（`None` 表示不限定）。
    pub version: Option<Version>,
    /// 目标形态（`None` 表示不限定）。
    pub kind: Option<PluginKind>,
}

impl CapabilityOverride {
    /// 按插件名绑定能力。
    #[must_use]
    pub fn to(provider: impl Into<String>, capability: CapabilityId) -> Self {
        Self {
            capability,
            provider: provider.into(),
            version: None,
            kind: None,
        }
    }

    /// 限定目标版本。
    #[must_use]
    pub fn with_version(mut self, version: Version) -> Self {
        self.version = Some(version);
        self
    }

    /// 限定目标形态。
    #[must_use]
    pub fn with_kind(mut self, kind: PluginKind) -> Self {
        self.kind = Some(kind);
        self
    }
}

/// 插件清单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    /// 插件名。
    pub name: String,
    /// 插件版本。
    pub version: Version,
    /// 形态。
    pub kind: PluginKind,
    /// 宿主 SDK/ABI 版本要求。
    pub sdk_abi: Version,
    /// 提供的能力。
    pub provides: Vec<ProvidedCapability>,
    /// 依赖的能力。
    pub requires: Vec<Dependency>,
    /// 权限声明。
    pub permissions: Vec<Permission>,
    /// 是否受保护：受保护的插件**永不被遮蔽**（同名的高优先级插件也只能进入 shadow）。
    ///
    /// 用于 JIT 运行时（agent-monty）这类「插件体系的地基」。
    pub protected: bool,
}

impl PluginManifest {
    /// 构造清单（`sdk_abi` 默认取 [`crate::HOST_ABI`]）。
    #[must_use]
    pub fn new(name: impl Into<String>, version: Version, kind: PluginKind) -> Self {
        Self {
            name: name.into(),
            version,
            kind,
            sdk_abi: crate::HOST_ABI,
            provides: Vec::new(),
            requires: Vec::new(),
            permissions: Vec::new(),
            protected: false,
        }
    }

    /// 插件标识。
    #[must_use]
    pub fn id(&self) -> PluginId {
        PluginId::new(self.name.clone(), self.version.clone())
    }

    /// 声明提供一项能力。
    #[must_use]
    pub fn providing(mut self, capability: CapabilityId, version: Version) -> Self {
        self.provides
            .push(ProvidedCapability::new(capability, version));
        self
    }

    /// 声明依赖一项能力。
    #[must_use]
    pub fn requiring(mut self, capability: CapabilityId, req: VersionReq) -> Self {
        self.requires.push(Dependency::required(capability, req));
        self
    }

    /// 声明权限。
    #[must_use]
    pub fn with_permission(mut self, permission: impl Into<String>) -> Self {
        self.permissions.push(Permission::new(permission));
        self
    }

    /// 标记为受保护（永不被遮蔽）。
    #[must_use]
    pub fn protected(mut self) -> Self {
        self.protected = true;
        self
    }

    /// 该清单是否提供某能力。
    #[must_use]
    pub fn provides_capability(&self, capability: &CapabilityId) -> Option<&Version> {
        self.provides
            .iter()
            .find(|p| &p.id == capability)
            .map(|p| &p.version)
    }
}
