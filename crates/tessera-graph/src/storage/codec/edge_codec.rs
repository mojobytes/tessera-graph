// SPDX-License-Identifier: Apache-2.0

use crate::edge::Edge;
use crate::error::{EdgeId, NodeId, Result};
use crate::storage::codec::node_codec::{label_hash, SLOT_EMPTY, SLOT_LIVE, SLOT_TOMBSTONE};
use crate::storage::codec::property_codec;

pub const EDGE_SLOT_SIZE: usize = 128;
pub const EDGE_LABEL_INLINE_MAX: usize = 55;
pub const EDGE_PROP_INLINE_MAX: usize = 30;

// Slot layout offsets
const OFF_FLAGS: usize = 0;
const OFF_ID: usize = 1;
/// Byte offset of the source node ID within an edge slot.
pub const OFF_SOURCE: usize = 9;
/// Byte offset of the target node ID within an edge slot.
pub const OFF_TARGET: usize = 17;
/// Byte offset of the CRC32 label hash within an edge slot.
pub const OFF_LABEL_HASH: usize = 25;
const OFF_LABEL_LEN: usize = 29;
const OFF_LABEL_INLINE: usize = 30;
const OFF_LABEL_OVERFLOW: usize = 85;
const OFF_PROP_COUNT: usize = 89;
const OFF_PROP_INLINE_LEN: usize = 91;
const OFF_PROP_INLINE: usize = 93;
const OFF_PROP_OVERFLOW: usize = 123;
const OFF_RESERVED: usize = 127;

const LABEL_OVERFLOW_MARKER: u8 = 0xFF;

/// Metadata about what overflowed during encoding.
pub struct EdgeSlotOverflow {
    pub label_overflowed: bool,
    pub label_bytes: Option<Vec<u8>>,
    pub props_overflowed: bool,
    pub props_bytes: Option<Vec<u8>>,
}

/// Encodes an `Edge` into a 128-byte slot buffer.
pub fn encode_edge_slot(edge: &Edge) -> Result<([u8; EDGE_SLOT_SIZE], EdgeSlotOverflow)> {
    let mut slot = [0u8; EDGE_SLOT_SIZE];
    let mut overflow = EdgeSlotOverflow {
        label_overflowed: false,
        label_bytes: None,
        props_overflowed: false,
        props_bytes: None,
    };

    slot[OFF_FLAGS] = SLOT_LIVE;

    // id
    slot[OFF_ID..=8].copy_from_slice(&edge.id.0.to_le_bytes());

    // source / target
    slot[OFF_SOURCE..OFF_SOURCE + 8].copy_from_slice(&edge.source.0.to_le_bytes());
    slot[OFF_TARGET..OFF_TARGET + 8].copy_from_slice(&edge.target.0.to_le_bytes());

    // label_hash
    let lh = label_hash(edge.label());
    slot[OFF_LABEL_HASH..OFF_LABEL_HASH + 4].copy_from_slice(&lh.to_le_bytes());

    // label
    let label_bytes = edge.label().as_bytes();
    if label_bytes.len() <= EDGE_LABEL_INLINE_MAX {
        // Inside the branch that checked `len <= EDGE_LABEL_INLINE_MAX` (< 255);
        // the else-branch takes the overflow path instead.
        #[allow(clippy::cast_possible_truncation)]
        let label_len = label_bytes.len() as u8;
        slot[OFF_LABEL_LEN] = label_len;
        slot[OFF_LABEL_INLINE..OFF_LABEL_INLINE + label_bytes.len()]
            .copy_from_slice(label_bytes);
    } else {
        slot[OFF_LABEL_LEN] = LABEL_OVERFLOW_MARKER;
        overflow.label_overflowed = true;
        overflow.label_bytes = Some(label_bytes.to_vec());
    }

    // properties
    let props_encoded = property_codec::encode_properties(edge.properties())?;
    // See `node_codec::encode_node_slot`: a wrapped count reads back as zero
    // properties, silently (issue #62).
    let prop_count = u16::try_from(edge.properties().len())
        .map_err(|_| crate::Error::RecordTooLarge { size: edge.properties().len() })?;
    slot[OFF_PROP_COUNT..OFF_PROP_COUNT + 2].copy_from_slice(&prop_count.to_le_bytes());

    if props_encoded.len() <= EDGE_PROP_INLINE_MAX {
        // Inside the branch that checked `len <= EDGE_PROP_INLINE_MAX`.
        #[allow(clippy::cast_possible_truncation)]
        let inline_len = props_encoded.len() as u16;
        slot[OFF_PROP_INLINE_LEN..OFF_PROP_INLINE_LEN + 2]
            .copy_from_slice(&inline_len.to_le_bytes());
        slot[OFF_PROP_INLINE..OFF_PROP_INLINE + props_encoded.len()]
            .copy_from_slice(&props_encoded);
    } else {
        slot[OFF_PROP_INLINE_LEN..OFF_PROP_INLINE_LEN + 2].copy_from_slice(&0u16.to_le_bytes());
        overflow.props_overflowed = true;
        overflow.props_bytes = Some(props_encoded);
    }

    slot[OFF_RESERVED] = 0;

    Ok((slot, overflow))
}

