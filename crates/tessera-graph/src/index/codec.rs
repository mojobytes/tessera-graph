// SPDX-License-Identifier: MIT

// Index serialization codec for `index.bin`.
//
// Format (little-endian):
//   magic:       [u8; 4]  = b"TGIX"
//   version:     u16      = 2
//   _pad:        [u8; 2]  = 0
//   entry_count: u32      — total entries (nodes + edges)
//   entries:     repeated entry_count times:
//     kind:      u8       — 0x01 = node, 0x02 = edge
//     label_len: u32      — a u16 in version 1 (widened by issue #75)
//     label:     [u8; label_len]
//     id:        u64

use super::LabelIndex;
use crate::error::{Error, Result};

pub const INDEX_MAGIC: [u8; 4] = *b"TGIX";
/// Version history:
/// - `1`: initial format, `label_len: u16`.
/// - `2`: issue #75 — `label_len` widened to `u32`, matching the property
///   codec and string heap. A version-1 file is rejected on open: entries are
///   sequential with no separator, so reading 2-byte lengths as 4-byte ones
///   desynchronises every entry in the file.
pub const INDEX_VERSION: u16 = 2;

const HEADER_SIZE: usize = 4 + 2 + 2 + 4; // magic + version + pad + entry_count

const KIND_NODE: u8 = 0x01;
const KIND_EDGE: u8 = 0x02;

/// Serializes the node and edge label indexes into a byte vector.
///
/// # Errors
///
/// Returns [`Error::RecordTooLarge`] if the total number of entries exceeds
/// `u32::MAX`, or if any label's byte length does. Either would silently
/// truncate the field recording it and produce a corrupt `index.bin`.
pub fn serialize(nodes: &LabelIndex, edges: &LabelIndex) -> Result<Vec<u8>> {
    let entry_count = nodes.entry_count() + edges.entry_count();
    let count =
        u32::try_from(entry_count).map_err(|_| Error::RecordTooLarge { size: entry_count })?;

    // Estimate: 1 (kind) + 4 (label_len) + avg_label_bytes + 8 (id). Using 20 bytes/entry is
    // conservative for typical short labels and avoids reallocations in the common case.
    let mut buf = Vec::with_capacity(HEADER_SIZE + entry_count * 20);

    // Header
    buf.extend_from_slice(&INDEX_MAGIC);
    buf.extend_from_slice(&INDEX_VERSION.to_le_bytes());
    buf.extend_from_slice(&[0u8; 2]); // pad
    buf.extend_from_slice(&count.to_le_bytes());

    // Entries
    for (label, id) in nodes.iter() {
        write_entry(&mut buf, KIND_NODE, label, id)?;
    }
    for (label, id) in edges.iter() {
        write_entry(&mut buf, KIND_EDGE, label, id)?;
    }

    Ok(buf)
}

/// Writes one `kind + label_len + label + id` entry.
///
/// # Errors
///
/// Returns [`Error::RecordTooLarge`] for a label whose byte length does not
/// fit the `u32` length field (a `u16` until issue #75). This is worse than a
/// truncated label: entries are read back sequentially with no separator, so
/// a wrapped length desynchronises every entry after it in `index.bin`
/// (issue #62).
fn write_entry(buf: &mut Vec<u8>, kind: u8, label: &str, id: u64) -> Result<()> {
    let label_len =
        u32::try_from(label.len()).map_err(|_| Error::RecordTooLarge { size: label.len() })?;
    buf.push(kind);
    buf.extend_from_slice(&label_len.to_le_bytes());
    buf.extend_from_slice(label.as_bytes());
    buf.extend_from_slice(&id.to_le_bytes());
    Ok(())
}

