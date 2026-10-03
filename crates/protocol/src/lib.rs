//! `protocol` — Rust 对应 pi 的 `packages/protocol`。
//!
//! 三块内容：
//!
//! - [`framing`]：4 字节大端长度前缀的帧编解码（增量、可切分、带上限）。
//! - [`cbor`]：pi 用的**严格 CBOR 子集**（definite-length、无 tag、字符串键、数值在 JS 安全整数范围内）。
//! - [`protocol`] / [`codec`]：RPC 信封（`hello` / `request` / `cancel` / `response` /
//!   `service_update` / `attachment`）的严格校验与编解码，[`PROTOCOL_VERSION`] = 8。
//!
//! 契约（design.md 第 7 节第 8 条）：客户端的第一条帧必须是 `hello`；
//! 服务端可以用 [`ClientHandshake`] 检查。

pub mod cbor;
pub mod codec;
pub mod framing;
pub mod protocol;

pub use cbor::{
    CborError, CborOptions, CborValue, DEFAULT_MAX_CBOR_BYTE_LENGTH,
    DEFAULT_MAX_CBOR_CONTAINER_LENGTH, DEFAULT_MAX_CBOR_DEPTH, MAX_SAFE_INTEGER, MAX_UINT32,
    ResolvedCborOptions, UINT32_BASE, decode_cbor, encode_cbor,
};
pub use codec::{
    ClientMessageDecoder, ServerMessageDecoder, encode_client_message, encode_server_message,
    is_supported_protocol_version, parse_client_message, parse_server_message,
};
pub use framing::{
    DEFAULT_MAX_FRAME_LENGTH, FRAME_HEADER_LENGTH, FrameDecoder, FrameDecoderOptions, FrameError,
    encode_frame,
};
pub use protocol::{
    AttachmentEnvelope, CancelEnvelope, ClientHandshake, ClientHello, ClientMessage,
    PROTOCOL_VERSION, ProtocolError, ProtocolValidationError, RequestEnvelope, ResponseEnvelope,
    RpcTarget, ServerHello, ServerHelloError, ServerId, ServerMessage, ServerTarget,
    ServiceEventEnvelope, SessionTarget, is_server_id,
};
