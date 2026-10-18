//! 注册表与解析的单元测试（`docs/plugins.md` 第 14 节）。
//!
//! 覆盖：插件级覆盖优先级、能力级覆盖、shadow 记录、环检测、同 major 收敛、
//! 跨 major 并存、静态单实例、静态→动态拒绝，以及缺失/可选依赖、菱形收敛与初始化顺序。

use semver::Version;
use tower_agent_plugins::{
    CapabilityId, CapabilityOverride, Dependency, PluginError, PluginId, PluginKind,
    PluginManifest, PluginRegistry, Resolution,
};

fn v(major: u64, minor: u64, patch: u64) -> Version {
    Version::new(major, minor, patch)
}

fn req(expr: &str) -> semver::VersionReq {
    expr.parse().unwrap()
}

fn cap(namespace: &str, name: &str) -> CapabilityId {
    CapabilityId::new(namespace, name)
}

fn manifest(name: &str, version: Version, kind: PluginKind) -> PluginManifest {
    PluginManifest::new(name, version, kind)
}

/// 取某消费者对某能力的绑定提供者。
fn binding_for(resolution: &Resolution, consumer: &str, capability: &CapabilityId) -> PluginId {
    resolution
        .plugins()
        .iter()
        .find(|plugin| plugin.id.name == consumer)
        .expect("consumer present")
        .bindings
        .iter()
        .find(|binding| &binding.capability == capability)
        .expect("binding present")
        .provider
        .clone()
}

#[test]
fn plugin_level_override_prefers_the_highest_kind() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("p", v(1, 0, 0), PluginKind::Static).providing(cap("tool", "x"), v(1, 0, 0)),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("p", v(1, 0, 0), PluginKind::Dynamic).providing(cap("tool", "x"), v(1, 0, 0)),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("p", v(1, 0, 0), PluginKind::Jit).providing(cap("tool", "x"), v(1, 0, 0)),
        )
        .unwrap();

    let resolution = registry.resolve().unwrap();
    let active: Vec<(&str, PluginKind)> = resolution
        .plugins()
        .iter()
        .map(|plugin| (plugin.id.name.as_str(), plugin.kind))
        .collect();
    assert_eq!(active, vec![("p", PluginKind::Jit)]);
}

#[test]
fn shadow_records_identify_the_shadowing_plugin() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("p", v(1, 0, 0), PluginKind::Static).providing(cap("tool", "x"), v(1, 0, 0)),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("p", v(1, 1, 0), PluginKind::Dynamic).providing(cap("tool", "x"), v(1, 1, 0)),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("p", v(1, 2, 0), PluginKind::Jit).providing(cap("tool", "x"), v(1, 2, 0)),
        )
        .unwrap();

    let resolution = registry.resolve().unwrap();
    assert_eq!(resolution.shadows().len(), 2);
    let beacon = PluginId::new("p", v(1, 2, 0));
    for shadow in resolution.shadows() {
        assert_eq!(
            shadow.shadowed_by, beacon,
            "shadow must point at the JIT instance"
        );
        assert_ne!(shadow.id, beacon, "the JIT instance is not shadowed");
    }
}

