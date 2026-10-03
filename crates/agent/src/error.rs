//! agent 工具边界的统一错误类型。

/// 类型擦除错误，供不需要 [`AgentError`] 结构的调用方使用。
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// 工具服务在建立/调用阶段的失败。
///
/// pi 的工具契约是"抛错，或返回 `isError: true` 的结果"。业务失败应优先编码为
/// [`crate::AgentToolResult`] 的 `is_error`，本类型只承载框架级失败（无法构造流、
/// 内部 panic 转换等）。
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// 工具执行本身失败。
    #[error("tool failure: {0}")]
    Tool(String),

    /// 编排循环拒绝执行（例如从一个 assistant 消息继续）。
    #[error("agent loop error: {0}")]
    Loop(String),

    /// 建立 provider 流失败（凭据、传输、超时等）。
    #[error(transparent)]
    Provider(#[from] ai::PiError),

    /// 兜底变体，用于不值得单列的错误。
    #[error(transparent)]
    Other(#[from] BoxError),
}

impl AgentError {
    /// 把任意错误包成 [`AgentError::Other`]。
    pub fn other(error: impl Into<BoxError>) -> Self {
        Self::Other(error.into())
    }
}
