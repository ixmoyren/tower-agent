//! provider 层：把 [`StreamRequest`] 变成 provider 的 HTTP 请求，再把响应流翻成
//! [`AssistantEventStream`]。
//!
//! 分层（design.md 4.3）：`Models 路由 → timeout → retry → auth(header 注入) → provider`。
//! provider 只依赖 [`crate::http::HttpTransport`]，所以整条链路在测试里可以用脚本化
//! 传输层驱动，不需要网络。
//!
//! 错误归属（design D7）：**建立请求之前**的失败（缺凭据、无法构造请求、传输/超时/重试耗尽）
//! 走 `Service::Error`；**provider 明确报告**的失败（非重试的 4xx、流中的 `error` 事件）
//! 编码进流，与 pi「错误编码进流」的契约一致。

pub mod anthropic;
pub(crate) mod common;
pub mod google;
pub mod layers;
pub mod openai;
pub mod openai_responses;

use std::time::{SystemTime, UNIX_EPOCH};

use tower::util::BoxCloneService;

use crate::{AssistantEventStream, PiError, StreamRequest};

/// provider 流服务。
pub trait ProviderService:
    tower::Service<StreamRequest, Response = AssistantEventStream, Error = PiError> + Send + 'static
{
}

impl<T> ProviderService for T where
    T: tower::Service<StreamRequest, Response = AssistantEventStream, Error = PiError>
        + Send
        + 'static
{
}

/// 类型擦除的 provider 服务（design D5：运行时集合存放不同实现）。
pub type BoxProviderService = BoxCloneService<StreamRequest, AssistantEventStream, PiError>;

/// 把具体 provider 装箱成 [`BoxProviderService`]。
pub trait ProviderServiceExt: ProviderService + Clone + Sized {
    /// 装箱。
    fn boxed_provider(self) -> BoxProviderService
    where
        <Self as tower::Service<StreamRequest>>::Future: Send + 'static,
    {
        BoxCloneService::new(self)
    }
}

impl<T> ProviderServiceExt for T where T: ProviderService + Clone + Sized {}

/// 当前 Unix 毫秒时间戳（pi 用 `Date.now()`）。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
