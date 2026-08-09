// SPDX-License-Identifier: MIT

use crate::Error;
use crate::error::Result;
use crate::property::{Properties, Property};

const TAG_STRING: u8 = 0x01;
const TAG_I64: u8 = 0x02;
const TAG_F64: u8 = 0x03;
const TAG_BOOL: u8 = 0x04;
const TAG_BYTES: u8 = 0x05;

/// Serializes a `Properties` map into a byte vector.
///
/// Format per entry:
/// ```text
/// key_len: u8
/// key: [u8; key_len]
/// type_tag: u8
/// value: (type-dependent)
/// ```
///
/// # Errors
///
/// Returns [`Error::RecordTooLarge`] when a key or value is too long for the
/// width the format records its length in:
///
/// | Part | Width | Limit |
/// |---|---|---|
/// | key | `u8` | 255 bytes |
/// | [`Property::String`] | `u32` | 4,294,967,295 bytes |
/// | [`Property::Bytes`] | `u32` | 4,294,967,295 bytes |
///
/// String and blob limits are on **bytes**, not characters. The string width
/// was a `u16` (65,535-byte cap) until issue #75 widened it: real legal text
/// (a 283,718-byte aggregated `full_text`) legitimately exceeds 64 KiB.
///
/// Rejecting is the point — see [`encode_value`] for what the previous silent
/// truncation did to the data (issue #62).
pub fn encode_properties(props: &Properties) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    for (key, value) in props {
        let key_bytes = key.as_bytes();
        if key_bytes.len() > 255 {
            return Err(Error::RecordTooLarge {
                size: key_bytes.len(),
            });
        }
        // The `> 255` check immediately above returns RecordTooLarge first.
        #[allow(clippy::cast_possible_truncation)]
        buf.push(key_bytes.len() as u8);
        buf.extend_from_slice(key_bytes);
        encode_value(value, &mut buf)?;
    }
    Ok(buf)
}

/// Deserializes `prop_count` properties from a byte slice.
///
/// Returns the decoded map and number of bytes consumed.
pub fn decode_properties(
    data: &[u8],
    prop_count: u16,
    page_id: u32,
) -> Result<(Properties, usize)> {
    let mut props = Properties::new();
    let mut offset = 0;

    for _ in 0..prop_count {
        let key_len = *read_byte(data, offset, "property key_len", page_id)? as usize;
        offset += 1;

        if offset + key_len > data.len() {
            return Err(corrupt("property key truncated", page_id));
        }
        let key = std::str::from_utf8(&data[offset..offset + key_len])
            .map_err(|_| corrupt("property key is not valid UTF-8", page_id))?
            .to_owned();
        offset += key_len;

        let (value, consumed) = decode_value(data, offset, page_id)?;
        offset += consumed;

        props.insert(key, value);
    }

    Ok((props, offset))
}

/// Deserializes only the properties whose keys appear in `keys`.
///
/// Properties not in `keys` are skipped without allocating.
/// The full byte stream is still consumed (offset advances past all entries)
/// because the format is sequential with no jump table.
///
/// Returns the projected map and number of bytes consumed (same as full decode).
pub fn decode_properties_projected(
    data: &[u8],
    prop_count: u16,
    keys: &[&str],
    page_id: u32,
) -> Result<(Properties, usize)> {
    let mut props = Properties::new();
    let mut offset = 0;

    for _ in 0..prop_count {
        let key_len = *read_byte(data, offset, "property key_len", page_id)? as usize;
        offset += 1;

        if offset + key_len > data.len() {
            return Err(corrupt("property key truncated", page_id));
        }
        let key_bytes = &data[offset..offset + key_len];
        let matches = keys.iter().any(|k| k.as_bytes() == key_bytes);
        offset += key_len;

        if matches {
            let key = std::str::from_utf8(key_bytes)
                .map_err(|_| corrupt("property key is not valid UTF-8", page_id))?
                .to_owned();
            let (value, consumed) = decode_value(data, offset, page_id)?;
            offset += consumed;
            props.insert(key, value);
        } else {
            let skipped = skip_value(data, offset, page_id)?;
            offset += skipped;
        }
    }

    Ok((props, offset))
}

