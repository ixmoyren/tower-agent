//! `AssistantEventStream`：pi `AssistantMessageEventStream` 的 Rust 形态。

use std::{
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;

use crate::event::AssistantMessageEvent;

/// assistant 事件流。
///
/// 对应 pi 的 `AssistantMessageEventStream`：只产出 [`AssistantMessageEvent`]，
/// 终态事件（`done` / `error`）自带最终 [`crate::AssistantMessage`]，
/// 因此 [`AssistantMessageEvent::terminal_message`] 等价于 pi 的 `result()`。
///
/// 内部是 boxed stream，因此本类型 `Unpin`，可直接配合 `StreamExt`。
pub struct AssistantEventStream {
    inner: Pin<Box<dyn Stream<Item = AssistantMessageEvent> + Send + 'static>>,
}

impl AssistantEventStream {
    /// 用任意事件流构造。
    pub fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = AssistantMessageEvent> + Send + 'static,
    {
        Self {
            inner: Box::pin(stream),
        }
    }
}

impl Stream for AssistantEventStream {
    type Item = AssistantMessageEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl fmt::Debug for AssistantEventStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssistantEventStream")
            .finish_non_exhaustive()
    }
}
