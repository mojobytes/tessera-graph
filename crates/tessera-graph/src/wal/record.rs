// SPDX-License-Identifier: MIT

use crate::Error;
use crate::error::Result;
use crate::storage::page::{PAGE_SIZE, PageBuf};

/// Slot size for both nodes and edges (128 bytes).
const SLOT_SIZE: usize = 128;

/// WAL record type tags.
const TAG_WRITE_NODE: u8 = 0x01;
const TAG_WRITE_EDGE: u8 = 0x02;
const TAG_TOMBSTONE_NODE: u8 = 0x03;
const TAG_TOMBSTONE_EDGE: u8 = 0x04;
const TAG_WRITE_ADJ_PAGE: u8 = 0x05;
const TAG_CHECKPOINT: u8 = 0x06;
const TAG_WRITE_STRING_PAGE: u8 = 0x07;
const TAG_WRITE_OVERFLOW_PAGE: u8 = 0x08;
const TAG_BEGIN: u8 = 0x09;
const TAG_COMMIT: u8 = 0x0A;
const TAG_ROLLBACK: u8 = 0x0B;
const TAG_FREE_LIST_STATE: u8 = 0x0C;

/// A WAL record representing a single atomic operation.
#[derive(Debug, Clone)]
pub enum WalRecord {
    /// Write a node slot to a specific page and slot index.
    ///
    /// `txn_id` is `Some` when the write belongs to an explicit MVCC
    /// transaction (Block 4), `None` for an auto-commit write. Recovery replays
    /// `None` unconditionally and `Some(id)` only if `id` committed.
    WriteNode {
        lsn: u64,
        page_id: u32,
        slot_idx: u8,
        slot: Box<[u8; SLOT_SIZE]>,
        txn_id: Option<u64>,
    },
    /// Write an edge slot to a specific page and slot index. See
    /// [`WalRecord::WriteNode`] for `txn_id`.
    WriteEdge {
        lsn: u64,
        page_id: u32,
        slot_idx: u8,
        slot: Box<[u8; SLOT_SIZE]>,
        txn_id: Option<u64>,
    },
    /// Tombstone a node by ID. See [`WalRecord::WriteNode`] for `txn_id`.
    TombstoneNode {
        lsn: u64,
        node_id: u64,
        txn_id: Option<u64>,
    },
    /// Tombstone an edge by ID. See [`WalRecord::WriteNode`] for `txn_id`.
    TombstoneEdge {
        lsn: u64,
        edge_id: u64,
        txn_id: Option<u64>,
    },
    /// Write a full adjacency page. See [`WalRecord::WriteNode`] for `txn_id`.
    WriteAdjPage {
        lsn: u64,
        page_id: u32,
        data: PageBuf,
        txn_id: Option<u64>,
    },
    /// Write a full string-heap page. See [`WalRecord::WriteNode`] for `txn_id`.
    WriteStringPage {
        lsn: u64,
        page_id: u32,
        data: PageBuf,
        txn_id: Option<u64>,
    },
    /// Write a full overflow page. See [`WalRecord::WriteNode`] for `txn_id`.
    WriteOverflowPage {
        lsn: u64,
        page_id: u32,
        data: PageBuf,
        txn_id: Option<u64>,
    },
    /// Checkpoint — marks the end of a consistent state.
    Checkpoint { lsn: u64 },
    /// Transaction begin marker.
    Begin { lsn: u64, txn_id: u64 },
    /// Transaction commit marker.
    Commit { lsn: u64, txn_id: u64 },
    /// Transaction rollback marker.
    Rollback { lsn: u64, txn_id: u64 },
    /// One data file's free-page bookkeeping: which page heads its free
    /// directory, which page is held in the metadata spare slot, and how many
    /// pages are free.
    ///
    /// This state lives in the metadata page, which is written only by `flush`.
    /// Without this record a release that is not followed by a flush does not
    /// survive a crash: the page comes back neither live nor reusable, i.e.
    /// leaked — the very defect the free list exists to remove, reappearing on
    /// the recovery path.
    ///
    /// Carries no `txn_id`: page reuse is a physical, backend-level concern
    /// with no transactional identity. A released page belongs to no
    /// transaction, and replaying its release is always safe — worst case the
    /// page is listed as free when nothing points at it, which is exactly what
    /// being free means.
    FreeListState {
        lsn: u64,
        /// Index of the data file, per `DataFile::index`.
        file_index: u8,
        directory_head: u32,
        spare_page: u32,
        free_count: u32,
    },
}

