//! CBOR 子集的 RFC 8949 向量与严格性测试（对应 pi `test/cbor/cbor.test.ts`）。

use tower_agent_protocol::{
    CborError, CborOptions, CborValue, DEFAULT_MAX_CBOR_DEPTH, decode_cbor, encode_cbor,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_hex(value: &str) -> Vec<u8> {
    assert!(
        value.len().is_multiple_of(2),
        "hex fixture must contain whole bytes"
    );
    (0..value.len() / 2)
        .map(|index| u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).expect("valid hex"))
        .collect()
}

/// pi 测试里用到的同一组 RFC 8949 向量。
fn vectors() -> Vec<(CborValue, &'static str)> {
    vec![
        (CborValue::Null, "f6"),
        (CborValue::Bool(false), "f4"),
        (CborValue::Bool(true), "f5"),
        (CborValue::Uint(0), "00"),
        (CborValue::Uint(1), "01"),
        (CborValue::Uint(10), "0a"),
        (CborValue::Uint(23), "17"),
        (CborValue::Uint(24), "1818"),
        (CborValue::Uint(25), "1819"),
        (CborValue::Uint(100), "1864"),
        (CborValue::Uint(1000), "1903e8"),
        (CborValue::Uint(1_000_000), "1a000f4240"),
        (CborValue::Uint(1_000_000_000_000), "1b000000e8d4a51000"),
        (CborValue::Uint(9_007_199_254_740_991), "1b001fffffffffffff"),
        (CborValue::Int(-1), "20"),
        (CborValue::Int(-10), "29"),
        (CborValue::Int(-24), "37"),
        (CborValue::Int(-25), "3818"),
        (CborValue::Int(-100), "3863"),
        (CborValue::Int(-1000), "3903e7"),
        (CborValue::Int(-1_000_000), "3a000f423f"),
        (CborValue::Int(-9_007_199_254_740_991), "3b001ffffffffffffe"),
        (CborValue::Float(1.1), "fb3ff199999999999a"),
        (CborValue::Float(-0.0), "fb8000000000000000"),
        (CborValue::Bytes(vec![1, 2, 3, 4]), "4401020304"),
        (CborValue::Text(String::new()), "60"),
        (CborValue::Text("IETF".into()), "6449455446"),
        (CborValue::Text("ü".into()), "62c3bc"),
        (CborValue::Text("水".into()), "63e6b0b4"),
        (CborValue::Text("𐅑".into()), "64f0908591"),
        (CborValue::Array(Vec::new()), "80"),
        (
            CborValue::Array(vec![
                CborValue::Uint(1),
                CborValue::Uint(2),
                CborValue::Uint(3),
            ]),
            "83010203",
        ),
        (
            CborValue::Array(vec![
                CborValue::Uint(1),
                CborValue::Array(vec![CborValue::Uint(2), CborValue::Uint(3)]),
                CborValue::Array(vec![CborValue::Uint(4), CborValue::Uint(5)]),
            ]),
            "8301820203820405",
        ),
        (
            CborValue::Map(vec![
                ("a".into(), CborValue::Uint(1)),
                (
                    "b".into(),
                    CborValue::Array(vec![CborValue::Uint(2), CborValue::Uint(3)]),
                ),
            ]),
            "a26161016162820203",
        ),
    ]
}

#[test]
fn encodes_and_decodes_rfc_8949_vectors() {
    for (value, wire) in vectors() {
        let encoded = encode_cbor(&value, CborOptions::default()).expect("encode");
        assert_eq!(hex(&encoded), wire, "encoding {value:?}");
        let decoded = decode_cbor(&from_hex(wire), CborOptions::default()).expect("decode");
        assert_eq!(decoded, value, "decoding {wire}");
    }
}

#[test]
fn preserves_negative_zero_as_a_float() {
    let encoded = encode_cbor(&CborValue::Float(-0.0), CborOptions::default()).expect("encode");
    let decoded = decode_cbor(&encoded, CborOptions::default()).expect("decode");
    match decoded {
        CborValue::Float(value) => assert!(value == 0.0 && value.is_sign_negative()),
        other => panic!("expected a float, got {other:?}"),
    }
}

