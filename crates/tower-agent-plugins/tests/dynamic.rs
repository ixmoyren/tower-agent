//! 动态插件加载的端到端测试（`docs/plugins.md` 第 14 节）。
//!
//! 端到端用例需要先构建示例插件：
//!
//! ```text
//! cargo build --manifest-path examples/plugin-example/Cargo.toml
//! TPA_PLUGIN_EXAMPLE=examples/plugin-example/target/debug/libtpa_plugin_example.so \
//!   cargo test -p plugins --features dynamic --test dynamic
//! ```
//!
//! 未设置 `TPA_PLUGIN_EXAMPLE` 时跳过（不做嵌套 cargo，避免与 `cargo test` 抢 target 锁）。
#![cfg(feature = "dynamic")]
#![allow(unsafe_code)] // 加载外部代码本质上需要 unsafe。

use std::path::PathBuf;

use tower_agent_plugins::{LoadedPlugin, PluginError, PluginState};

fn example_path() -> Option<PathBuf> {
    std::env::var_os("TPA_PLUGIN_EXAMPLE").map(PathBuf::from)
}

#[test]
fn loading_a_missing_library_is_reported() {
    let error = unsafe { LoadedPlugin::load("/definitely/not/a/plugin.so") };
    assert!(matches!(error, Err(PluginError::Load { .. })));
}

#[test]
fn loads_calls_and_unloads_the_example_plugin() {
    let Some(path) = example_path() else {
        eprintln!("skipping: set TPA_PLUGIN_EXAMPLE to the example cdylib path");
        return;
    };
    let plugin = unsafe { LoadedPlugin::load(&path) }.expect("load example plugin");
    assert_eq!(plugin.manifest().name, "example");
    assert_eq!(plugin.lifecycle().state(), PluginState::Started);
    assert_eq!(plugin.call("hi").expect("call"), "echo: hi");
    plugin.stop().expect("stop");
    plugin.unload().expect("unload");
}
