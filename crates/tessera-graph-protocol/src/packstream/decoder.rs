// SPDX-License-Identifier: BSL-1.1

//! `PackStream` decoder — deserializes bytes into `PackStreamValue`.

use super::markers as m;
use super::value::PackStreamValue;
use crate::error::{ProtocolError, Result};

/// Maximum nesting depth for recursive decode. Prevents stack overflow from
/// adversarially crafted deeply-nested lists or dicts.
const MAX_DECODE_DEPTH: usize = 64;

/// Maximum number of elements to pre-allocate for list/dict/struct bodies.
/// Pre-allocation is capped at this value even when the wire count is larger;
/// the actual elements are still decoded one-by-one (so an undersized buffer
/// triggers `PackStreamUnderflow` before we allocate further).
const MAX_PREALLOC: usize = 8_192;

/// Decode one `PackStream` value from `buf`.
///
/// Returns the decoded value and the number of bytes consumed from `buf`.
///
/// # Errors
///
/// Returns:
/// - [`ProtocolError::PackStreamUnderflow`] — buffer is too short.
/// - [`ProtocolError::PackStreamUnknownMarker`] — unrecognised marker byte.
/// - [`ProtocolError::PackStreamInvalidUtf8`] — string bytes are not valid UTF-8.
/// - [`ProtocolError::PackStreamDictKeyNotString`] — a dict key decoded as a non-string value.
/// - [`ProtocolError::PackStreamDepthLimitExceeded`] — nesting exceeds [`MAX_DECODE_DEPTH`].
pub fn decode(buf: &[u8]) -> Result<(PackStreamValue, usize)> {
    require_bytes(buf, 1)?;
    decode_at(buf, 0, 0)
}

