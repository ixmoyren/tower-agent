//! `Models` 注册表与路由（对应 pi `packages/ai` 的 `Models`）。
//!
//! 三块内容：
//!
//! - **路由**：注册 `(provider, api) → provider Service`，按 [`StreamRequest::model`] 的
//!   `provider` / `api` 转发（design.md 4.2 的 `Models` 行）。
//! - **目录**：`(provider, model id) → Model` 的模型目录。
//! - **刷新**：[`Models::refresh`] 逐个 provider 调用注册的目录来源，成功就替换该 provider
//!   的条目（统计 added/updated/removed），失败就保留旧条目并把错误写进报告——与 pi
//!   `Models.refresh()` 返回 `{ aborted, errors }` 的语义一致：不抛出、按 provider 汇报。

use std::{
    collections::BTreeMap,
    sync::Arc,
    task::{Context, Poll},
};

use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;
use tower::Service;

use crate::{
    AssistantEventStream, Model, PiError, StreamRequest,
    provider::{BoxProviderService, ProviderService, ProviderServiceExt},
};

/// provider 路由表。
#[derive(Clone, Default)]
pub struct Models {
    routes: BTreeMap<(String, String), BoxProviderService>,
    catalog: BTreeMap<(String, String), Model>,
    sources: Vec<(String, ModelSource)>,
}

/// 模型目录来源：拉取某个 provider 的全部模型。
pub type ModelSource =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<Model>, PiError>> + Send + Sync>;

/// `refresh` 的选项（pi `ModelsRefreshOptions`）。
#[derive(Clone, Default)]
pub struct RefreshOptions {
    /// 是否允许访问来源；`false` 时只保持现有目录（pi 的 `allowNetwork`）。
    pub allow_network: bool,
    /// 只刷新这些 provider（`None` 表示全部）。
    pub providers: Option<Vec<String>>,
    /// 取消信号。
    pub signal: Option<CancellationToken>,
}

impl RefreshOptions {
    /// 默认选项：允许访问来源、刷新全部、不取消。
    pub fn new() -> Self {
        Self {
            allow_network: true,
            providers: None,
            signal: None,
        }
    }
}

impl std::fmt::Debug for RefreshOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshOptions")
            .field("allow_network", &self.allow_network)
            .field("providers", &self.providers)
            .finish_non_exhaustive()
    }
}

/// 单个 provider 的刷新结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRefresh {
    /// provider 标识。
    pub provider: String,
    /// 新增的模型数。
    pub added: usize,
    /// 定义发生变化的模型数。
    pub updated: usize,
    /// 来源里不再存在的模型数。
    pub removed: usize,
    /// 失败原因；成功时为 `None`。
    pub error: Option<String>,
}

/// `refresh` 的报告（pi `ModelsRefreshResult`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshReport {
    /// 调用方取消时为 `true`。
    pub aborted: bool,
    /// 各 provider 的结果，按 provider 字典序。
    pub providers: Vec<ProviderRefresh>,
}

impl RefreshReport {
    /// 失败的 provider 列表。
    pub fn errors(&self) -> Vec<(&str, &str)> {
        self.providers
            .iter()
            .filter_map(|entry| {
                entry
                    .error
                    .as_deref()
                    .map(|error| (entry.provider.as_str(), error))
            })
            .collect()
    }

    /// 是否所有 provider 都成功（被取消不算失败）。
    pub fn is_ok(&self) -> bool {
        !self.aborted && self.errors().is_empty()
    }
}

impl Models {
    /// 空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个 provider 服务，返回被替换掉的旧服务。
    pub fn register<S>(
        &mut self,
        provider: &str,
        api: &str,
        service: S,
    ) -> Option<BoxProviderService>
    where
        S: ProviderService + Clone + 'static,
        <S as Service<StreamRequest>>::Future: Send + 'static,
    {
        self.routes.insert(
            (provider.to_string(), api.to_string()),
            service.boxed_provider(),
        )
    }

    /// 注册一个已经装箱的 provider 服务。
    pub fn register_boxed(
        &mut self,
        provider: &str,
        api: &str,
        service: BoxProviderService,
    ) -> Option<BoxProviderService> {
        self.routes
            .insert((provider.to_string(), api.to_string()), service)
    }

    /// 已注册的 `(provider, api)` 列表（按字典序）。
    pub fn routes(&self) -> Vec<(&str, &str)> {
        self.routes
            .keys()
            .map(|(provider, api)| (provider.as_str(), api.as_str()))
            .collect()
    }

    /// 是否注册过某个 `(provider, api)`。
    pub fn has_route(&self, provider: &str, api: &str) -> bool {
        self.routes
            .contains_key(&(provider.to_string(), api.to_string()))
    }

    /// 按 `(provider, api)` 查找服务。
    pub fn resolve(&self, provider: &str, api: &str) -> Option<&BoxProviderService> {
        self.routes.get(&(provider.to_string(), api.to_string()))
    }

    /// 该模型是否有可用的 provider。
    pub fn supports(&self, model: &Model) -> bool {
        self.has_route(&model.provider, &model.api)
    }

    // ---------- 模型目录 ----------

