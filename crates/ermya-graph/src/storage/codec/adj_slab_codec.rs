// SPDX-License-Identifier: MIT

//! Shared adjacency slab codec (issue #54, Block A).
//!
//! A slab page packs adjacency sub-blocks for MULTIPLE `(node_id, direction)`
//! pairs into a single page, instead of dedicating a whole 4096-byte page to
//! one node's adjacency chain (today's `adjacency_codec` format, which wastes
//! ~99.8% of a page for a degree-1 node). This module implements ONLY the
//! isolated slab codec (write/read/append/overflow of sub-blocks); it does not
//! wire into `Graph::add_edge`, `resolve_adj_pointer`, or `node_codec`; that
//! integration is handled separately.
//!
//! # Page layout (payload, [`PAGE_PAYLOAD_SIZE`] = 4080 bytes)
//!
//! ```text
//! [0..2)   directory_count: u16 LE — total directory ENTRIES ever written
//!          (live + tombstoned; `free_subblock` marks an
//!          entry freed WITHOUT decrementing this count or compacting).
//! [2..2 + directory_count * DIR_ENTRY_SIZE)
//!          packed directory entries, growing FORWARD from the start of the
//!          payload as sub-blocks are added.
//! [free space]
//! [PAGE_PAYLOAD_SIZE - subblocks_bytes_used .. PAGE_PAYLOAD_SIZE)
//!          packed sub-blocks, growing BACKWARD from the end of the payload
//!          as sub-blocks are added.
//! ```
//!
//! The two regions grow towards each other from opposite ends of the
//! payload — the directory forward from offset 0, the packed sub-block area
//! backward from `PAGE_PAYLOAD_SIZE`. This decouples a sub-block's absolute
//! offset from the directory's size: adding a new directory entry never
//! moves where already-written sub-blocks live, only shrinks the free gap
//! between the two regions. (An earlier draft anchored sub-block offsets to
//! "just after the directory", which breaks the moment a second sub-block is
//! written — the directory's growth would retroactively invalidate the first
//! sub-block's relative offset. Growing from the END avoids that class of
//! bug entirely: each directory entry's `offset` is an ABSOLUTE payload
//! offset, immutable once written until the sub-block itself is freed.)
//! Free space is computed as `PAGE_PAYLOAD_SIZE - directory_bytes_used -
//! subblocks_bytes_used` (see [`slab_free_space`]). Kept intentionally
//! simple for Ciclo 1: no sorting, no compaction — directory entries are
//! appended in insertion order and sub-blocks are appended to the packed
//! area in insertion order (each new sub-block's `offset` is the current
//! low-water mark of the packed area, minus its own size).
//!
//! ## Directory entry (`DIR_ENTRY_SIZE` = 14 bytes)
//!
//! ```text
//! node_id:    u64 (8 bytes, LE)
//! direction:  u8  (1 byte; 0 = Outgoing, 1 = Incoming; 0xFF = freed/tombstone)
//! _pad:       u8  (1 byte, reserved, always 0)
//! offset:     u16 (2 bytes, LE) — ABSOLUTE byte offset (within the payload)
//!             where this sub-block's edge data starts.
//! edge_count: u16 (2 bytes, LE) — number of `u64` edge IDs currently stored.
//! ```
//!
//! The directory is the single source of truth for a sub-block's size: the
//! sub-block itself stores ONLY the raw edge IDs (no repeated header), which
//! is what makes in-place append (Ciclo 3) a matter of bumping one `u16` in
//! the directory rather than rewriting a sub-block-local header too.
//!
//! ## Sub-block (packed area)
//!
//! ```text
//! edges: [u64; edge_count] (8 bytes each, LE), tightly packed, no padding.
//! ```
//!
//! The LAST sub-block written (in insertion order) has the LOWEST absolute
//! offset — it sits closest to the directory, at the current low-water mark
//! of the packed area. This is what [`append_subblock_edges`]'s "is this the
//! last sub-block" check tests: growing it downward (toward smaller offsets)
//! is the only in-place-safe direction, since no other sub-block's bytes lie
//! between it and the free gap.
//!
//! # Design decisions not fully specified by the plan
//!
//! - **Directory entry has no embedded `node_id`+`direction` redundancy in the
//!   sub-block body**: the plan's ASCII sketch shows `node_id(8) +
//!   direction(1) + edge_count(4) + pad(3)` INSIDE each sub-block, mirroring
//!   `adjacency_codec::RECORD_HEADER_SIZE`. This codec instead keeps
//!   `node_id`/`direction`/`edge_count` only in the directory entry and makes
//!   the sub-block a pure edge array. Rationale: the directory is already the
//!   index used to locate a sub-block by `(node_id, direction)`, so a second
//!   copy of the same key inside the sub-block body would be redundant data
//!   that has to be kept in sync on every append — a source of the exact kind
//!   of silent corruption the project's error-log flags as costly. Capacity
//!   is not affected by this choice (the freed 16 bytes/sub-block just IS the
//!   16-byte header the plan describes, relocated to the directory, which
//!   already carries equivalent fields).
//! - **`AppendOutcome`/free-space accounting are pure functions over an
//!   in-memory payload slice** wherever possible, to keep the Ciclo 3/4 logic
//!   testable without a backend round-trip on every assertion.

use crate::Error;
use crate::error::Result;
use crate::storage::backend::{DataFile, PageId, StorageBackend};
use crate::storage::codec::adjacency_codec::AdjDirection;
use crate::storage::page::{
    PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE, PageHeader, PageType, finalize_page, magic, new_page_buf,
};

/// Slab page format version stamped in the page header's `version` field.
/// Distinct number space from `adjacency_codec::ADJ_FORMAT_V1`/`V2` — a slab
/// page is identified primarily by `page_type` (`PageType::AdjacencySlab`),
/// this version exists for the same forward-compatibility reason
/// `adjacency_codec` versions its own format (future slab layout changes
/// without breaking old slab pages).
const ADJ_SLAB_FORMAT_V1: u16 = 1;

/// Size of one directory entry: `node_id`(8) + `direction`(1) + `_pad`(1) +
/// `offset`(2) + `edge_count`(2) = 14 bytes.
const DIR_ENTRY_SIZE: usize = 14;

/// Size of the `directory_count` field at the start of the payload.
const DIR_COUNT_SIZE: usize = 2;

/// Size of one edge ID in a packed sub-block.
const EDGE_SIZE: usize = 8;

/// Sentinel `direction` byte marking a directory entry as freed (Ciclo 4). A
/// live entry only ever stores 0 (Outgoing) or 1 (Incoming); this value is
/// unreachable as a real direction, so a freed slot is unambiguous without
/// needing a separate "tombstone" bitmap.
const DIR_ENTRY_FREED: u8 = 0xFF;

const fn direction_to_u8(direction: AdjDirection) -> u8 {
    match direction {
        AdjDirection::Outgoing => 0,
        AdjDirection::Incoming => 1,
    }
}

const fn direction_from_u8(page_id: PageId, byte: u8) -> Result<AdjDirection> {
    match byte {
        0 => Ok(AdjDirection::Outgoing),
        1 => Ok(AdjDirection::Incoming),
        _ => Err(corrupt(page_id, "invalid slab sub-block direction")),
    }
}

const fn corrupt(page_id: PageId, reason: &'static str) -> Error {
    Error::CorruptPage {
        file: "adjacency.db",
        page_id,
        reason,
    }
}

/// One decoded directory entry, plus its index within the directory (needed
/// by callers that patch the entry in place, e.g. append/free).
#[derive(Debug, Clone, Copy)]
struct DirEntry {
    node_id: u64,
    direction: AdjDirection,
    /// ABSOLUTE byte offset (within the payload) where this sub-block's edge
    /// data starts. Immutable once written (until freed) — independent of
    /// how many directory entries exist, since the packed area grows
    /// backward from `PAGE_PAYLOAD_SIZE` (see module docs).
    offset: u16,
    edge_count: u16,
}

/// Byte offset (within the payload) marking the end of the directory area,
/// given `total_entries` LIVE + FREED entries currently occupying it. Freed
/// entries are NOT compacted out of the directory (Ciclo 4 tombstones in
/// place), so this is `DIR_COUNT_SIZE + total_entries * DIR_ENTRY_SIZE`, not
/// a function of only the live count.
///
/// `total_entries` is a raw `u16` read directly from disk, so this is plain
/// `usize` arithmetic that can never overflow on any supported platform
/// (`u16::MAX * DIR_ENTRY_SIZE` fits comfortably in a `usize`) — the actual
/// corruption risk is the result exceeding [`PAGE_PAYLOAD_SIZE`], which
/// callers must check explicitly via [`validate_dir_total_count`] before
/// trusting `total_entries` to index into the payload.
const fn directory_area_end(total_entries: u16) -> usize {
    DIR_COUNT_SIZE + total_entries as usize * DIR_ENTRY_SIZE
}

/// Reads the raw entry count (live + freed) stored at payload offset 0 and
/// validates that it describes a directory area that actually fits inside
/// [`PAGE_PAYLOAD_SIZE`]. A page corrupted on disk can carry an arbitrary
/// 2-byte value here (up to `u16::MAX` = 65535); trusting it blindly to
/// compute byte offsets is what causes out-of-bounds panics elsewhere in this
/// module (hallazgos #1/#2), so every other function in this module MUST
/// obtain `total_count` through this validated accessor rather than reading
/// bytes `[0..2]` directly.
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if `directory_area_end(count) >
/// PAGE_PAYLOAD_SIZE`.
fn read_dir_total_count(page_id: PageId, payload: &[u8]) -> Result<u16> {
    let count = u16::from_le_bytes(payload[0..2].try_into().unwrap());
    if directory_area_end(count) > PAGE_PAYLOAD_SIZE {
        return Err(corrupt(
            page_id,
            "slab directory_count exceeds page capacity",
        ));
    }
    Ok(count)
}