/// Deserializes a byte slice back into node and edge label indexes.
pub fn deserialize(data: &[u8]) -> Result<(LabelIndex, LabelIndex)> {
    if data.len() < HEADER_SIZE {
        return Err(Error::CorruptIndex("file too short for header"));
    }

    // Magic
    if data[..4] != INDEX_MAGIC {
        return Err(Error::CorruptIndex("invalid magic bytes"));
    }

    // Version
    let version = u16::from_le_bytes([data[4], data[5]]);
    if version != INDEX_VERSION {
        return Err(Error::IncompatibleVersion {
            found: version,
            expected: INDEX_VERSION,
        });
    }

    // Entry count
    let entry_count = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;

    let mut nodes = LabelIndex::new();
    let mut edges = LabelIndex::new();
    let mut offset = HEADER_SIZE;

    for _ in 0..entry_count {
        // kind (1 byte)
        let kind = *data
            .get(offset)
            .ok_or(Error::CorruptIndex("truncated entry: missing kind"))?;
        offset += 1;

        // label_len (4 bytes; 2 in version 1, widened by issue #75)
        let label_len_bytes: [u8; 4] = data
            .get(offset..offset + 4)
            .ok_or(Error::CorruptIndex("truncated entry: missing label_len"))?
            .try_into()
            .expect("slice is 4 bytes");
        let label_len = u32::from_le_bytes(label_len_bytes) as usize;
        offset += 4;

        // label (label_len bytes)
        let label_bytes = data
            .get(offset..offset + label_len)
            .ok_or(Error::CorruptIndex("truncated entry: missing label data"))?;
        let label = std::str::from_utf8(label_bytes)
            .map_err(|_| Error::CorruptIndex("invalid UTF-8 in label"))?;
        offset += label_len;

        // id (8 bytes)
        let id_bytes: [u8; 8] = data
            .get(offset..offset + 8)
            .ok_or(Error::CorruptIndex("truncated entry: missing id"))?
            .try_into()
            .expect("slice is 8 bytes");
        let id = u64::from_le_bytes(id_bytes);
        offset += 8;

        match kind {
            KIND_NODE => nodes.insert(label, id),
            KIND_EDGE => edges.insert(label, id),
            _ => return Err(Error::CorruptIndex("unknown entry kind")),
        }
    }

    Ok((nodes, edges))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Label length limit (issues #62 and #75) ---
    //
    // Each entry records its label length in a u32 (a u16 until issue #75).
    // Under the old width, past 65,535 the cast used to wrap, and since
    // entries are read back sequentially with no separator, a short length
    // did not just truncate that one label — it desynchronised every entry
    // after it in `index.bin`. The count check above (`entry_count`) already
    // guarded its own field this way; the label length did not. #75 then
    // widened the field, so labels past 64 KiB are legitimate.

    #[test]
    fn serialize_accepts_label_exactly_65535_bytes() {
        let mut nodes = LabelIndex::new();
        nodes.insert(&"L".repeat(65_535), 1);
        let edges = LabelIndex::new();

        let bytes = serialize(&nodes, &edges).unwrap();
        let (n, _) = deserialize(&bytes).unwrap();
        assert_eq!(
            n.entry_count(),
            1,
            "the largest length a u16 holds must still round-trip"
        );
    }

    /// 70,000 bytes exceeded the old u16 field and was rejected (v0.11.1,
    /// issue #62). Since #75 widened the field to u32, labels past 64 KiB
    /// must round-trip — and an entry AFTER the big one must survive, since
    /// entries are sequential with no separator and a wrong length
    /// desynchronises everything that follows.
    #[test]
    fn roundtrip_label_over_u16_limit_keeps_later_entries_in_sync() {
        let mut nodes = LabelIndex::new();
        nodes.insert(&"L".repeat(283_718), 1);
        nodes.insert("Person", 2);
        let mut edges = LabelIndex::new();
        edges.insert("KNOWS", 100);

        let bytes = serialize(&nodes, &edges).unwrap();
        let (n, e) = deserialize(&bytes).unwrap();

        assert_eq!(n.ids_for(&"L".repeat(283_718)), vec![1]);
        assert_eq!(
            n.ids_for("Person"),
            vec![2],
            "entry after the big label lost"
        );
        assert_eq!(e.ids_for("KNOWS"), vec![100], "edge entries desynchronised");
    }

    /// The *edge* loop writes through the same `write_entry`, but a
    /// regression splitting the two paths must not reintroduce the old cap
    /// on one side only.
    #[test]
    fn roundtrip_over_long_edge_label() {
        let nodes = LabelIndex::new();
        let mut edges = LabelIndex::new();
        edges.insert(&"E".repeat(70_000), 1);

        let bytes = serialize(&nodes, &edges).unwrap();
        let (_, e) = deserialize(&bytes).unwrap();
        assert_eq!(e.ids_for(&"E".repeat(70_000)), vec![1]);
    }

    #[test]
    fn roundtrip_empty_index() {
        let nodes = LabelIndex::new();
        let edges = LabelIndex::new();
        let bytes = serialize(&nodes, &edges).unwrap();
        let (n, e) = deserialize(&bytes).unwrap();
        assert_eq!(n.entry_count(), 0);
        assert_eq!(e.entry_count(), 0);
    }

    #[test]
    fn roundtrip_node_entries() {
        let mut nodes = LabelIndex::new();
        nodes.insert("Person", 1);
        nodes.insert("Person", 2);
        nodes.insert("Device", 10);
        let edges = LabelIndex::new();

        let bytes = serialize(&nodes, &edges).unwrap();
        let (n, e) = deserialize(&bytes).unwrap();

        let mut persons = n.ids_for("Person");
        persons.sort_unstable();
        assert_eq!(persons, vec![1, 2]);
        assert_eq!(n.ids_for("Device"), vec![10]);
        assert_eq!(e.entry_count(), 0);
    }

    #[test]
    fn roundtrip_edge_entries() {
        let nodes = LabelIndex::new();
        let mut edges = LabelIndex::new();
        edges.insert("KNOWS", 100);
        edges.insert("FOLLOWS", 200);

        let bytes = serialize(&nodes, &edges).unwrap();
        let (n, e) = deserialize(&bytes).unwrap();

        assert_eq!(n.entry_count(), 0);
        assert_eq!(e.ids_for("KNOWS"), vec![100]);
        assert_eq!(e.ids_for("FOLLOWS"), vec![200]);
    }

    #[test]
    fn roundtrip_mixed_entries() {
        let mut nodes = LabelIndex::new();
        nodes.insert("Person", 1);
        let mut edges = LabelIndex::new();
        edges.insert("KNOWS", 100);

        let bytes = serialize(&nodes, &edges).unwrap();
        let (n, e) = deserialize(&bytes).unwrap();

        assert_eq!(n.ids_for("Person"), vec![1]);
        assert_eq!(e.ids_for("KNOWS"), vec![100]);
    }

    #[test]
    fn wrong_magic_returns_error() {
        let mut bytes = serialize(&LabelIndex::new(), &LabelIndex::new()).unwrap();
        bytes[0] = b'X';
        let err = deserialize(&bytes).unwrap_err();
        assert!(
            matches!(err, Error::CorruptIndex(msg) if msg.contains("magic")),
            "expected CorruptIndex with magic, got {err:?}"
        );
    }

    #[test]
    fn wrong_version_returns_error() {
        let mut bytes = serialize(&LabelIndex::new(), &LabelIndex::new()).unwrap();
        // version is at offset 4..6, set to 99
        bytes[4] = 99;
        bytes[5] = 0;
        let err = deserialize(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                Error::IncompatibleVersion {
                    found: 99,
                    expected: INDEX_VERSION
                }
            ),
            "expected IncompatibleVersion, got {err:?}"
        );
    }

    /// The index format version in use before issue #75 widened `label_len`
    /// from u16 to u32. A version-1 file must be rejected, never reinterpreted:
    /// its entries record label lengths in 2 bytes, so reading them with the
    /// 4-byte field would desynchronise every entry in the file.
    const PRE_U32_LABEL_INDEX_VERSION: u16 = 1;

    #[test]
    fn deserialize_rejects_pre_u32_label_index_version() {
        let mut bytes = serialize(&LabelIndex::new(), &LabelIndex::new()).unwrap();
        bytes[4..6].copy_from_slice(&PRE_U32_LABEL_INDEX_VERSION.to_le_bytes());

        let err = deserialize(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                Error::IncompatibleVersion {
                    found: PRE_U32_LABEL_INDEX_VERSION,
                    expected: INDEX_VERSION,
                }
            ),
            "expected a clean IncompatibleVersion rejection, got {err:?}"
        );
        // Compile-time: the #75 field widening must bump INDEX_VERSION past
        // the old one, or a pre-#75 index.bin is misread silently.
        const {
            assert!(INDEX_VERSION > PRE_U32_LABEL_INDEX_VERSION);
        }
    }

    #[test]
    fn serialize_returns_ok_for_normal_counts() {
        let mut nodes = LabelIndex::new();
        nodes.insert("A", 1);
        let edges = LabelIndex::new();
        let bytes = serialize(&nodes, &edges).expect("serialization must succeed");
        let (n, _) = deserialize(&bytes).unwrap();
        assert_eq!(n.ids_for("A"), vec![1]);
    }

    #[test]
    fn truncated_data_returns_error() {
        // Too short for header
        let err = deserialize(&[0u8; 4]).unwrap_err();
        assert!(matches!(err, Error::CorruptIndex(_)));

        // Valid header claiming 1 entry but no entry data
        let mut nodes = LabelIndex::new();
        nodes.insert("X", 1);
        let bytes = serialize(&nodes, &LabelIndex::new()).unwrap();
        // Truncate to just the header
        let err = deserialize(&bytes[..HEADER_SIZE]).unwrap_err();
        assert!(matches!(err, Error::CorruptIndex(_)));
    }
}

