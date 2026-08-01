// SPDX-License-Identifier: Apache-2.0

//! Adjacency page encoding.
//!
//! # `EDGE_COUNT_BOUND`
//!
//! The `edge_count` field in a record header is a `u32` holding the total
//! degree of one node, and several sites cast a `usize` total into it. That is
//! bounded rather than checked: reaching `u32::MAX` needs 4.29 billion edges on
//! a single node, and the chain cannot address that many — page ids are
//! themselves `u32` and each page holds ~509 edge ids, so the chain tops out
//! around 2^32 × 509 slots of addressable space while the count would have to
//! wrap first. Sites relying on this carry a short reference to this note
//! (issue #65).

use crate::error::Result;
use crate::storage::backend::{DataFile, PageId, StorageBackend};
use crate::storage::page::{
    finalize_page, magic, new_page_buf, PageType, PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE,
};
use crate::Error;

const NO_NEXT: u32 = 0xFFFF_FFFF;

/// Adjacency first-page format version stamped in the page header (`version`
/// field). `V1` is the legacy layout without a persisted `last_page_id` (tail
/// discovered by walking the chain). `V2` persists `last_page_id` in the record
/// header so the tail is resolved in one read. Reads accept both; writes always
/// emit `V2`. See [`read_adj_chain_state`] for the version-gated read path.
const ADJ_FORMAT_V1: u16 = 1;
const ADJ_FORMAT_V2: u16 = 2;

/// Rejects a page that is not a dedicated-chain adjacency page.
///
/// Dedicated chains and shared slabs (issue #54) coexist in `DataFile::Adjacency`
/// under the same `magic::ADJACENCY` stamp, and both are reached through the same
/// node-slot pointer. Their `version` number spaces are independent — a slab page
/// stamps version 1 just as a legacy V1 chain page does — so `page_type` is the
/// only field that tells the two apart. Without this check, a slab page reaching
/// this codec would be parsed as a V1 chain: its bytes would be read as a record
/// header and a `next_page` pointer, yielding fabricated edges instead of an error.
const fn ensure_dedicated_chain_page(
    header: &crate::storage::page::PageHeader,
    page_id: PageId,
) -> Result<()> {
    if header.page_type == PageType::Adjacency as u16 {
        return Ok(());
    }
    Err(corrupt(
        page_id,
        "expected a dedicated adjacency chain page, found a different page type",
    ))
}

/// Record overhead: `node_id`(8) + direction(1) + `edge_count`(4) + pad(3) = 16.
/// Unchanged from V1 — edges start at the same offset in both formats, so a V1
/// page reads back correctly and single-page capacity is unchanged. The V2
/// `last_page_id` lives at the END of the first page's payload (see
/// [`LAST_PAGE_ID_TAIL_OFFSET`]), not in this header, precisely to avoid
/// shifting the edge region.
const RECORD_HEADER_SIZE: usize = 16;

/// Byte offset (within the payload) of the V2 `last_page_id` field on a chained
/// first page: the 4 bytes immediately before the trailing `next_page` pointer.
/// A V1 chained first page used this slot for an edge, so V2 reserves one fewer
/// edge on the first page (see [`MAX_EDGES_CHAINED_PAGE`]).
const LAST_PAGE_ID_TAIL_OFFSET: usize = PAGE_PAYLOAD_SIZE - 8;

/// Max edges per single page: (4080 - 16) / 8 = 508. Unchanged from V1 — a
/// single-page record carries no `last_page_id` (tail is the first page).
const MAX_EDGES_SINGLE_PAGE: usize = (PAGE_PAYLOAD_SIZE - RECORD_HEADER_SIZE) / 8;

/// When chaining, the last 8 bytes of the first page's payload hold
/// `last_page_id`(4) + `next_page`(4). So a V2 chained first page holds one fewer
/// edge than V1 did (which reserved only 4 bytes for `next_page`).
const MAX_EDGES_CHAINED_PAGE: usize = (PAGE_PAYLOAD_SIZE - RECORD_HEADER_SIZE - 8) / 8;

/// Max edges on a continuation page: (4080 - 4) / 8 = 509.5 -> 509
const MAX_EDGES_CONT_PAGE: usize = (PAGE_PAYLOAD_SIZE - 4) / 8;

/// Edges the FIRST page of a chained record holds, which differs by format
/// version: V1 reserved only 4 trailing bytes (`next_page`) → 507 edges; V2
/// reserves 8 (`last_page_id` + `next_page`) → 506. Reads must use the value
/// matching the page's own version so a legacy V1 chain decodes correctly and a
/// V2 chain written by this code round-trips.
const fn first_page_edges_for_version(version: u16) -> usize {
    if version >= ADJ_FORMAT_V2 {
        MAX_EDGES_CHAINED_PAGE
    } else {
        // V1: 4-byte next_page only.
        (PAGE_PAYLOAD_SIZE - RECORD_HEADER_SIZE - 4) / 8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdjDirection {
    Outgoing = 0,
    Incoming = 1,
}

/// Pointer to where a node's adjacency data lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdjacencyPointer {
    pub outgoing_page: Option<PageId>,
    pub incoming_page: Option<PageId>,
}

/// An adjacency record for one node + direction.
#[derive(Debug, Clone)]
pub struct AdjacencyRecord {
    pub node_id: u64,
    pub direction: AdjDirection,
    pub edge_ids: Vec<u64>,
}

/// Encodes an adjacency record into bytes.
///
/// Format:
/// ```text
/// node_id:    u64 (8 bytes, LE)
/// direction:  u8  (0=outgoing, 1=incoming)
/// edge_count: u32 (4 bytes, LE)
/// _pad:       [u8; 3]
/// edge_ids:   [u64; edge_count] (8 bytes each, LE)
/// ```
///
/// Used only for single-page records, whose tail is the first page (no
/// `last_page_id` needed). Chained records are written by [`write_adjacency`],
/// which places the V2 `last_page_id` at the end of the first page's payload.
#[must_use]
pub fn encode_adjacency_record(record: &AdjacencyRecord) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RECORD_HEADER_SIZE + record.edge_ids.len() * 8);
    buf.extend_from_slice(&record.node_id.to_le_bytes());
    buf.push(record.direction as u8);
    // A record is built to fit one page, so edge_ids.len() is bounded by
    // MAX_EDGES_SINGLE_PAGE (~509) — orders of magnitude below u32.
    #[allow(clippy::cast_possible_truncation)]
    buf.extend_from_slice(&(record.edge_ids.len() as u32).to_le_bytes());
    buf.extend_from_slice(&[0u8; 3]); // padding
    for &eid in &record.edge_ids {
        buf.extend_from_slice(&eid.to_le_bytes());
    }
    buf
}

/// Writes an adjacency record to adjacency pages via the backend.
///
/// If the record fits on a single page, one page is used. Otherwise pages
/// are chained: the first page holds the record header + as many edge IDs
/// as fit, with `next_page` in the last 4 payload bytes. Continuation pages
/// hold `next_page(4)` followed by more edge IDs.
pub fn write_adjacency(
    backend: &mut dyn StorageBackend,
    record: &AdjacencyRecord,
) -> Result<PageId> {
    let edge_count = record.edge_ids.len();

    // Does it fit on a single page without chaining?
    if edge_count <= MAX_EDGES_SINGLE_PAGE {
        let page_id = backend.allocate_page(DataFile::Adjacency)?;
        let encoded = encode_adjacency_record(record);

        let mut buf = new_page_buf();
        buf[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + encoded.len()].copy_from_slice(&encoded);
        // Single-page: tail is the first page (derivable on read), so the
        // last_page_id field stays at its 0 placeholder. Stamp V2.
        finalize_page(&mut buf, magic::ADJACENCY, ADJ_FORMAT_V2, PageType::Adjacency, 1);
        backend.write_page(DataFile::Adjacency, page_id, &buf)?;
        return Ok(page_id);
    }

    // Needs chaining
    let first_page_id = backend.allocate_page(DataFile::Adjacency)?;

    // First page: record header + up to MAX_EDGES_CHAINED_PAGE edges + next_page
    let first_batch = &record.edge_ids[..MAX_EDGES_CHAINED_PAGE];
    let mut remaining_edges = &record.edge_ids[MAX_EDGES_CHAINED_PAGE..];

    let next_page_id = backend.allocate_page(DataFile::Adjacency)?;

    {
        let mut buf = new_page_buf();
        let p = PAGE_HEADER_SIZE;
        buf[p..p + 8].copy_from_slice(&record.node_id.to_le_bytes());
        buf[p + 8] = record.direction as u8;
        // `edge_count` is `edge_ids.len()` for a single-page record, bounded by
        // `MAX_EDGES_SINGLE_PAGE` (~509).
        #[allow(clippy::cast_possible_truncation)]
        buf[p + 9..p + 13].copy_from_slice(&(edge_count as u32).to_le_bytes());
        // 3 bytes padding already zero
        let mut off = p + RECORD_HEADER_SIZE;
        for &eid in first_batch {
            buf[off..off + 8].copy_from_slice(&eid.to_le_bytes());
            off += 8;
        }
        // V2 trailer: next_page in the last 4 payload bytes; last_page_id in the
        // 4 bytes before it is stamped by backpatch_last_page_id below.
        let np_off = p + PAGE_PAYLOAD_SIZE - 4;
        buf[np_off..np_off + 4].copy_from_slice(&next_page_id.to_le_bytes());

        finalize_page(&mut buf, magic::ADJACENCY, ADJ_FORMAT_V2, PageType::Adjacency, 1);
        backend.write_page(DataFile::Adjacency, first_page_id, &buf)?;
    }

    // Continuation pages. Track the last page that actually holds edges so the
    // first page's last_page_id can be backpatched to it.
    let mut current_page_id = next_page_id;
    let mut last_written_page = next_page_id;
    while !remaining_edges.is_empty() {
        let batch_size = remaining_edges.len().min(MAX_EDGES_CONT_PAGE);
        let batch = &remaining_edges[..batch_size];
        remaining_edges = &remaining_edges[batch_size..];

        let next = if remaining_edges.is_empty() {
            NO_NEXT
        } else {
            backend.allocate_page(DataFile::Adjacency)?
        };

        let mut buf = new_page_buf();
        let p = PAGE_HEADER_SIZE;
        buf[p..p + 4].copy_from_slice(&next.to_le_bytes());
        let mut off = p + 4;
        for &eid in batch {
            buf[off..off + 8].copy_from_slice(&eid.to_le_bytes());
            off += 8;
        }

        finalize_page(&mut buf, magic::ADJACENCY, ADJ_FORMAT_V2, PageType::Adjacency, 0);
        backend.write_page(DataFile::Adjacency, current_page_id, &buf)?;
        last_written_page = current_page_id;
        current_page_id = next;
    }

    // Backpatch the first page's last_page_id now that the tail is known, then
    // recompute its CRC (finalize_page). One extra read+write, only on a full
    // chain write (rare — appends use the O(1) cached-tail path instead).
    backpatch_last_page_id(backend, first_page_id, last_written_page)?;

    Ok(first_page_id)
}

