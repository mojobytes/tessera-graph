// SPDX-License-Identifier: MIT

use crate::error::{NodeId, Result};
use crate::node::Node;
use crate::property::Properties;
use crate::storage::codec::property_codec;

pub const NODE_SLOT_SIZE: usize = 128;
pub const NODE_LABEL_INLINE_MAX: usize = 47;
pub const NODE_PROP_INLINE_MAX: usize = 38;
pub const SLOTS_PER_PAGE: usize = 31;

pub const SLOT_EMPTY: u8 = 0x00;
pub const SLOT_LIVE: u8 = 0x01;
pub const SLOT_TOMBSTONE: u8 = 0x02;

/// Sentinel value for `adj_page_id` meaning "this node has no adjacency head yet".
pub const ADJ_PAGE_ID_SENTINEL: u32 = u32::MAX;

/// `adj_flags` bit meaning the node has at least one outgoing edge.
pub const ADJ_FLAG_OUTGOING: u8 = 0b0000_0001;
/// `adj_flags` bit meaning the node has at least one incoming edge.
pub const ADJ_FLAG_INCOMING: u8 = 0b0000_0010;

// Slot layout offsets
const OFF_FLAGS: usize = 0;
const OFF_ID: usize = 1;
const OFF_LABEL_HASH: usize = 9;
const OFF_LABEL_LEN: usize = 13;
const OFF_LABEL_INLINE: usize = 14;
const OFF_LABEL_OVERFLOW: usize = 61;
const OFF_PROP_COUNT: usize = 65;
const OFF_PROP_INLINE_LEN: usize = 67;
const OFF_PROP_INLINE: usize = 69;
const OFF_PROP_OVERFLOW: usize = 107;
const OFF_ADJ_PAGE_ID: usize = 111; // outgoing chain head (u32 LE)
const OFF_ADJ_FLAGS: usize = 115;
const OFF_ADJ_INCOMING_PAGE_ID: usize = 116; // incoming chain head (u32 LE)
const OFF_RESERVED: usize = 120;

const LABEL_OVERFLOW_MARKER: u8 = 0xFF;

/// Metadata about what overflowed during encoding.
pub struct NodeSlotOverflow {
    pub label_overflowed: bool,
    pub label_bytes: Option<Vec<u8>>,
    pub props_overflowed: bool,
    pub props_bytes: Option<Vec<u8>>,
}

