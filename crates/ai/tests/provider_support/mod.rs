#![allow(dead_code)]

//! provider 测试的脚本化传输层：记录请求、按脚本返回响应（含分块 body）。
//!
//! 与 `tests/support`（conformance 用的 faux provider）分开，避免命名冲突：
//! 这里的 `collect` / `request` 是同名但不同用途的辅助。

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use ai::http::{ByteStream, HttpError, HttpHeaders, HttpRequest, HttpResponse, HttpTransport};
use bytes::Bytes;
use futures_util::{future::BoxFuture, stream};

/// 脚本化的一条响应。
#[derive(Clone)]
pub enum ScriptedResponse {
    /// 返回一个状态码 + body（body 按给定分块流出，用来模拟分片到达）。
    Response {
        status: u16,
        headers: HttpHeaders,
        chunks: Vec<Vec<u8>>,
    },
    /// 传输层直接失败。
    Error(HttpError),
}

impl ScriptedResponse {
    /// 200 + 单个文本 body。
    pub fn ok(body: impl Into<String>) -> Self {
        Self::text(200, body)
    }

    /// 指定状态码 + 文本 body。
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::Response {
            status,
            headers: HttpHeaders::new(),
            chunks: vec![body.into().into_bytes()],
        }
    }

    /// 指定状态码、响应头与分块 body。
    pub fn chunked(status: u16, headers: HttpHeaders, chunks: Vec<Vec<u8>>) -> Self {
        Self::Response {
            status,
            headers,
            chunks,
        }
    }
}

/// 记录请求、按顺序返回脚本响应的传输层。
#[derive(Clone, Default)]
pub struct ScriptedTransport {
    requests: Arc<Mutex<Vec<HttpRequest>>>,
    responses: Arc<Mutex<VecDeque<ScriptedResponse>>>,
}

impl ScriptedTransport {
    /// 用一串响应构造。
    pub fn new(responses: Vec<ScriptedResponse>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses.into())),
        }
    }

    /// 已收到的请求。
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().expect("requests").clone()
    }

    /// 收到的请求数。
    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("requests").len()
    }
}

impl HttpTransport for ScriptedTransport {
    fn send(&self, request: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
        self.requests.lock().expect("requests").push(request);
        let response = self
            .responses
            .lock()
            .expect("responses")
            .pop_front()
            .unwrap_or_else(|| ScriptedResponse::Error(HttpError::fatal("no scripted response")));
        Box::pin(async move {
            match response {
                ScriptedResponse::Error(error) => Err(error),
                ScriptedResponse::Response {
                    status,
                    headers,
                    chunks,
                } => {
                    let body: ByteStream = Box::pin(stream::iter(
                        chunks
                            .into_iter()
                            .map(|chunk| Ok(Bytes::from(chunk)))
                            .collect::<Vec<_>>(),
                    ));
                    Ok(HttpResponse {
                        status,
                        headers,
                        body,
                    })
                }
            }
        })
    }
}

/// 构造一条测试模型。
pub fn test_model() -> ai::Model {
    ai::Model {
        id: "claude-test".into(),
        name: "Claude Test".into(),
        api: ai::ANTHROPIC_MESSAGES_API.into(),
        provider: "anthropic".into(),
        base_url: "https://api.anthropic.com".into(),
        reasoning: true,
        context_window: 200_000,
        max_tokens: 4096,
        cost: ai::ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
            tiers: Vec::new(),
        },
        ..ai::Model::default()
    }
}

/// 构造一条 `StreamRequest`。
pub fn request(model: ai::Model, messages: Vec<ai::Message>) -> ai::StreamRequest {
    ai::StreamRequest {
        model,
        context: ai::TranscriptContext::new(messages),
        options: ai::SimpleStreamOptions::default(),
    }
}

/// 用户文本消息。
pub fn user(text: &str) -> ai::Message {
    ai::Message::User(ai::UserMessage {
        content: ai::UserContent::Text(text.into()),
        timestamp: 0,
    })
}

/// system 消息（可携带工具声明）。
pub fn system(text: &str, tools: Vec<ai::Tool>) -> ai::Message {
    ai::Message::System(ai::SystemMessage {
        content: vec![ai::ContentBlock::Text(ai::TextContent {
            text: text.into(),
            text_signature: None,
        })],
        sections: Default::default(),
        tools_added: tools,
        tools_removed: Vec::new(),
        timestamp: 0,
    })
}

/// 把事件流收干。
pub async fn collect(stream: ai::AssistantEventStream) -> Vec<ai::AssistantMessageEvent> {
    use futures_util::StreamExt as _;
    let mut stream = stream;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

/// 事件类型序列。
pub fn kinds(events: &[ai::AssistantMessageEvent]) -> Vec<&'static str> {
    events.iter().map(ai::AssistantMessageEvent::kind).collect()
}