#[test]
fn capability_override_binds_to_the_named_implementation() {
    let mut plain = PluginRegistry::new();
    plain
        .register_manifest(
            manifest("a", v(1, 2, 0), PluginKind::Static)
                .providing(cap("sandbox", "exec"), v(1, 2, 0)),
        )
        .unwrap();
    plain
        .register_manifest(
            manifest("b", v(1, 1, 0), PluginKind::Static)
                .providing(cap("sandbox", "exec"), v(1, 1, 0)),
        )
        .unwrap();
    plain
        .register_manifest(
            manifest("c", v(1, 0, 0), PluginKind::Static)
                .requiring(cap("sandbox", "exec"), req("^1.0.0")),
        )
        .unwrap();
    // 无覆盖：同形态下选最高版本（a@1.2.0）。
    let resolution = plain.resolve().unwrap();
    assert_eq!(
        binding_for(&resolution, "c", &cap("sandbox", "exec")).name,
        "a"
    );

    let mut overridden = PluginRegistry::new();
    overridden
        .register_manifest(
            manifest("a", v(1, 2, 0), PluginKind::Static)
                .providing(cap("sandbox", "exec"), v(1, 2, 0)),
        )
        .unwrap();
    overridden
        .register_manifest(
            manifest("b", v(1, 1, 0), PluginKind::Static)
                .providing(cap("sandbox", "exec"), v(1, 1, 0)),
        )
        .unwrap();
    overridden
        .register_manifest(
            manifest("c", v(1, 0, 0), PluginKind::Static)
                .requiring(cap("sandbox", "exec"), req("^1.0.0")),
        )
        .unwrap();
    overridden.add_override(CapabilityOverride::to("b", cap("sandbox", "exec")));
    // 能力级覆盖把非工具能力 `sandbox.exec` 显式绑到 b。
    let resolution = overridden.resolve().unwrap();
    assert_eq!(
        binding_for(&resolution, "c", &cap("sandbox", "exec")).name,
        "b"
    );
}

#[test]
fn override_to_unknown_provider_is_reported() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("a", v(1, 0, 0), PluginKind::Static)
                .providing(cap("sandbox", "exec"), v(1, 0, 0)),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("c", v(1, 0, 0), PluginKind::Static)
                .requiring(cap("sandbox", "exec"), req("^1.0.0")),
        )
        .unwrap();
    registry.add_override(CapabilityOverride::to("nope", cap("sandbox", "exec")));
    let error = registry.resolve().unwrap_err();
    assert!(matches!(error, PluginError::OverrideTargetMissing { .. }));
}

#[test]
fn cycle_is_reported_with_the_full_path() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("a", v(1, 0, 0), PluginKind::Static)
                .providing(cap("tool", "a"), v(1, 0, 0))
                .requiring(cap("tool", "b"), req("^1.0.0")),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("b", v(1, 0, 0), PluginKind::Static)
                .providing(cap("tool", "b"), v(1, 0, 0))
                .requiring(cap("tool", "a"), req("^1.0.0")),
        )
        .unwrap();

    match registry.resolve() {
        Err(PluginError::Cycle(path)) => {
            assert_eq!(path.len(), 3, "a -> b -> a");
            assert_eq!(path[0], path[2]);
        }
        other => panic!("expected a cycle, got {other:?}"),
    }
}

#[test]
fn same_name_same_major_dynamic_converges_to_the_highest_version() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("p", v(1, 0, 0), PluginKind::Dynamic).providing(cap("tool", "x"), v(1, 0, 0)),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("p", v(1, 5, 0), PluginKind::Dynamic).providing(cap("tool", "x"), v(1, 5, 0)),
        )
        .unwrap();

    let resolution = registry.resolve().unwrap();
    let active: Vec<Version> = resolution
        .plugins()
        .iter()
        .map(|plugin| plugin.id.version.clone())
        .collect();
    assert_eq!(active, vec![v(1, 5, 0)]);
    assert_eq!(resolution.shadows().len(), 1);
    assert_eq!(resolution.shadows()[0].id, PluginId::new("p", v(1, 0, 0)));
}

#[test]
fn cross_major_dynamic_versions_coexist() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("p", v(1, 0, 0), PluginKind::Dynamic).providing(cap("tool", "x"), v(1, 0, 0)),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("p", v(2, 0, 0), PluginKind::Dynamic).providing(cap("tool", "x"), v(2, 0, 0)),
        )
        .unwrap();

    let resolution = registry.resolve().unwrap();
    let active: Vec<Version> = resolution
        .plugins()
        .iter()
        .map(|plugin| plugin.id.version.clone())
        .collect();
    assert_eq!(active.len(), 2);
    assert!(active.contains(&v(1, 0, 0)));
    assert!(active.contains(&v(2, 0, 0)));
    assert!(resolution.shadows().is_empty());
}