/// Encodes a `Node` into a 128-byte slot buffer.
///
/// If the label exceeds `NODE_LABEL_INLINE_MAX` (47) bytes, `label_len` is set
/// to `0xFF` and the label is not stored inline — the caller must write it to
/// the string heap and patch the overflow offset via [`patch_overflow`].
///
/// If serialized properties exceed 38 bytes, `prop_overflow` is flagged and
/// the caller must write them to overflow pages.
///
/// The adjacency pointer (`adj_page_id`/`adj_flags`) is initialized to "no
/// adjacency yet" ([`ADJ_PAGE_ID_SENTINEL`], flags `0`); callers that need to
/// wire a node into the adjacency structure must patch it afterwards via
/// [`patch_adj_pointer`].
pub fn encode_node_slot(node: &Node) -> Result<([u8; NODE_SLOT_SIZE], NodeSlotOverflow)> {
    let mut slot = [0u8; NODE_SLOT_SIZE];
    let mut overflow = NodeSlotOverflow {
        label_overflowed: false,
        label_bytes: None,
        props_overflowed: false,
        props_bytes: None,
    };

    // flags
    slot[OFF_FLAGS] = SLOT_LIVE;

    // id (u64 LE)
    slot[OFF_ID..=OFF_ID + 7].copy_from_slice(&node.id.0.to_le_bytes());

    // label_hash (CRC32 of label bytes)
    let lh = label_hash(node.label());
    slot[OFF_LABEL_HASH..OFF_LABEL_HASH + 4].copy_from_slice(&lh.to_le_bytes());

    // label
    let label_bytes = node.label().as_bytes();
    if label_bytes.len() <= NODE_LABEL_INLINE_MAX {
        // Inside the branch that checked `len <= NODE_LABEL_INLINE_MAX` (< 255);
        // the else-branch takes the overflow path instead.
        #[allow(clippy::cast_possible_truncation)]
        let label_len = label_bytes.len() as u8;
        slot[OFF_LABEL_LEN] = label_len;
        slot[OFF_LABEL_INLINE..OFF_LABEL_INLINE + label_bytes.len()].copy_from_slice(label_bytes);
    } else {
        slot[OFF_LABEL_LEN] = LABEL_OVERFLOW_MARKER;
        overflow.label_overflowed = true;
        overflow.label_bytes = Some(label_bytes.to_vec());
    }

    // properties
    let props_encoded = property_codec::encode_properties(node.properties())?;
    // A wrapped count is worse than a rejected write: 65,536 properties would
    // record as 0, so the whole set reads back as absent while its bytes sit
    // on disk (issue #62).
    let prop_count =
        u16::try_from(node.properties().len()).map_err(|_| crate::Error::RecordTooLarge {
            size: node.properties().len(),
        })?;
    slot[OFF_PROP_COUNT..OFF_PROP_COUNT + 2].copy_from_slice(&prop_count.to_le_bytes());

    if props_encoded.len() <= NODE_PROP_INLINE_MAX {
        // Inside the branch that checked `len <= NODE_PROP_INLINE_MAX` (38).
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

    // adjacency pointer: carried on the Node so re-serialization (vacuum, WAL
    // redo) preserves it. A node with no edges yet holds the sentinel/0. The
    // slot stores two heads — outgoing and incoming chain — so a bidirectional
    // node keeps both without a page scan.
    slot[OFF_ADJ_PAGE_ID..OFF_ADJ_PAGE_ID + 4].copy_from_slice(&node.adj_page_id.to_le_bytes());
    slot[OFF_ADJ_FLAGS] = node.adj_flags;
    slot[OFF_ADJ_INCOMING_PAGE_ID..OFF_ADJ_INCOMING_PAGE_ID + 4]
        .copy_from_slice(&node.adj_incoming_page_id.to_le_bytes());

    // reserved bytes
    slot[OFF_RESERVED..NODE_SLOT_SIZE].fill(0);

    Ok((slot, overflow))
}

/// Decodes a 128-byte slot into a `Node`.
///
/// Returns `Ok(None)` for empty or tombstoned slots.
///
/// `page_id` is used for error context when decoding fails.
/// `resolve_label` is called when the label overflowed to the string heap.
/// `resolve_props` is called when properties overflowed to overflow pages;
/// it must return the raw serialized property bytes.
pub fn decode_node_slot(
    slot: &[u8; NODE_SLOT_SIZE],
    page_id: u32,
    resolve_label: impl FnOnce(u32) -> Result<String>,
    resolve_props: impl FnOnce(u32) -> Result<Vec<u8>>,
) -> Result<Option<Node>> {
    decode_node_slot_inner(slot, page_id, resolve_label, resolve_props, None)
}

/// Decodes a 128-byte slot into a `Node` with only the projected properties.
///
/// Only properties whose keys appear in `projected_keys` are decoded;
/// all others are skipped without allocating. If `projected_keys` is empty,
/// no properties are decoded (but the label is always resolved).
///
/// When overflow properties are not needed (empty `projected_keys` or all
/// requested keys are inline), `resolve_props` is never called — avoiding
/// the overflow page I/O entirely.
pub fn decode_node_slot_projected(
    slot: &[u8; NODE_SLOT_SIZE],
    page_id: u32,
    resolve_label: impl FnOnce(u32) -> Result<String>,
    resolve_props: impl FnOnce(u32) -> Result<Vec<u8>>,
    projected_keys: &[&str],
) -> Result<Option<Node>> {
    decode_node_slot_inner(
        slot,
        page_id,
        resolve_label,
        resolve_props,
        Some(projected_keys),
    )
}

fn decode_node_slot_inner(
    slot: &[u8; NODE_SLOT_SIZE],
    page_id: u32,
    resolve_label: impl FnOnce(u32) -> Result<String>,
    resolve_props: impl FnOnce(u32) -> Result<Vec<u8>>,
    projected_keys: Option<&[&str]>,
) -> Result<Option<Node>> {
    let flags = slot[OFF_FLAGS];
    if flags == SLOT_EMPTY || flags == SLOT_TOMBSTONE {
        return Ok(None);
    }

    // id
    let id_bytes: [u8; 8] = slot[OFF_ID..=OFF_ID + 7].try_into().unwrap();
    let id = NodeId(u64::from_le_bytes(id_bytes));

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
        // The length byte comes from the page, so it is not trusted: a `u8`
        // holds up to 254 here (0xFF means "overflowed"), while only
        // NODE_LABEL_INLINE_MAX bytes of room exist. Slicing on the raw value
        // would run past the end of the slot and panic.
        //
        // A corrupt page normally fails its CRC before reaching this point, so
        // this guard is not what stands between a bad disk and a crash. It
        // covers the case CRC cannot: a length that is wrong but *consistent*
        // — written by a logic error, checksummed as valid, and read back as a
        // panic instead of an error. That is the shape of #62.
        let len = label_len as usize;
        if len > NODE_LABEL_INLINE_MAX {
            return Err(crate::Error::CorruptPage {
                file: "nodes.db",
                page_id,
                reason: "node label length exceeds the inline capacity",
            });
        }
        std::str::from_utf8(&slot[OFF_LABEL_INLINE..OFF_LABEL_INLINE + len])
            .map_err(|_| crate::Error::CorruptPage {
                file: "nodes.db",
                page_id,
                reason: "node label is not valid UTF-8",
            })?
            .to_owned()
    };

    // properties
    let prop_count =
        u16::from_le_bytes(slot[OFF_PROP_COUNT..OFF_PROP_COUNT + 2].try_into().unwrap());

    let inline_len = u16::from_le_bytes(
        slot[OFF_PROP_INLINE_LEN..OFF_PROP_INLINE_LEN + 2]
            .try_into()
            .unwrap(),
    ) as usize;

    // Like the label length above, this comes off the page and is used to
    // slice the slot in three places below. A `u16` can name 65,535 bytes
    // where only NODE_PROP_INLINE_MAX exist, so slicing on the raw value runs
    // past the end and panics. Validated once here rather than at each of the
    // three slice sites.
    if inline_len > NODE_PROP_INLINE_MAX {
        return Err(crate::Error::CorruptPage {
            file: "nodes.db",
            page_id,
            reason: "inline property length exceeds the inline capacity",
        });
    }

    let overflowed = prop_count > 0 && inline_len == 0;

    let properties = match projected_keys {
        // Empty projection: skip ALL property I/O, including overflow page reads.
        // The caller requested zero properties, so there is nothing to decode.
        // `resolve_props` is guaranteed not to be called here.
        Some([]) => Properties::new(),
        // Projected: skip overflow I/O if props didn't overflow
        Some(keys) if !overflowed => {
            let (props, _) = property_codec::decode_properties_projected(
                &slot[OFF_PROP_INLINE..OFF_PROP_INLINE + inline_len],
                prop_count,
                keys,
                page_id,
            )?;
            props
        }
        // Projected: must read overflow, then project
        Some(keys) => {
            let prop_overflow_offset = u32::from_le_bytes(
                slot[OFF_PROP_OVERFLOW..OFF_PROP_OVERFLOW + 4]
                    .try_into()
                    .unwrap(),
            );
            let raw = resolve_props(prop_overflow_offset)?;
            let (props, _) =
                property_codec::decode_properties_projected(&raw, prop_count, keys, page_id)?;
            props
        }
        // Full decode: overflow path
        None if overflowed => {
            let prop_overflow_offset = u32::from_le_bytes(
                slot[OFF_PROP_OVERFLOW..OFF_PROP_OVERFLOW + 4]
                    .try_into()
                    .unwrap(),
            );
            let raw = resolve_props(prop_overflow_offset)?;
            let (props, _) = property_codec::decode_properties(&raw, prop_count, page_id)?;
            props
        }
        // Full decode: inline path
        None => {
            let (props, _) = property_codec::decode_properties(
                &slot[OFF_PROP_INLINE..OFF_PROP_INLINE + inline_len],
                prop_count,
                page_id,
            )?;
            props
        }
    };

    // Carry the on-disk adjacency pointer onto the Node so any later
    // re-serialization (MVCC vacuum, WAL redo) preserves it instead of
    // resetting it to the sentinel.
    let mut node = Node::new(id, label, properties);
    node.adj_page_id = slot_adj_page_id(slot);
    node.adj_incoming_page_id = slot_adj_incoming_page_id(slot);
    node.adj_flags = slot_adj_flags(slot);
    Ok(Some(node))
}

