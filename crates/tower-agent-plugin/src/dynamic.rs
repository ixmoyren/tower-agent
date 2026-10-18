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
            protected: false,
        }
    }
}

/// 插件入口 vtable：全部是 `extern "C"` 函数指针，因此是 ABI 稳定的（`#[repr(C)]`）。
#[repr(C)]
pub struct PluginVTable {
    /// 返回插件清单。
    pub manifest: extern "C" fn() -> AbiManifest,
    /// 初始化；宿主传入宿主回调表（`null` = 不允许回调）。返回 `false` 表示加载失败。
    pub init: extern "C" fn(host: *const HostVTable) -> bool,
    /// 中介调用：rkyv 归档载荷进、归档载荷出。
    pub call: extern "C" fn(request_ptr: *const u8, request_len: usize) -> AbiBuffer,
    /// 关闭。
    pub shutdown: extern "C" fn(),
}

/// 插件入口符号名。
pub const ENTRY_SYMBOL: &[u8] = b"__tpa_plugin_entry";

/// 归档载荷缓冲：`ptr`/`len` 指向一块由持有方管理、16 字节对齐的内存，有效期到下一次调用。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AbiBuffer {
    /// 数据指针（16 字节对齐）。
    pub ptr: *const u8,
    /// 数据长度。
    pub len: usize,
}

impl AbiBuffer {
    /// 空缓冲（错误/无结果的约定表示）。
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            ptr: std::ptr::null(),
            len: 0,
        }
    }
}

