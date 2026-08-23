// SPDX-License-Identifier: MIT

//! Shared property slab: several entities' overflowed properties per page.
//!
//! # Why this exists
//!
//! An entity whose encoded properties exceed the inline cap (38 bytes for a
//! node, 30 for an edge) used to take a whole 4096-byte page to itself. A
//! 39-byte property set therefore cost 4096 bytes on disk — 99.05% waste, and
//! the threshold is low enough that a node with three ordinary properties
//! crosses it. Measured over 10 000 such nodes: 39.06 MB of overflow for
//! ~390 KB of live data, an amplification of 105x.
//!
//! Reclaiming freed pages (the free-list work that precedes this) stopped the
//! file from growing without bound, but did nothing about the waste *inside*
//! each page: one entity per page is one entity per page however many times
//! the page is recycled. This module removes that by packing many entities'
//! property blobs into one page.
//!
//! This is the same treatment the adjacency format received in issue #54, and
//! deliberately mirrors [`super::adj_slab_codec`]'s page shape — the design is
//! already proven there, and two similar formats that differ gratuitously are
//! harder to reason about than two that match.
//!
//! # What still needs a chain
//!
//! A blob larger than a page cannot be packed. Those keep using the existing
//! chained format ([`super::overflow_codec`]); this module handles only blobs
//! that fit, which is the overwhelmingly common case and the one carrying all
//! the waste. A 39-byte blob is the problem; a 40 KB blob already uses its
//! pages efficiently.
//!
//! # Page layout (payload, [`PAGE_PAYLOAD_SIZE`] = 4080 bytes)
//!
//! ```text
//! [0..2)   directory_count: u16 LE — directory entries ever written,
//!          including freed ones (a free tombstones its entry rather than
//!          compacting, so live entries keep their absolute offsets).
//! [2..2 + directory_count * DIR_ENTRY_SIZE)
//!          directory entries, growing FORWARD from offset 0.
//! [free gap]
//! [PAGE_PAYLOAD_SIZE - blob_bytes_used .. PAGE_PAYLOAD_SIZE)
//!          packed blobs, growing BACKWARD from the end.
//! ```
//!
//! The two regions grow towards each other from opposite ends. That is what
//! makes a blob's recorded offset absolute and immutable: appending a
//! directory entry never moves already-written blobs, it only narrows the free
//! gap. Anchoring blobs "just after the directory" instead would invalidate
//! every existing offset the moment a second entry was added.
//!
//! ## Directory entry (`DIR_ENTRY_SIZE` = 16 bytes)
//!
//! ```text
//! entity_id: u64 (8 bytes, LE)  — the node or edge this blob belongs to
//! kind:      u8  (1 byte)       — 0 = node, 1 = edge, 0xFF = freed
//! _pad:      u8  (1 byte, reserved, always 0)
//! offset:    u16 (2 bytes, LE)  — absolute payload offset of the blob
//! len:       u32 (4 bytes, LE)  — blob length in bytes
//! ```
//!
//! `len` is a `u32` rather than the `u16` the adjacency slab uses for its
//! count. A `u16` would cap a blob at 65 535 bytes, and property values have
//! been explicitly `u32`-lengthed since issue #75 removed exactly that limit
//! one layer down; reintroducing it here would silently truncate a value that
//! the layer below now accepts.
//!
//! Node and edge ids are numbered independently, so `entity_id` alone does not
//! identify an entity — `kind` is what stops a node's blob being returned for
//! an edge with the same id.

use crate::error::Result;
use crate::storage::backend::{DataFile, PageId, StorageBackend};
use crate::storage::page::{
    PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE, PageType, finalize_page, magic, new_page_buf,
};

/// Bytes of one directory entry.
const DIR_ENTRY_SIZE: usize = 16;

/// Bytes of the `directory_count` field at the start of the payload.
const DIR_COUNT_SIZE: usize = 2;

/// Format version stamped on a slab page.
const PROP_SLAB_VERSION: u16 = 1;

/// `kind` byte marking a directory entry as freed.
///
/// Freeing tombstones the entry instead of compacting the directory, so every
/// surviving entry keeps the absolute offset already recorded for it.
const KIND_FREED: u8 = 0xFF;