/// Decodes a 128-byte slot into an `Edge`.
///
/// Returns `Ok(None)` for empty or tombstoned slots.
/// `page_id` is used for error context when decoding fails.
pub fn decode_edge_slot(
    slot: &[u8; EDGE_SLOT_SIZE],
    page_id: u32,
    resolve_label: impl FnOnce(u32) -> Result<String>,
    resolve_props: impl FnOnce(u32) -> Result<Vec<u8>>,
) -> Result<Option<Edge>> {
    let flags = slot[OFF_FLAGS];
    if flags == SLOT_EMPTY || flags == SLOT_TOMBSTONE {
        return Ok(None);
    }

    let id = EdgeId(u64::from_le_bytes(
        slot[OFF_ID..=8].try_into().unwrap(),
    ));
    let source = NodeId(u64::from_le_bytes(
        slot[OFF_SOURCE..OFF_SOURCE + 8].try_into().unwrap(),
    ));
    let target = NodeId(u64::from_le_bytes(
        slot[OFF_TARGET..OFF_TARGET + 8].try_into().unwrap(),
    ));

    // label
    let label_len = slot[OFF_LABEL_LEN];
    let label = if label_len == LABEL_OVERFLOW_MARKER {
        let overflow_offset = u32::from_le_bytes(
            slot[OFF_LABEL_OVERFLOW..OFF_LABEL_OVERFLOW + 4]
                .try_into()
                .unwrap(),
        );
        resolve_label(overflow_offset)?
    } else {
        // Length byte comes off the page: a u8 reaches 254 where only
        // EDGE_LABEL_INLINE_MAX bytes exist, so slicing raw runs past the slot
        // and panics. See the same guard in `node_codec` (issue #67).
        let len = label_len as usize;
        if len > EDGE_LABEL_INLINE_MAX {
            return Err(crate::Error::CorruptPage {
                file: "edges.db",
                page_id,
                reason: "edge label length exceeds the inline capacity",
            });
        }
        std::str::from_utf8(&slot[OFF_LABEL_INLINE..OFF_LABEL_INLINE + len])
            .map_err(|_| crate::Error::CorruptPage {
                file: "edges.db",
                page_id,
                reason: "edge label is not valid UTF-8",
            })?
            .to_owned()
    };

    // properties
    let prop_overflow_offset = u32::from_le_bytes(
        slot[OFF_PROP_OVERFLOW..OFF_PROP_OVERFLOW + 4]
            .try_into()
            .unwrap(),
    );
    let prop_count = u16::from_le_bytes(
        slot[OFF_PROP_COUNT..OFF_PROP_COUNT + 2]
            .try_into()
            .unwrap(),
    );

    let inline_len = u16::from_le_bytes(
        slot[OFF_PROP_INLINE_LEN..OFF_PROP_INLINE_LEN + 2]
            .try_into()
            .unwrap(),
    ) as usize;

    // Same reasoning as the label above: a u16 can name far more bytes than
    // the slot holds, and this value slices it below (issue #67).
    if inline_len > EDGE_PROP_INLINE_MAX {
        return Err(crate::Error::CorruptPage {
            file: "edges.db",
            page_id,
            reason: "inline property length exceeds the inline capacity",
        });
    }

    // Properties overflowed when prop_count > 0 but inline_len == 0
    let properties = if prop_count > 0 && inline_len == 0 {
        let raw = resolve_props(prop_overflow_offset)?;
        let (props, _) = property_codec::decode_properties(&raw, prop_count, page_id)?;
        props
    } else {
        let (props, _) = property_codec::decode_properties(
            &slot[OFF_PROP_INLINE..OFF_PROP_INLINE + inline_len],
            prop_count,
            page_id,
        )?;
        props
    };

    Ok(Some(Edge::new(id, label, source, target, properties)))
}