/// Computes the label hash (CRC32 of label bytes) used for future indexing.
#[must_use]
pub fn label_hash(label: &str) -> u32 {
    crc32fast::hash(label.as_bytes())
}

/// Returns true if the slot has an overflowed label that needs resolution.
#[must_use]
pub const fn slot_needs_label_resolve(slot: &[u8; NODE_SLOT_SIZE]) -> bool {
    slot[OFF_LABEL_LEN] == LABEL_OVERFLOW_MARKER
}

/// Reads the inline label from a slot. Returns `Err(CorruptPage)` if the
/// bytes are not valid UTF-8. Only valid when `slot_needs_label_resolve` is false.
pub fn slot_inline_label(slot: &[u8; NODE_SLOT_SIZE], page_id: u32) -> Result<String> {
    let len = slot[OFF_LABEL_LEN] as usize;
    // Same guard as in `decode_node_slot_inner`: this is a public entry point
    // that slices on a length byte taken straight off the page (issue #67).
    if len > NODE_LABEL_INLINE_MAX {
        return Err(crate::Error::CorruptPage {
            file: "nodes.db",
            page_id,
            reason: "node label length exceeds the inline capacity",
        });
    }
    std::str::from_utf8(&slot[OFF_LABEL_INLINE..OFF_LABEL_INLINE + len])
        .map(str::to_owned)
        .map_err(|_| crate::Error::CorruptPage {
            file: "nodes.db",
            page_id,
            reason: "inline node label is not valid UTF-8",
        })
}

/// Returns the label overflow string ref from the slot.
#[must_use]
pub fn slot_label_overflow_ref(slot: &[u8; NODE_SLOT_SIZE]) -> u32 {
    u32::from_le_bytes(
        slot[OFF_LABEL_OVERFLOW..OFF_LABEL_OVERFLOW + 4]
            .try_into()
            .unwrap(),
    )
}

/// Returns the property overflow page ID from the slot.
#[must_use]
pub fn slot_prop_overflow_ref(slot: &[u8; NODE_SLOT_SIZE]) -> u32 {
    u32::from_le_bytes(
        slot[OFF_PROP_OVERFLOW..OFF_PROP_OVERFLOW + 4]
            .try_into()
            .unwrap(),
    )
}

/// Returns true if the slot has overflowed properties that need resolution.
#[must_use]
pub fn slot_needs_prop_resolve(slot: &[u8; NODE_SLOT_SIZE]) -> bool {
    let prop_count =
        u16::from_le_bytes(slot[OFF_PROP_COUNT..OFF_PROP_COUNT + 2].try_into().unwrap());
    let inline_len = u16::from_le_bytes(
        slot[OFF_PROP_INLINE_LEN..OFF_PROP_INLINE_LEN + 2]
            .try_into()
            .unwrap(),
    );
    prop_count > 0 && inline_len == 0
}

