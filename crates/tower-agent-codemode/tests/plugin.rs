//! codemode 的静态插件形态：清单声明与重复安装（注册表每次 `resolve` 都会 `install`）。

use std::path::Path;

use tower_agent_codemode::{CodemodePlugin, SessionRoot};
use tower_agent_plugins::{CapabilityId, PluginContext, StaticPlugin as _};

#[test]
fn manifest_claims_a_protected_codemode_capability() {
    let plugin = CodemodePlugin::new(Path::new("."), SessionRoot::Ephemeral);
    let manifest = plugin.manifest();
    assert_eq!(manifest.name, "codemode");
    assert!(manifest.protected, "codemode 必须是受保护插件");
    assert!(
        manifest
            .provides_capability(&CapabilityId::new("tool", "codemode"))
            .is_some(),
        "必须提供 tool.codemode"
    );
}

#[test]
fn install_is_idempotent_and_registers_the_codemode_tool() {
    let plugin = CodemodePlugin::new(Path::new("."), SessionRoot::Ephemeral);

    let mut first = PluginContext::new();
    plugin.install(&mut first).expect("first install");
    assert_eq!(first.tools().len(), 1);
    assert_eq!(first.tools()[0].name(), "codemode");

    // 注册表每次 resolve 都会重新 install；缓存保证第二次不必再编译组件。
    let mut second = PluginContext::new();
    plugin.install(&mut second).expect("second install");
    assert_eq!(second.tools().len(), 1);
    assert_eq!(second.tools()[0].name(), "codemode");
}
