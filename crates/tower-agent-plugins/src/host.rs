//! 宿主 API（`docs/plugins.md` 第 8 节）：插件通过它中介调用宿主提供的能力。
//!
//! 目前宿主可中介的能力是内置工具（`tool.<name>`）。插件拿到的不是实现本身，而是
//! 一个按清单 `requires` 收窄的 [`PluginAccess`]：只能调用自己声明过、宿主也确实提供
//! 的能力；未声明的调用直接 [`PluginError::PermissionDenied`]。跨形态调用都走这里，
//! 宿主负责实现查找与（动态/JIT 场景下的）in-flight 记账。

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use tower_agent_runtime::{AgentTool, ToolCallRequest, ToolResultStream};

use crate::{
    CapabilityId, Permission, PermissionPolicy, PluginError, PluginId, PluginManifest, Resolution,
};

/// 宿主句柄：已登记的能力实现 + 权限策略。
#[derive(Clone)]
pub struct HostApi {
    // `AgentTool` 是 `Send` 但不 `Sync`（含 `BoxCloneService`），因此用 `Mutex` 包一层，
    // 让 `HostApi` 本身可跨线程共享（插件在别的任务里回调宿主）。
    tools: Arc<Mutex<BTreeMap<String, AgentTool>>>,
    permissions: Arc<PermissionPolicy>,
}

impl HostApi {
    /// 从解析结果构造：安装静态插件后登记所有工具能力。
    pub fn from_resolution(resolution: &Resolution) -> Result<Self, PluginError> {
        let context = resolution.install()?;
        let mut tools = BTreeMap::new();
        for tool in context.tools() {
            tools.insert(tool.name().to_string(), tool.clone());
        }
        Ok(Self {
            tools: Arc::new(Mutex::new(tools)),
            permissions: Arc::new(PermissionPolicy::unattended()),
        })
    }

    /// 用一组工具实现构造。
    pub fn from_tools(tools: impl IntoIterator<Item = AgentTool>) -> Self {
        let tools = tools
            .into_iter()
            .map(|tool| (tool.name().to_string(), tool))
            .collect();
        Self {
            tools: Arc::new(Mutex::new(tools)),
            permissions: Arc::new(PermissionPolicy::unattended()),
        }
    }

    /// 替换权限策略。
    #[must_use]
    pub fn with_permissions(mut self, permissions: PermissionPolicy) -> Self {
        self.permissions = Arc::new(permissions);
        self
    }

