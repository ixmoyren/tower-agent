//! provider 请求边界的统一错误类型。

/// 类型擦除错误，供不需要 [`PiError`] 结构的调用方使用。
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// 建立 provider 流时产生的错误。
///
/// pi 的契约为：一旦 `StreamFunction` 返回了事件流，模型/运行期失败都编码进流，
/// 而不是抛错。因此 `Service::Error`（本类型）只覆盖"还没拿到流"的阶段：
/// 缺凭据、鉴权失败、传输失败、超时、未知 api 等。
#[derive(Debug, thiserror::Error)]
pub enum PiError {
    /// 请求的 provider 没有可用凭据。
    #[error("missing credentials for provider `{provider}`")]
    MissingCredentials {
        /// 缺少凭据的 provider 标识。
        provider: String,
    },

    /// provider 在开始流之前拒绝了这个请求。
    #[error("provider `{provider}` rejected the request: {message}")]
    Provider {
        /// 拒绝请求的 provider 标识。
        provider: String,
        /// provider 返回的错误信息。
        message: String,
    },

    /// 建立请求时的传输层失败。
    #[error("transport error: {0}")]
    Transport(String),

    /// 请求的 api/provider/model 组合不受支持。
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// 载荷编解码失败。
    #[error("serialization error: {0}")]
    Serialization(String),

    /// 建立请求超过了其超时时间。
    #[error("request timed out")]
    Timeout,

    /// 兜底变体，用于不值得单列的错误。
    #[error(transparent)]
    Other(#[from] BoxError),
}

impl PiError {
    /// 把任意错误包成 [`PiError::Other`]。
    pub fn other(error: impl Into<BoxError>) -> Self {
        Self::Other(error.into())
    }
}