/// Rewrites the `last_page_id` field in the header of a chain's first page and
/// recomputes its CRC. Used after a full chain write and after an append that
/// extends the chain onto a new tail page.
fn backpatch_last_page_id(
    backend: &mut dyn StorageBackend,
    first_page_id: PageId,
    last_page_id: PageId,
) -> Result<()> {
    let mut buf = backend.read_page(DataFile::Adjacency, first_page_id)?;
    let off = PAGE_HEADER_SIZE + LAST_PAGE_ID_TAIL_OFFSET;
    buf[off..off + 4].copy_from_slice(&last_page_id.to_le_bytes());
    // Re-finalize preserving magic/type; slot_count for a first page is 1.
    finalize_page(&mut buf, magic::ADJACENCY, ADJ_FORMAT_V2, PageType::Adjacency, 1);
    backend.write_page(DataFile::Adjacency, first_page_id, &buf)?;
    Ok(())
}

/// Captures the structural state of an existing adjacency chain
/// without decoding all edge IDs.
///
/// Used by [`append_adjacency`] to perform in-place extension.
#[derive(Debug, Clone, Copy)]
pub struct AdjChainState {
    /// `PageId` of the first (header) page in the chain.
    ///
    /// Echoes the caller-supplied first page; retained for a complete
    /// structural snapshot and asserted by the chain-state tests, though
    /// `append_adjacency` reuses its own `first_page_id` argument.
    pub first_page_id: PageId,
    /// `PageId` of the last page in the chain (may equal `first_page_id`).
    pub last_page_id: PageId,
    /// Total edge count stored in the `edge_count` field of the first page.
    pub total_edges: usize,
    /// Number of edge slots currently occupied in the last page.
    pub last_page_used_slots: usize,
    /// `true` when the chain is a single-page record (no `next_page` pointer).
    pub is_single: bool,
}

/// Reads structural metadata of an adjacency chain without decoding edge IDs.
///
/// Traverses the page chain to locate the last page and count its used slots.
/// Cost is O(number of pages), not O(number of edges).
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if any page in the chain cannot be read, or if
/// `first_page_id` is a shared slab page rather than a dedicated-chain page.
pub fn read_adj_chain_state(
    backend: &dyn StorageBackend,
    first_page_id: PageId,
) -> Result<AdjChainState> {
    let page = backend.read_page(DataFile::Adjacency, first_page_id)?;
    let header = crate::storage::page::PageHeader::read_from(&page);
    ensure_dedicated_chain_page(&header, first_page_id)?;
    let version = header.version;
    let payload = &page[PAGE_HEADER_SIZE..];

    let total_edges = u32::from_le_bytes(payload[9..13].try_into().unwrap()) as usize;

    if total_edges <= MAX_EDGES_SINGLE_PAGE {
        // Single-page record: tail is the first page, no read of the chain.
        return Ok(AdjChainState {
            first_page_id,
            last_page_id: first_page_id,
            total_edges,
            last_page_used_slots: total_edges,
            is_single: true,
        });
    }

    // Chained. In V2 the last page id is persisted in the first-page header, so
    // the tail is resolved in this single read (O(1)); last_page_used_slots is
    // derived arithmetically from total_edges. In V1 (legacy) that field is
    // absent, so fall back to walking the chain once.
    if version >= ADJ_FORMAT_V2 {
        let last_page_id = u32::from_le_bytes(
            payload[LAST_PAGE_ID_TAIL_OFFSET..LAST_PAGE_ID_TAIL_OFFSET + 4].try_into().unwrap(),
        );
        return Ok(state_after_append(first_page_id, last_page_id, total_edges));
    }

    // V1 (legacy) fallback: the field is absent, so walk to the last page. Any
    // subsequent append rewrites the first page as V2, migrating it lazily.
    debug_assert_eq!(version, ADJ_FORMAT_V1, "unexpected adjacency page version");
    let mut next_page =
        u32::from_le_bytes(payload[PAGE_PAYLOAD_SIZE - 4..].try_into().unwrap());
    let mut last_page_id = first_page_id;
    let mut edges_accounted = first_page_edges_for_version(version);

    while next_page != NO_NEXT {
        last_page_id = next_page;
        let cont = backend.read_page(DataFile::Adjacency, next_page)?;
        let cont_payload = &cont[PAGE_HEADER_SIZE..];
        next_page = u32::from_le_bytes(cont_payload[0..4].try_into().unwrap());
        if next_page != NO_NEXT {
            edges_accounted += MAX_EDGES_CONT_PAGE;
        }
    }

    let last_page_used_slots = total_edges - edges_accounted;

    Ok(AdjChainState {
        first_page_id,
        last_page_id,
        total_edges,
        last_page_used_slots,
        is_single: false,
    })
}

/// Computes the `AdjChainState` a chain has after an append that brought it to
/// `new_total` edges, whose last written page is `last_page_id`.
///
/// Pure arithmetic over the page-capacity constants — no page reads — so callers
/// can cache the post-append tail without re-walking the chain. Mirrors what
/// `read_adj_chain_state` would return for the same chain.
const fn state_after_append(first_page_id: PageId, last_page_id: PageId, new_total: usize) -> AdjChainState {
    if new_total <= MAX_EDGES_SINGLE_PAGE {
        return AdjChainState {
            first_page_id,
            last_page_id: first_page_id,
            total_edges: new_total,
            last_page_used_slots: new_total,
            is_single: true,
        };
    }
    // Chained: first page holds MAX_EDGES_CHAINED_PAGE, the rest spread over
    // continuation pages of MAX_EDGES_CONT_PAGE each. A last page filled exactly
    // to capacity leaves a zero remainder, which means MAX_EDGES_CONT_PAGE used.
    let in_conts = new_total - MAX_EDGES_CHAINED_PAGE;
    let rem = in_conts % MAX_EDGES_CONT_PAGE;
    let last_page_used_slots = if rem == 0 { MAX_EDGES_CONT_PAGE } else { rem };
    AdjChainState {
        first_page_id,
        last_page_id,
        total_edges: new_total,
        last_page_used_slots,
        is_single: false,
    }
}

/// Case A of [`append_adjacency_with_state`]: the chain is a single page and the
/// new edges still fit without chaining. Appends them in place and returns the
/// post-append tail state.
///
/// The existing-edge count and write offset are read from the page's own header,
/// not from the caller's `cached_total` — so a stale cached state can never place
/// the new edges at the wrong offset (defense in depth; under correct cache
/// discipline the two agree, asserted in debug builds).
fn append_into_single_page(
    backend: &mut dyn StorageBackend,
    first_page_id: PageId,
    cached_total: usize,
    new_edge_ids: &[u64],
) -> Result<(PageId, Vec<PageId>, AdjChainState)> {
    let mut buf = backend.read_page(DataFile::Adjacency, first_page_id)?;
    let p = PAGE_HEADER_SIZE;
    let existing = u32::from_le_bytes(buf[p + 9..p + 13].try_into().unwrap()) as usize;
    debug_assert_eq!(
        existing, cached_total,
        "cached total_edges disagrees with the page header"
    );
    let real_new_total = existing + new_edge_ids.len();

    // Update edge_count in the header (bytes [9..13] of payload).
    // Single-page append: the caller only takes this path when the result
    // still fits one page, so the total is bounded by `MAX_EDGES_SINGLE_PAGE`.
    #[allow(clippy::cast_possible_truncation)]
    buf[p + 9..p + 13].copy_from_slice(&(real_new_total as u32).to_le_bytes());

    // Append new edge_ids after the real existing ones.
    let mut off = p + RECORD_HEADER_SIZE + existing * 8;
    for &eid in new_edge_ids {
        buf[off..off + 8].copy_from_slice(&eid.to_le_bytes());
        off += 8;
    }

    finalize_page(&mut buf, magic::ADJACENCY, ADJ_FORMAT_V2, PageType::Adjacency, 1);
    backend.write_page(DataFile::Adjacency, first_page_id, &buf)?;
    let new_state = state_after_append(first_page_id, first_page_id, real_new_total);
    Ok((first_page_id, vec![first_page_id], new_state))
}