fn write_dir_total_count(payload: &mut [u8], count: u16) {
    payload[0..2].copy_from_slice(&count.to_le_bytes());
}

/// Validates that a single directory entry's `offset`/`edge_count` describe a
/// sub-block whose byte range `[offset, offset + edge_count * EDGE_SIZE)`
/// lies entirely within `[0, PAGE_PAYLOAD_SIZE)`. Shared by every reader that
/// is about to index `payload` using an entry's `offset`/`edge_count`
/// (hallazgos #3/#4/#7): a corrupted offset or `edge_count` must be rejected
/// here, once, rather than re-validated ad hoc at each call site.
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if the sub-block's byte range overflows or
/// exceeds `PAGE_PAYLOAD_SIZE`.
fn validate_subblock_range(page_id: PageId, offset: u16, edge_count: u16) -> Result<()> {
    let byte_len = edge_count as usize * EDGE_SIZE;
    let end = (offset as usize)
        .checked_add(byte_len)
        .ok_or_else(|| corrupt(page_id, "slab sub-block offset+len overflow"))?;
    if end > PAGE_PAYLOAD_SIZE {
        return Err(corrupt(
            page_id,
            "slab sub-block range exceeds page capacity",
        ));
    }
    Ok(())
}

/// Reads directory entry at index `idx` (0-based), regardless of whether it
/// is live or freed. Returns `Ok(None)` if `idx >= total_count` (not
/// corruption — this is the normal end-of-directory loop condition for
/// callers scanning `0..total_count`).
///
/// A freed entry (direction byte [`DIR_ENTRY_FREED`]) always has its
/// `offset`/`edge_count` validated too: [`free_subblock`] zeroes both fields,
/// so a validly-freed entry trivially passes range validation; a freed entry
/// whose offset/`edge_count` are NOT zero (impossible via this module's own
/// writers, but exactly what a corrupted disk page can contain) is corruption
/// and must be reported, not silently trusted as "stale, ignore" (hallazgo
/// #14 — the old comment describing these fields as unused for a freed slot
/// was wrong: [`subblocks_bytes_used`] reads them for every entry regardless
/// of freed status).
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if the entry's direction byte is neither a
/// valid direction nor the freed sentinel, or if its offset/`edge_count`
/// describe an out-of-bounds sub-block range.
fn read_dir_entry(
    page_id: PageId,
    payload: &[u8],
    idx: u16,
    total_count: u16,
) -> Result<Option<(u8, DirEntry)>> {
    if idx >= total_count {
        return Ok(None);
    }
    let base = DIR_COUNT_SIZE + idx as usize * DIR_ENTRY_SIZE;
    let node_id = u64::from_le_bytes(payload[base..base + 8].try_into().unwrap());
    let direction_byte = payload[base + 8];
    let offset = u16::from_le_bytes(payload[base + 10..base + 12].try_into().unwrap());
    let edge_count = u16::from_le_bytes(payload[base + 12..base + 14].try_into().unwrap());
    validate_subblock_range(page_id, offset, edge_count)?;
    if direction_byte == DIR_ENTRY_FREED {
        return Ok(Some((
            direction_byte,
            DirEntry {
                node_id,
                direction: AdjDirection::Outgoing, // placeholder, caller checks freed byte first
                offset,
                edge_count,
            },
        )));
    }
    let direction = direction_from_u8(page_id, direction_byte)?;
    Ok(Some((
        direction_byte,
        DirEntry {
            node_id,
            direction,
            offset,
            edge_count,
        },
    )))
}

fn write_dir_entry(payload: &mut [u8], idx: u16, entry: &DirEntry) {
    let base = DIR_COUNT_SIZE + idx as usize * DIR_ENTRY_SIZE;
    payload[base..base + 8].copy_from_slice(&entry.node_id.to_le_bytes());
    payload[base + 8] = direction_to_u8(entry.direction);
    payload[base + 9] = 0; // _pad
    payload[base + 10..base + 12].copy_from_slice(&entry.offset.to_le_bytes());
    payload[base + 12..base + 14].copy_from_slice(&entry.edge_count.to_le_bytes());
}

fn mark_dir_entry_freed(payload: &mut [u8], idx: u16) {
    let base = DIR_COUNT_SIZE + idx as usize * DIR_ENTRY_SIZE;
    payload[base + 8] = DIR_ENTRY_FREED;
}

/// Finds the live directory entry for `(node_id, direction)`. Returns
/// `Ok(Some((index, entry)))` if found among live (non-freed) entries,
/// `Ok(None)` if the directory contains no such live entry.
///
/// A corrupted entry encountered while scanning (hallazgo #9) is reported as
/// [`Error::CorruptPage`] immediately — the whole page is untrustworthy at
/// that point, since the directory is a flat array with no independent
/// recovery for a single bad slot — rather than silently skipped (which would
/// make an unrelated valid entry further down look like "not found" instead
/// of exposing the real problem) or propagated as a bare index-out-of-range
/// panic.
fn find_live_entry(
    page_id: PageId,
    payload: &[u8],
    node_id: u64,
    direction: AdjDirection,
) -> Result<Option<(u16, DirEntry)>> {
    let total = read_dir_total_count(page_id, payload)?;
    let want_direction = direction_to_u8(direction);
    for idx in 0..total {
        let Some((direction_byte, entry)) = read_dir_entry(page_id, payload, idx, total)? else {
            break;
        };
        if direction_byte == want_direction && entry.node_id == node_id {
            return Ok(Some((idx, entry)));
        }
    }
    Ok(None)
}

/// Total bytes currently occupied by the directory (live + freed entries).
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if the stored `directory_count` describes a
/// directory area exceeding `PAGE_PAYLOAD_SIZE`.
fn directory_bytes_used(page_id: PageId, payload: &[u8]) -> Result<usize> {
    let total = read_dir_total_count(page_id, payload)?;
    Ok(directory_area_end(total) - DIR_COUNT_SIZE)
}

/// Total bytes currently occupied by the packed sub-block area: `
/// PAGE_PAYLOAD_SIZE - low_water_mark`, where `low_water_mark` is the
/// smallest `offset` among entries that have ever reserved space there
/// (freed entries' bytes remain physically occupied — Ciclo 4 does not
/// compact — so ALL entries, live or freed, are scanned to find the true
/// low-water mark; only entries whose `offset`/`edge_count` were explicitly
/// zeroed by `free_subblock`, i.e. the LAST-written sub-block being freed,
/// stop contributing to it).
///
/// A live entry can legitimately have `edge_count == 0` (hallazgo #11: a
/// sub-block written or shrunk to zero edges is still LIVE, not freed) — so
/// this function does NOT infer "freed" from `edge_count == 0` (hallazgo
/// #10). Freed-ness is read explicitly from the direction byte returned by
/// `read_dir_entry`; only entries confirmed freed are excluded from the
/// low-water-mark scan, an empty-but-live entry still contributes its
/// `offset` (which is always a valid, in-bounds placement, occupying zero
/// bytes but not a "hole").
///
/// Returns `PAGE_PAYLOAD_SIZE` (i.e. zero bytes used) when there are no
/// entries with a non-zero footprint.
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if the directory or any entry is corrupt.
fn subblocks_bytes_used(page_id: PageId, payload: &[u8]) -> Result<usize> {
    let total = read_dir_total_count(page_id, payload)?;
    let mut low_water_mark = PAGE_PAYLOAD_SIZE;
    for idx in 0..total {
        let Some((direction_byte, entry)) = read_dir_entry(page_id, payload, idx, total)? else {
            break;
        };
        if direction_byte == DIR_ENTRY_FREED {
            continue; // freed — no footprint, regardless of stored offset/edge_count
        }
        low_water_mark = low_water_mark.min(entry.offset as usize);
    }
    Ok(PAGE_PAYLOAD_SIZE - low_water_mark)
}

/// Bytes a fresh sub-block of `edge_count` edges consumes: its directory entry
/// plus its packed edges.
const fn subblock_bytes_needed(edge_count: usize) -> usize {
    DIR_ENTRY_SIZE + edge_count * EDGE_SIZE
}

/// Whether the slab page at `page_id` can take a new sub-block of `edge_count`
/// edges.
///
/// This is the question [`write_subblock`]'s caller must answer first, since that
/// function treats "does not fit" as a corrupt-page error rather than a signal.
///
/// The directory grows forward from the payload start and the packed area backward
/// from its end, so a fit needs strictly more free space than the sub-block's own
/// size: at exactly equal, the new entry's data would start where its directory
/// entry ends and the two frontiers would touch.
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if the page's directory is corrupt.
pub fn slab_can_fit_subblock(
    page_id: PageId,
    page: &crate::storage::page::PageBuf,
    edge_count: usize,
) -> Result<bool> {
    Ok(slab_free_space(page_id, page)? > subblock_bytes_needed(edge_count))
}

