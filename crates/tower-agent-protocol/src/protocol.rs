//! RPC 信封（对应 pi `packages/protocol/src/protocol.ts`）。
//!
//! 客户端消息：`hello` / `request` / `cancel`；服务端消息：`hello` / `hello_error` /
//! `response` / `service_update` / `attachment`。所有对象都是**严格**的（多字段即非法），
//! 字段形状与 pi 的 typebox schema 一一对应。
//!
//! 契约：**客户端的第一条帧必须是 `hello`**（见 design.md 第 7 节第 8 条），
//! 服务端可以用 [`ClientHandshake`] 检查这一点。

use serde_json::Value as JsonValue;

use crate::cbor::CborError;

/// 协议版本（pi `PROTOCOL_VERSION`）。
pub const PROTOCOL_VERSION: u64 = 8;

/// 协议校验错误（pi `ProtocolValidationError`）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ProtocolValidationError {
    message: String,
}

impl ProtocolValidationError {
    /// 构造一个校验错误。
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 错误信息（与 pi 的文案一致）。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<CborError> for ProtocolValidationError {
    fn from(error: CborError) -> Self {
        Self::new(error.message())
    }
}

/// 服务端标识：UUID v4（pi `ServerIdSchema`）。
pub type ServerId = String;

/// 判断是否是合法的 serverId（UUID v4 小写十六进制形式）。
pub fn is_server_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        match index {
            8 | 13 | 18 | 23 => {
                if *byte != b'-' {
                    return false;
                }
            }
            14 => {
                if *byte != b'4' {
                    return false;
                }
            }
            19 => {
                if !matches!(*byte, b'8' | b'9' | b'a' | b'b') {
                    return false;
                }
            }
            _ => {
                if !byte.is_ascii_digit() && !(b'a'..=b'f').contains(byte) {
                    return false;
                }
            }
        }
    }
    true
}

/// 协议错误载荷（pi `ProtocolError`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    /// 机器可读的错误码（非空字符串）。
    pub code: String,
    /// 人能读的错误信息。
    pub message: String,
}

/// 面向整个逻辑服务端的调用目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerTarget {
    /// 目标服务端。
    pub server_id: ServerId,
}

/// 面向某个会话（服务端 + 持久会话 + 当前 attachment）的调用目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTarget {
    /// 目标服务端。
    pub server_id: ServerId,
    /// 会话标识。
    pub session_id: String,
    /// 当前 attachment 标识。
    pub attachment_id: String,
}

/// RPC 目标（pi `RpcTarget`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcTarget {
    /// 服务端级目标。
    Server(ServerTarget),
    /// 会话级目标。
    Session(SessionTarget),
}

/// 客户端握手的首帧（pi `ClientHello`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    /// 客户端支持的协议版本（>= 0）。
    pub version: u64,
}

/// 一次调用（pi `RequestEnvelope`）。
#[derive(Debug, Clone, PartialEq)]
pub struct RequestEnvelope {
    /// 调用 id（非空，响应按它配对）。
    pub id: String,
    /// 调用目标。
    pub target: RpcTarget,
    /// 调用载荷（不透明 JSON）。
    pub call: JsonValue,
}

/// 取消一次调用（pi `CancelEnvelope`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelEnvelope {
    /// 被取消的调用 id。
    pub id: String,
    /// 调用目标。
    pub target: RpcTarget,
}

/// 客户端消息（pi `ClientMessage`）。
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    /// 握手首帧。
    Hello(ClientHello),
    /// 调用。
    Request(RequestEnvelope),
    /// 取消。
    Cancel(CancelEnvelope),
}

/// 服务端握手成功（pi `ServerHello`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    /// 服务端协议版本，必须等于 [`PROTOCOL_VERSION`]。
    pub server_id: ServerId,
}

/// 服务端拒绝握手（pi `ServerHelloError`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHelloError {
    /// 拒绝原因。
    pub error: ProtocolError,
}

/// 调用响应（pi `ResponseEnvelope`）。
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseEnvelope {
    /// 成功，`result` 为可选的 JSON 载荷。
    Ok {
        /// 对应的调用 id。
        id: String,
        /// 调用结果。
        result: Option<JsonValue>,
    },
    /// 失败。
    Err {
        /// 对应的调用 id。
        id: String,
        /// 错误。
        error: ProtocolError,
    },
}

