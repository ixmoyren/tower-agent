//! HTTP 传输层接缝——provider 栈的最底部（design.md 4.3）。
//!
//! provider 只依赖 [`HttpTransport`] 这个 trait，因此测试可以用脚本化传输层覆盖
//! 「请求长什么样」与「不同响应/错误怎么映射」，不需要网络。生产实现是
//! [`ReqwestTransport`]（design Q5：以 reqwest 为主）。

use std::collections::BTreeMap;

use bytes::Bytes;
use futures_core::Stream;
use futures_util::{StreamExt as _, future::BoxFuture};

/// 请求/响应头（名字保持调用方给的大小写，比较时用大小写不敏感）。
pub type HttpHeaders = BTreeMap<String, String>;

/// 观测层需要的调用上下文：provider / api / model。
///
/// **不会**出现在线上——只挂在 [`HttpRequest`] 上供 `onPayload` / `onResponse` 这类
/// 观测中间件读取（pi 把 `model` 直接传给回调）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallContext {
    /// provider 标识（pi `ProviderId`）。
    pub provider: String,
    /// api 标识（如 `anthropic-messages`）。
    pub api: String,
    /// 模型 id。
    pub model: String,
}

/// 一次 HTTP 请求；body 已经序列化好。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpRequest {
    /// HTTP 方法（目前只用 `POST`）。
    pub method: String,
    /// 完整 URL。
    pub url: String,
    /// 请求头。
    pub headers: HttpHeaders,
    /// 请求体。
    pub body: Vec<u8>,
    /// 观测层上下文（不发送）。
    pub context: Option<CallContext>,
}

/// 传输层错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct HttpError {
    /// 错误信息。
    pub message: String,
    /// 是否值得重试（网络错误为 true；协议/编码错误为 false）。
    pub retryable: bool,
}

impl HttpError {
    /// 构造一个可重试错误（连接、超时等）。
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }

    /// 构造一个不可重试错误。
    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

/// 响应体：分块的字节流。
pub type ByteStream =
std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, HttpError>> + Send + 'static>>;

/// 一次 HTTP 响应。
pub struct HttpResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 响应头。
    pub headers: HttpHeaders,
    /// 响应体。
    pub body: ByteStream,
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

impl HttpResponse {
    /// 把响应体整体读成 UTF-8 文本（用于非 2xx 的错误消息）。
    pub async fn body_text(self) -> Result<String, HttpError> {
        let mut collected = Vec::new();
        let mut body = self.body;
        while let Some(chunk) = body.next().await {
            collected.extend_from_slice(&chunk?);
        }
        Ok(String::from_utf8_lossy(&collected).into_owned())
    }

    /// 状态码是否表示成功。
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// 传输层接缝。
pub trait HttpTransport: Clone + Send + Sync + 'static {
    /// 发送一个请求。
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>>;
}

/// reqwest 实现（生产用）。
#[derive(Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestTransport {
    /// 构造一个默认客户端。
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// 用给定客户端构造。
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl HttpTransport for ReqwestTransport {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
        let client = self.client.clone();
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|error| HttpError::fatal(error.to_string()))?;
            let mut builder = client.request(method, &request.url).body(request.body);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            let response = builder
                .send()
                .await
                .map_err(|error| HttpError::retryable(error.to_string()))?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_string(), value.to_string()))
                })
                .collect();
            let body: ByteStream = Box::pin(
                response
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(|error| HttpError::retryable(error.to_string()))),
            );
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        })
    }
}
