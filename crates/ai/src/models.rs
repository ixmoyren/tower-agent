//! `Models` 注册表与路由（对应 pi `packages/ai` 的 `Models`）。
//!
//! pi 的 `Models` 把「目录 / 凭据」这类状态与「按 provider + api 分发」的路由放在一起；
//! 这里先落地路由部分：注册 `(provider, api) → provider Service`，然后按
//! [`StreamRequest::model`] 里的 `provider` / `api` 转发（design.md 4.2 的 `Models` 行）。

use std::{
    collections::BTreeMap,
    task::{Context, Poll},
};

use futures_util::future::BoxFuture;
use tower::Service;

use crate::{
    AssistantEventStream, Model, PiError, StreamRequest,
    provider::{BoxProviderService, ProviderService, ProviderServiceExt},
};

/// provider 路由表。
#[derive(Clone, Default)]
pub struct Models {
    routes: BTreeMap<(String, String), BoxProviderService>,
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