/// 订阅更新（pi `ServiceEventEnvelope`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceEventEnvelope {
    /// 订阅 id。
    pub subscription_id: String,
    /// 更新载荷。
    pub update: JsonValue,
}

/// 当前 presentation 选中的 session 路由（pi `AttachmentEnvelope`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentEnvelope {
    /// `None` 表示清空选择；`Some` 表示切到该会话。
    pub attachment: Option<SessionTarget>,
}

/// 服务端消息（pi `ServerMessage`）。
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    /// 握手成功。
    Hello(ServerHello),
    /// 握手失败。
    HelloError(ServerHelloError),
    /// 调用响应。
    Response(ResponseEnvelope),
    /// 订阅更新。
    ServiceUpdate(ServiceEventEnvelope),
    /// 会话路由变更。
    Attachment(AttachmentEnvelope),
}

/// 服务端用来检查「第一条客户端帧必须是 hello」（见 design.md 第 7 节第 8 条）。
#[derive(Debug, Default)]
pub struct ClientHandshake {
    greeted: bool,
}

impl ClientHandshake {
    /// 检查下一条客户端消息的到达顺序。
    pub fn accept(&mut self, message: &ClientMessage) -> Result<(), ProtocolValidationError> {
        if self.greeted {
            return Ok(());
        }
        if !matches!(message, ClientMessage::Hello(_)) {
            return Err(ProtocolValidationError::new(
                "The first client frame must be `hello`",
            ));
        }
        self.greeted = true;
        Ok(())
    }

    /// 是否已经收到过 `hello`。
    pub fn greeted(&self) -> bool {
        self.greeted
    }
}

// ---------- JSON ↔ 信封 ----------

/// 非空字符串字段（pi `IdSchema`）。
fn id(value: &JsonValue, field: &str) -> Result<String, ProtocolValidationError> {
    match value {
        JsonValue::String(text) if !text.is_empty() => Ok(text.clone()),
        _ => Err(ProtocolValidationError::new(format!(
            "`{field}` must be a non-empty string"
        ))),
    }
}

fn object<'a>(
    value: &'a JsonValue,
    field: &str,
) -> Result<&'a serde_json::Map<String, JsonValue>, ProtocolValidationError> {
    match value {
        JsonValue::Object(entries) => Ok(entries),
        _ => Err(ProtocolValidationError::new(format!(
            "`{field}` must be an object"
        ))),
    }
}

/// 严格对象：字段必须恰好是 `expected`（pi 的 `additionalProperties: false`）。
fn strict(
    value: &JsonValue,
    expected: &[&str],
    field: &str,
) -> Result<(), ProtocolValidationError> {
    let entries = object(value, field)?;
    for key in entries.keys() {
        if !expected.contains(&key.as_str()) {
            return Err(ProtocolValidationError::new(format!(
                "`{field}` has unexpected field `{key}`"
            )));
        }
    }
    Ok(())
}

fn protocol_error(value: &JsonValue) -> Result<ProtocolError, ProtocolValidationError> {
    strict(value, &["code", "message"], "error")?;
    let entries = object(value, "error")?;
    Ok(ProtocolError {
        code: id(
            entries.get("code").unwrap_or(&JsonValue::Null),
            "error.code",
        )?,
        message: match entries.get("message") {
            Some(JsonValue::String(text)) => text.clone(),
            _ => {
                return Err(ProtocolValidationError::new(
                    "`error.message` must be a string",
                ));
            }
        },
    })
}

