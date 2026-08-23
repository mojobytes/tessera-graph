// SPDX-License-Identifier: BSL-1.1

use std::f64::consts::PI;

use ermya_graph_protocol::ProtocolError;
use ermya_graph_protocol::packstream::{PackStreamValue, decode, encode};

// ---------------------------------------------------------------------------
// Round-trip helper
// ---------------------------------------------------------------------------

fn roundtrip(value: &PackStreamValue) {
    let mut buf = Vec::new();
    encode(value, &mut buf).unwrap();
    let (decoded, consumed) = decode(&buf).unwrap();
    assert_eq!(
        consumed,
        buf.len(),
        "round-trip did not consume entire buffer for {value:?}"
    );
    assert_eq!(&decoded, value, "round-trip value mismatch for {value:?}");
}

// ---------------------------------------------------------------------------
// Encoding tests — verify exact byte layout
// ---------------------------------------------------------------------------

#[test]
fn encode_null() {
    let mut buf = Vec::new();
    encode(&PackStreamValue::Null, &mut buf).unwrap();
    assert_eq!(buf, [0xC0]);
}

#[test]
fn encode_bool_true() {
    let mut buf = Vec::new();
    encode(&PackStreamValue::Bool(true), &mut buf).unwrap();
    assert_eq!(buf, [0xC3]);
}

#[test]
fn encode_bool_false() {
    let mut buf = Vec::new();
    encode(&PackStreamValue::Bool(false), &mut buf).unwrap();
    assert_eq!(buf, [0xC2]);
}

// --- Integer boundaries ---

fn encode_int_bytes(i: i64) -> Vec<u8> {
    let mut buf = Vec::new();
    encode(&PackStreamValue::Int(i), &mut buf).unwrap();
    buf
}

#[test]
fn encode_int_zero() {
    assert_eq!(encode_int_bytes(0), [0x00]);
}

#[test]
fn encode_int_one() {
    assert_eq!(encode_int_bytes(1), [0x01]);
}

#[test]
fn encode_int_positive_tiny_max() {
    // 127 is the last TinyInt positive value.
    assert_eq!(encode_int_bytes(127), [0x7F]);
}

#[test]
fn encode_int_minus_one() {
    // -1 as u8 = 0xFF (TinyInt).
    assert_eq!(encode_int_bytes(-1), [0xFF]);
}

#[test]
fn encode_int_minus_sixteen() {
    // -16 as u8 = 0xF0 (last TinyInt negative).
    assert_eq!(encode_int_bytes(-16), [0xF0]);
}

#[test]
fn encode_int_minus_seventeen() {
    // -17 is the first Int8 value.
    assert_eq!(encode_int_bytes(-17), [0xC8, 0xEF]);
}

#[test]
fn encode_int_minus_one_twenty_eight() {
    // -128 is the last Int8 value.
    assert_eq!(encode_int_bytes(-128), [0xC8, 0x80]);
}

#[test]
fn encode_int_128() {
    // 128 is the first Int16 value (beyond TinyInt).
    assert_eq!(encode_int_bytes(128), [0xC9, 0x00, 0x80]);
}

#[test]
fn encode_int_minus_129() {
    // -129 is the first Int16 value (beyond Int8).
    assert_eq!(encode_int_bytes(-129), [0xC9, 0xFF, 0x7F]);
}

#[test]
fn encode_int_32767() {
    assert_eq!(encode_int_bytes(32_767), [0xC9, 0x7F, 0xFF]);
}

#[test]
fn encode_int_minus_32768() {
    assert_eq!(encode_int_bytes(-32_768), [0xC9, 0x80, 0x00]);
}

#[test]
fn encode_int_32768() {
    // 32768 is the first Int32 value (beyond Int16).
    assert_eq!(encode_int_bytes(32_768), [0xCA, 0x00, 0x00, 0x80, 0x00]);
}

#[test]
fn encode_int_minus_32769() {
    assert_eq!(encode_int_bytes(-32_769), [0xCA, 0xFF, 0xFF, 0x7F, 0xFF]);
}

