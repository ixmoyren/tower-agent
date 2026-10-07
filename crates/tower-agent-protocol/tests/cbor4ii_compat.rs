//! 用 `cbor4ii`（独立实现）交叉验证线上格式确实是标准 CBOR。
//!
//! 这补足了 RFC 8949 向量之外的一层证据：pi 写出来的字节由标准 CBOR 编码器产出，
//! 反过来标准编码器写出来的字节我们的严格子集也要能读。

use cbor4ii::core::{
    Value,
    dec::Decode as _,
    enc::Encode as _,
    utils::{BufWriter, SliceReader},
};
use serde_json::json;
use tower_agent_protocol::{
    CborOptions, CborValue, ClientHello, ClientMessage, FrameDecoderOptions, RequestEnvelope,
    RpcTarget, ServerTarget, decode_cbor, encode_cbor, encode_client_message,
};

/// 我们的 `Uint` / `Int` 在 cbor4ii 里都折进 `Integer(i128)`。
fn to_cbor4ii(value: &CborValue) -> Value {
    match value {
        CborValue::Null => Value::Null,
        CborValue::Bool(value) => Value::Bool(*value),
        CborValue::Uint(value) => Value::Integer(i128::from(*value)),
        CborValue::Int(value) => Value::Integer(i128::from(*value)),
        CborValue::Float(value) => Value::Float(*value),
        CborValue::Text(value) => Value::Text(value.clone()),
        CborValue::Bytes(value) => Value::Bytes(value.clone()),
        CborValue::Array(items) => Value::Array(items.iter().map(to_cbor4ii).collect()),
        CborValue::Map(entries) => Value::Map(
            entries
                .iter()
                .map(|(key, value)| (Value::Text(key.clone()), to_cbor4ii(value)))
                .collect(),
        ),
    }
}

fn from_cbor4ii(value: &Value) -> CborValue {
    match value {
        Value::Null => CborValue::Null,
        Value::Bool(value) => CborValue::Bool(*value),
        Value::Integer(value) => {
            if *value < 0 {
                CborValue::Int(*value as i64)
            } else {
                CborValue::Uint(*value as u64)
            }
        }
        Value::Float(value) => CborValue::Float(*value),
        Value::Text(value) => CborValue::Text(value.clone()),
        Value::Bytes(value) => CborValue::Bytes(value.clone()),
        Value::Array(items) => CborValue::Array(items.iter().map(from_cbor4ii).collect()),
        Value::Map(entries) => CborValue::Map(
            entries
                .iter()
                .map(|(key, value)| match key {
                    Value::Text(key) => (key.clone(), from_cbor4ii(value)),
                    other => panic!("unexpected non-text key {other:?}"),
                })
                .collect(),
        ),
        // 我们的子集不支持 tag（`Value` 是 `#[non_exhaustive]`，这也覆盖未知变体）。
        other => panic!("unsupported cbor4ii value {other:?}"),
    }
}

fn encode_with_cbor4ii(value: &Value) -> Vec<u8> {
    let mut writer = BufWriter::new(Vec::new());
    value.encode(&mut writer).expect("cbor4ii encode");
    writer.into_inner()
}

fn decode_with_cbor4ii(bytes: &[u8]) -> Value {
    let mut reader = SliceReader::new(bytes);
    Value::decode(&mut reader).expect("cbor4ii decode")
}

fn samples() -> Vec<CborValue> {
    vec![
        CborValue::Null,
        CborValue::Bool(true),
        CborValue::Uint(0),
        CborValue::Uint(1_000_000),
        CborValue::Uint(9_007_199_254_740_991),
        CborValue::Int(-1),
        CborValue::Int(-9_007_199_254_740_991),
        CborValue::Float(1.1),
        CborValue::Text("水".into()),
        CborValue::Bytes(vec![1, 2, 3, 4]),
        CborValue::Array(vec![
            CborValue::Uint(1),
            CborValue::Array(vec![CborValue::Uint(2), CborValue::Text("x".into())]),
        ]),
        CborValue::Map(vec![
            ("type".into(), CborValue::Text("hello".into())),
            ("version".into(), CborValue::Uint(8)),
        ]),
    ]
}