// ---------------------------------------------------------------------------
// Core dispatch
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn decode_at(buf: &[u8], offset: usize, depth: usize) -> Result<(PackStreamValue, usize)> {
    require_bytes_from(buf, offset, 1)?;
    let marker = buf[offset];
    let start = offset + 1; // position after the marker byte

    match marker {
        // TinyInt positive: 0x00–0x7F
        0x00..=0x7F => Ok((PackStreamValue::Int(i64::from(marker)), start)),

        // TinyString: 0x80–0x8F
        0x80..=0x8F => {
            let len = usize::from(marker & 0x0F);
            decode_string_body(buf, start, len)
        }

        // TinyList: 0x90–0x9F
        0x90..=0x9F => {
            let count = usize::from(marker & 0x0F);
            decode_list_body(buf, start, count, depth)
        }

        // TinyDict: 0xA0–0xAF
        0xA0..=0xAF => {
            let count = usize::from(marker & 0x0F);
            decode_dict_body(buf, start, count, depth)
        }

        // TinyStruct: 0xB0–0xBF
        0xB0..=0xBF => {
            let field_count = usize::from(marker & 0x0F);
            require_bytes_from(buf, start, 1)?;
            let tag = buf[start];
            decode_struct_body(buf, start + 1, tag, field_count, depth)
        }

        m::NULL => Ok((PackStreamValue::Null, start)),

        m::FLOAT64 => {
            require_bytes_from(buf, start, 8)?;
            let bits = u64::from_be_bytes([
                buf[start],
                buf[start + 1],
                buf[start + 2],
                buf[start + 3],
                buf[start + 4],
                buf[start + 5],
                buf[start + 6],
                buf[start + 7],
            ]);
            Ok((PackStreamValue::Float(f64::from_bits(bits)), start + 8))
        }

        m::BOOL_FALSE => Ok((PackStreamValue::Bool(false), start)),
        m::BOOL_TRUE => Ok((PackStreamValue::Bool(true), start)),

        m::INT8 | m::INT16 | m::INT32 | m::INT64 => decode_integer_at(buf, marker, start),

        m::BYTES8 => {
            require_bytes_from(buf, start, 1)?;
            let len = usize::from(buf[start]);
            decode_bytes_body(buf, start + 1, len)
        }

        m::BYTES16 => {
            require_bytes_from(buf, start, 2)?;
            let len = usize::from(u16::from_be_bytes([buf[start], buf[start + 1]]));
            decode_bytes_body(buf, start + 2, len)
        }

        m::BYTES32 => {
            require_bytes_from(buf, start, 4)?;
            let raw =
                u32::from_be_bytes([buf[start], buf[start + 1], buf[start + 2], buf[start + 3]]);
            let len = raw as usize;
            decode_bytes_body(buf, start + 4, len)
        }

        m::STRING8 => {
            require_bytes_from(buf, start, 1)?;
            let len = usize::from(buf[start]);
            decode_string_body(buf, start + 1, len)
        }

        m::STRING16 => {
            require_bytes_from(buf, start, 2)?;
            let len = usize::from(u16::from_be_bytes([buf[start], buf[start + 1]]));
            decode_string_body(buf, start + 2, len)
        }

        m::STRING32 => {
            require_bytes_from(buf, start, 4)?;
            let raw =
                u32::from_be_bytes([buf[start], buf[start + 1], buf[start + 2], buf[start + 3]]);
            let len = raw as usize;
            decode_string_body(buf, start + 4, len)
        }

        m::LIST8 => {
            require_bytes_from(buf, start, 1)?;
            let count = usize::from(buf[start]);
            decode_list_body(buf, start + 1, count, depth)
        }

        m::LIST16 => {
            require_bytes_from(buf, start, 2)?;
            let count = usize::from(u16::from_be_bytes([buf[start], buf[start + 1]]));
            decode_list_body(buf, start + 2, count, depth)
        }

        m::LIST32 => {
            require_bytes_from(buf, start, 4)?;
            let raw =
                u32::from_be_bytes([buf[start], buf[start + 1], buf[start + 2], buf[start + 3]]);
            let count = raw as usize;
            decode_list_body(buf, start + 4, count, depth)
        }

        m::DICT8 => {
            require_bytes_from(buf, start, 1)?;
            let count = usize::from(buf[start]);
            decode_dict_body(buf, start + 1, count, depth)
        }

        m::DICT16 => {
            require_bytes_from(buf, start, 2)?;
            let count = usize::from(u16::from_be_bytes([buf[start], buf[start + 1]]));
            decode_dict_body(buf, start + 2, count, depth)
        }

        m::DICT32 => {
            require_bytes_from(buf, start, 4)?;
            let raw =
                u32::from_be_bytes([buf[start], buf[start + 1], buf[start + 2], buf[start + 3]]);
            let count = raw as usize;
            decode_dict_body(buf, start + 4, count, depth)
        }

        // TinyInt negative: 0xF0–0xFF
        // Safe: u8 in 0xF0..=0xFF interpreted as i8 gives -16..=-1.
        #[allow(clippy::cast_possible_wrap)]
        0xF0..=0xFF => Ok((PackStreamValue::Int(i64::from(marker as i8)), start)),

        // Unrecognised marker bytes (covers gaps like 0xC4–0xC7, 0xD3, 0xD7, etc.)
        other => Err(ProtocolError::PackStreamUnknownMarker { marker: other }),
    }
}

// ---------------------------------------------------------------------------
// Integer helper — extracted from the main dispatch to reduce its line count
// ---------------------------------------------------------------------------

fn decode_integer_at(buf: &[u8], marker: u8, start: usize) -> Result<(PackStreamValue, usize)> {
    match marker {
        m::INT8 => {
            require_bytes_from(buf, start, 1)?;
            // Safe: i8 from u8 intentionally wraps (wire format).
            #[allow(clippy::cast_possible_wrap)]
            let value = buf[start] as i8;
            Ok((PackStreamValue::Int(i64::from(value)), start + 1))
        }

        m::INT16 => {
            require_bytes_from(buf, start, 2)?;
            let value = i16::from_be_bytes([buf[start], buf[start + 1]]);
            Ok((PackStreamValue::Int(i64::from(value)), start + 2))
        }

        m::INT32 => {
            require_bytes_from(buf, start, 4)?;
            let value =
                i32::from_be_bytes([buf[start], buf[start + 1], buf[start + 2], buf[start + 3]]);
            Ok((PackStreamValue::Int(i64::from(value)), start + 4))
        }

        m::INT64 => {
            require_bytes_from(buf, start, 8)?;
            let value = i64::from_be_bytes([
                buf[start],
                buf[start + 1],
                buf[start + 2],
                buf[start + 3],
                buf[start + 4],
                buf[start + 5],
                buf[start + 6],
                buf[start + 7],
            ]);
            Ok((PackStreamValue::Int(value), start + 8))
        }

        // Caller guarantees only INT8/INT16/INT32/INT64 are passed here.
        _ => unreachable!("decode_integer_at called with non-integer marker 0x{marker:02X}"),
    }
}