/// 解析目标（server 级或 session 级）。
pub fn parse_rpc_target(value: &JsonValue) -> Result<RpcTarget, ProtocolValidationError> {
    let entries = object(value, "target")?;
    let has_session = entries.contains_key("sessionId") || entries.contains_key("attachmentId");
    if has_session {
        strict(value, &["serverId", "sessionId", "attachmentId"], "target")?;
        let server_id = id(
            entries.get("serverId").unwrap_or(&JsonValue::Null),
            "serverId",
        )?;
        if !is_server_id(&server_id) {
            return Err(ProtocolValidationError::new("`serverId` must be a UUID v4"));
        }
        Ok(RpcTarget::Session(SessionTarget {
            server_id,
            session_id: id(
                entries.get("sessionId").unwrap_or(&JsonValue::Null),
                "sessionId",
            )?,
            attachment_id: id(
                entries.get("attachmentId").unwrap_or(&JsonValue::Null),
                "attachmentId",
            )?,
        }))
    } else {
        strict(value, &["serverId"], "target")?;
        let server_id = id(
            entries.get("serverId").unwrap_or(&JsonValue::Null),
            "serverId",
        )?;
        if !is_server_id(&server_id) {
            return Err(ProtocolValidationError::new("`serverId` must be a UUID v4"));
        }
        Ok(RpcTarget::Server(ServerTarget { server_id }))
    }
}

/// 把目标编码成 JSON。
pub fn target_to_json(target: &RpcTarget) -> JsonValue {
    match target {
        RpcTarget::Server(server) => serde_json::json!({ "serverId": server.server_id }),
        RpcTarget::Session(session) => serde_json::json!({
            "serverId": session.server_id,
            "sessionId": session.session_id,
            "attachmentId": session.attachment_id,
        }),
    }
}

/// 把错误编码成 JSON。
pub fn protocol_error_to_json(error: &ProtocolError) -> JsonValue {
    serde_json::json!({ "code": error.code, "message": error.message })
}

impl ClientMessage {
    /// 从 JSON 值解析并校验客户端消息。
    pub fn from_json(value: &JsonValue) -> Result<Self, ProtocolValidationError> {
        let entries = object(value, "message")?;
        let kind = match entries.get("type") {
            Some(JsonValue::String(kind)) => kind.clone(),
            _ => {
                return Err(ProtocolValidationError::new("`type` must be a string"));
            }
        };
        match kind.as_str() {
            "hello" => {
                strict(value, &["type", "version"], "hello")?;
                let version = match entries.get("version") {
                    Some(JsonValue::Number(number)) => number.as_u64().ok_or_else(|| {
                        ProtocolValidationError::new("`version` must be a non-negative integer")
                    })?,
                    _ => {
                        return Err(ProtocolValidationError::new(
                            "`version` must be a non-negative integer",
                        ));
                    }
                };
                Ok(Self::Hello(ClientHello { version }))
            }
            "request" => {
                strict(value, &["type", "id", "target", "call"], "request")?;
                Ok(Self::Request(RequestEnvelope {
                    id: id(entries.get("id").unwrap_or(&JsonValue::Null), "id")?,
                    target: parse_rpc_target(entries.get("target").unwrap_or(&JsonValue::Null))?,
                    call: entries.get("call").cloned().unwrap_or(JsonValue::Null),
                }))
            }
            "cancel" => {
                strict(value, &["type", "id", "target"], "cancel")?;
                Ok(Self::Cancel(CancelEnvelope {
                    id: id(entries.get("id").unwrap_or(&JsonValue::Null), "id")?,
                    target: parse_rpc_target(entries.get("target").unwrap_or(&JsonValue::Null))?,
                }))
            }
            other => Err(ProtocolValidationError::new(format!(
                "unknown client message type `{other}`"
            ))),
        }
    }

    /// 编码成 JSON 值。
    pub fn to_json(&self) -> JsonValue {
        match self {
            Self::Hello(hello) => serde_json::json!({ "type": "hello", "version": hello.version }),
            Self::Request(request) => serde_json::json!({
                "type": "request",
                "id": request.id,
                "target": target_to_json(&request.target),
                "call": request.call,
            }),
            Self::Cancel(cancel) => serde_json::json!({
                "type": "cancel",
                "id": cancel.id,
                "target": target_to_json(&cancel.target),
            }),
        }
    }
}

