//! Tool authoring API shared by Rust extensions and host-side registration.

use std::future::Future;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tower_agent_runtime::{
    AgentError, AgentTool, AgentToolResult, ToolCallRequest,
    ToolRegistration as CoreToolRegistration,
};

pub use tower_agent_runtime::{ToolAnnotations, ToolExposure, ToolNamespace, ToolRegistration};

/// Rich metadata for a model-facing or codemode-callable tool.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolMetadata {
    /// Tool name used by the model and host router.
    pub name: String,
    /// Human-readable UI label.
    pub label: Option<String>,
    /// Model-facing description.
    pub description: String,
    /// Explicit parameter schema; typed registration generates one when omitted.
    pub parameters: Option<Value>,
    /// Schema for structured tool output.
    pub output_schema: Option<Value>,
    /// How the model and other tools can reach this tool.
    pub exposure: Option<ToolExposure>,
    /// Namespace/group metadata.
    pub namespace: Option<ToolNamespace>,
    /// MCP-style behavior hints.
    pub annotations: Option<ToolAnnotations>,
}

impl ToolMetadata {
    /// Construct metadata with the required name and description.
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            label: None,
            description: description.into(),
            parameters: None,
            output_schema: None,
            exposure: None,
            namespace: None,
            annotations: None,
        }
    }

    /// Set the UI label.
    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Override the parameter schema.
    #[must_use]
    pub fn parameters(mut self, parameters: Value) -> Self {
        self.parameters = Some(parameters);
        self
    }

    /// Set the structured-output schema.
    #[must_use]
    pub fn output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// Set tool exposure.
    #[must_use]
    pub fn exposure(mut self, exposure: ToolExposure) -> Self {
        self.exposure = Some(exposure);
        self
    }

    /// Set namespace/group metadata.
    #[must_use]
    pub fn namespace(mut self, namespace: ToolNamespace) -> Self {
        self.namespace = Some(namespace);
        self
    }

    /// Set MCP-style behavior hints.
    #[must_use]
    pub fn annotations(mut self, annotations: ToolAnnotations) -> Self {
        self.annotations = Some(annotations);
        self
    }
}

/// Apply SDK metadata to an existing [`AgentTool`].
pub trait AgentToolMetadataExt {
    /// Replace the name and attach all metadata present in `metadata`.
    fn with_metadata(self, metadata: ToolMetadata) -> AgentTool;
}

impl AgentToolMetadataExt for AgentTool {
    fn with_metadata(self, metadata: ToolMetadata) -> AgentTool {
        let ToolMetadata {
            name,
            label,
            description,
            parameters,
            output_schema,
            exposure,
            namespace,
            annotations,
        } = metadata;
        let mut tool = self.with_name(name).description(description);
        if let Some(label) = label {
            tool = tool.label(label);
        }
        if let Some(parameters) = parameters {
            tool = tool.parameters(parameters);
        }
        if let Some(output_schema) = output_schema {
            tool = tool.output_schema(output_schema);
        }
        if let Some(exposure) = exposure {
            tool = tool.exposure(exposure);
        }
        if let Some(namespace) = namespace {
            tool = tool.namespace(namespace);
        }
        if let Some(annotations) = annotations {
            tool = tool.annotations(annotations);
        }
        tool
    }
}

/// Register a typed handler with rich tool metadata.
///
/// `P` supplies the generated parameter schema unless
/// [`ToolMetadata::parameters`] overrides it.
pub fn typed<P, F, Fut>(metadata: ToolMetadata, handler: F) -> AgentTool
where
    P: DeserializeOwned + JsonSchema + Send + 'static,
    F: Fn(P, ToolCallRequest) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<AgentToolResult, AgentError>> + Send + 'static,
{
    let name = metadata.name.clone();
    let description = metadata.description.clone();
    CoreToolRegistration::new::<P, _, _>(name, description, handler).with_metadata(metadata)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use semver::Version;
    use serde::Deserialize;

    use super::*;
    use crate::{
        CapabilityId, PluginContext, PluginError, PluginKind, PluginManifest, StaticPlugin,
        declare_plugin,
    };

    #[derive(Deserialize, JsonSchema)]
    struct Echo {
        text: String,
    }

    #[test]
    fn rust_plugin_registers_a_tool_with_full_metadata() {
        let metadata = ToolMetadata::new("demo.echo", "Echo text")
            .label("Echo")
            .output_schema(serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } }
            }))
            .exposure(ToolExposure::Codemode)
            .namespace(ToolNamespace {
                name: "demo".to_owned(),
                aliases: vec!["examples".to_owned()],
                description: Some("Demo tools".to_owned()),
                instructions: Some("Use for SDK tests".to_owned()),
            })
            .annotations(ToolAnnotations {
                read_only_hint: Some(true),
                destructive_hint: Some(false),
                idempotent_hint: Some(true),
                open_world_hint: Some(false),
            });
        let tool = typed::<Echo, _, _>(metadata, |params: Echo, _request| async move {
            Ok(AgentToolResult::text(params.text))
        });
        let tool = Mutex::new(Some(tool));

        let plugin = declare_plugin!(
            PluginManifest::new("metadata-plugin", Version::new(1, 0, 0), PluginKind::Static)
                .providing(
                    CapabilityId::new("tool", "demo.echo"),
                    Version::new(1, 0, 0)
                ),
            move |context: &mut PluginContext| -> Result<(), PluginError> {
                let tool = tool
                    .lock()
                    .expect("tool mutex poisoned")
                    .take()
                    .expect("tool installed once");
                context.register_tool(tool);
                Ok(())
            }
        );
        let mut context = PluginContext::new();
        plugin.install(&mut context).expect("install plugin");

        let registered = context.tools().first().expect("registered tool");
        assert_eq!(registered.name(), "demo.echo");
        assert_eq!(registered.display_label(), "Echo");
        assert_eq!(registered.tool_description(), "Echo text");
        assert_eq!(registered.declared_exposure(), ToolExposure::Codemode);
        assert_eq!(
            registered
                .tool_namespace()
                .map(|namespace| namespace.name.as_str()),
            Some("demo")
        );
        assert_eq!(
            registered
                .tool_annotations()
                .and_then(|annotations| annotations.read_only_hint),
            Some(true)
        );
        assert!(registered.declared_output_schema().is_some());
        assert_eq!(
            registered
                .tool_parameters()
                .get("properties")
                .and_then(Value::as_object)
                .and_then(|properties| properties.get("text"))
                .and_then(|schema| schema.get("type"))
                .and_then(Value::as_str),
            Some("string")
        );
    }
}
