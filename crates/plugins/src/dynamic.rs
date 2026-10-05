//! 动态插件（stabby `cdylib`）加载器（`docs/plugins.md` 第 4.2 / 7 节）。
//!
//! 插件以 `cdylib` 形式导出 `__tpa_plugin_entry`，返回一个 [`PluginVTable`]：清单 +
//! 初始化 / 调用 / 关闭函数指针。宿主用 `libloading` 加载，做 SDK ABI major 校验后
//! 通过 [`LoadedPlugin`] 调用与卸载；调用走 [`Lifecycle`] 记账，卸载前必须先 `stop`
//! 且 in-flight 归零，否则 [`PluginError::UnloadBusy`]。

use std::ffi::OsStr;

use semver::Version;
use stabby::{string::String as AbiString, vec::Vec as AbiVec};

use crate::{
    CapabilityId, HOST_ABI, Lifecycle, PluginError, PluginKind, PluginManifest, PluginState,
    ProvidedCapability,
};

/// ABI 稳定的语义版本。
#[stabby::stabby]
pub struct AbiVersion {
    /// 主版本号。
    pub major: u64,
    /// 次版本号。
    pub minor: u64,
    /// 修订号。
    pub patch: u64,
}

impl AbiVersion {
    /// 用 [`crate::HOST_ABI`] 填充。
    #[must_use]
    pub fn host() -> Self {
        Self {
            major: HOST_ABI.major,
            minor: HOST_ABI.minor,
            patch: HOST_ABI.patch,
        }
    }

    /// 转成 [`semver::Version`]。
    #[must_use]
    pub fn to_version(&self) -> Version {
        Version::new(self.major, self.minor, self.patch)
    }
}

/// ABI 稳定的一项能力声明。
#[stabby::stabby]
pub struct AbiCapability {
    /// 命名空间。
    pub namespace: AbiString,
    /// 能力名。
    pub name: AbiString,
    /// 能力版本。
    pub version: AbiVersion,
}

/// ABI 稳定的插件清单（动态插件的入口载荷）。
#[stabby::stabby]
pub struct AbiManifest {
    /// 插件名。
    pub name: AbiString,
    /// 插件版本。
    pub version: AbiVersion,
    /// 插件声明的宿主 SDK/ABI 版本。
    pub sdk_abi: AbiVersion,
    /// 提供的能力。
    pub provides: AbiVec<AbiCapability>,
}

impl AbiManifest {
    /// 转成宿主清单（形态固定为 [`PluginKind::Dynamic`]）。
    #[must_use]
    pub fn to_manifest(self) -> PluginManifest {
        let Self {
            name,
            version,
            sdk_abi,
            provides,
        } = self;
        let provides = provides
            .iter()
            .map(|capability| {
                ProvidedCapability::new(
                    CapabilityId::new(
                        std::string::String::from(capability.namespace.clone()),
                        std::string::String::from(capability.name.clone()),
                    ),
                    capability.version.to_version(),
                )
            })
            .collect();
        PluginManifest {
            name: std::string::String::from(name),
            version: version.to_version(),
            kind: PluginKind::Dynamic,
            sdk_abi: sdk_abi.to_version(),
            provides,
            requires: Vec::new(),
            permissions: Vec::new(),
        }
    }
}

/// 插件入口 vtable：全部是 `extern "C"` 函数指针，因此是 ABI 稳定的（`#[repr(C)]`）。
#[repr(C)]
pub struct PluginVTable {
    /// 返回插件清单。
    pub manifest: extern "C" fn() -> AbiManifest,
    /// 初始化；返回 `false` 表示加载失败。
    pub init: extern "C" fn() -> bool,
    /// 中介调用：把输入转成输出。
    pub call: extern "C" fn(AbiString) -> AbiString,
    /// 关闭。
    pub shutdown: extern "C" fn(),
}

/// 插件入口符号名。
pub const ENTRY_SYMBOL: &[u8] = b"__tpa_plugin_entry";

/// 已加载的动态插件：持有 `libloading::Library`、vtable、清单与生命周期。
pub struct LoadedPlugin {
    library: libloading::Library,
    vtable: PluginVTable,
    manifest: PluginManifest,
    lifecycle: Lifecycle,
}

impl LoadedPlugin {
    /// 加载一个 `cdylib`：取入口符号 → 校验 ABI major → `init`。
    ///
    /// # Safety
    ///
    /// 调用方必须保证该 `.so` / `.dll` 是用本 SDK 构建的、受信任的动态插件；加载并执行
    /// 外部代码本质上是不安全的。
    pub unsafe fn load(path: impl AsRef<OsStr>) -> Result<Self, PluginError> {
        let path = path.as_ref();
        let label = path.to_string_lossy().into_owned();
        let library =
            unsafe { libloading::Library::new(path) }.map_err(|error| PluginError::Load {
                plugin: label.clone(),
                message: error.to_string(),
            })?;

        let vtable = {
            let entry = unsafe { library.get::<extern "C" fn() -> PluginVTable>(ENTRY_SYMBOL) }
                .map_err(|error| PluginError::Load {
                    plugin: label.clone(),
                    message: format!("missing `__tpa_plugin_entry`: {error}"),
                })?;
            entry()
        };

        let manifest = (vtable.manifest)().to_manifest();
        manifest.validate()?;
        if manifest.sdk_abi.major != HOST_ABI.major {
            return Err(PluginError::AbiMismatch {
                plugin: Box::new(manifest.id()),
                expected: Box::new(HOST_ABI.clone()),
                found: Box::new(manifest.sdk_abi.clone()),
            });
        }

        let lifecycle = Lifecycle::new(manifest.name.clone());
        for state in [
            PluginState::Loaded,
            PluginState::Validated,
            PluginState::Resolved,
        ] {
            lifecycle.transition(state)?;
        }
        if !(vtable.init)() {
            return Err(PluginError::Load {
                plugin: manifest.name.clone(),
                message: "plugin init returned false".into(),
            });
        }
        lifecycle.transition(PluginState::Started)?;

        Ok(Self {
            library,
            vtable,
            manifest,
            lifecycle,
        })
    }

    /// 插件清单。
    #[must_use]
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// 生命周期句柄。
    #[must_use]
    pub fn lifecycle(&self) -> &Lifecycle {
        &self.lifecycle
    }

    /// 中介调用插件（带 in-flight 记账）。
    pub fn call(&self, input: &str) -> Result<std::string::String, PluginError> {
        let _guard = self
            .lifecycle
            .begin_call()
            .ok_or_else(|| PluginError::Load {
                plugin: self.manifest.name.clone(),
                message: "plugin is not started".into(),
            })?;
        let output = (self.vtable.call)(AbiString::from(input));
        Ok(std::string::String::from(output))
    }

    /// 停止：卸载前必须调用，成功后才允许 [`LoadedPlugin::unload`]。
    pub fn stop(&self) -> Result<(), PluginError> {
        self.lifecycle.stop()
    }

    /// 卸载：要求已 `stop` 且 in-flight 归零，随后调用插件的 `shutdown` 并释放库。
    pub fn unload(self) -> Result<(), PluginError> {
        self.lifecycle.unload()?;
        (self.vtable.shutdown)();
        drop(self.library);
        Ok(())
    }
}