#[test]
fn our_encoder_produces_standard_cbor() {
    for value in samples() {
        let bytes = encode_cbor(&value, CborOptions::default()).expect("encode");
        let decoded = decode_with_cbor4ii(&bytes);
        assert_eq!(decoded, to_cbor4ii(&value), "value {value:?}");
    }
}

#[test]
fn our_decoder_reads_standard_cbor() {
    for value in samples() {
        let bytes = encode_with_cbor4ii(&to_cbor4ii(&value));
        let decoded =
            decode_cbor(&bytes, CborOptions::default()).expect("we decode cbor4ii output");
        assert_eq!(decoded, value, "value {value:?}");
    }
}

#[test]
fn framed_protocol_messages_are_standard_cbor() {
    let message = ClientMessage::Request(RequestEnvelope {
        id: "request-1".into(),
        target: RpcTarget::Server(ServerTarget {
            server_id: "00000000-0000-4000-8000-000000000001".into(),
        }),
        call: json!({ "serviceId": "pi.models", "member": "list", "args": [] }),
    });
    let wire = encode_client_message(&message, FrameDecoderOptions::default()).expect("encode");
    // 剥掉 4 字节长度前缀后，载荷应当能被独立 CBOR 实现解出来。
    let payload = from_cbor4ii(&decode_with_cbor4ii(&wire[4..]));
    let expected = CborValue::Map(vec![
        ("type".into(), CborValue::Text("request".into())),
        ("id".into(), CborValue::Text("request-1".into())),
        (
            "target".into(),
            CborValue::Map(vec![(
                "serverId".into(),
                CborValue::Text("00000000-0000-4000-8000-000000000001".into()),
            )]),
        ),
        (
            "call".into(),
            CborValue::Map(vec![
                ("args".into(), CborValue::Array(Vec::new())),
                ("member".into(), CborValue::Text("list".into())),
                ("serviceId".into(), CborValue::Text("pi.models".into())),
            ]),
        ),
    ]);
    // serde_json 对象默认按 BTreeMap 排序，这里比较前先按 key 排序。
    let mut payload_map = match payload {
        CborValue::Map(entries) => entries,
        other => panic!("expected a map, got {other:?}"),
    };
    let mut expected_map = match expected {
        CborValue::Map(entries) => entries,
        other => panic!("expected a map, got {other:?}"),
    };
    let sort = |entries: &mut Vec<(String, CborValue)>| {
        for (_, value) in entries.iter_mut() {
            if let CborValue::Map(inner) = value {
                inner.sort_by(|(left, _), (right, _)| left.cmp(right));
            }
        }
        entries.sort_by(|(left, _), (right, _)| left.cmp(right));
    };
    sort(&mut payload_map);
    sort(&mut expected_map);
    assert_eq!(payload_map, expected_map);

    // 也验证同一条消息能被自己的解码器按帧读回。
    let decoded = tower_agent_protocol::ClientMessageDecoder::new(FrameDecoderOptions::default())
        .expect("decoder")
        .push(&wire)
        .expect("push");
    assert_eq!(decoded, vec![message]);
}

#[test]
fn hello_message_payload_is_standard_cbor() {
    let message = ClientMessage::Hello(ClientHello { version: 8 });
    let wire = encode_client_message(&message, FrameDecoderOptions::default()).expect("encode");
    let CborValue::Map(entries) = from_cbor4ii(&decode_with_cbor4ii(&wire[4..])) else {
        panic!("expected a map");
    };
    assert!(entries.contains(&("type".into(), CborValue::Text("hello".into()))));
    assert!(entries.contains(&("version".into(), CborValue::Uint(8))));
}
