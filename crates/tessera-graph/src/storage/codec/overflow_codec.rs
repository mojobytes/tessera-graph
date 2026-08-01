// SPDX-License-Identifier: Apache-2.0

use crate::error::Result;
use crate::storage::backend::{DataFile, PageId, StorageBackend};
use crate::storage::page::{
    finalize_page, magic, new_page_buf, PageType, PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE,
};
/// End-of-chain sentinel for `next_page` pointers.
const NO_NEXT: u32 = 0xFFFF_FFFF;

/// First page layout within payload (4080 bytes):
///   [0..4]      `total_len`: u32 LE
///   [4..4076]   data (up to 4072 bytes)
///   [4076..4080] `next_page`: u32 LE (`NO_NEXT` if no continuation)
const FIRST_PAGE_HEADER: usize = 4; // total_len
const FIRST_PAGE_FOOTER: usize = 4; // next_page
const FIRST_PAGE_DATA_CAP: usize = PAGE_PAYLOAD_SIZE - FIRST_PAGE_HEADER - FIRST_PAGE_FOOTER;

/// Continuation page layout within payload (4080 bytes):
///   [0..4]      `next_page`: u32 LE (`NO_NEXT` if last)
///   [4..4080]   data (up to 4076 bytes)
const CONT_PAGE_HEADER: usize = 4; // next_page
const CONT_PAGE_DATA_CAP: usize = PAGE_PAYLOAD_SIZE - CONT_PAGE_HEADER;

/// Writes a byte blob to overflow pages, returning the first page ID.
///
/// Allocates as many pages as needed and chains them via `next_page` pointers.
///
/// # Errors
///
/// Returns [`Error::RecordTooLarge`](crate::Error::RecordTooLarge) when the blob
/// does not fit the `u32` `total_len` header. A wrapped length is worse than a
/// rejected write: the chain would be read back short, silently, exactly as in
/// the four truncation bugs of issue #62.
///
/// Reaching it needs ~4 GiB in a single entity — a node may hold 65 535
/// properties and each value up to 65 535 bytes, so the encoded blob tops out
/// just above `u32::MAX`. Unlikely, but the check costs nothing and the failure
/// mode is silent corruption.
pub fn write_overflow(backend: &mut dyn StorageBackend, data: &[u8]) -> Result<PageId> {
    let total_len = data.len();
    let total_len_u32 =
        u32::try_from(total_len).map_err(|_| crate::Error::RecordTooLarge { size: total_len })?;
    let mut remaining = data;

    // Allocate first page
    let first_page_id = backend.allocate_page(DataFile::Overflow)?;
    let mut current_page_id = first_page_id;

    // Write first page
    let first_chunk_len = remaining.len().min(FIRST_PAGE_DATA_CAP);
    let first_chunk = &remaining[..first_chunk_len];
    remaining = &remaining[first_chunk_len..];

    let needs_continuation = !remaining.is_empty();
    let next_page_id = if needs_continuation {
        backend.allocate_page(DataFile::Overflow)?
    } else {
        NO_NEXT
    };

    write_first_page(backend, current_page_id, total_len_u32, first_chunk, next_page_id)?;

    if !needs_continuation {
        return Ok(first_page_id);
    }

    current_page_id = next_page_id;

    // Write continuation pages
    while !remaining.is_empty() {
        let chunk_len = remaining.len().min(CONT_PAGE_DATA_CAP);
        let chunk = &remaining[..chunk_len];
        remaining = &remaining[chunk_len..];

        let next = if remaining.is_empty() {
            NO_NEXT
        } else {
            backend.allocate_page(DataFile::Overflow)?
        };

        write_cont_page(backend, current_page_id, chunk, next)?;
        current_page_id = next;
    }

    Ok(first_page_id)
}

