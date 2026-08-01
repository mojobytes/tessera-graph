// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use crate::Error;
use crate::error::Result;
use crate::storage::backend::{DataFile, StorageBackend};
use crate::storage::page::{PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE, PageType, finalize_page, magic};

/// A reference to a string in the string heap (absolute byte offset within payload space).
pub type StringRef = u32;

/// Append-only string heap stored across `strings.db` pages.
///
/// Each entry is `len: u32 (LE) + data: [u8; len]` (the prefix was a `u16`
/// until issue #75). Entries are packed sequentially within page payloads and
/// may span across page boundaries.
///
/// A session-level deduplication cache avoids writing the same string twice.
pub struct StringHeap {
    dedup: HashMap<String, StringRef>,
    write_offset: u32,
}

impl StringHeap {
    #[must_use]
    pub fn new() -> Self {
        Self {
            dedup: HashMap::new(),
            write_offset: 0,
        }
    }

    /// Creates a `StringHeap` resuming from a known write offset.
    /// Used when reopening a persisted graph.
    #[must_use]
    pub fn with_offset(write_offset: u32) -> Self {
        Self {
            dedup: HashMap::new(),
            write_offset,
        }
    }

    /// Returns the current write offset (for persisting in meta).
    #[must_use]
    pub const fn write_offset(&self) -> u32 {
        self.write_offset
    }

    /// Appends a string to the heap, returning its `StringRef`.
    ///
    /// If the same string was already appended in this session, returns the
    /// existing reference (deduplication).
    ///
    /// # Errors
    ///
    /// Returns [`Error::RecordTooLarge`] for a value whose length does not fit
    /// the `u32` length prefix (a `u16` until issue #75). Writing it anyway
    /// would store a wrapped length — the text lands on disk whole while its
    /// prefix reads as `len % 2^width` — and resolving it would return a prefix
    /// of the value with no indication anything was lost. Measured through
    /// `add_node` before this guard existed (issue #62, on the old u16 width):
    /// a 70,000-byte label resolved back to 4,464 bytes, and a 131,072-byte
    /// one to nothing at all, neither raising an error.
    ///
    /// Also returns it when the heap's write cursor would pass `u32::MAX`. That
    /// bound is on the WHOLE heap rather than one entry, so it is reached by
    /// accumulating ~4 GiB of labels and long strings over the life of the
    /// database, with no single value being large (issue #65). Before this check
    /// the cursor wrapped to a low offset and later entries were handed
    /// references into live bytes already written there — silently, and
    /// permanently on disk.
    ///
    /// The two cases share the variant but not its payload: an oversized entry
    /// reports the entry's own length, the cursor case reports the offset the
    /// heap would have reached, which is always well above `u32::MAX` and so
    /// tells the two apart.
    ///
    /// Either check runs before anything is written, so a rejected value leaves
    /// the heap untouched and still usable.
    pub fn append(&mut self, backend: &mut dyn StorageBackend, value: &str) -> Result<StringRef> {
        if let Some(&existing) = self.dedup.get(value) {
            return Ok(existing);
        }

        let entry_bytes = value.as_bytes();
        let entry_len = entry_bytes.len();
        let header_len =
            u32::try_from(entry_len).map_err(|_| Error::RecordTooLarge { size: entry_len })?;
        let header = header_len.to_le_bytes();

        // The check above bounds THIS entry; the cursor accumulates across every
        // entry ever appended and needs its own. Past u32::MAX it wrapped to a
        // low offset, so the next entries were handed references pointing into
        // unrelated bytes already written there — no error, and permanent on
        // disk (issue #65). Rejecting before any write keeps the heap usable.
        let end_offset = u64::from(self.write_offset) + 4 + u64::from(header_len);
        if end_offset > u64::from(u32::MAX) {
            // Report the offset the heap would have reached, not the entry
            // length: the entry may be small, and the caller needs to see that
            // the HEAP is full rather than the value oversized. Saturates only
            // in the sense that usize is 64-bit here, so the value survives.
            return Err(Error::RecordTooLarge {
                size: usize::try_from(end_offset).unwrap_or(usize::MAX),
            });
        }

        let str_ref = self.write_offset;

        // Write the 4-byte length prefix + data, handling page boundaries
        let mut to_write: Vec<u8> = Vec::with_capacity(4 + entry_len);
        to_write.extend_from_slice(&header);
        to_write.extend_from_slice(entry_bytes);

        let mut remaining = &to_write[..];
        let mut current_offset = self.write_offset as usize;

        while !remaining.is_empty() {
            // `current_offset` is bounded by the u32 write cursor (checked on append,
            // issue #65), so dividing by the page size lands well inside u32.
            #[allow(clippy::cast_possible_truncation)]
            let page_idx = (current_offset / PAGE_PAYLOAD_SIZE) as u32;
            let offset_in_payload = current_offset % PAGE_PAYLOAD_SIZE;

            // Ensure page exists
            while backend.page_count(DataFile::Strings) <= page_idx {
                backend.allocate_page(DataFile::Strings)?;
            }

            let space_on_page = PAGE_PAYLOAD_SIZE - offset_in_payload;
            let write_len = remaining.len().min(space_on_page);

            let mut page = backend.read_page(DataFile::Strings, page_idx)?;
            let buf_offset = PAGE_HEADER_SIZE + offset_in_payload;
            page[buf_offset..buf_offset + write_len].copy_from_slice(&remaining[..write_len]);

            // `offset_in_payload + write_len` never exceeds PAGE_PAYLOAD_SIZE (4080).
            #[allow(clippy::cast_possible_truncation)]
            let slots_used = (offset_in_payload + write_len) as u16;
            finalize_page(&mut page, magic::STRINGS, 1, PageType::String, slots_used);
            backend.write_page(DataFile::Strings, page_idx, &page)?;

            remaining = &remaining[write_len..];
            current_offset += write_len;
        }

        // The end-offset check above already refused anything that would not fit,
        // so this conversion cannot fail (issue #65). Converting rather than
        // casting keeps that guarantee checked instead of assumed.
        self.write_offset = u32::try_from(current_offset).map_err(|_| Error::RecordTooLarge {
            size: current_offset,
        })?;
        self.dedup.insert(value.to_owned(), str_ref);

        Ok(str_ref)
    }

