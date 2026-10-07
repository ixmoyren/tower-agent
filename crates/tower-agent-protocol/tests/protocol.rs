//! 信封校验与编解码测试（对应 pi `test/protocol.test.ts`）。

use serde_json::json;
use tower_agent_protocol::{
    CborValue, ClientHandshake, ClientHello, ClientMessage, ClientMessageDecoder,
    FrameDecoderOptions, PROTOCOL_VERSION, ProtocolValidationError, ResponseEnvelope, RpcTarget,
    ServerHello, ServerMessage, ServerMessageDecoder, ServerTarget, ServiceEventEnvelope,
    SessionTarget, encode_client_message, encode_server_message, is_server_id,
    is_supported_protocol_version, parse_client_message, parse_server_message,
};

const SERVER_ID: &str = "00000000-0000-4000-8000-000000000001";

fn client_hello() -> ClientMessage {
    ClientMessage::Hello(ClientHello {
        version: PROTOCOL_VERSION,
    })
}

fn request() -> ClientMessage {
    ClientMessage::Request(tower_agent_protocol::RequestEnvelope {
        id: "request-1".into(),
        target: RpcTarget::Session(SessionTarget {
            server_id: SERVER_ID.into(),
            session_id: "session-1".into(),
            attachment_id: "attachment-1".into(),
        }),
        call: json!({
            "serviceId": "application.custom",
            "member": "invoke",
            "args": [{ "arbitrary": true }, ["opaque"]],
        }),
    })
}

fn parse_client_json(value: serde_json::Value) -> Result<ClientMessage, ProtocolValidationError> {
    parse_client_message(&CborValue::from_json(&value))
}

fn parse_server_json(value: serde_json::Value) -> Result<ServerMessage, ProtocolValidationError> {
    parse_server_message(&CborValue::from_json(&value))
}

#[test]
fn negotiates_protocol_version_8() {
    assert_eq!(PROTOCOL_VERSION, 8);
    assert!(is_supported_protocol_version(8));
    assert!(!is_supported_protocol_version(7));
}

#[test]
fn accepts_integer_client_hello_versions_for_negotiation() {
    for version in [0, PROTOCOL_VERSION, PROTOCOL_VERSION + 1] {
        let decoded =
            parse_client_json(json!({ "type": "hello", "version": version })).expect("valid");
        assert_eq!(decoded, ClientMessage::Hello(ClientHello { version }));
    }
}

#[test]
fn rejects_invalid_client_hello() {
    for message in [
        json!({ "type": "hello", "version": "8" }),
        json!({ "type": "hello", "version": 8.5 }),
        json!({ "type": "hello", "version": 8, "extra": true }),
        json!({ "type": "hello" }),
    ] {
        assert!(
            parse_client_json(message.clone()).is_err(),
            "should reject {message}"
        );
    }
}

#[test]
fn validates_server_ids_as_canonical_uuid_v4() {
    assert!(is_server_id(SERVER_ID));
    for value in [
        "",
        "server-1",
        "00000000-0000-7000-8000-000000000001", // 版本位不是 4
        "00000000-0000-4000-7000-000000000001", // variant 位不对
        "00000000-0000-4000-8000-00000000000A", // 大写
        "00000000-0000-4000-8000-00000000001",  // 长度不对
    ] {
        assert!(!is_server_id(value), "should reject {value}");
        assert!(
            parse_client_json(json!({
                "type": "request",
                "id": "request-1",
                "target": { "serverId": value },
                "call": {},
            }))
                .is_err(),
            "request with serverId {value} should fail"
        );
    }
}

#[test]
fn keeps_routed_payloads_opaque_and_strict() {
    let message = request();
    let json = message.to_json();
    assert_eq!(parse_client_json(json.clone()).expect("valid"), message);

    // `call` 的内容不解释，但对象字段仍然严格。
    let mut with_extra = json.clone();
    with_extra["extra"] = json!(true);
    assert!(parse_client_json(with_extra).is_err());

    // 目标对象也严格：多一个字段就非法。
    assert!(
        parse_client_json(json!({
            "type": "request",
            "id": "request-1",
            "target": { "serverId": SERVER_ID, "extra": 1 },
            "call": {},
        }))
            .is_err()
    );

    // session 目标必须三个字段齐全。
    assert!(
        parse_client_json(json!({
            "type": "request",
            "id": "request-1",
            "target": { "serverId": SERVER_ID, "sessionId": "session-1" },
            "call": {},
        }))
            .is_err()
    );
}

#[test]
fn rejects_non_json_payloads() {
    // CBOR 字节串不是 JSON 值（pi 的 `isJsonValue` 检查）。
    let value = CborValue::Map(vec![
        ("type".into(), CborValue::Text("request".into())),
        ("id".into(), CborValue::Text("request-1".into())),
        (
            "target".into(),
            CborValue::Map(vec![("serverId".into(), CborValue::Text(SERVER_ID.into()))]),
        ),
        (
            "call".into(),
            CborValue::Array(vec![CborValue::Bytes(vec![1])]),
        ),
    ]);
    let error = parse_client_message(&value).expect_err("bytes are not JSON");
    assert_eq!(error.message(), "Invalid client protocol message");

    // 服务端响应里的 result 同理。
    let value = CborValue::Map(vec![
        ("type".into(), CborValue::Text("response".into())),
        ("id".into(), CborValue::Text("request-1".into())),
        ("ok".into(), CborValue::Bool(true)),
        ("result".into(), CborValue::Bytes(vec![1])),
    ]);
    assert!(parse_server_message(&value).is_err());
}

