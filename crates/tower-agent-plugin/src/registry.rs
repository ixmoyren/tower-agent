//! 插件注册表与解析：覆盖优先级、版本收敛、依赖图与拓扑排序。

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use semver::Version;
use tower_agent_runtime::AgentTool;

use crate::{
    error::PluginError,
    model::{CapabilityId, CapabilityOverride, Dependency, PluginId, PluginKind, PluginManifest},
    static_plugin::{PluginContext, StaticPlugin},
};

/// 一个注册候选（清单 + 可选载荷；M1 只有静态插件有载荷）。
struct Candidate {
    manifest: PluginManifest,
    payload: Option<Arc<dyn StaticPlugin>>,
}

impl fmt::Debug for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Candidate")
            .field("manifest", &self.manifest)
            .field("has_payload", &self.payload.is_some())
            .finish()
    }
}

/// 被同名高优先级插件遮蔽的实现。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowedPlugin {
    /// 被遮蔽的插件。
    pub id: PluginId,
    /// 遮蔽它的激活插件。
    pub shadowed_by: PluginId,
}

/// 一条「消费者 → 提供者」的能力绑定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// 被依赖的能力。
    pub capability: CapabilityId,
    /// 选中的提供者。
    pub provider: PluginId,
}

/// 解析后的一个激活插件。
pub struct ResolvedPlugin {
    /// 插件标识。
    pub id: PluginId,
    /// 形态。
    pub kind: PluginKind,
    /// 清单。
    pub manifest: PluginManifest,
    /// 依赖绑定。
    pub bindings: Vec<Binding>,
    payload: Option<Arc<dyn StaticPlugin>>,
}

impl fmt::Debug for ResolvedPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedPlugin")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("bindings", &self.bindings)
            .finish_non_exhaustive()
    }
}

/// 解析结果：激活插件（按初始化顺序）、被遮蔽插件、能力槽位。
#[derive(Debug, Default)]
pub struct Resolution {
    plugins: Vec<ResolvedPlugin>,
    shadows: Vec<ShadowedPlugin>,
    slots: BTreeMap<CapabilityId, Vec<PluginId>>,
}

impl Resolution {
    /// 激活插件，按初始化顺序（依赖在前）。
    #[must_use]
    pub fn plugins(&self) -> &[ResolvedPlugin] {
        &self.plugins
    }

    /// 被遮蔽的插件。
    #[must_use]
    pub fn shadows(&self) -> &[ShadowedPlugin] {
        &self.shadows
    }

    /// 每个能力的激活提供者（动态/JIT 可能多版本）。
    #[must_use]
    pub fn slots(&self) -> &BTreeMap<CapabilityId, Vec<PluginId>> {
        &self.slots
    }

    /// 依次执行静态插件的 `install`，收集工具与中间件计划。
    pub fn install(&self) -> Result<PluginContext, PluginError> {
        let mut context = PluginContext::new();
        for plugin in &self.plugins {
            if let Some(payload) = &plugin.payload {
                context.set_current_source(Some((plugin.id.clone(), plugin.kind)));
                for layer in payload.agent_layers() {
                    context.register_agent_layer(layer);
                }
                for layer in payload.tool_layers() {
                    context.register_tool_layer(layer);
                }
                for tool in payload.tools() {
                    context.register_tool(tool);
                }
                for handler in payload.event_handlers() {
                    context.register_event_handler(handler);
                }
                payload.install(&mut context).map_err(|error| match error {
                    PluginError::Install { .. } => error,
                    other => PluginError::Install {
                        plugin: plugin.id.to_string(),
                        message: other.to_string(),
                    },
                })?;
                context.set_current_source(None);
            }
        }
        Ok(context)
    }

    /// 便捷：解析后直接取内置工具。
    pub fn tools(&self) -> Result<Vec<AgentTool>, PluginError> {
        Ok(self.install()?.tools().to_vec())
    }
}

