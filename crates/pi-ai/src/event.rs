//! `AssistantMessageEventStream` 的事件协议（pi `AssistantMessageEvent`）。

use crate::message::{AssistantMessage, StopReason, ToolCall};

/// `done` 事件的终止原因子集（pi `Extract<StopReason, ...>`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DoneReason {
    /// 模型自然结束。
    Stop,
    /// 达到长度上限。
    Length,
    /// 模型要求调用工具。
    ToolUse,
    /// 延迟响应。
    Deferred,
}

impl DoneReason {
    /// 转换为完整的 [`StopReason`]。
    pub const fn stop_reason(self) -> StopReason {
        match self {
            Self::Stop => StopReason::Stop,
            Self::Length => StopReason::Length,
            Self::ToolUse => StopReason::ToolUse,
            Self::Deferred => StopReason::Deferred,
        }
    }
}

/// `error` 事件的终止原因子集（pi `Extract<StopReason, ...>`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorReason {
    /// 请求被取消。
    Aborted,
    /// 请求以错误结束。
    Error,
}

impl ErrorReason {
    /// 转换为完整的 [`StopReason`]。
    pub const fn stop_reason(self) -> StopReason {
        match self {
            Self::Aborted => StopReason::Aborted,
            Self::Error => StopReason::Error,
        }
    }
}

/// `AssistantEventStream` 的事件协议（pi `AssistantMessageEvent`）。
///
/// 成功流先发 `start`，再发部分更新，最后以 `done` 结束；请求在生成开始前失败时
/// 可以直接以 `error` 结束。`update` / `done` 不得早于 `start`。
///
/// `partial` 是"截至目前为止的完整响应"这一共享视图，而不是事件时刻的快照：
/// 文本/思考块在对应的 `*_start` 时为空，此后只通过 `*_delta` 增长，直到
/// 权威的 `*_end`。`toolcall_start` 时的参数是 provider 特定的，
/// `toolcall_delta` 承载后续 JSON 片段。
#[derive(Debug, Clone, PartialEq)]
pub enum AssistantMessageEvent {
    /// 流开始，携带初始的空响应。
    Start {
        /// 截至目前为止的响应视图。
        partial: AssistantMessage,
    },
    /// 某个文本块开始。
    TextStart {
        /// 该块在 `partial.content` 中的下标。
        content_index: usize,
        /// 截至目前为止的响应视图。
        partial: AssistantMessage,
    },
    /// 文本块新增一段增量。
    TextDelta {
        /// 该块在 `partial.content` 中的下标。
        content_index: usize,
        /// 本次新增的文本片段。
        delta: String,
        /// 截至目前为止的响应视图。
        partial: AssistantMessage,
    },
    /// 某个文本块结束，携带权威文本。
    TextEnd {
        /// 该块在 `partial.content` 中的下标。
        content_index: usize,
        /// 该块的完整文本。
        content: String,
        /// 截至目前为止的响应视图。
        partial: AssistantMessage,
    },
    /// 某个思考块开始。
    ThinkingStart {
        /// 该块在 `partial.content` 中的下标。
        content_index: usize,
        /// 截至目前为止的响应视图。
        partial: AssistantMessage,
    },
    /// 思考块新增一段增量。
    ThinkingDelta {
        /// 该块在 `partial.content` 中的下标。
        content_index: usize,
        /// 本次新增的思考片段。
        delta: String,
        /// 截至目前为止的响应视图。
        partial: AssistantMessage,
    },
    /// 某个思考块结束，携带权威文本。
    ThinkingEnd {
        /// 该块在 `partial.content` 中的下标。
        content_index: usize,
        /// 该块的完整思考文本。
        content: String,
        /// 截至目前为止的响应视图。
        partial: AssistantMessage,
    },
    /// 某个工具调用块开始。
    ToolCallStart {
        /// 该块在 `partial.content` 中的下标。
        content_index: usize,
        /// 截至目前为止的响应视图。
        partial: AssistantMessage,
    },
    /// 工具调用参数新增一段 JSON 片段。
    ToolCallDelta {
        /// 该块在 `partial.content` 中的下标。
        content_index: usize,
        /// 本次新增的参数 JSON 片段。
        delta: String,
        /// 截至目前为止的响应视图。
        partial: AssistantMessage,
    },
    /// 某个工具调用块结束，携带完整调用。
    ToolCallEnd {
        /// 该块在 `partial.content` 中的下标。
        content_index: usize,
        /// 完整的工具调用。
        tool_call: ToolCall,
        /// 截至目前为止的响应视图。
        partial: AssistantMessage,
    },
    /// 流成功结束。
    Done {
        /// 成功终止的原因。
        reason: DoneReason,
        /// 最终 assistant 消息。
        message: AssistantMessage,
    },
    /// 流以失败或取消结束。
    Error {
        /// 失败或取消的原因。
        reason: ErrorReason,
        /// 携带错误信息的最终 assistant 消息。
        error: AssistantMessage,
    },
}

impl AssistantMessageEvent {
    /// 该事件是否终止流（pi `EventStream::isComplete`）。
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error { .. })
    }

    /// 若该事件终止流，返回其中的终态消息（pi `EventStream::result`）。
    pub fn terminal_message(&self) -> Option<&AssistantMessage> {
        match self {
            Self::Done { message, .. } => Some(message),
            Self::Error { error, .. } => Some(error),
            _ => None,
        }
    }

    /// pi 的 wire 事件 `type` 字符串。
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::TextStart { .. } => "text_start",
            Self::TextDelta { .. } => "text_delta",
            Self::TextEnd { .. } => "text_end",
            Self::ThinkingStart { .. } => "thinking_start",
            Self::ThinkingDelta { .. } => "thinking_delta",
            Self::ThinkingEnd { .. } => "thinking_end",
            Self::ToolCallStart { .. } => "toolcall_start",
            Self::ToolCallDelta { .. } => "toolcall_delta",
            Self::ToolCallEnd { .. } => "toolcall_end",
            Self::Done { .. } => "done",
            Self::Error { .. } => "error",
        }
    }
}
