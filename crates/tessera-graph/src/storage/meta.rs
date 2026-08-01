// SPDX-License-Identifier: Apache-2.0

use crate::Error;
use crate::error::Result;
use crate::storage::backend::{ALL_DATA_FILES, DataFile};
use crate::storage::page::{
    PAGE_HEADER_SIZE, PAGE_SIZE, PageBuf, PageType, compute_crc32, finalize_page, magic,
    new_page_buf,
};

/// Current format version for the meta page.
/// Used by `serialize`/`deserialize`, consumed by `FileBackend` (Step 12).
///
/// History:
/// - `1`: initial on-disk format.
/// - `2`: issue #54. The node slot carries an adjacency pointer (`adj_page_id` /
///   `adj_flags`), which shifted every field after `label_inline` and shrank
///   `label_inline` from 63 to 47 bytes; adjacency also gained the shared-slab page
///   type alongside the existing dedicated chains. A version-1 store is rejected on
///   open rather than converted: its node slots use the old offsets, so reading them
///   with the current layout would silently yield wrong labels and properties.
/// - `3`: issue #75. Property values record their byte length in a `u32`
///   instead of a `u16` (`property_codec.rs`), removing the 65,535-byte cap on
///   a single string value. The string heap (`string_codec.rs`) and the label
///   index (`index.bin`, its own `INDEX_VERSION`) received the same width for
///   consistency. A version-2 store is rejected on open rather than
///   reinterpreted: its property blobs use the old 2-byte length prefix, so
///   reading them with the 4-byte prefix would desynchronize every property
///   after the first string.
/// - `4`: free-page directory. Each data file records the head of a chain of
///   directory pages listing its reusable page ids, plus how many there are.
///   Before this, `allocate_page` was a counter that only ever increased: an
///   entity whose properties overflowed took a whole page, updating it
///   abandoned that page, and deleting it freed nothing. A version-3 store is
///   rejected rather than adopted, because its files may already hold orphaned
///   pages that this build cannot distinguish from live ones — handing them out
///   as free would overwrite data that a live slot still points at.
///   Each file also holds one free page directly in metadata, so that releasing
///   and immediately reallocating a single page costs no directory page at all.
const META_VERSION: u16 = 4;

/// Sentinel for "this file has no free-directory pages".
///
/// Page 0 is a legitimate page id in every data file, so the empty marker has
/// to be a value no real page can take.
pub const FREE_DIRECTORY_EMPTY: u32 = u32::MAX;

/// Sentinel for "no page is held in the single-page free slot".
///
/// Same reasoning as [`FREE_DIRECTORY_EMPTY`]: page 0 is a real page id.
pub const FREE_SPARE_EMPTY: u32 = u32::MAX;

/// Payload offset of the per-file free-directory heads, one `u32` each.
/// Named once so the writer and the reader cannot drift apart.
const FREE_HEADS_OFFSET: usize = 64;

/// Payload offset of the per-file free-page counts, one `u32` each.
const FREE_COUNTS_OFFSET: usize = FREE_HEADS_OFFSET + ALL_DATA_FILES.len() * 4;

/// Payload offset of the per-file single-page free slots, one `u32` each.
const FREE_SPARE_OFFSET: usize = FREE_COUNTS_OFFSET + ALL_DATA_FILES.len() * 4;