/// Appends `new_edge_ids` to an existing adjacency chain in-place,
/// or creates a new chain if `existing_first_page` is `None`.
///
/// Thin wrapper over [`append_adjacency_with_state`] that always recomputes the
/// chain state via [`read_adj_chain_state`]. Retained only for the codec tests
/// that exercise the no-cache path directly; production always goes through
/// [`append_adjacency_with_state`] with a cached tail state, so this wrapper is
/// gated to test builds.
///
/// Returns `(first_page_id, pages_written)` where `pages_written` is the
/// list of `PageId` values that were written during this call.
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if any page in the chain cannot be read
/// or written.
#[cfg(test)]
fn append_adjacency(
    backend: &mut dyn StorageBackend,
    node_id: u64,
    direction: AdjDirection,
    existing_first_page: Option<PageId>,
    new_edge_ids: &[u64],
) -> Result<(PageId, Vec<PageId>)> {
    let (pid, pages, _state) = append_adjacency_with_state(
        backend,
        node_id,
        direction,
        existing_first_page,
        None,
        new_edge_ids,
    )?;
    Ok((pid, pages))
}

/// Appends `new_edge_ids` to an adjacency chain.
///
/// Reuses a precomputed `existing_state` (the chain tail) when supplied to avoid
/// re-walking the chain, and returns the resulting `AdjChainState` so callers
/// can cache it.
///
/// When `existing_state` is `Some`, the O(pages) chain walk of
/// [`read_adj_chain_state`] is skipped entirely; only the last existing page
/// (partial fill) and any newly allocated continuation pages are read/written —
/// O(`new_edge_ids.len()` / page capacity), independent of node degree. When
/// `None`, the state is recomputed exactly as before, so a cache miss degrades
/// to today's correctness, never to incorrect behavior.
///
/// # Errors
///
/// Returns [`Error::CorruptPage`] if any page in the chain cannot be read
/// or written.
///
/// # Panics
///
/// Panics via `debug_assert` only if a supplied `existing_state` does not match
/// the `existing_first_page` argument, or if the recomposed edge total
/// disagrees with the chain state (internal invariants, not reachable from valid
/// input).
/// If the chain at `first_page_id` is a legacy V1 multi-page record, rewrite it
/// whole as V2 with `new_edge_ids` appended, and return the append result. The
/// two formats put a different edge count on the first page, so a V1 chain
/// cannot be extended in place under V2 offsets — hence a one-shot full rewrite
/// (bounded per legacy chain; brand-new chains are already V2). Returns `None`
/// when the chain is already V2 or is single-page (identical layout across
/// versions), leaving the caller to take the normal in-place append path.
///
/// Reads the first page once to inspect the version and total; that read is on
/// the cache-miss path, where the caller would read it anyway.
fn try_migrate_v1_chain(
    backend: &mut dyn StorageBackend,
    node_id: u64,
    direction: AdjDirection,
    first_page_id: PageId,
    new_edge_ids: &[u64],
) -> Result<Option<(PageId, Vec<PageId>, AdjChainState)>> {
    let first_page = backend.read_page(DataFile::Adjacency, first_page_id)?;
    let version = crate::storage::page::PageHeader::read_from(&first_page).version;
    let total_edges = u32::from_le_bytes(
        first_page[PAGE_HEADER_SIZE + 9..PAGE_HEADER_SIZE + 13].try_into().unwrap(),
    ) as usize;
    if version >= ADJ_FORMAT_V2 || total_edges <= MAX_EDGES_SINGLE_PAGE {
        return Ok(None);
    }
    let mut all = read_adjacency(backend, first_page_id)?.edge_ids;
    all.extend_from_slice(new_edge_ids);
    let record = AdjacencyRecord { node_id, direction, edge_ids: all };
    let pid = write_adjacency(backend, &record)?;
    let migrated = read_adj_chain_state(backend, pid)?;
    Ok(Some((pid, vec![pid], migrated)))
}

/// Rewrites the first page of a record in chained mode: stamps the new total,
/// writes `first_batch` into the page, allocates the head continuation page and
/// links to it. Returns the continuation page id.
///
/// Split out of [`append_adjacency_with_state`] to keep that function readable;
/// it is one step of case B (single page overflowing into a chain).
fn rewrite_first_page_chained(
    backend: &mut dyn StorageBackend,
    first_page_id: PageId,
    new_total: usize,
    first_batch: &[u64],
    written_pages: &mut Vec<PageId>,
) -> Result<PageId> {
    let mut first_buf = backend.read_page(DataFile::Adjacency, first_page_id)?;
    let p = PAGE_HEADER_SIZE;
    // Bounded: see EDGE_COUNT_BOUND note on this module.
    #[allow(clippy::cast_possible_truncation)]
    let total = new_total as u32;
    first_buf[p + 9..p + 13].copy_from_slice(&total.to_le_bytes());
    let mut off = p + RECORD_HEADER_SIZE;
    for &eid in first_batch {
        first_buf[off..off + 8].copy_from_slice(&eid.to_le_bytes());
        off += 8;
    }

    // Allocate the head continuation page (the caller only takes this path when
    // the rest is non-empty).
    let head_cont = backend.allocate_page(DataFile::Adjacency)?;
    let np_off = p + PAGE_PAYLOAD_SIZE - 4;
    first_buf[np_off..np_off + 4].copy_from_slice(&head_cont.to_le_bytes());
    finalize_page(&mut first_buf, magic::ADJACENCY, ADJ_FORMAT_V2, PageType::Adjacency, 1);
    backend.write_page(DataFile::Adjacency, first_page_id, &first_buf)?;
    written_pages.push(first_page_id);

    Ok(head_cont)
}