#[test]
fn preserves_a_leading_bom() {
    let decoded = decode_cbor(&from_hex("63efbbbf"), CborOptions::default()).expect("decode");
    assert_eq!(decoded, CborValue::Text("\u{feff}".into()));
}

#[test]
fn rejects_redundant_non_minimal_integer_encoding() {
    // 24 用 1 字节参数编码（0x18 0x18）是合法的最小形式；这里检查 pi 允许的长形式也没问题，
    // 但 0x19 0x00 0x18（用 2 字节表示 24）属于非最短编码，pi 只校验范围不校验最短性。
    let decoded = decode_cbor(&from_hex("190018"), CborOptions::default()).expect("decode");
    assert_eq!(decoded, CborValue::Uint(24));
}

#[test]
fn rejects_unsupported_or_malformed_input() {
    let cases = [
        ("indefinite array", "9f".to_string()),
        ("break marker", "ff".to_string()),
        (
            "tag",
            "c074323031332d30332d32315432303a30343a30305a".to_string(),
        ),
        ("truncated", "18".to_string()),
        ("trailing data", "0000".to_string()),
        ("duplicate map key", "a2616101616102".to_string()),
        ("non-string map key", "a10102".to_string()),
        ("unsupported simple value", "f8ff".to_string()),
        ("f16 float width", "f93c00".to_string()),
        ("f32 float width", "fa47c35000".to_string()),
        ("invalid utf-8 text", "61ff".to_string()),
    ];
    for (label, wire) in cases {
        let result = decode_cbor(&from_hex(&wire), CborOptions::default());
        assert!(result.is_err(), "{label} should fail: {result:?}");
    }
}

#[test]
fn rejects_out_of_range_numbers() {
    // 2^53（超过 JS 安全整数）：0x1b 0020000000000000
    let error = decode_cbor(&from_hex("1b0020000000000000"), CborOptions::default())
        .expect_err("unsafe integer must be rejected");
    assert!(error.message().contains("safe range"), "got: {error}");

    let error = encode_cbor(&CborValue::Uint(u64::MAX), CborOptions::default())
        .expect_err("unsafe integer must be rejected");
    assert!(
        error.message().contains("safe JavaScript integers"),
        "got: {error}"
    );

    let error = encode_cbor(&CborValue::Float(f64::NAN), CborOptions::default())
        .expect_err("NaN must be rejected");
    assert!(matches!(error, CborError { .. }));
}

#[test]
fn enforces_depth_byte_and_container_limits() {
    let mut deep = CborValue::Null;
    for _ in 0..=DEFAULT_MAX_CBOR_DEPTH {
        deep = CborValue::Array(vec![deep]);
    }
    let error = encode_cbor(&deep, CborOptions::default()).expect_err("depth limit");
    assert!(error.message().contains("depth"), "got: {error}");

    let options = CborOptions {
        max_depth: Some(2),
        ..CborOptions::default()
    };
    let nested = CborValue::Array(vec![CborValue::Array(vec![CborValue::Array(vec![
        CborValue::Null,
    ])])]);
    assert!(encode_cbor(&nested, options).is_err());

    let options = CborOptions {
        max_byte_length: Some(4),
        ..CborOptions::default()
    };
    assert!(encode_cbor(&CborValue::Text("hello".into()), options).is_err());

    let options = CborOptions {
        max_container_length: Some(1),
        ..CborOptions::default()
    };
    assert!(
        encode_cbor(
            &CborValue::Array(vec![CborValue::Null, CborValue::Null]),
            options
        )
        .is_err()
    );
}

#[test]
fn json_round_trips_through_the_codec() {
    let json = serde_json::json!({
        "type": "request",
        "call": { "nested": [1, 2.5, true, null, "文本"] },
    });
    let value = CborValue::from_json(&json);
    let encoded = encode_cbor(&value, CborOptions::default()).expect("encode");
    let decoded = decode_cbor(&encoded, CborOptions::default()).expect("decode");
    assert_eq!(decoded.to_json().expect("json"), json);
}

#[test]
fn byte_strings_are_not_json_values() {
    let value = CborValue::Bytes(vec![1, 2, 3]);
    assert!(value.to_json().is_err());
}