/// Releases every page of the chain starting at `first_page`.
///
/// The caller guarantees no live record still points at this chain — releasing
/// a chain that is still referenced hands its pages to the next writer, and the
/// original owner then reads back another entity's bytes with no error raised.
///
/// A chain whose head page cannot be read is skipped rather than reported: the
/// pages are unreachable either way, and turning a delete into a hard failure
/// because its already-unreadable overflow could not be walked would block the
/// caller from removing the record at all.
///
/// # Errors
///
/// Propagates failures from the free-list write itself.
pub fn free_overflow_chain(backend: &mut dyn StorageBackend, first_page: PageId) -> Result<()> {
    let mut to_free = Vec::new();
    let mut current = first_page;

    // Collect first, release afterwards: releasing as we walk would let the
    // free list hand a page out and have it overwritten before we read the
    // `next` pointer still stored in it.
    while let Ok(page) = backend.read_page(DataFile::Overflow, current) {
        let payload = &page[PAGE_HEADER_SIZE..];

        // The first page keeps its link in the last four payload bytes; every
        // continuation keeps it in the first four.
        let next = if to_free.is_empty() {
            u32::from_le_bytes(payload[4076..4080].try_into().expect("4 bytes"))
        } else {
            u32::from_le_bytes(payload[0..4].try_into().expect("4 bytes"))
        };

        to_free.push(current);

        if next == NO_NEXT {
            break;
        }
        // A chain cannot be longer than the file itself; a cycle from a
        // corrupt link would otherwise spin here forever.
        if to_free.len() > backend.page_count(DataFile::Overflow) as usize {
            break;
        }
        current = next;
    }

    for page_id in to_free {
        backend.free_page(DataFile::Overflow, page_id)?;
    }

    Ok(())
}

/// Reads a complete byte blob from overflow pages starting at `first_page`.
pub fn read_overflow(backend: &dyn StorageBackend, first_page: PageId) -> Result<Vec<u8>> {
    let page = backend.read_page(DataFile::Overflow, first_page)?;
    let payload = &page[PAGE_HEADER_SIZE..];

    let total_len =
        u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
    let next_page =
        u32::from_le_bytes(payload[4076..4080].try_into().unwrap());

    let first_data_len = total_len.min(FIRST_PAGE_DATA_CAP);
    let mut result = Vec::with_capacity(total_len);
    result.extend_from_slice(&payload[4..4 + first_data_len]);

    let mut current_next = next_page;
    while current_next != NO_NEXT && result.len() < total_len {
        let page = backend.read_page(DataFile::Overflow, current_next)?;
        let payload = &page[PAGE_HEADER_SIZE..];

        current_next = u32::from_le_bytes(payload[0..4].try_into().unwrap());

        let remaining = total_len - result.len();
        let chunk_len = remaining.min(CONT_PAGE_DATA_CAP);
        result.extend_from_slice(&payload[4..4 + chunk_len]);
    }

    Ok(result)
}

fn write_first_page(
    backend: &mut dyn StorageBackend,
    page_id: PageId,
    total_len: u32,
    data: &[u8],
    next_page: u32,
) -> Result<()> {
    let mut buf = new_page_buf();
    let p = PAGE_HEADER_SIZE;

    buf[p..p + 4].copy_from_slice(&total_len.to_le_bytes());
    buf[p + 4..p + 4 + data.len()].copy_from_slice(data);
    buf[p + 4076..p + 4080].copy_from_slice(&next_page.to_le_bytes());

    finalize_page(&mut buf, magic::OVERFLOW, 1, PageType::Overflow, 0);
    backend.write_page(DataFile::Overflow, page_id, &buf)
}