impl WalRecord {
    /// Returns the LSN of this record.
    #[must_use]
    pub const fn lsn(&self) -> u64 {
        match self {
            Self::WriteNode { lsn, .. }
            | Self::WriteEdge { lsn, .. }
            | Self::TombstoneNode { lsn, .. }
            | Self::TombstoneEdge { lsn, .. }
            | Self::WriteAdjPage { lsn, .. }
            | Self::WriteStringPage { lsn, .. }
            | Self::WriteOverflowPage { lsn, .. }
            | Self::Checkpoint { lsn }
            | Self::Begin { lsn, .. }
            | Self::Commit { lsn, .. }
            | Self::Rollback { lsn, .. }
            | Self::FreeListState { lsn, .. } => *lsn,
        }
    }
}

/// Encodes a WAL record into bytes.
///
/// Binary format:
///
/// ```text
/// [0..4]   record_len: u32 LE (total bytes after this field, including CRC)
/// [4]      tag: u8
/// [5..13]  lsn: u64 LE
/// [13..]   payload (variable)
/// [N-4..N] crc32: u32 LE (covers bytes [4..N-4])
/// ```
///
/// Encodes a slot-write payload: `[tag][lsn:u64][page_id:u32][slot_idx:u8][slot:128B]`.
fn encode_slot(
    buf: &mut Vec<u8>,
    tag: u8,
    lsn: u64,
    page_id: u32,
    slot_idx: u8,
    slot: &[u8; SLOT_SIZE],
) {
    buf.push(tag);
    buf.extend_from_slice(&lsn.to_le_bytes());
    buf.extend_from_slice(&page_id.to_le_bytes());
    buf.push(slot_idx);
    buf.extend_from_slice(slot.as_ref());
}

/// Encodes a full-page payload: `[tag][lsn:u64][page_id:u32][data:4096B]`.
fn encode_page(buf: &mut Vec<u8>, tag: u8, lsn: u64, page_id: u32, data: &PageBuf) {
    buf.push(tag);
    buf.extend_from_slice(&lsn.to_le_bytes());
    buf.extend_from_slice(&page_id.to_le_bytes());
    buf.extend_from_slice(data.as_ref());
}

/// Encodes a tombstone payload: `[tag][lsn:u64][id:u64]`.
fn encode_id(buf: &mut Vec<u8>, tag: u8, lsn: u64, id: u64) {
    buf.push(tag);
    buf.extend_from_slice(&lsn.to_le_bytes());
    buf.extend_from_slice(&id.to_le_bytes());
}

/// Appends the optional `txn_id` trailer to a data-variant payload:
/// `[present:u8]` followed by `[txn_id:u64 LE]` only when present. Absent (the
/// old format) reads back as `None` — see [`decode_opt_txn_id`].
fn encode_opt_txn_id(buf: &mut Vec<u8>, txn_id: Option<u64>) {
    match txn_id {
        Some(id) => {
            buf.push(1);
            buf.extend_from_slice(&id.to_le_bytes());
        }
        None => buf.push(0),
    }
}

/// Reads the optional `txn_id` trailer from `rest` (the payload bytes after the
/// variant's fixed fields). Backward-compatible: an empty `rest` (a record
/// written before this field existed) decodes as `None` rather than erroring.
/// A present byte of `1` must be followed by 8 bytes or the record is corrupt.
fn decode_opt_txn_id(rest: &[u8], label: &'static str) -> Result<Option<u64>> {
    match rest.first() {
        None | Some(0) => Ok(None),
        Some(1) => {
            if rest.len() < 9 {
                return Err(wal_error(label));
            }
            Ok(Some(u64::from_le_bytes(rest[1..9].try_into().unwrap())))
        }
        Some(_) => Err(wal_error(label)),
    }
}

/// Decodes a slot payload: `[page_id:u32][slot_idx:u8][slot:128B]`.
fn decode_slot(payload: &[u8], label: &'static str) -> Result<(u32, u8, Box<[u8; SLOT_SIZE]>)> {
    if payload.len() < 5 + SLOT_SIZE {
        return Err(wal_error(label));
    }
    let page_id = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let slot_idx = payload[4];
    let mut slot = Box::new([0u8; SLOT_SIZE]);
    slot.copy_from_slice(&payload[5..5 + SLOT_SIZE]);
    Ok((page_id, slot_idx, slot))
}