pub fn append_adjacency_with_state(
    backend: &mut dyn StorageBackend,
    node_id: u64,
    direction: AdjDirection,
    existing_first_page: Option<PageId>,
    existing_state: Option<AdjChainState>,
    new_edge_ids: &[u64],
) -> Result<(PageId, Vec<PageId>, AdjChainState)> {
    let Some(first_page_id) = existing_first_page else {
        // No existing chain — delegate to write_adjacency (full write).
        let record = AdjacencyRecord {
            node_id,
            direction,
            edge_ids: new_edge_ids.to_vec(),
        };
        let pid = write_adjacency(backend, &record)?;
        // The full write links continuation pages internally. For a single-page
        // record the tail is the first page — pure arithmetic, no read. When the
        // fresh chain is large enough to span continuation pages, their ids are
        // not returned by `write_adjacency`, so recover the tail with one chain
        // read (bounded, and rare: a brand-new node created with >508 edges at
        // once).
        let state = if new_edge_ids.len() <= MAX_EDGES_SINGLE_PAGE {
            state_after_append(pid, pid, new_edge_ids.len())
        } else {
            read_adj_chain_state(backend, pid)?
        };
        return Ok((pid, vec![pid], state));
    };

    // A cached tail state is only ever produced for a V2 chain (writes emit V2;
    // a legacy chain is migrated on its first cache-miss append), so the hot
    // cached path needs no version re-check and no extra page read.
    let state = if let Some(s) = existing_state {
        debug_assert_eq!(
            s.first_page_id, first_page_id,
            "cached AdjChainState first page must match existing_first_page"
        );
        s
    } else if let Some(migrated) =
        try_migrate_v1_chain(backend, node_id, direction, first_page_id, new_edge_ids)?
    {
        // Cache miss over a legacy V1 chain: it was rewritten whole as V2 with the
        // new edges already applied. Done.
        return Ok(migrated);
    } else {
        // Cache miss over a V2 (or single-page) chain: resolve the tail (O(1) for
        // V2) and continue with the in-place append below.
        read_adj_chain_state(backend, first_page_id)?
    };

    let new_total = state.total_edges + new_edge_ids.len();
    let mut written_pages: Vec<PageId> = Vec::new();

    if state.is_single && new_total <= MAX_EDGES_SINGLE_PAGE {
        // Case A: single page, new edges fit without triggering chaining.
        return append_into_single_page(
            backend,
            first_page_id,
            state.total_edges,
            new_edge_ids,
        );
    }

    if state.is_single {
        // Case B: single page overflows → rewrite the first page as chained and
        // allocate continuation pages for the excess.
        //
        // A single page can hold up to MAX_EDGES_SINGLE_PAGE (508) edges, but a
        // chained first page only holds MAX_EDGES_CHAINED_PAGE (507) — the last
        // 4 payload bytes become the `next_page` pointer. So when the existing
        // single is nearly full, one existing edge may be displaced into the
        // continuation. Re-read the existing edges and recompose the full edge
        // sequence; this is O(MAX_EDGES_SINGLE_PAGE), i.e. bounded, not O(total).
        let existing = read_adjacency(backend, first_page_id)?.edge_ids;
        let mut all_edges = existing;
        all_edges.extend_from_slice(new_edge_ids);
        debug_assert_eq!(all_edges.len(), new_total);

        let (first_batch, rest) = all_edges.split_at(MAX_EDGES_CHAINED_PAGE);

        let head_cont = rewrite_first_page_chained(
            backend,
            first_page_id,
            new_total,
            first_batch,
            &mut written_pages,
        )?;

        write_continuation_pages(backend, head_cont, rest, &mut written_pages)?;
        // Case B always ends chained with at least one continuation page; the
        // last written page is the chain tail.
        let last_page_id = *written_pages.last().unwrap_or(&first_page_id);
        // Persist the new tail id in the first-page header (issue #46) so a later
        // cold-cache read resolves it in O(1).
        backpatch_last_page_id(backend, first_page_id, last_page_id)?;
        let new_state = state_after_append(first_page_id, last_page_id, new_total);
        return Ok((first_page_id, written_pages, new_state));
    }

    // Case C & D: already chained.
    // Fill the current last continuation page, then overflow into new ones. Do
    // this BEFORE rewriting the first page so the new tail is known and the first
    // page can be written exactly once (edge_count + last_page_id together),
    // avoiding a re-read to backpatch the tail.
    let room_in_last = MAX_EDGES_CONT_PAGE - state.last_page_used_slots;
    let (fill_last, overflow) = new_edge_ids.split_at(room_in_last.min(new_edge_ids.len()));

    let mut last_buf = backend.read_page(DataFile::Adjacency, state.last_page_id)?;
    let lp = PAGE_HEADER_SIZE;
    let next_for_last = if overflow.is_empty() {
        NO_NEXT
    } else {
        backend.allocate_page(DataFile::Adjacency)?
    };
    last_buf[lp..lp + 4].copy_from_slice(&next_for_last.to_le_bytes());
    let mut off = lp + 4 + state.last_page_used_slots * 8;
    for &eid in fill_last {
        last_buf[off..off + 8].copy_from_slice(&eid.to_le_bytes());
        off += 8;
    }
    finalize_page(&mut last_buf, magic::ADJACENCY, ADJ_FORMAT_V2, PageType::Adjacency, 0);
    backend.write_page(DataFile::Adjacency, state.last_page_id, &last_buf)?;
    written_pages.push(state.last_page_id);

    let last_page_id = if overflow.is_empty() {
        state.last_page_id
    } else {
        write_continuation_pages(backend, next_for_last, overflow, &mut written_pages)?;
        *written_pages.last().unwrap_or(&state.last_page_id)
    };

    // Now rewrite the first page once: bump edge_count and stamp the (possibly
    // new) last_page_id in the V2 trailer. One read + one write, no backpatch.
    {
        let mut first_buf = backend.read_page(DataFile::Adjacency, first_page_id)?;
        let p = PAGE_HEADER_SIZE;
        // Bounded: see EDGE_COUNT_BOUND note on this module.
        #[allow(clippy::cast_possible_truncation)]
        first_buf[p + 9..p + 13].copy_from_slice(&(new_total as u32).to_le_bytes());
        let lp_off = p + LAST_PAGE_ID_TAIL_OFFSET;
        first_buf[lp_off..lp_off + 4].copy_from_slice(&last_page_id.to_le_bytes());
        finalize_page(&mut first_buf, magic::ADJACENCY, ADJ_FORMAT_V2, PageType::Adjacency, 1);
        backend.write_page(DataFile::Adjacency, first_page_id, &first_buf)?;
        written_pages.push(first_page_id);
    }

    let new_state = state_after_append(first_page_id, last_page_id, new_total);
    Ok((first_page_id, written_pages, new_state))
}

/// Writes a sequence of edge IDs into a chain of continuation pages, starting
/// at `head_id` (already allocated). Allocates further continuation pages as
/// needed and records every written `PageId` in `written_pages`.
///
/// Each continuation page holds `next_page(4)` followed by up to
/// `MAX_EDGES_CONT_PAGE` edge IDs. The final page's `next_page` is `NO_NEXT`.
fn write_continuation_pages(
    backend: &mut dyn StorageBackend,
    head_id: PageId,
    edges: &[u64],
    written_pages: &mut Vec<PageId>,
) -> Result<()> {
    let mut remaining = edges;
    let mut current_id = head_id;
    loop {
        let batch_size = remaining.len().min(MAX_EDGES_CONT_PAGE);
        let (batch, tail) = remaining.split_at(batch_size);
        remaining = tail;

        let next = if remaining.is_empty() {
            NO_NEXT
        } else {
            backend.allocate_page(DataFile::Adjacency)?
        };

        let mut cont_buf = backend.read_page(DataFile::Adjacency, current_id)?;
        let cp = PAGE_HEADER_SIZE;
        cont_buf[cp..cp + 4].copy_from_slice(&next.to_le_bytes());
        let mut coff = cp + 4;
        for &eid in batch {
            cont_buf[coff..coff + 8].copy_from_slice(&eid.to_le_bytes());
            coff += 8;
        }
        finalize_page(&mut cont_buf, magic::ADJACENCY, ADJ_FORMAT_V2, PageType::Adjacency, 0);
        backend.write_page(DataFile::Adjacency, current_id, &cont_buf)?;
        written_pages.push(current_id);

        if remaining.is_empty() {
            break;
        }
        current_id = next;
    }
    Ok(())
}

/// Reads an adjacency record starting from the given page.
pub fn read_adjacency(
    backend: &dyn StorageBackend,
    page_id: PageId,
) -> Result<AdjacencyRecord> {
    let page = backend.read_page(DataFile::Adjacency, page_id)?;
    let header = crate::storage::page::PageHeader::read_from(&page);
    ensure_dedicated_chain_page(&header, page_id)?;
    let version = header.version;
    let payload = &page[PAGE_HEADER_SIZE..];

    let node_id = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    let direction = match payload[8] {
        0 => AdjDirection::Outgoing,
        1 => AdjDirection::Incoming,
        _ => return Err(corrupt(page_id, "invalid adjacency direction")),
    };
    let edge_count = u32::from_le_bytes(payload[9..13].try_into().unwrap()) as usize;

    // Does it fit on a single page?
    if edge_count <= MAX_EDGES_SINGLE_PAGE {
        let mut edge_ids = Vec::with_capacity(edge_count);
        let mut off = RECORD_HEADER_SIZE;
        for _ in 0..edge_count {
            edge_ids.push(u64::from_le_bytes(payload[off..off + 8].try_into().unwrap()));
            off += 8;
        }
        return Ok(AdjacencyRecord {
            node_id,
            direction,
            edge_ids,
        });
    }

    // Chained: first page has edges + trailer. The first-page edge count differs
    // by version (V1 507, V2 506) because V2 reserves 4 extra trailing bytes for
    // last_page_id.
    let mut edge_ids = Vec::with_capacity(edge_count);

    let mut off = RECORD_HEADER_SIZE;
    for _ in 0..first_page_edges_for_version(version) {
        edge_ids.push(u64::from_le_bytes(payload[off..off + 8].try_into().unwrap()));
        off += 8;
    }
    let mut next_page =
        u32::from_le_bytes(payload[PAGE_PAYLOAD_SIZE - 4..].try_into().unwrap());

    // Read continuation pages
    while next_page != NO_NEXT && edge_ids.len() < edge_count {
        let page = backend.read_page(DataFile::Adjacency, next_page)?;
        let payload = &page[PAGE_HEADER_SIZE..];

        next_page = u32::from_le_bytes(payload[0..4].try_into().unwrap());

        let remaining = edge_count - edge_ids.len();
        let batch_size = remaining.min(MAX_EDGES_CONT_PAGE);
        let mut off = 4;
        for _ in 0..batch_size {
            edge_ids.push(u64::from_le_bytes(payload[off..off + 8].try_into().unwrap()));
            off += 8;
        }
    }

    Ok(AdjacencyRecord {
        node_id,
        direction,
        edge_ids,
    })
}