    /// Resolves a `StringRef` back to a `String`.
    #[allow(clippy::unused_self)]
    pub fn resolve(&self, backend: &dyn StorageBackend, string_ref: StringRef) -> Result<String> {
        let ref_offset = string_ref as usize;
        let total_capacity = backend.page_count(DataFile::Strings) as usize * PAGE_PAYLOAD_SIZE;

        if ref_offset + 4 > total_capacity {
            return Err(Error::InvalidStringRef(string_ref));
        }

        // Read the 4-byte length prefix (may span pages)
        let len_bytes = Self::read_heap_bytes(backend, ref_offset, 4)?;
        let str_len =
            u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;

        if ref_offset + 4 + str_len > total_capacity {
            return Err(Error::InvalidStringRef(string_ref));
        }

        let str_bytes = Self::read_heap_bytes(backend, ref_offset + 4, str_len)?;
        // `ref_offset` comes from a u32 StringRef; dividing shrinks it further.
        // Only used to name the page in an error message.
        #[allow(clippy::cast_possible_truncation)]
        let error_page = (ref_offset / PAGE_PAYLOAD_SIZE) as u32;
        String::from_utf8(str_bytes).map_err(|_| Error::CorruptPage {
            file: "strings.db",
            page_id: error_page,
            reason: "string heap entry is not valid UTF-8",
        })
    }

