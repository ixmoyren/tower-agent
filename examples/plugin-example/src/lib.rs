//! 示例动态插件（`cdylib`）：声明一个 `tool.echo` 能力并回显输入。
//!
//! 构建：`cargo build --manifest-path examples/plugin-example/Cargo.toml`
//! 产物：`target/debug/libtpa_plugin_example.so`（Linux）。

use tower_agent_plugin_sdk::{AbiCapability, AbiManifest, AbiVersion};
use stabby::{string::String, vec::Vec};

extern "C" fn example_manifest() -> AbiManifest {
    let provides: Vec<AbiCapability> = [AbiCapability {
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

extern "C" fn example_init() -> bool {
    true
}

extern "C" fn example_call(input: String) -> String {
    String::from(format!("echo: {}", input.as_str()))
}

extern "C" fn example_shutdown() {}

tower_agent_plugin_sdk::declare_dynamic_plugin!(
    example_manifest,
    example_init,
    example_call,
    example_shutdown
);