#[test]
fn encode_int_i32_max() {
    assert_eq!(
        encode_int_bytes(i64::from(i32::MAX)),
        [0xCA, 0x7F, 0xFF, 0xFF, 0xFF]
    );
}

#[test]
fn encode_int_i32_min() {
    assert_eq!(
        encode_int_bytes(i64::from(i32::MIN)),
        [0xCA, 0x80, 0x00, 0x00, 0x00]
    );
}

#[test]
fn encode_int_i64_max() {
    let expected = {
        let mut v = vec![0xCBu8];
        v.extend_from_slice(&i64::MAX.to_be_bytes());
        v
    };
    assert_eq!(encode_int_bytes(i64::MAX), expected);
}

#[test]
fn encode_int_i64_min() {
    let expected = {
        let mut v = vec![0xCBu8];
        v.extend_from_slice(&i64::MIN.to_be_bytes());
        v
    };
    assert_eq!(encode_int_bytes(i64::MIN), expected);
}

// --- Float ---

#[test]
fn encode_float_zero() {
    let mut buf = Vec::new();
    encode(&PackStreamValue::Float(0.0_f64), &mut buf).unwrap();
    assert_eq!(&buf[0..1], [0xC1]);
    assert_eq!(&buf[1..], 0.0_f64.to_bits().to_be_bytes());
}

#[test]
fn encode_float_one() {
    let mut buf = Vec::new();
    encode(&PackStreamValue::Float(1.0_f64), &mut buf).unwrap();
    assert_eq!(&buf[0..1], [0xC1]);
    assert_eq!(&buf[1..], 1.0_f64.to_bits().to_be_bytes());
}

#[test]
fn encode_float_pi() {
    let mut buf = Vec::new();
    encode(&PackStreamValue::Float(PI), &mut buf).unwrap();
    assert_eq!(&buf[0..1], [0xC1]);
    assert_eq!(&buf[1..], PI.to_bits().to_be_bytes());
}

#[test]
fn encode_nan_returns_error() {
    let mut buf = Vec::new();
    let err = encode(&PackStreamValue::Float(f64::NAN), &mut buf).unwrap_err();
    assert!(
        matches!(err, ProtocolError::PackStreamInvalidFloat),
        "expected PackStreamInvalidFloat, got {err:?}"
    );
}

#[test]
fn encode_infinity_returns_error() {
    let mut buf = Vec::new();
    let err = encode(&PackStreamValue::Float(f64::INFINITY), &mut buf).unwrap_err();
    assert!(
        matches!(err, ProtocolError::PackStreamInvalidFloat),
        "expected PackStreamInvalidFloat for +Inf, got {err:?}"
    );

    let err = encode(&PackStreamValue::Float(f64::NEG_INFINITY), &mut buf).unwrap_err();
    assert!(
        matches!(err, ProtocolError::PackStreamInvalidFloat),
        "expected PackStreamInvalidFloat for -Inf, got {err:?}"
    );
}

// --- Strings ---

#[test]
fn encode_string_empty() {
    let mut buf = Vec::new();
    encode(&PackStreamValue::String(String::new()), &mut buf).unwrap();
    assert_eq!(buf, [0x80]);
}

#[test]
fn encode_string_hello() {
    let mut buf = Vec::new();
    encode(&PackStreamValue::String("hello".to_owned()), &mut buf).unwrap();
    // 0x85 = TINY_STRING_BASE | 5
    assert_eq!(buf, [0x85, b'h', b'e', b'l', b'l', b'o']);
}

