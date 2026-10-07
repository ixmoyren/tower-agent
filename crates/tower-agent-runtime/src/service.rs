//! 无状态 / 流式 / 池化的 agent 服务（`docs/tower-agent-architecture.md` §4.2）。
//!
//! - [`AgentService`]：无状态形态——请求的 `messages` 就是完整上下文，每次调用用全新
//!   [`Agent`] 跑一轮，不保留任何会话状态；配置 [`SessionStore`] 后也可用 `session_id`
//!   让服务端代为存取 transcript。
//! - [`AgentStreamService`]：流式形态——`Response = AgentEventStream`，适合 SSE / TUI。
//! - [`AgentPool`]：池化形态——用信号量限制并发（背压），给 axum 这类多请求宿主用。

use std::{
    collections::HashMap,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use futures_core::Stream;
use futures_util::future::BoxFuture;
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc};
use tower::Service;
use tower_agent_core::{
    AgentError, AgentEvent, AgentEventStream, AgentMessage, AgentRequest, AgentResponse, AgentTool,
    SessionStore,
};
use tower_agent_provider::{BoxStreamService, Message, Model, StopReason};

use crate::{
    Agent, AgentOptions,
    agent::{failure_message, summarize},
};

/// 无状态 agent 服务：每次请求携带完整上下文，不保留会话状态。
#[derive(Clone)]
pub struct AgentService {
    // `AgentOptions` 含 `BoxStreamService`（`Send` 但非 `Sync`），用 `Mutex` 包一层，
    // 让 `AgentService` 本身 `Send + Sync`（跨 await 持 `&self` 才安全）。
    template: Arc<Mutex<AgentOptions>>,
    sessions: Option<Arc<dyn SessionStore>>,
}

impl AgentService {
    /// 用 provider 流服务构造（其余取默认）。
    #[must_use]
    pub fn new(stream_fn: BoxStreamService) -> Self {
        Self {
            template: Arc::new(Mutex::new(AgentOptions::new(stream_fn))),
            sessions: None,
        }
    }

    /// 用一份 [`AgentOptions`] 作为模板构造（每次请求会克隆它）。
    #[must_use]
    pub fn from_options(template: AgentOptions) -> Self {
        Self {
            template: Arc::new(Mutex::new(template)),
            sessions: None,
        }
    }

    /// 配置可选会话存储（启用 `session_id` 模式）。
    #[must_use]
    pub fn with_sessions(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.sessions = Some(store);
        self
    }

    /// 设置默认模型。
    #[must_use]
    pub fn with_model(self, model: Model) -> Self {
        self.template
            .lock()
            .expect("agent template poisoned")
            .initial
            .model = Some(model);
        self
    }

    /// 设置默认工具集。
    #[must_use]
    pub fn with_tools(self, tools: Vec<AgentTool>) -> Self {
        self.template
            .lock()
            .expect("agent template poisoned")
            .initial
            .tools = tools;
        self
    }

    /// 模板快照（用于诊断）。
    #[must_use]
    pub fn template(&self) -> AgentOptions {
        self.template
            .lock()
            .expect("agent template poisoned")
            .clone()
    }

    /// 为一次请求构造运行时（含 transcript 解析与覆盖项）。
    ///
    /// 用 owned `request`（而不是 `&request`）是为了让 future 保持 `Send`：
    /// `AgentRequest` 含 `AgentTool`（`Send` 但非 `Sync`），`&AgentRequest` 不能跨 await。
    async fn prepare(&self, request: AgentRequest) -> Agent {
        let AgentRequest {
            messages,
            session_id,
            model,
            tools,
            system_prompt,
        } = request;

        let mut options = self.template();
        if let Some(model) = model {
            options.initial.model = Some(model);
        }
        if let Some(tools) = tools {
            options.initial.tools = tools;
        }
        if let Some(prompt) = system_prompt {
            options.initial.system_prompt = Some(prompt);
        }

        let transcript = match (self.sessions.as_ref(), session_id.as_deref()) {
            (Some(store), Some(session)) => {
                let mut transcript = store.load(session).await.unwrap_or_default();
                transcript.extend(messages);
                transcript
            }
            _ => messages,
        };
        options.initial.messages = transcript;
        Agent::new(options)
    }