#[test]
fn cross_major_dynamic_versions_bind_consumers_to_their_major() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("p", v(1, 0, 0), PluginKind::Dynamic).providing(cap("tool", "x"), v(1, 0, 0)),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("p", v(2, 0, 0), PluginKind::Dynamic).providing(cap("tool", "x"), v(2, 0, 0)),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("c1", v(1, 0, 0), PluginKind::Dynamic)
                .requiring(cap("tool", "x"), req("^1.0.0")),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("c2", v(1, 0, 0), PluginKind::Dynamic)
                .requiring(cap("tool", "x"), req("^2.0.0")),
        )
        .unwrap();

    let resolution = registry.resolve().unwrap();
    assert_eq!(
        binding_for(&resolution, "c1", &cap("tool", "x")).version,
        v(1, 0, 0)
    );
    assert_eq!(
        binding_for(&resolution, "c2", &cap("tool", "x")).version,
        v(2, 0, 0)
    );
}

#[test]
fn duplicate_static_plugin_is_rejected() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(manifest("p", v(1, 0, 0), PluginKind::Static))
        .unwrap();
    registry
        .register_manifest(manifest("p", v(2, 0, 0), PluginKind::Static))
        .unwrap();
    let error = registry.resolve().unwrap_err();
    assert!(matches!(error, PluginError::DuplicatePlugin(_)));
}

#[test]
fn static_plugin_cannot_depend_on_a_dynamic_provider() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("dyn", v(1, 0, 0), PluginKind::Dynamic)
                .providing(cap("sandbox", "exec"), v(1, 0, 0)),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("stat", v(1, 0, 0), PluginKind::Static)
                .requiring(cap("sandbox", "exec"), req("^1.0.0")),
        )
        .unwrap();

    let error = registry.resolve().unwrap_err();
    match error {
        PluginError::KindViolation {
            plugin,
            capability,
            provider_kind,
        } => {
            assert_eq!(*plugin, PluginId::new("stat", v(1, 0, 0)));
            assert_eq!(capability, cap("sandbox", "exec"));
            assert_eq!(provider_kind, PluginKind::Dynamic);
        }
        other => panic!("expected KindViolation, got {other:?}"),
    }
}

#[test]
fn static_single_instance_must_satisfy_all_consumers() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("b", v(1, 0, 0), PluginKind::Static)
                .requiring(cap("tool", "d"), req("^1.0.0")),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("c", v(1, 0, 0), PluginKind::Static)
                .requiring(cap("tool", "d"), req("^2.0.0")),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("d", v(1, 0, 0), PluginKind::Static).providing(cap("tool", "d"), v(1, 0, 0)),
        )
        .unwrap();

    let error = registry.resolve().unwrap_err();
    assert!(matches!(error, PluginError::VersionConflict { .. }));
}

#[test]
fn diamond_dependency_converges_to_a_single_static_instance() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("a", v(1, 0, 0), PluginKind::Static)
                .requiring(cap("tool", "b"), req("^1.0.0"))
                .requiring(cap("tool", "c"), req("^1.0.0")),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("b", v(1, 0, 0), PluginKind::Static)
                .providing(cap("tool", "b"), v(1, 0, 0))
                .requiring(cap("tool", "d"), req("^1.0.0")),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("c", v(1, 0, 0), PluginKind::Static)
                .providing(cap("tool", "c"), v(1, 0, 0))
                .requiring(cap("tool", "d"), req("^1.0.0")),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("d", v(1, 0, 0), PluginKind::Static).providing(cap("tool", "d"), v(1, 0, 0)),
        )
        .unwrap();

    let resolution = registry.resolve().unwrap();
    let d_count = resolution
        .plugins()
        .iter()
        .filter(|plugin| plugin.id.name == "d")
        .count();
    assert_eq!(d_count, 1, "static `d` must be a single instance");
    assert_eq!(binding_for(&resolution, "b", &cap("tool", "d")).name, "d");
    assert_eq!(binding_for(&resolution, "c", &cap("tool", "d")).name, "d");
}