/// Whether a sub-block of `edge_count` edges could fit an empty slab page at all.
///
/// `false` means no slab can ever hold it and the caller must use a dedicated
/// chain: past this size the record needs pages of its own. The budget is the
/// payload minus the `directory_count` field that every slab page carries, and
/// the fit is strict for the same reason as [`slab_can_fit_subblock`] — the
/// directory and the packed area must not meet.
#[must_use]
pub const fn fits_in_empty_slab(edge_count: usize) -> bool {
    subblock_bytes_needed(edge_count) < PAGE_PAYLOAD_SIZE - DIR_COUNT_SIZE
}

/// Free space remaining in a slab page's payload, in bytes: total payload
/// minus directory bytes used minus packed sub-block bytes used.
///
/// `page` is the full [`crate::storage::page::PageBuf`] (header + payload);
/// only the payload region is inspected.
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if the page's directory is corrupt.
fn slab_free_space(page_id: PageId, page: &crate::storage::page::PageBuf) -> Result<usize> {
    let payload = &page[PAGE_HEADER_SIZE..];
    let dir_bytes = directory_bytes_used(page_id, payload)?;
    let sub_bytes = subblocks_bytes_used(page_id, payload)?;
    // dir_bytes and sub_bytes are each individually bounded by
    // PAGE_PAYLOAD_SIZE, but a corrupt-but-individually-valid combination
    // (e.g. a directory that alone occupies the whole page, on a page with
    // no sub-blocks so sub_bytes reports the full payload as "used" too) can
    // make their SUM exceed PAGE_PAYLOAD_SIZE. checked_sub turns that into
    // CorruptPage instead of an underflow panic.
    PAGE_PAYLOAD_SIZE
        .checked_sub(dir_bytes)
        .and_then(|v| v.checked_sub(sub_bytes))
        .ok_or_else(|| {
            corrupt(
                page_id,
                "slab directory + sub-block usage exceeds page capacity",
            )
        })
}

/// Allocates and finalizes an empty slab page (zero directory entries),
/// stamping the slab's own `page_type`/`version` so a reader never confuses
/// it with a dedicated-chain adjacency page.
pub fn allocate_slab_page(backend: &mut dyn StorageBackend) -> Result<PageId> {
    let page_id = backend.allocate_page(DataFile::Adjacency)?;
    let mut buf = new_page_buf();
    finalize_slab_buf(&mut buf);
    backend.write_page(DataFile::Adjacency, page_id, &buf)?;
    Ok(page_id)
}

fn finalize_slab_buf(buf: &mut crate::storage::page::PageBuf) {
    finalize_page(
        buf,
        magic::ADJACENCY,
        ADJ_SLAB_FORMAT_V1,
        PageType::AdjacencySlab,
        0,
    );
}

/// Returns `true` if the page at `page_id` is a slab page (as opposed to a
/// dedicated adjacency chain page written by `adjacency_codec`).
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if the page cannot be read.
pub fn is_slab_page(backend: &dyn StorageBackend, page_id: PageId) -> Result<bool> {
    let buf = backend.read_page(DataFile::Adjacency, page_id)?;
    let header = PageHeader::read_from(&buf);
    Ok(header.page_type == PageType::AdjacencySlab as u16)
}

/// Outcome of [`append_subblock_edges`]: whether the append happened in place
/// on the existing page, or the caller must fall back to a different
/// strategy (Ciclo 4: migrate to a dedicated chain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    /// The new edges were appended in place; the sub-block now holds the
    /// combined edge count on the same page.
    InPlace,
    /// No contiguous room was found (either the page lacks free bytes, or the
    /// sub-block is not the last one in the packed area — see module docs on
    /// why this codec never displaces other sub-blocks' bytes). The caller
    /// must decide what to do next (Ciclo 4: migrate to a dedicated chain).
    NoRoom,
}

/// Writes a brand-new sub-block for `(node_id, direction)` into the slab page
/// at `page_id`, searching for room among any existing sub-blocks.
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if the page cannot be read/written, or if
/// there is not enough free space for the new directory entry + edge data
/// (surfaced as `Error::CorruptPage` with a descriptive reason — a full slab
/// page is an expected, not exceptional, condition once `add_edge` wiring
/// exists (Ciclo 6+); THIS codec layer's job is only to report it, the
/// caller decides to migrate/allocate a new slab page).
///
/// # Panics
///
/// Does not panic on valid input; a `(node_id, direction)` that already has a
/// live sub-block on this page is treated as a caller bug and returns
/// [`Error::CorruptPage`] rather than silently overwriting data.
pub fn write_subblock(
    backend: &mut dyn StorageBackend,
    page_id: PageId,
    node_id: u64,
    direction: AdjDirection,
    edge_ids: &[u64],
) -> Result<()> {
    let mut buf = backend.read_page(DataFile::Adjacency, page_id)?;
    let payload = &buf[PAGE_HEADER_SIZE..];

    if find_live_entry(page_id, payload, node_id, direction)?.is_some() {
        return Err(corrupt(
            page_id,
            "slab sub-block already exists for (node_id, direction)",
        ));
    }

    let needed_bytes = DIR_ENTRY_SIZE + edge_ids.len() * EDGE_SIZE;
    if slab_free_space(page_id, &buf)? < needed_bytes {
        return Err(corrupt(
            page_id,
            "slab page has insufficient free space for new sub-block",
        ));
    }

    let payload_mut = &mut buf[PAGE_HEADER_SIZE..];
    let total = read_dir_total_count(page_id, payload_mut)?;
    let sub_bytes_used = subblocks_bytes_used(page_id, payload_mut)?;
    let edge_bytes = edge_ids.len() * EDGE_SIZE;
    // The packed area grows BACKWARD from PAGE_PAYLOAD_SIZE, so this new
    // sub-block's absolute offset is the new low-water mark: current used
    // bytes plus this sub-block's own size, subtracted from the payload end.
    // Both operands were already bounds-checked above (sub_bytes_used and
    // edge_bytes together fit within PAGE_PAYLOAD_SIZE per the free-space
    // guard), but a corrupted sub_bytes_used derived from a bad directory
    // could still underflow here — checked_sub turns that into CorruptPage
    // instead of a wrapped (huge) offset that would silently corrupt the page.
    let new_abs_offset = PAGE_PAYLOAD_SIZE
        .checked_sub(sub_bytes_used)
        .and_then(|v| v.checked_sub(edge_bytes))
        .ok_or_else(|| corrupt(page_id, "slab sub-block offset underflow"))?;

    let total_plus_one = total
        .checked_add(1)
        .ok_or_else(|| corrupt(page_id, "slab directory_count overflow"))?;
    // The directory grows forward from the payload start while sub-blocks grow
    // backward from its end, so the two must not cross: the new entry has to end
    // at or before where this sub-block's data will begin. Checking it against
    // PAGE_PAYLOAD_SIZE alone would let a directory-heavy page (many small
    // sub-blocks) overrun the packed area and hand out an offset inside the
    // directory.
    if directory_area_end(total_plus_one) > new_abs_offset {
        return Err(corrupt(
            page_id,
            "slab directory has no room for a new entry",
        ));
    }

    write_dir_entry(
        payload_mut,
        total,
        &DirEntry {
            node_id,
            direction,
            offset: u16::try_from(new_abs_offset)
                .map_err(|_| corrupt(page_id, "slab sub-block offset overflow"))?,
            edge_count: u16::try_from(edge_ids.len())
                .map_err(|_| corrupt(page_id, "slab sub-block edge_count overflow"))?,
        },
    );
    write_dir_total_count(payload_mut, total_plus_one);

    // Write the packed edge data. new_abs_offset + edge_bytes <=
    // PAGE_PAYLOAD_SIZE is guaranteed by the checked_sub above (it computed
    // new_abs_offset as PAGE_PAYLOAD_SIZE - sub_bytes_used - edge_bytes with
    // sub_bytes_used >= 0), so this slice is always in-bounds.
    let mut off = new_abs_offset;
    for &eid in edge_ids {
        payload_mut[off..off + EDGE_SIZE].copy_from_slice(&eid.to_le_bytes());
        off += EDGE_SIZE;
    }

    finalize_slab_buf(&mut buf);
    backend.write_page(DataFile::Adjacency, page_id, &buf)?;
    Ok(())
}

/// Reads back the edge IDs for `(node_id, direction)` on the slab page at
/// `page_id`.
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if the page cannot be read, or if no live
/// sub-block exists for `(node_id, direction)`.
pub fn read_subblock(
    backend: &dyn StorageBackend,
    page_id: PageId,
    node_id: u64,
    direction: AdjDirection,
) -> Result<Vec<u64>> {
    let buf = backend.read_page(DataFile::Adjacency, page_id)?;
    let payload = &buf[PAGE_HEADER_SIZE..];
    let (_idx, entry) = find_live_entry(page_id, payload, node_id, direction)?
        .ok_or_else(|| corrupt(page_id, "no live slab sub-block for (node_id, direction)"))?;

    // entry.offset/edge_count were already range-validated by
    // find_live_entry -> read_dir_entry -> validate_subblock_range, so this
    // indexing can never go out of bounds even for a hostile on-disk entry.
    let mut edges = Vec::with_capacity(entry.edge_count as usize);
    let mut off = entry.offset as usize;
    for _ in 0..entry.edge_count {
        edges.push(u64::from_le_bytes(
            payload[off..off + EDGE_SIZE].try_into().unwrap(),
        ));
        off += EDGE_SIZE;
    }
    Ok(edges)
}