/// Property-based tests (issue #67).
///
/// This codec has the nastiest failure mode of the ones #62 turned up: entries
/// are read back sequentially with no separator between them, so a label whose
/// recorded length is wrong does not corrupt just its own entry — it shifts
/// the read cursor and garbles **every entry after it**.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    /// Same lumpy distribution as `property_codec`: uniform sizes almost never
    /// land near a limit, and the limit is where this class of bug lives.
    fn size_around(limit: usize) -> impl Strategy<Value = usize> {
        prop_oneof![
            4 => 0..=limit,
            3 => limit.saturating_sub(5)..=limit + 5,
            3 => (limit + 1)..=(limit * 2).max(limit + 2),
        ]
    }

    /// ASCII, so generated length equals byte length.
    fn label_of_size(size: impl Strategy<Value = usize>) -> impl Strategy<Value = String> {
        size.prop_map(|n| "L".repeat(n))
    }

    /// Mostly short labels — this generator is about having *many* entries, so
    /// a cursor that drifts has somewhere to show up — but one in eight is
    /// drawn around the `u16` limit instead.
    ///
    /// That mix is deliberate and was corrected after measuring: with short
    /// labels only, reverting the length check left this suite green and only
    /// the `#[ignore]`d boundary test caught it. An invariant that needs an
    /// opt-in flag to fail is not protecting the everyday path.
    fn entries_strategy() -> impl Strategy<Value = Vec<(String, u64)>> {
        // The u16::MAX band is no longer a limit since #75, but stays as an
        // ordinary-large size (it is where #62's corruption lived). A rare
        // draw at the real ~283 KB size that motivated #75 is weighted down
        // because each such case moves ~300 KB through the codec.
        let label = prop_oneof![
            14 => "[a-z]{1,8}".prop_map(|s| s),
            2 => label_of_size(size_around(u16::MAX as usize)),
            1 => label_of_size(279_000..=288_000usize),
        ];
        proptest::collection::vec((label, any::<u64>()), 0..12)
    }

    fn build(entries: &[(String, u64)]) -> LabelIndex {
        let mut idx = LabelIndex::new();
        for (label, id) in entries {
            idx.insert(label, *id);
        }
        idx
    }

    /// Compares by iterated contents: `LabelIndex` has no `PartialEq`, and the
    /// contents are what the format has to preserve anyway.
    fn contents(idx: &LabelIndex) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = idx.iter().map(|(l, id)| (l.to_owned(), id)).collect();
        v.sort();
        v
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// Accept or reject, but never lie — and with many entries, so a
        /// cursor that drifts by even one byte shows up as garbage in the
        /// entries that follow rather than silently vanishing.
        #[test]
        fn roundtrip_matches_or_serialize_rejects_but_never_corrupts(
            node_entries in entries_strategy(),
            edge_entries in entries_strategy(),
        ) {
            let nodes = build(&node_entries);
            let edges = build(&edge_entries);

            let Ok(bytes) = serialize(&nodes, &edges) else {
                return Ok(()); // refusing is a valid answer
            };
            let (n, e) = deserialize(&bytes)
                .expect("what serialize accepted, deserialize must read");

            prop_assert_eq!(contents(&n), contents(&nodes), "node labels changed");
            prop_assert_eq!(contents(&e), contents(&edges), "edge labels changed");
        }

        /// Deserializing arbitrary bytes must not abort the process — this
        /// file is read at startup, so a panic here means a database that
        /// cannot be opened rather than one that reports a bad index.
        #[test]
        fn deserialize_never_panics_on_arbitrary_bytes(
            garbage in proptest::collection::vec(any::<u8>(), 0..1024)
        ) {
            let outcome = deserialize(&garbage);
            prop_assert!(outcome.is_ok() || outcome.is_err());
        }

        /// A truncated-but-well-formed prefix is the realistic corruption: a
        /// write that was cut short. It must be reported, never misread.
        #[test]
        fn truncated_index_is_rejected_not_misread(
            node_entries in entries_strategy(),
            cut_permille in 0u32..1000,
        ) {
            let nodes = build(&node_entries);
            let edges = LabelIndex::new();
            let Ok(bytes) = serialize(&nodes, &edges) else {
                return Ok(());
            };
            prop_assume!(bytes.len() > HEADER_SIZE);

            // Integer arithmetic on purpose: a float cut point would need a
            // lossy cast back, and the exact byte is irrelevant — any prefix
            // shorter than the whole exercises the same path.
            let keep = bytes.len() * cut_permille as usize / 1000;
            prop_assume!(keep < bytes.len());

            // Either it errors, or it reads back a prefix of the entries —
            // what it must never do is panic or invent entries that were
            // never written.
            if let Ok((n, _)) = deserialize(&bytes[..keep]) {
                let full = contents(&nodes);
                for entry in contents(&n) {
                    prop_assert!(
                        full.contains(&entry),
                        "a truncated read produced an entry that was never written: {:?}",
                        entry
                    );
                }
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

        /// One big label among ordinary ones, at the old u16 boundary (where
        /// #62's corruption lived) and at the real ~283 KB size from #75.
        /// Since #75 the whole range must serialize and round-trip with the
        /// surrounding entries intact — a wrong length would corrupt every
        /// entry serialized after the big one.
        ///
        /// Kept `#[ignore]` for cost: cases now move 65-290 KB labels (up
        /// from 60-80 KB before #75). Run with:
        /// `cargo test -p tessera-graph -- --ignored`
        #[test]
        #[ignore = "boundary sizes: 65-290KB labels; run with --ignored"]
        fn label_around_old_u16_boundary_and_real_case_never_corrupts(
            size in prop_oneof![
                65_530..=70_600usize,
                279_000..=288_000usize,
            ],
            others in entries_strategy(),
        ) {
            let mut nodes = build(&others);
            nodes.insert(&"L".repeat(size), 1);

            let bytes = serialize(&nodes, &LabelIndex::new())
                .expect("every size in this range fits the u32 width");
            let (n, _) = deserialize(&bytes)
                .expect("what serialize accepted, deserialize must read");
            prop_assert_eq!(contents(&n), contents(&nodes));
        }
    }
}
