// SPDX-License-Identifier: Apache-2.0

//! Free-page directory: which pages of a data file may be handed out again.
//!
//! # Why this exists
//!
//! Page allocation used to be a counter that only ever increased. Nothing could
//! record that a page had stopped being referenced, which produced three
//! defects at once: an entity whose properties overflowed took a whole
//! 4096-byte page even for 39 bytes of data, updating that entity wrote a new
//! chain and abandoned the old one, and deleting it released nothing. Measured
//! consequence: 2 000 nodes updated 20 times each held 164 MB of overflow pages
//! for roughly 78 KB of live data, growing without bound from there.
//!
//! # Shape
//!
//! Free ids are batched into directory pages rather than threaded one-per-page
//! as a linked list through the free pages themselves. Both designs cost the
//! same in metadata, but batching makes the common operations touch **one**
//! page instead of one per id: releasing a 40-page chain appends 40 ids to a
//! single directory page, and the count of what is available is read from
//! metadata without any page access at all. A per-page linked list would need
//! 40 page writes for the same release, and could not answer "how many are
//! free" without walking the whole chain.
//!
//! A directory page is drawn from the very file it describes, so it is
//! distinguishable from live data only by its [`PageType::FreeDirectory`]
//! stamp. Every read verifies that stamp before trusting the contents.
//!
//! # Layout within the 4080-byte payload
//!
//! ```text
//! [0..4]      next_page: u32 LE   (FREE_DIRECTORY_EMPTY when this is the last)
//! [4..8]      len: u32 LE         (how many ids follow)
//! [8..4080]   ids: u32 LE each    (up to ENTRIES_PER_PAGE of them)
//! ```

use crate::error::Result;
use crate::storage::meta::FREE_DIRECTORY_EMPTY;
use crate::storage::page::{
    PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE, PageBuf, PageHeader, PageType, finalize_page,
    new_page_buf,
};

/// Format version stamped on a directory page.
const FREE_DIRECTORY_VERSION: u16 = 1;

/// Payload offset of the link to the next directory page.
const OFF_NEXT: usize = 0;
/// Payload offset of the entry count.
const OFF_LEN: usize = 4;
/// Payload offset of the first entry.
const OFF_ENTRIES: usize = 8;

/// How many page ids one directory page holds.
///
/// At 1018 per page, a file would need over a million free pages (4 GB of
/// reclaimable space) before the directory itself spans more than one page.
pub const ENTRIES_PER_PAGE: usize = (PAGE_PAYLOAD_SIZE - OFF_ENTRIES) / 4;

/// One decoded directory page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeDirectoryPage {
    /// Next directory page, or [`FREE_DIRECTORY_EMPTY`] if this is the last.
    pub next: u32,
    /// The reusable page ids this page records.
    pub entries: Vec<u32>,
}

impl FreeDirectoryPage {
    /// A directory page with no entries and no successor.
    #[must_use]
    pub const fn empty() -> Self {
        Self { next: FREE_DIRECTORY_EMPTY, entries: Vec::new() }
    }

    /// Whether another id can be appended without spilling to a new page.
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.entries.len() < ENTRIES_PER_PAGE
    }
}

/// Encodes a directory page.
///
/// `file_magic` is the stamp of the data file this directory lives in: a
/// directory page occupies a page of the very file it describes, so it must
/// carry that file's magic to pass the buffer pool's per-file validation. The
/// [`PageType::FreeDirectory`] stamp is what distinguishes it from live data
/// within the file.
///
/// # Errors
///
/// Returns [`Error::RecordTooLarge`](crate::Error::RecordTooLarge) if more
/// entries are supplied than a page can hold. Silently truncating would drop
/// page ids, which does not corrupt data but does leak the pages permanently —
/// exactly the failure this module exists to remove.
pub fn encode(page: &FreeDirectoryPage, file_magic: [u8; 4]) -> Result<PageBuf> {
    if page.entries.len() > ENTRIES_PER_PAGE {
        return Err(crate::Error::RecordTooLarge { size: page.entries.len() });
    }

    let mut buf = new_page_buf();
    let p = PAGE_HEADER_SIZE;

    buf[p + OFF_NEXT..p + OFF_NEXT + 4].copy_from_slice(&page.next.to_le_bytes());

    // Length fits a u32 by the check above (ENTRIES_PER_PAGE is ~1018).
    #[allow(clippy::cast_possible_truncation)]
    let len = page.entries.len() as u32;
    buf[p + OFF_LEN..p + OFF_LEN + 4].copy_from_slice(&len.to_le_bytes());

    for (i, id) in page.entries.iter().enumerate() {
        let off = p + OFF_ENTRIES + i * 4;
        buf[off..off + 4].copy_from_slice(&id.to_le_bytes());
    }

    finalize_page(&mut buf, file_magic, FREE_DIRECTORY_VERSION, PageType::FreeDirectory, 0);
    Ok(buf)
}

/// Decodes a directory page.
///
/// # Errors
///
/// Returns [`Error::CorruptPage`](crate::Error::CorruptPage) if the recorded
/// entry count exceeds what a page can hold. That check is what stops a page
/// of live data — or a stale directory page from a previous format — from
/// being read as a list of "free" ids and handed out over data that is still
/// referenced.
pub fn decode(buf: &[u8; crate::storage::page::PAGE_SIZE], file_name: &'static str, page_id: u32) -> Result<FreeDirectoryPage> {
    // Refuse anything that is not stamped as a directory page. Without this a
    // page of live data reached through a stale head would be read as a list
    // of free ids, and those ids handed out over data still referenced by a
    // live slot — silent corruption rather than a reported error.
    let header = PageHeader::read_from(buf);
    if header.page_type != PageType::FreeDirectory as u16 {
        return Err(crate::Error::CorruptPage {
            file: file_name,
            page_id,
            reason: "expected a free-directory page",
        });
    }

    let p = PAGE_HEADER_SIZE;

    let next = u32::from_le_bytes(buf[p + OFF_NEXT..p + OFF_NEXT + 4].try_into().expect("4 bytes"));
    let len =
        u32::from_le_bytes(buf[p + OFF_LEN..p + OFF_LEN + 4].try_into().expect("4 bytes")) as usize;

    if len > ENTRIES_PER_PAGE {
        return Err(crate::Error::CorruptPage {
            file: file_name,
            page_id,
            reason: "free-directory entry count exceeds page capacity",
        });
    }

    let mut entries = Vec::with_capacity(len);
    for i in 0..len {
        let off = p + OFF_ENTRIES + i * 4;
        entries.push(u32::from_le_bytes(buf[off..off + 4].try_into().expect("4 bytes")));
    }

    Ok(FreeDirectoryPage { next, entries })
}