/// Appends `new_edge_ids` to the existing sub-block for `(node_id,
/// direction)` on the slab page at `page_id`.
///
/// Fast path only: the append happens in place ([`AppendOutcome::InPlace`])
/// only if the sub-block is the LAST one written into the packed area (i.e.
/// its `offset` equals the current low-water mark — see module docs: the
/// packed area grows backward from `PAGE_PAYLOAD_SIZE`, so the most-recently
/// written sub-block sits at the smallest offset) AND there is enough free
/// space in the page for the additional edges. In any other case this
/// returns [`AppendOutcome::NoRoom`] without modifying the page —
/// deliberately: this codec never displaces the bytes of other sub-blocks to
/// make room (see module docs), so a non-last sub-block always falls back to
/// migration (Ciclo 4) even when the page has bytes to spare elsewhere.
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if the page cannot be read/written, or if
/// no live sub-block exists for `(node_id, direction)`.
pub fn append_subblock_edges(
    backend: &mut dyn StorageBackend,
    page_id: PageId,
    node_id: u64,
    direction: AdjDirection,
    new_edge_ids: &[u64],
) -> Result<AppendOutcome> {
    let mut buf = backend.read_page(DataFile::Adjacency, page_id)?;
    let payload = &buf[PAGE_HEADER_SIZE..];
    let (idx, entry) = find_live_entry(page_id, payload, node_id, direction)?
        .ok_or_else(|| corrupt(page_id, "no live slab sub-block for (node_id, direction)"))?;

    let sub_bytes_used = subblocks_bytes_used(page_id, payload)?;
    // "Last" means: this sub-block sits exactly at the current low-water
    // mark (PAGE_PAYLOAD_SIZE - sub_bytes_used). A live entry with
    // edge_count == 0 (hallazgo #11) is a valid, in-bounds sub-block that
    // simply owns zero bytes — it can be "last" too (offset ==
    // PAGE_PAYLOAD_SIZE - sub_bytes_used trivially, since it contributes no
    // footprint of its own), and growing it in place works exactly like any
    // other last sub-block: the equality check below already handles it
    // without special-casing, since `entry.offset` for a zero-length live
    // entry still equals the low-water mark by construction of write_subblock.
    let is_last = PAGE_PAYLOAD_SIZE
        .checked_sub(sub_bytes_used)
        .is_some_and(|low_water_mark| entry.offset as usize == low_water_mark);

    if !is_last {
        return Ok(AppendOutcome::NoRoom);
    }

    let needed_bytes = new_edge_ids.len() * EDGE_SIZE;
    if slab_free_space(page_id, &buf)? < needed_bytes {
        return Ok(AppendOutcome::NoRoom);
    }

    let old_edge_count = entry.edge_count as usize;
    let old_bytes = old_edge_count * EDGE_SIZE;
    let new_edge_count = old_edge_count + new_edge_ids.len();
    let new_edge_count_u16 = u16::try_from(new_edge_count)
        .map_err(|_| corrupt(page_id, "slab sub-block edge_count overflow"))?;
    // entry.offset is validated (>= 0, <= PAGE_PAYLOAD_SIZE) but a corrupted
    // directory could still make needed_bytes exceed it — checked_sub turns
    // that into CorruptPage instead of a wrapped (huge) usize that would then
    // pass the u16::try_from as some unrelated small value and corrupt the page.
    let new_offset = (entry.offset as usize)
        .checked_sub(needed_bytes)
        .ok_or_else(|| corrupt(page_id, "slab sub-block offset underflow on append"))?;
    let new_offset_u16 = u16::try_from(new_offset)
        .map_err(|_| corrupt(page_id, "slab sub-block offset overflow"))?;

    // Growing moves this sub-block's data LEFT, toward the directory growing right
    // from the payload start. `slab_free_space` reports the gap between the two
    // frontiers, but says nothing about which side may claim it — so a sub-block
    // whose growth would cross the directory's end must stop here and let the
    // caller migrate it out instead. Without this, the shifted edges land on top of
    // directory bytes: the first edge ID silently loses its high bytes, and the
    // corruption surfaces as a wrong (small) edge ID on read, not as an error.
    let total = read_dir_total_count(page_id, &buf[PAGE_HEADER_SIZE..])?;
    if directory_area_end(total) > new_offset {
        return Ok(AppendOutcome::NoRoom);
    }

    // Validate the COPY SOURCE range before touching the page: entry.offset
    // was already range-checked by read_dir_entry for [offset, offset +
    // edge_count*8), which is exactly [entry.offset, entry.offset +
    // old_bytes) — so this is redundant defense-in-depth (copy_within would
    // otherwise panic on an out-of-bounds source range) rather than a new
    // corruption class, kept explicit so this function's safety does not rely
    // on read_dir_entry's invariant alone.
    let old_range_end = entry
        .offset
        .checked_add(u16::try_from(old_bytes).unwrap_or(u16::MAX))
        .map_or(usize::MAX, |v| v as usize);
    if old_range_end > PAGE_PAYLOAD_SIZE {
        return Err(corrupt(
            page_id,
            "slab sub-block copy source range exceeds page capacity",
        ));
    }

    let payload_mut = &mut buf[PAGE_HEADER_SIZE..];

    // The free gap sits at SMALLER offsets than this sub-block (packed area
    // grows backward from PAGE_PAYLOAD_SIZE). To grow without touching any
    // OTHER sub-block's bytes, the block's own existing edges are shifted
    // into the newly claimed room at `new_offset`, preserving their order,
    // and the new edges are appended right after them — so logical order
    // (existing edges, then newly appended ones) is preserved even though
    // the physical start address moved left.
    payload_mut.copy_within(
        entry.offset as usize..entry.offset as usize + old_bytes,
        new_offset,
    );
    let mut off = new_offset + old_bytes;
    for &eid in new_edge_ids {
        payload_mut[off..off + EDGE_SIZE].copy_from_slice(&eid.to_le_bytes());
        off += EDGE_SIZE;
    }

    // Patch only this directory entry (offset + edge_count) — no other entry touched.
    write_dir_entry(
        payload_mut,
        idx,
        &DirEntry {
            node_id,
            direction,
            offset: new_offset_u16,
            edge_count: new_edge_count_u16,
        },
    );

    finalize_slab_buf(&mut buf);
    backend.write_page(DataFile::Adjacency, page_id, &buf)?;
    Ok(AppendOutcome::InPlace)
}

/// Marks the directory entry for `(node_id, direction)` as freed (tombstone).
///
/// Does not compact the packed sub-block area. Used by the overflow path
/// (Ciclo 4) after migrating a sub-block's edges to a dedicated chain.
///
/// The freed entry's bytes remain physically occupied by the (now orphaned)
/// edge data — [`slab_free_space`] only credits the freed DIRECTORY entry's
/// implicit reservation is NOT reclaimed either (the entry stays in the
/// directory, just marked dead, per the module's "no compaction" design);
/// what changes after this call is that a subsequent `write_subblock`/`
/// append_subblock_edges` will never target this `(node_id, direction)` pair
/// again, and — if this was the LAST sub-block in the packed area — the
/// high-water mark used by [`slab_free_space`] recedes to the previous
/// sub-block's end, since [`subblocks_bytes_used`] only counts entries whose
/// direction byte is not the freed sentinel... except freed entries ARE
/// still scanned for their raw offset/`edge_count` bytes, which is why
/// freeing is implemented as zeroing the entry's `offset`/`edge_count` too
/// (see body) so a freed trailing entry truly stops contributing to the
/// high-water mark.
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if the page cannot be read/written, or if
/// no live sub-block exists for `(node_id, direction)`.
pub fn free_subblock(
    backend: &mut dyn StorageBackend,
    page_id: PageId,
    node_id: u64,
    direction: AdjDirection,
) -> Result<()> {
    let mut buf = backend.read_page(DataFile::Adjacency, page_id)?;
    let payload = &buf[PAGE_HEADER_SIZE..];
    let (idx, _entry) = find_live_entry(page_id, payload, node_id, direction)?
        .ok_or_else(|| corrupt(page_id, "no live slab sub-block for (node_id, direction)"))?;

    let payload_mut = &mut buf[PAGE_HEADER_SIZE..];
    mark_dir_entry_freed(payload_mut, idx);
    // Zero offset/edge_count too, so a freed trailing sub-block does not keep
    // contributing to subblocks_bytes_used's high-water-mark scan.
    let base = DIR_COUNT_SIZE + idx as usize * DIR_ENTRY_SIZE;
    payload_mut[base + 10..base + 12].copy_from_slice(&0u16.to_le_bytes());
    payload_mut[base + 12..base + 14].copy_from_slice(&0u16.to_le_bytes());

    finalize_slab_buf(&mut buf);
    backend.write_page(DataFile::Adjacency, page_id, &buf)?;
    Ok(())
}

