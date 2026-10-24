//! Static plugin declarations for the built-in tools.
//!
//! Each built-in tool is an independent plugin with a stable plugin name and a
//! matching `tool.<name>` capability. Registration, resolution, and assembly are
//! host responsibilities; this module only exposes the plugin instances.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use semver::Version;
use tower_agent_plugin::{
    CapabilityId, PluginContext, PluginError, PluginKind, PluginManifest, StaticPlugin,
};

/// Version of the built-in static tool plugins.
const BUILTIN_VERSION: Version = Version::new(1, 0, 0);

/// Names of the built-in tool plugins, in their stable registration order.
pub const DEFAULT_TOOL_NAMES: [&str; 6] = [
    crate::ReadTool::NAME,
    crate::EditTool::NAME,
    crate::WriteTool::NAME,
    crate::GrepTool::NAME,
    crate::FindTool::NAME,
    crate::LsTool::NAME,
];

macro_rules! define_tool_plugin {
    ($plugin:ident, $name:expr, $build:path) => {
        #[doc = concat!("Static plugin for the built-in `", $name, "` tool.")]
        #[derive(Debug, Clone)]
        pub struct $plugin {
            cwd: PathBuf,
        }

        impl $plugin {
            #[doc = concat!("Construct the `", $name, "` plugin for a workspace.")]
            #[must_use]
            pub fn new(cwd: impl Into<PathBuf>) -> Self {
                Self { cwd: cwd.into() }
            }
        }

        impl StaticPlugin for $plugin {
            fn manifest(&self) -> PluginManifest {
                PluginManifest::new($name, BUILTIN_VERSION.clone(), PluginKind::Static)
                    .providing(CapabilityId::new("tool", $name), BUILTIN_VERSION.clone())
            }

            fn install(&self, context: &mut PluginContext) -> Result<(), PluginError> {
                context.register_tool($build(&self.cwd));
                Ok(())
            }
        }
    };
}

define_tool_plugin!(ReadPlugin, "read", crate::create_read_tool);
define_tool_plugin!(EditPlugin, "edit", crate::create_edit_tool);
define_tool_plugin!(WritePlugin, "write", crate::create_write_tool);
define_tool_plugin!(GrepPlugin, "grep", crate::create_grep_tool);
define_tool_plugin!(FindPlugin, "find", crate::create_find_tool);
define_tool_plugin!(LsPlugin, "ls", crate::create_ls_tool);

/// Build the built-in plugin instances without registering them.
///
/// A host assembly layer is responsible for registering these values with a
/// [`tower_agent_plugin::PluginRegistry`], resolving them, and installing their
/// tools.
#[must_use]
pub fn builtin_plugins(cwd: impl AsRef<Path>) -> Vec<Arc<dyn StaticPlugin>> {
    let cwd = cwd.as_ref();
    vec![
        Arc::new(ReadPlugin::new(cwd)),
        Arc::new(EditPlugin::new(cwd)),
        Arc::new(WritePlugin::new(cwd)),
        Arc::new(GrepPlugin::new(cwd)),
        Arc::new(FindPlugin::new(cwd)),
        Arc::new(LsPlugin::new(cwd)),
    ]
}

#[cfg(test)]
mod tests {
    use tower_agent_plugin::PluginRegistry;

    use super::*;

    #[test]
    fn builtin_tools_are_independent_static_plugins() {
        let plugins = builtin_plugins(".");
        let manifests: Vec<PluginManifest> =
            plugins.iter().map(|plugin| plugin.manifest()).collect();
        let names: Vec<&str> = manifests
            .iter()
            .map(|manifest| manifest.name.as_str())
            .collect();
        assert_eq!(names, DEFAULT_TOOL_NAMES);
        for manifest in manifests {
            assert_eq!(manifest.kind, PluginKind::Static);
            assert!(
                manifest
                    .provides_capability(&CapabilityId::new("tool", &manifest.name))
                    .is_some()
            );
        }
    }

    #[test]
    fn host_resolution_installs_one_tool_per_plugin() {
        let mut registry = PluginRegistry::new();
        for plugin in builtin_plugins(".") {
            registry.register_plugin(plugin).expect("register plugin");
        }
        let tools = registry
            .resolve()
            .expect("resolve plugins")
            .tools()
            .expect("install");
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(names, DEFAULT_TOOL_NAMES);
    }

    #[test]
    fn a_replacement_plugin_overrides_one_builtin_by_plugin_name() {
        let mut registry = PluginRegistry::new();
        for plugin in builtin_plugins(".") {
            registry.register_plugin(plugin).expect("register plugin");
        }
        registry
            .register_manifest(
                PluginManifest::new("read", Version::new(2, 0, 0), PluginKind::Dynamic)
                    .providing(CapabilityId::new("tool", "read"), Version::new(2, 0, 0)),
            )
            .expect("register replacement");

        let resolution = registry.resolve().expect("resolve plugins");
        let reads: Vec<_> = resolution
            .plugins()
            .iter()
            .filter(|plugin| plugin.id.name == "read")
            .collect();
        assert_eq!(reads.len(), 1);
        assert_eq!(reads[0].kind, PluginKind::Dynamic);
        assert!(
            resolution
                .shadows()
                .iter()
                .any(|shadow| shadow.id.name == "read"),
            "the static read plugin should be shadowed as one plugin"
        );
        assert_eq!(
            resolution
                .plugins()
                .iter()
                .filter(|plugin| plugin.id.name != "read")
                .count(),
            6,
            "other built-in plugins remain active"
        );
    }
}
