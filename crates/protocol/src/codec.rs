//! 校验 + 编码 + 增量解码（对应 pi `packages/protocol/src/codec.ts`）。

use serde_json::Value as JsonValue;

use crate::{
    cbor::{CborOptions, CborValue, decode_cbor, encode_cbor},
    framing::{DEFAULT_MAX_FRAME_LENGTH, FrameDecoder, FrameDecoderOptions, encode_frame},
    protocol::{ClientMessage, PROTOCOL_VERSION, ProtocolValidationError, ServerMessage},
};

/// 版本号是否被本实现支持（pi `isSupportedProtocolVersion`）。
pub fn is_supported_protocol_version(version: u64) -> bool {
    version == PROTOCOL_VERSION
}

fn bounded_message(message: &str) -> String {
    if message.chars().count() <= 500 {
        return message.to_string();
    }
    let head: String = message.chars().take(497).collect();
    format!("{head}...")
}

/// 解码出来的 CBOR 值 → JSON（协议载荷必须是 JSON 值，pi `isJsonValue`）。
fn to_message_json(value: &CborValue, kind: &str) -> Result<JsonValue, ProtocolValidationError> {
    value
        .to_json()
        .map_err(|_| ProtocolValidationError::new(format!("Invalid {kind} protocol message")))
}

/// 解析并校验一条客户端消息（pi `parseClientMessage`）。
pub fn parse_client_message(value: &CborValue) -> Result<ClientMessage, ProtocolValidationError> {
    ClientMessage::from_json(&to_message_json(value, "client")?)
}

/// 解析并校验一条服务端消息（pi `parseServerMessage`）。
pub fn parse_server_message(value: &CborValue) -> Result<ServerMessage, ProtocolValidationError> {
    ServerMessage::from_json(&to_message_json(value, "server")?)
}

fn encode_message(
    json: &JsonValue,
    kind: &str,
    options: FrameDecoderOptions,
) -> Result<Vec<u8>, ProtocolValidationError> {
    let max_byte_length = options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH);
    let cbor = encode_cbor(
        &CborValue::from_json(json),
        CborOptions {
            max_byte_length: Some(max_byte_length),
            ..CborOptions::default()
        },
    )
    .map_err(|error| {
        ProtocolValidationError::new(format!(
            "Unable to encode {kind} protocol message: {}",
            bounded_message(error.message())
        ))
    })?;
    encode_frame(&cbor).map_err(|error| {
        ProtocolValidationError::new(format!(
            "Unable to encode {kind} protocol message: {}",
            bounded_message(error.message())
        ))
    })
}

/// 校验并编码一条完整的客户端消息（含长度前缀）。
pub fn encode_client_message(
    message: &ClientMessage,
    options: FrameDecoderOptions,
) -> Result<Vec<u8>, ProtocolValidationError> {
    // 先按 schema 重新解析一遍，保证 encode 与 parse 的校验一致（与 pi 相同）。
    let json = message.to_json();
    parse_client_message(&CborValue::from_json(&json))?;
    encode_message(&json, "client", options)
}

/// 校验并编码一条完整的服务端消息（含长度前缀）。
pub fn encode_server_message(
    message: &ServerMessage,
    options: FrameDecoderOptions,
) -> Result<Vec<u8>, ProtocolValidationError> {
    let json = message.to_json();
    parse_server_message(&CborValue::from_json(&json))?;
    encode_message(&json, "server", options)
}

/// 增量解码 + 校验的通用实现。
#[derive(Debug)]
struct ValidatedMessageDecoder<T> {
    frames: FrameDecoder,
    kind: &'static str,
    max_byte_length: usize,
    failed: bool,
    parse: fn(&CborValue) -> Result<T, ProtocolValidationError>,
}

impl<T> ValidatedMessageDecoder<T> {
    fn new(
        kind: &'static str,
        parse: fn(&CborValue) -> Result<T, ProtocolValidationError>,
        options: FrameDecoderOptions,
    ) -> Result<Self, ProtocolValidationError> {
        Ok(Self {
            frames: FrameDecoder::new(options)
                .map_err(|error| ProtocolValidationError::new(bounded_message(error.message())))?,
            kind,
            max_byte_length: options.max_frame_length.unwrap_or(DEFAULT_MAX_FRAME_LENGTH),
            failed: false,
            parse,
        })
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Vec<T>, ProtocolValidationError> {
        if self.failed {
            return Err(ProtocolValidationError::new(format!(
                "{} message decoder has failed",
                self.kind
            )));
        }
        let frames = match self.frames.push(chunk) {
            Ok(frames) => frames,
            Err(error) => {
                self.failed = true;
                return Err(ProtocolValidationError::new(format!(
                    "Invalid {} protocol frame: {}",
                    self.kind,
                    bounded_message(error.message())
                )));
            }
        };
        let mut messages = Vec::with_capacity(frames.len());
        for frame in frames {
            let value = match decode_cbor(
                &frame,
                CborOptions {
                    max_byte_length: Some(self.max_byte_length),
                    ..CborOptions::default()
                },
            ) {
                Ok(value) => value,
                Err(error) => {
                    self.failed = true;
                    return Err(ProtocolValidationError::new(format!(
                        "Invalid {} protocol frame: {}",
                        self.kind,
                        bounded_message(error.message())
                    )));
                }
            };
            match (self.parse)(&value) {
                Ok(message) => messages.push(message),
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            }
        }
        Ok(messages)
    }

    fn end(&mut self) -> Result<(), ProtocolValidationError> {
        if self.failed {
            return Err(ProtocolValidationError::new(format!(
                "{} message decoder has failed",
                self.kind
            )));
        }
        if let Err(error) = self.frames.end() {
            self.failed = true;
            return Err(ProtocolValidationError::new(format!(
                "Invalid {} protocol framing: {}",
                self.kind,
                bounded_message(error.message())
            )));
        }
        Ok(())
    }
}

/// 增量解码并校验客户端消息（pi `ClientMessageDecoder`）。
#[derive(Debug)]
pub struct ClientMessageDecoder {
    inner: ValidatedMessageDecoder<ClientMessage>,
}

impl ClientMessageDecoder {
    /// 用给定选项构造。
    pub fn new(options: FrameDecoderOptions) -> Result<Self, ProtocolValidationError> {
        Ok(Self {
            inner: ValidatedMessageDecoder::new("client", parse_client_message, options)?,
        })
    }

    /// 推入一段字节，返回这次解出的完整消息。
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ClientMessage>, ProtocolValidationError> {
        self.inner.push(chunk)
    }

    /// 结束解码。
    pub fn end(&mut self) -> Result<(), ProtocolValidationError> {
        self.inner.end()
    }
}

/// 增量解码并校验服务端消息（pi `ServerMessageDecoder`）。
#[derive(Debug)]
pub struct ServerMessageDecoder {
    inner: ValidatedMessageDecoder<ServerMessage>,
}

impl ServerMessageDecoder {
    /// 用给定选项构造。
    pub fn new(options: FrameDecoderOptions) -> Result<Self, ProtocolValidationError> {
        Ok(Self {
            inner: ValidatedMessageDecoder::new("server", parse_server_message, options)?,
        })
    }

    /// 推入一段字节，返回这次解出的完整消息。
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ServerMessage>, ProtocolValidationError> {
        self.inner.push(chunk)
    }

    /// 结束解码。
    pub fn end(&mut self) -> Result<(), ProtocolValidationError> {
        self.inner.end()
    }
}