    /// 跑一次无状态 run，返回本次新增的消息与汇总。
    pub async fn run(&self, request: AgentRequest) -> Result<AgentResponse, AgentError> {
        let session = request.session_id.clone();
        let agent = self.prepare(request).await;
        let before = agent.state().messages.len();
        agent.continue_run().await?;

        let messages = agent.state().messages;
        let produced = messages
            .get(before.min(messages.len())..)
            .unwrap_or(&[])
            .to_vec();
        if let (Some(store), Some(session)) = (self.sessions.as_ref(), session.as_ref()) {
            store.save(session.clone(), messages).await;
        }
        let (text, usage, stop_reason) = summarize(&produced);
        Ok(AgentResponse {
            messages: produced,
            text,
            usage,
            stop_reason,
        })
    }

    /// 跑一次流式 run，返回事件流（`AgentEventStream`）。
    pub fn run_stream(&self, request: AgentRequest) -> AgentEventStream {
        let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
        let service = self.clone();
        tokio::spawn(async move {
            let session = request.session_id.clone();
            let model = service.template().initial.model.clone().unwrap_or_default();
            let agent = service.prepare(request).await;
            let mut subscription = agent.subscribe();

            let emitted = Arc::new(AtomicUsize::new(0));
            let forwarder = {
                let tx = tx.clone();
                let emitted = Arc::clone(&emitted);
                tokio::spawn(async move {
                    while let Some(event) = subscription.recv().await {
                        emitted.fetch_add(1, Ordering::SeqCst);
                        if tx.send(event).is_err() {
                            break;
                        }
                    }
                })
            };

            let result = agent.continue_run().await;
            let messages = agent.state().messages;
            if let (Some(store), Some(session)) = (service.sessions.as_ref(), session.as_ref()) {
                store.save(session.clone(), messages).await;
            }

            // run 返回时所有事件都已 emit 并送达；停掉转发任务并关闭通道即可结束流。
            forwarder.abort();
            let _ = forwarder.await;
            if let Err(error) = result
                && emitted.load(Ordering::SeqCst) == 0
            {
                emit_failure(&tx, &model, &error);
            }
            drop(tx);
        });
        Box::pin(ReceiverStream { rx })
    }
}

impl Service<AgentRequest> for AgentService {
    type Response = AgentResponse;
    type Error = AgentError;
    type Future = BoxFuture<'static, Result<AgentResponse, AgentError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: AgentRequest) -> Self::Future {
        let service = self.clone();
        Box::pin(async move { service.run(request).await })
    }
}

/// 流式 agent 服务：`Response = AgentEventStream`。
#[derive(Clone)]
pub struct AgentStreamService {
    inner: AgentService,
}

impl AgentStreamService {
    /// 用无状态服务构造。
    #[must_use]
    pub fn new(inner: AgentService) -> Self {
        Self { inner }
    }

    /// 无状态服务。
    #[must_use]
    pub fn inner(&self) -> &AgentService {
        &self.inner
    }
}

impl Service<AgentRequest> for AgentStreamService {
    type Response = AgentEventStream;
    type Error = AgentError;
    type Future = BoxFuture<'static, Result<AgentEventStream, AgentError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: AgentRequest) -> Self::Future {
        let stream = self.inner.run_stream(request);
        Box::pin(async move { Ok(stream) })
    }
}

/// 池化 agent 服务：用信号量限制并发，`poll_ready` 负责取用配额（背压）。
pub struct AgentPool {
    service: AgentService,
    permits: Arc<Semaphore>,
    max: usize,
    permit: Option<OwnedSemaphorePermit>,
    pending: Option<BoxFuture<'static, Result<OwnedSemaphorePermit, AcquireError>>>,
}