/// 插件注册表。
#[derive(Debug, Default)]
pub struct PluginRegistry {
    candidates: Vec<Candidate>,
    overrides: Vec<CapabilityOverride>,
    shutdown_started: AtomicBool,
}

/// 一个插件在停机时返回的失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginShutdownFailure {
    /// 插件名。
    pub plugin: String,
    /// 失败原因。
    pub message: String,
}

impl PluginRegistry {
    /// 空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个带载荷的插件。
    ///
    /// 载荷形态由 `manifest().kind` 决定：静态插件（编译期 Layer）与 JIT 插件（可热重载
    /// 的运行时载荷）都走这里；解析时的覆盖优先级、依赖与版本规则只看 `kind`。
    pub fn register_static(&mut self, plugin: Arc<dyn StaticPlugin>) -> Result<(), PluginError> {
        self.ensure_running()?;
        let manifest = plugin.manifest();
        manifest.validate()?;
        self.candidates.push(Candidate {
            manifest,
            payload: Some(plugin),
        });
        Ok(())
    }

    /// [`Self::register_static`] 的别名，表达「与形态无关」。
    pub fn register_plugin(&mut self, plugin: Arc<dyn StaticPlugin>) -> Result<(), PluginError> {
        self.register_static(plugin)
    }

    /// 便捷：把一个现成工具注册成一个静态插件（提供 `tool.<name>`）。
    ///
    /// 用于把宿主/测试注入的工具也纳入统一解析（`AgentTool` 非 `Sync`，用 `Mutex`
    /// 包一层让它能进 `StaticPlugin`）。
    pub fn register_tool(&mut self, tool: AgentTool) -> Result<(), PluginError> {
        let manifest = PluginManifest::new(
            tool.name(),
            Version::new(1, 0, 0),
            crate::PluginKind::Static,
        )
        .providing(
            CapabilityId::new("tool", tool.name()),
            Version::new(1, 0, 0),
        );
        self.register_static(Arc::new(ToolPlugin {
            manifest,
            tool: std::sync::Mutex::new(tool),
        }))
    }

    /// 按插件名移除候选（JIT 插件热更/卸载用）。返回移除的候选数。
    pub fn remove(&mut self, name: &str) -> usize {
        let before = self.candidates.len();
        self.candidates
            .retain(|candidate| candidate.manifest.name != name);
        before - self.candidates.len()
    }

    /// 受保护插件声明的能力（JIT 运行时地基提供的能力，不允许被 JIT 插件遮蔽）。
    #[must_use]
    pub fn protected_capabilities(&self) -> Vec<CapabilityId> {
        self.candidates
            .iter()
            .filter(|candidate| candidate.manifest.protected)
            .flat_map(|candidate| candidate.manifest.provides.iter())
            .map(|provided| provided.id.clone())
            .collect()
    }

    /// 受保护插件的名字。
    #[must_use]
    pub fn protected_names(&self) -> Vec<String> {
        self.candidates
            .iter()
            .filter(|candidate| candidate.manifest.protected)
            .map(|candidate| candidate.manifest.name.clone())
            .collect()
    }

    /// 若 `name` 已被受保护插件占用则报错。
    ///
    /// 受保护的静态插件（例如 JIT 运行时地基与 codemode 这类宿主能力）
    /// **永不被遮蔽**：动态与 JIT 加载器在注册前统一走这里，规则只有一处，避免
    /// 「JIT 与动态两条路各自漂移」。
    pub fn ensure_not_protected(&self, name: &str) -> Result<(), PluginError> {
        if self
            .candidates
            .iter()
            .any(|candidate| candidate.manifest.protected && candidate.manifest.name == name)
        {
            return Err(PluginError::Install {
                plugin: name.to_string(),
                message: format!(
                    "plugin name `{name}` is reserved by a protected static plugin and cannot be shadowed"
                ),
            });
        }
        Ok(())
    }