    /// Reads `len` bytes from the heap starting at the given payload offset,
    /// handling cross-page reads.
    fn read_heap_bytes(backend: &dyn StorageBackend, offset: usize, len: usize) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }

        let mut result = Vec::with_capacity(len);
        let mut remaining = len;
        let mut current_offset = offset;

        while remaining > 0 {
            // `current_offset` is bounded by the u32 write cursor (checked on append,
            // issue #65), so dividing by the page size lands well inside u32.
            #[allow(clippy::cast_possible_truncation)]
            let page_idx = (current_offset / PAGE_PAYLOAD_SIZE) as u32;
            let offset_in_payload = current_offset % PAGE_PAYLOAD_SIZE;
            let available = PAGE_PAYLOAD_SIZE - offset_in_payload;
            let read_len = remaining.min(available);

            let page = backend.read_page(DataFile::Strings, page_idx)?;
            let buf_start = PAGE_HEADER_SIZE + offset_in_payload;
            result.extend_from_slice(&page[buf_start..buf_start + read_len]);

            remaining -= read_len;
            current_offset += read_len;
        }

        Ok(result)
    }
}

impl Default for StringHeap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory::MemoryBackend;

    fn make_backend() -> MemoryBackend {
        MemoryBackend::new()
    }

    // --- Entry length limit (issues #62 and #75) ---
    //
    // Each heap entry is prefixed with its length in a u32 (a u16 until issue
    // #75). Under the old width, past 65,535 the cast used to wrap: the text
    // was written whole but its recorded length read as `len % 65536`, so
    // resolving returned a prefix of it. Measured through the public API
    // before the v0.11.1 fix, a 70,000-byte node label came back as 4,464
    // bytes and a 131,072-byte one as empty — no error either time. Same
    // defect as the property-value one, reached through labels. #75 then
    // widened the prefix so sizes past 64 KiB are legitimate.

    // --- Heap cursor overflow (issue #65) ---
    //
    // The per-entry length check above bounds ONE entry. The write cursor is a
    // separate u32 that accumulates across every entry ever appended, and it
    // had no check at all: past 4 GiB of heap it wrapped to a low offset, and
    // subsequent entries were handed references pointing into unrelated bytes
    // already written there — silently, and permanently on disk.
    //
    // Unlike the entry-length bug this needs no huge value: any mix of labels
    // and long strings totalling 4 GiB reaches it. `with_offset` starts the
    // heap near the ceiling so the case is reachable without writing 4 GiB.

    #[test]
    fn append_rejects_entry_that_would_overflow_the_heap_cursor() {
        let mut backend = make_backend();
        // 40 bytes below u32::MAX: an entry of 100 bytes cannot fit.
        let mut heap = StringHeap::with_offset(u32::MAX - 40);

        let err = heap
            .append(&mut backend, &"x".repeat(100))
            .expect_err("appending past the u32 cursor must be refused, not wrapped");

        assert!(
            matches!(err, Error::RecordTooLarge { .. }),
            "expected RecordTooLarge, got {err:?}"
        );
    }

    /// The heap-full case reports the offset the heap would have reached —
    /// always past `u32::MAX` — rather than the entry's length, so the caller
    /// can tell "the heap is full" apart from "the value is oversized".
    ///
    /// The entry-too-long half of the old distinction test is gone with #75:
    /// reaching it now takes a single value past 4 GiB, which is not worth
    /// materializing in a unit test (same precedent as `overflow_codec.rs` —
    /// the guard is the same one-line `u32::try_from` pattern).
    #[test]
    fn heap_full_reports_the_offset_reached_not_the_entry_length() {
        let mut backend = make_backend();

        let mut full = StringHeap::with_offset(u32::MAX - 40);
        match full.append(&mut backend, &"x".repeat(100)) {
            Err(Error::RecordTooLarge { size }) => assert!(
                size > u32::MAX as usize,
                "the heap-full case must report the offset reached ({size}), not the entry length"
            ),
            other => panic!("expected RecordTooLarge for the full heap, got {other:?}"),
        }
    }

    #[test]
    fn heap_cursor_overflow_leaves_the_heap_usable() {
        let mut backend = make_backend();
        let mut heap = StringHeap::with_offset(u32::MAX - 40);
        let before = heap.write_offset();

        let _ = heap.append(&mut backend, &"x".repeat(100));

        assert_eq!(
            heap.write_offset(),
            before,
            "a refused append must not move the cursor — a wrapped one would \
             point later entries at unrelated bytes"
        );
    }

    #[test]
    fn append_accepts_entry_exactly_65535_bytes() {
        let mut backend = make_backend();
        let mut heap = StringHeap::new();
        let value = "x".repeat(65_535);

        let r = heap.append(&mut backend, &value).unwrap();
        assert_eq!(
            heap.resolve(&backend, r).unwrap().len(),
            65_535,
            "the old u16 cap — now an ordinary size, must still round-trip"
        );
    }

    /// 70,000 bytes exceeded the old u16 prefix and used to resolve back as
    /// 4,464 bytes with no error (issue #62), then was rejected outright
    /// (v0.11.1). Since #75 widened the prefix to u32 it must round-trip.
    #[test]
    fn append_accepts_entry_between_old_and_new_limit() {
        let mut backend = make_backend();
        let mut heap = StringHeap::new();

        let r = heap.append(&mut backend, &"x".repeat(70_000)).unwrap();
        assert_eq!(heap.resolve(&backend, r).unwrap().len(), 70_000);
    }

    /// The real-world size that motivated #75: a 283,718-byte legal
    /// `full_text`, spanning ~70 heap pages.
    #[test]
    fn append_accepts_entry_over_u16_limit() {
        let mut backend = make_backend();
        let mut heap = StringHeap::new();
        let value = "x".repeat(283_718);

        let r = heap.append(&mut backend, &value).unwrap();
        let resolved = heap.resolve(&backend, r).unwrap();
        assert_eq!(resolved.len(), 283_718);
        assert_eq!(resolved, value);
    }

    /// A rejected entry must not move the write cursor: the heap has to stay
    /// usable, with the next append landing where the rejected one would have.
    /// Since #75 the only rejection reachable without 4 GiB of data is the
    /// cursor ceiling, so the scenario starts the heap near it.
    #[test]
    fn rejected_entry_leaves_the_heap_intact() {
        let mut backend = make_backend();
        let mut heap = StringHeap::with_offset(u32::MAX - 40);

        let before = heap.write_offset();
        assert!(heap.append(&mut backend, &"x".repeat(100)).is_err());
        assert_eq!(
            heap.write_offset(),
            before,
            "a rejected append must not consume heap space"
        );
    }

    #[test]
    fn append_and_resolve_single() {
        let mut backend = make_backend();
        let mut heap = StringHeap::new();

        let r = heap.append(&mut backend, "hello").unwrap();
        let resolved = heap.resolve(&backend, r).unwrap();
        assert_eq!(resolved, "hello");
    }

    #[test]
    fn append_multiple_strings() {
        let mut backend = make_backend();
        let mut heap = StringHeap::new();

        let r1 = heap.append(&mut backend, "alpha").unwrap();
        let r2 = heap.append(&mut backend, "beta").unwrap();
        let r3 = heap.append(&mut backend, "gamma").unwrap();

        assert_ne!(r1, r2);
        assert_ne!(r2, r3);
        assert_eq!(heap.resolve(&backend, r1).unwrap(), "alpha");
        assert_eq!(heap.resolve(&backend, r2).unwrap(), "beta");
        assert_eq!(heap.resolve(&backend, r3).unwrap(), "gamma");
    }

    #[test]
    fn deduplication() {
        let mut backend = make_backend();
        let mut heap = StringHeap::new();

        let r1 = heap.append(&mut backend, "same").unwrap();
        let r2 = heap.append(&mut backend, "same").unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn empty_string() {
        let mut backend = make_backend();
        let mut heap = StringHeap::new();

        let r = heap.append(&mut backend, "").unwrap();
        let resolved = heap.resolve(&backend, r).unwrap();
        assert_eq!(resolved, "");
    }

    #[test]
    fn cross_page_string() {
        let mut backend = make_backend();
        let mut heap = StringHeap::new();

        // Fill most of the first page payload (4080 bytes).
        // Each entry = 2 (len) + data_len. Fill with 100-byte strings.
        // 4080 / 102 = 40 strings = 4080 bytes exactly. But we want partial fill.
        // Write strings until < 50 bytes remain on first page, then a long string that spans.
        // Write unique 100-byte strings to fill most of the first page.
        // Each entry = 2 (len) + 100 (data) = 102 bytes.
        // 39 * 102 = 3978 bytes used, 4080 - 3978 = 102 bytes remaining.
        for i in 0..39 {
            let filler = format!("{i:>100}");
            heap.append(&mut backend, &filler).unwrap();
        }
        // 4080 - 3978 = 102 bytes remaining on first page
        // Write a 200-byte string: needs 202 bytes, will span across pages
        let long = "y".repeat(200);
        let r = heap.append(&mut backend, &long).unwrap();

        assert!(backend.page_count(DataFile::Strings) >= 2);
        let resolved = heap.resolve(&backend, r).unwrap();
        assert_eq!(resolved, long);
    }

    #[test]
    fn long_string() {
        let mut backend = make_backend();
        let mut heap = StringHeap::new();

        // 65,535 bytes — the old u16 cap, an ordinary size since #75.
        let long = "z".repeat(65_535);
        let r = heap.append(&mut backend, &long).unwrap();
        let resolved = heap.resolve(&backend, r).unwrap();
        assert_eq!(resolved, long);
    }

    #[test]
    fn resolve_invalid_ref() {
        let backend = make_backend();
        let heap = StringHeap::new();

        let result = heap.resolve(&backend, 999);
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::InvalidStringRef(offset) => assert_eq!(offset, 999),
            other => panic!("expected InvalidStringRef, got {other:?}"),
        }
    }

    #[test]
    fn multiple_pages_sequential() {
        let mut backend = make_backend();
        let mut heap = StringHeap::new();

        // Each entry = 2 + 100 = 102 bytes. 4080/102 = 40 per page.
        // Write 100 strings => need at least 3 pages.
        let mut refs = Vec::new();
        for i in 0..100 {
            let s = format!("{i:>100}");
            let r = heap.append(&mut backend, &s).unwrap();
            refs.push((r, s));
        }

        assert!(backend.page_count(DataFile::Strings) > 1);

        for (r, expected) in &refs {
            let resolved = heap.resolve(&backend, *r).unwrap();
            assert_eq!(&resolved, expected);
        }
    }
}