/// Metadata stored in `graph.meta`. Tracks counters and page counts.
///
/// Serialized as a single 4KB page with magic `TGMD`, CRC32 integrity,
/// and a dirty flag for future WAL recovery (Phase 3).
#[derive(Debug, Clone)]
pub struct GraphMeta {
    pub next_node_id: u64,
    pub next_edge_id: u64,
    pub node_count: u64,
    pub edge_count: u64,
    pub nodes_page_count: u32,
    pub edges_page_count: u32,
    pub adj_page_count: u32,
    pub strings_page_count: u32,
    pub overflow_page_count: u32,
    /// Reserved for Phase 2 index pages. Always 0 in Phase 1.
    pub index_page_count: u32,
    /// Bit 0: dirty flag. Set on open, cleared on clean close.
    pub flags: u16,
    /// Byte offset for the next string heap append.
    /// Enables reopening without scanning all string pages.
    pub strings_write_offset: u32,
    /// Per-file head of the free-page directory chain, indexed by
    /// [`DataFile::index`]. [`FREE_DIRECTORY_EMPTY`] means no free pages.
    ///
    /// Private so the index arithmetic lives in one place: a caller indexing
    /// this directly with an enum discriminant would silently read another
    /// file's slot. Use [`GraphMeta::free_directory_head`].
    free_directory_heads: [u32; ALL_DATA_FILES.len()],
    /// Per-file count of pages currently on the free chain, indexed by
    /// [`DataFile::index`].
    ///
    /// Tracked alongside the head rather than derived from it so that asking
    /// "how much of this file is reusable" costs nothing. Walking the chain
    /// would mean reading every directory page — the exact I/O this whole
    /// mechanism exists to avoid.
    free_page_counts: [u32; ALL_DATA_FILES.len()],
    /// One free page per file held directly in metadata, indexed by
    /// [`DataFile::index`]. [`FREE_SPARE_EMPTY`] means the slot is empty.
    ///
    /// Exists because the directory has to live *somewhere*, and the only
    /// pages available to hold it are the free ones. Without this slot the
    /// first release of a file spends the page it was given on an empty
    /// directory, so freeing one page and immediately reallocating still grows
    /// the file — the exact behaviour the fix is meant to remove, and the most
    /// common shape there is (one entity, repeatedly rewritten). Holding a
    /// single id here makes that case cost nothing, and the directory is only
    /// built once a second page needs recording.
    free_spare_pages: [u32; ALL_DATA_FILES.len()],
}

