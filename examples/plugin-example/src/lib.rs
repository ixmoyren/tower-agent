//! 示例动态插件（`cdylib`）：声明 `tool.echo` 能力，演示 rkyv 归档载荷 + 宿主回调。
//!
//! 构建：`cargo build --manifest-path examples/plugin-example/Cargo.toml`
//! 产物：`target/debug/libtpa_plugin_example.so`（Linux）。

use std::{
    cell::RefCell,
    sync::atomic::{AtomicPtr, Ordering},
};

use stabby::{string::String, vec::Vec as AbiVec};
use tower_agent_plugin_sdk::{
    AbiBuffer, AbiCapability, AbiHostCall, AbiKind, AbiManifest, AbiNode, AbiToolResult, AbiValue,
    AbiVersion, ArchivedAbiValue, HostVTable,
};

/// 宿主回调表（`init` 时拿到；空表示宿主不允许回调）。
static HOST: AtomicPtr<HostVTable> = AtomicPtr::new(std::ptr::null_mut());

thread_local! {
    /// 返回缓冲：由插件持有到下一次调用。
    static REPLY: RefCell<tower_agent_plugin_sdk::rkyv::util::AlignedVec> =
        RefCell::new(tower_agent_plugin_sdk::rkyv::util::AlignedVec::new());
}

extern "C" fn example_manifest() -> AbiManifest {
    let provides: AbiVec<AbiCapability> = [AbiCapability {
        namespace: String::from("tool"),
        name: String::from("echo"),
        version: AbiVersion {
            major: 1,
            minor: 0,
            patch: 0,
        },
    }]
    .into_iter()
    .collect();
    AbiManifest {
        name: String::from("example"),
        version: AbiVersion {
            major: 1,
            minor: 0,
            patch: 0,
        },
        sdk_abi: AbiVersion::host(),
        provides,
    }
}

extern "C" fn example_init(host: *const HostVTable) -> bool {
    HOST.store(host as *mut HostVTable, Ordering::SeqCst);
    // 演示宿主回调：往宿主日志写一行。
    host_call("log", text_value("example plugin initialized"));
    true
}

extern "C" fn example_call(request_ptr: *const u8, request_len: usize) -> AbiBuffer {
    let bytes = unsafe { std::slice::from_raw_parts(request_ptr, request_len) };
    let call = match tower_agent_plugin_sdk::rkyv::access::<
        tower_agent_plugin_sdk::ArchivedAbiToolCall,
        tower_agent_plugin_sdk::rkyv::rancor::Error,
    >(bytes)
    {
        Ok(call) => call,
        Err(_) => return AbiBuffer::empty(),
    };
    // 零反序列化读取归档 arena：取 args 映射里的 "text" 值。
    let text = map_text(&call.args, "text").unwrap_or_default();
    let result = AbiToolResult {
        ok: true,
        content: format!("echo: {text}"),
        details: None,
    };
    respond(&result)
}

extern "C" fn example_shutdown() {}

/// 在归档 arena 里取映射键 `key` 的文本值（不反序列化）。
fn map_text(value: &ArchivedAbiValue, key: &str) -> Option<std::string::String> {
    let root = value.nodes.get(0)?;
    for pair in root.children.as_slice().chunks(2) {
        let [key_index, value_index] = pair else {
            continue;
        };
        let key_node = value.nodes.get(key_index.to_native() as usize)?;
        if key_node.text.as_str() == key {
            let value_node = value.nodes.get(value_index.to_native() as usize)?;
            return Some(value_node.text.as_str().to_string());
        }
    }
    None
}

/// 构造一个只含文本的 `AbiValue`。
fn text_value(text: &str) -> AbiValue {
    AbiValue {
        nodes: vec![AbiNode {
            kind: AbiKind::Text,
            flag: false,
            int: 0,
            float: 0.0,
            text: text.to_string(),
            children: std::vec::Vec::new(),
        }],
    }
}

/// 调一次宿主函数（归档载荷进 / 归档载荷出），返回结构化结果。
fn host_call(function: &str, args: AbiValue) -> Option<AbiValue> {
    let host = HOST.load(Ordering::SeqCst);
    if host.is_null() {
        return None;
    }
    let call = AbiHostCall {
        function: function.to_string(),
        args,
    };
    let request = tower_agent_plugin_sdk::rkyv::to_bytes::<
        tower_agent_plugin_sdk::rkyv::rancor::Error,
    >(&call)
    .ok()?;
    let buffer = unsafe { ((*host).call)((*host).ctx, request.as_ptr(), request.len()) };
    if buffer.ptr.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len) };
    let reply = tower_agent_plugin_sdk::rkyv::access::<
        tower_agent_plugin_sdk::ArchivedAbiHostResult,
        tower_agent_plugin_sdk::rkyv::rancor::Error,
    >(bytes)
    .ok()?;
    match reply {
        tower_agent_plugin_sdk::ArchivedAbiHostResult::Ok(value) => {
            Some(AbiValue::from_archived(value))
        }
        tower_agent_plugin_sdk::ArchivedAbiHostResult::Err(_) => None,
    }
}

/// 把结果归档进线程本地缓冲并返回指针。
fn respond(result: &AbiToolResult) -> AbiBuffer {
    let Ok(out) = tower_agent_plugin_sdk::rkyv::to_bytes::<
        tower_agent_plugin_sdk::rkyv::rancor::Error,
    >(result) else {
        return AbiBuffer::empty();
    };
    REPLY.with(|cell| {
        *cell.borrow_mut() = out;
        let bytes = cell.borrow();
        AbiBuffer {
            ptr: bytes.as_ptr(),
            len: bytes.len(),
        }
    })
}

tower_agent_plugin_sdk::declare_dynamic_plugin!(
    example_manifest,
    example_init,
    example_call,
    example_shutdown
);