/// Patches the `label_overflow` and `prop_overflow` offsets into an already-encoded slot.
pub fn patch_overflow(slot: &mut [u8; NODE_SLOT_SIZE], label_overflow: u32, prop_overflow: u32) {
    slot[OFF_LABEL_OVERFLOW..OFF_LABEL_OVERFLOW + 4].copy_from_slice(&label_overflow.to_le_bytes());
    slot[OFF_PROP_OVERFLOW..OFF_PROP_OVERFLOW + 4].copy_from_slice(&prop_overflow.to_le_bytes());
}

/// Returns the node's adjacency head page ID from the slot.
///
/// Returns [`ADJ_PAGE_ID_SENTINEL`] if the node has no adjacency head yet.
#[must_use]
pub fn slot_adj_page_id(slot: &[u8; NODE_SLOT_SIZE]) -> u32 {
    u32::from_le_bytes(
        slot[OFF_ADJ_PAGE_ID..OFF_ADJ_PAGE_ID + 4]
            .try_into()
            .unwrap(),
    )
}

/// Returns the node's incoming-chain head page ID from the slot.
///
/// Returns [`ADJ_PAGE_ID_SENTINEL`] if the node has no incoming chain yet.
#[must_use]
pub fn slot_adj_incoming_page_id(slot: &[u8; NODE_SLOT_SIZE]) -> u32 {
    u32::from_le_bytes(
        slot[OFF_ADJ_INCOMING_PAGE_ID..OFF_ADJ_INCOMING_PAGE_ID + 4]
            .try_into()
            .unwrap(),
    )
}

/// Returns the node's adjacency flags from the slot (see [`ADJ_FLAG_OUTGOING`],
/// [`ADJ_FLAG_INCOMING`]).
#[must_use]
pub const fn slot_adj_flags(slot: &[u8; NODE_SLOT_SIZE]) -> u8 {
    slot[OFF_ADJ_FLAGS]
}

