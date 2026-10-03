//! agent 事件协议与 agent 消息（pi `AgentEvent` / `AgentMessage`）。

use pi_ai::{AssistantMessageEvent, JsonValue, Message, ToolResultMessage};

use crate::AgentToolResult;

/// agent 消息：pi 的 LLM 消息加上应用自定义消息扩展点（pi `AgentMessage`）。
///
/// pi 里 `AgentMessage = Message | CustomAgentMessages[keyof CustomAgentMessages]`，
/// 应用通过 declaration merging 增加自定义消息。Rust 没有等价的开放联合，因此
/// 退化为不透明 JSON 值；自定义消息应是带 `role` 字段的 JSON 对象，才能在编排层
/// 参与按 role 的判断（例如插入 system 消息、检查最后一条消息的角色）。
#[derive(Debug, Clone, PartialEq)]
pub enum AgentMessage {
    /// pi 基础 LLM 消息。
    Message(Message),
    /// 应用自定义消息（不透明 JSON）。
    Custom(JsonValue),
}

impl AgentMessage {
    /// 用一条 LLM 消息构造。
    pub fn message(message: Message) -> Self {
        Self::Message(message)
    }

    /// 用一条应用自定义消息构造。
    pub fn custom(value: JsonValue) -> Self {
        Self::Custom(value)
    }

    /// 消息角色（pi `message.role`）。
    ///
    /// 基础消息返回 `"system"` / `"user"` / `"assistant"` / `"toolResult"`；
    /// 自定义消息返回其 JSON `role` 字段（缺失或非字符串时为 `None`）。
    pub fn role(&self) -> Option<&str> {
        match self {
            Self::Message(Message::System(_)) => Some("system"),
            Self::Message(Message::User(_)) => Some("user"),
            Self::Message(Message::Assistant(_)) => Some("assistant"),
            Self::Message(Message::ToolResult(_)) => Some("toolResult"),
            Self::Custom(value) => value.get("role").and_then(JsonValue::as_str),
        }
    }
}

impl From<Message> for AgentMessage {
    fn from(message: Message) -> Self {
        Self::Message(message)
    }
}

/// agent 对订阅者发出的事件（pi `AgentEvent`）。
///
/// 事件是 push、多订阅者的，退出通用 tower 中间件模型（design.md D1/D3）。
/// `agent_end` 是一次运行的最后一个事件，但被 await 的 listener 仍在运行结算范围内：
/// agent 只有在这些 listener 完成后才进入 idle。
///
/// 变体尺寸差异来自 wire 协议本身（`message_update` 同时携带消息与 provider 事件）；
/// 这里保持与 pi 事件形状一一对应，不为省栈空间重新装箱。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// 一次运行开始。
    AgentStart,
    /// 一次运行结束，携带本次运行产生的新消息。
    AgentEnd {
        /// 本次运行新增的消息（按产生顺序）。
        messages: Vec<AgentMessage>,
    },
    /// 一个 turn 开始：一次 assistant 响应加上它引发的工具调用/结果。
    TurnStart,
    /// 一个 turn 结束。
    TurnEnd {
        /// 本 turn 的 assistant 消息。
        message: AgentMessage,
        /// 本 turn 产生的工具结果消息。
        tool_results: Vec<ToolResultMessage>,
    },
    /// 任意消息（system/user/assistant/toolResult）开始。
    MessageStart {
        /// 开始的消息。
        message: AgentMessage,
    },
    /// assistant 消息流式更新（仅 assistant 消息在流式期间发出）。
    MessageUpdate {
        /// 截至目前为止的消息视图。
        message: AgentMessage,
        /// 底层 provider 流事件。
        assistant_message_event: AssistantMessageEvent,
    },
    /// 任意消息结束。
    MessageEnd {
        /// 结束的消息。
        message: AgentMessage,
    },
    /// 一个工具开始执行。
    ToolExecutionStart {
        /// 对应的工具调用 id。
        tool_call_id: String,
        /// 工具名。
        tool_name: String,
        /// 模型给出的调用参数。
        args: JsonValue,
    },
    /// 工具发出一条部分结果（pi `onUpdate` 回调）。
    ToolExecutionUpdate {
        /// 对应的工具调用 id。
        tool_call_id: String,
        /// 工具名。
        tool_name: String,
        /// 模型给出的调用参数。
        args: JsonValue,
        /// 本次部分结果。
        partial_result: AgentToolResult,
    },
    /// 一个工具定稿（pi `tool_execution_end`）。
    ///
    /// 并行执行时按**完成顺序**发出；随后按 assistant 源顺序发出工具结果消息事件。
    ToolExecutionEnd {
        /// 对应的工具调用 id。
        tool_call_id: String,
        /// 工具名。
        tool_name: String,
        /// 终态结果。
        result: AgentToolResult,
        /// 该结果是否表示错误（pi `isError`）。
        is_error: bool,
    },
}

