//! provider 栈里的传输层中间件（对应 design.md 4.3 的 auth / retry）。

use std::{sync::Arc, time::Duration};

use futures_util::future::BoxFuture;
use serde_json::Value as JsonValue;

use crate::http::{CallContext, HttpError, HttpHeaders, HttpRequest, HttpResponse, HttpTransport};

/// 头部注入层：把配置里的请求头合并进每个请求（auth 就落在这里）。
///
/// 同名头（大小写不敏感）会被覆盖，与 pi「caller 覆盖 provider 默认」的优先级一致。
#[derive(Clone)]
pub struct HeaderTransport<T> {
    inner: T,
    headers: HttpHeaders,
}

impl<T: HttpTransport> HeaderTransport<T> {
    /// 用一组要注入的请求头构造。
    pub fn new(inner: T, headers: HttpHeaders) -> Self {
        Self { inner, headers }
    }

    /// 被注入的请求头。
    pub fn headers(&self) -> &HttpHeaders {
        &self.headers
    }
}

fn set_header(headers: &mut HttpHeaders, name: &str, value: &str) {
    if let Some(existing) = headers
        .keys()
        .find(|key| key.eq_ignore_ascii_case(name))
        .cloned()
    {
        headers.remove(&existing);
    }
    headers.insert(name.to_string(), value.to_string());
}

impl<T: HttpTransport> HttpTransport for HeaderTransport<T> {
    fn send(
        &self,
        mut request: HttpRequest,
    ) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
        for (name, value) in &self.headers {
            set_header(&mut request.headers, name, value);
        }
        self.inner.send(request)
    }
}

/// 重试层：对可重试的传输错误与 429 / 5xx 重试。
///
/// pi 的语义（design.md 第 7 节第 7 条）：服务器要求的长延迟**超过** `max_retry_delay_ms` 时
/// 立即失败，而不是等待——所以这种情况直接返回响应，不重试。
#[derive(Clone)]
pub struct RetryTransport<T> {
    inner: T,
    max_retries: u32,
    max_retry_delay: Duration,
}

impl<T: HttpTransport> RetryTransport<T> {
    /// 用最大重试次数与延迟上限构造。
    pub fn new(inner: T, max_retries: u32, max_retry_delay: Duration) -> Self {
        Self {
            inner,
            max_retries,
            max_retry_delay,
        }
    }

    /// 最大重试次数。
    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// 延迟上限。
    pub fn max_retry_delay(&self) -> Duration {
        self.max_retry_delay
    }
}

/// 响应状态是否值得重试。
fn retryable_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..600).contains(&status)
}

/// 解析 `Retry-After`（秒数或 HTTP-date；这里支持秒数与毫秒容错）。
fn parse_retry_after(headers: &HttpHeaders) -> Option<Duration> {
    let value = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .map(|(_, value)| value)?;
    let seconds: f64 = value.trim().parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(seconds))
}

/// 指数退避（没有 `Retry-After` 时使用），上限由 `max_retry_delay` 决定。
fn backoff(attempt: u32, max: Duration) -> Duration {
    let millis = 500u64.saturating_mul(1u64 << attempt.min(6));
    Duration::from_millis(millis).min(max)
}

