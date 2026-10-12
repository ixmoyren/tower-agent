//! 组装层：宿主静态插件集合（内置工具 + wasmtime [+ codemode]）。
//!
//! 关键点：组装与 CLI 会话无关，任何依赖 `tower-agent` 的宿主（axum 等）都从这里取。

#[test]
fn builtin_tools_are_assembled() {
    let tools = tower_agent::assembly::HostPlugins::new(".")
        .tools()
        .expect("assemble tools");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
    for expected in ["read", "shell", "edit", "write", "grep", "find", "ls"] {
        assert!(
            names.contains(&expected),
            "缺少内置工具 `{expected}`：{names:?}"
        );
    }
}

#[cfg(feature = "sandbox-wasmtime")]
#[test]
fn wasmtime_capability_is_registered_and_protected() {
    let registry = tower_agent::assembly::HostPlugins::new(".").registry();
    assert!(
        registry
            .protected_names()
            .iter()
            .any(|name| name == "wasmtime"),
        "wasmtime 必须作为受保护静态插件登记"
    );
    assert!(
        registry.ensure_not_protected("wasmtime").is_err(),
        "同名动态/JIT 插件必须被挡"
    );
}

#[cfg(feature = "codemode")]
#[test]
fn codemode_is_registered_and_protected() {
    use tower_agent::assembly::{HostPlugins, SessionRoot};

    // 只组装不安装：构造 codemode 插件很便宜（组件到安装时才编译）。
    let registry = HostPlugins::new(".")
        .with_codemode(SessionRoot::Ephemeral, Vec::new())
        .registry();
    assert!(
        registry
            .protected_names()
            .iter()
            .any(|name| name == "codemode"),
        "codemode 必须作为受保护静态插件登记"
    );
    assert!(registry.ensure_not_protected("codemode").is_err());
}

#[cfg(feature = "mods")]
#[test]
fn mods_host_is_assembled_on_top_of_the_plugins() {
    let manager = tower_agent::assembly::HostPlugins::new(".")
        .mods(tower_agent::core::UiState::new())
        .expect("build mods host");
    let tools = manager.tools();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
    for expected in tower_agent::monty::RESERVED_NAMES {
        assert!(
            names.contains(&expected),
            "缺少作者工具 `{expected}`：{names:?}"
        );
    }
}
