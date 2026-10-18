//! Adapter from a provider plugin to the generic static-plugin host.

use std::sync::Arc;

use tower_agent_provider::ProviderPlugin;

use crate::{CapabilityId, PluginContext, PluginError, PluginKind, PluginManifest, StaticPlugin};

/// Register a [`ProviderPlugin`] as an independent static plugin.
///
/// The plugin is named `provider.<id>` and provides the matching
/// `provider.<id>` capability, so provider replacement has plugin-level
/// granularity.
pub struct ProviderPluginRegistration {
    plugin: Arc<dyn ProviderPlugin>,
}

impl ProviderPluginRegistration {
    /// Wrap a provider plugin.
    #[must_use]
    pub fn new(plugin: Arc<dyn ProviderPlugin>) -> Self {
        Self { plugin }
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
            PluginKind::Static,
        )
        .providing(capability, descriptor.version)
    }

    fn install(&self, context: &mut PluginContext) -> Result<(), PluginError> {
        context.register_provider_plugin(Arc::clone(&self.plugin));
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
    fn provider_plugin_has_plugin_level_manifest_and_is_installed() {
        let plugin = ProviderPluginRegistration::new(Arc::new(TestProvider));
        let manifest = plugin.manifest();
        assert_eq!(manifest.name, "provider.test");
        assert!(
            manifest
                .provides_capability(&CapabilityId::new("provider", "test"))
                .is_some()
        );

        let mut context = PluginContext::new();
        plugin.install(&mut context).expect("install provider");
        assert_eq!(context.provider_plugins().len(), 1);
    }
}