impl GraphMeta {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next_node_id: 1,
            next_edge_id: 1,
            node_count: 0,
            edge_count: 0,
            nodes_page_count: 0,
            edges_page_count: 0,
            adj_page_count: 0,
            strings_page_count: 0,
            overflow_page_count: 0,
            index_page_count: 0,
            flags: 0,
            strings_write_offset: 0,
            free_directory_heads: [FREE_DIRECTORY_EMPTY; ALL_DATA_FILES.len()],
            free_page_counts: [0; ALL_DATA_FILES.len()],
            free_spare_pages: [FREE_SPARE_EMPTY; ALL_DATA_FILES.len()],
        }
    }

    /// The single free page held for `file`, or [`FREE_SPARE_EMPTY`].
    #[must_use]
    pub const fn free_spare_page(&self, file: DataFile) -> u32 {
        self.free_spare_pages[file.index()]
    }

    /// Sets the single free page held for `file`.
    pub const fn set_free_spare_page(&mut self, file: DataFile, page_id: u32) {
        self.free_spare_pages[file.index()] = page_id;
    }

    /// Head of `file`'s free-page directory chain, or [`FREE_DIRECTORY_EMPTY`]
    /// when the file has no reusable pages recorded.
    #[must_use]
    pub const fn free_directory_head(&self, file: DataFile) -> u32 {
        self.free_directory_heads[file.index()]
    }

    /// Records the head of `file`'s free-page directory chain.
    pub const fn set_free_directory_head(&mut self, file: DataFile, head: u32) {
        self.free_directory_heads[file.index()] = head;
    }

    /// How many pages of `file` are currently reusable.
    #[must_use]
    pub const fn free_page_count(&self, file: DataFile) -> u32 {
        self.free_page_counts[file.index()]
    }

    /// Records how many pages of `file` are currently reusable.
    pub const fn set_free_page_count(&mut self, file: DataFile, count: u32) {
        self.free_page_counts[file.index()] = count;
    }

    /// Returns `true` if the dirty flag (bit 0) is set.
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.flags & 1 != 0
    }

    /// Sets the dirty flag (bit 0).
    pub const fn set_dirty(&mut self) {
        self.flags |= 1;
    }

    /// Clears the dirty flag (bit 0).
    pub const fn clear_dirty(&mut self) {
        self.flags &= !1;
    }

    /// Serializes this `GraphMeta` into a 4KB page buffer.
    ///
    /// Layout (all little-endian):
    /// - `[0..16]`: page header (magic TGMD, version 1, CRC32, `page_type`=0, `slot_count`=0)
    /// - `[16..24]`: `next_node_id`
    /// - `[24..32]`: `next_edge_id`
    /// - `[32..40]`: `node_count`
    /// - `[40..48]`: `edge_count`
    /// - `[48..52]`: `nodes_page_count`
    /// - `[52..56]`: `edges_page_count`
    /// - `[56..60]`: `adj_page_count`
    /// - `[60..64]`: `strings_page_count`
    /// - `[64..68]`: `overflow_page_count`
    /// - `[68..72]`: `index_page_count` (reserved Phase 2)
    /// - `[72..74]`: flags
    /// - `[74..76]`: reserved (0)
    /// - `[76..80]`: `strings_write_offset`
    /// - `[80..100]`: `free_directory_heads`, one u32 per data file in
    ///   [`ALL_DATA_FILES`] order
    /// - `[100..120]`: `free_page_counts`, same order
    /// - `[120..140]`: `free_spare_pages`, same order
    /// - `[140..4096]`: padding (0)
    #[must_use]
    pub fn serialize(&self) -> PageBuf {
        let mut buf = new_page_buf();

        // Write payload fields into [16..76]
        let p = PAGE_HEADER_SIZE;
        buf[p..p + 8].copy_from_slice(&self.next_node_id.to_le_bytes());
        buf[p + 8..p + 16].copy_from_slice(&self.next_edge_id.to_le_bytes());
        buf[p + 16..p + 24].copy_from_slice(&self.node_count.to_le_bytes());
        buf[p + 24..p + 32].copy_from_slice(&self.edge_count.to_le_bytes());
        buf[p + 32..p + 36].copy_from_slice(&self.nodes_page_count.to_le_bytes());
        buf[p + 36..p + 40].copy_from_slice(&self.edges_page_count.to_le_bytes());
        buf[p + 40..p + 44].copy_from_slice(&self.adj_page_count.to_le_bytes());
        buf[p + 44..p + 48].copy_from_slice(&self.strings_page_count.to_le_bytes());
        buf[p + 48..p + 52].copy_from_slice(&self.overflow_page_count.to_le_bytes());
        buf[p + 52..p + 56].copy_from_slice(&self.index_page_count.to_le_bytes());
        buf[p + 56..p + 58].copy_from_slice(&self.flags.to_le_bytes());
        // [p+58..p+60] = reserved, already zero
        buf[p + 60..p + 64].copy_from_slice(&self.strings_write_offset.to_le_bytes());

        // Per-file free-page directory state. Written by array position, which
        // is `DataFile::index`; see ALL_DATA_FILES for why that order is fixed.
        for (i, head) in self.free_directory_heads.iter().enumerate() {
            let off = p + FREE_HEADS_OFFSET + i * 4;
            buf[off..off + 4].copy_from_slice(&head.to_le_bytes());
        }
        for (i, count) in self.free_page_counts.iter().enumerate() {
            let off = p + FREE_COUNTS_OFFSET + i * 4;
            buf[off..off + 4].copy_from_slice(&count.to_le_bytes());
        }
        for (i, spare) in self.free_spare_pages.iter().enumerate() {
            let off = p + FREE_SPARE_OFFSET + i * 4;
            buf[off..off + 4].copy_from_slice(&spare.to_le_bytes());
        }

        // Finalize header with magic, version, CRC
        finalize_page(&mut buf, magic::META, META_VERSION, PageType::Meta, 0);

        buf
    }

    /// Deserializes a `GraphMeta` from a 4KB page buffer.
    ///
    /// Validates magic bytes, format version, and CRC32 integrity.
    pub fn deserialize(buf: &[u8; PAGE_SIZE]) -> Result<Self> {
        // Validate magic
        if buf[0..4] != magic::META {
            return Err(Error::InvalidMagic("graph.meta"));
        }

        // Validate version
        let version = u16::from_le_bytes([buf[4], buf[5]]);
        if version != META_VERSION {
            return Err(Error::IncompatibleVersion {
                found: version,
                expected: META_VERSION,
            });
        }

        // Validate CRC
        let stored_crc = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let computed_crc = compute_crc32(buf);
        if stored_crc != computed_crc {
            return Err(Error::ChecksumMismatch {
                file: "graph.meta",
                page_id: 0,
                expected: stored_crc,
                actual: computed_crc,
            });
        }

        // Read payload fields from [16..76]
        let p = PAGE_HEADER_SIZE;

        let next_node_id = u64::from_le_bytes(buf[p..p + 8].try_into().expect("8 bytes"));
        let next_edge_id = u64::from_le_bytes(buf[p + 8..p + 16].try_into().expect("8 bytes"));
        let node_count = u64::from_le_bytes(buf[p + 16..p + 24].try_into().expect("8 bytes"));
        let edge_count = u64::from_le_bytes(buf[p + 24..p + 32].try_into().expect("8 bytes"));
        let nodes_page_count = u32::from_le_bytes(buf[p + 32..p + 36].try_into().expect("4 bytes"));
        let edges_page_count = u32::from_le_bytes(buf[p + 36..p + 40].try_into().expect("4 bytes"));
        let adj_page_count = u32::from_le_bytes(buf[p + 40..p + 44].try_into().expect("4 bytes"));
        let strings_page_count =
            u32::from_le_bytes(buf[p + 44..p + 48].try_into().expect("4 bytes"));
        let overflow_page_count =
            u32::from_le_bytes(buf[p + 48..p + 52].try_into().expect("4 bytes"));
        let index_page_count = u32::from_le_bytes(buf[p + 52..p + 56].try_into().expect("4 bytes"));
        let flags = u16::from_le_bytes(buf[p + 56..p + 58].try_into().expect("2 bytes"));
        let strings_write_offset =
            u32::from_le_bytes(buf[p + 60..p + 64].try_into().expect("4 bytes"));

        let mut free_directory_heads = [FREE_DIRECTORY_EMPTY; ALL_DATA_FILES.len()];
        for (i, head) in free_directory_heads.iter_mut().enumerate() {
            let off = p + FREE_HEADS_OFFSET + i * 4;
            *head = u32::from_le_bytes(buf[off..off + 4].try_into().expect("4 bytes"));
        }
        let mut free_page_counts = [0u32; ALL_DATA_FILES.len()];
        for (i, count) in free_page_counts.iter_mut().enumerate() {
            let off = p + FREE_COUNTS_OFFSET + i * 4;
            *count = u32::from_le_bytes(buf[off..off + 4].try_into().expect("4 bytes"));
        }

        let mut free_spare_pages = [FREE_SPARE_EMPTY; ALL_DATA_FILES.len()];
        for (i, spare) in free_spare_pages.iter_mut().enumerate() {
            let off = p + FREE_SPARE_OFFSET + i * 4;
            *spare = u32::from_le_bytes(buf[off..off + 4].try_into().expect("4 bytes"));
        }

        Ok(Self {
            next_node_id,
            next_edge_id,
            node_count,
            edge_count,
            nodes_page_count,
            edges_page_count,
            adj_page_count,
            strings_page_count,
            overflow_page_count,
            index_page_count,
            flags,
            strings_write_offset,
            free_directory_heads,
            free_page_counts,
            free_spare_pages,
        })
    }
}