/// Which entity table an id belongs to.
///
/// Node ids and edge ids are allocated independently, so the id alone is
/// ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Node = 0,
    Edge = 1,
}

impl EntityKind {
    const fn as_byte(self) -> u8 {
        self as u8
    }
}

/// Largest blob this format can hold in a page with an empty directory.
///
/// A blob larger than this must use the chained format instead.
pub const MAX_PACKED_BLOB: usize = PAGE_PAYLOAD_SIZE - DIR_COUNT_SIZE - DIR_ENTRY_SIZE;

/// One directory entry, decoded.
#[derive(Debug, Clone, Copy)]
struct DirEntry {
    entity_id: u64,
    kind: u8,
    offset: usize,
    len: usize,
}

fn read_dir_count(payload: &[u8]) -> usize {
    u16::from_le_bytes(payload[0..2].try_into().expect("2 bytes")) as usize
}

fn read_dir_entry(payload: &[u8], i: usize) -> DirEntry {
    let base = DIR_COUNT_SIZE + i * DIR_ENTRY_SIZE;
    DirEntry {
        entity_id: u64::from_le_bytes(payload[base..base + 8].try_into().expect("8 bytes")),
        kind: payload[base + 8],
        offset: u16::from_le_bytes(payload[base + 10..base + 12].try_into().expect("2 bytes"))
            as usize,
        len: u32::from_le_bytes(payload[base + 12..base + 16].try_into().expect("4 bytes"))
            as usize,
    }
}

fn write_dir_entry(payload: &mut [u8], i: usize, e: &DirEntry) {
    let base = DIR_COUNT_SIZE + i * DIR_ENTRY_SIZE;
    payload[base..base + 8].copy_from_slice(&e.entity_id.to_le_bytes());
    payload[base + 8] = e.kind;
    payload[base + 9] = 0; // reserved
    // Offsets are payload-relative and the payload is 4080 bytes, so a u16
    // cannot overflow here.
    #[allow(clippy::cast_possible_truncation)]
    let off = e.offset as u16;
    payload[base + 10..base + 12].copy_from_slice(&off.to_le_bytes());
    #[allow(clippy::cast_possible_truncation)]
    let len = e.len as u32;
    payload[base + 12..base + 16].copy_from_slice(&len.to_le_bytes());
}

/// Lowest offset any live blob occupies — the packed area's low-water mark.
fn blob_low_water(payload: &[u8]) -> usize {
    let count = read_dir_count(payload);
    let mut low = PAGE_PAYLOAD_SIZE;
    for i in 0..count {
        let e = read_dir_entry(payload, i);
        // A freed entry's bytes are still reserved: compacting them away would
        // move live blobs, invalidating offsets recorded elsewhere.
        if e.offset < low {
            low = e.offset;
        }
    }
    low
}

/// Rewrites the page keeping only live entries, reclaiming tombstoned bytes.
///
/// Safe to do here — and only here — because a blob's offset is recorded
/// nowhere outside its own page: entity slots store the page id, and the blob
/// is then found by looking the entity up in this directory. Moving bytes
/// within the page therefore invalidates nothing external. (This is exactly why
/// the directory is keyed by entity rather than the slot storing an offset.)
///
/// Not called on every write: compacting costs a full page rewrite, and until
/// the page is actually short of room the tombstoned bytes cost nothing.
fn compact(payload: &mut [u8]) {
    let count = read_dir_count(payload);

    // Collect live entries with their current bytes before anything moves.
    let mut live: Vec<(DirEntry, Vec<u8>)> = Vec::new();
    for i in 0..count {
        let e = read_dir_entry(payload, i);
        if e.kind == KIND_FREED {
            continue;
        }
        if e.offset < DIR_COUNT_SIZE || e.offset + e.len > PAGE_PAYLOAD_SIZE {
            // A corrupt entry is dropped rather than propagated: keeping it
            // would carry a bad offset into the rewritten page.
            continue;
        }
        live.push((e, payload[e.offset..e.offset + e.len].to_vec()));
    }

    // Clear the whole payload so no stale bytes survive between the regions.
    payload.fill(0);

    let mut write_at = PAGE_PAYLOAD_SIZE;
    for (i, (entry, bytes)) in live.iter().enumerate() {
        write_at -= bytes.len();
        payload[write_at..write_at + bytes.len()].copy_from_slice(bytes);
        write_dir_entry(
            payload,
            i,
            &DirEntry {
                entity_id: entry.entity_id,
                kind: entry.kind,
                offset: write_at,
                len: bytes.len(),
            },
        );
    }

    // Live count fits a u16 for the same reason the original count did.
    #[allow(clippy::cast_possible_truncation)]
    let new_count = live.len() as u16;
    payload[0..2].copy_from_slice(&new_count.to_le_bytes());
}