/// Returns true if the slot has an overflowed label that needs resolution.
#[must_use]
pub const fn edge_slot_needs_label_resolve(slot: &[u8; EDGE_SLOT_SIZE]) -> bool {
    slot[OFF_LABEL_LEN] == LABEL_OVERFLOW_MARKER
}

/// Reads the inline label from a slot. Returns `Err(CorruptPage)` if the
/// bytes are not valid UTF-8. Only valid when `edge_slot_needs_label_resolve` is false.
pub fn edge_slot_inline_label(slot: &[u8; EDGE_SLOT_SIZE], page_id: u32) -> Result<String> {
    let len = slot[OFF_LABEL_LEN] as usize;
    if len > EDGE_LABEL_INLINE_MAX {
        return Err(crate::Error::CorruptPage {
            file: "edges.db",
            page_id,
            reason: "edge label length exceeds the inline capacity",
        });
    }
    std::str::from_utf8(&slot[OFF_LABEL_INLINE..OFF_LABEL_INLINE + len])
        .map(str::to_owned)
        .map_err(|_| crate::Error::CorruptPage {
            file: "edges.db",
            page_id,
            reason: "inline edge label is not valid UTF-8",
        })
}

/// Returns the label overflow string ref from the slot.
#[must_use]
pub fn edge_slot_label_overflow_ref(slot: &[u8; EDGE_SLOT_SIZE]) -> u32 {
    u32::from_le_bytes(
        slot[OFF_LABEL_OVERFLOW..OFF_LABEL_OVERFLOW + 4]
            .try_into()
            .unwrap(),
    )
}

/// Returns the property overflow page ID from the slot.
#[must_use]
pub fn edge_slot_prop_overflow_ref(slot: &[u8; EDGE_SLOT_SIZE]) -> u32 {
    u32::from_le_bytes(
        slot[OFF_PROP_OVERFLOW..OFF_PROP_OVERFLOW + 4]
            .try_into()
            .unwrap(),
    )
}

/// Returns true if the slot has overflowed properties that need resolution.
#[must_use]
pub fn edge_slot_needs_prop_resolve(slot: &[u8; EDGE_SLOT_SIZE]) -> bool {
    let prop_count = u16::from_le_bytes(
        slot[OFF_PROP_COUNT..OFF_PROP_COUNT + 2]
            .try_into()
            .unwrap(),
    );
    let inline_len = u16::from_le_bytes(
        slot[OFF_PROP_INLINE_LEN..OFF_PROP_INLINE_LEN + 2]
            .try_into()
            .unwrap(),
    );
    prop_count > 0 && inline_len == 0
}