    /// 注册一个只有清单的实现（动态 / JIT 在 M1 之外；测试与后续加载器用）。
    pub fn register_manifest(&mut self, manifest: PluginManifest) -> Result<(), PluginError> {
        self.ensure_running()?;
        manifest.validate()?;
        self.candidates.push(Candidate {
            manifest,
            payload: None,
        });
        Ok(())
    }

    /// 添加一条能力级覆盖。
    pub fn add_override(&mut self, capability_override: CapabilityOverride) {
        if self.shutdown_started.load(Ordering::SeqCst) {
            return;
        }
        self.overrides.push(capability_override);
    }

    /// 停止所有活跃插件并运行它们的关闭逻辑。
    ///
    /// 该操作幂等；重复调用不会再次停止插件。
    pub fn shutdown(&mut self) -> Vec<PluginShutdownFailure> {
        if self.shutdown_started.swap(true, Ordering::SeqCst) {
            return Vec::new();
        }
        let failures = self
            .candidates
            .iter()
            .filter_map(|candidate| {
                candidate.payload.as_ref().and_then(|payload| {
                    payload.shutdown().err().map(|error| PluginShutdownFailure {
                        plugin: candidate.manifest.name.clone(),
                        message: error.to_string(),
                    })
                })
            })
            .collect();
        self.candidates.clear();
        self.overrides.clear();
        failures
    }

    fn ensure_running(&self) -> Result<(), PluginError> {
        if self.shutdown_started.load(Ordering::SeqCst) {
            return Err(PluginError::Install {
                plugin: "<registry>".to_owned(),
                message: "plugin registry is shutting down".to_owned(),
            });
        }
        Ok(())
    }

