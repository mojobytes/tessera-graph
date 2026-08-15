// SPDX-License-Identifier: MIT

//! Binary codec for `schema.bin`.
//!
//! # Wire format (little-endian)
//! ```text
//! magic:       [u8; 4]  = b"TGSC"
//! version:     u16      = 1
//! _pad:        [u8; 2]  = 0
//! idx_count:   u32
//! con_count:   u32
//! entries...   repeated (idx_count + con_count) times:
//!   label_len: u16
//!   label:     [u8; label_len]
//!   prop_len:  u16
//!   prop:      [u8; prop_len]
//! append-only block (OPTIONAL, issue #43 Part A):
//!   ao_count:  u32
//!   labels...  repeated ao_count times:
//!     label_len:     u16
//!     label:         [u8; label_len]
//!     since_node_id: u64   (issue #61; absent in files written before it)
//! ```
//!
//! The `u16` length prefixes here are DELIBERATELY narrower than the `u32`
//! ones issue #75 introduced in the property codec, string heap and label
//! index. Those three carry user *data* (a legal text legitimately exceeds
//! 64 KiB); these carry schema *names* — labels and property keys — which
//! elsewhere in the engine are capped far below (property keys at 255 bytes,
//! `property_codec.rs`). Widening here would force a `SCHEMA_VERSION` bump
//! and a migration for no reachable case. If a real >64 KiB schema name ever
//! appears, widen these WITH a version bump — the entries are sequential
//! with no separator, so a wrapped length desynchronises the whole file
//! (the #62 failure mode).
//!
//! Index entries precede constraint entries; the two counts in the header
//! tell the reader how many of each to expect. The append-only block is
//! appended after the constraint entries and detected by "bytes remain after
//! the last constraint" — the header carries no total length, so a pre-#43
//! file simply ends there and the reader declares zero append-only labels.
//! This keeps [`SCHEMA_VERSION`] at 1: old files open unchanged, and a new
//! file with no append-only labels omits the block entirely. An empty catalog
//! (no indexes, no constraints, no append-only labels) serialises to an empty
//! byte vector (the file is simply not written on flush).
//!
//! # `since_node_id` and files written before issue #61
//!
//! Each append-only entry carries the node id its declaration started applying
//! from, so a reopen reconstructs the same membership the running graph had
//! instead of capturing every node of the label. Entries written before #61
//! carry only the label, and the version stays at 1 rather than rising: the
//! block already ends at the end of the file, so the reader distinguishes the
//! two shapes by how many bytes are left, exactly as it already distinguishes
//! a pre-#43 file.
//!
//! An old entry is read as `since_node_id = 0`, meaning "covers every node of
//! this label". That is precisely the behaviour those files had — before #61
//! the rebuild captured every node of a declared label — so upgrading preserves
//! it rather than silently changing which nodes are exempt.

use super::SchemaCatalog;
use crate::error::{Error, Result};

pub const SCHEMA_MAGIC: [u8; 4] = *b"TGSC";
pub const SCHEMA_VERSION: u16 = 1;

/// magic + version + pad + `idx_count` + `con_count`.
const HEADER_SIZE: usize = 4 + 2 + 2 + 4 + 4;

/// Serialises the catalog. Returns an empty vec for an empty catalog
/// (the file is not written when empty to avoid touching disk unnecessarily).
///
/// # Errors
///
/// Returns [`Error::RecordTooLarge`] if the entry count exceeds `u32::MAX`
/// or any label/property exceeds `u16::MAX` bytes.
pub fn serialize(catalog: &SchemaCatalog) -> Result<Vec<u8>> {
    let indexes = catalog.indexes();
    let constraints = catalog.constraints();
    let append_only = catalog.append_only_labels();
    if indexes.is_empty() && constraints.is_empty() && append_only.is_empty() {
        return Ok(Vec::new());
    }
    let idx_count = u32::try_from(indexes.len()).map_err(|_| Error::RecordTooLarge {
        size: indexes.len(),
    })?;
    let con_count = u32::try_from(constraints.len()).map_err(|_| Error::RecordTooLarge {
        size: constraints.len(),
    })?;

    let mut buf = Vec::with_capacity(HEADER_SIZE + (indexes.len() + constraints.len()) * 32);
    buf.extend_from_slice(&SCHEMA_MAGIC);
    buf.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    buf.extend_from_slice(&[0u8; 2]); // pad
    buf.extend_from_slice(&idx_count.to_le_bytes());
    buf.extend_from_slice(&con_count.to_le_bytes());
    for idx in &indexes {
        write_entry(&mut buf, &idx.label, &idx.prop)?;
    }
    for con in &constraints {
        write_entry(&mut buf, &con.label, &con.prop)?;
    }
    // Optional append-only block (issue #43 Part A). Only written when there
    // are labels to record: a catalog with none omits it entirely, so a file
    // that predates this block and one that simply declares no append-only
    // labels are byte-identical, and old readers stop at the last constraint.
    if !append_only.is_empty() {
        let ao_count = u32::try_from(append_only.len()).map_err(|_| Error::RecordTooLarge {
            size: append_only.len(),
        })?;
        buf.extend_from_slice(&ao_count.to_le_bytes());
        for decl in &append_only {
            write_label(&mut buf, &decl.label)?;
            buf.extend_from_slice(&decl.since_node_id.to_le_bytes());
        }
    }
    Ok(buf)
}