/// Patches the `label_overflow` and `prop_overflow` offsets into an already-encoded slot.
pub fn patch_edge_overflow(
    slot: &mut [u8; EDGE_SLOT_SIZE],
    label_overflow: u32,
    prop_overflow: u32,
) {
    slot[OFF_LABEL_OVERFLOW..OFF_LABEL_OVERFLOW + 4]
        .copy_from_slice(&label_overflow.to_le_bytes());
    slot[OFF_PROP_OVERFLOW..OFF_PROP_OVERFLOW + 4]
        .copy_from_slice(&prop_overflow.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::property::{Properties, Property};

    fn make_edge(id: u64, label: &str, source: u64, target: u64, props: Properties) -> Edge {
        Edge::new(EdgeId(id), label, NodeId(source), NodeId(target), props)
    }

    fn no_resolve_label(_: u32) -> Result<String> {
        panic!("label resolver should not be called");
    }

    fn no_resolve_props(_: u32) -> Result<Vec<u8>> {
        panic!("props resolver should not be called");
    }

    #[test]
    fn edge_slot_round_trip_inline() {
        let mut props = Properties::new();
        props.insert("w".into(), Property::I64(10));
        let edge = make_edge(1, "KNOWS", 10, 20, props.clone());

        let (slot, overflow) = encode_edge_slot(&edge).unwrap();
        assert!(!overflow.label_overflowed);
        assert!(!overflow.props_overflowed);

        let decoded = decode_edge_slot(&slot, 0, no_resolve_label, no_resolve_props)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.id(), EdgeId(1));
        assert_eq!(decoded.label(), "KNOWS");
        assert_eq!(decoded.source(), NodeId(10));
        assert_eq!(decoded.target(), NodeId(20));
        assert_eq!(decoded.properties(), &props);
    }

    #[test]
    fn edge_slot_source_target_preserved() {
        let edge = make_edge(1, "R", 0x0A0B_0C0D_0E0F_1011, 0x1213_1415_1617_1819, Properties::new());
        let (slot, _) = encode_edge_slot(&edge).unwrap();

        // source at offset 9, little-endian
        assert_eq!(
            &slot[OFF_SOURCE..OFF_SOURCE + 8],
            &0x0A0B_0C0D_0E0F_1011_u64.to_le_bytes()
        );
        // target at offset 17, little-endian
        assert_eq!(
            &slot[OFF_TARGET..OFF_TARGET + 8],
            &0x1213_1415_1617_1819_u64.to_le_bytes()
        );

        let decoded = decode_edge_slot(&slot, 0, no_resolve_label, no_resolve_props)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.source(), NodeId(0x0A0B_0C0D_0E0F_1011));
        assert_eq!(decoded.target(), NodeId(0x1213_1415_1617_1819));
    }

    #[test]
    fn edge_slot_label_exactly_55_bytes() {
        let label = "e".repeat(55);
        let edge = make_edge(1, &label, 1, 2, Properties::new());

        let (slot, overflow) = encode_edge_slot(&edge).unwrap();
        assert!(!overflow.label_overflowed);

        let decoded = decode_edge_slot(&slot, 0, no_resolve_label, no_resolve_props)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.label(), label);
    }

    #[test]
    fn edge_slot_label_56_bytes_overflows() {
        let label = "e".repeat(56);
        let edge = make_edge(1, &label, 1, 2, Properties::new());

        let (slot, overflow) = encode_edge_slot(&edge).unwrap();
        assert!(overflow.label_overflowed);
        assert_eq!(slot[OFF_LABEL_LEN], LABEL_OVERFLOW_MARKER);
    }

    #[test]
    fn edge_slot_props_exactly_30_bytes() {
        // key_len(1) + key("x"=1) + tag(1) + len(4) + data = 7 + data_len
        // 30 - 7 = 23 bytes of data
        let mut props = Properties::new();
        props.insert("x".into(), Property::Bytes(vec![0xCD; 23]));

        let encoded_len = property_codec::encode_properties(&props).unwrap().len();
        assert_eq!(encoded_len, 30);

        let edge = make_edge(1, "R", 1, 2, props.clone());
        let (slot, overflow) = encode_edge_slot(&edge).unwrap();
        assert!(!overflow.props_overflowed);

        let decoded = decode_edge_slot(&slot, 0, no_resolve_label, no_resolve_props)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.properties(), &props);
    }

    #[test]
    fn edge_slot_props_overflow() {
        let mut props = Properties::new();
        props.insert("x".into(), Property::Bytes(vec![0xCD; 24]));

        let encoded_len = property_codec::encode_properties(&props).unwrap().len();
        assert!(encoded_len > EDGE_PROP_INLINE_MAX);

        let edge = make_edge(1, "R", 1, 2, props);
        let (_, overflow) = encode_edge_slot(&edge).unwrap();
        assert!(overflow.props_overflowed);
        assert!(overflow.props_bytes.is_some());
    }

    #[test]
    fn edge_slot_empty_is_none() {
        let slot = [0u8; EDGE_SLOT_SIZE];
        let result = decode_edge_slot(&slot, 0, no_resolve_label, no_resolve_props).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn edge_slot_tombstone_is_none() {
        let mut slot = [0u8; EDGE_SLOT_SIZE];
        slot[OFF_FLAGS] = SLOT_TOMBSTONE;
        let result = decode_edge_slot(&slot, 0, no_resolve_label, no_resolve_props).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn edge_slot_label_hash_stored() {
        let edge = make_edge(1, "HAS_SYSTEM", 1, 2, Properties::new());
        let (slot, _) = encode_edge_slot(&edge).unwrap();

        let stored = u32::from_le_bytes(
            slot[OFF_LABEL_HASH..OFF_LABEL_HASH + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(stored, label_hash("HAS_SYSTEM"));
    }

    #[test]
    fn edge_slot_no_props() {
        let edge = make_edge(1, "R", 1, 2, Properties::new());
        let (slot, overflow) = encode_edge_slot(&edge).unwrap();
        assert!(!overflow.props_overflowed);

        let prop_count = u16::from_le_bytes(
            slot[OFF_PROP_COUNT..OFF_PROP_COUNT + 2].try_into().unwrap(),
        );
        assert_eq!(prop_count, 0);

        let decoded = decode_edge_slot(&slot, 0, no_resolve_label, no_resolve_props)
            .unwrap()
            .unwrap();
        assert!(decoded.properties().is_empty());
    }

    #[test]
    fn decode_edge_slot_error_carries_page_id() {
        let mut slot = [0u8; EDGE_SLOT_SIZE];
        slot[OFF_FLAGS] = SLOT_LIVE;
        // Write valid id, source, target
        slot[OFF_ID..=8].copy_from_slice(&1u64.to_le_bytes());
        slot[OFF_SOURCE..OFF_SOURCE + 8].copy_from_slice(&10u64.to_le_bytes());
        slot[OFF_TARGET..OFF_TARGET + 8].copy_from_slice(&20u64.to_le_bytes());
        // Set label_len = 3 (inline, not overflow)
        slot[OFF_LABEL_LEN] = 3;
        // Write invalid UTF-8 at the label inline region
        slot[OFF_LABEL_INLINE] = 0xFF;
        slot[OFF_LABEL_INLINE + 1] = 0xFE;
        slot[OFF_LABEL_INLINE + 2] = 0xFD;

        let err = decode_edge_slot(&slot, 99, no_resolve_label, no_resolve_props)
            .unwrap_err();
        match err {
            crate::Error::CorruptPage { page_id, .. } => assert_eq!(page_id, 99),
            other => panic!("expected CorruptPage, got {other:?}"),
        }
    }

    #[test]
    fn decode_edge_with_label_resolver() {
        let label = "x".repeat(100);
        let edge = make_edge(1, &label, 1, 2, Properties::new());
        let (mut slot, overflow) = encode_edge_slot(&edge).unwrap();
        assert!(overflow.label_overflowed);

        patch_edge_overflow(&mut slot, 77, 0);

        let resolved = label.clone();
        let decoded = decode_edge_slot(
            &slot,
            0,
            move |offset| {
                assert_eq!(offset, 77);
                Ok(resolved)
            },
            no_resolve_props,
        )
        .unwrap()
        .unwrap();
        assert_eq!(decoded.label(), label);
    }

    #[test]
    fn decode_edge_with_props_resolver() {
        let mut props = Properties::new();
        props.insert("big".into(), Property::Bytes(vec![0xFF; 40]));

        let edge = make_edge(1, "R", 1, 2, props.clone());
        let (mut slot, overflow) = encode_edge_slot(&edge).unwrap();
        assert!(overflow.props_overflowed);

        let raw = overflow.props_bytes.unwrap();
        patch_edge_overflow(&mut slot, 0, 55);

        let decoded = decode_edge_slot(
            &slot,
            0,
            no_resolve_label,
            move |page_id| {
                assert_eq!(page_id, 55);
                Ok(raw)
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(decoded.properties(), &props);
    }

    #[test]
    fn patch_edge_overflow_writes_offsets() {
        let mut slot = [0u8; EDGE_SLOT_SIZE];
        patch_edge_overflow(&mut slot, 0xAABB_CCDD, 0x1122_3344);

        let label_ov = u32::from_le_bytes(
            slot[OFF_LABEL_OVERFLOW..OFF_LABEL_OVERFLOW + 4]
                .try_into()
                .unwrap(),
        );
        let prop_ov = u32::from_le_bytes(
            slot[OFF_PROP_OVERFLOW..OFF_PROP_OVERFLOW + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(label_ov, 0xAABB_CCDD);
        assert_eq!(prop_ov, 0x1122_3344);
    }

    #[test]
    fn edge_slot_inline_label_rejects_invalid_utf8() {
        let mut slot = [0u8; EDGE_SLOT_SIZE];
        slot[OFF_FLAGS] = SLOT_LIVE;
        slot[OFF_LABEL_LEN] = 3;
        slot[OFF_LABEL_INLINE] = 0xFF;
        slot[OFF_LABEL_INLINE + 1] = 0xFE;
        slot[OFF_LABEL_INLINE + 2] = 0xFD;
        let result = edge_slot_inline_label(&slot, 11);
        assert!(
            result.is_err(),
            "expected Err for invalid UTF-8, got Ok"
        );
        match result.unwrap_err() {
            crate::Error::CorruptPage { page_id, .. } => assert_eq!(page_id, 11),
            other => panic!("expected CorruptPage, got {other:?}"),
        }
    }
}

/// Property-based tests (issue #67).
///
/// Mirrors `node_codec`'s suite. Worth having separately rather than trusting
/// the symmetry: the two codecs have different inline budgets (55/30 bytes
/// here against 47/38 for nodes), and the panic guards this suite drove out
/// were needed independently in both.
#[cfg(test)]
mod proptests {
    use super::*;
    use crate::property::{Properties, Property};
    use proptest::prelude::*;

    fn size_around(limit: usize) -> impl Strategy<Value = usize> {
        prop_oneof![
            4 => 0..=limit,
            3 => limit.saturating_sub(5)..=limit + 5,
            3 => (limit + 1)..=(limit * 2).max(limit + 2),
        ]
    }

    fn label_strategy() -> impl Strategy<Value = String> {
        size_around(EDGE_LABEL_INLINE_MAX).prop_map(|n| "E".repeat(n.max(1)))
    }

    fn properties_strategy() -> impl Strategy<Value = Properties> {
        proptest::collection::hash_map(
            "[a-z]{1,10}",
            prop_oneof![
                size_around(EDGE_PROP_INLINE_MAX).prop_map(|n| Property::String("v".repeat(n))),
                any::<i64>().prop_map(Property::I64),
                any::<bool>().prop_map(Property::Bool),
            ],
            0..5,
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// Accept or reject, but never lie — across the slot and whatever
        /// spilled out of it.
        #[test]
        fn slot_roundtrip_matches_or_encode_rejects_but_never_corrupts(
            id in 1u64..=u64::MAX,
            source in 1u64..=u64::MAX,
            target in 1u64..=u64::MAX,
            label in label_strategy(),
            props in properties_strategy(),
        ) {
            let edge = Edge::new(
                EdgeId(id),
                &label,
                NodeId(source),
                NodeId(target),
                props.clone(),
            );

            let Ok((slot, overflow)) = encode_edge_slot(&edge) else {
                return Ok(());
            };

            let spilled_label = overflow.label_bytes;
            let spilled_props = overflow.props_bytes;

            let decoded = decode_edge_slot(
                &slot,
                0,
                |_| {
                    spilled_label
                        .as_ref()
                        .map(|b| String::from_utf8(b.clone()).expect("label was valid UTF-8"))
                        .ok_or(crate::Error::CorruptPage {
                            file: "edges.db",
                            page_id: 0,
                            reason: "label resolver called but nothing overflowed",
                        })
                },
                |_| {
                    spilled_props.clone().ok_or(crate::Error::CorruptPage {
                        file: "edges.db",
                        page_id: 0,
                        reason: "props resolver called but nothing overflowed",
                    })
                },
            )
            .expect("what encode accepted, decode must read")
            .expect("a live slot must decode to an edge");

            prop_assert_eq!(decoded.id, edge.id, "id changed");
            prop_assert_eq!(decoded.source, edge.source, "source changed");
            prop_assert_eq!(decoded.target, edge.target, "target changed");
            prop_assert_eq!(decoded.label(), edge.label(), "label changed");
            prop_assert_eq!(decoded.properties(), &props, "properties changed");
        }

        /// The slot's own view of what spilled must match what the encoder
        /// reported. If those drift apart, the decoder reads inline bytes for
        /// a value that lives in overflow, or the reverse.
        #[test]
        fn resolvers_are_called_exactly_when_something_overflowed(
            id in 1u64..=u64::MAX,
            label in label_strategy(),
            props in properties_strategy(),
        ) {
            let edge = Edge::new(EdgeId(id), &label, NodeId(1), NodeId(2), props);
            let Ok((slot, overflow)) = encode_edge_slot(&edge) else {
                return Ok(());
            };

            prop_assert_eq!(
                edge_slot_needs_label_resolve(&slot),
                overflow.label_overflowed,
                "the slot and the encoder disagree about whether the label spilled"
            );
            prop_assert_eq!(
                edge_slot_needs_prop_resolve(&slot),
                overflow.props_overflowed,
                "the slot and the encoder disagree about whether the properties spilled"
            );
        }

        /// Arbitrary slot bytes must produce an error, never a panic.
        ///
        /// This is the test that found the real bugs: both codecs sliced the
        /// slot on a length byte read straight from the page, which a `u8` or
        /// `u16` can set far past the inline capacity. Twenty-odd example
        /// tests per codec never tripped it, because nobody writes an example
        /// with a label length of 200.
        #[test]
        fn decode_never_panics_on_arbitrary_slot_bytes(
            bytes in proptest::collection::vec(any::<u8>(), EDGE_SLOT_SIZE)
        ) {
            let mut slot = [0u8; EDGE_SLOT_SIZE];
            slot.copy_from_slice(&bytes);

            let outcome = decode_edge_slot(
                &slot,
                0,
                |_| Ok("recovered".to_owned()),
                |_| Ok(Vec::new()),
            );
            prop_assert!(outcome.is_ok() || outcome.is_err());
        }

        /// The public inline-label helper is a separate entry point with the
        /// same hazard, so it needs its own guarantee.
        #[test]
        fn inline_label_helper_never_panics_on_arbitrary_slot_bytes(
            bytes in proptest::collection::vec(any::<u8>(), EDGE_SLOT_SIZE)
        ) {
            let mut slot = [0u8; EDGE_SLOT_SIZE];
            slot.copy_from_slice(&bytes);
            let outcome = edge_slot_inline_label(&slot, 0);
            prop_assert!(outcome.is_ok() || outcome.is_err());
        }
    }
}