/// Bytes still available for one more entry plus its blob.
#[must_use]
pub fn free_space(payload: &[u8]) -> usize {
    let count = read_dir_count(payload);
    let dir_end = DIR_COUNT_SIZE + count * DIR_ENTRY_SIZE;
    blob_low_water(payload).saturating_sub(dir_end)
}

/// Whether `blob_len` bytes plus a directory entry fit in this page.
///
/// Counts space currently held by tombstoned entries as available, because
/// [`write_blob`] reclaims it when it has to. Reporting only the contiguous gap
/// would make a caller allocate a fresh page while this one still has room it
/// simply has not swept yet — which is how an entity rewritten repeatedly ends
/// up spread over pages instead of staying on one.
#[must_use]
pub fn has_room_for(payload: &[u8], blob_len: usize) -> bool {
    let needed = blob_len + DIR_ENTRY_SIZE;
    if free_space(payload) >= needed {
        return true;
    }
    free_space_after_compaction(payload) >= needed
}

/// Bytes that would be available once tombstoned entries are swept away.
fn free_space_after_compaction(payload: &[u8]) -> usize {
    let count = read_dir_count(payload);
    let mut live_entries = 0;
    let mut live_bytes = 0;
    for i in 0..count {
        let e = read_dir_entry(payload, i);
        if e.kind == KIND_FREED {
            continue;
        }
        live_entries += 1;
        live_bytes += e.len;
    }
    PAGE_PAYLOAD_SIZE
        .saturating_sub(DIR_COUNT_SIZE)
        .saturating_sub(live_entries * DIR_ENTRY_SIZE)
        .saturating_sub(live_bytes)
}

/// Stores `blob` for `(entity_id, kind)` in the slab page `page_id`.
///
/// # Errors
///
/// Returns [`Error::RecordTooLarge`](crate::Error::RecordTooLarge) if the page
/// has no room. Callers must check [`has_room_for`] first and allocate a new
/// page when it says no; erroring rather than silently spilling keeps the
/// "which page holds this entity" decision with the caller, who is the one
/// recording it in the slot.
pub fn write_blob(
    backend: &mut dyn StorageBackend,
    page_id: PageId,
    entity_id: u64,
    kind: EntityKind,
    blob: &[u8],
) -> Result<()> {
    let mut buf = backend.read_page(DataFile::Overflow, page_id)?;
    let payload = &mut buf[PAGE_HEADER_SIZE..];

    // Retire any entry this entity already has on the page, BEFORE deciding
    // whether there is room. Two reasons, both load-bearing:
    //
    // - A rewrite that left the old entry in place would have two entries for
    //   one entity, and a read stops at the first match — so it would return
    //   the STALE value while the new one sat on the page, unreachable.
    // - Retiring first also frees the old blob's bytes for this very write, so
    //   rewriting an entity in place needs no extra space.
    //
    // Tombstoning rather than editing in place keeps every other blob's offset
    // immutable, and the new value need not be the same length as the old.
    let count = read_dir_count(payload);
    for i in 0..count {
        let e = read_dir_entry(payload, i);
        if e.kind == kind.as_byte() && e.entity_id == entity_id {
            payload[DIR_COUNT_SIZE + i * DIR_ENTRY_SIZE + 8] = KIND_FREED;
        }
    }

    if free_space(payload) < blob.len() + DIR_ENTRY_SIZE {
        // Out of contiguous room, but some of what the page holds is
        // tombstoned — bytes of entities freed or rewritten earlier. Sweep
        // those up before declaring the page full: without this, repeatedly
        // rewriting one entity consumes fresh bytes every time and exhausts a
        // page that in truth holds a single live blob.
        compact(payload);
        if free_space(payload) < blob.len() + DIR_ENTRY_SIZE {
            return Err(crate::Error::RecordTooLarge { size: blob.len() });
        }
    }

    let count = read_dir_count(payload);
    let offset = blob_low_water(payload) - blob.len();

    payload[offset..offset + blob.len()].copy_from_slice(blob);
    write_dir_entry(
        payload,
        count,
        &DirEntry {
            entity_id,
            kind: kind.as_byte(),
            offset,
            len: blob.len(),
        },
    );

    // Count fits a u16: the directory cannot hold more entries than the page
    // has room for, which is far below 65 535.
    #[allow(clippy::cast_possible_truncation)]
    let new_count = (count + 1) as u16;
    payload[0..2].copy_from_slice(&new_count.to_le_bytes());

    finalize_page(
        &mut buf,
        magic::OVERFLOW,
        PROP_SLAB_VERSION,
        PageType::PropertySlab,
        0,
    );
    backend.write_page(DataFile::Overflow, page_id, &buf)
}