fn write_entry(buf: &mut Vec<u8>, label: &str, prop: &str) -> Result<()> {
    let llen =
        u16::try_from(label.len()).map_err(|_| Error::RecordTooLarge { size: label.len() })?;
    let plen = u16::try_from(prop.len()).map_err(|_| Error::RecordTooLarge { size: prop.len() })?;
    buf.extend_from_slice(&llen.to_le_bytes());
    buf.extend_from_slice(label.as_bytes());
    buf.extend_from_slice(&plen.to_le_bytes());
    buf.extend_from_slice(prop.as_bytes());
    Ok(())
}

/// Writes a bare `(label_len: u16, label)` entry — used by the append-only
/// block, whose entries carry a label but no property.
fn write_label(buf: &mut Vec<u8>, label: &str) -> Result<()> {
    let llen =
        u16::try_from(label.len()).map_err(|_| Error::RecordTooLarge { size: label.len() })?;
    buf.extend_from_slice(&llen.to_le_bytes());
    buf.extend_from_slice(label.as_bytes());
    Ok(())
}

/// Deserialises a catalog from bytes read from `schema.bin`.
///
/// # Errors
///
/// Returns [`Error::InvalidMagic`] on a bad magic, [`Error::IncompatibleVersion`]
/// on an unknown version, or [`Error::CorruptIndex`] on a header/entry that is
/// truncated or carries a non-UTF-8 string.
pub fn deserialize(bytes: &[u8]) -> Result<SchemaCatalog> {
    if bytes.len() < HEADER_SIZE {
        return Err(Error::CorruptIndex("schema.bin: header too short"));
    }
    if bytes[..4] != SCHEMA_MAGIC {
        return Err(Error::InvalidMagic("schema.bin"));
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != SCHEMA_VERSION {
        return Err(Error::IncompatibleVersion {
            found: version,
            expected: SCHEMA_VERSION,
        });
    }
    let idx_count = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let con_count = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
    let mut pos = HEADER_SIZE;
    let mut cat = SchemaCatalog::new();

    for _ in 0..idx_count {
        let (label, prop, new_pos) = read_entry(bytes, pos)?;
        pos = new_pos;
        cat.add_index(&label, &prop);
    }
    for _ in 0..con_count {
        let (label, prop, new_pos) = read_entry(bytes, pos)?;
        pos = new_pos;
        cat.add_unique_constraint(&label, &prop);
    }
    // Optional append-only block (issue #43 Part A). Bytes remaining after the
    // last constraint mean this file carries the block; a pre-#43 file ends
    // exactly here, so its absence is normal, not corruption. Read defensively
    // via `bytes.get`/`read_u16`/`read_str` so a truncated block errors rather
    // than panicking.
    if pos < bytes.len() {
        let ao_count = read_u32(bytes, pos)? as usize;
        pos += 4;
        for _ in 0..ao_count {
            let (label, new_pos) = read_label(bytes, pos)?;
            pos = new_pos;
            // Issue #61: entries written before it stop at the label, so the
            // 8-byte boundary is present only if the bytes are actually there.
            // Reading it as absent when missing gives `0` — "covers every node
            // of this label" — which is what those files meant.
            //
            // `first_chunk` rather than a slice plus `try_into`: it yields a
            // fixed-size array directly, so the 8-byte width is guaranteed by
            // the type and there is no fallible conversion to unwrap. This
            // function is called on every `open()`, and a panic here is a
            // database that will not start.
            //
            // A truncated final entry reads as an old-format one instead of
            // erroring, and 0 is the conservative answer: it exempts more nodes
            // from versioning, never fewer, so no committed write is skipped.
            // `bytes.get(pos..)` rather than `bytes[pos..]`: `pos` is derived
            // from a length field in the file, so an oversized one must yield
            // `None`, not an out-of-bounds panic.
            let since_node_id = match bytes.get(pos..).and_then(<[u8]>::first_chunk::<8>) {
                Some(b) => {
                    pos += 8;
                    u64::from_le_bytes(*b)
                }
                // The bound is absent. That is only legitimate at the very END
                // of the block: a pre-#61 file stops after each label, and its
                // last entry is followed by nothing.
                //
                // Running out mid-block is different — the bytes left are not a
                // declaration, and carrying on would read them as the next
                // entry's length and label. A truncated file then parses into
                // an invented catalog: a real fixture produced the label
                // "Audi", assembled from the leftovers of the previous entry's
                // bound. Corrupt input must be reported, never guessed at.
                None if pos < bytes.len() => {
                    return Err(Error::CorruptIndex(
                        "schema.bin: append-only entry truncated mid-block",
                    ));
                }
                None => 0,
            };
            cat.mark_label_append_only(&label, since_node_id);
        }
    }
    Ok(cat)
}

fn read_entry(bytes: &[u8], pos: usize) -> Result<(String, String, usize)> {
    let llen = read_u16(bytes, pos)? as usize;
    let pos = pos + 2;
    let label = read_str(bytes, pos, llen)?;
    let pos = pos + llen;
    let plen = read_u16(bytes, pos)? as usize;
    let pos = pos + 2;
    let prop = read_str(bytes, pos, plen)?;
    Ok((label, prop, pos + plen))
}

fn read_u16(bytes: &[u8], pos: usize) -> Result<u16> {
    bytes
        .get(pos..pos + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or(Error::CorruptIndex("schema.bin: truncated entry"))
}

fn read_u32(bytes: &[u8], pos: usize) -> Result<u32> {
    bytes
        .get(pos..pos + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(Error::CorruptIndex(
            "schema.bin: truncated append-only count",
        ))
}

/// Reads a bare `(label_len: u16, label)` entry from the append-only block,
/// returning the label and the position just past it.
fn read_label(bytes: &[u8], pos: usize) -> Result<(String, usize)> {
    let llen = read_u16(bytes, pos)? as usize;
    let pos = pos + 2;
    let label = read_str(bytes, pos, llen)?;
    Ok((label, pos + llen))
}

fn read_str(bytes: &[u8], pos: usize, len: usize) -> Result<String> {
    let slice = bytes
        .get(pos..pos + len)
        .ok_or(Error::CorruptIndex("schema.bin: truncated string"))?;
    std::str::from_utf8(slice)
        .map(str::to_owned)
        .map_err(|_| Error::CorruptIndex("schema.bin: non-UTF-8 string"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SchemaCatalog;

    #[test]
    fn empty_catalog_serializes_to_empty_vec() {
        let cat = SchemaCatalog::default();
        let bytes = serialize(&cat).unwrap();
        assert!(bytes.is_empty(), "empty catalog must produce empty bytes");
    }

    #[test]
    fn roundtrip_indexes_and_constraints() {
        let mut cat = SchemaCatalog::default();
        cat.add_index("Person", "id");
        cat.add_index("Asset", "status");
        cat.add_unique_constraint("Asset", "id");

        let bytes = serialize(&cat).unwrap();
        assert!(!bytes.is_empty());
        let restored = deserialize(&bytes).unwrap();

        assert!(restored.has_index("Person", "id"));
        assert!(restored.has_index("Asset", "status"));
        assert!(restored.has_unique_constraint("Asset", "id"));
        assert!(!restored.has_unique_constraint("Person", "id")); // not declared
        assert_eq!(restored.indexes().len(), 2);
        assert_eq!(restored.constraints().len(), 1);
    }

    #[test]
    fn bad_magic_returns_error() {
        let bad = b"XXXX\x01\x00\x00\x00\x00\x00\x00\x00";
        assert!(deserialize(bad).is_err());
    }

    #[test]
    fn truncated_bytes_returns_error() {
        // Write a valid header but truncate before entries.
        let mut cat = SchemaCatalog::default();
        cat.add_index("L", "p");
        let bytes = serialize(&cat).unwrap();
        let truncated = &bytes[..bytes.len() / 2];
        assert!(deserialize(truncated).is_err());
    }

    #[test]
    fn schema_magic_is_tgsc() {
        assert_eq!(&SCHEMA_MAGIC, b"TGSC");
    }

    // ── Issue #43 Part A: append-only labels persist in schema.bin ───────

    #[test]
    fn roundtrip_append_only_labels() {
        let mut cat = SchemaCatalog::default();
        cat.add_index("Person", "id");
        cat.mark_label_append_only("Event", 0);
        cat.mark_label_append_only("AuditLog", 0);

        let bytes = serialize(&cat).unwrap();
        let restored = deserialize(&bytes).unwrap();

        assert!(restored.has_index("Person", "id"));
        assert!(restored.is_label_append_only("Event"));
        assert!(restored.is_label_append_only("AuditLog"));
        assert!(!restored.is_label_append_only("Person"));
        assert_eq!(restored.append_only_labels().len(), 2);
    }

    #[test]
    fn catalog_with_only_append_only_labels_serializes_non_empty() {
        // An append-only declaration alone must be enough to write the file —
        // the empty-catalog guard must not treat "no indexes, no constraints"
        // as empty when append-only labels are present.
        let mut cat = SchemaCatalog::default();
        cat.mark_label_append_only("Event", 0);
        let bytes = serialize(&cat).unwrap();
        assert!(!bytes.is_empty(), "append-only-only catalog must serialize");
        let restored = deserialize(&bytes).unwrap();
        assert!(restored.is_label_append_only("Event"));
    }

    #[test]
    fn schema_without_append_only_block_opens_with_none_declared() {
        // Replicate the pre-#43 on-disk format by hand: header with counts, then
        // index/constraint entries, and NOTHING after. The reader must treat the
        // absent trailing block as "no append-only labels", not as corruption.
        let mut buf = Vec::new();
        buf.extend_from_slice(&SCHEMA_MAGIC);
        buf.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
        buf.extend_from_slice(&[0u8; 2]); // pad
        buf.extend_from_slice(&1u32.to_le_bytes()); // idx_count
        buf.extend_from_slice(&0u32.to_le_bytes()); // con_count
        // one index entry: label "L", prop "p"
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(b"L");
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(b"p");

        let restored = deserialize(&buf).unwrap();
        assert!(restored.has_index("L", "p"));
        assert!(
            restored.append_only_labels().is_empty(),
            "old-format schema.bin declares no append-only labels",
        );
    }

    /// The same guarantee, but pinned against bytes produced by the ACTUAL
    /// pre-#43 serializer (built from the codec at commit 81e76e3) rather than
    /// a format replicated by hand — a hand-rolled fixture would keep passing
    /// even if it had drifted from what the old code really wrote.
    ///
    /// Fixture: two indexes (`Person.id`, `Asset.status`) and one unique
    /// constraint (`Asset.id`). A real 0.11.0 `schema.bin` must open with every
    /// index and constraint intact and no append-only labels declared.
    /// Issue #61 widened each append-only entry with an 8-byte node-id bound
    /// while leaving [`SCHEMA_VERSION`] at 1, so a file written between #43 and
    /// #61 must still open — its entries stop right after the label.
    ///
    /// Those entries are read as bound 0, "covers every node of this label",
    /// which is exactly what that build did: its rebuild captured every node of
    /// a declared label. So an upgrade preserves which nodes are exempt instead
    /// of quietly changing it.
    ///
    /// Fixture hand-built to the old shape — one index (`Person.id`) and one
    /// append-only label (`Event`) with no bound after it.
    #[test]
    fn append_only_entry_without_a_bound_reads_as_covering_every_node() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&SCHEMA_MAGIC);
        buf.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
        buf.extend_from_slice(&[0u8; 2]); // pad
        buf.extend_from_slice(&1u32.to_le_bytes()); // idx_count
        buf.extend_from_slice(&0u32.to_le_bytes()); // con_count
        buf.extend_from_slice(&6u16.to_le_bytes());
        buf.extend_from_slice(b"Person");
        buf.extend_from_slice(&2u16.to_le_bytes());
        buf.extend_from_slice(b"id");
        // Append-only block in the pre-#61 shape: count, then label only.
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&5u16.to_le_bytes());
        buf.extend_from_slice(b"Event");

        let restored = deserialize(&buf).unwrap();
        assert!(restored.has_index("Person", "id"));
        assert!(restored.is_label_append_only("Event"));
        assert_eq!(
            restored.append_only_since("Event"),
            Some(0),
            "an entry with no bound must cover every node, as it used to"
        );
    }

    /// A file truncated in the MIDDLE of an append-only entry must be rejected,
    /// not silently accepted with an invented catalog.
    ///
    /// The absent-bound fallback exists for pre-#61 files, where entries stop
    /// after the label and nothing follows the last one. Reading a missing
    /// bound as 0 is right there. But if the bytes run out mid-entry when
    /// further entries are still expected, the remaining bytes are not a
    /// declaration at all — continuing to parse reads whatever follows as a
    /// label length and a label, and can accept a corrupt name with an
    /// arbitrary bound.
    ///
    /// Fixture: header, `ao_count = 2`, entry `Event` with only 6 of its 8
    /// bound bytes present. Those 6 bytes end in a plausible-looking length
    /// prefix followed by ASCII, which is exactly the shape that parses as a
    /// second entry by accident.
    #[test]
    fn entry_truncated_mid_bound_with_more_entries_expected_is_rejected() {
        let truncated: &[u8] = &[
            84, 71, 83, 67, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 5, 0, 69, 118, 101,
            110, 116, 4, 0, 65, 117, 100, 105,
        ];

        let outcome = deserialize(truncated);

        assert!(
            outcome.is_err(),
            "a mid-entry truncation must be reported, not parsed into an \
             invented catalog: {:?}",
            outcome.map(|c| c
                .append_only_labels()
                .iter()
                .map(|d| (d.label.clone(), d.since_node_id))
                .collect::<Vec<_>>())
        );
    }

    /// The bound survives a round trip, including values in the high bytes of
    /// the 8-byte field — a width or endianness mistake there stays invisible
    /// to small ids.
    #[test]
    fn append_only_bound_survives_a_round_trip() {
        let mut cat = SchemaCatalog::default();
        cat.mark_label_append_only("Event", 42);
        cat.mark_label_append_only("Audit", u64::MAX);

        let restored = deserialize(&serialize(&cat).unwrap()).unwrap();

        assert_eq!(restored.append_only_since("Event"), Some(42));
        assert_eq!(restored.append_only_since("Audit"), Some(u64::MAX));
    }

    #[test]
    fn schema_bin_written_by_pre_issue43_serializer_opens_without_loss() {
        let old_bytes: &[u8] = &[
            84, 71, 83, 67, 1, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 6, 0, 80, 101, 114, 115, 111, 110,
            2, 0, 105, 100, 5, 0, 65, 115, 115, 101, 116, 6, 0, 115, 116, 97, 116, 117, 115, 5, 0,
            65, 115, 115, 101, 116, 2, 0, 105, 100,
        ];

        let restored = deserialize(old_bytes).unwrap();
        assert!(restored.has_index("Person", "id"));
        assert!(restored.has_index("Asset", "status"));
        assert!(restored.has_unique_constraint("Asset", "id"));
        assert_eq!(restored.indexes().len(), 2, "no index may be lost");
        assert_eq!(restored.constraints().len(), 1, "no constraint may be lost");
        assert!(restored.append_only_labels().is_empty());
    }
}

/// Property-based tests (issue #67).
///
/// This codec already guards its own limits (`serialize` checks the entry
/// counts with `try_from`), so unlike `property_codec` and `index/codec` there
/// is no known bug here to reproduce. These tests exist to keep it that way:
/// the same invariant, generated rather than enumerated, so a future change to
/// the format has to keep satisfying it.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    /// Same lumpy distribution used by the other codecs: uniform sizes almost
    /// never land near a limit, and the limit is where this class of bug lives.
    fn size_around(limit: usize) -> impl Strategy<Value = usize> {
        prop_oneof![
            4 => 0..=limit,
            3 => limit.saturating_sub(5)..=limit + 5,
            3 => (limit + 1)..=(limit * 2).max(limit + 2),
        ]
    }

    /// Mostly short names, one in eight drawn around the `u16` limit — the mix
    /// that `index/codec` proved necessary: with short names only, a broken
    /// length check stays invisible to the everyday suite.
    fn name_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            7 => "[a-zA-Z_][a-zA-Z0-9_]{0,15}".prop_map(|s| s),
            1 => size_around(u16::MAX as usize).prop_map(|n| "N".repeat(n)),
        ]
    }

    /// Node-id boundaries for append-only declarations (issue #61). Weighted
    /// towards 0 and small values, which is what a real catalog holds, but
    /// reaching `u64::MAX` so the full 8-byte field is exercised — a generator
    /// capped at small ids would encode zeros in the high bytes and never
    /// notice a width or endianness mistake there.
    fn since_strategy() -> impl Strategy<Value = u64> {
        prop_oneof![
            2 => Just(0u64),
            4 => 0u64..1_000,
            2 => (u64::MAX - 1_000)..=u64::MAX,
            1 => any::<u64>(),
        ]
    }

    /// Covers all three parts of the catalog, including the append-only block
    /// (issue #43), which the format writes only when non-empty — a generator
    /// that skipped it would leave that optional section untested.
    fn catalog_strategy() -> impl Strategy<Value = SchemaCatalog> {
        (
            proptest::collection::vec((name_strategy(), name_strategy()), 0..6),
            proptest::collection::vec((name_strategy(), name_strategy()), 0..6),
            proptest::collection::vec((name_strategy(), since_strategy()), 0..4),
        )
            .prop_map(|(indexes, constraints, append_only)| {
                let mut cat = SchemaCatalog::new();
                for (label, prop) in indexes {
                    cat.add_index(&label, &prop);
                }
                for (label, prop) in constraints {
                    cat.add_unique_constraint(&label, &prop);
                }
                for (label, since) in append_only {
                    cat.mark_label_append_only(&label, since);
                }
                cat
            })
    }

    /// `SchemaCatalog` has no `PartialEq`, and its contents are what the
    /// format has to preserve anyway.
    ///
    /// The append-only part carries `(label, since_node_id)` rather than the
    /// label alone: `AppendOnlyDecl` compares by label, so comparing the decls
    /// themselves would pass while the boundary was dropped or corrupted in
    /// transit (issue #61).
    type Contents = (
        Vec<(String, String)>,
        Vec<(String, String)>,
        Vec<(String, u64)>,
    );

    fn contents(cat: &SchemaCatalog) -> Contents {
        let mut idx: Vec<(String, String)> = cat
            .indexes()
            .iter()
            .map(|d| (d.label.clone(), d.prop.clone()))
            .collect();
        let mut con: Vec<(String, String)> = cat
            .constraints()
            .iter()
            .map(|d| (d.label.clone(), d.prop.clone()))
            .collect();
        let mut ao: Vec<(String, u64)> = cat
            .append_only_labels()
            .iter()
            .map(|d| (d.label.clone(), d.since_node_id))
            .collect();
        idx.sort();
        con.sort();
        ao.sort();
        (idx, con, ao)
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// Accept or reject, but never lie.
        #[test]
        fn roundtrip_matches_or_serialize_rejects_but_never_corrupts(
            catalog in catalog_strategy()
        ) {
            let Ok(bytes) = serialize(&catalog) else {
                return Ok(()); // refusing is a valid answer
            };

            // An empty catalog serialises to zero bytes by design: the file is
            // not written at all in that case, and both call sites check
            // `is_empty()` before writing (graph.rs:1494, graph.rs:3093). So
            // "no bytes" means "nothing to persist", not "a document that
            // should parse" — feeding it back to `deserialize` would be
            // testing a call production code never makes.
            if bytes.is_empty() {
                prop_assert_eq!(
                    contents(&catalog),
                    contents(&SchemaCatalog::new()),
                    "only an empty catalog may serialise to no bytes"
                );
                return Ok(());
            }

            let restored = deserialize(&bytes)
                .expect("what serialize accepted, deserialize must read");

            prop_assert_eq!(contents(&restored), contents(&catalog));
        }

        /// The schema file is read at startup, so a panic on corrupt bytes
        /// means a database that will not open rather than one reporting a
        /// bad catalog.
        #[test]
        fn deserialize_never_panics_on_arbitrary_bytes(
            garbage in proptest::collection::vec(any::<u8>(), 0..1024)
        ) {
            let outcome = deserialize(&garbage);
            prop_assert!(outcome.is_ok() || outcome.is_err());
        }
    }
}