impl AgentEvent {
    /// pi 的 wire 事件 `type` 字符串。
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::AgentStart => "agent_start",
            Self::AgentEnd { .. } => "agent_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd { .. } => "turn_end",
            Self::MessageStart { .. } => "message_start",
            Self::MessageUpdate { .. } => "message_update",
            Self::MessageEnd { .. } => "message_end",
            Self::ToolExecutionStart { .. } => "tool_execution_start",
            Self::ToolExecutionUpdate { .. } => "tool_execution_update",
            Self::ToolExecutionEnd { .. } => "tool_execution_end",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use pi_ai::{
        AssistantMessage, AssistantMessageEvent, Message, StopReason, ToolCall, ToolResultMessage,
        Usage, UserContent, UserMessage,
    };
    use serde_json::json;

    use super::{AgentEvent, AgentMessage};
    use crate::AgentToolResult;

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::message(Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: 0,
        }))
    }

    fn assistant_message() -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }
    }

    fn tool_call() -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "echo".into(),
            arguments: Default::default(),
            thought_signature: None,
            namespace: None,
        }
    }

    fn tool_result() -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: "call-1".into(),
            tool_name: "echo".into(),
            content: Vec::new(),
            details: None,
            is_error: false,
            timestamp: 0,
        }
    }

    #[test]
    fn base_message_roles_match_wire_strings() {
        assert_eq!(user_message("hi").role(), Some("user"));
        assert_eq!(
            AgentMessage::message(Message::Assistant(Box::new(assistant_message()))).role(),
            Some("assistant")
        );
        assert_eq!(
            AgentMessage::message(Message::System(pi_ai::SystemMessage {
                content: Vec::new(),
                sections: BTreeMap::new(),
                tools_added: Vec::new(),
                tools_removed: Vec::new(),
                timestamp: 0,
            }))
            .role(),
            Some("system")
        );
        assert_eq!(
            AgentMessage::message(Message::ToolResult(tool_result())).role(),
            Some("toolResult")
        );
    }

    #[test]
    fn custom_message_role_reads_json_field() {
        assert_eq!(
            AgentMessage::custom(json!({ "role": "artifact" })).role(),
            Some("artifact")
        );
        assert_eq!(AgentMessage::custom(json!({})).role(), None);
        assert_eq!(AgentMessage::custom(json!({ "role": 7 })).role(), None);
    }

    #[test]
    fn event_kinds_match_wire_strings() {
        let message = user_message("hi");
        let partial = assistant_message();
        let call = tool_call();
        let result = AgentToolResult::text("ok");
        let events = [
            (AgentEvent::AgentStart, "agent_start"),
            (
                AgentEvent::AgentEnd {
                    messages: vec![message.clone()],
                },
                "agent_end",
            ),
            (AgentEvent::TurnStart, "turn_start"),
            (
                AgentEvent::TurnEnd {
                    message: message.clone(),
                    tool_results: vec![tool_result()],
                },
                "turn_end",
            ),
            (
                AgentEvent::MessageStart {
                    message: message.clone(),
                },
                "message_start",
            ),
            (
                AgentEvent::MessageUpdate {
                    message: AgentMessage::message(Message::Assistant(Box::new(partial.clone()))),
                    assistant_message_event: AssistantMessageEvent::Start { partial },
                },
                "message_update",
            ),
            (
                AgentEvent::MessageEnd {
                    message: message.clone(),
                },
                "message_end",
            ),
            (
                AgentEvent::ToolExecutionStart {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    args: json!({}),
                },
                "tool_execution_start",
            ),
            (
                AgentEvent::ToolExecutionUpdate {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    args: json!({}),
                    partial_result: result.clone(),
                },
                "tool_execution_update",
            ),
            (
                AgentEvent::ToolExecutionEnd {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    result,
                    is_error: false,
                },
                "tool_execution_end",
            ),
        ];

        for (event, expected) in &events {
            assert_eq!(event.kind(), *expected);
        }
    }
}