/// Reads the blob stored for `(entity_id, kind)`, if the page holds one.
///
/// Returns `Ok(None)` when no live entry matches — which is the honest answer
/// for a stale reference, and lets the caller treat it as "no properties"
/// rather than surfacing bytes belonging to whoever occupies that space now.
pub fn read_blob(
    backend: &dyn StorageBackend,
    page_id: PageId,
    entity_id: u64,
    kind: EntityKind,
) -> Result<Option<Vec<u8>>> {
    let buf = backend.read_page(DataFile::Overflow, page_id)?;
    let payload = &buf[PAGE_HEADER_SIZE..];

    let count = read_dir_count(payload);
    for i in 0..count {
        let e = read_dir_entry(payload, i);
        // `kind` never equals KIND_FREED for a live entry, so comparing it
        // against the requested kind already skips tombstones.
        if e.kind == kind.as_byte() && e.entity_id == entity_id {
            // Guard the slice: a corrupt offset/len must not panic, and must
            // not read another entity's bytes either.
            if e.offset < DIR_COUNT_SIZE || e.offset + e.len > PAGE_PAYLOAD_SIZE {
                return Err(crate::Error::CorruptPage {
                    file: DataFile::Overflow.file_name(),
                    page_id,
                    reason: "property slab entry points outside its page",
                });
            }
            return Ok(Some(payload[e.offset..e.offset + e.len].to_vec()));
        }
    }
    Ok(None)
}

/// Marks `(entity_id, kind)`'s blob as no longer live.
///
/// The bytes stay where they are: reclaiming them would mean moving other
/// blobs, and their offsets are recorded in entity slots this module cannot
/// reach. The space returns to use when the whole page is recycled, which the
/// free-list layer already handles once every entry is freed.
///
/// Returns whether the page still holds any live entry, so the caller can
/// release an emptied page.
pub fn free_blob(
    backend: &mut dyn StorageBackend,
    page_id: PageId,
    entity_id: u64,
    kind: EntityKind,
) -> Result<bool> {
    let mut buf = backend.read_page(DataFile::Overflow, page_id)?;
    let payload = &mut buf[PAGE_HEADER_SIZE..];

    let count = read_dir_count(payload);
    let mut any_live = false;
    let mut changed = false;

    for i in 0..count {
        let e = read_dir_entry(payload, i);
        if e.kind == KIND_FREED {
            continue;
        }
        if e.entity_id == entity_id && e.kind == kind.as_byte() {
            let base = DIR_COUNT_SIZE + i * DIR_ENTRY_SIZE;
            payload[base + 8] = KIND_FREED;
            changed = true;
        } else {
            any_live = true;
        }
    }

    if changed {
        finalize_page(
            &mut buf,
            magic::OVERFLOW,
            PROP_SLAB_VERSION,
            PageType::PropertySlab,
            0,
        );
        backend.write_page(DataFile::Overflow, page_id, &buf)?;
    }

    Ok(any_live)
}