/// Encodes one value as `tag + length + payload`.
///
/// # Errors
///
/// Returns [`Error::RecordTooLarge`] when a value's length does not fit the
/// width the format records it in: `u32` for both strings and blobs (strings
/// were `u16` until issue #75). Writing it anyway would store a wrapped
/// length — the payload lands on disk whole while its recorded length reads
/// as `len % 2^width`, so the decoder resumes in the middle of the value.
/// Measured before this guard existed (issue #62, on the old u16 width): a
/// 65,536-byte string raised `unknown property type tag` on read, while a
/// 70,000-byte one returned a 4,464-byte string and **no error at all**. The
/// second is the dangerous one — nothing signalled that the data was mangled.
fn encode_value(value: &Property, buf: &mut Vec<u8>) -> Result<()> {
    match value {
        Property::String(s) => {
            let bytes = s.as_bytes();
            let len = u32::try_from(bytes.len())
                .map_err(|_| Error::RecordTooLarge { size: bytes.len() })?;
            buf.push(TAG_STRING);
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(bytes);
        }
        Property::I64(v) => {
            buf.push(TAG_I64);
            buf.extend_from_slice(&v.to_le_bytes());
        }
        Property::F64(v) => {
            buf.push(TAG_F64);
            buf.extend_from_slice(&v.to_le_bytes());
        }
        Property::Bool(v) => {
            buf.push(TAG_BOOL);
            buf.push(u8::from(*v));
        }
        Property::Bytes(v) => {
            let len =
                u32::try_from(v.len()).map_err(|_| Error::RecordTooLarge { size: v.len() })?;
            buf.push(TAG_BYTES);
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(v);
        }
    }
    Ok(())
}

fn decode_value(data: &[u8], offset: usize, page_id: u32) -> Result<(Property, usize)> {
    let tag = *read_byte(data, offset, "property type tag", page_id)?;
    let mut pos = offset + 1;

    match tag {
        TAG_STRING => {
            let len = read_u32_le(data, pos, "string length", page_id)? as usize;
            pos += 4;
            if pos + len > data.len() {
                return Err(corrupt("string value truncated", page_id));
            }
            let s = std::str::from_utf8(&data[pos..pos + len])
                .map_err(|_| corrupt("string value is not valid UTF-8", page_id))?
                .to_owned();
            pos += len;
            Ok((Property::String(s), pos - offset))
        }
        TAG_I64 => {
            let v = read_i64_le(data, pos, "i64 value", page_id)?;
            pos += 8;
            Ok((Property::I64(v), pos - offset))
        }
        TAG_F64 => {
            let v = read_f64_le(data, pos, "f64 value", page_id)?;
            pos += 8;
            Ok((Property::F64(v), pos - offset))
        }
        TAG_BOOL => {
            let b = *read_byte(data, pos, "bool value", page_id)?;
            pos += 1;
            Ok((Property::Bool(b != 0), pos - offset))
        }
        TAG_BYTES => {
            let len = read_u32_le(data, pos, "bytes length", page_id)? as usize;
            pos += 4;
            if pos + len > data.len() {
                return Err(corrupt("bytes value truncated", page_id));
            }
            let v = data[pos..pos + len].to_vec();
            pos += len;
            Ok((Property::Bytes(v), pos - offset))
        }
        _ => Err(corrupt("unknown property type tag", page_id)),
    }
}

/// Advances past a property value without allocating.
///
/// Returns the number of bytes consumed (tag + payload).
fn skip_value(data: &[u8], offset: usize, page_id: u32) -> Result<usize> {
    let tag = *read_byte(data, offset, "property type tag (skip)", page_id)?;
    match tag {
        TAG_STRING => {
            let len = read_u32_le(data, offset + 1, "string length (skip)", page_id)? as usize;
            let total = 1 + 4 + len; // tag + u32 + string bytes
            if offset + total > data.len() {
                return Err(corrupt("string value truncated (skip)", page_id));
            }
            Ok(total)
        }
        TAG_I64 | TAG_F64 => {
            if offset + 9 > data.len() {
                return Err(corrupt("numeric value truncated (skip)", page_id));
            }
            Ok(9) // tag + 8 bytes
        }
        TAG_BOOL => {
            if offset + 2 > data.len() {
                return Err(corrupt("bool value truncated (skip)", page_id));
            }
            Ok(2) // tag + 1 byte
        }
        TAG_BYTES => {
            let len = read_u32_le(data, offset + 1, "bytes length (skip)", page_id)? as usize;
            let total = 1 + 4 + len; // tag + u32 + blob bytes
            if offset + total > data.len() {
                return Err(corrupt("bytes value truncated (skip)", page_id));
            }
            Ok(total)
        }
        _ => Err(corrupt("unknown property type tag (skip)", page_id)),
    }
}

fn read_byte<'a>(data: &'a [u8], offset: usize, ctx: &'static str, page_id: u32) -> Result<&'a u8> {
    data.get(offset).ok_or_else(|| corrupt(ctx, page_id))
}