    /// 已登记的工具能力名（`tool.<name>` 的 `name` 部分，按字典序）。
    #[must_use]
    pub fn capabilities(&self) -> Vec<String> {
        self.tools
            .lock()
            .expect("host tools mutex poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// 为某个插件构造受限的能力视图。
    pub fn access_for(&self, manifest: &PluginManifest) -> Result<PluginAccess, PluginError> {
        let mut allowed = BTreeSet::new();
        for dependency in &manifest.requires {
            if dependency.id.namespace != "tool" {
                continue;
            }
            let provided = self
                .tools
                .lock()
                .expect("host tools mutex poisoned")
                .contains_key(&dependency.id.name);
            if !provided {
                if dependency.optional {
                    continue;
                }
                return Err(PluginError::MissingCapability {
                    consumer: Box::new(manifest.id()),
                    capability: dependency.id.clone(),
                    req: dependency.req.clone(),
                });
            }
            allowed.insert(dependency.id.name.clone());
        }
        Ok(PluginAccess {
            host: self.clone(),
            plugin: manifest.id(),
            allowed: Arc::new(allowed),
        })
    }

    /// 中介调用 `tool.<name>`。
    pub async fn call_tool(
        &self,
        name: &str,
        request: ToolCallRequest,
    ) -> Result<ToolResultStream, PluginError> {
        self.call_tool_with(name, request, &[]).await
    }

    /// 带权限声明的中介调用：先查权限，再调用实现。
    pub async fn call_tool_with(
        &self,
        name: &str,
        request: ToolCallRequest,
        permissions: &[Permission],
    ) -> Result<ToolResultStream, PluginError> {
        let capability = CapabilityId::new("tool", name);
        let tool = self
            .tools
            .lock()
            .expect("host tools mutex poisoned")
            .get(name)
            .cloned()
            .ok_or_else(|| PluginError::Load {
                plugin: "host".into(),
                message: format!("unknown capability `{capability}`"),
            })?;
        self.permissions.check(&capability, permissions)?;
        tool.call(request).await.map_err(|error| PluginError::Load {
            plugin: capability.to_string(),
            message: error.to_string(),
        })
    }
}

/// 限定给某个插件的能力视图：只能调用其 `requires` 声明过的能力。
#[derive(Clone)]
pub struct PluginAccess {
    host: HostApi,
    plugin: PluginId,
    allowed: Arc<BTreeSet<String>>,
}

impl PluginAccess {
    /// 所属插件。
    #[must_use]
    pub fn plugin(&self) -> &PluginId {
        &self.plugin
    }

    /// 该视图允许调用的能力名。
    pub fn capabilities(&self) -> impl Iterator<Item = &str> {
        self.allowed.iter().map(String::as_str)
    }

    /// 调用一个声明过的工具能力；未声明则拒绝。
    pub async fn call_tool(
        &self,
        name: &str,
        request: ToolCallRequest,
    ) -> Result<ToolResultStream, PluginError> {
        if !self.allowed.contains(name) {
            return Err(PluginError::PermissionDenied {
                capability: CapabilityId::new("tool", name),
            });
        }
        self.host.call_tool(name, request).await
    }
}

#[cfg(test)]
mod tests {
    use semver::Version;
    use tower_agent_provider::{Arguments, ToolCall};

    use super::*;
    use crate::{Dependency, PluginKind};

    fn request(name: &str) -> ToolCallRequest {
        ToolCallRequest::new(ToolCall {
            id: "call-1".into(),
            name: name.into(),
            arguments: Arguments::default(),
            thought_signature: None,
            namespace: None,
        })
    }

    fn host() -> HostApi {
        HostApi::from_tools([tower_agent_tools::create_ls_tool(std::env::temp_dir())])
    }

    fn manifest(name: &str) -> PluginManifest {
        PluginManifest::new(name, Version::new(1, 0, 0), PluginKind::Static)
    }

    #[test]
    fn access_is_narrowed_to_declared_requires() {
        let host = host();
        let manifest =
            manifest("probe").requiring(CapabilityId::new("tool", "ls"), "^1.0.0".parse().unwrap());
        let access = host.access_for(&manifest).expect("ls is provided");
        assert_eq!(access.capabilities().collect::<Vec<_>>(), vec!["ls"]);
        assert_eq!(access.plugin().name, "probe");
    }

    #[test]
    fn missing_required_capability_is_reported() {
        let host = host();
        let manifest = manifest("probe")
            .requiring(CapabilityId::new("tool", "nope"), "^1.0.0".parse().unwrap());
        assert!(matches!(
            host.access_for(&manifest),
            Err(PluginError::MissingCapability { .. })
        ));
    }

    #[test]
    fn optional_missing_capability_is_skipped() {
        let host = host();
        let mut manifest = manifest("probe");
        manifest.requires.push(Dependency::optional(
            CapabilityId::new("tool", "nope"),
            "^1.0.0".parse().unwrap(),
        ));
        assert!(host.access_for(&manifest).is_ok());
    }

    #[tokio::test]
    async fn undeclared_calls_are_denied_and_declared_calls_run() {
        let host = host();
        let manifest =
            manifest("probe").requiring(CapabilityId::new("tool", "ls"), "^1.0.0".parse().unwrap());
        let access = host.access_for(&manifest).unwrap();

        assert!(matches!(
            access.call_tool("read", request("read")).await,
            Err(PluginError::PermissionDenied { .. })
        ));
        let stream = access
            .call_tool("ls", request("ls"))
            .await
            .expect("ls runs");
        drop(stream);
    }
}