/// Decodes a full-page payload: `[page_id:u32][data:4096B]`.
fn decode_page(payload: &[u8], label: &'static str) -> Result<(u32, PageBuf)> {
    if payload.len() < 4 + PAGE_SIZE {
        return Err(wal_error(label));
    }
    let page_id = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let mut data = crate::storage::page::new_page_buf();
    data.copy_from_slice(&payload[4..4 + PAGE_SIZE]);
    Ok((page_id, data))
}

/// Decodes a tombstone payload: `[id:u64]`.
fn decode_id(payload: &[u8], label: &'static str) -> Result<u64> {
    if payload.len() < 8 {
        return Err(wal_error(label));
    }
    Ok(u64::from_le_bytes(payload[0..8].try_into().unwrap()))
}

/// Record lengths by type (data variants add 1 byte for `txn_id: None`, or
/// 9 bytes for `Some`):
/// - `WriteNode`/`WriteEdge`: 4 + 1 + 8 + 4 + 1 + 128 + (1|9) + 4 = 151 or 159
/// - `TombstoneNode`/`TombstoneEdge`: 4 + 1 + 8 + 8 + (1|9) + 4 = 26 or 34
/// - `WriteAdjPage`/`WriteStringPage`/`WriteOverflowPage`: 4 + 1 + 8 + 4 + 4096 + (1|9) + 4 = 4118 or 4126
/// - `Checkpoint`: 4 + 1 + 8 + 4 = 17 (control variant, no `txn_id`)
/// - `FreeListState`: 4 + 1 + 8 + 1 + 4 + 4 + 4 + 4 = 30 (no `txn_id`)
///
/// One `FreeListState` accompanies each free-list change, which in the worst
/// case is one per page release. At 30 bytes against the 4118 of the page write
/// that release already journals, that is under 1% added to what the journal
/// carries anyway — small enough not to move the checkpoint threshold.
#[must_use]
pub fn encode(record: &WalRecord) -> Vec<u8> {
    let mut buf = Vec::new();

    // Placeholder for record_len (will be filled at the end)
    buf.extend_from_slice(&[0u8; 4]);

    match record {
        WalRecord::WriteNode {
            lsn,
            page_id,
            slot_idx,
            slot,
            txn_id,
        } => {
            encode_slot(&mut buf, TAG_WRITE_NODE, *lsn, *page_id, *slot_idx, slot);
            encode_opt_txn_id(&mut buf, *txn_id);
        }
        WalRecord::WriteEdge {
            lsn,
            page_id,
            slot_idx,
            slot,
            txn_id,
        } => {
            encode_slot(&mut buf, TAG_WRITE_EDGE, *lsn, *page_id, *slot_idx, slot);
            encode_opt_txn_id(&mut buf, *txn_id);
        }
        WalRecord::TombstoneNode {
            lsn,
            node_id,
            txn_id,
        } => {
            encode_id(&mut buf, TAG_TOMBSTONE_NODE, *lsn, *node_id);
            encode_opt_txn_id(&mut buf, *txn_id);
        }
        WalRecord::TombstoneEdge {
            lsn,
            edge_id,
            txn_id,
        } => {
            encode_id(&mut buf, TAG_TOMBSTONE_EDGE, *lsn, *edge_id);
            encode_opt_txn_id(&mut buf, *txn_id);
        }
        WalRecord::WriteAdjPage {
            lsn,
            page_id,
            data,
            txn_id,
        } => {
            encode_page(&mut buf, TAG_WRITE_ADJ_PAGE, *lsn, *page_id, data);
            encode_opt_txn_id(&mut buf, *txn_id);
        }
        WalRecord::WriteStringPage {
            lsn,
            page_id,
            data,
            txn_id,
        } => {
            encode_page(&mut buf, TAG_WRITE_STRING_PAGE, *lsn, *page_id, data);
            encode_opt_txn_id(&mut buf, *txn_id);
        }
        WalRecord::WriteOverflowPage {
            lsn,
            page_id,
            data,
            txn_id,
        } => {
            encode_page(&mut buf, TAG_WRITE_OVERFLOW_PAGE, *lsn, *page_id, data);
            encode_opt_txn_id(&mut buf, *txn_id);
        }
        WalRecord::Checkpoint { lsn } => {
            buf.push(TAG_CHECKPOINT);
            buf.extend_from_slice(&lsn.to_le_bytes());
        }
        WalRecord::Begin { lsn, txn_id } => {
            encode_id(&mut buf, TAG_BEGIN, *lsn, *txn_id);
        }
        WalRecord::Commit { lsn, txn_id } => {
            encode_id(&mut buf, TAG_COMMIT, *lsn, *txn_id);
        }
        WalRecord::Rollback { lsn, txn_id } => {
            encode_id(&mut buf, TAG_ROLLBACK, *lsn, *txn_id);
        }
        WalRecord::FreeListState {
            lsn,
            file_index,
            directory_head,
            spare_page,
            free_count,
        } => {
            buf.push(TAG_FREE_LIST_STATE);
            buf.extend_from_slice(&lsn.to_le_bytes());
            buf.push(*file_index);
            buf.extend_from_slice(&directory_head.to_le_bytes());
            buf.extend_from_slice(&spare_page.to_le_bytes());
            buf.extend_from_slice(&free_count.to_le_bytes());
        }
    }

    // CRC32 covers tag + lsn + payload (bytes [4..])
    let crc = crc32fast::hash(&buf[4..]);
    buf.extend_from_slice(&crc.to_le_bytes());

    // Fill in record_len (everything after the 4-byte length prefix)
    // One WAL record encodes one slot or page write, both page-sized; the
    // buffer is orders of magnitude below u32.
    #[allow(clippy::cast_possible_truncation)]
    let record_len = (buf.len() - 4) as u32;
    buf[0..4].copy_from_slice(&record_len.to_le_bytes());

    buf
}

