//! 用 `ciborium`（独立实现）交叉验证线上格式确实是标准 CBOR。
//!
//! 这补足了 RFC 8949 向量之外的一层证据：pi 写出来的字节由标准 CBOR 编码器产出，
//! 反过来标准编码器写出来的字节我们的严格子集也要能读。

use protocol::{
    CborOptions, CborValue, ClientHello, ClientMessage, FrameDecoderOptions, RequestEnvelope,
    RpcTarget, ServerTarget, decode_cbor, encode_cbor, encode_client_message,
};
use serde_json::json;

fn to_ciborium(value: &CborValue) -> ciborium::Value {
    match value {
        CborValue::Null => ciborium::Value::Null,
        CborValue::Bool(value) => ciborium::Value::Bool(*value),
        CborValue::Uint(value) => ciborium::Value::Integer((*value).into()),
        CborValue::Int(value) => ciborium::Value::Integer((*value).into()),
        CborValue::Float(value) => ciborium::Value::Float(*value),
        CborValue::Text(value) => ciborium::Value::Text(value.clone()),
        CborValue::Bytes(value) => ciborium::Value::Bytes(value.clone()),
        CborValue::Array(items) => ciborium::Value::Array(items.iter().map(to_ciborium).collect()),
        CborValue::Map(entries) => ciborium::Value::Map(
            entries
                .iter()
                .map(|(key, value)| (ciborium::Value::Text(key.clone()), to_ciborium(value)))
                .collect(),
        ),
    }
}

fn from_ciborium(value: &ciborium::Value) -> CborValue {
    match value {
        ciborium::Value::Null => CborValue::Null,
        ciborium::Value::Bool(value) => CborValue::Bool(*value),
        ciborium::Value::Integer(value) => {
            let as_i128: i128 = (*value).into();
            if as_i128 < 0 {
                CborValue::Int(as_i128 as i64)
            } else {
                CborValue::Uint(as_i128 as u64)
            }
        }
        ciborium::Value::Float(value) => CborValue::Float(*value),
        ciborium::Value::Text(value) => CborValue::Text(value.clone()),
        ciborium::Value::Bytes(value) => CborValue::Bytes(value.clone()),
        ciborium::Value::Array(items) => {
            CborValue::Array(items.iter().map(from_ciborium).collect())
        }
        ciborium::Value::Map(entries) => CborValue::Map(
            entries
                .iter()
                .map(|(key, value)| match key {
                    ciborium::Value::Text(key) => (key.clone(), from_ciborium(value)),
                    other => panic!("unexpected non-text key {other:?}"),
                })
                .collect(),
        ),
        other => panic!("unsupported ciborium value {other:?}"),
    }
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
        let decoded: ciborium::Value =
            ciborium::from_reader(bytes.as_slice()).expect("ciborium decodes our output");
        assert_eq!(decoded, to_ciborium(&value), "value {value:?}");
    }
}

#[test]
fn our_decoder_reads_standard_cbor() {
    for value in samples() {
        let mut bytes = Vec::new();
        ciborium::into_writer(&to_ciborium(&value), &mut bytes).expect("ciborium encode");
        let decoded =
            decode_cbor(&bytes, CborOptions::default()).expect("we decode ciborium output");
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
    let payload: ciborium::Value =
        ciborium::from_reader(&wire[4..]).expect("ciborium decodes the framed payload");
    let expected = to_ciborium(&CborValue::Map(vec![
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
    ]));
    // serde_json 对象默认按 BTreeMap 排序，这里比较前先按 key 排序。
    let mut payload_map = match payload {
        ciborium::Value::Map(entries) => entries,
        other => panic!("expected a map, got {other:?}"),
    };
    let mut expected_map = match expected {
        ciborium::Value::Map(entries) => entries,
        other => panic!("expected a map, got {other:?}"),
    };
    let sort = |entries: &mut Vec<(ciborium::Value, ciborium::Value)>| {
        for (_, value) in entries.iter_mut() {
            if let ciborium::Value::Map(inner) = value {
                inner.sort_by_key(|(key, _)| format!("{key:?}"));
            }
        }
        entries.sort_by_key(|(key, _)| format!("{key:?}"));
    };
    sort(&mut payload_map);
    sort(&mut expected_map);
    assert_eq!(payload_map, expected_map);

    // 也验证同一条消息能被自己的解码器按帧读回。
    let decoded = protocol::ClientMessageDecoder::new(FrameDecoderOptions::default())
        .expect("decoder")
        .push(&wire)
        .expect("push");
    assert_eq!(decoded, vec![message]);
}

#[test]
fn hello_message_payload_is_standard_cbor() {
    let message = ClientMessage::Hello(ClientHello { version: 8 });
    let wire = encode_client_message(&message, FrameDecoderOptions::default()).expect("encode");
    let payload: ciborium::Value = ciborium::from_reader(&wire[4..]).expect("ciborium decode");
    let CborValue::Map(entries) = from_ciborium(&payload) else {
        panic!("expected a map");
    };
    assert!(entries.contains(&("type".into(), CborValue::Text("hello".into()))));
    assert!(entries.contains(&("version".into(), CborValue::Uint(8))));
}