#[test]
fn encode_string_16_chars_uses_string8() {
    // 16 characters → can't fit in TinyString (max 15).
    let s: String = "a".repeat(16);
    let mut buf = Vec::new();
    encode(&PackStreamValue::String(s.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0xD0, "should use STRING8 marker");
    assert_eq!(buf[1], 16u8);
    assert_eq!(&buf[2..], s.as_bytes());
}

#[test]
fn encode_string_256_chars_uses_string16() {
    let s: String = "x".repeat(256);
    let mut buf = Vec::new();
    encode(&PackStreamValue::String(s.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0xD1, "should use STRING16 marker");
    assert_eq!(u16::from_be_bytes([buf[1], buf[2]]), 256u16);
    assert_eq!(&buf[3..], s.as_bytes());
}

// --- Bytes ---

#[test]
fn encode_bytes_empty() {
    let mut buf = Vec::new();
    encode(&PackStreamValue::Bytes(vec![]), &mut buf).unwrap();
    assert_eq!(buf, [0xCC, 0x00]);
}

#[test]
fn encode_bytes_small() {
    let mut buf = Vec::new();
    encode(&PackStreamValue::Bytes(vec![1, 2, 3]), &mut buf).unwrap();
    assert_eq!(buf, [0xCC, 0x03, 0x01, 0x02, 0x03]);
}

#[test]
fn encode_bytes_256_uses_bytes16() {
    let data = vec![0xABu8; 256];
    let mut buf = Vec::new();
    encode(&PackStreamValue::Bytes(data.clone()), &mut buf).unwrap();
    assert_eq!(buf[0], 0xCD, "should use BYTES16 marker");
    assert_eq!(u16::from_be_bytes([buf[1], buf[2]]), 256u16);
    assert_eq!(&buf[3..], data.as_slice());
}

// --- Lists ---

#[test]
fn encode_list_empty() {
    let mut buf = Vec::new();
    encode(&PackStreamValue::List(vec![]), &mut buf).unwrap();
    assert_eq!(buf, [0x90]);
}

#[test]
fn encode_list_one_null() {
    let mut buf = Vec::new();
    encode(
        &PackStreamValue::List(vec![PackStreamValue::Null]),
        &mut buf,
    )
    .unwrap();
    assert_eq!(buf, [0x91, 0xC0]);
}

#[test]
fn encode_list_nested() {
    let inner = PackStreamValue::List(vec![PackStreamValue::Null]);
    let outer = PackStreamValue::List(vec![inner]);
    let mut buf = Vec::new();
    encode(&outer, &mut buf).unwrap();
    // outer TinyList(1) + inner TinyList(1) + Null
    assert_eq!(buf, [0x91, 0x91, 0xC0]);
}

// --- Dicts ---

#[test]
fn encode_dict_empty() {
    let mut buf = Vec::new();
    encode(&PackStreamValue::Dict(vec![]), &mut buf).unwrap();
    assert_eq!(buf, [0xA0]);
}

#[test]
fn encode_dict_one_entry() {
    let mut buf = Vec::new();
    encode(
        &PackStreamValue::Dict(vec![("a".to_owned(), PackStreamValue::Null)]),
        &mut buf,
    )
    .unwrap();
    // TinyDict(1) + TinyString(1,"a") + Null
    assert_eq!(buf, [0xA1, 0x81, b'a', 0xC0]);
}

#[test]
fn encode_dict_two_entries() {
    let mut buf = Vec::new();
    encode(
        &PackStreamValue::Dict(vec![
            ("x".to_owned(), PackStreamValue::Bool(true)),
            ("y".to_owned(), PackStreamValue::Bool(false)),
        ]),
        &mut buf,
    )
    .unwrap();
    // TinyDict(2) + "x" + true + "y" + false
    assert_eq!(buf, [0xA2, 0x81, b'x', 0xC3, 0x81, b'y', 0xC2]);
}

// --- Structs ---

#[test]
fn encode_struct_empty_fields() {
    let mut buf = Vec::new();
    encode(
        &PackStreamValue::Struct {
            tag: 0x01,
            fields: vec![],
        },
        &mut buf,
    )
    .unwrap();
    // TinyStruct(0) + tag 0x01
    assert_eq!(buf, [0xB0, 0x01]);
}

#[test]
fn encode_struct_with_dict_field() {
    let mut buf = Vec::new();
    encode(
        &PackStreamValue::Struct {
            tag: 0x71,
            fields: vec![PackStreamValue::Dict(vec![])],
        },
        &mut buf,
    )
    .unwrap();
    // TinyStruct(1) + tag + TinyDict(0)
    assert_eq!(buf, [0xB1, 0x71, 0xA0]);
}

/// Structs with more than 15 fields are not representable in `PackStream`'s
/// Tiny form and will panic in debug builds.
#[test]
#[should_panic(expected = "PackStream structs support at most 15 fields")]
#[cfg(debug_assertions)]
fn encode_struct_with_16_fields_panics_in_debug() {
    let mut buf = Vec::new();
    let fields = vec![PackStreamValue::Null; 16];
    encode(&PackStreamValue::Struct { tag: 0x01, fields }, &mut buf).unwrap();
}

// ---------------------------------------------------------------------------
// Decoding tests
// ---------------------------------------------------------------------------

#[test]
fn decode_null() {
    let (val, consumed) = decode(&[0xC0]).unwrap();
    assert_eq!(val, PackStreamValue::Null);
    assert_eq!(consumed, 1);
}

#[test]
fn decode_bool_true() {
    let (val, consumed) = decode(&[0xC3]).unwrap();
    assert_eq!(val, PackStreamValue::Bool(true));
    assert_eq!(consumed, 1);
}

#[test]
fn decode_bool_false() {
    let (val, consumed) = decode(&[0xC2]).unwrap();
    assert_eq!(val, PackStreamValue::Bool(false));
    assert_eq!(consumed, 1);
}

#[test]
fn decode_tiny_int_zero() {
    let (val, consumed) = decode(&[0x00]).unwrap();
    assert_eq!(val, PackStreamValue::Int(0));
    assert_eq!(consumed, 1);
}

#[test]
fn decode_tiny_int_positive_max() {
    let (val, consumed) = decode(&[0x7F]).unwrap();
    assert_eq!(val, PackStreamValue::Int(127));
    assert_eq!(consumed, 1);
}

#[test]
fn decode_tiny_int_negative_min() {
    // 0xF0 = -16
    let (val, consumed) = decode(&[0xF0]).unwrap();
    assert_eq!(val, PackStreamValue::Int(-16));
    assert_eq!(consumed, 1);
}

#[test]
fn decode_tiny_int_minus_one() {
    let (val, consumed) = decode(&[0xFF]).unwrap();
    assert_eq!(val, PackStreamValue::Int(-1));
    assert_eq!(consumed, 1);
}

#[test]
fn decode_int8() {
    let (val, consumed) = decode(&[0xC8, 0x80]).unwrap();
    assert_eq!(val, PackStreamValue::Int(-128));
    assert_eq!(consumed, 2);
}

#[test]
fn decode_int16() {
    let (val, consumed) = decode(&[0xC9, 0x7F, 0xFF]).unwrap();
    assert_eq!(val, PackStreamValue::Int(32_767));
    assert_eq!(consumed, 3);
}

#[test]
fn decode_int32() {
    let (val, consumed) = decode(&[0xCA, 0x7F, 0xFF, 0xFF, 0xFF]).unwrap();
    assert_eq!(val, PackStreamValue::Int(i64::from(i32::MAX)));
    assert_eq!(consumed, 5);
}

#[test]
fn decode_int64() {
    let mut input = vec![0xCBu8];
    input.extend_from_slice(&i64::MAX.to_be_bytes());
    let (val, consumed) = decode(&input).unwrap();
    assert_eq!(val, PackStreamValue::Int(i64::MAX));
    assert_eq!(consumed, 9);
}

#[test]
fn decode_float64() {
    let mut input = vec![0xC1u8];
    input.extend_from_slice(&PI.to_bits().to_be_bytes());
    let (val, consumed) = decode(&input).unwrap();
    assert_eq!(val, PackStreamValue::Float(PI));
    assert_eq!(consumed, 9);
}

#[test]
fn decode_tiny_string() {
    let (val, consumed) = decode(&[0x85, b'h', b'e', b'l', b'l', b'o']).unwrap();
    assert_eq!(val, PackStreamValue::String("hello".to_owned()));
    assert_eq!(consumed, 6);
}

#[test]
fn decode_string8() {
    let s = "a".repeat(16);
    let mut input = vec![0xD0u8, 16u8];
    input.extend_from_slice(s.as_bytes());
    let (val, consumed) = decode(&input).unwrap();
    assert_eq!(val, PackStreamValue::String(s));
    assert_eq!(consumed, 18);
}

#[test]
fn decode_tiny_list() {
    let (val, consumed) = decode(&[0x91, 0xC0]).unwrap();
    assert_eq!(val, PackStreamValue::List(vec![PackStreamValue::Null]));
    assert_eq!(consumed, 2);
}

#[test]
fn decode_tiny_dict() {
    let (val, consumed) = decode(&[0xA1, 0x81, b'a', 0xC0]).unwrap();
    assert_eq!(
        val,
        PackStreamValue::Dict(vec![("a".to_owned(), PackStreamValue::Null)])
    );
    assert_eq!(consumed, 4);
}

#[test]
fn decode_tiny_struct() {
    let (val, consumed) = decode(&[0xB1, 0x71, 0xA0]).unwrap();
    assert_eq!(
        val,
        PackStreamValue::Struct {
            tag: 0x71,
            fields: vec![PackStreamValue::Dict(vec![])]
        }
    );
    assert_eq!(consumed, 3);
}

// --- Security: OOM protection ---

/// A LIST32 header claiming ~4 billion elements with only 5 bytes available
/// must return `PackStreamUnderflow`, not allocate 4 GiB of memory.
#[test]
fn decode_list32_with_huge_count_returns_underflow_not_oom() {
    // 0xD6 = LIST32 marker, followed by 4 bytes = 0xFFFFFFFF (4,294,967,295 elements)
    let buf = [0xD6u8, 0xFF, 0xFF, 0xFF, 0xFF];
    let err = decode(&buf).unwrap_err();
    assert!(
        matches!(err, ProtocolError::PackStreamUnderflow { .. }),
        "expected PackStreamUnderflow, got {err:?}"
    );
}

/// A DICT32 header claiming ~4 billion entries with only 5 bytes available
/// must return `PackStreamUnderflow`, not allocate 4 GiB of memory.
#[test]
fn decode_dict32_with_huge_count_returns_underflow_not_oom() {
    // 0xDA = DICT32 marker, followed by 4 bytes = 0xFFFFFFFF
    let buf = [0xDAu8, 0xFF, 0xFF, 0xFF, 0xFF];
    let err = decode(&buf).unwrap_err();
    assert!(
        matches!(err, ProtocolError::PackStreamUnderflow { .. }),
        "expected PackStreamUnderflow, got {err:?}"
    );
}

/// A 100-deep nested list (100 × TinyList(1) then Null) must return
/// `PackStreamDepthLimitExceeded` instead of overflowing the stack.
#[test]
fn decode_deeply_nested_list_returns_depth_error() {
    // Construct: 0x91 × 100 followed by 0xC0 (Null)
    // Each 0x91 = TinyList with 1 element, so nesting goes 100 levels deep.
    let depth = 100;
    let mut buf = vec![0x91u8; depth];
    buf.push(0xC0); // innermost element: Null
    let err = decode(&buf).unwrap_err();
    assert!(
        matches!(err, ProtocolError::PackStreamDepthLimitExceeded { .. }),
        "expected PackStreamDepthLimitExceeded, got {err:?}"
    );
}

// --- Error cases ---

#[test]
fn decode_empty_buffer_returns_underflow() {
    let err = decode(&[]).unwrap_err();
    assert!(
        matches!(err, ProtocolError::PackStreamUnderflow { .. }),
        "expected underflow, got {err:?}"
    );
}

#[test]
fn decode_unknown_marker_0xc4() {
    let err = decode(&[0xC4]).unwrap_err();
    assert!(
        matches!(err, ProtocolError::PackStreamUnknownMarker { marker: 0xC4 }),
        "expected UnknownMarker(0xC4), got {err:?}"
    );
}

#[test]
fn decode_truncated_int16_returns_underflow() {
    // Marker present but only 1 byte of int16 data.
    let err = decode(&[0xC9, 0x00]).unwrap_err();
    assert!(
        matches!(err, ProtocolError::PackStreamUnderflow { .. }),
        "expected underflow, got {err:?}"
    );
}

#[test]
fn decode_invalid_utf8_returns_error() {
    // TinyString(1) followed by 0xFF — invalid UTF-8.
    let err = decode(&[0x81, 0xFF]).unwrap_err();
    assert!(
        matches!(err, ProtocolError::PackStreamInvalidUtf8),
        "expected InvalidUtf8, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Round-trip tests
// ---------------------------------------------------------------------------

#[test]
fn roundtrip_null() {
    roundtrip(&PackStreamValue::Null);
}

#[test]
fn roundtrip_bools() {
    roundtrip(&PackStreamValue::Bool(true));
    roundtrip(&PackStreamValue::Bool(false));
}

#[test]
fn roundtrip_integers() {
    for i in [
        0,
        1,
        127,
        -1,
        -16,
        -17,
        -128,
        128,
        -129,
        32_767,
        -32_768,
        32_768,
        -32_769,
        i64::from(i32::MAX),
        i64::from(i32::MIN),
        i64::MAX,
        i64::MIN,
    ] {
        roundtrip(&PackStreamValue::Int(i));
    }
}

#[test]
fn roundtrip_floats() {
    roundtrip(&PackStreamValue::Float(0.0));
    roundtrip(&PackStreamValue::Float(1.0));
    roundtrip(&PackStreamValue::Float(PI));
}

#[test]
fn roundtrip_strings() {
    roundtrip(&PackStreamValue::String(String::new()));
    roundtrip(&PackStreamValue::String("hello".to_owned()));
    roundtrip(&PackStreamValue::String("a".repeat(16)));
    roundtrip(&PackStreamValue::String("b".repeat(256)));
}

#[test]
fn roundtrip_bytes() {
    roundtrip(&PackStreamValue::Bytes(vec![]));
    roundtrip(&PackStreamValue::Bytes(vec![1, 2, 3]));
    roundtrip(&PackStreamValue::Bytes(vec![0xAB; 256]));
}

#[test]
fn roundtrip_list_empty() {
    roundtrip(&PackStreamValue::List(vec![]));
}

#[test]
fn roundtrip_list_mixed() {
    roundtrip(&PackStreamValue::List(vec![
        PackStreamValue::Null,
        PackStreamValue::Bool(true),
        PackStreamValue::Int(42),
        PackStreamValue::String("hi".to_owned()),
    ]));
}

#[test]
fn roundtrip_dict_empty() {
    roundtrip(&PackStreamValue::Dict(vec![]));
}

#[test]
fn roundtrip_dict_mixed() {
    roundtrip(&PackStreamValue::Dict(vec![
        ("null_key".to_owned(), PackStreamValue::Null),
        ("int_key".to_owned(), PackStreamValue::Int(-1)),
    ]));
}

#[test]
fn roundtrip_struct_empty() {
    roundtrip(&PackStreamValue::Struct {
        tag: 0x01,
        fields: vec![],
    });
}

#[test]
fn roundtrip_struct_complex() {
    roundtrip(&PackStreamValue::Struct {
        tag: 0x71,
        fields: vec![PackStreamValue::Dict(vec![(
            "x".to_owned(),
            PackStreamValue::Int(1),
        )])],
    });
}

#[test]
fn roundtrip_deep_nesting() {
    // List of Dict of List
    let inner_list = PackStreamValue::List(vec![PackStreamValue::Int(99)]);
    let dict = PackStreamValue::Dict(vec![("nested".to_owned(), inner_list)]);
    let outer = PackStreamValue::List(vec![dict]);
    roundtrip(&outer);
}

// ---------------------------------------------------------------------------
// Edge-case / boundary tests
// ---------------------------------------------------------------------------

#[test]
fn integer_boundary_tiny_to_int8() {
    // -16 is TinyInt (1 byte), -17 is Int8 (2 bytes).
    let tiny = encode_int_bytes(-16);
    assert_eq!(tiny.len(), 1);
    let int8 = encode_int_bytes(-17);
    assert_eq!(int8.len(), 2);
    assert_eq!(int8[0], 0xC8);
}

#[test]
fn integer_boundary_tiny_positive_to_int16() {
    // 127 is TinyInt (1 byte), 128 is Int16 (3 bytes).
    let tiny = encode_int_bytes(127);
    assert_eq!(tiny.len(), 1);
    let int16 = encode_int_bytes(128);
    assert_eq!(int16.len(), 3);
    assert_eq!(int16[0], 0xC9);
}

#[test]
fn integer_boundary_int8_to_int16() {
    // -128 is Int8 (2 bytes), -129 is Int16 (3 bytes).
    assert_eq!(encode_int_bytes(-128).len(), 2);
    assert_eq!(encode_int_bytes(-129).len(), 3);
}

#[test]
fn integer_boundary_int16_to_int32() {
    // 32767 is Int16 (3 bytes), 32768 is Int32 (5 bytes).
    assert_eq!(encode_int_bytes(32_767).len(), 3);
    assert_eq!(encode_int_bytes(32_768).len(), 5);
    assert_eq!(encode_int_bytes(-32_768).len(), 3);
    assert_eq!(encode_int_bytes(-32_769).len(), 5);
}

#[test]
fn integer_boundary_int32_to_int64() {
    assert_eq!(encode_int_bytes(i64::from(i32::MAX)).len(), 5);
    assert_eq!(encode_int_bytes(i64::from(i32::MAX) + 1).len(), 9);
    assert_eq!(encode_int_bytes(i64::from(i32::MIN)).len(), 5);
    assert_eq!(encode_int_bytes(i64::from(i32::MIN) - 1).len(), 9);
}

#[test]
fn string_boundary_15_to_16() {
    // 15-char string → TinyString (1-byte header).
    let s15: String = "z".repeat(15);
    let mut buf = Vec::new();
    encode(&PackStreamValue::String(s15), &mut buf).unwrap();
    assert_eq!(buf[0], 0x8F); // TINY_STRING_BASE | 15

    // 16-char string → String8 (2-byte header).
    let s16: String = "z".repeat(16);
    let mut buf = Vec::new();
    encode(&PackStreamValue::String(s16), &mut buf).unwrap();
    assert_eq!(buf[0], 0xD0);
}

#[test]
fn string_boundary_255_to_256() {
    let s255: String = "a".repeat(255);
    let mut buf = Vec::new();
    encode(&PackStreamValue::String(s255), &mut buf).unwrap();
    assert_eq!(buf[0], 0xD0); // String8

    let s256: String = "a".repeat(256);
    let mut buf = Vec::new();
    encode(&PackStreamValue::String(s256), &mut buf).unwrap();
    assert_eq!(buf[0], 0xD1); // String16
}

#[test]
fn list_boundary_15_to_16() {
    let list15 = PackStreamValue::List(vec![PackStreamValue::Null; 15]);
    let mut buf = Vec::new();
    encode(&list15, &mut buf).unwrap();
    assert_eq!(buf[0], 0x9F); // TINY_LIST_BASE | 15

    let list16 = PackStreamValue::List(vec![PackStreamValue::Null; 16]);
    let mut buf = Vec::new();
    encode(&list16, &mut buf).unwrap();
    assert_eq!(buf[0], 0xD4); // LIST8
}

#[test]
fn dict_boundary_15_to_16() {
    let pairs15: Vec<(String, PackStreamValue)> = (0..15)
        .map(|i| (format!("k{i}"), PackStreamValue::Null))
        .collect();
    let mut buf = Vec::new();
    encode(&PackStreamValue::Dict(pairs15), &mut buf).unwrap();
    assert_eq!(buf[0], 0xAF); // TINY_DICT_BASE | 15

    let pairs16: Vec<(String, PackStreamValue)> = (0..16)
        .map(|i| (format!("k{i}"), PackStreamValue::Null))
        .collect();
    let mut buf = Vec::new();
    encode(&PackStreamValue::Dict(pairs16), &mut buf).unwrap();
    assert_eq!(buf[0], 0xD8); // DICT8
}