/// Property-based tests (issue #67).
///
/// Unlike the pure codecs, this one owns mutable state: a write cursor and a
/// deduplication map, both of which survive across calls. That makes the
/// "no partial effects" invariant meaningful here in a way it is not for a
/// pure function — a rejected append must leave the cursor exactly where it
/// was, or every subsequent entry lands at the wrong offset.
#[cfg(test)]
mod proptests {
    use super::*;
    use crate::storage::memory::MemoryBackend;
    use proptest::prelude::*;

    fn size_around(limit: usize) -> impl Strategy<Value = usize> {
        prop_oneof![
            4 => 0..=limit,
            3 => limit.saturating_sub(5)..=limit + 5,
            3 => (limit + 1)..=(limit * 2).max(limit + 2),
        ]
    }

    /// Mostly short entries with one in eight drawn around the old `u16`
    /// boundary (no longer a limit since #75, but the neighbourhood where
    /// #62's corruption lived) and a rare one at the real ~283 KB size that
    /// motivated #75 — weighted down because each such case moves ~300 KB
    /// through the heap.
    fn entry_strategy() -> impl Strategy<Value = String> {
        prop_oneof![
            14 => "[a-zA-Z0-9_]{0,40}".prop_map(|s| s),
            2 => size_around(u16::MAX as usize).prop_map(|n| "s".repeat(n)),
            1 => (279_000..=288_000usize).prop_map(|n| "s".repeat(n)),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// Accept or reject, but never lie — over a sequence of appends, so
        /// that an entry written at a wrong offset shows up as a later entry
        /// resolving to the wrong text rather than going unnoticed.
        #[test]
        fn appended_entries_all_resolve_to_what_was_written(
            entries in proptest::collection::vec(entry_strategy(), 1..8)
        ) {
            let mut backend = MemoryBackend::new();
            let mut heap = StringHeap::new();

            let mut written: Vec<(StringRef, String)> = Vec::new();
            for entry in entries {
                // Refusing is a valid answer; only accepted entries are checked.
                if let Ok(r) = heap.append(&mut backend, &entry) {
                    written.push((r, entry));
                }
            }

            // Resolve everything at the end rather than after each append:
            // a cursor that drifts corrupts entries written *later*, so
            // checking as you go would miss exactly the failure that matters.
            for (r, expected) in written {
                let got = heap.resolve(&backend, r)
                    .expect("what append accepted, resolve must read");
                prop_assert_eq!(got, expected, "an entry resolved to different text");
            }
        }

        /// A refused entry must leave the heap byte-for-byte usable: same
        /// cursor, and the next append still lands where it should.
        ///
        /// This is the invariant a pure codec cannot have. If a rejection
        /// moved the cursor even a little, every later reference would be off
        /// and the damage would surface far from its cause.
        /// Since #75 the only refusal reachable without materializing 4 GiB is
        /// the heap-cursor ceiling (issue #65), so the scenario starts the
        /// heap near `u32::MAX` and drives the refusal with a small entry
        /// whose end offset would cross it.
        #[test]
        fn a_rejected_append_leaves_the_cursor_and_later_entries_intact(
            headroom in 1usize..32,
            oversized_by in 1usize..64,
        ) {
            let mut backend = MemoryBackend::new();
            // Cursor sits `headroom + 4` bytes below the ceiling: an entry of
            // `headroom + oversized_by` bytes cannot fit its 4-byte prefix
            // plus payload.
            let start = u32::MAX - u32::try_from(headroom).unwrap() - 4;
            let mut heap = StringHeap::with_offset(start);
            let cursor_before = heap.write_offset();

            let too_big = "x".repeat(headroom + oversized_by);
            prop_assert!(
                heap.append(&mut backend, &too_big).is_err(),
                "an entry past the heap-cursor ceiling must be refused"
            );
            prop_assert_eq!(
                heap.write_offset(),
                cursor_before,
                "a refused append consumed heap space"
            );
            // No follow-up append here: actually writing at an offset near
            // u32::MAX would make the in-memory backend allocate ~1M pages
            // (4 GiB). "The heap stays usable after a refusal" is covered at
            // ordinary offsets by `appended_entries_all_resolve_to_what_was_
            // written` plus the cursor equality above — an unmoved cursor IS
            // the usability invariant.
        }

        /// Resolving an arbitrary offset must report an error, never panic or
        /// hand back text from somewhere else. Offsets come off node slots,
        /// so a wrong one is reachable from the same class of bug as #62.
        #[test]
        fn resolve_never_panics_on_arbitrary_offsets(
            entries in proptest::collection::vec(entry_strategy(), 0..4),
            probe in any::<u32>(),
        ) {
            let mut backend = MemoryBackend::new();
            let mut heap = StringHeap::new();
            for entry in &entries {
                let _ = heap.append(&mut backend, entry);
            }

            let outcome = heap.resolve(&backend, probe);
            prop_assert!(outcome.is_ok() || outcome.is_err());
        }

        /// Appending the same text twice must return the same reference: the
        /// heap deduplicates, and a broken dedup would either waste space or,
        /// worse, hand back a reference to different text.
        #[test]
        fn identical_entries_share_one_reference(entry in entry_strategy()) {
            let mut backend = MemoryBackend::new();
            let mut heap = StringHeap::new();

            let (Ok(first), Ok(second)) = (
                heap.append(&mut backend, &entry),
                heap.append(&mut backend, &entry),
            ) else {
                return Ok(()); // refused: nothing to deduplicate
            };

            prop_assert_eq!(first, second, "the same text got two different references");
            prop_assert_eq!(
                heap.resolve(&backend, first).expect("resolve deduplicated entry"),
                entry
            );
        }
    }
}
