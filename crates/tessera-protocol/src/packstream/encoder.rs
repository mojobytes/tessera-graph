// Copyright 2026 BelowZero Security OU. All rights reserved.

//! `PackStream` encoder — serializes `PackStreamValue` to a byte buffer.

use super::markers as m;
use super::value::PackStreamValue;

/// Encode `value` into `buf` using the `PackStream` binary format.
///
/// The encoding appends bytes to `buf`; callers may pre-allocate capacity as
/// needed. All multi-byte integers are written in big-endian byte order.
pub fn encode(value: &PackStreamValue, buf: &mut Vec<u8>) {
    match value {
        PackStreamValue::Null => buf.push(m::NULL),
        PackStreamValue::Bool(true) => buf.push(m::BOOL_TRUE),
        PackStreamValue::Bool(false) => buf.push(m::BOOL_FALSE),
        PackStreamValue::Int(i) => encode_int(*i, buf),
        PackStreamValue::Float(f) => {
            buf.push(m::FLOAT64);
            buf.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        PackStreamValue::String(s) => encode_string(s, buf),
        PackStreamValue::Bytes(b) => encode_bytes(b, buf),
        PackStreamValue::List(items) => encode_list(items, buf),
        PackStreamValue::Dict(pairs) => encode_dict(pairs, buf),
        PackStreamValue::Struct { tag, fields } => encode_struct(*tag, fields, buf),
    }
}

// ---------------------------------------------------------------------------
// Integer encoding
// ---------------------------------------------------------------------------

fn encode_int(i: i64, buf: &mut Vec<u8>) {
    // TinyInt range: -16..=127 — the marker byte IS the value.
    if (-16..=127).contains(&i) {
        // Safe: the value fits in i8 and therefore in u8 for wire encoding.
        // i64 in -16..=127 wraps to the correct bit pattern in u8.
        buf.push(i.to_ne_bytes()[0]);
        return;
    }
    // Int8 range: -128..=-17
    if (-128..=-17).contains(&i) {
        buf.push(m::INT8);
        // Safe: value is verified to be in i8 range above.
        #[allow(clippy::cast_possible_truncation)]
        let byte = i as i8;
        buf.push(byte.to_be_bytes()[0]);
        return;
    }
    // Int16 range: -32768..=32767 (excluding TinyInt and Int8)
    if (-32_768..=32_767).contains(&i) {
        buf.push(m::INT16);
        // Safe: value is verified to be in i16 range above.
        #[allow(clippy::cast_possible_truncation)]
        let word = i as i16;
        buf.extend_from_slice(&word.to_be_bytes());
        return;
    }
    // Int32 range: -2^31..=2^31-1 (excluding Int16 range)
    if (i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&i) {
        buf.push(m::INT32);
        // Safe: value is verified to be in i32 range above.
        #[allow(clippy::cast_possible_truncation)]
        let dword = i as i32;
        buf.extend_from_slice(&dword.to_be_bytes());
        return;
    }
    // Everything else: Int64
    buf.push(m::INT64);
    buf.extend_from_slice(&i.to_be_bytes());
}

// ---------------------------------------------------------------------------
// String encoding
// ---------------------------------------------------------------------------

fn encode_string(s: &str, buf: &mut Vec<u8>) {
    let bytes = s.as_bytes();
    encode_sized_header(
        bytes.len(),
        m::TINY_STRING_BASE,
        m::STRING8,
        m::STRING16,
        m::STRING32,
        buf,
    );
    buf.extend_from_slice(bytes);
}

// ---------------------------------------------------------------------------
// Bytes encoding
// ---------------------------------------------------------------------------

fn encode_bytes(data: &[u8], buf: &mut Vec<u8>) {
    let len = data.len();
    if len <= 0xFF {
        buf.push(m::BYTES8);
        // Safe: len is verified to be <= u8::MAX.
        #[allow(clippy::cast_possible_truncation)]
        buf.push(len as u8);
    } else if len <= 0xFFFF {
        buf.push(m::BYTES16);
        // Safe: len is verified to be <= u16::MAX.
        #[allow(clippy::cast_possible_truncation)]
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        buf.push(m::BYTES32);
        // Safe: len is verified to be <= u32::MAX (usize >= 32 bits on all targets).
        #[allow(clippy::cast_possible_truncation)]
        buf.extend_from_slice(&(len as u32).to_be_bytes());
    }
    buf.extend_from_slice(data);
}

// ---------------------------------------------------------------------------
// List encoding
// ---------------------------------------------------------------------------

fn encode_list(items: &[PackStreamValue], buf: &mut Vec<u8>) {
    encode_sized_header(
        items.len(),
        m::TINY_LIST_BASE,
        m::LIST8,
        m::LIST16,
        m::LIST32,
        buf,
    );
    for item in items {
        encode(item, buf);
    }
}

// ---------------------------------------------------------------------------
// Dict encoding
// ---------------------------------------------------------------------------

fn encode_dict(pairs: &[(String, PackStreamValue)], buf: &mut Vec<u8>) {
    encode_sized_header(
        pairs.len(),
        m::TINY_DICT_BASE,
        m::DICT8,
        m::DICT16,
        m::DICT32,
        buf,
    );
    for (key, value) in pairs {
        encode_string(key, buf);
        encode(value, buf);
    }
}

// ---------------------------------------------------------------------------
// Struct encoding
// ---------------------------------------------------------------------------

fn encode_struct(tag: u8, fields: &[PackStreamValue], buf: &mut Vec<u8>) {
    // Structs only have a Tiny form (0–15 fields).
    // Safe: field count is masked to 4 bits.
    #[allow(clippy::cast_possible_truncation)]
    buf.push(m::TINY_STRUCT_BASE | (fields.len() as u8 & 0x0F));
    buf.push(tag);
    for field in fields {
        encode(field, buf);
    }
}

// ---------------------------------------------------------------------------
// Shared header helper
// ---------------------------------------------------------------------------

/// Write a size header for types that have Tiny / 8-bit / 16-bit / 32-bit forms.
///
/// - 0–15     → `tiny_base | len` (single byte, no length field)
/// - 16–255   → `marker8`  + u8 length
/// - 256–65535 → `marker16` + u16 BE length
/// - 65536+   → `marker32` + u32 BE length
fn encode_sized_header(
    len: usize,
    tiny_base: u8,
    marker8: u8,
    marker16: u8,
    marker32: u8,
    buf: &mut Vec<u8>,
) {
    if len <= 0x0F {
        // Safe: len is verified to be <= 15, fits in low 4 bits.
        #[allow(clippy::cast_possible_truncation)]
        buf.push(tiny_base | len as u8);
    } else if len <= 0xFF {
        buf.push(marker8);
        // Safe: len is verified to be <= u8::MAX.
        #[allow(clippy::cast_possible_truncation)]
        buf.push(len as u8);
    } else if len <= 0xFFFF {
        buf.push(marker16);
        // Safe: len is verified to be <= u16::MAX.
        #[allow(clippy::cast_possible_truncation)]
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        buf.push(marker32);
        // Safe: len is verified to be <= u32::MAX (usize >= 32 bits on all targets).
        #[allow(clippy::cast_possible_truncation)]
        buf.extend_from_slice(&(len as u32).to_be_bytes());
    }
}