impl AgentPool {
    /// 用无状态服务与最大并发构造。
    #[must_use]
    pub fn new(service: AgentService, max_concurrency: usize) -> Self {
        let max = max_concurrency.max(1);
        Self {
            service,
            permits: Arc::new(Semaphore::new(max)),
            max,
            permit: None,
            pending: None,
        }
    }

    /// 最大并发。
    #[must_use]
    pub fn max_concurrency(&self) -> usize {
        self.max
    }

    /// 当前可用配额。
    #[must_use]
    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }
}

impl Clone for AgentPool {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            permits: Arc::clone(&self.permits),
            max: self.max,
            permit: None,
            pending: None,
        }
    }
}

impl Service<AgentRequest> for AgentPool {
    type Response = AgentResponse;
    type Error = AgentError;
    type Future = BoxFuture<'static, Result<AgentResponse, AgentError>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.permit.is_some() {
            return Poll::Ready(Ok(()));
        }
        loop {
            if let Some(pending) = self.pending.as_mut() {
                match pending.as_mut().poll(cx) {
                    Poll::Ready(Ok(permit)) => {
                        self.permit = Some(permit);
                        self.pending = None;
                        return Poll::Ready(Ok(()));
                    }
                    Poll::Ready(Err(_)) => {
                        self.pending = None;
                        return Poll::Ready(Err(pool_closed()));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
            match Arc::clone(&self.permits).try_acquire_owned() {
                Ok(permit) => {
                    self.permit = Some(permit);
                    return Poll::Ready(Ok(()));
                }
                Err(TryAcquireError::NoPermits) => {
                    self.pending = Some(Box::pin(Arc::clone(&self.permits).acquire_owned()));
                }
                Err(TryAcquireError::Closed) => return Poll::Ready(Err(pool_closed())),
            }
        }
    }

    fn call(&mut self, request: AgentRequest) -> Self::Future {
        let service = self.service.clone();
        let permit = self.permit.take();
        Box::pin(async move {
            let _permit = permit;
            service.run(request).await
        })
    }
}

fn pool_closed() -> AgentError {
    AgentError::Loop("agent pool is closed".into())
}

/// 内存会话存储（进程内 `HashMap`）。
#[derive(Default)]
pub struct MemorySessionStore {
    sessions: Mutex<HashMap<String, Vec<AgentMessage>>>,
}

impl MemorySessionStore {
    /// 新建空存储。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前会话数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.lock().expect("session store poisoned").len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl SessionStore for MemorySessionStore {
    fn load(&self, session: &str) -> BoxFuture<'static, Option<Vec<AgentMessage>>> {
        let loaded = self
            .sessions
            .lock()
            .expect("session store poisoned")
            .get(session)
            .cloned();
        Box::pin(async move { loaded })
    }

    fn save(&self, session: String, messages: Vec<AgentMessage>) -> BoxFuture<'static, ()> {
        self.sessions
            .lock()
            .expect("session store poisoned")
            .insert(session, messages);
        Box::pin(async {})
    }
}

/// `mpsc` 接收端的 [`Stream`] 包装。
struct ReceiverStream {
    rx: mpsc::UnboundedReceiver<AgentEvent>,
}

impl Stream for ReceiverStream {
    type Item = AgentEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().rx.poll_recv(cx)
    }
}

/// run 未产出任何事件就失败时，补一条失败 turn，保证流里有可观察结果。
fn emit_failure(tx: &mpsc::UnboundedSender<AgentEvent>, model: &Model, error: &AgentError) {
    let message = failure_message(model, &error.to_string(), StopReason::Error);
    let agent_message = AgentMessage::Message(Message::Assistant(Box::new(message)));
    let _ = tx.send(AgentEvent::AgentStart);
    let _ = tx.send(AgentEvent::MessageStart {
        message: agent_message.clone(),
    });
    let _ = tx.send(AgentEvent::MessageEnd {
        message: agent_message.clone(),
    });
    let _ = tx.send(AgentEvent::TurnEnd {
        message: agent_message.clone(),
        tool_results: Vec::new(),
    });
    let _ = tx.send(AgentEvent::AgentEnd {
        messages: vec![agent_message],
    });
}