#[test]
fn round_trips_client_messages_through_the_framed_codec() {
    for message in [client_hello(), request()] {
        let wire = encode_client_message(&message, FrameDecoderOptions::default()).expect("encode");
        let mut decoder =
            ClientMessageDecoder::new(FrameDecoderOptions::default()).expect("decoder");
        // 逐字节喂入，验证增量解码。
        let mut decoded = Vec::new();
        for byte in &wire {
            decoded.extend(decoder.push(&[*byte]).expect("push"));
        }
        decoder.end().expect("end");
        assert_eq!(decoded, vec![message]);
    }
}

#[test]
fn round_trips_every_server_message_shape() {
    let messages = vec![
        ServerMessage::Hello(ServerHello {
            server_id: SERVER_ID.into(),
        }),
        ServerMessage::HelloError(tower_agent_protocol::ServerHelloError {
            error: tower_agent_protocol::ProtocolError {
                code: "unsupported_version".into(),
                message: "版本不匹配".into(),
            },
        }),
        ServerMessage::Response(ResponseEnvelope::Ok {
            id: "request-1".into(),
            result: Some(json!({ "ok": true })),
        }),
        ServerMessage::Response(ResponseEnvelope::Ok {
            id: "request-1".into(),
            result: None,
        }),
        ServerMessage::Response(ResponseEnvelope::Err {
            id: "request-1".into(),
            error: tower_agent_protocol::ProtocolError {
                code: "boom".into(),
                message: "出错了".into(),
            },
        }),
        ServerMessage::ServiceUpdate(ServiceEventEnvelope {
            subscription_id: "subscription-1".into(),
            update: json!({ "applicationDefined": true }),
        }),
        ServerMessage::Attachment(tower_agent_protocol::AttachmentEnvelope {
            attachment: Some(SessionTarget {
                server_id: SERVER_ID.into(),
                session_id: "session-1".into(),
                attachment_id: "attachment-1".into(),
            }),
        }),
        ServerMessage::Attachment(tower_agent_protocol::AttachmentEnvelope { attachment: None }),
    ];

    for message in messages {
        let wire = encode_server_message(&message, FrameDecoderOptions::default()).expect("encode");
        let mut decoder =
            ServerMessageDecoder::new(FrameDecoderOptions::default()).expect("decoder");
        let decoded = decoder.push(&wire).expect("push");
        decoder.end().expect("end");
        assert_eq!(decoded, vec![message]);
    }
}

#[test]
fn server_hello_must_use_the_current_version_and_a_uuid() {
    assert!(
        parse_server_json(json!({ "type": "hello", "version": 8, "serverId": SERVER_ID })).is_ok()
    );
    assert!(
        parse_server_json(json!({ "type": "hello", "version": 7, "serverId": SERVER_ID })).is_err()
    );
    assert!(
        parse_server_json(json!({ "type": "hello", "version": 8, "serverId": "nope" })).is_err()
    );
    assert!(
        parse_server_json(
            json!({ "type": "hello", "version": 8, "serverId": SERVER_ID, "extra": 1 })
        )
            .is_err()
    );
}

#[test]
fn attachment_must_be_a_session_target_or_null() {
    let value = json!({
        "type": "attachment",
        "attachment": { "serverId": SERVER_ID, "sessionId": "s", "attachmentId": "a" },
    });
    assert!(parse_server_json(value).is_ok());
    assert!(parse_server_json(json!({ "type": "attachment", "attachment": null })).is_ok());
    // server 级目标不能当 attachment。
    assert!(
        parse_server_json(json!({ "type": "attachment", "attachment": { "serverId": SERVER_ID } }))
            .is_err()
    );
}

#[test]
fn the_first_client_frame_must_be_hello() {
    let mut handshake = ClientHandshake::default();
    assert!(!handshake.greeted());
    let error = handshake.accept(&request()).expect_err("must greet first");
    assert!(error.message().contains("must be `hello`"), "got: {error}");
    assert!(!handshake.greeted());

    handshake.accept(&client_hello()).expect("hello accepted");
    assert!(handshake.greeted());
    // 之后的消息不再受限。
    handshake.accept(&request()).expect("request after hello");
}

#[test]
fn decoder_fails_closed_after_a_bad_frame() {
    let mut decoder = ClientMessageDecoder::new(FrameDecoderOptions::default()).expect("decoder");
    // 0x00 0x00 0x00 0x01 + 一条非法 CBOR（tag）载荷。
    let mut wire = vec![0, 0, 0, 1];
    wire.push(0xc0);
    let error = decoder.push(&wire).expect_err("bad payload");
    assert!(
        error
            .message()
            .starts_with("Invalid client protocol frame:"),
        "got: {error}"
    );
    let error = decoder.push(&[]).expect_err("decoder has failed");
    assert!(error.message().contains("has failed"), "got: {error}");
}

#[test]
fn rejects_unknown_message_types() {
    assert!(parse_client_json(json!({ "type": "hello_world" })).is_err());
    assert!(parse_server_json(json!({ "type": "hello_world" })).is_err());
    // 目标不能被当作客户端消息。
    assert!(parse_client_json(json!({ "serverId": SERVER_ID })).is_err());
    let _ = ServerTarget {
        server_id: SERVER_ID.into(),
    };
}