/// Patches both adjacency chain heads (outgoing and incoming) and their
/// direction flags into an already-encoded slot, without touching label or
/// property bytes.
///
/// A `None` head is written as [`ADJ_PAGE_ID_SENTINEL`] and clears its flag; a
/// `Some(page)` head is written as-is and sets its flag. The two directions are
/// independent, so a bidirectional node keeps both heads.
pub fn patch_adj_pointer(
    slot: &mut [u8; NODE_SLOT_SIZE],
    outgoing_page: Option<u32>,
    incoming_page: Option<u32>,
) {
    let mut flags = 0u8;
    slot[OFF_ADJ_PAGE_ID..OFF_ADJ_PAGE_ID + 4]
        .copy_from_slice(&outgoing_page.unwrap_or(ADJ_PAGE_ID_SENTINEL).to_le_bytes());
    if outgoing_page.is_some() {
        flags |= ADJ_FLAG_OUTGOING;
    }
    slot[OFF_ADJ_INCOMING_PAGE_ID..OFF_ADJ_INCOMING_PAGE_ID + 4]
        .copy_from_slice(&incoming_page.unwrap_or(ADJ_PAGE_ID_SENTINEL).to_le_bytes());
    if incoming_page.is_some() {
        flags |= ADJ_FLAG_INCOMING;
    }
    slot[OFF_ADJ_FLAGS] = flags;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::property::{Properties, Property};

    // --- Property count limit (issue #62) ---
    //
    // The slot records how many properties it holds in a u16. Past 65,535 the
    // cast wrapped, so a node with 65,536 properties recorded a count of 0 and
    // read back as having none — every value silently gone, with the bytes
    // still on disk. Far less reachable than the string-length cases (it needs
    // an entity with that many distinct keys) but the same defect.
    #[test]
    fn encode_rejects_more_properties_than_u16_can_count() {
        let mut props = Properties::new();
        for i in 0..65_536u32 {
            props.insert(format!("k{i}"), Property::I64(i64::from(i)));
        }
        let node = make_node(1, "N", props);

        match encode_node_slot(&node) {
            Err(crate::Error::RecordTooLarge { size }) => assert_eq!(size, 65_536),
            Err(other) => panic!("expected RecordTooLarge, got {other:?}"),
            Ok(_) => {
                panic!("65,536 properties wrap the count to 0 — all of them read back as none")
            }
        }
    }

    fn make_node(id: u64, label: &str, props: Properties) -> Node {
        Node::new(NodeId(id), label, props)
    }

    fn no_resolve_label(_: u32) -> Result<String> {
        panic!("label resolver should not be called");
    }

    fn no_resolve_props(_: u32) -> Result<Vec<u8>> {
        panic!("props resolver should not be called");
    }

    #[test]
    fn node_slot_round_trip_inline() {
        let mut props = Properties::new();
        props.insert("name".into(), Property::String("test".into()));
        let node = make_node(42, "Person", props.clone());

        let (slot, overflow) = encode_node_slot(&node).unwrap();
        assert!(!overflow.label_overflowed);
        assert!(!overflow.props_overflowed);

        let decoded = decode_node_slot(&slot, 0, no_resolve_label, no_resolve_props)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.id(), NodeId(42));
        assert_eq!(decoded.label(), "Person");
        assert_eq!(decoded.properties(), &props);
    }

    #[test]
    fn node_slot_label_inline_max_is_47() {
        // Label of exactly NODE_LABEL_INLINE_MAX (47) bytes fits inline.
        let label = "a".repeat(NODE_LABEL_INLINE_MAX);
        let node = make_node(1, &label, Properties::new());

        let (slot, overflow) = encode_node_slot(&node).expect("encode must succeed");
        assert!(
            !overflow.label_overflowed,
            "a {NODE_LABEL_INLINE_MAX}-byte label must fit inline"
        );
        assert_eq!(
            slot[OFF_LABEL_LEN] as usize, NODE_LABEL_INLINE_MAX,
            "label_len must record the exact inline length"
        );

        let decoded = decode_node_slot(&slot, 0, no_resolve_label, no_resolve_props)
            .expect("decode must succeed")
            .expect("slot must be live");
        assert_eq!(decoded.label(), label);

        // Label of NODE_LABEL_INLINE_MAX + 1 (48) bytes overflows.
        let overflow_label = "a".repeat(NODE_LABEL_INLINE_MAX + 1);
        let node = make_node(1, &overflow_label, Properties::new());

        let (slot, overflow) = encode_node_slot(&node).expect("encode must succeed");
        assert!(
            overflow.label_overflowed,
            "a {}-byte label must overflow the {NODE_LABEL_INLINE_MAX}-byte inline region",
            NODE_LABEL_INLINE_MAX + 1
        );
        assert_eq!(
            overflow
                .label_bytes
                .as_ref()
                .expect("overflow bytes must be set")
                .len(),
            NODE_LABEL_INLINE_MAX + 1
        );
        assert_eq!(slot[OFF_LABEL_LEN], LABEL_OVERFLOW_MARKER);
    }

    #[test]
    fn node_slot_empty_label() {
        let node = make_node(1, "", Properties::new());

        let (slot, overflow) = encode_node_slot(&node).unwrap();
        assert!(!overflow.label_overflowed);
        assert_eq!(slot[OFF_LABEL_LEN], 0);

        let decoded = decode_node_slot(&slot, 0, no_resolve_label, no_resolve_props)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.label(), "");
    }

    #[test]
    fn node_slot_props_exactly_38_bytes() {
        // Build props that serialize to exactly 38 bytes.
        // key_len(1) + key + tag(1) + value
        // We need total = 38. Use a Bytes property:
        // key_len(1) + key("x"=1) + tag(1) + len(4) + data = 7 + data_len
        // 38 - 7 = 31 bytes of data
        let mut props = Properties::new();
        props.insert("x".into(), Property::Bytes(vec![0xAB; 31]));

        let encoded_len = property_codec::encode_properties(&props).unwrap().len();
        assert_eq!(encoded_len, 38, "props must be exactly 38 bytes");

        let node = make_node(1, "N", props.clone());
        let (slot, overflow) = encode_node_slot(&node).unwrap();
        assert!(!overflow.props_overflowed);

        let decoded = decode_node_slot(&slot, 0, no_resolve_label, no_resolve_props)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.properties(), &props);
    }

    #[test]
    fn node_slot_props_overflow() {
        // 39 bytes => overflows the 38-byte inline region
        let mut props = Properties::new();
        props.insert("x".into(), Property::Bytes(vec![0xAB; 32]));

        let encoded_len = property_codec::encode_properties(&props).unwrap().len();
        assert!(encoded_len > NODE_PROP_INLINE_MAX);

        let node = make_node(1, "N", props);
        let (_, overflow) = encode_node_slot(&node).unwrap();
        assert!(overflow.props_overflowed);
        assert!(overflow.props_bytes.is_some());
    }

    #[test]
    fn node_slot_no_props() {
        let node = make_node(1, "N", Properties::new());
        let (slot, overflow) = encode_node_slot(&node).unwrap();
        assert!(!overflow.props_overflowed);

        let prop_count =
            u16::from_le_bytes(slot[OFF_PROP_COUNT..OFF_PROP_COUNT + 2].try_into().unwrap());
        let prop_inline_len = u16::from_le_bytes(
            slot[OFF_PROP_INLINE_LEN..OFF_PROP_INLINE_LEN + 2]
                .try_into()
                .unwrap(),
        );
        assert_eq!(prop_count, 0);
        assert_eq!(prop_inline_len, 0);

        let decoded = decode_node_slot(&slot, 0, no_resolve_label, no_resolve_props)
            .unwrap()
            .unwrap();
        assert!(decoded.properties().is_empty());
    }

    #[test]
    fn node_slot_empty_is_none() {
        let slot = [0u8; NODE_SLOT_SIZE];
        let result = decode_node_slot(&slot, 0, no_resolve_label, no_resolve_props).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn node_slot_tombstone_is_none() {
        let mut slot = [0u8; NODE_SLOT_SIZE];
        slot[OFF_FLAGS] = SLOT_TOMBSTONE;
        let result = decode_node_slot(&slot, 0, no_resolve_label, no_resolve_props).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn node_slot_id_preserved() {
        let node = make_node(0x0102_0304_0506_0708, "N", Properties::new());
        let (slot, _) = encode_node_slot(&node).unwrap();

        // Little-endian at offset 1
        assert_eq!(
            &slot[OFF_ID..=OFF_ID + 7],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );

        let decoded = decode_node_slot(&slot, 0, no_resolve_label, no_resolve_props)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.id(), NodeId(0x0102_0304_0506_0708));
    }

    #[test]
    fn node_slot_label_hash_stored() {
        let node = make_node(1, "Person", Properties::new());
        let (slot, _) = encode_node_slot(&node).unwrap();

        let stored_hash =
            u32::from_le_bytes(slot[OFF_LABEL_HASH..OFF_LABEL_HASH + 4].try_into().unwrap());
        assert_eq!(stored_hash, label_hash("Person"));
    }

    #[test]
    fn label_hash_deterministic() {
        let h1 = label_hash("SomeLabel");
        let h2 = label_hash("SomeLabel");
        assert_eq!(h1, h2);
    }

    #[test]
    fn patch_overflow_writes_offsets() {
        let mut slot = [0u8; NODE_SLOT_SIZE];
        patch_overflow(&mut slot, 0xAABB_CCDD, 0x1122_3344);

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
    fn node_slot_utf8_label() {
        // "café" is 5 bytes in UTF-8 (c, a, f, 0xC3, 0xA9)
        let node = make_node(1, "café", Properties::new());
        let (slot, overflow) = encode_node_slot(&node).unwrap();
        assert!(!overflow.label_overflowed);

        let decoded = decode_node_slot(&slot, 0, no_resolve_label, no_resolve_props)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.label(), "café");
    }

    #[test]
    fn decode_node_slot_error_carries_page_id() {
        let mut slot = [0u8; NODE_SLOT_SIZE];
        slot[OFF_FLAGS] = SLOT_LIVE;
        // Write a valid id so we get past id parsing
        slot[OFF_ID..=OFF_ID + 7].copy_from_slice(&1u64.to_le_bytes());
        // Set label_len = 3 (inline, not overflow)
        slot[OFF_LABEL_LEN] = 3;
        // Write invalid UTF-8 at the label inline region
        slot[OFF_LABEL_INLINE] = 0xFF;
        slot[OFF_LABEL_INLINE + 1] = 0xFE;
        slot[OFF_LABEL_INLINE + 2] = 0xFD;

        let err = decode_node_slot(&slot, 42, no_resolve_label, no_resolve_props).unwrap_err();
        match err {
            crate::Error::CorruptPage { page_id, .. } => assert_eq!(page_id, 42),
            other => panic!("expected CorruptPage, got {other:?}"),
        }
    }

    #[test]
    fn decode_with_label_resolver() {
        let label = "a".repeat(100);
        let node = make_node(1, &label, Properties::new());
        let (mut slot, overflow) = encode_node_slot(&node).unwrap();
        assert!(overflow.label_overflowed);

        // Simulate: caller wrote label to heap at offset 42
        patch_overflow(&mut slot, 42, 0);

        let resolved_label = label.clone();
        let decoded = decode_node_slot(
            &slot,
            0,
            move |offset| {
                assert_eq!(offset, 42);
                Ok(resolved_label)
            },
            no_resolve_props,
        )
        .unwrap()
        .unwrap();
        assert_eq!(decoded.label(), label);
    }

    #[test]
    fn decode_with_props_resolver() {
        // Create props that overflow
        let mut props = Properties::new();
        props.insert("big".into(), Property::Bytes(vec![0xFF; 50]));

        let node = make_node(1, "N", props.clone());
        let (mut slot, overflow) = encode_node_slot(&node).unwrap();
        assert!(overflow.props_overflowed);

        let raw_props = overflow.props_bytes.unwrap();

        // Simulate: caller wrote props to overflow page 99
        patch_overflow(&mut slot, 0, 99);

        let decoded = decode_node_slot(&slot, 0, no_resolve_label, move |page_id| {
            assert_eq!(page_id, 99);
            Ok(raw_props)
        })
        .unwrap()
        .unwrap();
        assert_eq!(decoded.properties(), &props);
    }

    // --- Projected decode tests ---

    #[test]
    fn decode_projected_returns_only_requested_props() {
        let mut props = Properties::new();
        props.insert("a".into(), Property::I64(1));
        props.insert("b".into(), Property::I64(2));
        props.insert("c".into(), Property::I64(3));
        let node = make_node(1, "Person", props);

        let (slot, overflow) = encode_node_slot(&node).unwrap();
        assert!(!overflow.props_overflowed);

        let decoded =
            decode_node_slot_projected(&slot, 0, no_resolve_label, no_resolve_props, &["a"])
                .unwrap()
                .unwrap();

        assert_eq!(decoded.id(), NodeId(1));
        assert_eq!(decoded.label(), "Person");
        assert_eq!(decoded.properties().len(), 1);
        assert_eq!(decoded.properties().get("a").unwrap(), &Property::I64(1));
    }

    #[test]
    fn decode_projected_empty_keys_skips_all_props() {
        let mut props = Properties::new();
        props.insert("name".into(), Property::String("Alice".into()));
        props.insert("age".into(), Property::I64(30));
        let node = make_node(1, "Person", props);

        let (slot, _) = encode_node_slot(&node).unwrap();

        let decoded = decode_node_slot_projected(&slot, 0, no_resolve_label, no_resolve_props, &[])
            .unwrap()
            .unwrap();

        assert!(decoded.properties().is_empty());
        assert_eq!(decoded.label(), "Person");
    }

    #[test]
    fn decode_projected_overflow_skips_resolve_when_empty_keys() {
        let mut props = Properties::new();
        props.insert("big".into(), Property::Bytes(vec![0xFF; 50]));
        let node = make_node(1, "N", props);

        let (slot, overflow) = encode_node_slot(&node).unwrap();
        assert!(overflow.props_overflowed);

        // no_resolve_props panics if called — this proves it's never invoked
        let decoded = decode_node_slot_projected(&slot, 0, no_resolve_label, no_resolve_props, &[])
            .unwrap()
            .unwrap();

        assert!(decoded.properties().is_empty());
        assert_eq!(decoded.label(), "N");
    }

    #[test]
    fn decode_projected_overflow_resolves_only_requested() {
        let mut props = Properties::new();
        props.insert("big".into(), Property::Bytes(vec![0xFF; 50]));
        props.insert("name".into(), Property::String("Bob".into()));
        let node = make_node(1, "N", props);

        let (mut slot, overflow) = encode_node_slot(&node).unwrap();
        assert!(overflow.props_overflowed);
        let raw_props = overflow.props_bytes.unwrap();

        patch_overflow(&mut slot, 0, 99);

        let decoded = decode_node_slot_projected(
            &slot,
            0,
            no_resolve_label,
            move |page_id| {
                assert_eq!(page_id, 99);
                Ok(raw_props)
            },
            &["name"],
        )
        .unwrap()
        .unwrap();

        assert_eq!(decoded.properties().len(), 1);
        assert_eq!(
            decoded.properties().get("name").unwrap(),
            &Property::String("Bob".into())
        );
    }

    #[test]
    fn node_slot_adj_pointer_round_trip() {
        let node = make_node(1, "N", Properties::new());
        let (mut slot, _) = encode_node_slot(&node).expect("encode must succeed");

        // A freshly encoded node has no adjacency yet: both heads at sentinel.
        assert_eq!(
            slot_adj_page_id(&slot),
            ADJ_PAGE_ID_SENTINEL,
            "a node with no adjacency patched must report the sentinel outgoing head"
        );
        assert_eq!(
            slot_adj_incoming_page_id(&slot),
            ADJ_PAGE_ID_SENTINEL,
            "a node with no adjacency patched must report the sentinel incoming head"
        );
        assert_eq!(
            slot_adj_flags(&slot),
            0,
            "a node with no adjacency patched must report no adjacency flags"
        );

        patch_adj_pointer(&mut slot, Some(42), None);

        assert_eq!(
            slot_adj_page_id(&slot),
            42,
            "patch_adj_pointer must persist the outgoing head page id"
        );
        assert_eq!(
            slot_adj_incoming_page_id(&slot),
            ADJ_PAGE_ID_SENTINEL,
            "no incoming head must leave the incoming field at the sentinel"
        );
        let flags = slot_adj_flags(&slot);
        assert_eq!(
            flags & ADJ_FLAG_OUTGOING,
            ADJ_FLAG_OUTGOING,
            "outgoing flag set"
        );
        assert_eq!(flags & ADJ_FLAG_INCOMING, 0, "incoming flag clear");
    }

    #[test]
    fn node_slot_adj_pointer_round_trip_both_directions() {
        // A node that is both a source and a target stores TWO distinct heads:
        // the outgoing chain page and the incoming chain page. This is the
        // format that lets resolve_adj_pointer stop scanning for bidirectional
        // nodes (cycle 7).
        let node = make_node(2, "N", Properties::new());
        let (mut slot, _) = encode_node_slot(&node).expect("encode must succeed");

        patch_adj_pointer(&mut slot, Some(7), Some(9));

        assert_eq!(slot_adj_page_id(&slot), 7, "outgoing head persisted");
        assert_eq!(
            slot_adj_incoming_page_id(&slot),
            9,
            "incoming head persisted"
        );
        let flags = slot_adj_flags(&slot);
        assert_eq!(
            flags & ADJ_FLAG_OUTGOING,
            ADJ_FLAG_OUTGOING,
            "outgoing flag set"
        );
        assert_eq!(
            flags & ADJ_FLAG_INCOMING,
            ADJ_FLAG_INCOMING,
            "incoming flag set"
        );

        // Overwriting with incoming-only must clear the outgoing head and flag.
        patch_adj_pointer(&mut slot, None, Some(11));
        assert_eq!(
            slot_adj_page_id(&slot),
            ADJ_PAGE_ID_SENTINEL,
            "outgoing head cleared"
        );
        assert_eq!(
            slot_adj_incoming_page_id(&slot),
            11,
            "incoming head updated"
        );
        let flags = slot_adj_flags(&slot);
        assert_eq!(flags & ADJ_FLAG_OUTGOING, 0, "outgoing flag cleared");
        assert_eq!(
            flags & ADJ_FLAG_INCOMING,
            ADJ_FLAG_INCOMING,
            "incoming flag set"
        );
    }

    #[test]
    fn slot_inline_label_rejects_invalid_utf8() {
        let mut slot = [0u8; NODE_SLOT_SIZE];
        slot[OFF_FLAGS] = SLOT_LIVE;
        slot[OFF_LABEL_LEN] = 3;
        slot[OFF_LABEL_INLINE] = 0xFF;
        slot[OFF_LABEL_INLINE + 1] = 0xFE;
        slot[OFF_LABEL_INLINE + 2] = 0xFD;
        let result = slot_inline_label(&slot, 7);
        assert!(result.is_err(), "expected Err for invalid UTF-8, got Ok");
        match result.unwrap_err() {
            crate::Error::CorruptPage { page_id, .. } => assert_eq!(page_id, 7),
            other => panic!("expected CorruptPage, got {other:?}"),
        }
    }
}