/// Shrinks a sub-block to `edge_ids`, keeping the node on its slab page.
///
/// Used when deleting an edge: the node keeps only the edges it still has and
/// stays put, so a delete never evicts a low-degree node from the slab.
///
/// The new list is written over the sub-block's existing footprint and its
/// directory entry's count is lowered; nothing else on the page moves, and no
/// directory entry is added. The vacated tail bytes stay claimed by this entry
/// (the module's no-compaction rule) and are reused by this node's own next
/// append before any room beyond them is claimed.
///
/// Only shrinking is supported, and that is what makes it total: growth needs
/// room that may not exist, which is [`append_subblock_edges`]'s job — it knows
/// when to migrate instead.
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if the page cannot be read/written, if no live
/// sub-block exists for `(node_id, direction)`, or if `edge_ids` is longer than
/// the sub-block being replaced.
pub fn rewrite_subblock_edges(
    backend: &mut dyn StorageBackend,
    page_id: PageId,
    node_id: u64,
    direction: AdjDirection,
    edge_ids: &[u64],
) -> Result<()> {
    let mut buf = backend.read_page(DataFile::Adjacency, page_id)?;
    let payload = &buf[PAGE_HEADER_SIZE..];
    let (idx, entry) = find_live_entry(page_id, payload, node_id, direction)?
        .ok_or_else(|| corrupt(page_id, "no live slab sub-block for (node_id, direction)"))?;

    let new_count = u16::try_from(edge_ids.len())
        .map_err(|_| corrupt(page_id, "slab sub-block edge count overflow"))?;
    if new_count > entry.edge_count {
        return Err(corrupt(
            page_id,
            "rewrite_subblock_edges cannot grow a sub-block; use append_subblock_edges",
        ));
    }

    // The write range is a prefix of the existing footprint, which read_dir_entry
    // already validated as in-bounds, so a shorter list cannot escape the page.
    let payload_mut = &mut buf[PAGE_HEADER_SIZE..];
    let mut off = entry.offset as usize;
    for &eid in edge_ids {
        payload_mut[off..off + EDGE_SIZE].copy_from_slice(&eid.to_le_bytes());
        off += EDGE_SIZE;
    }

    write_dir_entry(
        payload_mut,
        idx,
        &DirEntry {
            node_id,
            direction,
            offset: entry.offset,
            edge_count: new_count,
        },
    );

    finalize_slab_buf(&mut buf);
    backend.write_page(DataFile::Adjacency, page_id, &buf)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory::MemoryBackend;

    fn make_backend() -> MemoryBackend {
        MemoryBackend::new()
    }

    /// Asserts that `result` is `Err(Error::CorruptPage { .. })` with the
    /// expected `page_id`, failing with a diagnostic message (including the
    /// actual value obtained) otherwise. Centralizes the "must be a typed
    /// `CorruptPage` error, not a panic" assertion shared by every corruption
    /// test below (hallazgo #15's tests all funnel through this helper so a
    /// mismatch reports the real `page_id`/variant it saw, not just "assertion
    /// failed").
    fn assert_corrupt_page<T: std::fmt::Debug>(
        result: Result<T>,
        expected_page_id: PageId,
        context: &str,
    ) {
        match result {
            Err(Error::CorruptPage { page_id, .. }) => {
                assert_eq!(
                    page_id, expected_page_id,
                    "{context}: CorruptPage carried page_id {page_id}, expected {expected_page_id}"
                );
            }
            Err(other) => {
                panic!("{context}: expected Error::CorruptPage, got a different error: {other:?}")
            }
            Ok(value) => panic!("{context}: expected Error::CorruptPage, got Ok({value:?})"),
        }
    }

    /// Directly pokes `directory_count` (payload bytes `[0..2]`) to an
    /// arbitrary raw value on an otherwise-valid slab page, then re-finalizes
    /// (recomputes CRC) so the corruption is only in the logical field being
    /// tested, not also flagged by the page-level checksum. Mirrors
    /// `adjacency_codec::tests::write_v1_chain`'s approach of fabricating
    /// bytes by hand rather than going through the codec's own writers (which
    /// would refuse to produce invalid data in the first place).
    fn corrupt_directory_count(backend: &mut MemoryBackend, page_id: PageId, raw_count: u16) {
        let mut buf = backend.read_page(DataFile::Adjacency, page_id).unwrap();
        buf[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + 2].copy_from_slice(&raw_count.to_le_bytes());
        finalize_slab_buf(&mut buf);
        backend
            .write_page(DataFile::Adjacency, page_id, &buf)
            .unwrap();
    }

    /// Directly overwrites directory entry `idx`'s raw bytes (`node_id`,
    /// `direction` byte, `offset`, `edge_count`) without going through
    /// `write_dir_entry`'s type-safe `DirEntry`, so a caller can fabricate an
    /// out-of-range `offset`/`edge_count` or an invalid direction byte that
    /// the codec's own writers would never produce.
    fn corrupt_raw_dir_entry(
        backend: &mut MemoryBackend,
        page_id: PageId,
        idx: u16,
        node_id: u64,
        direction_byte: u8,
        offset: u16,
        edge_count: u16,
    ) {
        let mut buf = backend.read_page(DataFile::Adjacency, page_id).unwrap();
        let payload = &mut buf[PAGE_HEADER_SIZE..];
        let base = DIR_COUNT_SIZE + idx as usize * DIR_ENTRY_SIZE;
        payload[base..base + 8].copy_from_slice(&node_id.to_le_bytes());
        payload[base + 8] = direction_byte;
        payload[base + 9] = 0;
        payload[base + 10..base + 12].copy_from_slice(&offset.to_le_bytes());
        payload[base + 12..base + 14].copy_from_slice(&edge_count.to_le_bytes());
        finalize_slab_buf(&mut buf);
        backend
            .write_page(DataFile::Adjacency, page_id, &buf)
            .unwrap();
    }

    // --- Ciclo 1 ---

    #[test]
    fn slab_write_read_single_subblock_roundtrip() {
        let mut backend = make_backend();
        let page_id =
            allocate_slab_page(&mut backend).expect("allocating a fresh slab page must succeed");

        write_subblock(
            &mut backend,
            page_id,
            7,
            AdjDirection::Outgoing,
            &[100, 200, 300],
        )
        .expect("writing the first sub-block on a fresh page must succeed");

        let edges = read_subblock(&backend, page_id, 7, AdjDirection::Outgoing)
            .expect("reading back the sub-block just written must succeed");
        assert_eq!(
            edges,
            vec![100, 200, 300],
            "round-tripped edge IDs must match exactly what was written"
        );

        // Page format is slab, not a dedicated chain.
        assert!(
            is_slab_page(&backend, page_id)
                .expect("is_slab_page must succeed on a page this module allocated"),
            "allocate_slab_page must stamp PageType::AdjacencySlab, not a dedicated-chain adjacency page"
        );
        assert_eq!(
            backend.page_count(DataFile::Adjacency),
            1,
            "a single sub-block must fit on the one page allocate_slab_page created, no extra pages"
        );
    }

    // --- Ciclo 2 ---

    #[test]
    fn slab_multiple_subblocks_share_one_page() {
        let mut backend = make_backend();
        let page_id =
            allocate_slab_page(&mut backend).expect("allocating a fresh slab page must succeed");

        write_subblock(&mut backend, page_id, 1, AdjDirection::Outgoing, &[11])
            .expect("sub-block 1 write");
        write_subblock(
            &mut backend,
            page_id,
            2,
            AdjDirection::Outgoing,
            &[21, 22, 23],
        )
        .expect("sub-block 2 write");
        write_subblock(&mut backend, page_id, 3, AdjDirection::Outgoing, &[31])
            .expect("sub-block 3 write");

        assert_eq!(
            read_subblock(&backend, page_id, 1, AdjDirection::Outgoing).expect("read sub-block 1"),
            vec![11],
            "sub-block 1's edges must be unaffected by later writes to sub-blocks 2/3"
        );
        assert_eq!(
            read_subblock(&backend, page_id, 2, AdjDirection::Outgoing).expect("read sub-block 2"),
            vec![21, 22, 23],
            "sub-block 2's edges must round-trip intact"
        );
        assert_eq!(
            read_subblock(&backend, page_id, 3, AdjDirection::Outgoing).expect("read sub-block 3"),
            vec![31],
            "sub-block 3's edges must round-trip intact"
        );

        // Central assertion of Pieza 2: all three share ONE physical page.
        assert_eq!(
            backend.page_count(DataFile::Adjacency),
            1,
            "3 small sub-blocks must share ONE slab page, not allocate one page per node"
        );
    }

    #[test]
    fn slab_free_space_shrinks_as_subblocks_are_added() {
        let mut backend = make_backend();
        let page_id =
            allocate_slab_page(&mut backend).expect("allocating a fresh slab page must succeed");
        let buf_empty = backend
            .read_page(DataFile::Adjacency, page_id)
            .expect("read fresh page");
        let free_empty = slab_free_space(page_id, &buf_empty)
            .expect("slab_free_space on a freshly allocated page must succeed");
        assert_eq!(
            free_empty, PAGE_PAYLOAD_SIZE,
            "a freshly allocated slab page must report its entire payload as free"
        );

        write_subblock(&mut backend, page_id, 1, AdjDirection::Outgoing, &[11, 12])
            .expect("writing a 2-edge sub-block must succeed with a full page of free space");
        let buf_after = backend
            .read_page(DataFile::Adjacency, page_id)
            .expect("read page after write");
        let free_after = slab_free_space(page_id, &buf_after)
            .expect("slab_free_space after one write must succeed");

        // Derived from the module's own size constants, not a hardcoded magic
        // number, so this test stays correct if DIR_ENTRY_SIZE/EDGE_SIZE change.
        let expected_used = DIR_ENTRY_SIZE + 2 * EDGE_SIZE;
        assert_eq!(
            free_empty - free_after,
            expected_used,
            "free space must shrink by exactly one directory entry ({DIR_ENTRY_SIZE} bytes) plus 2 edge IDs ({EDGE_SIZE} bytes each)"
        );
    }

    #[test]
    fn slab_shrink_subblock_works_on_a_full_page() {
        // Deleting an edge shrinks a sub-block, and must keep working when the page
        // has no free space left — a delete is exactly when the caller has nowhere
        // else to put the node. An implementation that freed the entry and re-added
        // it would need a fresh directory slot and fail here (the no-compaction rule
        // means freeing reclaims no bytes unless the block is last).
        let mut backend = make_backend();
        let page_id =
            allocate_slab_page(&mut backend).expect("allocating a fresh slab page must succeed");

        // The node under test, deliberately NOT the last sub-block.
        write_subblock(
            &mut backend,
            page_id,
            5,
            AdjDirection::Outgoing,
            &[10, 20, 30],
        )
        .expect("write node 5");

        // Fill the rest of the page so nothing new could ever be inserted.
        let mut filler = 100u64;
        loop {
            let page = backend.read_page(DataFile::Adjacency, page_id).unwrap();
            if !slab_can_fit_subblock(page_id, &page, 1).unwrap() {
                break;
            }
            write_subblock(
                &mut backend,
                page_id,
                filler,
                AdjDirection::Outgoing,
                &[filler],
            )
            .expect("filler sub-block");
            filler += 1;
        }

        rewrite_subblock_edges(&mut backend, page_id, 5, AdjDirection::Outgoing, &[10, 30])
            .expect("shrinking a sub-block must succeed even on a full page");

        assert_eq!(
            read_subblock(&backend, page_id, 5, AdjDirection::Outgoing).unwrap(),
            vec![10, 30],
            "the shrunk sub-block must hold exactly the remaining edges"
        );
        // Neighbours are untouched: the shrink moved nobody else's bytes.
        assert_eq!(
            read_subblock(&backend, page_id, 100, AdjDirection::Outgoing).unwrap(),
            vec![100],
            "a neighbouring sub-block must be unaffected by the shrink"
        );
        assert_eq!(
            backend.page_count(DataFile::Adjacency),
            1,
            "shrinking must not allocate a page"
        );
    }

    #[test]
    fn slab_rewrite_subblock_rejects_growth() {
        // Growth needs room this operation cannot guarantee; append_subblock_edges
        // owns that path because it can migrate the node out instead.
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).unwrap();
        write_subblock(&mut backend, page_id, 5, AdjDirection::Outgoing, &[1, 2]).unwrap();

        let err =
            rewrite_subblock_edges(&mut backend, page_id, 5, AdjDirection::Outgoing, &[1, 2, 3])
                .unwrap_err();

        assert!(
            matches!(err, Error::CorruptPage { .. }),
            "growing via rewrite must be rejected, got {err:?}"
        );
    }

    // --- Ciclo 3 ---

    #[test]
    fn slab_append_to_existing_subblock_grows_in_place() {
        let mut backend = make_backend();
        let page_id =
            allocate_slab_page(&mut backend).expect("allocating a fresh slab page must succeed");

        write_subblock(&mut backend, page_id, 5, AdjDirection::Outgoing, &[1, 2])
            .expect("write node 5");
        // A second sub-block that stays untouched, to prove isolation.
        write_subblock(&mut backend, page_id, 6, AdjDirection::Outgoing, &[900])
            .expect("write node 6");

        // Append to node 5 — NOT the last sub-block anymore (node 6 is last) ->
        // must be NoRoom per this codec's no-displacement rule.
        let outcome_blocked = append_subblock_edges(
            &mut backend,
            page_id,
            5,
            AdjDirection::Outgoing,
            &[3, 4, 5],
        )
        .expect(
            "append_subblock_edges must return Ok(NoRoom), not an error, for a non-last sub-block",
        );
        assert_eq!(
            outcome_blocked,
            AppendOutcome::NoRoom,
            "node 5 is not the last-written sub-block (node 6 is) -> the no-displacement rule must reject in-place growth"
        );
        // Node 5's data must be untouched by the rejected append.
        assert_eq!(
            read_subblock(&backend, page_id, 5, AdjDirection::Outgoing)
                .expect("read node 5 after rejected append"),
            vec![1, 2],
            "a NoRoom append must not mutate the target sub-block's stored edges"
        );

        // Appending to node 6 (the actual last sub-block) grows in place.
        let outcome_ok = append_subblock_edges(
            &mut backend,
            page_id,
            6,
            AdjDirection::Outgoing,
            &[901, 902],
        )
        .expect("append_subblock_edges on the true last sub-block must succeed");
        assert_eq!(
            outcome_ok,
            AppendOutcome::InPlace,
            "node 6 IS the last-written sub-block -> in-place growth must be allowed"
        );
        assert_eq!(
            backend.page_count(DataFile::Adjacency),
            1,
            "in-place append must reuse the same page_id, no reallocation"
        );
        assert_eq!(
            read_subblock(&backend, page_id, 6, AdjDirection::Outgoing)
                .expect("read node 6 after append"),
            vec![900, 901, 902],
            "node 6's edges must be the original edge followed by the newly appended ones, in order"
        );
        // Node 5 (untouched entry) still reads back correctly.
        assert_eq!(
            read_subblock(&backend, page_id, 5, AdjDirection::Outgoing)
                .expect("read node 5 after node 6's append"),
            vec![1, 2],
            "growing node 6 in place must not disturb node 5's unrelated sub-block"
        );
    }

    #[test]
    fn slab_append_to_sole_last_subblock_in_place() {
        // A single sub-block is trivially "the last one" -> in-place append
        // must succeed with no other entries to disturb.
        let mut backend = make_backend();
        let page_id =
            allocate_slab_page(&mut backend).expect("allocating a fresh slab page must succeed");
        write_subblock(&mut backend, page_id, 42, AdjDirection::Incoming, &[1, 2])
            .expect("write node 42");

        let outcome = append_subblock_edges(
            &mut backend,
            page_id,
            42,
            AdjDirection::Incoming,
            &[3, 4, 5],
        )
        .expect("appending to the sole sub-block on a page must always succeed in place");
        assert_eq!(
            outcome,
            AppendOutcome::InPlace,
            "the only sub-block on the page is trivially the 'last' one -> must grow in place"
        );
        assert_eq!(
            read_subblock(&backend, page_id, 42, AdjDirection::Incoming)
                .expect("read node 42 after append"),
            vec![1, 2, 3, 4, 5],
            "appended edges must follow the original ones in insertion order"
        );
        assert_eq!(
            backend.page_count(DataFile::Adjacency),
            1,
            "in-place append must not allocate a new page"
        );
    }

    // --- Ciclo 4 ---

    #[test]
    fn slab_free_subblock_then_reuse_slot() {
        let mut backend = make_backend();
        let page_id =
            allocate_slab_page(&mut backend).expect("allocating a fresh slab page must succeed");
        write_subblock(&mut backend, page_id, 1, AdjDirection::Outgoing, &[10, 20])
            .expect("write node 1");

        let buf_before = backend
            .read_page(DataFile::Adjacency, page_id)
            .expect("read page before free");
        let free_before = slab_free_space(page_id, &buf_before)
            .expect("slab_free_space before free must succeed");

        free_subblock(&mut backend, page_id, 1, AdjDirection::Outgoing)
            .expect("freeing node 1's sub-block must succeed");

        let buf_after = backend
            .read_page(DataFile::Adjacency, page_id)
            .expect("read page after free");
        let free_after =
            slab_free_space(page_id, &buf_after).expect("slab_free_space after free must succeed");
        assert!(
            free_after > free_before,
            "freeing the trailing (last-written) sub-block must reclaim its edge bytes (before={free_before}, after={free_after})"
        );

        // A new write for a DIFFERENT node still works and the freed node_id
        // is no longer readable.
        let read_freed = read_subblock(&backend, page_id, 1, AdjDirection::Outgoing);
        assert!(
            read_freed.is_err(),
            "a freed (node_id, direction) must no longer be readable, got Ok: {read_freed:?}"
        );
        write_subblock(&mut backend, page_id, 2, AdjDirection::Outgoing, &[30])
            .expect("a different node_id must be able to reuse the reclaimed directory slot");
        assert_eq!(
            read_subblock(&backend, page_id, 2, AdjDirection::Outgoing)
                .expect("read node 2 after reuse"),
            vec![30],
            "the new sub-block written into the reclaimed slot must round-trip correctly"
        );
    }

    // --- Corruption hardening (issue #54 QR, hallazgos #1-#15) ---
    //
    // Every test below fabricates a page with bytes written DIRECTLY into
    // the payload (never through this module's own writers, which refuse to
    // produce invalid data), then asserts that the codec reports
    // Error::CorruptPage with the real page_id — never panics, never reads
    // or writes out of bounds.

    #[test]
    fn corrupt_directory_count_huge_read_dir_entry_returns_corrupt_page() {
        // Hallazgo #1/#2: a garbage directory_count (here u16::MAX = 65535)
        // makes directory_area_end(count) astronomically exceed
        // PAGE_PAYLOAD_SIZE. Before the fix, read_dir_entry indexed
        // `payload[base..base+8]` with base ~= 917000, panicking. Now
        // read_dir_total_count itself must reject the count up front.
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        corrupt_directory_count(&mut backend, page_id, u16::MAX);

        let result = read_subblock(&backend, page_id, 1, AdjDirection::Outgoing);
        assert_corrupt_page(
            result,
            page_id,
            "read_subblock with directory_count = u16::MAX",
        );
    }

    #[test]
    fn corrupt_directory_count_just_over_capacity_is_rejected() {
        // A directory_count that is only SLIGHTLY too large (not an extreme
        // value like u16::MAX) must be caught too — this exercises the exact
        // boundary of directory_area_end(count) > PAGE_PAYLOAD_SIZE rather
        // than an obviously-absurd value.
        let max_valid_count = u16::try_from((PAGE_PAYLOAD_SIZE - DIR_COUNT_SIZE) / DIR_ENTRY_SIZE)
            .expect("max_valid_count fits in u16 for this page size");
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        corrupt_directory_count(&mut backend, page_id, max_valid_count + 1);

        let result = read_subblock(&backend, page_id, 1, AdjDirection::Outgoing);
        assert_corrupt_page(
            result,
            page_id,
            "read_subblock with directory_count one past the max that fits the page",
        );
    }

    #[test]
    fn slab_free_space_rejects_corrupt_directory_count() {
        // Hallazgo #2: slab_free_space must validate directory_area_end
        // before trusting total_entries, same as any other reader — it must
        // return Err, not silently compute a nonsensical (or panicking)
        // free-space value.
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        corrupt_directory_count(&mut backend, page_id, u16::MAX);

        let buf = backend
            .read_page(DataFile::Adjacency, page_id)
            .expect("read corrupted page");
        let result = slab_free_space(page_id, &buf);
        assert_corrupt_page(
            result,
            page_id,
            "slab_free_space with directory_count = u16::MAX",
        );
    }

    #[test]
    fn corrupt_subblock_offset_out_of_range_read_subblock_returns_corrupt_page() {
        // Hallazgo #3/#4: a live entry (direction=Outgoing) whose offset/
        // edge_count place its byte range past PAGE_PAYLOAD_SIZE (here
        // offset=4075, edge_count=3 -> end=4075+24=4099 > 4080). Before the
        // fix, read_subblock indexed payload[4075..4083], panicking.
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        corrupt_directory_count(&mut backend, page_id, 1);
        corrupt_raw_dir_entry(&mut backend, page_id, 0, 7, 0, 4075, 3);

        let result = read_subblock(&backend, page_id, 7, AdjDirection::Outgoing);
        assert_corrupt_page(
            result,
            page_id,
            "read_subblock with an out-of-range (offset, edge_count)",
        );
    }

    #[test]
    fn corrupt_subblock_offset_overflow_is_rejected_not_wrapped() {
        // Hallazgo #3/#4 boundary case: offset + edge_count*EDGE_SIZE must not
        // even be allowed to wrap a usize computation; edge_count = u16::MAX
        // makes byte_len alone (u16::MAX * 8) already exceed PAGE_PAYLOAD_SIZE
        // by a wide margin, exercising the "clearly oversized len" path
        // distinctly from the "borderline past the edge" case above.
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        corrupt_directory_count(&mut backend, page_id, 1);
        corrupt_raw_dir_entry(&mut backend, page_id, 0, 7, 0, 0, u16::MAX);

        let result = read_subblock(&backend, page_id, 7, AdjDirection::Outgoing);
        assert_corrupt_page(
            result,
            page_id,
            "read_subblock with edge_count = u16::MAX (byte_len far exceeds page capacity)",
        );
    }

    #[test]
    fn write_subblock_rejects_page_with_corrupt_directory() {
        // write_subblock must fail closed too: it reads the directory before
        // deciding where to place a new sub-block, so a corrupt existing
        // directory must not be silently trusted just because we're about to
        // ADD an entry rather than read one.
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        corrupt_directory_count(&mut backend, page_id, u16::MAX);

        let result = write_subblock(&mut backend, page_id, 1, AdjDirection::Outgoing, &[1, 2, 3]);
        assert_corrupt_page(
            result,
            page_id,
            "write_subblock on a page with a corrupt directory_count",
        );
    }

    #[test]
    fn append_subblock_edges_rejects_out_of_range_entry() {
        // Hallazgo #5/#6/#7: append_subblock_edges must validate the target
        // entry's offset/edge_count before computing new_offset or calling
        // copy_within — a corrupted entry here must not panic via
        // copy_within's own bounds check, nor silently wrap a usize
        // subtraction into a huge offset.
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        corrupt_directory_count(&mut backend, page_id, 1);
        corrupt_raw_dir_entry(&mut backend, page_id, 0, 7, 0, 4075, 3);

        let result = append_subblock_edges(&mut backend, page_id, 7, AdjDirection::Outgoing, &[99]);
        assert_corrupt_page(
            result,
            page_id,
            "append_subblock_edges on an out-of-range target entry",
        );
    }

    #[test]
    fn free_subblock_rejects_out_of_range_entry() {
        // free_subblock also scans the directory via find_live_entry before
        // marking an entry freed; a corrupt entry earlier in the scan must
        // surface as CorruptPage rather than an index panic or a silent skip
        // that would make the intended target look "not found".
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        corrupt_directory_count(&mut backend, page_id, 1);
        corrupt_raw_dir_entry(&mut backend, page_id, 0, 7, 0, 4075, 3);

        let result = free_subblock(&mut backend, page_id, 7, AdjDirection::Outgoing);
        assert_corrupt_page(
            result,
            page_id,
            "free_subblock on an out-of-range target entry",
        );
    }

    #[test]
    fn corrupt_direction_byte_is_reported_as_corrupt_not_not_found() {
        // Hallazgo #8: a directory entry whose direction byte is neither a
        // valid direction (0/1) nor the freed sentinel (0xFF) — e.g. 0x02 —
        // must be reported as page corruption. Before the fix,
        // direction_from_u8's Err was collapsed via `.ok()?` into a bare
        // None, indistinguishable from "this (node_id, direction) simply
        // isn't in the directory".
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        corrupt_directory_count(&mut backend, page_id, 1);
        corrupt_raw_dir_entry(&mut backend, page_id, 0, 7, 0x02, 4000, 1);

        let result = read_subblock(&backend, page_id, 7, AdjDirection::Outgoing);
        assert_corrupt_page(
            result,
            page_id,
            "read_subblock with an invalid direction byte (0x02)",
        );
    }

    #[test]
    fn find_live_entry_reports_corruption_on_intermediate_entry_not_silent_skip() {
        // Hallazgo #9: an unrelated CORRUPT entry earlier in the directory
        // must abort the whole scan with CorruptPage — the page is
        // untrustworthy — rather than being silently skipped, which would
        // make a DIFFERENT, valid, later entry look reachable. Entry 0 is
        // corrupt (invalid direction byte); entry 1 (a different node_id)
        // would otherwise be a valid target for a search that doesn't match
        // node_id=7, proving the corruption itself is what triggers the
        // error, not merely "not found".
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        corrupt_directory_count(&mut backend, page_id, 2);
        corrupt_raw_dir_entry(&mut backend, page_id, 0, 999, 0x02, 4000, 1); // corrupt, unrelated node_id
        corrupt_raw_dir_entry(&mut backend, page_id, 1, 7, 0, 3900, 1); // would-be valid match

        let result = read_subblock(&backend, page_id, 7, AdjDirection::Outgoing);
        assert_corrupt_page(
            result,
            page_id,
            "a corrupt entry earlier in the directory must abort the scan even though a later entry would have matched",
        );
    }

    #[test]
    fn freed_entry_with_nonzero_offset_edge_count_is_corrupt() {
        // Hallazgo #10/#14: a freed entry (direction byte = DIR_ENTRY_FREED)
        // whose offset/edge_count are NOT zero cannot occur via this
        // module's own free_subblock (which always zeroes both), so it can
        // only mean the disk page is corrupt. The old code treated ANY freed
        // entry's offset/edge_count as "stale, ignore" without validating
        // them at all — this must now be caught by the same range validation
        // applied to every entry, live or freed.
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        corrupt_directory_count(&mut backend, page_id, 1);
        // Freed sentinel direction byte, but offset/edge_count place it
        // out-of-range instead of the zeroed values free_subblock would write.
        corrupt_raw_dir_entry(&mut backend, page_id, 0, 1, DIR_ENTRY_FREED, 4075, 3);

        let buf = backend
            .read_page(DataFile::Adjacency, page_id)
            .expect("read corrupted page");
        let result = subblocks_bytes_used(page_id, &buf[PAGE_HEADER_SIZE..]);
        assert_corrupt_page(
            result,
            page_id,
            "subblocks_bytes_used with a freed entry carrying a non-zeroed out-of-range offset/edge_count",
        );
    }

    #[test]
    fn live_entry_with_zero_edge_count_is_not_confused_with_freed() {
        // Hallazgo #11: a LIVE entry (direction byte = valid direction, not
        // DIR_ENTRY_FREED) with edge_count == 0 is a legitimate empty
        // sub-block, not a freed slot. It must still be found by
        // find_live_entry / read_subblock (returning zero edges), and must
        // still count as "occupying" its offset for subblocks_bytes_used's
        // low-water-mark scan (hallazgo #10's fix must key off the direction
        // byte, never off edge_count == 0).
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        corrupt_directory_count(&mut backend, page_id, 1);
        // Live (Outgoing) entry, offset at the very end of the payload,
        // edge_count = 0: a valid, empty, live sub-block.
        let offset_at_payload_end = u16::try_from(PAGE_PAYLOAD_SIZE)
            .expect("PAGE_PAYLOAD_SIZE (4080) always fits in u16 for this page format");
        corrupt_raw_dir_entry(&mut backend, page_id, 0, 1, 0, offset_at_payload_end, 0);

        let edges = read_subblock(&backend, page_id, 1, AdjDirection::Outgoing)
            .expect("a live entry with edge_count == 0 must be found and read back as an empty Vec, not treated as freed/missing");
        assert_eq!(
            edges,
            Vec::<u64>::new(),
            "a live zero-edge sub-block must read back as an empty edge list"
        );
    }

    #[test]
    fn write_subblock_directory_count_overflow_is_rejected() {
        // Hallazgo #12: write_subblock bumps directory_count by 1
        // (write_dir_total_count(payload, total + 1)). If total is corrupted
        // to a value where total+1 would overflow u16 arithmetic, or where
        // the resulting directory_area_end would exceed the page, this must
        // be a typed error, not a silent wraparound. Using the max count that
        // legitimately fits the page's directory area (so it is a realistic,
        // not merely numeric, boundary) plus 1 more entry demonstrates the
        // "directory has no room for one more entry" guard added alongside
        // the u16 checked_add.
        let max_valid_count = u16::try_from((PAGE_PAYLOAD_SIZE - DIR_COUNT_SIZE) / DIR_ENTRY_SIZE)
            .expect("max_valid_count fits in u16 for this page size");
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        corrupt_directory_count(&mut backend, page_id, max_valid_count);

        let result = write_subblock(&mut backend, page_id, 123, AdjDirection::Outgoing, &[1]);
        assert_corrupt_page(
            result,
            page_id,
            "write_subblock must refuse to grow the directory past what fits in PAGE_PAYLOAD_SIZE, not overflow/wrap",
        );
    }

    #[test]
    fn append_subblock_edges_copy_source_range_validated() {
        // Hallazgo #7: append_subblock_edges's copy_within source range
        // [entry.offset, entry.offset + old_bytes) must be validated before
        // the copy — this is defense-in-depth on top of read_dir_entry's own
        // validation (hallazgo #3/#4), asserted here directly against
        // append_subblock_edges so a future refactor that bypasses
        // read_dir_entry's guard is still caught by THIS function's own check.
        let mut backend = make_backend();
        let page_id = allocate_slab_page(&mut backend).expect("allocate slab page");
        write_subblock(&mut backend, page_id, 1, AdjDirection::Outgoing, &[10])
            .expect("write a real sub-block to append to");
        // Directly corrupt entry 0's edge_count to a huge value so its
        // computed byte range busts PAGE_PAYLOAD_SIZE, while direction stays
        // valid (this must be caught before it ever reaches copy_within).
        corrupt_raw_dir_entry(&mut backend, page_id, 0, 1, 0, 4072, u16::MAX);

        let result = append_subblock_edges(&mut backend, page_id, 1, AdjDirection::Outgoing, &[20]);
        assert_corrupt_page(
            result,
            page_id,
            "append_subblock_edges with a corrupted edge_count that would overflow the copy source range",
        );
    }
}