#[test]
fn missing_required_capability_is_reported() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("a", v(1, 0, 0), PluginKind::Static)
                .requiring(cap("tool", "missing"), req("^1.0.0")),
        )
        .unwrap();

    match registry.resolve() {
        Err(PluginError::MissingCapability { capability, .. }) => {
            assert_eq!(capability, cap("tool", "missing"));
        }
        other => panic!("expected MissingCapability, got {other:?}"),
    }
}

#[test]
fn optional_missing_capability_resolves_without_a_binding() {
    let mut registry = PluginRegistry::new();
    let mut optional = manifest("a", v(1, 0, 0), PluginKind::Static);
    optional
        .requires
        .push(Dependency::optional(cap("tool", "missing"), req("^1.0.0")));
    registry.register_manifest(optional).unwrap();

    let resolution = registry.resolve().unwrap();
    assert_eq!(resolution.plugins().len(), 1);
    assert!(resolution.plugins()[0].bindings.is_empty());
}

#[test]
fn resolution_orders_providers_before_consumers() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("a", v(1, 0, 0), PluginKind::Static)
                .requiring(cap("tool", "b"), req("^1.0.0")),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("b", v(1, 0, 0), PluginKind::Static).providing(cap("tool", "b"), v(1, 0, 0)),
        )
        .unwrap();

    let resolution = registry.resolve().unwrap();
    let names: Vec<&str> = resolution
        .plugins()
        .iter()
        .map(|plugin| plugin.id.name.as_str())
        .collect();
    assert_eq!(names, vec!["b", "a"]);
}

#[test]
fn protected_names_are_reported_by_ensure_not_protected() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("foundation", v(1, 0, 0), PluginKind::Static)
                .providing(cap("tool", "codemode_write"), v(1, 0, 0))
                .protected(),
        )
        .unwrap();

    match registry.ensure_not_protected("foundation") {
        Err(PluginError::Install { plugin, message }) => {
            assert_eq!(plugin, "foundation");
            assert!(message.contains("reserved"), "{message}");
        }
        other => panic!("expected Install, got {other:?}"),
    }
    assert!(registry.ensure_not_protected("other").is_ok());
}

#[test]
fn protected_capability_ignores_higher_kind_providers() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("wasmtime", v(1, 0, 0), PluginKind::Static)
                .providing(cap("sandbox", "wasmtime"), v(1, 0, 0))
                .protected(),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("shady", v(9, 0, 0), PluginKind::Jit)
                .providing(cap("sandbox", "wasmtime"), v(9, 0, 0)),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("consumer", v(1, 0, 0), PluginKind::Static)
                .requiring(cap("sandbox", "wasmtime"), req("^1.0.0")),
        )
        .unwrap();

    // 即使 JIT 形态优先级更高、版本更高，受保护能力也只认受保护实现。
    let resolution = registry.resolve().unwrap();
    assert_eq!(
        binding_for(&resolution, "consumer", &cap("sandbox", "wasmtime")).name,
        "wasmtime"
    );
}

#[test]
fn capability_override_cannot_redirect_a_protected_capability() {
    let mut registry = PluginRegistry::new();
    registry
        .register_manifest(
            manifest("wasmtime", v(1, 0, 0), PluginKind::Static)
                .providing(cap("sandbox", "wasmtime"), v(1, 0, 0))
                .protected(),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("shady", v(1, 0, 0), PluginKind::Static)
                .providing(cap("sandbox", "wasmtime"), v(1, 0, 0)),
        )
        .unwrap();
    registry
        .register_manifest(
            manifest("consumer", v(1, 0, 0), PluginKind::Static)
                .requiring(cap("sandbox", "wasmtime"), req("^1.0.0")),
        )
        .unwrap();
    registry.add_override(CapabilityOverride::to("shady", cap("sandbox", "wasmtime")));

    match registry.resolve() {
        Err(PluginError::OverrideProtected {
            capability,
            provider,
        }) => {
            assert_eq!(capability, cap("sandbox", "wasmtime"));
            assert_eq!(provider, "shady");
        }
        other => panic!("expected OverrideProtected, got {other:?}"),
    }
}
