//! Provider capability for plugin authors and hosts.
//!
//! A provider is not a special plugin kind. It is a normal static, dynamic, or
//! JIT plugin that installs [`ProviderPlugin`] values into the generic
//! [`PluginContext`] extension slot. This module owns the provider-specific
//! adaptation while `tower-agent-plugin` remains domain-agnostic.

#[cfg(feature = "dynamic")]
pub mod dynamic;
pub mod wire;

use std::sync::Arc;

use crate::{CapabilityId, PluginContext, PluginError, PluginKind, PluginManifest, StaticPlugin};
use tower_agent_provider::ProviderPlugin;

#[cfg(feature = "dynamic")]
pub use dynamic::DynamicProviderPlugin;

/// Register a [`ProviderPlugin`] as an independent static plugin.
///
/// The plugin is named `provider.<id>` and provides the matching
/// `provider.<id>` capability.
pub struct ProviderPluginRegistration {
    plugin: Arc<dyn ProviderPlugin>,
    kind: PluginKind,
}

impl ProviderPluginRegistration {
    /// Wrap a provider plugin as a static plugin.
    #[must_use]
    pub fn new(plugin: Arc<dyn ProviderPlugin>) -> Self {
        Self {
            plugin,
            kind: PluginKind::Static,
        }
    }

    /// Set the plugin kind used by registry resolution.
    #[must_use]
    pub fn with_kind(mut self, kind: PluginKind) -> Self {
        self.kind = kind;
        self
    }

    /// Access the wrapped provider plugin.
    #[must_use]
    pub fn plugin(&self) -> &Arc<dyn ProviderPlugin> {
        &self.plugin
    }
}

impl StaticPlugin for ProviderPluginRegistration {
    fn manifest(&self) -> PluginManifest {
        let descriptor = self.plugin.descriptor();
        let capability = CapabilityId::new("provider", descriptor.id.clone());
        PluginManifest::new(
            format!("provider.{}", descriptor.id),
            descriptor.version.clone(),
            self.kind,
        )
        .providing(capability, descriptor.version)
    }

    fn install(&self, context: &mut PluginContext) -> Result<(), PluginError> {
        context.register_extension(Arc::clone(&self.plugin));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use semver::Version;
    use tower_agent_provider::{
        CredentialDescriptor, ProviderBuild, ProviderConfig, ProviderDescriptor,
        ProviderPluginError,
    };

    use super::*;

    struct TestProvider;

    impl ProviderPlugin for TestProvider {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor::new("test", "Test", Version::new(1, 2, 3))
                .api("test-api")
                .default_model("test-model")
                .credentials(CredentialDescriptor::None)
        }

        fn build(&self, _config: &ProviderConfig) -> Result<ProviderBuild, ProviderPluginError> {
            Err(ProviderPluginError::InvalidConfig {
                provider: "test".into(),
                message: "not used in this test".into(),
            })
        }
    }

    #[test]
    fn provider_plugin_manifest_and_extension_are_installed() {
        let plugin = ProviderPluginRegistration::new(Arc::new(TestProvider));
        let manifest = plugin.manifest();
        assert_eq!(manifest.name, "provider.test");
        assert_eq!(manifest.kind, PluginKind::Static);
        assert!(
            manifest
                .provides_capability(&CapabilityId::new("provider", "test"))
                .is_some()
        );

        let mut context = PluginContext::new();
        plugin.install(&mut context).expect("install provider");
        assert_eq!(context.extensions::<Arc<dyn ProviderPlugin>>().count(), 1);

        let dynamic =
            ProviderPluginRegistration::new(Arc::new(TestProvider)).with_kind(PluginKind::Dynamic);
        assert_eq!(dynamic.manifest().kind, PluginKind::Dynamic);
    }
}