/// Property-based tests (issue #67).
///
/// What makes this codec different from every other one in this issue: a
/// single page is *shared* by many nodes, packed behind an intra-page
/// directory. So the invariant is not only "what I wrote comes back" but "and
/// writing mine did not disturb yours" — the failure mode #54 shipped was one
/// sub-block growing over its neighbour's directory entry and handing back a
/// wrong edge id with no error at all.
///
/// # What mutation testing showed here
///
/// Four separate attempts to break the *write* path went undetected — loosening
/// the fit predicate, loosening the free-space check, disabling that check
/// outright. That looked like a weak test, and it is worth recording that it
/// was not: `write_subblock` carries four independent guards (the fit check,
/// a checked subtraction that catches offset underflow, a directory-capacity
/// check, and a checked offset conversion), so removing any one leaves the
/// others catching the same case. That is the defence-in-depth added after
/// #54, working as intended.
///
/// Mutating the *read* path instead — returning one edge more than the entry
/// declares, which is precisely the #54 failure — does fail
/// `subblocks_sharing_a_page_do_not_disturb_each_other`. So the suite bites;
/// the write path simply has no single point left to break.
#[cfg(test)]
mod proptests {
    use super::*;
    use crate::storage::memory::MemoryBackend;
    use proptest::prelude::*;