/// 宿主 ↔ 插件的 rkyv 归档载荷（宿主与插件必须用同一 rkyv 版本 + 同一份定义编译）。
///
/// 值用**扁平 arena** 编码：`nodes[0]` 是根，容器用 `children` 里的 `u32` 索引引用子节点。
/// 这样避免了 rkyv 对递归枚举（`Vec<Self>`）的 derive 限制，同时仍是零反序列化访问。
pub mod abi {
    use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};

    /// 节点种类。
    #[derive(Archive, RkyvSerialize, RkyvDeserialize, Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AbiKind {
        /// null。
        Null,
        /// 布尔。
        Bool,
        /// 整数。
        Int,
        /// 浮点。
        Float,
        /// 字符串。
        Text,
        /// 列表。
        List,
        /// 映射（`children` 交替为 key/value 索引，key 是 `Text`）。
        Map,
    }

    /// arena 里一个节点。
    #[derive(Archive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq)]
    pub struct AbiNode {
        /// 种类。
        pub kind: AbiKind,
        /// 布尔值。
        pub flag: bool,
        /// 整数值。
        pub int: i64,
        /// 浮点值。
        pub float: f64,
        /// 文本值 / 映射键。
        pub text: String,
        /// 子节点索引。
        pub children: Vec<u32>,
    }

    /// 结构化值（JSON 语义），替代把参数塞进字符串。
    #[derive(Archive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq)]
    pub struct AbiValue {
        /// 节点表；0 号是根。
        pub nodes: Vec<AbiNode>,
    }

    impl AbiValue {
        /// 空的 `null` 值。
        #[must_use]
        pub fn null() -> Self {
            let mut nodes = Vec::new();
            let _ = push_json(&serde_json::Value::Null, &mut nodes);
            Self { nodes }
        }

        /// 从 JSON 构造（递归展开成 arena）。
        #[must_use]
        pub fn from_json(value: &serde_json::Value) -> Self {
            let mut nodes = Vec::new();
            let _ = push_json(value, &mut nodes);
            Self { nodes }
        }

        /// 还原成 JSON。
        #[must_use]
        pub fn to_json(&self) -> serde_json::Value {
            build_json(&self.nodes, 0)
        }

        /// 从归档视图还原成拥有值（宿主回调边界用）。
        #[must_use]
        pub fn from_archived(archived: &ArchivedAbiValue) -> Self {
            rkyv::deserialize::<Self, rkyv::rancor::Error>(archived)
                .unwrap_or_else(|_| Self::null())
        }
    }

    fn push_json(value: &serde_json::Value, nodes: &mut Vec<AbiNode>) -> u32 {
        let index = nodes.len() as u32;
        nodes.push(AbiNode {
            kind: AbiKind::Null,
            flag: false,
            int: 0,
            float: 0.0,
            text: String::new(),
            children: Vec::new(),
        });
        let mut children = Vec::new();
        let (kind, flag, int, float, text) = match value {
            serde_json::Value::Null => (AbiKind::Null, false, 0, 0.0, String::new()),
            serde_json::Value::Bool(flag) => (AbiKind::Bool, *flag, 0, 0.0, String::new()),
            serde_json::Value::Number(number) => match number.as_i64() {
                Some(int) => (AbiKind::Int, false, int, 0.0, String::new()),
                None => (
                    AbiKind::Float,
                    false,
                    0,
                    number.as_f64().unwrap_or_default(),
                    String::new(),
                ),
            },
            serde_json::Value::String(text) => (AbiKind::Text, false, 0, 0.0, text.clone()),
            serde_json::Value::Array(items) => {
                for item in items {
                    children.push(push_json(item, nodes));
                }
                (AbiKind::List, false, 0, 0.0, String::new())
            }
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    children.push(push_json(&serde_json::Value::String(key.clone()), nodes));
                    children.push(push_json(value, nodes));
                }
                (AbiKind::Map, false, 0, 0.0, String::new())
            }
        };
        nodes[index as usize] = AbiNode {
            kind,
            flag,
            int,
            float,
            text,
            children,
        };
        index
    }

    fn build_json(nodes: &[AbiNode], index: u32) -> serde_json::Value {
        let Some(node) = nodes.get(index as usize) else {
            return serde_json::Value::Null;
        };
        match node.kind {
            AbiKind::Null => serde_json::Value::Null,
            AbiKind::Bool => serde_json::Value::Bool(node.flag),
            AbiKind::Int => serde_json::Value::from(node.int),
            AbiKind::Float => serde_json::Value::from(node.float),
            AbiKind::Text => serde_json::Value::String(node.text.clone()),
            AbiKind::List => serde_json::Value::Array(
                node.children
                    .iter()
                    .map(|child| build_json(nodes, *child))
                    .collect(),
            ),
            AbiKind::Map => {
                let mut map = serde_json::Map::new();
                for pair in node.children.chunks(2) {
                    if let [key, value] = pair {
                        let key = nodes
                            .get(*key as usize)
                            .map(|node| node.text.clone())
                            .unwrap_or_default();
                        map.insert(key, build_json(nodes, *value));
                    }
                }
                serde_json::Value::Object(map)
            }
        }
    }

    /// 一次工具调用请求（宿主 -> 插件）。
    #[derive(Archive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq)]
    pub struct AbiToolCall {
        /// 调用 id。
        pub id: String,
        /// 工具名。
        pub tool: String,
        /// 结构化参数（通常是 `Map`）。
        pub args: AbiValue,
    }

    /// 一次工具调用结果（插件 -> 宿主）。
    #[derive(Archive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq)]
    pub struct AbiToolResult {
        /// 是否错误。
        pub ok: bool,
        /// 模型可见文本。
        pub content: String,
        /// 结构化 details。
        pub details: Option<AbiValue>,
    }

    /// 一次宿主函数调用（插件 -> 宿主回调）。
    #[derive(Archive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq)]
    pub struct AbiHostCall {
        /// 宿主函数名（`tool_call` / `log` / `state_get` …）。
        pub function: String,
        /// 结构化参数。
        pub args: AbiValue,
    }

    /// 宿主函数返回。
    #[derive(Archive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq)]
    pub enum AbiHostResult {
        /// 成功值。
        Ok(AbiValue),
        /// 失败原因。
        Err(String),
    }
}

pub use abi::{
    AbiHostCall, AbiHostResult, AbiKind, AbiNode, AbiToolCall, AbiToolResult, AbiValue,
    ArchivedAbiHostCall, ArchivedAbiHostResult, ArchivedAbiKind, ArchivedAbiNode,
    ArchivedAbiToolCall, ArchivedAbiToolResult, ArchivedAbiValue,
};

/// 宿主回调表：插件通过它调用宿主函数（`ctx` 由宿主持有，插件不得解引用）。
#[repr(C)]
pub struct HostVTable {
    /// 宿主上下文。
    pub ctx: *const core::ffi::c_void,
    /// 调一个宿主函数：请求归档载荷进，结果归档载荷出（缓冲由宿主持有到下一次调用）。
    pub call: extern "C" fn(
        ctx: *const core::ffi::c_void,
        request_ptr: *const u8,
        request_len: usize,
    ) -> AbiBuffer,
}