    /// 解析：同名覆盖 → 能力选择 → 形态约束 → 依赖图/环 → 初始化顺序。
    pub fn resolve(&self) -> Result<Resolution, PluginError> {
        let candidates = &self.candidates;
        if candidates.is_empty() {
            return Ok(Resolution::default());
        }

        for candidate in candidates {
            candidate.manifest.validate()?;
            if candidate.manifest.sdk_abi.major != crate::HOST_ABI.major {
                return Err(PluginError::AbiMismatch {
                    plugin: Box::new(candidate.manifest.id()),
                    expected: Box::new(crate::HOST_ABI.clone()),
                    found: Box::new(candidate.manifest.sdk_abi.clone()),
                });
            }
        }

        // 受保护插件提供的能力：解析时只认受保护实现，且覆盖不能把它转向别的实现。
        let protected_capabilities: BTreeSet<CapabilityId> = candidates
            .iter()
            .filter(|candidate| candidate.manifest.protected)
            .flat_map(|candidate| candidate.manifest.provides.iter())
            .map(|provided| provided.id.clone())
            .collect();

        // 1. 同名覆盖：取最高形态；静态单实例，动态/JIT 按 major 各留一份。
        let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (index, candidate) in candidates.iter().enumerate() {
            by_name
                .entry(candidate.manifest.name.as_str())
                .or_default()
                .push(index);
        }

        let mut active: Vec<usize> = Vec::new();
        let mut shadows: Vec<ShadowedPlugin> = Vec::new();
        for indices in by_name.values() {
            // 受保护的插件永不被遮蔽：protected 组内取胜者，其余（含同名 JIT）全部进入 shadow。
            let protected: Vec<usize> = indices
                .iter()
                .copied()
                .filter(|&index| candidates[index].manifest.protected)
                .collect();
            if !protected.is_empty() {
                let beacon = *protected
                    .iter()
                    .max_by_key(|&&index| {
                        (
                            candidates[index].manifest.kind,
                            candidates[index].manifest.version.clone(),
                        )
                    })
                    .expect("protected group is non-empty");
                for &index in indices {
                    if index != beacon {
                        shadows.push(ShadowedPlugin {
                            id: candidates[index].manifest.id(),
                            shadowed_by: candidates[beacon].manifest.id(),
                        });
                    }
                }
                active.push(beacon);
                continue;
            }

            let top_kind = indices
                .iter()
                .map(|&index| candidates[index].manifest.kind)
                .max()
                .expect("group is non-empty");
            let beacon = indices
                .iter()
                .copied()
                .find(|&index| candidates[index].manifest.kind == top_kind)
                .expect("top kind exists");
            for &index in indices {
                if candidates[index].manifest.kind < top_kind {
                    shadows.push(ShadowedPlugin {
                        id: candidates[index].manifest.id(),
                        shadowed_by: candidates[beacon].manifest.id(),
                    });
                }
            }

            let top: Vec<usize> = indices
                .iter()
                .copied()
                .filter(|&index| candidates[index].manifest.kind == top_kind)
                .collect();
            if top_kind == PluginKind::Static {
                if top.len() > 1 {
                    return Err(PluginError::DuplicatePlugin(Box::new(
                        candidates[top[1]].manifest.id(),
                    )));
                }
                active.push(top[0]);
            } else {
                let mut by_major: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
                for index in top {
                    by_major
                        .entry(candidates[index].manifest.version.major)
                        .or_default()
                        .push(index);
                }
                for mut group in by_major.into_values() {
                    group.sort_by(|&a, &b| {
                        candidates[b]
                            .manifest
                            .version
                            .cmp(&candidates[a].manifest.version)
                    });
                    active.push(group[0]);
                    for &index in &group[1..] {
                        shadows.push(ShadowedPlugin {
                            id: candidates[index].manifest.id(),
                            shadowed_by: candidates[group[0]].manifest.id(),
                        });
                    }
                }
            }
        }
        active.sort_unstable();

        // 2. 能力索引。
        let mut providers: BTreeMap<CapabilityId, Vec<usize>> = BTreeMap::new();
        for &index in &active {
            for provided in &candidates[index].manifest.provides {
                providers
                    .entry(provided.id.clone())
                    .or_default()
                    .push(index);
            }
        }

        // 3. 收集需求。
        let mut required: BTreeMap<CapabilityId, Vec<(usize, Dependency)>> = BTreeMap::new();
        for &index in &active {
            for dependency in &candidates[index].manifest.requires {
                required
                    .entry(dependency.id.clone())
                    .or_default()
                    .push((index, dependency.clone()));
            }
        }

        // 4. 逐能力选择提供者。
        let mut chosen_slots: BTreeMap<CapabilityId, Vec<usize>> = BTreeMap::new();
        let mut bindings: BTreeMap<(usize, CapabilityId), usize> = BTreeMap::new();
        for (capability, dependencies) in &required {
            let chosen = select_providers(
                capability,
                dependencies,
                &providers,
                candidates,
                &self.overrides,
                &protected_capabilities,
            )?;
            for (consumer, dependency) in dependencies {
                let pick = chosen.iter().copied().find(|&provider| {
                    dependency
                        .req
                        .matches(&candidates[provider].manifest.version)
                });
                match pick {
                    Some(provider) => {
                        bindings.insert((*consumer, capability.clone()), provider);
                    }
                    None if dependency.optional => {}
                    None => {
                        return Err(PluginError::MissingCapability {
                            consumer: Box::new(candidates[*consumer].manifest.id()),
                            capability: capability.clone(),
                            req: dependency.req.clone(),
                        });
                    }
                }
            }
            chosen_slots.insert(capability.clone(), chosen);
        }

        // 5. 形态约束：静态插件不能依赖动态/JIT。
        for ((consumer, capability), provider) in &bindings {
            let consumer_kind = candidates[*consumer].manifest.kind;
            let provider_kind = candidates[*provider].manifest.kind;
            if consumer_kind == PluginKind::Static && provider_kind != PluginKind::Static {
                return Err(PluginError::KindViolation {
                    plugin: Box::new(candidates[*consumer].manifest.id()),
                    capability: capability.clone(),
                    provider_kind,
                });
            }
        }

        // 6. 依赖图 + 环检测 + 拓扑排序。
        let mut edges: Vec<Vec<usize>> = vec![Vec::new(); candidates.len()];
        for ((consumer, _capability), provider) in &bindings {
            if consumer != provider && !edges[*consumer].contains(provider) {
                edges[*consumer].push(*provider);
            }
        }
        let order = topo_order(&active, &edges, candidates)?;

        // 7. 组装解析结果。
        let plugins = order
            .into_iter()
            .map(|index| {
                let candidate = &candidates[index];
                let bindings = bindings
                    .iter()
                    .filter(|((consumer, _), _)| *consumer == index)
                    .map(|((_, capability), provider)| Binding {
                        capability: capability.clone(),
                        provider: candidates[*provider].manifest.id(),
                    })
                    .collect();
                ResolvedPlugin {
                    id: candidate.manifest.id(),
                    kind: candidate.manifest.kind,
                    manifest: candidate.manifest.clone(),
                    bindings,
                    payload: candidate.payload.clone(),
                }
            })
            .collect();
        let slots = chosen_slots
            .into_iter()
            .map(|(capability, indices)| {
                let ids = indices
                    .into_iter()
                    .map(|index| candidates[index].manifest.id())
                    .collect();
                (capability, ids)
            })
            .collect();

        Ok(Resolution {
            plugins,
            shadows,
            slots,
        })
    }
}