    /// 写入/替换一批模型（按 provider + id），返回受影响的条目数。
    pub fn register_models(&mut self, provider: &str, models: Vec<Model>) -> usize {
        let mut affected = 0;
        for model in models {
            let key = (provider.to_string(), model.id.clone());
            self.catalog.insert(key, model);
            affected += 1;
        }
        affected
    }

    /// 目录里的全部模型（按 provider + id 排序）。
    pub fn models(&self) -> Vec<&Model> {
        self.catalog.values().collect()
    }

    /// 目录里的模型数量。
    pub fn model_count(&self) -> usize {
        self.catalog.len()
    }

    /// 查一个模型。
    pub fn get_model(&self, provider: &str, id: &str) -> Option<&Model> {
        self.catalog.get(&(provider.to_string(), id.to_string()))
    }

    /// 目录里出现过的 provider（去重、按字典序）。
    pub fn providers(&self) -> Vec<&str> {
        let mut providers: Vec<&str> = self
            .catalog
            .keys()
            .map(|(provider, _)| provider.as_str())
            .collect();
        providers.dedup();
        providers
    }

    /// 注册某个 provider 的目录来源（同一个 provider 可以注册多个，按注册顺序调用并合并）。
    pub fn register_source(&mut self, provider: &str, source: ModelSource) {
        self.sources.push((provider.to_string(), source));
    }

    /// 刷新模型目录（pi `Models.refresh`）。
    ///
    /// - 逐个 provider 调用它的来源；同名的后续来源覆盖前面的条目。
    /// - 成功：用来源返回的集合替换该 provider 的目录，并统计 added / updated / removed。
    /// - 失败：**保留**该 provider 原有条目，把错误记进报告（不抛出）。
    /// - `allow_network = false`：不调用来源，返回空报告（目录保持原样）。
    /// - 取消：检查到取消就停止，`aborted = true`，未处理的 provider 不出现在报告里。
    pub async fn refresh(&mut self, options: RefreshOptions) -> RefreshReport {
        let mut report = RefreshReport::default();
        if !options.allow_network {
            return report;
        }
        // Arc 克隆很便宜；先拿出来，避免迭代时和 `self.catalog` 的可变借用冲突。
        let sources = self.sources.clone();
        let mut providers: Vec<String> = sources
            .iter()
            .map(|(provider, _)| provider.clone())
            .collect();
        providers.sort();
        providers.dedup();
        if let Some(selected) = &options.providers {
            providers.retain(|provider| selected.contains(provider));
        }

        for provider in providers {
            if options
                .signal
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                report.aborted = true;
                return report;
            }
            let mut fetched: BTreeMap<String, Model> = BTreeMap::new();
            let mut error = None;
            for (source_provider, source) in &sources {
                if source_provider != &provider {
                    continue;
                }
                match source().await {
                    Ok(models) => {
                        for model in models {
                            fetched.insert(model.id.clone(), model);
                        }
                    }
                    Err(failure) => {
                        error = Some(failure.to_string());
                        break;
                    }
                }
            }
            if let Some(error) = error {
                // 失败时保留旧条目，只汇报错误。
                report.providers.push(ProviderRefresh {
                    provider,
                    added: 0,
                    updated: 0,
                    removed: 0,
                    error: Some(error),
                });
                continue;
            }
            let existing: BTreeMap<String, Model> = self
                .catalog
                .iter()
                .filter(|((entry_provider, _), _)| entry_provider == &provider)
                .map(|((_, id), model)| (id.clone(), model.clone()))
                .collect();
            let added = fetched
                .keys()
                .filter(|id| !existing.contains_key(*id))
                .count();
            let updated = fetched
                .iter()
                .filter(|(id, model)| existing.get(*id).is_some_and(|old| old != *model))
                .count();
            let removed = existing
                .keys()
                .filter(|id| !fetched.contains_key(*id))
                .count();
            self.catalog
                .retain(|(entry_provider, _), _| entry_provider != &provider);
            for (id, model) in fetched {
                self.catalog.insert((provider.clone(), id), model);
            }
            report.providers.push(ProviderRefresh {
                provider,
                added,
                updated,
                removed,
                error: None,
            });
        }
        report
    }
}

/// 路由不可用时的错误（对应 pi 的未知 api 行为）。
fn unsupported(model: &Model) -> PiError {
    PiError::Unsupported(format!(
        "no provider registered for provider `{}` api `{}` (model `{}`)",
        model.provider, model.api, model.id
    ))
}

impl Service<StreamRequest> for Models {
    type Response = AssistantEventStream;
    type Error = PiError;
    type Future = BoxFuture<'static, Result<AssistantEventStream, PiError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: StreamRequest) -> Self::Future {
        let model = request.model.clone();
        let Some(service) = self.resolve(&model.provider, &model.api).cloned() else {
            return Box::pin(async move { Err(unsupported(&model)) });
        };
        Box::pin(async move {
            use tower::ServiceExt as _;
            service.oneshot(request).await
        })
    }
}

impl std::fmt::Debug for Models {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Models")
            .field("routes", &self.routes())
            .finish()
    }
}
