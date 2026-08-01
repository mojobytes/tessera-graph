// SPDX-License-Identifier: Apache-2.0

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_HEADER_SIZE: usize = 16;
pub const PAGE_PAYLOAD_SIZE: usize = PAGE_SIZE - PAGE_HEADER_SIZE;

pub type PageBuf = Box<[u8; PAGE_SIZE]>;

/// Allocates a zeroed page buffer on the heap.
#[must_use]
pub fn new_page_buf() -> PageBuf {
    Box::new([0u8; PAGE_SIZE])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
#[allow(dead_code)]
pub enum PageType {
    Meta = 0,
    Node = 1,
    Edge = 2,
    Adjacency = 3,
    String = 4,
    Overflow = 5,
    /// Shared adjacency slab page (issue #54): packs sub-blocks for multiple
    /// `(node_id, direction)` pairs behind a directory, instead of dedicating
    /// a whole page to a single node's adjacency chain. Distinct `page_type`
    /// from [`Self::Adjacency`] so a reader never mistakes one format for the
    /// other, even though both live under `DataFile::Adjacency` and share the
    /// `magic::ADJACENCY` stamp.
    AdjacencySlab = 6,
    /// Free-page directory page: holds a batch of page ids that are no longer
    /// referenced and may be handed out again, plus a link to the next such
    /// page. Lives inside the data file whose free pages it lists, and is
    /// itself drawn from that file — so a directory page is distinguishable
    /// from live data only by this stamp.
    FreeDirectory = 7,
    /// Shared property slab page: packs the overflowed property blobs of
    /// several entities behind a directory, instead of dedicating a whole
    /// 4096-byte page to one entity's blob. Lives under `DataFile::Overflow`
    /// alongside the chained format and shares its `magic::OVERFLOW` stamp, so
    /// this `page_type` is the only thing telling the two apart.
    PropertySlab = 8,
}

pub mod magic {
    pub const META: [u8; 4] = *b"TGMD";
    pub const NODES: [u8; 4] = *b"TGND";
    pub const EDGES: [u8; 4] = *b"TGED";
    pub const ADJACENCY: [u8; 4] = *b"TGAD";
    pub const STRINGS: [u8; 4] = *b"TGSD";
    pub const OVERFLOW: [u8; 4] = *b"TGOD";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    pub magic: [u8; 4],
    pub version: u16,
    pub page_type: u16,
    pub crc32: u32,
    pub slot_count: u16,
    pub lsn_low: u16,
}

impl PageHeader {
    /// Reads the header from bytes [0..16] of `buf`.
    #[must_use]
    pub const fn read_from(buf: &[u8; PAGE_SIZE]) -> Self {
        Self {
            magic: [buf[0], buf[1], buf[2], buf[3]],
            version: u16::from_le_bytes([buf[4], buf[5]]),
            page_type: u16::from_le_bytes([buf[6], buf[7]]),
            crc32: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            slot_count: u16::from_le_bytes([buf[12], buf[13]]),
            lsn_low: u16::from_le_bytes([buf[14], buf[15]]),
        }
    }

    /// Writes the header into bytes [0..16] of `buf`.
    pub fn write_to(&self, buf: &mut [u8; PAGE_SIZE]) {
        buf[0..4].copy_from_slice(&self.magic);
        buf[4..6].copy_from_slice(&self.version.to_le_bytes());
        buf[6..8].copy_from_slice(&self.page_type.to_le_bytes());
        buf[8..12].copy_from_slice(&self.crc32.to_le_bytes());
        buf[12..14].copy_from_slice(&self.slot_count.to_le_bytes());
        buf[14..16].copy_from_slice(&self.lsn_low.to_le_bytes());
    }
}

/// Computes CRC32 over the payload region `[16..4096]`.
#[must_use]
pub fn compute_crc32(buf: &[u8; PAGE_SIZE]) -> u32 {
    crc32fast::hash(&buf[PAGE_HEADER_SIZE..PAGE_SIZE])
}

/// Writes header fields and computes + stores CRC32 into `buf`.
///
/// The payload (`buf[16..4096]`) must already contain the desired data
/// before calling this function.
pub fn finalize_page(
    buf: &mut [u8; PAGE_SIZE],
    magic: [u8; 4],
    version: u16,
    page_type: PageType,
    slot_count: u16,
) {
    finalize_page_with_lsn(buf, magic, version, page_type, slot_count, 0);
}

/// Like [`finalize_page`] but also stamps `lsn_low` (the low 16 bits of the WAL LSN).
pub fn finalize_page_with_lsn(
    buf: &mut [u8; PAGE_SIZE],
    magic: [u8; 4],
    version: u16,
    page_type: PageType,
    slot_count: u16,
    lsn_low: u16,
) {
    let crc = compute_crc32(buf);
    let header = PageHeader {
        magic,
        version,
        page_type: page_type as u16,
        crc32: crc,
        slot_count,
        lsn_low,
    };
    header.write_to(buf);
}

/// Validates a page buffer loaded from disk against `expected_magic` and its
/// stored CRC32.
///
/// A fully zeroed page is treated as a legitimate never-written page (reserved
/// by `allocate_page` but not yet populated) and passes validation unchanged.
/// This is the single source of truth for on-read page integrity, shared by
/// the buffer pool's `read_from_disk` and the snapshot/restore integrity check.
///
/// # Errors
///
/// - [`Error::CorruptPage`] if the page is non-zero and its magic does not
///   match `expected_magic` (data written with a wrong/garbled magic).
/// - [`Error::ChecksumMismatch`] if the magic is valid but the stored CRC32
///   does not match the CRC32 computed over the payload.
pub fn validate_page_buf(
    buf: &PageBuf,
    expected_magic: [u8; 4],
    file_name: &'static str,
    page_id: u32,
) -> crate::error::Result<()> {
    use crate::Error;

    // A fully zeroed page is a never-written page: valid engine state.
    if buf.as_ref().iter().all(|&b| b == 0) {
        return Ok(());
    }

    let hdr = PageHeader::read_from(buf);
    if hdr.magic != expected_magic {
        return Err(Error::CorruptPage {
            file: file_name,
            page_id,
            reason: "invalid magic",
        });
    }
    let actual = compute_crc32(buf);
    if hdr.crc32 != actual {
        return Err(Error::ChecksumMismatch {
            file: file_name,
            page_id,
            expected: hdr.crc32,
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_header_write_to() {
        let header = PageHeader {
            magic: magic::NODES,
            version: 1,
            page_type: PageType::Node as u16,
            crc32: 0xDEAD_BEEF,
            slot_count: 17,
            lsn_low: 0,
        };

        let mut buf = new_page_buf();
        header.write_to(&mut buf);

        assert_eq!(&buf[0..4], &magic::NODES);
        assert_eq!(u16::from_le_bytes([buf[4], buf[5]]), 1);
        assert_eq!(u16::from_le_bytes([buf[6], buf[7]]), PageType::Node as u16);
        assert_eq!(
            u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            0xDEAD_BEEF
        );
        assert_eq!(u16::from_le_bytes([buf[12], buf[13]]), 17);
        assert_eq!(u16::from_le_bytes([buf[14], buf[15]]), 0);
    }

    #[test]
    fn crc32_covers_payload_only() {
        let mut buf = new_page_buf();
        buf[16] = 0xAA;
        let crc_original = compute_crc32(&buf);

        // Modify a header byte — CRC should NOT change
        buf[0] = 0xFF;
        assert_eq!(crc_original, compute_crc32(&buf));

        // Modify a payload byte — CRC SHOULD change
        buf[17] = 0xBB;
        assert_ne!(crc_original, compute_crc32(&buf));
    }

    #[test]
    fn finalize_page_sets_crc() {
        let mut buf = new_page_buf();
        buf[20] = 42;
        buf[100] = 99;

        finalize_page(&mut buf, magic::NODES, 1, PageType::Node, 5);

        assert_eq!(&buf[0..4], &magic::NODES);
        assert_eq!(u16::from_le_bytes([buf[4], buf[5]]), 1);
        assert_eq!(u16::from_le_bytes([buf[6], buf[7]]), PageType::Node as u16);
        assert_eq!(u16::from_le_bytes([buf[12], buf[13]]), 5);
        assert_eq!(u16::from_le_bytes([buf[14], buf[15]]), 0);

        let stored_crc = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        assert_eq!(stored_crc, compute_crc32(&buf));
    }

    #[test]
    fn new_page_buf_zeroed() {
        let buf = new_page_buf();
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn magic_bytes_values() {
        assert_eq!(magic::META, [0x54, 0x47, 0x4D, 0x44]);
        assert_eq!(magic::NODES, [0x54, 0x47, 0x4E, 0x44]);
        assert_eq!(magic::EDGES, [0x54, 0x47, 0x45, 0x44]);
        assert_eq!(magic::ADJACENCY, [0x54, 0x47, 0x41, 0x44]);
        assert_eq!(magic::STRINGS, [0x54, 0x47, 0x53, 0x44]);
        assert_eq!(magic::OVERFLOW, [0x54, 0x47, 0x4F, 0x44]);
    }

    #[test]
    fn page_header_round_trip_read_write() {
        let header = PageHeader {
            magic: magic::EDGES,
            version: 1,
            page_type: PageType::Edge as u16,
            crc32: 0x1234_5678,
            slot_count: 25,
            lsn_low: 0,
        };

        let mut buf = new_page_buf();
        header.write_to(&mut buf);

        let read_back = PageHeader::read_from(&buf);
        assert_eq!(read_back, header);
    }

    #[test]
    fn lsn_low_stored_and_read_from_page_header() {
        let header = PageHeader {
            magic: magic::NODES,
            version: 1,
            page_type: PageType::Node as u16,
            crc32: 0,
            slot_count: 10,
            lsn_low: 0xABCD,
        };

        let mut buf = new_page_buf();
        header.write_to(&mut buf);

        // Verify raw bytes at offset [14..16].
        assert_eq!(u16::from_le_bytes([buf[14], buf[15]]), 0xABCD);

        // Verify via `read_from`.
        let read_back = PageHeader::read_from(&buf);
        assert_eq!(read_back.lsn_low, 0xABCD);
    }

    #[test]
    fn page_lsn_low_survives_round_trip() {
        let header = PageHeader {
            magic: magic::EDGES,
            version: 1,
            page_type: PageType::Edge as u16,
            crc32: 0x1111_2222,
            slot_count: 5,
            lsn_low: 0xFFFF, // max u16
        };

        let mut buf = new_page_buf();
        header.write_to(&mut buf);
        let read_back = PageHeader::read_from(&buf);
        assert_eq!(read_back.lsn_low, 0xFFFF);
        assert_eq!(read_back, header);
    }

    #[test]
    fn header_little_endian() {
        let mut buf = new_page_buf();
        let header = PageHeader {
            magic: [0x00; 4],
            version: 0x0100,
            page_type: 0,
            crc32: 0,
            slot_count: 0,
            lsn_low: 0,
        };
        header.write_to(&mut buf);

        // version=0x0100 at bytes [4..6], little-endian: [0x00, 0x01]
        assert_eq!(buf[4], 0x00);
        assert_eq!(buf[5], 0x01);
    }

    // --- validate_page_buf (Cycle A-4) ---

    #[test]
    fn validate_page_buf_valid_page_ok() {
        let mut buf = new_page_buf();
        buf[PAGE_HEADER_SIZE] = 0x42;
        finalize_page(&mut buf, magic::NODES, 1, PageType::Node, 1);

        assert!(validate_page_buf(&buf, magic::NODES, "nodes.db", 0).is_ok());
    }

    #[test]
    fn validate_page_buf_zeroed_is_valid() {
        // A fully zeroed page is a never-written page (allocated but not yet
        // populated) — valid engine state, not corruption.
        let buf = new_page_buf();
        assert!(validate_page_buf(&buf, magic::NODES, "nodes.db", 0).is_ok());
    }

    #[test]
    fn validate_page_buf_invalid_magic_fails() {
        // Non-zero page carrying a wrong magic is corruption.
        let mut buf = new_page_buf();
        buf[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        buf[PAGE_HEADER_SIZE] = 0x01;

        assert!(matches!(
            validate_page_buf(&buf, magic::NODES, "nodes.db", 0),
            Err(crate::Error::CorruptPage { .. })
        ));
    }

    #[test]
    fn validate_page_buf_corrupt_crc_fails() {
        // Valid magic, but a payload byte mutated after finalize -> CRC mismatch.
        let mut buf = new_page_buf();
        buf[PAGE_HEADER_SIZE] = 0x42;
        finalize_page(&mut buf, magic::NODES, 1, PageType::Node, 1);
        buf[PAGE_HEADER_SIZE] = 0x99; // mutate payload without recomputing CRC

        assert!(matches!(
            validate_page_buf(&buf, magic::NODES, "nodes.db", 0),
            Err(crate::Error::ChecksumMismatch { .. })
        ));
    }
}