/// 把一个现成工具包装成静态插件载荷。
struct ToolPlugin {
    manifest: PluginManifest,
    tool: std::sync::Mutex<AgentTool>,
}

impl StaticPlugin for ToolPlugin {
    fn manifest(&self) -> PluginManifest {
        self.manifest.clone()
    }

    fn install(&self, context: &mut PluginContext) -> Result<(), PluginError> {
        let tool = self
            .tool
            .lock()
            .map_err(|_| PluginError::Install {
                plugin: self.manifest.name.clone(),
                message: "tool plugin mutex poisoned".into(),
            })?
            .clone();
        context.register_tool(tool);
        Ok(())
    }
}

/// 为一个能力选择提供者（能力级覆盖优先，其次形态优先级 + 版本规则）。
fn select_providers(
    capability: &CapabilityId,
    dependencies: &[(usize, Dependency)],
    providers: &BTreeMap<CapabilityId, Vec<usize>>,
    candidates: &[Candidate],
    overrides: &[CapabilityOverride],
    protected_capabilities: &BTreeSet<CapabilityId>,
) -> Result<Vec<usize>, PluginError> {
    let all: &[usize] = providers.get(capability).map_or(&[], Vec::as_slice);
    // 受保护能力只认受保护实现：形态优先级（JIT > 静态）不能顶掉保护。
    let is_protected = protected_capabilities.contains(capability);
    let available: Vec<usize> = all
        .iter()
        .copied()
        .filter(|&index| !is_protected || candidates[index].manifest.protected)
        .collect();

    // 能力级覆盖：显式绑定到指定实现。
    if let Some(capability_override) = overrides.iter().find(|item| &item.capability == capability)
    {
        let matches = |index: usize| {
            let manifest = &candidates[index].manifest;
            manifest.name == capability_override.provider
                && capability_override
                    .version
                    .as_ref()
                    .is_none_or(|version| &manifest.version == version)
                && capability_override
                    .kind
                    .is_none_or(|kind| manifest.kind == kind)
        };
        let target = available
            .iter()
            .copied()
            .find(|&index| matches(index))
            .ok_or_else(|| {
                // 目标存在但不受保护（受保护能力不允许转向）→ 与「未知实现」区分开。
                if is_protected && all.iter().copied().any(matches) {
                    PluginError::OverrideProtected {
                        capability: capability.clone(),
                        provider: capability_override.provider.clone(),
                    }
                } else {
                    PluginError::OverrideTargetMissing {
                        capability: capability.clone(),
                        provider: capability_override.provider.clone(),
                    }
                }
            })?;
        let conflicts: Vec<String> = dependencies
            .iter()
            .filter(|(_, dependency)| {
                !dependency.optional
                    && !dependency.req.matches(&candidates[target].manifest.version)
            })
            .map(|(_, dependency)| dependency.req.to_string())
            .collect();
        if !conflicts.is_empty() {
            return Err(PluginError::VersionConflict {
                capability: capability.clone(),
                requirements: conflicts,
            });
        }
        return Ok(vec![target]);
    }

    if available.is_empty() {
        if let Some((consumer, dependency)) = dependencies
            .iter()
            .find(|(_, dependency)| !dependency.optional)
        {
            return Err(PluginError::MissingCapability {
                consumer: Box::new(candidates[*consumer].manifest.id()),
                capability: capability.clone(),
                req: dependency.req.clone(),
            });
        }
        return Ok(Vec::new());
    }

    let mut chosen: Vec<usize> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    for (_, dependency) in dependencies {
        let mut matching: Vec<usize> = available
            .iter()
            .copied()
            .filter(|&index| dependency.req.matches(&candidates[index].manifest.version))
            .collect();
        if matching.is_empty() {
            if !dependency.optional {
                conflicts.push(dependency.req.to_string());
            }
            continue;
        }
        matching.sort_by_key(|&index| provider_rank(candidates, index));
        let best = *matching.last().expect("non-empty");
        if !chosen.contains(&best) {
            chosen.push(best);
        }
    }
    if !conflicts.is_empty() {
        return Err(PluginError::VersionConflict {
            capability: capability.clone(),
            requirements: conflicts,
        });
    }

    // 菱形：多提供者只在「动态/JIT + 不同 major」时合法（静态单实例）。
    if chosen.len() > 1 {
        let all_multi_version = chosen
            .iter()
            .all(|&index| candidates[index].manifest.kind != PluginKind::Static);
        let majors: BTreeSet<u64> = chosen
            .iter()
            .map(|&index| candidates[index].manifest.version.major)
            .collect();
        if !all_multi_version || majors.len() != chosen.len() {
            return Err(PluginError::VersionConflict {
                capability: capability.clone(),
                requirements: dependencies
                    .iter()
                    .map(|(_, dependency)| dependency.req.to_string())
                    .collect(),
            });
        }
    }
    Ok(chosen)
}