/// Property-based tests (issue #67).
///
/// The round trip here spans two layers: a 128-byte slot plus whatever spilled
/// out of it. A label too long to sit inline goes to the string heap, and
/// properties too large for the inline budget go to overflow pages, with the
/// slot holding only references. The invariant has to cover the whole journey,
/// because a value that survives encoding but gets lost on the way back is
/// exactly the failure #62 was about.
///
/// The two resolver closures stand in for the string heap and the overflow
/// pages. That is not a shortcut: production hands `decode_node_slot` bytes it
/// already fetched (see `graph.rs:3149`), so passing them from a local is the
/// same contract without the disk.
#[cfg(test)]
mod proptests {
    use super::*;
    use crate::property::Property;
    use proptest::prelude::*;

    fn size_around(limit: usize) -> impl Strategy<Value = usize> {
        prop_oneof![
            4 => 0..=limit,
            3 => limit.saturating_sub(5)..=limit + 5,
            3 => (limit + 1)..=(limit * 2).max(limit + 2),
        ]
    }

    /// Labels drawn around the inline cap (47 bytes), so both the inline path
    /// and the string-heap overflow path get exercised.
    fn label_strategy() -> impl Strategy<Value = String> {
        size_around(NODE_LABEL_INLINE_MAX).prop_map(|n| "L".repeat(n.max(1)))
    }