fn read_u32_le(data: &[u8], offset: usize, ctx: &'static str, page_id: u32) -> Result<u32> {
    if offset + 4 > data.len() {
        return Err(corrupt(ctx, page_id));
    }
    Ok(u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]))
}

fn read_i64_le(data: &[u8], offset: usize, ctx: &'static str, page_id: u32) -> Result<i64> {
    if offset + 8 > data.len() {
        return Err(corrupt(ctx, page_id));
    }
    let bytes: [u8; 8] = data[offset..offset + 8].try_into().unwrap();
    Ok(i64::from_le_bytes(bytes))
}

fn read_f64_le(data: &[u8], offset: usize, ctx: &'static str, page_id: u32) -> Result<f64> {
    if offset + 8 > data.len() {
        return Err(corrupt(ctx, page_id));
    }
    let bytes: [u8; 8] = data[offset..offset + 8].try_into().unwrap();
    Ok(f64::from_le_bytes(bytes))
}

const fn corrupt(reason: &'static str, page_id: u32) -> Error {
    Error::CorruptPage {
        file: "properties",
        page_id,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(props: &Properties) -> Properties {
        let encoded = encode_properties(props).unwrap();
        // Test fixture: a handful of properties built in the same test.
        #[allow(clippy::cast_possible_truncation)]
        let prop_count = props.len() as u16;
        let (decoded, consumed) = decode_properties(&encoded, prop_count, 0).unwrap();
        assert_eq!(consumed, encoded.len());
        decoded
    }

    fn single_prop(key: &str, value: Property) -> Properties {
        let mut map = Properties::new();
        map.insert(key.to_owned(), value);
        map
    }

    #[test]
    fn encode_decode_string() {
        let props = single_prop("name", Property::String("hello".into()));
        let decoded = roundtrip(&props);
        assert_eq!(
            decoded.get("name").unwrap(),
            &Property::String("hello".into())
        );
    }

    #[test]
    fn encode_decode_i64() {
        let props = single_prop("count", Property::I64(-42));
        let decoded = roundtrip(&props);
        assert_eq!(decoded.get("count").unwrap(), &Property::I64(-42));
    }

    #[test]
    fn encode_decode_f64() {
        let props = single_prop("ratio", Property::F64(1.23456));
        let decoded = roundtrip(&props);
        assert_eq!(decoded.get("ratio").unwrap(), &Property::F64(1.23456));
    }

    #[test]
    fn encode_decode_bool_true() {
        let props = single_prop("active", Property::Bool(true));
        let decoded = roundtrip(&props);
        assert_eq!(decoded.get("active").unwrap(), &Property::Bool(true));
    }

    #[test]
    fn encode_decode_bool_false() {
        let props = single_prop("active", Property::Bool(false));
        let decoded = roundtrip(&props);
        assert_eq!(decoded.get("active").unwrap(), &Property::Bool(false));
    }

    #[test]
    fn encode_decode_bytes() {
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let props = single_prop("payload", Property::Bytes(data.clone()));
        let decoded = roundtrip(&props);
        assert_eq!(decoded.get("payload").unwrap(), &Property::Bytes(data));
    }

    #[test]
    fn encode_decode_empty_props() {
        let props = Properties::new();
        let encoded = encode_properties(&props).unwrap();
        assert!(encoded.is_empty());
        let (decoded, consumed) = decode_properties(&encoded, 0, 0).unwrap();
        assert!(decoded.is_empty());
        assert_eq!(consumed, 0);
    }

    #[test]
    fn encode_decode_multiple_props() {
        let mut props = Properties::new();
        props.insert("s".into(), Property::String("val".into()));
        props.insert("i".into(), Property::I64(999));
        props.insert("f".into(), Property::F64(1.5));
        props.insert("b".into(), Property::Bool(true));
        props.insert("d".into(), Property::Bytes(vec![1, 2, 3]));

        let decoded = roundtrip(&props);
        assert_eq!(decoded.len(), 5);
        assert_eq!(decoded.get("s").unwrap(), &Property::String("val".into()));
        assert_eq!(decoded.get("i").unwrap(), &Property::I64(999));
        assert_eq!(decoded.get("f").unwrap(), &Property::F64(1.5));
        assert_eq!(decoded.get("b").unwrap(), &Property::Bool(true));
        assert_eq!(decoded.get("d").unwrap(), &Property::Bytes(vec![1, 2, 3]));
    }

    #[test]
    fn encode_decode_empty_string_value() {
        let props = single_prop("key", Property::String(String::new()));
        let decoded = roundtrip(&props);
        assert_eq!(
            decoded.get("key").unwrap(),
            &Property::String(String::new())
        );
    }

    #[test]
    fn encode_decode_max_i64() {
        let props = single_prop("max", Property::I64(i64::MAX));
        let decoded = roundtrip(&props);
        assert_eq!(decoded.get("max").unwrap(), &Property::I64(i64::MAX));
    }

    #[test]
    fn encode_decode_min_i64() {
        let props = single_prop("min", Property::I64(i64::MIN));
        let decoded = roundtrip(&props);
        assert_eq!(decoded.get("min").unwrap(), &Property::I64(i64::MIN));
    }

    #[test]
    fn encode_decode_f64_nan() {
        let props = single_prop("nan", Property::F64(f64::NAN));
        let encoded = encode_properties(&props).unwrap();
        let (decoded, _) = decode_properties(&encoded, 1, 0).unwrap();
        match decoded.get("nan").unwrap() {
            Property::F64(v) => assert!(v.is_nan()),
            other => panic!("expected F64, got {other:?}"),
        }
    }

    #[test]
    fn encode_decode_f64_infinity() {
        let props = single_prop("inf", Property::F64(f64::INFINITY));
        let decoded = roundtrip(&props);
        assert_eq!(decoded.get("inf").unwrap(), &Property::F64(f64::INFINITY));
    }

    #[test]
    fn encode_decode_f64_neg_infinity() {
        let props = single_prop("ninf", Property::F64(f64::NEG_INFINITY));
        let decoded = roundtrip(&props);
        assert_eq!(
            decoded.get("ninf").unwrap(),
            &Property::F64(f64::NEG_INFINITY)
        );
    }

    #[test]
    fn encode_decode_empty_bytes() {
        let props = single_prop("empty", Property::Bytes(vec![]));
        let decoded = roundtrip(&props);
        assert_eq!(decoded.get("empty").unwrap(), &Property::Bytes(vec![]));
    }

    #[test]
    fn encode_decode_max_key_length() {
        let key = "k".repeat(255);
        let props = single_prop(&key, Property::I64(1));
        let decoded = roundtrip(&props);
        assert_eq!(decoded.get(&key).unwrap(), &Property::I64(1));
    }

    #[test]
    fn little_endian_encoding() {
        let props = single_prop("v", Property::I64(0x0102_0304_0506_0708));
        let encoded = encode_properties(&props).unwrap();
        // key_len(1) + key "v"(1) + tag(1) = offset 3
        // i64 bytes start at offset 3
        let i64_bytes = &encoded[3..11];
        assert_eq!(i64_bytes, &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn encode_rejects_key_over_255_bytes() {
        let key = "k".repeat(256);
        let props = single_prop(&key, Property::I64(1));
        let result = encode_properties(&props);
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::RecordTooLarge { size } => assert_eq!(size, 256),
            other => panic!("expected RecordTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn encode_accepts_key_exactly_255_bytes() {
        let key = "k".repeat(255);
        let props = single_prop(&key, Property::I64(1));
        let result = encode_properties(&props);
        assert!(result.is_ok());
    }

    // --- Value length limits (issues #62 and #75) ---
    //
    // A string's byte length is stored in a u32 (a u16 until issue #75).
    // Under the old width, past 65,535 the cast used to wrap silently (issue
    // #62): the value was written whole but its recorded length was
    // `len % 65536`, so the decoder resumed reading in the middle of the
    // text — 65,536 bytes raised `unknown property type tag` while 70,000
    // quietly returned a 4,464-byte string and no error at all. v0.11.1
    // turned the wrap into an explicit RecordTooLarge; issue #75 then raised
    // the limit itself, because real legal text exceeds 64 KiB legitimately.
    //
    // The u32 rejection path (> 4,294,967,295 bytes) is not exercised with
    // real data: materializing a 4 GiB string in a unit test costs more than
    // it proves, and the guard is the same one-line `u32::try_from` already
    // exercised for `Property::Bytes`. Same precedent as overflow_codec.rs,
    // which documents rather than allocates its u32 boundary.

    #[test]
    fn encode_accepts_string_exactly_65535_bytes() {
        let props = single_prop("text", Property::String("x".repeat(65_535)));
        assert!(
            encode_properties(&props).is_ok(),
            "65,535 was the old u16 cap — now an ordinary size, still accepted"
        );
    }

    /// Byte length, not character count: multibyte text used to hit the old
    /// u16 cap sooner than its `chars()` count suggested. 66,000 bytes sits
    /// between the old and new limits and must now be accepted.
    #[test]
    fn encode_accepts_multibyte_string_between_old_and_new_limit() {
        // 33,000 two-byte chars = 66,000 bytes — over the old u16 cap,
        // far under the u32 one.
        let props = single_prop("text", Property::String("á".repeat(33_000)));
        assert!(
            encode_properties(&props).is_ok(),
            "66,000 bytes exceeded the old u16 cap; the u32 width holds it easily"
        );
    }

    /// The real-world size that motivated widening the prefix (issue #75):
    /// the `full_text` of the AI Act's `Preamble` node measures 283,718
    /// bytes. Under the old u16 prefix this was rejected with
    /// `RecordTooLarge`; the format must accept it.
    #[test]
    fn encode_accepts_string_over_u16_limit() {
        let props = single_prop("full_text", Property::String("x".repeat(283_718)));
        assert!(
            encode_properties(&props).is_ok(),
            "a 283,718-byte string is a legitimate legal-text value (issue #75)"
        );
    }

    /// Round-trip of a value past the old u16 boundary: the decoder must read
    /// the 4-byte length and hand back the exact value, consuming the whole
    /// buffer (a stale 2-byte read would desynchronize everything after it).
    #[test]
    fn roundtrip_string_over_u16_limit() {
        let text = "x".repeat(283_718);
        let props = single_prop("full_text", Property::String(text.clone()));
        let encoded = encode_properties(&props).unwrap();

        let (decoded, consumed) = decode_properties(&encoded, 1, 0).unwrap();
        assert_eq!(consumed, encoded.len(), "must consume the entire buffer");
        assert_eq!(decoded.get("full_text"), Some(&Property::String(text)));
    }

    /// Projected decode skips the oversized value without reading it: if
    /// `skip_value` still assumed a 2-byte length, the cursor would land in
    /// the middle of the payload and the next entry would decode as garbage.
    #[test]
    fn decode_projected_skips_string_over_u16_limit() {
        let mut props = Properties::new();
        props.insert("big".into(), Property::String("x".repeat(283_718)));
        props.insert("count".into(), Property::I64(42));

        let encoded = encode_properties(&props).unwrap();
        let (projected, consumed) =
            decode_properties_projected(&encoded, 2, &["count"], 0).unwrap();

        assert_eq!(consumed, encoded.len(), "must consume the entire buffer");
        assert_eq!(projected.len(), 1);
        assert_eq!(projected.get("count"), Some(&Property::I64(42)));
    }

    /// `Property::Bytes` records its length in a u32, so its limit is far
    /// higher — but the same silent-wrap hazard exists, and the guard must
    /// cover it rather than leaving one of the two variants unchecked.
    #[test]
    fn encode_accepts_blob_far_past_the_string_limit() {
        let props = single_prop("blob", Property::Bytes(vec![0u8; 100_000]));
        assert!(
            encode_properties(&props).is_ok(),
            "a u32 length holds 100,000 bytes comfortably — only String is capped at u16"
        );
    }

    // --- Projected decode tests ---

    #[test]
    fn decode_projected_returns_only_requested_keys() {
        let mut props = Properties::new();
        props.insert("name".into(), Property::String("Alice".into()));
        props.insert("age".into(), Property::I64(30));
        props.insert("score".into(), Property::F64(9.5));

        let encoded = encode_properties(&props).unwrap();
        let (projected, consumed) = decode_properties_projected(&encoded, 3, &["name"], 0).unwrap();

        assert_eq!(projected.len(), 1);
        assert_eq!(
            projected.get("name").unwrap(),
            &Property::String("Alice".into())
        );
        assert_eq!(consumed, encoded.len(), "must consume all bytes");
    }

    #[test]
    fn decode_projected_nonexistent_key_returns_empty() {
        let mut props = Properties::new();
        props.insert("name".into(), Property::String("Alice".into()));
        props.insert("age".into(), Property::I64(30));

        let encoded = encode_properties(&props).unwrap();
        let (projected, consumed) =
            decode_properties_projected(&encoded, 2, &["nonexistent"], 0).unwrap();

        assert!(projected.is_empty());
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn decode_projected_empty_keys_returns_empty() {
        let mut props = Properties::new();
        props.insert("name".into(), Property::String("Alice".into()));
        props.insert("age".into(), Property::I64(30));

        let encoded = encode_properties(&props).unwrap();
        let (projected, consumed) = decode_properties_projected(&encoded, 2, &[], 0).unwrap();

        assert!(projected.is_empty());
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn decode_projected_multiple_keys() {
        let mut props = Properties::new();
        props.insert("name".into(), Property::String("Bob".into()));
        props.insert("age".into(), Property::I64(25));
        props.insert("city".into(), Property::String("Madrid".into()));
        props.insert("active".into(), Property::Bool(true));
        props.insert("data".into(), Property::Bytes(vec![1, 2, 3]));

        let encoded = encode_properties(&props).unwrap();
        let (projected, consumed) =
            decode_properties_projected(&encoded, 5, &["name", "active"], 0).unwrap();

        assert_eq!(projected.len(), 2);
        assert_eq!(
            projected.get("name").unwrap(),
            &Property::String("Bob".into())
        );
        assert_eq!(projected.get("active").unwrap(), &Property::Bool(true));
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn decode_projected_all_keys_matches_full_decode() {
        let mut props = Properties::new();
        props.insert("a".into(), Property::I64(1));
        props.insert("b".into(), Property::F64(2.0));
        props.insert("c".into(), Property::Bool(false));

        let encoded = encode_properties(&props).unwrap();
        let (full, full_consumed) = decode_properties(&encoded, 3, 0).unwrap();
        let (projected, proj_consumed) =
            decode_properties_projected(&encoded, 3, &["a", "b", "c"], 0).unwrap();

        assert_eq!(full, projected);
        assert_eq!(full_consumed, proj_consumed);
    }

    #[test]
    fn skip_value_bool_truncated_at_tag_returns_err() {
        // Buffer contains only the TAG_BOOL byte — value byte is missing
        let result = skip_value(&[TAG_BOOL], 0, 0);
        assert!(
            result.is_err(),
            "expected Err for truncated bool, got {result:?}"
        );
    }

    #[test]
    fn skip_value_numeric_truncated_returns_err() {
        // Buffer contains only the TAG_I64 byte — 8 value bytes missing
        let result = skip_value(&[TAG_I64], 0, 0);
        assert!(
            result.is_err(),
            "expected Err for truncated i64, got {result:?}"
        );

        let result = skip_value(&[TAG_F64], 0, 0);
        assert!(
            result.is_err(),
            "expected Err for truncated f64, got {result:?}"
        );
    }

    #[test]
    fn skip_value_handles_all_types() {
        // Encode one of each type and verify skip_value returns the correct byte count
        let types: Vec<(&str, Property)> = vec![
            ("s", Property::String("hello".into())),
            ("i", Property::I64(42)),
            ("f", Property::F64(1.5)),
            ("b", Property::Bool(true)),
            ("d", Property::Bytes(vec![0xDE, 0xAD])),
        ];

        for (key, value) in types {
            let props = single_prop(key, value);
            let encoded = encode_properties(&props).unwrap();
            // After key_len(1) + key(key.len()), the value starts
            let value_offset = 1 + key.len();
            let skipped = skip_value(&encoded, value_offset, 0).unwrap();
            assert_eq!(
                value_offset + skipped,
                encoded.len(),
                "skip_value for key '{key}' should consume remaining bytes"
            );
        }
    }

    #[test]
    fn decode_projected_large_payload_skips_variable_width_entries() {
        let mut props = Properties::new();
        props.insert("big".into(), Property::Bytes(vec![0u8; 50]));
        props.insert("name".into(), Property::String("hello world".into()));
        props.insert("count".into(), Property::I64(42));
        props.insert("flag".into(), Property::Bool(true));

        let encoded = encode_properties(&props).unwrap();
        assert!(encoded.len() > 38, "payload must exceed inline limit");

        let (projected, consumed) =
            decode_properties_projected(&encoded, 4, &["count"], 0).unwrap();

        assert_eq!(projected.len(), 1);
        assert_eq!(projected.get("count").unwrap(), &Property::I64(42));
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn decode_properties_corrupt_error_carries_page_id() {
        // Truncated buffer — will fail during decode
        let truncated = &[0x01]; // just a key_len byte, no key data
        let err = decode_properties(truncated, 1, 42).unwrap_err();
        match err {
            Error::CorruptPage { page_id, .. } => assert_eq!(page_id, 42),
            other => panic!("expected CorruptPage, got {other:?}"),
        }
    }

    #[test]
    fn decode_properties_projected_corrupt_error_carries_page_id() {
        let truncated = &[0x01];
        let err = decode_properties_projected(truncated, 1, &["x"], 99).unwrap_err();
        match err {
            Error::CorruptPage { page_id, .. } => assert_eq!(page_id, 99),
            other => panic!("expected CorruptPage, got {other:?}"),
        }
    }
}

/// Property-based tests (issue #67).
///
/// These complement the hand-written examples above rather than replacing
/// them. The distinction that matters: an example pins one input someone
/// thought of; these generate thousands nobody thought of, and when one fails
/// proptest shrinks it to the smallest input that still breaks.
///
/// That is not theoretical here. Run against the engine as it stood before
/// #62 was fixed, this suite found the silent-truncation bug and shrank it to
/// a 70,590-byte string — just past the then-65,535 limit (widened by #75) —
/// with nobody having told it that number mattered.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    /// Byte lengths drawn to land near a format limit far more often than a
    /// uniform distribution would.
    ///
    /// Uniform sizes are close to useless for this: over a 0..131,070 range,
    /// the odds of landing within a few bytes of 65,535 are vanishing, and
    /// that neighbourhood is exactly where every bug of this class has lived.
    /// So the distribution is deliberately lumpy — a third of the mass sits on
    /// the boundary, and another third above it so the reject path gets
    /// exercised as often as the accept path.
    fn size_around(limit: usize) -> impl Strategy<Value = usize> {
        prop_oneof![
            4 => 0..=limit,                                  // ordinary values
            3 => limit.saturating_sub(5)..=limit + 5,        // the boundary itself
            3 => (limit + 1)..=(limit * 2).max(limit + 2),   // over it: must reject
        ]
    }

    /// ASCII only, so one character is one byte and the generated length *is*
    /// the byte length. The limit being on bytes rather than characters is
    /// already pinned by `encode_rejects_multibyte_string_over_the_byte_limit`;
    /// generating random multibyte text here would add running time without
    /// covering anything that example does not.
    fn ascii_of_size(size: impl Strategy<Value = usize>) -> impl Strategy<Value = String> {
        size.prop_map(|n| "x".repeat(n))
    }

    /// Every `Property` variant, with the length-carrying ones drawn around
    /// their limits.
    ///
    /// `f64` excludes NaN on purpose: this suite asserts round-trip equality,
    /// and `NaN != NaN` however faithfully the bytes survive, so including it
    /// would fail on every run while proving nothing. NaN and the infinities
    /// are covered by `encode_decode_f64_nan` and `encode_decode_f64_infinity`,
    /// which assert on `is_nan()` instead of equality — the right shape for
    /// that case, and one a generator cannot express.
    /// String sizes cover two bands: the old u16 boundary (no longer a limit
    /// since #75, but the neighbourhood where #62's corruption lived — kept
    /// as an ordinary-large size) and a lighter band around the real case
    /// that motivated #75 (~283 KB), weighted down because each such case
    /// moves ~300 KB through the codec.
    fn string_size() -> impl Strategy<Value = usize> {
        prop_oneof![
            8 => size_around(u16::MAX as usize),
            1 => 279_000..=288_000usize,
        ]
    }

    fn property_strategy() -> impl Strategy<Value = Property> {
        prop_oneof![
            ascii_of_size(string_size()).prop_map(Property::String),
            any::<i64>().prop_map(Property::I64),
            any::<f64>()
                .prop_filter("NaN breaks equality, covered by its own example", |f| !f
                    .is_nan())
                .prop_map(Property::F64),
            any::<bool>().prop_map(Property::Bool),
            proptest::collection::vec(any::<u8>(), 0..2048).prop_map(Property::Bytes),
        ]
    }

    /// Keys are capped at 255 bytes by a `u8` length, so they get the same
    /// boundary treatment as values — a different limit, the same failure mode.
    fn properties_strategy() -> impl Strategy<Value = Properties> {
        proptest::collection::hash_map(
            ascii_of_size(size_around(u8::MAX as usize)),
            property_strategy(),
            0..8,
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// The invariant all four #62 bugs violated. Note what it does *not*
        /// say: encoding is allowed to fail. A codec refusing a value it
        /// cannot represent is correct behaviour — that refusal is the #62
        /// fix. What it may never do is accept a value and hand back something
        /// else. Stated as "everything encodes", this test would reject the
        /// very fix it exists to protect.
        #[test]
        fn roundtrip_matches_or_encode_rejects_but_never_corrupts(
            props in properties_strategy()
        ) {
            let Ok(encoded) = encode_properties(&props) else {
                return Ok(()); // refusing is a valid answer
            };

            let count = u16::try_from(props.len()).expect("generator caps at 8 entries");
            let (decoded, consumed) = decode_properties(&encoded, count, 0)
                .expect("what encode accepted, decode must be able to read");

            prop_assert_eq!(decoded, props, "round-trip changed the data");
            prop_assert_eq!(
                consumed,
                encoded.len(),
                "decode consumed a different number of bytes than encode produced — \
                 this is what desynchronises everything that follows in the stream"
            );
        }

        /// Decoding must never abort the process, whatever the bytes say.
        /// A panic here would be reachable from a corrupt page on disk, which
        /// turns a recoverable read error into a crashed database.
        ///
        /// `prop_count` is generated independently of the bytes on purpose:
        /// the point is total desynchronisation between header and payload.
        #[test]
        fn decode_never_panics_on_arbitrary_bytes(
            garbage in proptest::collection::vec(any::<u8>(), 0..1024),
            prop_count in any::<u16>(),
        ) {
            // Reaching the assertion at all is the assertion: a panic inside
            // decode would fail this test by unwinding, with the shrunk input
            // reported.
            let outcome = decode_properties(&garbage, prop_count, 0);
            prop_assert!(outcome.is_ok() || outcome.is_err());
        }

        /// The projected decode walks the same bytes with a different code
        /// path (it skips values instead of materialising them), so it needs
        /// its own guarantee: asking for every key must produce exactly what
        /// the full decode produces.
        #[test]
        fn projected_decode_with_all_keys_matches_full_decode(
            props in properties_strategy()
        ) {
            let Ok(encoded) = encode_properties(&props) else {
                return Ok(());
            };
            let count = u16::try_from(props.len()).expect("generator caps at 8 entries");

            let keys: Vec<&str> = props.keys().map(String::as_str).collect();
            let (projected, projected_consumed) =
                decode_properties_projected(&encoded, count, &keys, 0)
                    .expect("what encode accepted, projected decode must read");
            let (full, full_consumed) = decode_properties(&encoded, count, 0)
                .expect("what encode accepted, decode must read");

            prop_assert_eq!(projected, full, "projecting every key lost or changed data");
            prop_assert_eq!(
                projected_consumed, full_consumed,
                "the two decode paths disagree on how many bytes the entries occupy"
            );
        }

        /// `encode_properties` is a pure function with no heap or cursor to
        /// leave half-written, so "no partial effects" reduces to "the same
        /// input gives the same answer". That is a weaker claim than the one
        /// `string_codec` needs — there, a rejected append must not move the
        /// write cursor — and it is weaker on purpose: there is no mutable
        /// state here to inspect.
        /// Since #75 widened the value prefix to u32, the only rejection
        /// boundary a test can reach with real data is the key length (255
        /// bytes) — a value would need to exceed 4 GiB.
        #[test]
        fn rejection_is_deterministic(
            oversized_key in ascii_of_size(256..320usize),
            value in ascii_of_size(0..64usize),
        ) {
            let mut props = Properties::new();
            props.insert(oversized_key, Property::String(value));

            let first = encode_properties(&props);
            let second = encode_properties(&props);

            prop_assert!(first.is_err(), "a key past the u8 limit must be refused");
            prop_assert_eq!(
                format!("{:?}", first.unwrap_err()),
                format!("{:?}", second.unwrap_err()),
                "the same input produced two different outcomes"
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

        /// The exact neighbourhood where #62 lived, at full size — plus the
        /// real-world size that motivated #75 (the AI Act's 283,718-byte
        /// `full_text`). Since #75 the whole range must be accepted and
        /// round-trip intact: there is no rejection boundary here any more.
        ///
        /// Kept `#[ignore]` because each case now moves 65-290 KB through
        /// encode and decode (~0.1s for 64 cases after #75, up from 0.03s at
        /// the old 60-80 KB sizes). The separation is about intent rather
        /// than time: the everyday suite covers the shape of the invariant,
        /// this one covers the sizes that actually broke, and is worth
        /// running deliberately after touching any length limit.
        ///
        /// Run with: `cargo test -p tessera-graph -- --ignored`
        #[test]
        #[ignore = "boundary sizes: 65-290KB payloads; run with --ignored (~0.1s)"]
        fn string_around_old_u16_boundary_and_real_case_never_corrupts(
            size in prop_oneof![
                // The old u16 boundary, where #62's wrap lived.
                65_530..=70_600usize,
                // The real case from #75, ± a page either side.
                279_000..=288_000usize,
            ]
        ) {
            let mut props = Properties::new();
            props.insert("k".to_owned(), Property::String("x".repeat(size)));

            let encoded = encode_properties(&props)
                .expect("every size in this range fits the u32 width");
            let (decoded, consumed) = decode_properties(&encoded, 1, 0)
                .expect("what encode accepted, decode must read");
            prop_assert_eq!(consumed, encoded.len());
            match decoded.get("k") {
                Some(Property::String(s)) => prop_assert_eq!(s.len(), size),
                other => prop_assert!(false, "expected a String, got {:?}", other),
            }
        }
    }
}