/// 提供者排序键：形态优先级优先，其次版本（越高越靠后）。
fn provider_rank(candidates: &[Candidate], index: usize) -> (u8, Version) {
    (
        candidates[index].manifest.kind.priority(),
        candidates[index].manifest.version.clone(),
    )
}

/// 依赖图拓扑排序（依赖在前）；发现环时返回完整环路径。
fn topo_order(
    active: &[usize],
    edges: &[Vec<usize>],
    candidates: &[Candidate],
) -> Result<Vec<usize>, PluginError> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        White,
        Gray,
        Black,
    }

    fn visit(
        node: usize,
        edges: &[Vec<usize>],
        candidates: &[Candidate],
        marks: &mut [Mark],
        stack: &mut Vec<usize>,
        order: &mut Vec<usize>,
    ) -> Result<(), PluginError> {
        match marks[node] {
            Mark::Black => return Ok(()),
            Mark::Gray => {
                let start = stack.iter().position(|&item| item == node).unwrap_or(0);
                let mut cycle: Vec<PluginId> = stack[start..]
                    .iter()
                    .map(|&item| candidates[item].manifest.id())
                    .collect();
                cycle.push(candidates[node].manifest.id());
                return Err(PluginError::Cycle(cycle));
            }
            Mark::White => {}
        }
        marks[node] = Mark::Gray;
        stack.push(node);
        for &next in &edges[node] {
            visit(next, edges, candidates, marks, stack, order)?;
        }
        stack.pop();
        marks[node] = Mark::Black;
        order.push(node);
        Ok(())
    }

    let mut marks = vec![Mark::White; candidates.len()];
    let mut stack = Vec::new();
    let mut order = Vec::new();
    for &node in active {
        visit(node, edges, candidates, &mut marks, &mut stack, &mut order)?;
    }
    Ok(order)
}