impl<T: HttpTransport> HttpTransport for RetryTransport<T> {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
        let inner = self.inner.clone();
        let max_retries = self.max_retries;
        let max_delay = self.max_retry_delay;
        Box::pin(async move {
            let mut attempt = 0u32;
            loop {
                match inner.send(request.clone()).await {
                    Ok(response) if retryable_status(response.status) && attempt < max_retries => {
                        let requested = parse_retry_after(&response.headers);
                        // 服务器要求等太久：不再重试，让上层看到这个响应。
                        if requested.is_some_and(|delay| delay > max_delay) {
                            return Ok(response);
                        }
                        let delay = requested.unwrap_or_else(|| backoff(attempt, max_delay));
                        attempt += 1;
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        continue;
                    }
                    Ok(response) => return Ok(response),
                    Err(error) if error.retryable && attempt < max_retries => {
                        let delay = backoff(attempt, max_delay);
                        attempt += 1;
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
        })
    }
}

/// `onPayload` 钩子：拿到（已解析的）请求载荷与调用上下文，返回 `Some` 表示替换载荷。
pub type PayloadHook = Arc<dyn Fn(&CallContext, &JsonValue) -> Option<JsonValue> + Send + Sync>;

/// `onResponse` 钩子：拿到状态码、响应头与调用上下文（只观测，不改写）。
pub type ResponseHook = Arc<dyn Fn(&CallContext, u16, &HttpHeaders) + Send + Sync>;

/// 观测层：在请求发出前调用 `onPayload`，在拿到响应头后调用 `onResponse`。
///
/// 对应 pi `ProviderRequestOptions` 里的 `onPayload` / `onResponse`（design.md 4.2/4.3）。
/// 载荷替换后再序列化；解析失败或钩子返回 `None` 时保持原样。
#[derive(Clone)]
pub struct ObserveTransport<T> {
    inner: T,
    on_payload: Option<PayloadHook>,
    on_response: Option<ResponseHook>,
}

impl<T: HttpTransport> ObserveTransport<T> {
    /// 构造（默认不挂任何钩子）。
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            on_payload: None,
            on_response: None,
        }
    }

    /// 设置 `onPayload` 钩子。
    #[must_use]
    pub fn on_payload(mut self, hook: PayloadHook) -> Self {
        self.on_payload = Some(hook);
        self
    }

    /// 设置 `onResponse` 钩子。
    #[must_use]
    pub fn on_response(mut self, hook: ResponseHook) -> Self {
        self.on_response = Some(hook);
        self
    }
}

impl<T: HttpTransport> HttpTransport for ObserveTransport<T> {
    fn send(
        &self,
        mut request: HttpRequest,
    ) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
        if let (Some(hook), Some(context)) = (&self.on_payload, request.context.clone())
            && let Ok(payload) = serde_json::from_slice::<JsonValue>(&request.body)
            && let Some(replacement) = hook(&context, &payload)
            && let Ok(body) = serde_json::to_vec(&replacement)
        {
            request.body = body;
        }
        let context = request.context.clone();
        let on_response = self.on_response.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            let response = inner.send(request).await?;
            if let (Some(hook), Some(context)) = (on_response, context) {
                hook(&context, response.status, &response.headers);
            }
            Ok(response)
        })
    }
}

/// 超时层（design.md 4.3 的 `timeoutMs`）。
///
/// **只覆盖 `call` 返回的 future**，也就是"建立请求"阶段；响应流被消费的过程不受约束
/// （tower 中间件在本仓库的定位，见 design D1/D2）。超时映射成 [`PiError::Timeout`]，
/// 由 retry 层（如果在更内侧）先决定是否重试。
#[derive(Clone)]
pub struct TimeoutService<S> {
    inner: S,
    duration: Duration,
}

impl<S> TimeoutService<S> {
    /// 用超时时长包裹一个 provider 服务。
    pub fn new(inner: S, duration: Duration) -> Self {
        Self { inner, duration }
    }

    /// 超时时长。
    pub fn duration(&self) -> Duration {
        self.duration
    }
}

impl<S> tower::Service<crate::StreamRequest> for TimeoutService<S>
where
    S: tower::Service<
            crate::StreamRequest,
            Response = crate::AssistantEventStream,
            Error = crate::PiError,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = crate::AssistantEventStream;
    type Error = crate::PiError;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: crate::StreamRequest) -> Self::Future {
        let mut inner = self.inner.clone();
        let duration = self.duration;
        Box::pin(async move {
            use tower::ServiceExt as _;
            let ready =
                tokio::time::timeout(
                    duration,
                    async move { inner.ready().await?.call(request).await },
                );
            match ready.await {
                Ok(result) => result,
                Err(_) => Err(crate::PiError::Timeout),
            }
        })
    }
}