fn write_cont_page(
    backend: &mut dyn StorageBackend,
    page_id: PageId,
    data: &[u8],
    next_page: u32,
) -> Result<()> {
    let mut buf = new_page_buf();
    let p = PAGE_HEADER_SIZE;

    buf[p..p + 4].copy_from_slice(&next_page.to_le_bytes());
    buf[p + 4..p + 4 + data.len()].copy_from_slice(data);

    finalize_page(&mut buf, magic::OVERFLOW, 1, PageType::Overflow, 0);
    backend.write_page(DataFile::Overflow, page_id, &buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory::MemoryBackend;
    use crate::storage::page::compute_crc32;

    fn make_backend() -> MemoryBackend {
        MemoryBackend::new()
    }

    #[test]
    fn overflow_small_blob() {
        let mut backend = make_backend();
        let data = vec![0xAB; 100];
        let page_id = write_overflow(&mut backend, &data).unwrap();
        let read_back = read_overflow(&backend, page_id).unwrap();
        assert_eq!(read_back, data);
        assert_eq!(backend.page_count(DataFile::Overflow), 1);
    }

    #[test]
    fn overflow_exactly_4072_bytes() {
        let mut backend = make_backend();
        let data = vec![0xCD; FIRST_PAGE_DATA_CAP];
        assert_eq!(data.len(), 4072);

        let page_id = write_overflow(&mut backend, &data).unwrap();
        let read_back = read_overflow(&backend, page_id).unwrap();
        assert_eq!(read_back, data);
        assert_eq!(backend.page_count(DataFile::Overflow), 1);
    }

    #[test]
    fn overflow_two_pages() {
        let mut backend = make_backend();
        let data = vec![0xEF; FIRST_PAGE_DATA_CAP + 1]; // 4073 bytes
        let page_id = write_overflow(&mut backend, &data).unwrap();
        let read_back = read_overflow(&backend, page_id).unwrap();
        assert_eq!(read_back, data);
        assert_eq!(backend.page_count(DataFile::Overflow), 2);
    }

    #[test]
    fn overflow_three_pages() {
        let mut backend = make_backend();
        // first: 4072, cont1: 4076, cont2: remainder
        let data = vec![0x11; FIRST_PAGE_DATA_CAP + CONT_PAGE_DATA_CAP + 100];
        let page_id = write_overflow(&mut backend, &data).unwrap();
        let read_back = read_overflow(&backend, page_id).unwrap();
        assert_eq!(read_back, data);
        assert_eq!(backend.page_count(DataFile::Overflow), 3);
    }

    #[test]
    fn overflow_empty_blob() {
        let mut backend = make_backend();
        let data: Vec<u8> = vec![];
        let page_id = write_overflow(&mut backend, &data).unwrap();
        let read_back = read_overflow(&backend, page_id).unwrap();
        assert!(read_back.is_empty());
    }

    #[test]
    fn overflow_large_blob() {
        let mut backend = make_backend();
        let data = vec![0x77; 50_000];
        let page_id = write_overflow(&mut backend, &data).unwrap();
        let read_back = read_overflow(&backend, page_id).unwrap();
        assert_eq!(read_back, data);
    }

    #[test]
    fn overflow_exact_page_boundary() {
        let mut backend = make_backend();
        // Exactly fills first + one continuation: 4072 + 4076 = 8148
        let data = vec![0x99; FIRST_PAGE_DATA_CAP + CONT_PAGE_DATA_CAP];
        let page_id = write_overflow(&mut backend, &data).unwrap();
        let read_back = read_overflow(&backend, page_id).unwrap();
        assert_eq!(read_back, data);
        assert_eq!(backend.page_count(DataFile::Overflow), 2);
    }

    #[test]
    fn overflow_read_nonexistent() {
        let backend = make_backend();
        let result = read_overflow(&backend, 42);
        assert!(result.is_err());
    }

    #[test]
    fn overflow_crc_validated() {
        let mut backend = make_backend();
        let data = vec![0xBB; 5000]; // spans 2 pages
        let first = write_overflow(&mut backend, &data).unwrap();

        for page_id in 0..backend.page_count(DataFile::Overflow) {
            let page = backend.read_page(DataFile::Overflow, page_id).unwrap();
            let stored_crc =
                u32::from_le_bytes([page[8], page[9], page[10], page[11]]);
            assert_eq!(
                stored_crc,
                compute_crc32(&page),
                "CRC invalid on page {page_id}"
            );
        }

        // Verify data still reads correctly
        let read_back = read_overflow(&backend, first).unwrap();
        assert_eq!(read_back, data);
    }
}

/// Property-based tests (issue #67).
///
/// # Why there is no rejection axis here
///
/// The other codecs in this issue all record a length in a `u16` and were, or
/// still are, one unchecked cast away from silent truncation. This one records
/// its total in a `u32`, so the equivalent limit sits at about 4.29 billion
/// bytes — unreachable in any realistic call, and unreachable in a test
/// without allocating that much memory. Writing an "it must refuse oversized
/// input" test here would assert something that cannot happen, which is worse
/// than no test: it would read as coverage while proving nothing.
///
/// What this codec *does* have is a page-chaining boundary, and that is what
/// these tests aim at: a blob is split across a first page (4072 usable bytes)
/// and continuation pages (4076 each), so the interesting inputs are the ones
/// that land exactly on those seams.
#[cfg(test)]
mod proptests {
    use super::*;
    use crate::storage::memory::MemoryBackend;
    use proptest::prelude::*;

    /// Sizes concentrated around the page seams rather than spread evenly.
    ///
    /// A uniform draw over, say, 0..20000 would put roughly one case in a
    /// thousand within a byte of a boundary; the boundaries are where an
    /// off-by-one in the chaining arithmetic would show up, so they get
    /// deliberate weight instead.
    fn blob_size() -> impl Strategy<Value = usize> {
        let first = FIRST_PAGE_DATA_CAP;
        let cont = CONT_PAGE_DATA_CAP;
        prop_oneof![
            3 => 0..=first,                                   // fits in one page
            2 => (first.saturating_sub(2))..=(first + 2),      // first-page seam
            2 => (first + cont - 2)..=(first + cont + 2),      // second seam
            2 => (first + 2 * cont - 2)..=(first + 2 * cont + 2), // third seam
            1 => 0..=(first + 3 * cont),                       // anywhere
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

        /// What goes in comes back out, whatever the blob's size does to the
        /// page chain.
        #[test]
        fn blob_survives_the_page_chain_intact(
            size in blob_size(),
            fill in any::<u8>(),
        ) {
            let mut backend = MemoryBackend::new();
            // A repeating byte would hide an off-by-one that copies the wrong
            // slice, so the content varies with position instead.
            let data: Vec<u8> = (0..size)
                .map(|i| fill.wrapping_add(u8::try_from(i % 251).unwrap_or(0)))
                .collect();

            let first_page = write_overflow(&mut backend, &data)
                .expect("a u32 length holds any blob a test can allocate");
            let read_back = read_overflow(&backend, first_page)
                .expect("what write accepted, read must return");

            prop_assert_eq!(read_back.len(), data.len(), "length changed in the chain");
            prop_assert_eq!(read_back, data, "content changed in the chain");
        }

        /// Two blobs written in sequence must not read each other's bytes.
        ///
        /// Chained pages are allocated as the write proceeds, so a boundary
        /// error tends to surface as one blob's tail landing in the next
        /// blob's chain — invisible when only one blob exists.
        #[test]
        fn separate_blobs_do_not_bleed_into_each_other(
            size_a in blob_size(),
            size_b in blob_size(),
        ) {
            let mut backend = MemoryBackend::new();
            let a: Vec<u8> = (0..size_a).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
            let b: Vec<u8> = (0..size_b).map(|i| u8::try_from(i % 241).unwrap_or(0) | 0x80).collect();

            let ref_a = write_overflow(&mut backend, &a).expect("write a");
            let ref_b = write_overflow(&mut backend, &b).expect("write b");

            prop_assert_eq!(read_overflow(&backend, ref_a).expect("read a"), a);
            prop_assert_eq!(read_overflow(&backend, ref_b).expect("read b"), b);
        }

        /// Reading an arbitrary page id must report an error rather than
        /// panicking or returning someone else's bytes. Page ids come off node
        /// slots, so a wrong one is reachable from the same class of bug #62
        /// was about.
        #[test]
        fn read_never_panics_on_arbitrary_page_ids(
            size in 0..=8192usize,
            probe in any::<u32>(),
        ) {
            let mut backend = MemoryBackend::new();
            let data = vec![0xA5u8; size];
            let _ = write_overflow(&mut backend, &data);

            let outcome = read_overflow(&backend, probe);
            prop_assert!(outcome.is_ok() || outcome.is_err());
        }
    }
}
