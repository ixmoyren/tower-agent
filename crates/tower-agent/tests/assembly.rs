//! 组装层：宿主静态插件集合（内置工具 + provider [+ codemode]）。
//!
//! 关键点：组装与 CLI 会话无关，任何依赖 `tower-agent` 的宿主（axum 等）都从这里取。

#[test]
fn builtin_tools_are_assembled() {
    let tools = tower_agent::assembly::HostPlugins::new(".")
        .tools()
        .expect("assemble tools");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
    for expected in ["read", "edit", "write", "grep", "find", "ls"] {
        assert!(
            names.contains(&expected),
            "缺少内置工具 `{expected}`：{names:?}"
        );
    }
    assert!(!names.contains(&"shell"), "shell is a dynamic plugin");
}

#[test]
fn wasmtime_is_an_sdk_feature_not_a_protected_plugin() {
    let registry = tower_agent::assembly::HostPlugins::new(".").registry();
    assert!(
        !registry
            .protected_names()
            .iter()
            .any(|name| name == "wasmtime")
    );
    assert!(registry.ensure_not_protected("wasmtime").is_ok());
}

#[test]
fn builtin_and_user_providers_are_assembled_as_plugins() {
    use std::sync::Arc;

    use semver::Version;
    use tower_agent::provider::{
        ProviderBuild, ProviderConfig, ProviderDescriptor, ProviderPlugin, ProviderPluginError,
    };

    struct CustomProvider;

    impl ProviderPlugin for CustomProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor::new("custom", "Custom", Version::new(1, 0, 0))
                .api("custom-api")
                .default_model("custom-model")
        }

        fn build(&self, _config: &ProviderConfig) -> Result<ProviderBuild, ProviderPluginError> {
            Err(ProviderPluginError::Build {
                provider: "custom".into(),
                message: "not built in this assembly test".into(),
            })
        }
    }

    let providers = tower_agent::assembly::HostPlugins::new(".")
        .with_provider(Arc::new(CustomProvider))
        .providers()
        .expect("assemble providers");
    for expected in [
        "anthropic",
        "openai",
        "openai-responses",
        "google",
        "deepseek",
        "custom",
    ] {
        assert!(
            providers.descriptor(expected).is_some(),
            "missing provider `{expected}`: {:?}",
            providers.provider_ids()
        );
    }
}

#[test]
fn a_user_provider_can_replace_one_builtin_provider_plugin() {
    use std::sync::Arc;

    use semver::Version;
    use tower_agent::provider::{
        ProviderBuild, ProviderConfig, ProviderDescriptor, ProviderPlugin, ProviderPluginError,
    };

    struct ReplacementAnthropic;

    impl ProviderPlugin for ReplacementAnthropic {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor::new("anthropic", "Replacement Anthropic", Version::new(2, 0, 0))
                .api("anthropic-messages")
                .default_model("replacement-model")
        }

        fn build(&self, _config: &ProviderConfig) -> Result<ProviderBuild, ProviderPluginError> {
            Err(ProviderPluginError::Build {
                provider: "anthropic".into(),
                message: "not built in this assembly test".into(),
            })
        }
    }

    let providers = tower_agent::assembly::HostPlugins::new(".")
        .with_provider(Arc::new(ReplacementAnthropic))
        .providers()
        .expect("assemble providers");
    let descriptor = providers.descriptor("anthropic").expect("replacement");
    assert_eq!(descriptor.name, "Replacement Anthropic");
    assert_eq!(
        descriptor.default_model.as_deref(),
        Some("replacement-model")
    );
}

#[test]
fn dynamic_provider_uses_generic_plugin_resolution_and_shadowing() {
    use std::sync::Arc;

    use semver::Version;
    use tower_agent::provider::{
        ProviderBuild, ProviderConfig, ProviderDescriptor, ProviderPlugin, ProviderPluginError,
    };

    struct DynamicAnthropic;

    impl ProviderPlugin for DynamicAnthropic {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor::new("anthropic", "Dynamic Anthropic", Version::new(2, 0, 0))
                .api("anthropic-messages")
                .default_model("dynamic-model")
        }

        fn build(&self, _config: &ProviderConfig) -> Result<ProviderBuild, ProviderPluginError> {
            Err(ProviderPluginError::Build {
                provider: "anthropic".into(),
                message: "not built in this assembly test".into(),
            })
        }
    }

    let registry = tower_agent::assembly::HostPlugins::new(".")
        .with_provider_kind(
            Arc::new(DynamicAnthropic),
            tower_agent::plugin::PluginKind::Dynamic,
        )
        .registry();
    let resolution = registry.resolve().expect("resolve plugins");
    let active = resolution
        .plugins()
        .iter()
        .find(|plugin| plugin.id.name == "provider.anthropic")
        .expect("active provider plugin");
    assert_eq!(active.kind, tower_agent::plugin::PluginKind::Dynamic);
    assert!(
        resolution
            .shadows()
            .iter()
            .any(|shadow| shadow.id.name == "provider.anthropic")
    );
}

#[cfg(feature = "codemode-js")]
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

#[cfg(feature = "codemode-python")]
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