    /// Property values sized around the *inline* budget rather than the u16
    /// limit: for this codec the interesting boundary is where a map stops
    /// fitting in the slot and spills to overflow pages.
    fn properties_strategy() -> impl Strategy<Value = Properties> {
        proptest::collection::hash_map(
            "[a-z]{1,10}",
            prop_oneof![
                size_around(NODE_PROP_INLINE_MAX).prop_map(|n| Property::String("v".repeat(n))),
                any::<i64>().prop_map(Property::I64),
                any::<bool>().prop_map(Property::Bool),
            ],
            0..5,
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// Accept or reject, but never lie — across the slot *and* whatever
        /// overflowed out of it.
        #[test]
        fn slot_roundtrip_matches_or_encode_rejects_but_never_corrupts(
            id in 1u64..=u64::MAX,
            label in label_strategy(),
            props in properties_strategy(),
        ) {
            let node = Node::new(NodeId(id), &label, props.clone());

            let Ok((slot, overflow)) = encode_node_slot(&node) else {
                return Ok(()); // refusing is a valid answer
            };

            // Stand in for the string heap and the overflow pages: hand back
            // exactly what encoding said had spilled.
            let spilled_label = overflow.label_bytes;
            let spilled_props = overflow.props_bytes;

            let decoded = decode_node_slot(
                &slot,
                0,
                |_| {
                    spilled_label
                        .as_ref()
                        .map(|b| String::from_utf8(b.clone()).expect("label was valid UTF-8"))
                        .ok_or(crate::Error::CorruptPage {
                            file: "nodes.db",
                            page_id: 0,
                            reason: "label resolver called but nothing overflowed",
                        })
                },
                |_| {
                    spilled_props.clone().ok_or(crate::Error::CorruptPage {
                        file: "nodes.db",
                        page_id: 0,
                        reason: "props resolver called but nothing overflowed",
                    })
                },
            )
            .expect("what encode accepted, decode must read")
            .expect("a live slot must decode to a node");

            prop_assert_eq!(decoded.id, node.id, "id changed");
            prop_assert_eq!(decoded.label(), node.label(), "label changed");
            prop_assert_eq!(decoded.properties(), &props, "properties changed");
        }

        /// A resolver must be called exactly when the encoder said something
        /// spilled — never speculatively, and never skipped when it is needed.
        ///
        /// This is what stops the two halves from drifting apart: an encoder
        /// that overflows a value while the decoder believes it is inline
        /// reads garbage from the slot, which is the shape of the #62 bug one
        /// layer up.
        #[test]
        fn resolvers_are_called_exactly_when_something_overflowed(
            id in 1u64..=u64::MAX,
            label in label_strategy(),
            props in properties_strategy(),
        ) {
            let node = Node::new(NodeId(id), &label, props);
            let Ok((slot, overflow)) = encode_node_slot(&node) else {
                return Ok(());
            };

            prop_assert_eq!(
                slot_needs_label_resolve(&slot),
                overflow.label_overflowed,
                "the slot and the encoder disagree about whether the label spilled"
            );
            prop_assert_eq!(
                slot_needs_prop_resolve(&slot),
                overflow.props_overflowed,
                "the slot and the encoder disagree about whether the properties spilled"
            );
        }

        /// Decoding must never abort the process, whatever the 128 bytes say.
        /// A slot is read straight off a page, so a panic here is reachable
        /// from a corrupt file.
        #[test]
        fn decode_never_panics_on_arbitrary_slot_bytes(
            bytes in proptest::collection::vec(any::<u8>(), NODE_SLOT_SIZE)
        ) {
            let mut slot = [0u8; NODE_SLOT_SIZE];
            slot.copy_from_slice(&bytes);

            let outcome = decode_node_slot(
                &slot,
                0,
                |_| Ok("recovered".to_owned()),
                |_| Ok(Vec::new()),
            );
            prop_assert!(outcome.is_ok() || outcome.is_err());
        }
    }
}