// ---------------------------------------------------------------------------
// Body decoders
// ---------------------------------------------------------------------------

fn decode_string_body(buf: &[u8], offset: usize, len: usize) -> Result<(PackStreamValue, usize)> {
    require_bytes_from(buf, offset, len)?;
    let s = std::str::from_utf8(&buf[offset..offset + len])
        .map_err(|_| ProtocolError::PackStreamInvalidUtf8)?;
    Ok((PackStreamValue::String(s.to_owned()), offset + len))
}

fn decode_bytes_body(buf: &[u8], offset: usize, len: usize) -> Result<(PackStreamValue, usize)> {
    require_bytes_from(buf, offset, len)?;
    Ok((
        PackStreamValue::Bytes(buf[offset..offset + len].to_vec()),
        offset + len,
    ))
}

fn decode_list_body(
    buf: &[u8],
    mut offset: usize,
    count: usize,
    depth: usize,
) -> Result<(PackStreamValue, usize)> {
    if depth >= MAX_DECODE_DEPTH {
        return Err(ProtocolError::PackStreamDepthLimitExceeded {
            max: MAX_DECODE_DEPTH,
        });
    }
    // Cap pre-allocation: a malicious LIST32 claiming 4 billion items must not
    // cause OOM before we attempt to read any element (which will underflow).
    let prealloc = count
        .min(buf.len().saturating_sub(offset))
        .min(MAX_PREALLOC);
    let mut items = Vec::with_capacity(prealloc);
    for _ in 0..count {
        let (item, next) = decode_at(buf, offset, depth + 1)?;
        items.push(item);
        offset = next;
    }
    Ok((PackStreamValue::List(items), offset))
}

fn decode_dict_body(
    buf: &[u8],
    mut offset: usize,
    count: usize,
    depth: usize,
) -> Result<(PackStreamValue, usize)> {
    if depth >= MAX_DECODE_DEPTH {
        return Err(ProtocolError::PackStreamDepthLimitExceeded {
            max: MAX_DECODE_DEPTH,
        });
    }
    // Cap pre-allocation for the same reason as decode_list_body.
    let prealloc = count
        .min(buf.len().saturating_sub(offset))
        .min(MAX_PREALLOC);
    let mut pairs = Vec::with_capacity(prealloc);
    for _ in 0..count {
        let (key_val, next) = decode_at(buf, offset, depth + 1)?;
        let PackStreamValue::String(key) = key_val else {
            return Err(ProtocolError::PackStreamDictKeyNotString);
        };
        let (value, next2) = decode_at(buf, next, depth + 1)?;
        pairs.push((key, value));
        offset = next2;
    }
    Ok((PackStreamValue::Dict(pairs), offset))
}

fn decode_struct_body(
    buf: &[u8],
    mut offset: usize,
    tag: u8,
    field_count: usize,
    depth: usize,
) -> Result<(PackStreamValue, usize)> {
    if depth >= MAX_DECODE_DEPTH {
        return Err(ProtocolError::PackStreamDepthLimitExceeded {
            max: MAX_DECODE_DEPTH,
        });
    }
    let mut fields = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let (field, next) = decode_at(buf, offset, depth + 1)?;
        fields.push(field);
        offset = next;
    }
    Ok((PackStreamValue::Struct { tag, fields }, offset))
}

// ---------------------------------------------------------------------------
// Buffer bounds helpers
// ---------------------------------------------------------------------------

const fn require_bytes(buf: &[u8], needed: usize) -> Result<()> {
    if buf.len() < needed {
        return Err(ProtocolError::PackStreamUnderflow {
            needed,
            available: buf.len(),
        });
    }
    Ok(())
}

const fn require_bytes_from(buf: &[u8], offset: usize, needed: usize) -> Result<()> {
    let available = buf.len().saturating_sub(offset);
    if available < needed {
        return Err(ProtocolError::PackStreamUnderflow { needed, available });
    }
    Ok(())
}