/// Decodes a WAL record from a byte buffer.
///
/// Returns the decoded record and the number of bytes consumed.
/// Returns an error if the data is truncated or the CRC is invalid.
#[allow(clippy::too_many_lines)] // Keeping all record variants together makes bounds checks auditable.
pub fn decode(buf: &[u8]) -> Result<(WalRecord, usize)> {
    if buf.len() < 4 {
        return Err(wal_error("truncated record length"));
    }

    let record_len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
    let total_len = 4 + record_len;

    if buf.len() < total_len {
        return Err(wal_error("truncated record body"));
    }

    // Minimum: tag(1) + lsn(8) + crc(4) = 13
    if record_len < 13 {
        return Err(wal_error("record too small"));
    }

    // Verify CRC (covers everything between length prefix and CRC)
    let crc_offset = total_len - 4;
    let stored_crc = u32::from_le_bytes(buf[crc_offset..total_len].try_into().unwrap());
    let computed_crc = crc32fast::hash(&buf[4..crc_offset]);
    if stored_crc != computed_crc {
        return Err(wal_error("CRC mismatch"));
    }

    let tag = buf[4];
    let lsn = u64::from_le_bytes(buf[5..13].try_into().unwrap());
    let payload = &buf[13..crc_offset];

    let record = match tag {
        TAG_WRITE_NODE => {
            let (page_id, slot_idx, slot) = decode_slot(payload, "truncated WriteNode payload")?;
            let txn_id = decode_opt_txn_id(&payload[5 + SLOT_SIZE..], "corrupt WriteNode txn_id")?;
            WalRecord::WriteNode {
                lsn,
                page_id,
                slot_idx,
                slot,
                txn_id,
            }
        }
        TAG_WRITE_EDGE => {
            let (page_id, slot_idx, slot) = decode_slot(payload, "truncated WriteEdge payload")?;
            let txn_id = decode_opt_txn_id(&payload[5 + SLOT_SIZE..], "corrupt WriteEdge txn_id")?;
            WalRecord::WriteEdge {
                lsn,
                page_id,
                slot_idx,
                slot,
                txn_id,
            }
        }
        TAG_TOMBSTONE_NODE => {
            let node_id = decode_id(payload, "truncated TombstoneNode payload")?;
            let txn_id = decode_opt_txn_id(&payload[8..], "corrupt TombstoneNode txn_id")?;
            WalRecord::TombstoneNode {
                lsn,
                node_id,
                txn_id,
            }
        }
        TAG_TOMBSTONE_EDGE => {
            let edge_id = decode_id(payload, "truncated TombstoneEdge payload")?;
            let txn_id = decode_opt_txn_id(&payload[8..], "corrupt TombstoneEdge txn_id")?;
            WalRecord::TombstoneEdge {
                lsn,
                edge_id,
                txn_id,
            }
        }
        TAG_WRITE_ADJ_PAGE => {
            let (page_id, data) = decode_page(payload, "truncated WriteAdjPage payload")?;
            let txn_id =
                decode_opt_txn_id(&payload[4 + PAGE_SIZE..], "corrupt WriteAdjPage txn_id")?;
            WalRecord::WriteAdjPage {
                lsn,
                page_id,
                data,
                txn_id,
            }
        }
        TAG_WRITE_STRING_PAGE => {
            let (page_id, data) = decode_page(payload, "truncated WriteStringPage payload")?;
            let txn_id =
                decode_opt_txn_id(&payload[4 + PAGE_SIZE..], "corrupt WriteStringPage txn_id")?;
            WalRecord::WriteStringPage {
                lsn,
                page_id,
                data,
                txn_id,
            }
        }
        TAG_WRITE_OVERFLOW_PAGE => {
            let (page_id, data) = decode_page(payload, "truncated WriteOverflowPage payload")?;
            let txn_id = decode_opt_txn_id(
                &payload[4 + PAGE_SIZE..],
                "corrupt WriteOverflowPage txn_id",
            )?;
            WalRecord::WriteOverflowPage {
                lsn,
                page_id,
                data,
                txn_id,
            }
        }
        TAG_CHECKPOINT => WalRecord::Checkpoint { lsn },
        TAG_BEGIN => {
            let txn_id = decode_id(payload, "truncated Begin payload")?;
            WalRecord::Begin { lsn, txn_id }
        }
        TAG_COMMIT => {
            let txn_id = decode_id(payload, "truncated Commit payload")?;
            WalRecord::Commit { lsn, txn_id }
        }
        TAG_ROLLBACK => {
            let txn_id = decode_id(payload, "truncated Rollback payload")?;
            WalRecord::Rollback { lsn, txn_id }
        }
        TAG_FREE_LIST_STATE => {
            // 1 (file index) + 4 + 4 + 4
            if payload.len() < 13 {
                return Err(wal_error("truncated FreeListState payload"));
            }
            WalRecord::FreeListState {
                lsn,
                file_index: payload[0],
                directory_head: u32::from_le_bytes(payload[1..5].try_into().unwrap()),
                spare_page: u32::from_le_bytes(payload[5..9].try_into().unwrap()),
                free_count: u32::from_le_bytes(payload[9..13].try_into().unwrap()),
            }
        }
        _ => return Err(wal_error("unknown record tag")),
    };

    Ok((record, total_len))
}

