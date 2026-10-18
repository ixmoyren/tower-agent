//! Shared JSONL wire protocol for subprocess and dynamic providers.
//!
//! The request is one JSON line. Responses are `SidecarEvent` JSON lines and
//! are assembled into the provider event stream by [`SidecarAssembler`].

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use tower_agent_provider::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, DoneReason, ErrorReason, Message, Model,
    StopReason, StreamRequest, TextContent, ThinkingContent,
};

const PROTOCOL_VERSION: u32 = 1;

/// One provider request sent to a sidecar process or dynamic plugin.
#[derive(Debug, Serialize)]
pub struct SidecarRequest {
    protocol_version: u32,
    model: Model,
    messages: Vec<Message>,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    session_id: Option<String>,
    metadata: Option<serde_json::Value>,
}

impl SidecarRequest {
    /// Convert a provider stream request into its JSONL wire representation.
    #[must_use]
    pub fn from_request(request: &StreamRequest) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            model: request.model.clone(),
            messages: request.context.messages().to_vec(),
            temperature: request.options.base.temperature,
            max_tokens: request.options.base.max_tokens,
            session_id: request.options.base.session_id.clone(),
            metadata: request
                .options
                .base
                .metadata
                .clone()
                .map(serde_json::Value::Object),
        }
    }
}

/// A provider event emitted as one JSON line.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarEvent {
    /// The provider stream has started.
    Start,
    /// Append text to the response.
    TextDelta {
        /// Text fragment to append.
        text: String,
    },
    /// Append reasoning text to the response.
    ThinkingDelta {
        /// Reasoning fragment to append.
        text: String,
    },
    /// Complete the response.
    Done {
        /// Optional complete assistant message that replaces the assembled one.
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<Box<AssistantMessage>>,
        /// Optional final text for the assembled message.
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Fail the response.
    Error {
        /// Human-readable failure reason.
        message: String,
    },
}

impl SidecarEvent {
    /// Encode this event as one newline-terminated JSONL record.
    pub fn to_json_line(&self) -> Result<String, serde_json::Error> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }
}

/// Converts provider JSONL events into the canonical assistant event stream.
pub struct SidecarAssembler {
    partial: AssistantMessage,
    started: bool,
    terminal: bool,
    pending: VecDeque<AssistantMessageEvent>,
}

impl SidecarAssembler {
    /// Create an assembler for the requested model.
    #[must_use]
    pub fn new(model: &Model) -> Self {
        Self {
            partial: empty_message(model),
            started: false,
            terminal: false,
            pending: VecDeque::new(),
        }
    }

    /// Whether a terminal event has been produced or queued.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Remove the next assembled event, if one is ready.
    pub fn pop(&mut self) -> Option<AssistantMessageEvent> {
        self.pending.pop_front()
    }

    /// Apply one wire event and queue the corresponding assistant events.
    pub fn apply(&mut self, event: SidecarEvent) {
        match event {
            SidecarEvent::Start => self.ensure_started(),
            SidecarEvent::TextDelta { text } => {
                self.ensure_started();
                let index = self.ensure_text_block();
                if let Some(ContentBlock::Text(content)) = self.partial.content.get_mut(index) {
                    content.text.push_str(&text);
                }
                self.pending.push_back(AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta: text,
                    partial: self.partial.clone(),
                });
            }
            SidecarEvent::ThinkingDelta { text } => {
                self.ensure_started();
                let index = self.ensure_thinking_block();
                if let Some(ContentBlock::Thinking(content)) = self.partial.content.get_mut(index) {
                    content.thinking.push_str(&text);
                }
                self.pending
                    .push_back(AssistantMessageEvent::ThinkingDelta {
                        content_index: index,
                        delta: text,
                        partial: self.partial.clone(),
                    });
            }
            SidecarEvent::Done { message, text } => {
                self.ensure_started();
                if let Some(text) = text {
                    let index = self.ensure_text_block();
                    if let Some(ContentBlock::Text(content)) = self.partial.content.get_mut(index) {
                        content.text = text;
                    }
                }
                let message = message.map(|message| *message).unwrap_or_else(|| {
                    let mut message = self.partial.clone();
                    message.stop_reason = StopReason::Stop;
                    message
                });
                self.terminal = true;
                if message.stop_reason == StopReason::Error {
                    self.pending.push_back(AssistantMessageEvent::Error {
                        reason: ErrorReason::Error,
                        error: message,
                    });
                } else {
                    let reason = match message.stop_reason {
                        StopReason::Length => DoneReason::Length,
                        StopReason::ToolUse => DoneReason::ToolUse,
                        StopReason::Deferred => DoneReason::Deferred,
                        _ => DoneReason::Stop,
                    };
                    self.pending
                        .push_back(AssistantMessageEvent::Done { reason, message });
                }
            }
            SidecarEvent::Error { message } => self.fail(message),
        }
    }

    /// Fail the response and queue a terminal error event.
    pub fn fail(&mut self, message: String) {
        self.ensure_started();
        self.terminal = true;
        self.partial.stop_reason = StopReason::Error;
        self.partial.error_message = Some(message);
        self.pending.push_back(AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: self.partial.clone(),
        });
    }

    fn ensure_started(&mut self) {
        if !self.started {
            self.started = true;
            self.pending.push_back(AssistantMessageEvent::Start {
                partial: self.partial.clone(),
            });
        }
    }

    fn ensure_text_block(&mut self) -> usize {
        if let Some(index) = self
            .partial
            .content
            .iter()
            .position(|block| matches!(block, ContentBlock::Text(_)))
        {
            return index;
        }
        let index = self.partial.content.len();
        self.partial
            .content
            .push(ContentBlock::Text(TextContent::default()));
        self.pending.push_back(AssistantMessageEvent::TextStart {
            content_index: index,
            partial: self.partial.clone(),
        });
        index
    }

    fn ensure_thinking_block(&mut self) -> usize {
        if let Some(index) = self
            .partial
            .content
            .iter()
            .position(|block| matches!(block, ContentBlock::Thinking(_)))
        {
            return index;
        }
        let index = self.partial.content.len();
        self.partial
            .content
            .push(ContentBlock::Thinking(ThinkingContent::default()));
        self.pending
            .push_back(AssistantMessageEvent::ThinkingStart {
                content_index: index,
                partial: self.partial.clone(),
            });
        index
    }
}

fn empty_message(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        usage: Default::default(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }
}