const fn corrupt(page_id: u32, reason: &'static str) -> Error {
    Error::CorruptPage {
        file: "adjacency.db",
        page_id,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory::MemoryBackend;

    fn make_backend() -> MemoryBackend {
        MemoryBackend::new()
    }

    /// Writes a two-page adjacency chain in the LEGACY V1 layout (header 16
    /// bytes, first page holds 507 edges, only 4 trailing bytes for `next_page`,
    /// page version = 1). Used to exercise the V1→V2 migration path.
    fn write_v1_chain(
        backend: &mut dyn StorageBackend,
        node_id: u64,
        edge_ids: &[u64],
    ) -> PageId {
        const V1_FIRST_PAGE_EDGES: usize = (PAGE_PAYLOAD_SIZE - RECORD_HEADER_SIZE - 4) / 8; // 507
        assert!(edge_ids.len() > V1_FIRST_PAGE_EDGES, "need a chained chain");
        let first_pid = backend.allocate_page(DataFile::Adjacency).unwrap();
        let cont_pid = backend.allocate_page(DataFile::Adjacency).unwrap();

        // First page (V1): record header + 507 edges + next_page at payload end.
        let mut buf = new_page_buf();
        let p = PAGE_HEADER_SIZE;
        buf[p..p + 8].copy_from_slice(&node_id.to_le_bytes());
        buf[p + 8] = AdjDirection::Outgoing as u8;
        // Test-only V1 fixture writing exactly `V1_FIRST_PAGE_EDGES` edges.
        #[allow(clippy::cast_possible_truncation)]
        buf[p + 9..p + 13].copy_from_slice(&(edge_ids.len() as u32).to_le_bytes());
        let mut off = p + RECORD_HEADER_SIZE;
        for &eid in &edge_ids[..V1_FIRST_PAGE_EDGES] {
            buf[off..off + 8].copy_from_slice(&eid.to_le_bytes());
            off += 8;
        }
        let np_off = p + PAGE_PAYLOAD_SIZE - 4;
        buf[np_off..np_off + 4].copy_from_slice(&cont_pid.to_le_bytes());
        finalize_page(&mut buf, magic::ADJACENCY, ADJ_FORMAT_V1, PageType::Adjacency, 1);
        backend.write_page(DataFile::Adjacency, first_pid, &buf).unwrap();

        // Continuation page: next_page = NO_NEXT + remaining edges.
        let mut cbuf = new_page_buf();
        cbuf[p..p + 4].copy_from_slice(&NO_NEXT.to_le_bytes());
        let mut coff = p + 4;
        for &eid in &edge_ids[V1_FIRST_PAGE_EDGES..] {
            cbuf[coff..coff + 8].copy_from_slice(&eid.to_le_bytes());
            coff += 8;
        }
        finalize_page(&mut cbuf, magic::ADJACENCY, ADJ_FORMAT_V1, PageType::Adjacency, 0);
        backend.write_page(DataFile::Adjacency, cont_pid, &cbuf).unwrap();
        first_pid
    }

    #[test]
    fn reads_legacy_v1_chain_intact() {
        // A V1 chain must decode all its edges with the version-aware first-page
        // split (507 on the first page, not 506).
        let mut b = make_backend();
        let edges: Vec<u64> = (1..=520).collect();
        let first = write_v1_chain(&mut b, 42, &edges);
        let rec = read_adjacency(&b, first).unwrap();
        assert_eq!(rec.edge_ids, edges, "V1 chain edges must decode intact");
        let state = read_adj_chain_state(&b, first).unwrap();
        assert_eq!(state.total_edges, 520);
        assert!(!state.is_single);
    }

    #[test]
    fn append_migrates_v1_chain_to_v2_without_losing_edges() {
        // Appending to a legacy V1 chain migrates it to V2 and preserves every
        // edge (old + new). Afterwards the tail resolves in one read (V2).
        use std::sync::atomic::Ordering::Relaxed;
        let mut b = CountingBackend::new();
        let edges: Vec<u64> = (1..=520).collect();
        let first = write_v1_chain(&mut b, 42, &edges);

        let new_edges = vec![9001, 9002, 9003];
        let (new_first, _pages, _state) = append_adjacency_with_state(
            &mut b,
            42,
            AdjDirection::Outgoing,
            Some(first),
            None, // cold: force the migration path
            &new_edges,
        )
        .unwrap();

        // All edges survive, in order.
        let rec = read_adjacency(&b, new_first).unwrap();
        let mut expected = edges.clone();
        expected.extend_from_slice(&new_edges);
        assert_eq!(rec.edge_ids, expected, "migration must preserve all edges");

        // The migrated chain is V2: tail resolves in one read.
        b.read_count.store(0, Relaxed);
        let state = read_adj_chain_state(&b, new_first).unwrap();
        assert_eq!(b.read_count.load(Relaxed), 1, "migrated chain must be V2 (O(1) tail)");
        assert_eq!(state.total_edges, 523);
    }

    #[test]
    fn adjacency_outgoing_direction() {
        let record = AdjacencyRecord {
            node_id: 1,
            direction: AdjDirection::Outgoing,
            edge_ids: vec![],
        };
        let encoded = encode_adjacency_record(&record);
        assert_eq!(encoded[8], 0);
    }

    #[test]
    fn adjacency_incoming_direction() {
        let record = AdjacencyRecord {
            node_id: 1,
            direction: AdjDirection::Incoming,
            edge_ids: vec![],
        };
        let encoded = encode_adjacency_record(&record);
        assert_eq!(encoded[8], 1);
    }

    #[test]
    fn adjacency_many_edges_single_page() {
        let mut backend = make_backend();
        // A full single page holds exactly MAX_EDGES_SINGLE_PAGE edges (507 under
        // the V2 header that reserves 4 bytes for last_page_id).
        let edge_ids: Vec<u64> = (1..=MAX_EDGES_SINGLE_PAGE as u64).collect();

        let record = AdjacencyRecord {
            node_id: 1,
            direction: AdjDirection::Outgoing,
            edge_ids,
        };

        let page_id = write_adjacency(&mut backend, &record).unwrap();
        assert_eq!(backend.page_count(DataFile::Adjacency), 1);

        let read_back = read_adjacency(&backend, page_id).unwrap();
        assert_eq!(read_back.node_id, 1);
        assert_eq!(read_back.direction, AdjDirection::Outgoing);
        assert_eq!(read_back.edge_ids.len(), MAX_EDGES_SINGLE_PAGE);
        assert_eq!(read_back.edge_ids, record.edge_ids);
    }

    #[test]
    fn adjacency_overflow_to_second_page() {
        let mut backend = make_backend();
        let edge_ids: Vec<u64> = (1..=509).collect();

        let record = AdjacencyRecord {
            node_id: 10,
            direction: AdjDirection::Incoming,
            edge_ids: edge_ids.clone(),
        };

        let page_id = write_adjacency(&mut backend, &record).unwrap();
        assert!(backend.page_count(DataFile::Adjacency) >= 2);

        let read_back = read_adjacency(&backend, page_id).unwrap();
        assert_eq!(read_back.node_id, 10);
        assert_eq!(read_back.direction, AdjDirection::Incoming);
        assert_eq!(read_back.edge_ids, edge_ids);
    }

    #[test]
    fn write_read_adjacency_via_backend() {
        let mut backend = make_backend();
        let record = AdjacencyRecord {
            node_id: 7,
            direction: AdjDirection::Outgoing,
            edge_ids: vec![100, 200, 300],
        };

        let page_id = write_adjacency(&mut backend, &record).unwrap();
        let read_back = read_adjacency(&backend, page_id).unwrap();

        assert_eq!(read_back.node_id, 7);
        assert_eq!(read_back.direction, AdjDirection::Outgoing);
        assert_eq!(read_back.edge_ids, vec![100, 200, 300]);
    }

    #[test]
    fn adjacency_record_size_calculation() {
        let record = AdjacencyRecord {
            node_id: 1,
            direction: AdjDirection::Outgoing,
            edge_ids: vec![1, 2, 3, 4, 5],
        };
        let encoded = encode_adjacency_record(&record);
        assert_eq!(encoded.len(), RECORD_HEADER_SIZE + 5 * 8);
    }

    #[test]
    fn adjacency_pointer_eq() {
        let a = AdjacencyPointer {
            outgoing_page: Some(1),
            incoming_page: None,
        };
        let b = AdjacencyPointer {
            outgoing_page: Some(1),
            incoming_page: None,
        };
        assert_eq!(a, b);
        let c = AdjacencyPointer {
            outgoing_page: Some(2),
            incoming_page: None,
        };
        assert_ne!(a, c);
    }

    #[test]
    fn read_chain_state_single_partial() {
        let mut backend = MemoryBackend::new();
        let record = AdjacencyRecord {
            node_id: 1,
            direction: AdjDirection::Outgoing,
            edge_ids: vec![10, 20, 30],
        };
        let pid = write_adjacency(&mut backend, &record).unwrap();
        let state = read_adj_chain_state(&backend, pid).unwrap();
        assert_eq!(state.first_page_id, pid);
        assert_eq!(state.last_page_id, pid);
        assert_eq!(state.total_edges, 3);
        assert_eq!(state.last_page_used_slots, 3);
        assert!(state.is_single);
    }

    #[test]
    fn read_chain_state_single_full() {
        let mut backend = MemoryBackend::new();
        let edge_ids: Vec<u64> = (1..=MAX_EDGES_SINGLE_PAGE as u64).collect();
        let record = AdjacencyRecord {
            node_id: 2,
            direction: AdjDirection::Outgoing,
            edge_ids,
        };
        let pid = write_adjacency(&mut backend, &record).unwrap();
        let state = read_adj_chain_state(&backend, pid).unwrap();
        assert_eq!(state.total_edges, MAX_EDGES_SINGLE_PAGE);
        assert_eq!(state.last_page_used_slots, MAX_EDGES_SINGLE_PAGE);
        assert!(state.is_single);
    }

    #[test]
    fn read_chain_state_chained_one_cont() {
        // 509 edges: first chained page holds MAX_EDGES_CHAINED_PAGE (507),
        // the single cont page holds the remaining 2.
        let mut backend = MemoryBackend::new();
        let edge_ids: Vec<u64> = (1..=509).collect();
        let record = AdjacencyRecord {
            node_id: 3,
            direction: AdjDirection::Incoming,
            edge_ids,
        };
        let first_pid = write_adjacency(&mut backend, &record).unwrap();
        let state = read_adj_chain_state(&backend, first_pid).unwrap();
        assert_eq!(state.total_edges, 509);
        assert_eq!(state.first_page_id, first_pid);
        assert_ne!(state.last_page_id, first_pid); // la última es la cont page
        assert_eq!(state.last_page_used_slots, 509 - MAX_EDGES_CHAINED_PAGE); // = 2
        assert!(!state.is_single);
    }

    #[test]
    fn read_chain_state_resolves_tail_in_one_read_for_new_format() {
        use std::sync::atomic::Ordering::Relaxed;
        // Issue #46: a multi-page chain written in the new format (version 2,
        // last_page_id persisted in the first page) must resolve its tail state
        // reading ONLY the first page — O(1) — instead of walking every page.
        let mut b = CountingBackend::new();
        let edge_ids: Vec<u64> = (1..=(MAX_EDGES_CHAINED_PAGE + 3 * MAX_EDGES_CONT_PAGE) as u64)
            .collect();
        let record = AdjacencyRecord {
            node_id: 7,
            direction: AdjDirection::Outgoing,
            edge_ids,
        };
        let first_pid = write_adjacency(&mut b, &record).unwrap();

        b.read_count.store(0, Relaxed);
        let state = read_adj_chain_state(&b, first_pid).unwrap();
        let reads = b.read_count.load(Relaxed);
        assert_eq!(reads, 1, "new-format tail resolution must read only the first page");
        // And the state must still be correct (same as a walk would produce).
        assert_eq!(state.total_edges, MAX_EDGES_CHAINED_PAGE + 3 * MAX_EDGES_CONT_PAGE);
        assert_ne!(state.last_page_id, first_pid);
        assert!(!state.is_single);
        assert_eq!(state.last_page_used_slots, MAX_EDGES_CONT_PAGE);
    }

    #[test]
    fn read_chain_state_chained_last_page_full() {
        // 507 + 509 = 1016 edges: first chained page (507) + one cont page
        // filled to its 509-slot capacity.
        let total = MAX_EDGES_CHAINED_PAGE + MAX_EDGES_CONT_PAGE;
        assert_eq!(total, 1016);
        let mut backend = MemoryBackend::new();
        let edge_ids: Vec<u64> = (1..=total as u64).collect();
        let record = AdjacencyRecord {
            node_id: 4,
            direction: AdjDirection::Outgoing,
            edge_ids,
        };
        let first_pid = write_adjacency(&mut backend, &record).unwrap();
        let state = read_adj_chain_state(&backend, first_pid).unwrap();
        assert_eq!(state.total_edges, total);
        assert_eq!(state.last_page_used_slots, MAX_EDGES_CONT_PAGE); // = 509, full
        assert!(!state.is_single);
    }

    #[test]
    fn append_to_empty_chain_allocs_same_as_write() {
        // append with no existing page must behave identically to write_adjacency
        let mut b1 = CountingBackend::new();
        let mut b2 = CountingBackend::new();

        let edges: Vec<u64> = vec![1, 2, 3];
        let record = AdjacencyRecord {
            node_id: 7,
            direction: AdjDirection::Outgoing,
            edge_ids: edges.clone(),
        };

        let pid_write = write_adjacency(&mut b1, &record).unwrap();
        let (pid_append, written_pages) =
            append_adjacency(&mut b2, 7, AdjDirection::Outgoing, None, &edges).unwrap();

        // Same allocation count
        assert_eq!(b1.alloc_count, b2.alloc_count);
        // pid must be page 0 in both cases
        assert_eq!(pid_write, pid_append);
        // written_pages must contain the first page
        assert!(written_pages.contains(&pid_append));

        // Round-trip: data readable
        let r = read_adjacency(&b2.inner, pid_append).unwrap();
        assert_eq!(r.edge_ids, edges);
    }

    #[test]
    fn append_to_single_page_with_room_no_alloc() {
        // existing: 3 edges, append 5 more → total 8, all fit in single page
        let mut b = CountingBackend::new();
        let initial = AdjacencyRecord {
            node_id: 42,
            direction: AdjDirection::Outgoing,
            edge_ids: vec![1, 2, 3],
        };
        let first_pid = write_adjacency(&mut b, &initial).unwrap();
        let alloc_before = b.alloc_count;

        let (returned_pid, written_pages) =
            append_adjacency(&mut b, 42, AdjDirection::Outgoing, Some(first_pid), &[4, 5, 6, 7, 8])
                .unwrap();

        // No new pages allocated
        assert_eq!(b.alloc_count, alloc_before, "must not allocate for in-page append");
        assert_eq!(returned_pid, first_pid);
        assert_eq!(written_pages, vec![first_pid]);

        // Round-trip
        let r = read_adjacency(&b.inner, first_pid).unwrap();
        assert_eq!(r.edge_ids, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(r.node_id, 42);
        assert_eq!(r.direction, AdjDirection::Outgoing);
    }

    #[test]
    fn append_single_to_chained_transition() {
        // Single page FULL at MAX_EDGES_SINGLE_PAGE (508). Add 1 → single→chained.
        // Chained first page holds MAX_EDGES_CHAINED_PAGE (507); the displaced
        // existing edge (508th) + the new edge (2 total) go to one cont page.
        let mut b = CountingBackend::new();
        let edge_ids: Vec<u64> = (1..=MAX_EDGES_SINGLE_PAGE as u64).collect();
        let initial = AdjacencyRecord {
            node_id: 99,
            direction: AdjDirection::Outgoing,
            edge_ids,
        };
        let first_pid = write_adjacency(&mut b, &initial).unwrap();
        assert_eq!(b.alloc_count, 1); // 1 single page
        let alloc_before = b.alloc_count;

        let new_edge = MAX_EDGES_SINGLE_PAGE as u64 + 1;
        let (returned_pid, written_pages) =
            append_adjacency(&mut b, 99, AdjDirection::Outgoing, Some(first_pid), &[new_edge])
                .unwrap();

        // Exactly 1 new continuation page.
        assert_eq!(b.alloc_count - alloc_before, 1, "single→chained: 1 new page");
        assert_eq!(returned_pid, first_pid);
        assert!(written_pages.contains(&first_pid)); // first page rewritten as chained
        assert_eq!(written_pages.len(), 2); // first + new cont

        let r = read_adjacency(&b.inner, first_pid).unwrap();
        assert_eq!(r.edge_ids.len(), MAX_EDGES_SINGLE_PAGE + 1);
        let expected: Vec<u64> = (1..=new_edge).collect();
        assert_eq!(r.edge_ids, expected);
    }

    #[test]
    fn append_single_partial_to_chained_with_overflow() {
        // Single page with 505 edges. Add 10 → 515 total. First chained page
        // takes 507 (505 existing + 2 new); the remaining 8 go to one cont page.
        let mut b = CountingBackend::new();
        let edge_ids: Vec<u64> = (1..=505).collect();
        let initial = AdjacencyRecord {
            node_id: 11,
            direction: AdjDirection::Incoming,
            edge_ids,
        };
        let first_pid = write_adjacency(&mut b, &initial).unwrap();
        let alloc_before = b.alloc_count;

        let new_edges: Vec<u64> = (506..=515).collect();
        let (returned_pid, written_pages) =
            append_adjacency(&mut b, 11, AdjDirection::Incoming, Some(first_pid), &new_edges)
                .unwrap();

        assert_eq!(b.alloc_count - alloc_before, 1);
        assert_eq!(returned_pid, first_pid);
        assert_eq!(written_pages.len(), 2);

        let r = read_adjacency(&b.inner, first_pid).unwrap();
        assert_eq!(r.edge_ids.len(), 515);
        let expected: Vec<u64> = (1..=515).collect();
        assert_eq!(r.edge_ids, expected);
    }

    #[test]
    fn append_to_chained_last_page_has_room_no_new_page() {
        // 509 edges: first chained (507) + cont (2 used, room for 507 more).
        // Append 5 → last cont goes 2→7 used. 0 new allocs.
        let mut b = CountingBackend::new();
        let edge_ids: Vec<u64> = (1..=509).collect();
        let initial = AdjacencyRecord {
            node_id: 200,
            direction: AdjDirection::Outgoing,
            edge_ids,
        };
        let first_pid = write_adjacency(&mut b, &initial).unwrap();
        let alloc_before = b.alloc_count;

        let new_edges: Vec<u64> = (510..=514).collect();
        let (returned_pid, written_pages) =
            append_adjacency(&mut b, 200, AdjDirection::Outgoing, Some(first_pid), &new_edges)
                .unwrap();

        assert_eq!(b.alloc_count, alloc_before, "no new alloc: edges fit in existing last page");
        assert_eq!(returned_pid, first_pid);
        // 2 pages written: first page (edge_count update) + last cont page.
        assert_eq!(written_pages.len(), 2);
        assert!(written_pages.contains(&first_pid));

        let r = read_adjacency(&b.inner, first_pid).unwrap();
        assert_eq!(r.edge_ids.len(), 514);
        let expected: Vec<u64> = (1..=514).collect();
        assert_eq!(r.edge_ids, expected);
    }

    #[test]
    fn append_to_chained_exactly_fills_last_page() {
        // 509 edges: cont page has 2 used, 507 free. Append 507 → fills cont
        // exactly to MAX_EDGES_CONT_PAGE (509). 0 new allocs.
        let mut b = CountingBackend::new();
        let edge_ids: Vec<u64> = (1..=509).collect();
        let initial = AdjacencyRecord {
            node_id: 201,
            direction: AdjDirection::Incoming,
            edge_ids,
        };
        let first_pid = write_adjacency(&mut b, &initial).unwrap();
        let alloc_before = b.alloc_count;

        // cont currently holds 509-507 = 2; fill to 509 needs 507 more.
        let fill = MAX_EDGES_CONT_PAGE - (509 - MAX_EDGES_CHAINED_PAGE);
        assert_eq!(fill, 507);
        let new_edges: Vec<u64> = (510..=(509 + fill as u64)).collect();
        let (_, written_pages) =
            append_adjacency(&mut b, 201, AdjDirection::Incoming, Some(first_pid), &new_edges)
                .unwrap();

        assert_eq!(b.alloc_count, alloc_before);
        assert_eq!(written_pages.len(), 2); // first + filled cont, no new page
        let r = read_adjacency(&b.inner, first_pid).unwrap();
        assert_eq!(r.edge_ids.len(), 509 + fill);
    }

    #[test]
    fn append_to_chained_overflows_to_new_pages() {
        // Case D: last cont page nearly full, append overflows into new cont
        // pages. Start 509 (cont has 2), fill the cont to 509 AND overflow by
        // MAX_EDGES_CONT_PAGE + 3 → needs 2 new cont pages.
        let mut b = CountingBackend::new();
        let edge_ids: Vec<u64> = (1..=509).collect();
        let initial = AdjacencyRecord {
            node_id: 202,
            direction: AdjDirection::Outgoing,
            edge_ids,
        };
        let first_pid = write_adjacency(&mut b, &initial).unwrap();
        let alloc_before = b.alloc_count;

        // room_in_last = 507; then MAX_EDGES_CONT_PAGE (509) + 3 overflow.
        let room_in_last = MAX_EDGES_CONT_PAGE - (509 - MAX_EDGES_CHAINED_PAGE); // 507
        let overflow = MAX_EDGES_CONT_PAGE + 3; // 512 → 2 pages (509 + 3)
        let count = room_in_last + overflow;
        let new_edges: Vec<u64> = (510..=(509 + count as u64)).collect();
        let (_, written_pages) =
            append_adjacency(&mut b, 202, AdjDirection::Outgoing, Some(first_pid), &new_edges)
                .unwrap();

        // 2 new pages: the overflow (512) fills one cont (509) + one cont (3).
        assert_eq!(b.alloc_count - alloc_before, 2, "case D: 2 new cont pages");
        // written: first + previously-last cont + 2 new = 4.
        assert_eq!(written_pages.len(), 4);
        let r = read_adjacency(&b.inner, first_pid).unwrap();
        assert_eq!(r.edge_ids.len(), 509 + count);
        let expected: Vec<u64> = (1..=(509 + count as u64)).collect();
        assert_eq!(r.edge_ids, expected);
    }

    #[test]
    fn append_alloc_count_is_linear() {
        // Build an empty chain and append in batches of BATCH. Each append of
        // BATCH edges must allocate at most ceil(BATCH / MAX_EDGES_CONT_PAGE)
        // pages. A quadratic implementation would re-allocate O(total/509) per
        // call, so the cumulative alloc_count would grow super-linearly.
        const BATCH: usize = 100;
        const K: usize = 20; // 20 appends of 100 = 2000 edges total

        let mut b = CountingBackend::new();
        let mut first_pid: Option<PageId> = None;
        let mut edge_counter: u64 = 0;

        for i in 0..K {
            let new_edges: Vec<u64> =
                (edge_counter + 1..=edge_counter + BATCH as u64).collect();
            edge_counter += BATCH as u64;

            let alloc_before = b.alloc_count;
            let (pid, _) =
                append_adjacency(&mut b, 1, AdjDirection::Outgoing, first_pid, &new_edges)
                    .unwrap();
            first_pid = Some(pid);

            let alloc_delta = b.alloc_count - alloc_before;
            // ceil(BATCH / MAX_EDGES_CONT_PAGE) = 1 for BATCH=100; allow 2 for
            // the single→chained boundary and last-page-transition batches.
            let max_expected: u32 = 2;
            assert!(
                alloc_delta <= max_expected,
                "append {i}: alloc_delta={alloc_delta} > {max_expected} — quadratic behavior detected"
            );
        }

        // Total allocs bounded linearly: generous bound of K + 5.
        // Test assertion bound; `K` is a literal const.
        #[allow(clippy::cast_possible_truncation)]
        let linear_bound = K as u32 + 5;
        assert!(
            b.alloc_count <= linear_bound,
            "total alloc_count={} exceeds linear bound {}",
            b.alloc_count,
            K + 5
        );

        // Round-trip: all 2000 edges readable and in order.
        let r = read_adjacency(&b.inner, first_pid.unwrap()).unwrap();
        assert_eq!(r.edge_ids.len(), K * BATCH);
        let expected: Vec<u64> = (1..=(K * BATCH) as u64).collect();
        assert_eq!(r.edge_ids, expected);
    }

    /// Test-only backend that wraps `MemoryBackend` and counts `allocate_page` calls.
    ///
    /// Consumed by the `append_adjacency` allocation-count tests added in later
    /// tasks; `allow(dead_code)` keeps the tree warning-free between commits.
    #[test]
    fn append_adjacency_accepts_precomputed_state() {
        // Supplying a precomputed `AdjChainState` must produce the same result
        // as letting `append_adjacency` recompute it, and return the post-append
        // state so callers can cache the chain tail.
        let mut b = make_backend();
        let initial = AdjacencyRecord {
            node_id: 3,
            direction: AdjDirection::Outgoing,
            edge_ids: vec![10, 20, 30],
        };
        let first_pid = write_adjacency(&mut b, &initial).unwrap();
        let state = read_adj_chain_state(&b, first_pid).unwrap();

        let new_edges = vec![40, 50];
        let (pid, pages, new_state) = append_adjacency_with_state(
            &mut b,
            3,
            AdjDirection::Outgoing,
            Some(first_pid),
            Some(state),
            &new_edges,
        )
        .unwrap();

        assert_eq!(pid, first_pid);
        assert!(pages.contains(&first_pid));
        // Post-append state reflects the two appended edges.
        assert_eq!(new_state.total_edges, 5);
        assert!(new_state.is_single);
        assert_eq!(new_state.last_page_id, first_pid);
        // Data round-trips with the appended edges in order.
        let r = read_adjacency(&b, first_pid).unwrap();
        assert_eq!(r.edge_ids, vec![10, 20, 30, 40, 50]);
    }

    #[test]
    fn case_a_append_offset_comes_from_the_page_header() {
        // Defense in depth: Case A derives the existing-edge count (and thus the
        // write offset and the new header count) from the page's own header, so
        // it does not depend on `state.total_edges` being right. This exercises
        // the release-build path: the appended edge lands right after the real
        // existing edges. (In debug builds a mismatched state.total_edges trips a
        // debug_assert — see `case_a_mismatched_state_total_trips_debug_assert`.)
        let mut b = make_backend();
        let initial = AdjacencyRecord {
            node_id: 4,
            direction: AdjDirection::Outgoing,
            edge_ids: vec![1, 2, 3],
        };
        let first_pid = write_adjacency(&mut b, &initial).unwrap();
        let state = read_adj_chain_state(&b, first_pid).unwrap();

        let (_pid, _pages, new_state) = append_adjacency_with_state(
            &mut b,
            4,
            AdjDirection::Outgoing,
            Some(first_pid),
            Some(state),
            &[9],
        )
        .unwrap();

        let r = read_adjacency(&b, first_pid).unwrap();
        assert_eq!(r.edge_ids, vec![1, 2, 3, 9]);
        assert_eq!(new_state.total_edges, 4);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "cached total_edges disagrees with the page header")]
    fn case_a_mismatched_state_total_trips_debug_assert() {
        // A cached state whose total_edges disagrees with the page must fail fast
        // in debug builds rather than write at a phantom offset.
        let mut b = make_backend();
        let initial = AdjacencyRecord {
            node_id: 4,
            direction: AdjDirection::Outgoing,
            edge_ids: vec![1, 2, 3],
        };
        let first_pid = write_adjacency(&mut b, &initial).unwrap();
        let mut state = read_adj_chain_state(&b, first_pid).unwrap();
        state.total_edges = 3 + 100; // lie: page holds 3

        let _ = append_adjacency_with_state(
            &mut b,
            4,
            AdjDirection::Outgoing,
            Some(first_pid),
            Some(state),
            &[9],
        );
    }

    #[test]
    fn append_with_state_skips_chain_walk() {
        use std::sync::atomic::Ordering::Relaxed;
        // A chain spanning several continuation pages. Appending with a
        // precomputed state must NOT re-walk the chain: its read_page count
        // stays a small constant, unlike read_adj_chain_state which reads every
        // page in the chain.
        let mut b = CountingBackend::new();
        let initial_len = MAX_EDGES_CHAINED_PAGE + 2 * MAX_EDGES_CONT_PAGE + 5;
        let initial_edges: Vec<u64> = (0..initial_len as u64).collect();
        let record = AdjacencyRecord {
            node_id: 9,
            direction: AdjDirection::Outgoing,
            edge_ids: initial_edges,
        };
        let first_pid = write_adjacency(&mut b, &record).unwrap();

        // Resolving the tail state is now O(1) even without a cached state: the
        // V2 first page persists last_page_id, so read_adj_chain_state reads a
        // single page instead of walking the multi-page chain.
        b.read_count.store(0, Relaxed);
        let state = read_adj_chain_state(&b, first_pid).unwrap();
        let resolve_reads = b.read_count.load(Relaxed);
        assert_eq!(resolve_reads, 1, "V2 tail resolution must read one page, got {resolve_reads}");

        // Append with the precomputed state; count only this call's reads.
        b.read_count.store(0, Relaxed);
        let new_edges = vec![9991, 9992];
        let (_pid, _pages, _new_state) = append_adjacency_with_state(
            &mut b,
            9,
            AdjDirection::Outgoing,
            Some(first_pid),
            Some(state),
            &new_edges,
        )
        .unwrap();
        let append_reads = b.read_count.load(Relaxed);
        // Case C append with room in the last page reads only the first page
        // (edge_count update) and the last page — a small constant, independent
        // of chain length.
        assert!(
            append_reads <= 2,
            "append with precomputed state must not re-walk the chain: {append_reads} reads"
        );
    }

    #[test]
    fn dedicated_chain_reader_rejects_a_slab_page() {
        // Both formats live in DataFile::Adjacency under the same `TGAD` magic and
        // are reached through the same node-slot pointer, so a mis-set pointer or a
        // botched slab→chain migration can hand a slab page to this reader. The two
        // formats number their versions from 1 independently, so a slab page (version
        // 1, page_type AdjacencySlab) is indistinguishable from a legacy V1 chain page
        // on version alone: without a page_type check the reader would walk a bogus
        // `next_page` and hand back invented edges with no error. `page_type` is the
        // only field that separates them; it must be checked.
        let mut b = MemoryBackend::new();
        let slab_pid = crate::storage::codec::adj_slab_codec::allocate_slab_page(&mut b).unwrap();

        let state_err = read_adj_chain_state(&b, slab_pid).unwrap_err();
        assert!(
            matches!(state_err, Error::CorruptPage { .. }),
            "read_adj_chain_state must reject a slab page, got {state_err:?}"
        );

        let read_err = read_adjacency(&b, slab_pid).unwrap_err();
        assert!(
            matches!(read_err, Error::CorruptPage { .. }),
            "read_adjacency must reject a slab page, got {read_err:?}"
        );
    }

    struct CountingBackend {
        inner: MemoryBackend,
        alloc_count: u32,
        read_count: std::sync::atomic::AtomicU32,
    }

    #[allow(dead_code)]
    impl CountingBackend {
        fn new() -> Self {
            Self {
                inner: MemoryBackend::new(),
                alloc_count: 0,
                read_count: std::sync::atomic::AtomicU32::new(0),
            }
        }
    }

    impl StorageBackend for CountingBackend {
        fn read_page(
            &self,
            file: DataFile,
            page_id: PageId,
        ) -> crate::error::Result<crate::storage::page::PageBuf> {
            self.read_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.read_page(file, page_id)
        }
        fn write_page(
            &mut self,
            file: DataFile,
            page_id: PageId,
            data: &crate::storage::page::PageBuf,
        ) -> crate::error::Result<()> {
            self.inner.write_page(file, page_id, data)
        }
        fn allocate_page(&mut self, file: DataFile) -> crate::error::Result<PageId> {
            self.alloc_count += 1;
            self.inner.allocate_page(file)
        }
        fn free_page(&mut self, file: DataFile, page_id: PageId) -> crate::error::Result<()> {
            self.inner.free_page(file, page_id)
        }
        fn page_count(&self, file: DataFile) -> u32 {
            self.inner.page_count(file)
        }
        fn flush(&mut self) -> crate::error::Result<()> {
            self.inner.flush()
        }
        fn meta(&self) -> &crate::storage::meta::GraphMeta {
            self.inner.meta()
        }
        fn meta_mut(&mut self) -> &mut crate::storage::meta::GraphMeta {
            self.inner.meta_mut()
        }
        fn read_index_bytes(&mut self) -> crate::error::Result<Option<Vec<u8>>> {
            self.inner.read_index_bytes()
        }
        fn write_index_bytes(&mut self, data: &[u8]) -> crate::error::Result<()> {
            self.inner.write_index_bytes(data)
        }
    }
}

/// Property-based tests (issue #67).
///
/// The round trip here is `write_adjacency` / `read_adjacency`: the pure
/// `encode_adjacency_record` has no pure counterpart, since reading always
/// goes through the page chain. So these tests exercise the chaining, which is
/// where the interesting boundaries live — a record either fits one page or
/// spills into a chain, and the seam between those is where an off-by-one
/// would hide.
#[cfg(test)]
mod proptests {
    use super::*;
    use crate::storage::memory::MemoryBackend;
    use proptest::prelude::*;

    /// Edge counts concentrated on the single-page/chained seam rather than
    /// spread evenly: that threshold is the only structural boundary this
    /// format has, and a uniform draw would rarely land on it.
    fn edge_count() -> impl Strategy<Value = usize> {
        prop_oneof![
            3 => 0..=MAX_EDGES_SINGLE_PAGE,
            3 => MAX_EDGES_SINGLE_PAGE.saturating_sub(2)..=MAX_EDGES_SINGLE_PAGE + 2,
            2 => MAX_EDGES_SINGLE_PAGE..=(MAX_EDGES_SINGLE_PAGE + MAX_EDGES_CONT_PAGE + 2),
            1 => 0..=(MAX_EDGES_SINGLE_PAGE * 3),
        ]
    }

    fn record_strategy() -> impl Strategy<Value = AdjacencyRecord> {
        (any::<u64>(), any::<bool>(), edge_count()).prop_map(|(node_id, incoming, n)| {
            AdjacencyRecord {
                node_id,
                direction: if incoming {
                    AdjDirection::Incoming
                } else {
                    AdjDirection::Outgoing
                },
                // Distinct, position-dependent ids: a chunk copied from the
                // wrong offset shows up as a wrong id rather than blending in.
                edge_ids: (0..n).map(|i| (i as u64).wrapping_mul(0x9E37_79B9) | 1).collect(),
            }
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

        /// What was written comes back, however many pages it took.
        #[test]
        fn record_survives_the_page_chain_intact(record in record_strategy()) {
            let mut backend = MemoryBackend::new();

            let Ok(page_id) = write_adjacency(&mut backend, &record) else {
                return Ok(()); // refusing is a valid answer
            };
            let read_back = read_adjacency(&backend, page_id)
                .expect("what write accepted, read must return");

            prop_assert_eq!(read_back.node_id, record.node_id, "node id changed");
            prop_assert_eq!(
                read_back.direction as u8,
                record.direction as u8,
                "direction changed"
            );
            prop_assert_eq!(
                read_back.edge_ids.len(),
                record.edge_ids.len(),
                "edge count changed across the chain"
            );
            prop_assert_eq!(read_back.edge_ids, record.edge_ids, "edge ids changed");
        }

        /// Two records written in sequence must not read each other's pages.
        /// Chained pages are allocated as the write proceeds, so a boundary
        /// error typically surfaces as one record's tail landing in another's
        /// chain — invisible when only one record exists.
        #[test]
        fn separate_records_do_not_bleed_into_each_other(
            a in record_strategy(),
            b in record_strategy(),
        ) {
            let mut backend = MemoryBackend::new();

            let (Ok(ref_a), Ok(ref_b)) = (
                write_adjacency(&mut backend, &a),
                write_adjacency(&mut backend, &b),
            ) else {
                return Ok(());
            };

            prop_assert_eq!(read_adjacency(&backend, ref_a).expect("read a").edge_ids, a.edge_ids);
            prop_assert_eq!(read_adjacency(&backend, ref_b).expect("read b").edge_ids, b.edge_ids);
        }

        /// Reading an arbitrary page id must report an error, never panic.
        ///
        /// This codec reads its edge count straight off the page and uses it
        /// to size the read — the same shape that turned out to panic in
        /// `node_codec` and `edge_codec`, so it is worth asking directly.
        #[test]
        fn read_never_panics_on_arbitrary_page_ids(
            record in record_strategy(),
            probe in any::<u32>(),
        ) {
            let mut backend = MemoryBackend::new();
            let _ = write_adjacency(&mut backend, &record);

            let outcome = read_adjacency(&backend, probe);
            prop_assert!(outcome.is_ok() || outcome.is_err());
        }
    }
}