/// 宿主回调的实现者：把一次宿主函数调用映射成结构化结果。
pub trait HostDispatch: Send + Sync + 'static {
    /// 调用一个宿主函数；`args` 是**归档视图**（零反序列化读取）。
    fn call(
        &self,
        function: &str,
        args: &<AbiValue as rkyv::Archive>::Archived,
    ) -> Result<AbiValue, String>;
}

thread_local! {
    /// 宿主回调的返回缓冲（每线程一份，持有到下一次回调）。
    static HOST_REPLY: std::cell::RefCell<rkyv::util::AlignedVec> =
        std::cell::RefCell::new(rkyv::util::AlignedVec::new());
}

extern "C" fn host_call_dispatch(
    ctx: *const core::ffi::c_void,
    request_ptr: *const u8,
    request_len: usize,
) -> AbiBuffer {
    if ctx.is_null() || request_ptr.is_null() {
        return AbiBuffer::empty();
    }
    // SAFETY: ctx 由 `LoadedPlugin` 持有，指向 `Box<Arc<dyn HostDispatch>>`。
    let host = unsafe { &*(ctx as *const std::sync::Arc<dyn HostDispatch>) };
    let bytes = unsafe { std::slice::from_raw_parts(request_ptr, request_len) };
    let reply = match rkyv::access::<<AbiHostCall as rkyv::Archive>::Archived, rkyv::rancor::Error>(
        bytes,
    ) {
        Ok(call) => match host.call(&call.function, &call.args) {
            Ok(value) => AbiHostResult::Ok(value),
            Err(message) => AbiHostResult::Err(message),
        },
        Err(error) => AbiHostResult::Err(format!("invalid host-call payload: {error}")),
    };
    let Ok(out) = rkyv::to_bytes::<rkyv::rancor::Error>(&reply) else {
        return AbiBuffer::empty();
    };
    HOST_REPLY.with(|cell| {
        *cell.borrow_mut() = out;
        let bytes = cell.borrow();
        AbiBuffer {
            ptr: bytes.as_ptr(),
            len: bytes.len(),
        }
    })
}

extern "C" fn host_call_unavailable(
    _ctx: *const core::ffi::c_void,
    _request_ptr: *const u8,
    _request_len: usize,
) -> AbiBuffer {
    AbiBuffer::empty()
}

/// 已加载的动态插件：持有 `libloading::Library`、vtable、清单与生命周期。
pub struct LoadedPlugin {
    library: libloading::Library,
    vtable: PluginVTable,
    manifest: PluginManifest,
    lifecycle: Lifecycle,
    /// 宿主回调表（`Box` 保证地址稳定，插件持有其指针）。
    host_vtable: Box<HostVTable>,
    /// 回调上下文：`Box<Arc<dyn HostDispatch>>` 的裸指针，析构时释放。
    host_ctx: *mut std::sync::Arc<dyn HostDispatch>,
}

impl LoadedPlugin {
    /// 加载一个 `cdylib`：取入口符号 → 校验 ABI major → 传宿主回调表 `init`。
    ///
    /// # Safety
    ///
    /// 调用方必须保证该 `.so` / `.dll` 是用本 SDK 构建的、受信任的动态插件；加载并执行
    /// 外部代码本质上是不安全的。
    pub unsafe fn load(
        path: impl AsRef<OsStr>,
        host: impl FnOnce(&PluginManifest) -> Option<std::sync::Arc<dyn HostDispatch>>,
    ) -> Result<Self, PluginError> {
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
        // 先拿到清单（`init` 之前），再由调用方按插件名构造宿主回调表。
        let (host_ctx, host_vtable) = match host(&manifest) {
            Some(host) => {
                let ctx = Box::into_raw(Box::new(host));
                (
                    ctx,
                    Box::new(HostVTable {
                        ctx: ctx as *const core::ffi::c_void,
                        call: host_call_dispatch,
                    }),
                )
            }
            None => (
                std::ptr::null_mut(),
                Box::new(HostVTable {
                    ctx: std::ptr::null(),
                    call: host_call_unavailable,
                }),
            ),
        };
        if !(vtable.init)(&*host_vtable as *const HostVTable) {
            if !host_ctx.is_null() {
                // SAFETY: 刚由 `Box::into_raw` 创建，尚未交给任何结构。
                unsafe { drop(Box::from_raw(host_ctx)) };
            }
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
            host_vtable,
            host_ctx,
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

    /// 中介调用插件：归档载荷进、归档载荷出（带 in-flight 记账）。
    pub fn call_archived(&self, request: &[u8]) -> Result<rkyv::util::AlignedVec, PluginError> {
        let _guard = self
            .lifecycle
            .begin_call()
            .ok_or_else(|| PluginError::Load {
                plugin: self.manifest.name.clone(),
                message: "plugin is not started".into(),
            })?;
        let buffer = (self.vtable.call)(request.as_ptr(), request.len());
        if buffer.ptr.is_null() {
            return Err(PluginError::Load {
                plugin: self.manifest.name.clone(),
                message: "plugin returned an empty buffer".into(),
            });
        }
        // SAFETY: 插件保证缓冲在本次调用返回后仍可读到下一次调用。
        let bytes = unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len) };
        let mut out = rkyv::util::AlignedVec::with_capacity(bytes.len());
        out.extend_from_slice(bytes);
        Ok(out)
    }