    /// Small degrees: the whole point of the slab is packing many low-degree
    /// nodes into one page, so that is the regime worth generating. Large
    /// degrees take the dedicated-chain path, which `adjacency_codec` covers.
    fn subblock() -> impl Strategy<Value = (u64, bool, Vec<u64>)> {
        (
            // A wide node-id range: with a narrow one most draws collide and
            // get skipped as duplicate keys, so the page never fills. Measured
            // with 1..=64: the page still had 632 free bytes at best, which is
            // enough slack that no fit-check mutation could do damage.
            1u64..=4096,
            any::<bool>(),
            proptest::collection::vec(1u64..=u64::MAX, 0..12),
        )
    }

    fn dir_of(incoming: bool) -> AdjDirection {
        if incoming {
            AdjDirection::Incoming
        } else {
            AdjDirection::Outgoing
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

        /// Several sub-blocks sharing one page must each read back exactly
        /// what was written to them — including the ones written *before* the
        /// one that grew.
        ///
        /// Checking every sub-block at the end rather than after each write is
        /// deliberate: an overrun damages a *neighbour*, so verifying as you
        /// go would confirm the block just written and miss the one it broke.
        #[test]
        fn subblocks_sharing_a_page_do_not_disturb_each_other(
            // Up to 60 sub-blocks, because the page is what has to fill up.
            // Measured: a 12-edge sub-block costs 120 bytes against a 4080-byte
            // payload, so the six this generator first drew filled 17% of it and
            // never came near the boundary — a loosened fit check went
            // undetected. Roughly 34 fill the page, so the range has to reach
            // past that for the overrun to have anywhere to happen.
            blocks in proptest::collection::vec(subblock(), 1..90)
        ) {
            let mut backend = MemoryBackend::new();
            let page_id = allocate_slab_page(&mut backend).expect("fresh slab page");

            // Distinct (node, direction) keys only: writing the same key twice
            // is an update, not a second sub-block, and would make the
            // expected contents ambiguous.
            let mut written: Vec<(u64, AdjDirection, Vec<u64>)> = Vec::new();
            for (node_id, incoming, edge_ids) in blocks {
                let direction = dir_of(incoming);
                if written.iter().any(|(n, d, _)| *n == node_id && *d as u8 == direction as u8) {
                    continue;
                }
                // Deliberately NOT gated on `slab_can_fit_subblock`: attempting
                // the write and recording only what succeeded is what drives
                // the page to its limit, which is where a neighbour gets
                // overrun. Skipping the ones that do not fit would keep this
                // test comfortably away from the only interesting boundary.
                if write_subblock(&mut backend, page_id, node_id, direction, &edge_ids).is_ok() {
                    written.push((node_id, direction, edge_ids));
                }
            }

            for (node_id, direction, expected) in written {
                let got = read_subblock(&backend, page_id, node_id, direction)
                    .expect("a written sub-block must read back");
                prop_assert_eq!(
                    got, expected,
                    "sub-block for node {} came back different — a neighbour's write \
                     overran it",
                    node_id
                );
            }
        }


        /// Reading a key that was never written must say so, not return
        /// someone else's edges. The directory lookup is what keeps tenants
        /// apart, so a miss has to stay a miss.
        #[test]
        fn reading_an_absent_key_never_returns_another_nodes_edges(
            (node_id, incoming, edge_ids) in subblock(),
            other_id in 65u64..=128,
        ) {
            let mut backend = MemoryBackend::new();
            let page_id = allocate_slab_page(&mut backend).expect("fresh slab page");
            let direction = dir_of(incoming);

            // El rango de `other_id` (65..=128) NO es disjunto del de `node_id`
            // (1..=4096): se solapa entero. El comentario anterior afirmaba lo
            // contrario y la prueba fallaba cuando el generador sacaba el mismo
            // nodo dos veces — leerlo devuelve lo que sí se escribió, que es
            // correcto, no un fallo de aislamiento.
            //
            // Lo que esta prueba quiere comprobar es que leer un nodo AUSENTE no
            // devuelve las aristas de OTRO, así que la coincidencia se descarta
            // explícitamente en vez de darla por imposible.
            prop_assume!(other_id != node_id);
            prop_assume!(write_subblock(&mut backend, page_id, node_id, direction, &edge_ids).is_ok());

            let outcome = read_subblock(&backend, page_id, other_id, direction);
            prop_assert!(
                outcome.is_err(),
                "reading an absent node returned edges belonging to another"
            );
        }

        /// Reading arbitrary page ids and keys must report errors, never
        /// panic. This codec validates its directory ranges explicitly (a
        /// lesson from #54); this pins that it keeps doing so.
        #[test]
        fn read_never_panics_on_arbitrary_input(
            (node_id, incoming, edge_ids) in subblock(),
            probe_page in any::<u32>(),
            probe_node in any::<u64>(),
        ) {
            let mut backend = MemoryBackend::new();
            let page_id = allocate_slab_page(&mut backend).expect("fresh slab page");
            let _ = write_subblock(&mut backend, page_id, node_id, dir_of(incoming), &edge_ids);

            let outcome = read_subblock(&backend, probe_page, probe_node, dir_of(incoming));
            prop_assert!(outcome.is_ok() || outcome.is_err());
        }
    }
}
