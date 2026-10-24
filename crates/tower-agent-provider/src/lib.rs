//! Provider protocol and runtime contracts.
//!
//! Concrete vendor implementations live in `tower-agent-plugin-providers`.
//!
//! 关键契约：
//! - 一次 provider 流式调用 = 一次 [`tower::Service::call`]。
//! - `Response` 是 [`AssistantEventStream`]；`Service::Error` 只承载"建立请求"阶段的失败。
//! - 请求一旦建立成功，模型/运行期失败必须编码进流（[`AssistantMessageEvent::Error`]），
//!   与 pi `StreamFunction` 的"不得抛错、错误编码进流"契约一致。

mod api;
mod error;
mod event;
pub mod http;
mod json;
mod message;
mod model;
pub mod models;
pub mod plugin;
pub mod provider;
mod request;
mod service;
pub mod sse;
mod stream;
mod thinking;

pub use api::{
    ANTHROPIC_MESSAGES_API, GOOGLE_GENERATIVE_AI_API, OPENAI_COMPLETIONS_API, OPENAI_RESPONSES_API,
};
pub use error::{BoxError, PiError};
pub use event::{AssistantMessageEvent, DoneReason, ErrorReason};
pub use json::{JsonObject, JsonValue};
pub use message::{
    Arguments, AssistantMessage, ContentBlock, DeferredHandle, ImageContent, Message, StopReason,
    SystemMessage, TextContent, ThinkingContent, Tool, ToolCall, ToolReference, ToolResultMessage,
    Usage, UsageCost, UserContent, UserMessage,
};
pub use model::{InputModality, Model, ModelCost, ModelCostRates, ModelCostTier};
pub use models::{ModelSource, Models, ProviderRefresh, RefreshOptions, RefreshReport};
pub use plugin::{
    CredentialDescriptor, ProviderBuild, ProviderConfig, ProviderDescriptor, ProviderPlugin,
    ProviderPluginError, ProviderRegistry, ProviderSession,
};
pub use provider::{
    BoxProviderService, ProviderService, ProviderServiceExt,
    layers::{
        HeaderTransport, ObserveTransport, PayloadHook, ResponseHook, RetryTransport,
        TimeoutService,
    },
};
pub use request::{
    DeferredRequest, DeferredWindow, ProviderHeaders, SimpleStreamOptions, StreamOptions,
    StreamRequest, ThinkingBudgets, ToolChoice, TranscriptContext,
};
pub use service::{BoxStreamService, StreamFn, StreamService, StreamServiceExt};
pub use stream::AssistantEventStream;
pub use thinking::{ModelThinkingLevel, ThinkingLevel};