    /// 停止：卸载前必须调用，成功后才允许 [`LoadedPlugin::unload`]。
    pub fn stop(&self) -> Result<(), PluginError> {
        self.lifecycle.stop()
    }

    /// 卸载：要求已 `stop` 且 in-flight 归零，随后调用插件的 `shutdown` 并释放库。
    pub fn unload(self) -> Result<(), PluginError> {
        self.lifecycle.unload()?;
        (self.vtable.shutdown)();
        Ok(())
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        // 释放传给插件的宿主回调上下文（`host_vtable` 随结构一起释放）。
        let _ = &self.host_vtable;
        let _ = &self.library;
        if !self.host_ctx.is_null() {
            // SAFETY: 由 `load` 用 `Box::into_raw` 创建，且只在这里释放一次。
            unsafe { drop(Box::from_raw(self.host_ctx)) };
            self.host_ctx = std::ptr::null_mut();
        }
    }
}

// SAFETY: `host_ctx` 指向 `Box<Arc<dyn HostDispatch>>`；`Arc<dyn HostDispatch>` 是
// `Send + Sync`，指针本身只在插件回调里解引用，且 `LoadedPlugin` 拥有它的生命周期。
#[allow(unsafe_code)]
unsafe impl Send for LoadedPlugin {}
#[allow(unsafe_code)]
unsafe impl Sync for LoadedPlugin {}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// 一个回显宿主函数名与参数的假派发器。
    struct Echo;

    impl HostDispatch for Echo {
        fn call(&self, function: &str, args: &ArchivedAbiValue) -> Result<AbiValue, String> {
            let json = AbiValue::from_archived(args).to_json();
            Ok(AbiValue::from_json(
                &serde_json::json!({ "function": function, "args": json }),
            ))
        }
    }

    #[test]
    fn host_callback_dispatches_and_replies_via_archive() {
        let host: Arc<dyn HostDispatch> = Arc::new(Echo);
        let ctx = Box::into_raw(Box::new(host)) as *const core::ffi::c_void;
        let call = AbiHostCall {
            function: "log".into(),
            args: AbiValue::from_json(&serde_json::json!("hi")),
        };
        let request = rkyv::to_bytes::<rkyv::rancor::Error>(&call).expect("archive");
        let buffer = host_call_dispatch(ctx, request.as_ptr(), request.len());
        assert!(!buffer.ptr.is_null());
        let bytes = unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len) };
        let reply =
            rkyv::access::<ArchivedAbiHostResult, rkyv::rancor::Error>(bytes).expect("access");
        match reply {
            ArchivedAbiHostResult::Ok(value) => {
                assert_eq!(
                    AbiValue::from_archived(value).to_json(),
                    serde_json::json!({ "function": "log", "args": "hi" })
                );
            }
            ArchivedAbiHostResult::Err(message) => panic!("unexpected error: {message}"),
        }
        // SAFETY: 由本测试创建，且只释放一次。
        unsafe { drop(Box::from_raw(ctx as *mut Arc<dyn HostDispatch>)) };
    }

    #[test]
    fn abi_value_round_trips_nested_json() {
        let json = serde_json::json!({
            "a": 1,
            "b": [true, null, { "c": "x" }],
        });
        let value = AbiValue::from_json(&json);
        assert_eq!(value.to_json(), json);
        let archived = rkyv::to_bytes::<rkyv::rancor::Error>(&value).expect("archive");
        // 零反序列化访问：直接读归档节点。
        let view =
            rkyv::access::<ArchivedAbiValue, rkyv::rancor::Error>(&archived).expect("access");
        // root map + ("a",1) + ("b", list) + list 的 [true,null,{...}] + ("c","x")
        assert_eq!(view.nodes.len(), 10);
    }
}