const fn wal_error(reason: &'static str) -> Error {
    Error::WalCorrupt(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_slot(fill: u8) -> [u8; SLOT_SIZE] {
        [fill; SLOT_SIZE]
    }

    fn make_page(fill: u8) -> PageBuf {
        Box::new([fill; PAGE_SIZE])
    }

    #[test]
    fn encode_decode_write_node_record() {
        let record = WalRecord::WriteNode {
            lsn: 1,
            page_id: 5,
            slot_idx: 3,
            slot: Box::new(make_slot(0xAA)),
            txn_id: None,
        };
        let bytes = encode(&record);
        let (decoded, consumed) = decode(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        match decoded {
            WalRecord::WriteNode {
                lsn,
                page_id,
                slot_idx,
                slot,
                ..
            } => {
                assert_eq!(lsn, 1);
                assert_eq!(page_id, 5);
                assert_eq!(slot_idx, 3);
                assert_eq!(slot[0], 0xAA);
                assert_eq!(slot[127], 0xAA);
            }
            other => panic!("expected WriteNode, got {other:?}"),
        }
    }

    #[test]
    fn encode_decode_write_edge_record() {
        let record = WalRecord::WriteEdge {
            lsn: 2,
            page_id: 10,
            slot_idx: 7,
            slot: Box::new(make_slot(0xBB)),
            txn_id: None,
        };
        let bytes = encode(&record);
        let (decoded, _) = decode(&bytes).unwrap();
        match decoded {
            WalRecord::WriteEdge {
                lsn,
                page_id,
                slot_idx,
                slot,
                ..
            } => {
                assert_eq!(lsn, 2);
                assert_eq!(page_id, 10);
                assert_eq!(slot_idx, 7);
                assert_eq!(slot[0], 0xBB);
            }
            other => panic!("expected WriteEdge, got {other:?}"),
        }
    }

    #[test]
    fn encode_decode_tombstone_node() {
        let record = WalRecord::TombstoneNode {
            lsn: 3,
            node_id: 42,
            txn_id: None,
        };
        let bytes = encode(&record);
        let (decoded, _) = decode(&bytes).unwrap();
        match decoded {
            WalRecord::TombstoneNode { lsn, node_id, .. } => {
                assert_eq!(lsn, 3);
                assert_eq!(node_id, 42);
            }
            other => panic!("expected TombstoneNode, got {other:?}"),
        }
    }

    #[test]
    fn encode_decode_tombstone_edge() {
        let record = WalRecord::TombstoneEdge {
            lsn: 4,
            edge_id: 99,
            txn_id: None,
        };
        let bytes = encode(&record);
        let (decoded, _) = decode(&bytes).unwrap();
        match decoded {
            WalRecord::TombstoneEdge { lsn, edge_id, .. } => {
                assert_eq!(lsn, 4);
                assert_eq!(edge_id, 99);
            }
            other => panic!("expected TombstoneEdge, got {other:?}"),
        }
    }

    #[test]
    fn encode_decode_write_adj_page() {
        let record = WalRecord::WriteAdjPage {
            lsn: 5,
            page_id: 20,
            data: make_page(0xCC),
            txn_id: None,
        };
        let bytes = encode(&record);
        let (decoded, _) = decode(&bytes).unwrap();
        match decoded {
            WalRecord::WriteAdjPage {
                lsn, page_id, data, ..
            } => {
                assert_eq!(lsn, 5);
                assert_eq!(page_id, 20);
                assert_eq!(data[0], 0xCC);
                assert_eq!(data[PAGE_SIZE - 1], 0xCC);
            }
            other => panic!("expected WriteAdjPage, got {other:?}"),
        }
    }

    #[test]
    fn encode_decode_checkpoint() {
        let record = WalRecord::Checkpoint { lsn: 100 };
        let bytes = encode(&record);
        let (decoded, _) = decode(&bytes).unwrap();
        match decoded {
            WalRecord::Checkpoint { lsn } => assert_eq!(lsn, 100),
            other => panic!("expected Checkpoint, got {other:?}"),
        }
    }

    #[test]
    fn corrupt_crc32_returns_error() {
        let record = WalRecord::Checkpoint { lsn: 1 };
        let mut bytes = encode(&record);
        // Corrupt the last byte (part of CRC)
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn truncated_record_returns_error() {
        let record = WalRecord::WriteNode {
            lsn: 1,
            page_id: 0,
            slot_idx: 0,
            slot: Box::new(make_slot(0)),
            txn_id: None,
        };
        let bytes = encode(&record);
        // Truncate to half
        assert!(decode(&bytes[..bytes.len() / 2]).is_err());
    }

    #[test]
    fn lsn_increases_monotonically_across_records() {
        let records = [
            WalRecord::WriteNode {
                lsn: 1,
                page_id: 0,
                slot_idx: 0,
                slot: Box::new(make_slot(0)),
                txn_id: None,
            },
            WalRecord::TombstoneNode {
                lsn: 2,
                node_id: 1,
                txn_id: None,
            },
            WalRecord::Checkpoint { lsn: 3 },
        ];

        let mut all_bytes = Vec::new();
        for r in &records {
            all_bytes.extend_from_slice(&encode(r));
        }

        let mut offset = 0;
        let mut prev_lsn = 0;
        while offset < all_bytes.len() {
            let (record, consumed) = decode(&all_bytes[offset..]).unwrap();
            assert!(record.lsn() > prev_lsn);
            prev_lsn = record.lsn();
            offset += consumed;
        }
        assert_eq!(prev_lsn, 3);
    }

    #[test]
    fn empty_buffer_returns_error() {
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn encode_decode_write_string_page() {
        let record = WalRecord::WriteStringPage {
            lsn: 6,
            page_id: 30,
            data: make_page(0xDD),
            txn_id: None,
        };
        let bytes = encode(&record);
        let (decoded, _) = decode(&bytes).unwrap();
        match decoded {
            WalRecord::WriteStringPage {
                lsn, page_id, data, ..
            } => {
                assert_eq!(lsn, 6);
                assert_eq!(page_id, 30);
                assert_eq!(data[0], 0xDD);
                assert_eq!(data[PAGE_SIZE - 1], 0xDD);
            }
            other => panic!("expected WriteStringPage, got {other:?}"),
        }
    }

    #[test]
    fn a_journal_without_free_list_records_still_decodes() {
        // Upgrade path: a journal written before this record existed contains
        // only the older tags, and every one of them must still decode. The
        // reverse (an older binary meeting the new tag) is a hard error by
        // design — an unknown record cannot be safely ignored during recovery.
        for record in [
            WalRecord::WriteOverflowPage {
                lsn: 1,
                page_id: 0,
                data: make_page(0x11),
                txn_id: None,
            },
            WalRecord::TombstoneNode {
                lsn: 2,
                node_id: 5,
                txn_id: None,
            },
            WalRecord::Checkpoint { lsn: 3 },
        ] {
            let bytes = encode(&record);
            assert!(
                decode(&bytes).is_ok(),
                "a pre-existing record type stopped decoding: {record:?}"
            );
        }
    }

    #[test]
    fn encode_decode_free_list_state() {
        // Every field must survive: this record is what makes a page freed
        // since the last flush still reusable after a crash, so a field lost in
        // the codec silently reintroduces the leak.
        let record = WalRecord::FreeListState {
            lsn: 11,
            file_index: 4,
            directory_head: 0xDEAD_BEEF,
            spare_page: 0x0102_0304,
            free_count: 42_000,
        };
        let bytes = encode(&record);
        let (decoded, _) = decode(&bytes).unwrap();
        match decoded {
            WalRecord::FreeListState {
                lsn,
                file_index,
                directory_head,
                spare_page,
                free_count,
            } => {
                assert_eq!(lsn, 11);
                assert_eq!(file_index, 4);
                assert_eq!(directory_head, 0xDEAD_BEEF);
                assert_eq!(spare_page, 0x0102_0304);
                assert_eq!(free_count, 42_000);
            }
            other => panic!("expected FreeListState, got {other:?}"),
        }
    }

    #[test]
    fn free_list_state_rejects_a_truncated_payload() {
        // A short record must be reported, not read past its end: the payload
        // is fixed-width, so anything shorter is corruption.
        let record = WalRecord::FreeListState {
            lsn: 1,
            file_index: 0,
            directory_head: 1,
            spare_page: 2,
            free_count: 3,
        };
        let bytes = encode(&record);
        // Drop two payload bytes and re-stamp length and CRC so the failure is
        // attributable to the length check rather than to the checksum.
        let mut truncated = bytes[..bytes.len() - 4 - 2].to_vec();
        let crc = crc32fast::hash(&truncated[4..]);
        truncated.extend_from_slice(&crc.to_le_bytes());
        #[allow(clippy::cast_possible_truncation)]
        let record_len = (truncated.len() - 4) as u32;
        truncated[0..4].copy_from_slice(&record_len.to_le_bytes());

        assert!(
            decode(&truncated).is_err(),
            "a truncated FreeListState must be rejected, not partially read"
        );
    }

    #[test]
    fn encode_decode_write_overflow_page() {
        let record = WalRecord::WriteOverflowPage {
            lsn: 7,
            page_id: 40,
            data: make_page(0xEE),
            txn_id: None,
        };
        let bytes = encode(&record);
        let (decoded, _) = decode(&bytes).unwrap();
        match decoded {
            WalRecord::WriteOverflowPage {
                lsn, page_id, data, ..
            } => {
                assert_eq!(lsn, 7);
                assert_eq!(page_id, 40);
                assert_eq!(data[0], 0xEE);
                assert_eq!(data[PAGE_SIZE - 1], 0xEE);
            }
            other => panic!("expected WriteOverflowPage, got {other:?}"),
        }
    }

    #[test]
    fn encode_decode_begin() {
        let record = WalRecord::Begin { lsn: 0, txn_id: 42 };
        let bytes = encode(&record);
        let (decoded, _) = decode(&bytes).unwrap();
        assert!(matches!(decoded, WalRecord::Begin { txn_id: 42, .. }));
    }

    #[test]
    fn encode_decode_commit() {
        let record = WalRecord::Commit { lsn: 0, txn_id: 42 };
        let bytes = encode(&record);
        let (decoded, _) = decode(&bytes).unwrap();
        assert!(matches!(decoded, WalRecord::Commit { txn_id: 42, .. }));
    }

    #[test]
    fn encode_decode_rollback() {
        let record = WalRecord::Rollback { lsn: 0, txn_id: 42 };
        let bytes = encode(&record);
        let (decoded, _) = decode(&bytes).unwrap();
        assert!(matches!(decoded, WalRecord::Rollback { txn_id: 42, .. }));
    }

    #[test]
    fn unknown_tag_returns_error() {
        let record = WalRecord::Checkpoint { lsn: 1 };
        let mut bytes = encode(&record);
        // Replace tag byte with unknown value
        bytes[4] = 0xFF;
        // Recompute CRC for the modified bytes
        let crc_offset = bytes.len() - 4;
        let crc = crc32fast::hash(&bytes[4..crc_offset]);
        bytes[crc_offset..].copy_from_slice(&crc.to_le_bytes());
        assert!(decode(&bytes).is_err());
    }

    // ---- Block 4 Phase 4, Cycle 18: txn_id on data variants ----------------

    #[test]
    fn write_node_record_roundtrip_with_txn_id() {
        let record = WalRecord::WriteNode {
            lsn: 0,
            page_id: 1,
            slot_idx: 2,
            slot: Box::new(make_slot(0xAB)),
            txn_id: Some(7),
        };
        let (decoded, _) = decode(&encode(&record)).unwrap();
        assert!(matches!(
            decoded,
            WalRecord::WriteNode {
                txn_id: Some(7),
                ..
            }
        ));
    }

    #[test]
    fn write_node_record_roundtrip_without_txn_id_auto_commit() {
        let record = WalRecord::WriteNode {
            lsn: 0,
            page_id: 1,
            slot_idx: 2,
            slot: Box::new(make_slot(0xAB)),
            txn_id: None,
        };
        let (decoded, _) = decode(&encode(&record)).unwrap();
        assert!(matches!(decoded, WalRecord::WriteNode { txn_id: None, .. }));
    }

    #[test]
    fn tombstone_and_page_variants_roundtrip_txn_id() {
        for record in [
            WalRecord::TombstoneNode {
                lsn: 0,
                node_id: 9,
                txn_id: Some(3),
            },
            WalRecord::TombstoneEdge {
                lsn: 0,
                edge_id: 9,
                txn_id: None,
            },
            WalRecord::WriteAdjPage {
                lsn: 0,
                page_id: 1,
                data: make_page(0x11),
                txn_id: Some(4),
            },
            WalRecord::WriteStringPage {
                lsn: 0,
                page_id: 1,
                data: make_page(0x22),
                txn_id: None,
            },
            WalRecord::WriteOverflowPage {
                lsn: 0,
                page_id: 1,
                data: make_page(0x33),
                txn_id: Some(5),
            },
        ] {
            let expected = format!("{record:?}");
            let (decoded, _) = decode(&encode(&record)).unwrap();
            assert_eq!(format!("{decoded:?}"), expected);
        }
    }

    // Cycle 18.1: a record written in the OLD format (no txn_id trailer byte)
    // must decode as `txn_id: None`, not error. We synthesize the old bytes by
    // encoding a modern `None` record and stripping its trailing presence byte,
    // then fixing record_len + CRC to match the shortened body.
    #[test]
    fn decode_write_node_without_txn_id_byte_defaults_to_none() {
        let modern = encode(&WalRecord::WriteNode {
            lsn: 0,
            page_id: 1,
            slot_idx: 2,
            slot: Box::new(make_slot(0xAB)),
            txn_id: None,
        });
        // Drop the CRC (4 bytes) and the single presence byte (the `None` = 0
        // trailer), rebuilding the old-format body without any txn_id trailer.
        let body_end = modern.len() - 4 - 1; // strip crc + presence byte
        let mut old = modern[..body_end].to_vec();
        let crc = crc32fast::hash(&old[4..]);
        old.extend_from_slice(&crc.to_le_bytes());
        // Same bound as `encode_record`: one record covers at most one page.
        #[allow(clippy::cast_possible_truncation)]
        let new_len = (old.len() - 4) as u32;
        old[0..4].copy_from_slice(&new_len.to_le_bytes());

        let (decoded, consumed) = decode(&old).unwrap();
        assert_eq!(consumed, old.len());
        assert!(matches!(decoded, WalRecord::WriteNode { txn_id: None, .. }));
    }
}