impl Default for GraphMeta {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_default() {
        let meta = GraphMeta::new();
        let buf = meta.serialize();
        let restored = GraphMeta::deserialize(&buf).unwrap();

        assert_eq!(restored.next_node_id, 1);
        assert_eq!(restored.next_edge_id, 1);
        assert_eq!(restored.node_count, 0);
        assert_eq!(restored.edge_count, 0);
        assert_eq!(restored.nodes_page_count, 0);
        assert_eq!(restored.edges_page_count, 0);
        assert_eq!(restored.adj_page_count, 0);
        assert_eq!(restored.strings_page_count, 0);
        assert_eq!(restored.overflow_page_count, 0);
        assert_eq!(restored.index_page_count, 0);
        assert_eq!(restored.flags, 0);
    }

    #[test]
    fn roundtrip_populated() {
        let meta = GraphMeta {
            next_node_id: 42_000,
            next_edge_id: 99_000,
            node_count: 41_999,
            edge_count: 98_999,
            nodes_page_count: 1355,
            edges_page_count: 3194,
            adj_page_count: 500,
            strings_page_count: 200,
            overflow_page_count: 10,
            index_page_count: 0,
            flags: 0,
            strings_write_offset: 0,
            ..GraphMeta::new()
        };
        let buf = meta.serialize();
        let restored = GraphMeta::deserialize(&buf).unwrap();

        assert_eq!(restored.next_node_id, 42_000);
        assert_eq!(restored.next_edge_id, 99_000);
        assert_eq!(restored.node_count, 41_999);
        assert_eq!(restored.edge_count, 98_999);
        assert_eq!(restored.nodes_page_count, 1355);
        assert_eq!(restored.edges_page_count, 3194);
        assert_eq!(restored.adj_page_count, 500);
        assert_eq!(restored.strings_page_count, 200);
        assert_eq!(restored.overflow_page_count, 10);
        assert_eq!(restored.index_page_count, 0);
        assert_eq!(restored.flags, 0);
    }

