//! `ai` — Rust 对应 pi 的 `packages/ai`。
//!
//! 本 crate 当前只落地第一个移植单元：`StreamFn → Service` 的类型与 trait 边界。
//! 行为仍以 pi 参考实现为准。
//!
//! 关键契约：
//! - 一次 provider 流式调用 = 一次 [`tower::Service::call`]。
//! - `Response` 是 [`AssistantEventStream`]；`Service::Error` 只承载"建立请求"阶段的失败。
//! - 请求一旦建立成功，模型/运行期失败必须编码进流（[`AssistantMessageEvent::Error`]），
//!   与 pi `StreamFunction` 的"不得抛错、错误编码进流"契约一致。

mod error;
mod event;
pub mod http;
mod json;
mod message;
mod model;
pub mod models;
pub mod provider;
mod request;
mod service;
pub mod sse;
mod stream;

pub use error::{BoxError, PiError};
pub use event::{AssistantMessageEvent, DoneReason, ErrorReason};
pub use json::{JsonObject, JsonValue};
pub use message::{
    AssistantMessage, ContentBlock, ImageContent, Message, StopReason, SystemMessage, TextContent,
    ThinkingContent, Tool, ToolCall, ToolReference, ToolResultMessage, Usage, UsageCost,
    UserContent, UserMessage,
};
pub use model::{InputModality, Model, ModelCost, ModelCostRates, ModelCostTier};
pub use models::{ModelSource, Models, ProviderRefresh, RefreshOptions, RefreshReport};
pub use provider::{
    BoxProviderService, ProviderService, ProviderServiceExt,
    anthropic::{API as ANTHROPIC_MESSAGES_API, AnthropicConfig, AnthropicProvider},
    google::{API as GOOGLE_GENERATIVE_AI_API, GoogleConfig, GoogleProvider},
    layers::{
        HeaderTransport, ObserveTransport, PayloadHook, ResponseHook, RetryTransport,
        TimeoutService,
    },
    openai::{API as OPENAI_COMPLETIONS_API, OpenAiConfig, OpenAiProvider},
    openai_responses::{
        API as OPENAI_RESPONSES_API, OpenAiResponsesConfig, OpenAiResponsesProvider,
    },
};
pub use request::{
    DeferredHandle, DeferredRequest, DeferredWindow, ModelThinkingLevel, ProviderHeaders,
    SimpleStreamOptions, StreamOptions, StreamRequest, ThinkingBudgets, ThinkingLevel, ToolChoice,
    TranscriptContext,
};
pub use service::{BoxStreamService, StreamFn, StreamService, StreamServiceExt};
pub use stream::AssistantEventStream;