/// Prepares a freshly allocated page to act as a property slab.
///
/// A recycled page still holds its previous occupant's bytes, so the directory
/// count has to be zeroed explicitly rather than assumed.
pub fn init_page(backend: &mut dyn StorageBackend, page_id: PageId) -> Result<()> {
    let mut buf = new_page_buf();
    finalize_page(
        &mut buf,
        magic::OVERFLOW,
        PROP_SLAB_VERSION,
        PageType::PropertySlab,
        0,
    );
    backend.write_page(DataFile::Overflow, page_id, &buf)
}

/// Whether `page_id` is a property slab page.
///
/// Overflow pages come in two shapes now — chained blobs and slabs — and a
/// reader must not interpret one as the other.
pub fn is_slab_page(backend: &dyn StorageBackend, page_id: PageId) -> Result<bool> {
    let buf = backend.read_page(DataFile::Overflow, page_id)?;
    let header = crate::storage::page::PageHeader::read_from(&buf);
    Ok(header.page_type == PageType::PropertySlab as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory::MemoryBackend;

    /// Allocates and initialises one slab page.
    fn slab_page(backend: &mut MemoryBackend) -> PageId {
        let id = backend.allocate_page(DataFile::Overflow).unwrap();
        init_page(backend, id).unwrap();
        id
    }

    #[test]
    fn a_blob_round_trips() {
        let mut b = MemoryBackend::new();
        let p = slab_page(&mut b);
        let blob = vec![0xAB; 39];

        write_blob(&mut b, p, 7, EntityKind::Node, &blob).unwrap();

        assert_eq!(read_blob(&b, p, 7, EntityKind::Node).unwrap(), Some(blob));
    }

    #[test]
    fn many_entities_share_one_page() {
        // The whole point: 39-byte blobs used to cost a page each.
        let mut b = MemoryBackend::new();
        let p = slab_page(&mut b);

        let n = 50_u64;
        for i in 0..n {
            // Content varies with the id so a mixed-up offset is visible.
            let blob = vec![u8::try_from(i % 251).unwrap(); 39];
            write_blob(&mut b, p, i, EntityKind::Node, &blob).unwrap();
        }

        assert_eq!(
            b.page_count(DataFile::Overflow),
            1,
            "50 entities of 39 bytes must fit one page"
        );
        for i in 0..n {
            let expected = vec![u8::try_from(i % 251).unwrap(); 39];
            assert_eq!(
                read_blob(&b, p, i, EntityKind::Node).unwrap(),
                Some(expected),
                "entity {i} read back the wrong bytes"
            );
        }
    }

    #[test]
    fn rewriting_an_entity_reads_back_the_new_value() {
        // Caught by an integration test, not by this module: the first version
        // appended a second directory entry without retiring the first, and
        // `read_blob` stops at the first match — so a rewritten entity read
        // back its ORIGINAL value while the new one sat on the page,
        // unreachable. No error anywhere.
        let mut b = MemoryBackend::new();
        let p = slab_page(&mut b);

        write_blob(&mut b, p, 1, EntityKind::Node, b"first-value").unwrap();
        write_blob(&mut b, p, 1, EntityKind::Node, b"second-value").unwrap();
        write_blob(&mut b, p, 1, EntityKind::Node, b"third-value").unwrap();

        assert_eq!(
            read_blob(&b, p, 1, EntityKind::Node).unwrap().as_deref(),
            Some(&b"third-value"[..]),
            "a rewrite must supersede the previous value, not shadow behind it"
        );
    }

    #[test]
    fn rewriting_does_not_disturb_the_other_entities_on_the_page() {
        let mut b = MemoryBackend::new();
        let p = slab_page(&mut b);
        write_blob(&mut b, p, 1, EntityKind::Node, b"one").unwrap();
        write_blob(&mut b, p, 2, EntityKind::Node, b"two").unwrap();
        write_blob(&mut b, p, 3, EntityKind::Node, b"three").unwrap();

        // Rewrite the middle one with a longer value, so it cannot reuse its
        // old bytes and must be placed elsewhere on the page.
        write_blob(&mut b, p, 2, EntityKind::Node, b"two-much-longer-now").unwrap();

        assert_eq!(
            read_blob(&b, p, 1, EntityKind::Node).unwrap().as_deref(),
            Some(&b"one"[..])
        );
        assert_eq!(
            read_blob(&b, p, 2, EntityKind::Node).unwrap().as_deref(),
            Some(&b"two-much-longer-now"[..])
        );
        assert_eq!(
            read_blob(&b, p, 3, EntityKind::Node).unwrap().as_deref(),
            Some(&b"three"[..])
        );
    }

    #[test]
    fn rewriting_one_entity_forever_does_not_exhaust_its_page() {
        // Each rewrite tombstones the previous bytes rather than editing in
        // place, so without reclaiming them a single entity rewritten enough
        // times would fill a page it barely occupies.
        let mut b = MemoryBackend::new();
        let p = slab_page(&mut b);

        for i in 0..500 {
            let blob = format!("value-{i:04}-{}", "z".repeat(60));
            write_blob(&mut b, p, 1, EntityKind::Node, blob.as_bytes()).unwrap();
        }

        assert_eq!(
            b.page_count(DataFile::Overflow),
            1,
            "500 rewrites of one entity must still be one page"
        );
        let expected = format!("value-0499-{}", "z".repeat(60));
        assert_eq!(
            read_blob(&b, p, 1, EntityKind::Node).unwrap().as_deref(),
            Some(expected.as_bytes())
        );
    }

    #[test]
    fn compaction_preserves_every_live_entity() {
        // Compaction moves bytes around; anything it drops or mixes up is
        // silent data loss.
        let mut b = MemoryBackend::new();
        let p = slab_page(&mut b);

        // Fill with entities, then free every other one to create gaps.
        let mut expected = Vec::new();
        for i in 0..20_u64 {
            let blob = format!("entity-{i:03}-{}", "q".repeat(80));
            write_blob(&mut b, p, i, EntityKind::Node, blob.as_bytes()).unwrap();
            expected.push(blob);
        }
        for i in (0..20_u64).step_by(2) {
            free_blob(&mut b, p, i, EntityKind::Node).unwrap();
        }

        // Rewrite the survivors enough to force compaction.
        for _ in 0..40 {
            for i in (1..20_usize).step_by(2) {
                write_blob(
                    &mut b,
                    p,
                    i as u64,
                    EntityKind::Node,
                    expected[i].as_bytes(),
                )
                .unwrap();
            }
        }

        for i in (1..20_usize).step_by(2) {
            assert_eq!(
                read_blob(&b, p, i as u64, EntityKind::Node)
                    .unwrap()
                    .as_deref(),
                Some(expected[i].as_bytes()),
                "entity {i} was lost or corrupted by compaction"
            );
        }
        for i in (0..20_usize).step_by(2) {
            assert_eq!(
                read_blob(&b, p, i as u64, EntityKind::Node).unwrap(),
                None,
                "entity {i} was freed and must not come back"
            );
        }
    }

    #[test]
    fn node_and_edge_ids_do_not_collide() {
        // Node and edge ids are numbered independently, so id alone is
        // ambiguous — without the kind byte one would shadow the other.
        let mut b = MemoryBackend::new();
        let p = slab_page(&mut b);

        write_blob(&mut b, p, 1, EntityKind::Node, b"node-one").unwrap();
        write_blob(&mut b, p, 1, EntityKind::Edge, b"edge-one").unwrap();

        assert_eq!(
            read_blob(&b, p, 1, EntityKind::Node).unwrap().as_deref(),
            Some(&b"node-one"[..])
        );
        assert_eq!(
            read_blob(&b, p, 1, EntityKind::Edge).unwrap().as_deref(),
            Some(&b"edge-one"[..])
        );
    }

    #[test]
    fn a_missing_entity_reads_as_absent_not_as_someone_elses_bytes() {
        let mut b = MemoryBackend::new();
        let p = slab_page(&mut b);
        write_blob(&mut b, p, 1, EntityKind::Node, b"mine").unwrap();

        assert_eq!(read_blob(&b, p, 999, EntityKind::Node).unwrap(), None);
    }

    #[test]
    fn freeing_one_entity_leaves_the_others_intact() {
        // Freeing tombstones a directory entry; live blobs must keep their
        // offsets, which is why nothing is compacted.
        let mut b = MemoryBackend::new();
        let p = slab_page(&mut b);
        write_blob(&mut b, p, 1, EntityKind::Node, b"first").unwrap();
        write_blob(&mut b, p, 2, EntityKind::Node, b"second").unwrap();
        write_blob(&mut b, p, 3, EntityKind::Node, b"third").unwrap();

        let still_live = free_blob(&mut b, p, 2, EntityKind::Node).unwrap();

        assert!(still_live, "two entities still hold blobs here");
        assert_eq!(read_blob(&b, p, 2, EntityKind::Node).unwrap(), None);
        assert_eq!(
            read_blob(&b, p, 1, EntityKind::Node).unwrap().as_deref(),
            Some(&b"first"[..])
        );
        assert_eq!(
            read_blob(&b, p, 3, EntityKind::Node).unwrap().as_deref(),
            Some(&b"third"[..])
        );
    }

    #[test]
    fn freeing_the_last_entity_reports_the_page_as_empty() {
        // The signal the caller needs to hand the page back to the free list.
        let mut b = MemoryBackend::new();
        let p = slab_page(&mut b);
        write_blob(&mut b, p, 1, EntityKind::Node, b"only").unwrap();

        assert!(!free_blob(&mut b, p, 1, EntityKind::Node).unwrap());
    }

    #[test]
    fn a_full_page_refuses_rather_than_overwriting() {
        // Refusing is what makes "allocate another page" the caller's
        // decision. Silently spilling would leave the slot pointing at a page
        // that no longer holds the blob.
        let mut b = MemoryBackend::new();
        let p = slab_page(&mut b);

        let mut written = 0_u64;
        loop {
            let buf = b.read_page(DataFile::Overflow, p).unwrap();
            if !has_room_for(&buf[PAGE_HEADER_SIZE..], 100) {
                break;
            }
            write_blob(&mut b, p, written, EntityKind::Node, &[0x5A; 100]).unwrap();
            written += 1;
        }

        let err = write_blob(&mut b, p, 9999, EntityKind::Node, &[0x5A; 100]);
        assert!(err.is_err(), "a full page must refuse the write");

        // Everything already stored must still be readable.
        for i in 0..written {
            assert_eq!(
                read_blob(&b, p, i, EntityKind::Node).unwrap(),
                Some(vec![0x5A; 100]),
                "entity {i} was damaged by the refused write"
            );
        }
    }

    #[test]
    fn a_recycled_page_does_not_inherit_its_previous_contents() {
        // A page off the free list still holds the old occupant's bytes;
        // without an explicit reset its directory count would be believed.
        let mut b = MemoryBackend::new();
        let p = slab_page(&mut b);
        write_blob(&mut b, p, 42, EntityKind::Node, b"stale").unwrap();

        init_page(&mut b, p).unwrap();

        assert_eq!(
            read_blob(&b, p, 42, EntityKind::Node).unwrap(),
            None,
            "a reinitialised page must not report the previous occupant"
        );
    }

    #[test]
    fn a_slab_page_is_distinguishable_from_a_chained_page() {
        // Both live in the overflow file under the same magic, so the page
        // type is the only thing preventing one being read as the other.
        let mut b = MemoryBackend::new();
        let slab = slab_page(&mut b);
        let chained =
            crate::storage::codec::overflow_codec::write_overflow(&mut b, &[0x11; 200]).unwrap();

        assert!(is_slab_page(&b, slab).unwrap());
        assert!(!is_slab_page(&b, chained).unwrap());
    }

    #[test]
    fn the_largest_packable_blob_still_fits() {
        let mut b = MemoryBackend::new();
        let p = slab_page(&mut b);
        let blob = vec![0x7E; MAX_PACKED_BLOB];

        write_blob(&mut b, p, 1, EntityKind::Node, &blob).unwrap();

        assert_eq!(read_blob(&b, p, 1, EntityKind::Node).unwrap(), Some(blob));
    }
}