    #[test]
    fn roundtrip_with_dirty_flag() {
        let mut meta = GraphMeta::new();
        meta.set_dirty();
        assert!(meta.is_dirty());

        let buf = meta.serialize();
        let restored = GraphMeta::deserialize(&buf).unwrap();
        assert!(restored.is_dirty());
        assert_eq!(restored.flags, 1);
    }

    #[test]
    fn clear_dirty_flag() {
        let mut meta = GraphMeta::new();
        meta.set_dirty();
        meta.clear_dirty();
        assert!(!meta.is_dirty());
        assert_eq!(meta.flags, 0);
    }

    #[test]
    fn dirty_flag_preserves_other_bits() {
        let mut meta = GraphMeta::new();
        meta.flags = 0b1010; // some future flags
        meta.set_dirty();
        assert_eq!(meta.flags, 0b1011);
        meta.clear_dirty();
        assert_eq!(meta.flags, 0b1010);
    }

    #[test]
    fn magic_bytes_correct() {
        let meta = GraphMeta::new();
        let buf = meta.serialize();
        assert_eq!(&buf[0..4], &magic::META);
    }

    #[test]
    fn serialized_version_matches_current_format() {
        let meta = GraphMeta::new();
        let buf = meta.serialize();
        let version = u16::from_le_bytes([buf[4], buf[5]]);
        assert_eq!(version, META_VERSION);
    }

    #[test]
    fn crc32_validates() {
        let meta = GraphMeta::new();
        let buf = meta.serialize();
        let stored_crc = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let computed_crc = compute_crc32(&buf);
        assert_eq!(stored_crc, computed_crc);
    }

    #[test]
    fn corrupted_payload_fails_crc() {
        let meta = GraphMeta::new();
        let mut buf = meta.serialize();
        buf[20] ^= 0xFF; // flip a payload byte
        let err = GraphMeta::deserialize(&buf).unwrap_err();
        assert!(matches!(err, Error::ChecksumMismatch { .. }));
    }

    #[test]
    fn wrong_magic_fails() {
        let meta = GraphMeta::new();
        let mut buf = meta.serialize();
        buf[0] = 0x00; // corrupt magic
        let err = GraphMeta::deserialize(&buf).unwrap_err();
        assert!(matches!(err, Error::InvalidMagic("graph.meta")));
    }