impl ServerMessage {
    /// 从 JSON 值解析并校验服务端消息。
    pub fn from_json(value: &JsonValue) -> Result<Self, ProtocolValidationError> {
        let entries = object(value, "message")?;
        let kind = match entries.get("type") {
            Some(JsonValue::String(kind)) => kind.clone(),
            _ => return Err(ProtocolValidationError::new("`type` must be a string")),
        };
        match kind.as_str() {
            "hello" => {
                strict(value, &["type", "version", "serverId"], "hello")?;
                match entries.get("version") {
                    Some(JsonValue::Number(number))
                        if number.as_u64() == Some(PROTOCOL_VERSION) => {}
                    _ => {
                        return Err(ProtocolValidationError::new(format!(
                            "`version` must be {PROTOCOL_VERSION}"
                        )));
                    }
                }
                let server_id = id(
                    entries.get("serverId").unwrap_or(&JsonValue::Null),
                    "serverId",
                )?;
                if !is_server_id(&server_id) {
                    return Err(ProtocolValidationError::new("`serverId` must be a UUID v4"));
                }
                Ok(Self::Hello(ServerHello { server_id }))
            }
            "hello_error" => {
                strict(value, &["type", "error"], "hello_error")?;
                Ok(Self::HelloError(ServerHelloError {
                    error: protocol_error(entries.get("error").unwrap_or(&JsonValue::Null))?,
                }))
            }
            "response" => {
                let ok = match entries.get("ok") {
                    Some(JsonValue::Bool(ok)) => *ok,
                    _ => return Err(ProtocolValidationError::new("`ok` must be a boolean")),
                };
                if ok {
                    strict(value, &["type", "id", "ok", "result"], "response")?;
                    Ok(Self::Response(ResponseEnvelope::Ok {
                        id: id(entries.get("id").unwrap_or(&JsonValue::Null), "id")?,
                        result: entries.get("result").cloned(),
                    }))
                } else {
                    strict(value, &["type", "id", "ok", "error"], "response")?;
                    Ok(Self::Response(ResponseEnvelope::Err {
                        id: id(entries.get("id").unwrap_or(&JsonValue::Null), "id")?,
                        error: protocol_error(entries.get("error").unwrap_or(&JsonValue::Null))?,
                    }))
                }
            }
            "service_update" => {
                strict(
                    value,
                    &["type", "subscriptionId", "update"],
                    "service_update",
                )?;
                Ok(Self::ServiceUpdate(ServiceEventEnvelope {
                    subscription_id: id(
                        entries.get("subscriptionId").unwrap_or(&JsonValue::Null),
                        "subscriptionId",
                    )?,
                    update: entries.get("update").cloned().unwrap_or(JsonValue::Null),
                }))
            }
            "attachment" => {
                strict(value, &["type", "attachment"], "attachment")?;
                let attachment = match entries.get("attachment") {
                    None | Some(JsonValue::Null) => None,
                    Some(value) => Some(match parse_rpc_target(value)? {
                        RpcTarget::Session(session) => session,
                        RpcTarget::Server(_) => {
                            return Err(ProtocolValidationError::new(
                                "`attachment` must be a session target or null",
                            ));
                        }
                    }),
                };
                Ok(Self::Attachment(AttachmentEnvelope { attachment }))
            }
            other => Err(ProtocolValidationError::new(format!(
                "unknown server message type `{other}`"
            ))),
        }
    }

    /// 编码成 JSON 值。
    pub fn to_json(&self) -> JsonValue {
        match self {
            Self::Hello(hello) => serde_json::json!({
                "type": "hello",
                "version": PROTOCOL_VERSION,
                "serverId": hello.server_id,
            }),
            Self::HelloError(error) => serde_json::json!({
                "type": "hello_error",
                "error": protocol_error_to_json(&error.error),
            }),
            Self::Response(ResponseEnvelope::Ok { id, result }) => match result {
                Some(result) => {
                    serde_json::json!({ "type": "response", "id": id, "ok": true, "result": result })
                }
                None => serde_json::json!({ "type": "response", "id": id, "ok": true }),
            },
            Self::Response(ResponseEnvelope::Err { id, error }) => serde_json::json!({
                "type": "response",
                "id": id,
                "ok": false,
                "error": protocol_error_to_json(error),
            }),
            Self::ServiceUpdate(update) => serde_json::json!({
                "type": "service_update",
                "subscriptionId": update.subscription_id,
                "update": update.update,
            }),
            Self::Attachment(attachment) => serde_json::json!({
                "type": "attachment",
                "attachment": attachment.attachment.as_ref().map(|session| {
                    serde_json::json!({
                        "serverId": session.server_id,
                        "sessionId": session.session_id,
                        "attachmentId": session.attachment_id,
                    })
                }),
            }),
        }
    }
}