    #[test]
    fn wrong_version_fails() {
        let meta = GraphMeta::new();
        let mut buf = meta.serialize();
        // Write version=99, then recompute CRC so it's not a CRC error
        buf[4..6].copy_from_slice(&99_u16.to_le_bytes());
        let crc = compute_crc32(&buf);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());
        let err = GraphMeta::deserialize(&buf).unwrap_err();
        assert!(matches!(
            err,
            Error::IncompatibleVersion {
                found: 99,
                expected: META_VERSION
            }
        ));
    }

    /// The meta format version in use before the issue #54 migration (node slot with
    /// adjacency pointer + shared slab). A store written by that build must be rejected,
    /// never reinterpreted: its node slots use the pre-#54 offsets, so reading them with
    /// the current layout would silently yield wrong labels and properties.
    const PRE_SLAB_META_VERSION: u16 = 1;

    #[test]
    fn deserialize_rejects_pre_slab_format_version_cleanly() {
        let meta = GraphMeta::new();
        let mut buf = meta.serialize();
        // Forge a meta page carrying the pre-#54 version, with a valid CRC so the
        // failure is attributable to the version check alone and not to corruption.
        buf[4..6].copy_from_slice(&PRE_SLAB_META_VERSION.to_le_bytes());
        let crc = compute_crc32(&buf);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());

        let err = GraphMeta::deserialize(&buf).unwrap_err();

        assert!(
            matches!(
                err,
                Error::IncompatibleVersion {
                    found: PRE_SLAB_META_VERSION,
                    expected: META_VERSION,
                }
            ),
            "expected a clean IncompatibleVersion rejection, got {err:?}"
        );
        // Compile-time: the #54 format change must bump META_VERSION past the old
        // one, otherwise a pre-#54 store opens and its node slots are misread silently.
        const {
            assert!(META_VERSION > PRE_SLAB_META_VERSION);
        }
    }

    /// The meta format version in use before the issue #75 migration (property
    /// value length prefix widened from u16 to u32). A store written by that
    /// build must be rejected, never reinterpreted: its property blobs record
    /// value lengths in 2 bytes, so reading them with the 4-byte prefix would
    /// desynchronize every property after the first string.
    const PRE_U32_VALUE_META_VERSION: u16 = 2;

    #[test]
    fn deserialize_rejects_pre_u32_value_format_version_cleanly() {
        let meta = GraphMeta::new();
        let mut buf = meta.serialize();
        // Forge a meta page carrying the pre-#75 version, with a valid CRC so the
        // failure is attributable to the version check alone and not to corruption.
        buf[4..6].copy_from_slice(&PRE_U32_VALUE_META_VERSION.to_le_bytes());
        let crc = compute_crc32(&buf);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());

        let err = GraphMeta::deserialize(&buf).unwrap_err();

        assert!(
            matches!(
                err,
                Error::IncompatibleVersion {
                    found: PRE_U32_VALUE_META_VERSION,
                    expected: META_VERSION,
                }
            ),
            "expected a clean IncompatibleVersion rejection, got {err:?}"
        );
        // Compile-time: the #75 format change must bump META_VERSION past the old
        // one, otherwise a pre-#75 store opens and its property blobs are misread
        // silently.
        const {
            assert!(META_VERSION > PRE_U32_VALUE_META_VERSION);
        }
    }

    // ---- Free-page directory (property-overflow waste fix) ----------------
    //
    // Three defects motivate these: an overflowing entity takes a whole 4096-byte
    // page for as little as 39 bytes; updating it abandons the old chain; and
    // deleting it frees nothing. All three trace to the same root cause — the
    // page allocator is a counter that only ever increases, with no way to
    // record that a page has become reusable. These tests pin the metadata half
    // of the fix: the head of each file's free directory has to survive a
    // reopen, or every freed page is lost again on restart.

    /// The meta format version in use before the free-page directory. A store
    /// written by that build has no directory head recorded, and its overflow
    /// file may already contain orphaned pages that this build would otherwise
    /// hand out as if they were free.
    const PRE_FREE_DIRECTORY_META_VERSION: u16 = 3;

    #[test]
    fn deserialize_rejects_pre_free_directory_format_version_cleanly() {
        let meta = GraphMeta::new();
        let mut buf = meta.serialize();
        // Forge a meta page carrying the pre-directory version, with a valid CRC
        // so the failure is attributable to the version check alone.
        buf[4..6].copy_from_slice(&PRE_FREE_DIRECTORY_META_VERSION.to_le_bytes());
        let crc = compute_crc32(&buf);
        buf[8..12].copy_from_slice(&crc.to_le_bytes());

        let err = GraphMeta::deserialize(&buf).unwrap_err();

        assert!(
            matches!(
                err,
                Error::IncompatibleVersion {
                    found: PRE_FREE_DIRECTORY_META_VERSION,
                    expected: META_VERSION,
                }
            ),
            "expected a clean IncompatibleVersion rejection, got {err:?}"
        );
        const {
            assert!(META_VERSION > PRE_FREE_DIRECTORY_META_VERSION);
        }
    }

    #[test]
    fn free_directory_heads_default_to_empty() {
        let meta = GraphMeta::new();
        for file in ALL_DATA_FILES {
            assert_eq!(
                meta.free_directory_head(file),
                FREE_DIRECTORY_EMPTY,
                "a fresh store must start with no free pages recorded for {file:?}"
            );
        }
    }

    #[test]
    fn free_directory_heads_survive_a_roundtrip() {
        // Distinct value per file: a serializer that wrote every head to the
        // same offset, or read them back in the wrong order, would still pass
        // if they all shared one value.
        let mut meta = GraphMeta::new();
        meta.set_free_directory_head(DataFile::Nodes, 11);
        meta.set_free_directory_head(DataFile::Edges, 22);
        meta.set_free_directory_head(DataFile::Adjacency, 33);
        meta.set_free_directory_head(DataFile::Strings, 44);
        meta.set_free_directory_head(DataFile::Overflow, 55);

        let restored = GraphMeta::deserialize(&meta.serialize()).unwrap();

        assert_eq!(restored.free_directory_head(DataFile::Nodes), 11);
        assert_eq!(restored.free_directory_head(DataFile::Edges), 22);
        assert_eq!(restored.free_directory_head(DataFile::Adjacency), 33);
        assert_eq!(restored.free_directory_head(DataFile::Strings), 44);
        assert_eq!(restored.free_directory_head(DataFile::Overflow), 55);
    }

    #[test]
    fn free_page_totals_survive_a_roundtrip() {
        // The count is tracked separately from the head because reporting "how
        // much of this file is reusable" must not require walking the chain.
        let mut meta = GraphMeta::new();
        meta.set_free_page_count(DataFile::Overflow, 42_000);

        let restored = GraphMeta::deserialize(&meta.serialize()).unwrap();

        assert_eq!(restored.free_page_count(DataFile::Overflow), 42_000);
        assert_eq!(
            restored.free_page_count(DataFile::Nodes),
            0,
            "a count set on one file must not bleed into another"
        );
    }

    #[test]
    fn max_values_roundtrip() {
        let meta = GraphMeta {
            next_node_id: u64::MAX,
            next_edge_id: u64::MAX,
            node_count: u64::MAX,
            edge_count: u64::MAX,
            nodes_page_count: u32::MAX,
            edges_page_count: u32::MAX,
            adj_page_count: u32::MAX,
            strings_page_count: u32::MAX,
            overflow_page_count: u32::MAX,
            index_page_count: u32::MAX,
            flags: u16::MAX,
            strings_write_offset: u32::MAX,
            ..GraphMeta::new()
        };
        let buf = meta.serialize();
        let restored = GraphMeta::deserialize(&buf).unwrap();

        assert_eq!(restored.next_node_id, u64::MAX);
        assert_eq!(restored.next_edge_id, u64::MAX);
        assert_eq!(restored.node_count, u64::MAX);
        assert_eq!(restored.edge_count, u64::MAX);
        assert_eq!(restored.nodes_page_count, u32::MAX);
        assert_eq!(restored.edges_page_count, u32::MAX);
        assert_eq!(restored.adj_page_count, u32::MAX);
        assert_eq!(restored.strings_page_count, u32::MAX);
        assert_eq!(restored.overflow_page_count, u32::MAX);
        assert_eq!(restored.index_page_count, u32::MAX);
        assert_eq!(restored.flags, u16::MAX);
        assert_eq!(restored.strings_write_offset, u32::MAX);
    }

    /// The free-directory slots at their widest. Kept separate from
    /// `max_values_roundtrip` because those fields are private and cannot be
    /// set in a struct literal; without this, the largest value they ever see
    /// in a test would be the two-digit numbers used elsewhere.
    #[test]
    fn free_directory_max_values_roundtrip() {
        let mut meta = GraphMeta::new();
        for file in ALL_DATA_FILES {
            // u32::MAX doubles as FREE_DIRECTORY_EMPTY for the head, so the
            // largest *distinguishable* head is one below it.
            meta.set_free_directory_head(file, u32::MAX - 1);
            meta.set_free_page_count(file, u32::MAX);
        }

        let restored = GraphMeta::deserialize(&meta.serialize()).unwrap();

        for file in ALL_DATA_FILES {
            assert_eq!(restored.free_directory_head(file), u32::MAX - 1, "{file:?}");
            assert_eq!(restored.free_page_count(file), u32::MAX, "{file:?}");
        }
    }

    #[test]
    fn padding_is_zeroed() {
        let meta = GraphMeta {
            next_node_id: u64::MAX,
            next_edge_id: u64::MAX,
            node_count: u64::MAX,
            edge_count: u64::MAX,
            nodes_page_count: u32::MAX,
            edges_page_count: u32::MAX,
            adj_page_count: u32::MAX,
            strings_page_count: u32::MAX,
            overflow_page_count: u32::MAX,
            index_page_count: u32::MAX,
            flags: u16::MAX,
            strings_write_offset: u32::MAX,
            ..GraphMeta::new()
        };
        let buf = meta.serialize();
        // Bytes [74..76] are reserved (zero). Real padding now starts after the
        // per-file free-directory slots, which this test's `u32::MAX` fields do
        // NOT cover — they come from `GraphMeta::new()` via the struct update
        // below, so the heads are the empty sentinel and the counts are zero.
        let padding_start = PAGE_HEADER_SIZE + FREE_SPARE_OFFSET + ALL_DATA_FILES.len() * 4;
        assert!(
            buf[74..76].iter().all(|&b| b == 0),
            "reserved bytes not zero"
        );
        assert!(
            buf[padding_start..PAGE_SIZE].iter().all(|&b| b == 0),
            "padding not zeroed"
        );
    }

    /// The free-directory slots must occupy the bytes the layout promises, and
    /// nothing beyond them. A serializer whose offsets drifted would still pass
    /// the roundtrip tests — writer and reader would simply agree on the wrong
    /// place — but it would collide with whatever is added to the page next.
    #[test]
    fn free_directory_slots_sit_where_the_layout_says() {
        let mut meta = GraphMeta::new();
        meta.set_free_directory_head(DataFile::Overflow, 0x0102_0304);
        meta.set_free_page_count(DataFile::Overflow, 0x0A0B_0C0D);
        let buf = meta.serialize();

        let head_off = PAGE_HEADER_SIZE + FREE_HEADS_OFFSET + DataFile::Overflow.index() * 4;
        let count_off = PAGE_HEADER_SIZE + FREE_COUNTS_OFFSET + DataFile::Overflow.index() * 4;

        assert_eq!(
            u32::from_le_bytes(buf[head_off..head_off + 4].try_into().unwrap()),
            0x0102_0304
        );
        assert_eq!(
            u32::from_le_bytes(buf[count_off..count_off + 4].try_into().unwrap()),
            0x0A0B_0C0D
        );
    }

    #[test]
    fn slot_count_is_zero() {
        let meta = GraphMeta::new();
        let buf = meta.serialize();
        let slot_count = u16::from_le_bytes([buf[12], buf[13]]);
        assert_eq!(slot_count, 0);
    }
}
