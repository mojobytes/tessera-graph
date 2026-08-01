// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::error::Result;
use crate::wal::record::{self, WalRecord};

/// Reads WAL records sequentially from a file.
///
/// Designed for crash recovery: if a record is corrupt or truncated
/// (e.g. due to a crash mid-write), the iterator skips forward until it
/// finds the next valid record. Use [`WalRecordIter::skipped_count`] to
/// check how many corrupt regions were encountered.
///
/// # Memory usage
///
/// The entire WAL file is read into memory on [`open`](Self::open). This is
/// acceptable because the WAL is truncated after every successful
/// [`Graph::flush`](crate::Graph::flush), so its size is bounded by the
/// mutations since the last flush. For graphs with very large batch writes
/// between flushes, memory usage will be proportional to the WAL file size.
pub struct WalReader {
    data: Vec<u8>,
}

impl WalReader {
    /// Opens the WAL file for reading. Reads the entire file into memory.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened or read.
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        Ok(Self { data })
    }

    /// Returns an iterator over the WAL records.
    ///
    /// When a corrupt or truncated record is encountered, the iterator
    /// advances byte-by-byte until it finds the next valid record or
    /// reaches the end of the data. A `Checkpoint` record is yielded
    /// but signals that all prior records were already flushed.
    #[must_use]
    pub fn records(&self) -> WalRecordIter<'_> {
        WalRecordIter {
            data: &self.data,
            offset: 0,
            skipped_count: 0,
        }
    }
}

/// Result of a complete WAL read, including recovery statistics.
///
/// Produced by [`WalReader::read_all`]. Contains all valid records found
/// in the WAL file plus metadata about the read: how many corrupt regions
/// were skipped, and which transaction IDs had a `Commit` record.
#[derive(Debug)]
pub struct WalReadResult {
    /// Valid records decoded in order of appearance (by LSN).
    pub records: Vec<WalRecord>,
    /// Number of contiguous corrupt byte regions skipped during the scan.
    ///
    /// Each count represents one contiguous region where no valid record
    /// could be decoded. Non-contiguous corrupt areas (separated by valid
    /// records) are counted separately. This is NOT a count of individual
    /// corrupt records — the iterator cannot determine record boundaries
    /// within a corrupt region.
    pub skipped_corrupt_regions: usize,
    /// Transaction IDs for which a [`WalRecord::Commit`] was found.
    ///
    /// A transaction is considered committed if and only if a valid
    /// (CRC-verified) `Commit` record exists in this WAL. Transactions
    /// with only `Begin` (no `Commit` or `Rollback`) are considered
    /// in-flight at the time of the crash. `Rollback` records do NOT
    /// affect this set — only `Commit` records populate it. This field
    /// is intended for external transaction managers (e.g. enterprise
    /// `TxnManager`) to reconstruct their committed set during recovery.
    pub committed_txn_ids: HashSet<u64>,
}

impl WalReader {
    /// Reads all valid records from a WAL file with forward-scanning.
    ///
    /// Unlike iterating with [`records()`](Self::records), this method also
    /// computes transaction commit information from `Commit` records,
    /// returning a [`WalReadResult`] with full recovery statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if the WAL file cannot be opened or read.
    pub fn read_all(path: &Path) -> Result<WalReadResult> {
        let reader = Self::open(path)?;
        let mut iter = reader.records();
        let records: Vec<_> = iter.by_ref().collect();
        let skipped_corrupt_regions = iter.skipped_count();

        let mut committed_txn_ids = HashSet::new();
        for rec in &records {
            if let WalRecord::Commit { txn_id, .. } = rec {
                committed_txn_ids.insert(*txn_id);
            }
        }

        Ok(WalReadResult {
            records,
            skipped_corrupt_regions,
            committed_txn_ids,
        })
    }
}

/// Iterator over WAL records in a byte buffer.
///
/// When a corrupt or truncated record is encountered, the iterator advances
/// one byte at a time until it finds a valid record or reaches end-of-data.
/// Use [`skipped_count`](Self::skipped_count) after iteration to check how
/// many corrupt regions were skipped.
pub struct WalRecordIter<'a> {
    data: &'a [u8],
    offset: usize,
    skipped_count: usize,
}

impl WalRecordIter<'_> {
    /// Number of corrupt regions skipped during iteration so far.
    ///
    /// Each corrupt region is a contiguous sequence of bytes where no valid
    /// record could be decoded. Non-contiguous corrupt records (separated by
    /// valid records) are counted separately.
    #[must_use]
    pub const fn skipped_count(&self) -> usize {
        self.skipped_count
    }
}

impl Iterator for WalRecordIter<'_> {
    type Item = WalRecord;

    fn next(&mut self) -> Option<WalRecord> {
        while self.offset < self.data.len() {
            if let Ok((rec, consumed)) = record::decode(&self.data[self.offset..]) {
                self.offset += consumed;
                return Some(rec);
            }
            // Corrupt or truncated record — scan forward byte-by-byte
            // until we find a valid record boundary or exhaust the data.
            self.skipped_count += 1;
            self.offset += 1;
            while self.offset < self.data.len() {
                if let Ok((rec, consumed)) = record::decode(&self.data[self.offset..]) {
                    self.offset += consumed;
                    return Some(rec);
                }
                self.offset += 1;
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::writer::WalWriter;
    use tempfile::NamedTempFile;

    fn wal_path() -> NamedTempFile {
        NamedTempFile::new().unwrap()
    }

    #[test]
    fn read_records_written_by_writer() {
        let tf = wal_path();
        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            w.append(WalRecord::TombstoneNode { lsn: 0, node_id: 10, txn_id: None }).unwrap();
            w.append(WalRecord::TombstoneEdge { lsn: 0, edge_id: 20, txn_id: None }).unwrap();
            w.sync().unwrap();
        }

        let reader = WalReader::open(tf.path()).unwrap();
        let records: Vec<_> = reader.records().collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].lsn(), 1);
        assert_eq!(records[1].lsn(), 2);

        match &records[0] {
            WalRecord::TombstoneNode { node_id, .. } => assert_eq!(*node_id, 10),
            other => panic!("expected TombstoneNode, got {other:?}"),
        }
        match &records[1] {
            WalRecord::TombstoneEdge { edge_id, .. } => assert_eq!(*edge_id, 20),
            other => panic!("expected TombstoneEdge, got {other:?}"),
        }
    }

    #[test]
    fn checkpoint_is_yielded() {
        let tf = wal_path();
        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            w.append(WalRecord::TombstoneNode { lsn: 0, node_id: 1, txn_id: None }).unwrap();
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();
            w.append(WalRecord::TombstoneNode { lsn: 0, node_id: 2, txn_id: None }).unwrap();
            w.sync().unwrap();
        }

        let reader = WalReader::open(tf.path()).unwrap();
        let records: Vec<_> = reader.records().collect();
        assert_eq!(records.len(), 3);

        // The checkpoint should be present as the 2nd record.
        match &records[1] {
            WalRecord::Checkpoint { lsn } => assert_eq!(*lsn, 2),
            other => panic!("expected Checkpoint, got {other:?}"),
        }
    }

    #[test]
    fn corrupt_last_record_first_survives() {
        let tf = wal_path();
        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();
            w.sync().unwrap();
        }

        // Corrupt the second record's CRC.
        let mut data = std::fs::read(tf.path()).unwrap();
        let last = data.len() - 1;
        data[last] ^= 0xFF;
        std::fs::write(tf.path(), &data).unwrap();

        let reader = WalReader::open(tf.path()).unwrap();
        let mut iter = reader.records();
        let records: Vec<_> = iter.by_ref().collect();
        // First record survives; second is corrupt but no valid records follow.
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].lsn(), 1);
    }

    #[test]
    fn corrupt_middle_record_skips_and_continues() {
        let tf = wal_path();
        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();       // lsn=1
            w.append(WalRecord::TombstoneNode { lsn: 0, node_id: 10, txn_id: None }).unwrap(); // lsn=2
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();       // lsn=3
            w.sync().unwrap();
        }

        // Corrupt the second record (middle) by flipping its CRC.
        // Record 1 is a Checkpoint: 17 bytes. Record 2 starts at offset 17.
        // Record 2 is TombstoneNode: 25 bytes. Its CRC is at bytes [17+21..17+25).
        let mut data = std::fs::read(tf.path()).unwrap();
        let crc_byte = 17 + 25 - 1;
        data[crc_byte] ^= 0xFF;
        std::fs::write(tf.path(), &data).unwrap();

        let reader = WalReader::open(tf.path()).unwrap();
        let mut iter = reader.records();
        let records: Vec<_> = iter.by_ref().collect();
        assert_eq!(records.len(), 2, "should yield record 1 and 3, skipping 2");
        assert_eq!(records[0].lsn(), 1);
        assert_eq!(records[1].lsn(), 3);
        assert_eq!(iter.skipped_count(), 1);
    }

    #[test]
    fn multiple_contiguous_corrupt_records_all_skipped() {
        let tf = wal_path();
        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();       // lsn=1
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();       // lsn=2
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();       // lsn=3
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();       // lsn=4
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();       // lsn=5
            w.sync().unwrap();
        }

        // Each Checkpoint is 17 bytes. Corrupt records 2, 3, 4.
        let mut data = std::fs::read(tf.path()).unwrap();
        for &start in &[17usize, 34, 51] {
            let crc_byte = start + 17 - 1;
            data[crc_byte] ^= 0xFF;
        }
        std::fs::write(tf.path(), &data).unwrap();

        let reader = WalReader::open(tf.path()).unwrap();
        let mut iter = reader.records();
        let records: Vec<_> = iter.by_ref().collect();
        assert_eq!(records.len(), 2, "records 1 and 5 survive");
        assert_eq!(records[0].lsn(), 1);
        assert_eq!(records[1].lsn(), 5);
        // skipped_count tracks corrupt *regions* (contiguous corrupt bytes),
        // not individual records — the iterator cannot determine record
        // boundaries within a corrupt region.
        assert!(iter.skipped_count() >= 1, "at least one corrupt region detected");
    }

    #[test]
    fn non_contiguous_corrupt_records_counted_separately() {
        let tf = wal_path();
        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap(); // lsn=1 @ 0
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap(); // lsn=2 @ 17 — corrupt
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap(); // lsn=3 @ 34
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap(); // lsn=4 @ 51 — corrupt
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap(); // lsn=5 @ 68
            w.sync().unwrap();
        }

        // Corrupt records 2 and 4 (non-contiguous — separated by valid record 3).
        let mut data = std::fs::read(tf.path()).unwrap();
        for &start in &[17usize, 51] {
            let crc_byte = start + 17 - 1;
            data[crc_byte] ^= 0xFF;
        }
        std::fs::write(tf.path(), &data).unwrap();

        let reader = WalReader::open(tf.path()).unwrap();
        let mut iter = reader.records();
        let records: Vec<_> = iter.by_ref().collect();
        assert_eq!(records.len(), 3, "records 1, 3, 5 survive");
        assert_eq!(records[0].lsn(), 1);
        assert_eq!(records[1].lsn(), 3);
        assert_eq!(records[2].lsn(), 5);
        assert_eq!(iter.skipped_count(), 2, "two separate corrupt regions");
    }

    #[test]
    fn read_all_returns_result_with_stats() {
        let tf = wal_path();
        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();       // lsn=1
            w.append(WalRecord::TombstoneNode { lsn: 0, node_id: 10, txn_id: None }).unwrap(); // lsn=2
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();       // lsn=3
            w.sync().unwrap();
        }

        // Corrupt record 2.
        let mut data = std::fs::read(tf.path()).unwrap();
        let crc_byte = 17 + 25 - 1;
        data[crc_byte] ^= 0xFF;
        std::fs::write(tf.path(), &data).unwrap();

        let result = WalReader::read_all(tf.path()).unwrap();
        assert_eq!(result.records.len(), 2);
        assert_eq!(result.skipped_corrupt_regions, 1);
        assert!(result.committed_txn_ids.is_empty());
    }

    #[test]
    fn read_all_returns_committed_txn_ids() {
        let tf = wal_path();
        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            // txn 1: committed
            w.append(WalRecord::Begin { lsn: 0, txn_id: 1 }).unwrap();
            w.append(WalRecord::TombstoneNode { lsn: 0, node_id: 10, txn_id: None }).unwrap();
            w.append(WalRecord::Commit { lsn: 0, txn_id: 1 }).unwrap();
            // txn 2: rolled back
            w.append(WalRecord::Begin { lsn: 0, txn_id: 2 }).unwrap();
            w.append(WalRecord::Rollback { lsn: 0, txn_id: 2 }).unwrap();
            // txn 3: in-flight (no commit, simulates crash)
            w.append(WalRecord::Begin { lsn: 0, txn_id: 3 }).unwrap();
            w.sync().unwrap();
        }

        let result = WalReader::read_all(tf.path()).unwrap();
        assert!(result.committed_txn_ids.contains(&1), "txn 1 was committed");
        assert!(!result.committed_txn_ids.contains(&2), "txn 2 was rolled back");
        assert!(!result.committed_txn_ids.contains(&3), "txn 3 was in-flight");
        assert_eq!(result.committed_txn_ids.len(), 1);
        assert_eq!(result.skipped_corrupt_regions, 0);
    }

    #[test]
    fn read_all_committed_txn_ids_survives_corruption() {
        let tf = wal_path();
        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            // txn 1: committed (Begin + Commit = 25+25 = 50 bytes, starts at 0)
            w.append(WalRecord::Begin { lsn: 0, txn_id: 1 }).unwrap();
            w.append(WalRecord::Commit { lsn: 0, txn_id: 1 }).unwrap();
            // txn 2: committed but Commit record will be corrupted
            w.append(WalRecord::Begin { lsn: 0, txn_id: 2 }).unwrap();
            w.append(WalRecord::Commit { lsn: 0, txn_id: 2 }).unwrap(); // lsn=4
            // txn 3: committed
            w.append(WalRecord::Begin { lsn: 0, txn_id: 3 }).unwrap();
            w.append(WalRecord::Commit { lsn: 0, txn_id: 3 }).unwrap();
            w.sync().unwrap();
        }

        // Corrupt the 4th record (Commit for txn 2). Each Begin/Commit is 25 bytes.
        let mut data = std::fs::read(tf.path()).unwrap();
        let crc_byte = 3 * 25 + 25 - 1; // offset of 4th record's last CRC byte
        data[crc_byte] ^= 0xFF;
        std::fs::write(tf.path(), &data).unwrap();

        let result = WalReader::read_all(tf.path()).unwrap();
        assert!(result.committed_txn_ids.contains(&1), "txn 1 commit survived");
        assert!(!result.committed_txn_ids.contains(&2), "txn 2 commit was corrupt");
        assert!(result.committed_txn_ids.contains(&3), "txn 3 commit survived");
        assert!(result.skipped_corrupt_regions >= 1);
    }

    #[test]
    fn rollback_does_not_remove_legitimately_committed_txn() {
        // A Rollback for an already-committed txn_id should not remove it.
        // This scenario can arise from a false-positive decode during
        // forward-scanning of corrupt data.
        let tf = wal_path();
        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            w.append(WalRecord::Begin { lsn: 0, txn_id: 1 }).unwrap();
            w.append(WalRecord::Commit { lsn: 0, txn_id: 1 }).unwrap();
            w.append(WalRecord::Rollback { lsn: 0, txn_id: 1 }).unwrap();
            w.sync().unwrap();
        }
        let result = WalReader::read_all(tf.path()).unwrap();
        assert!(
            result.committed_txn_ids.contains(&1),
            "Rollback after Commit must not remove txn from committed set"
        );
    }

    #[test]
    fn empty_wal_returns_empty_iterator() {
        let tf = wal_path();
        // Write nothing — just ensure the file exists.
        std::fs::write(tf.path(), []).unwrap();

        let reader = WalReader::open(tf.path()).unwrap();
        assert_eq!(reader.records().count(), 0);
    }

    #[test]
    fn partial_write_at_end_is_ignored() {
        let tf = wal_path();
        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            w.append(WalRecord::TombstoneNode { lsn: 0, node_id: 42, txn_id: None }).unwrap();
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();
            w.sync().unwrap();
        }

        // Truncate the file to simulate a partial write of record 2.
        let data = std::fs::read(tf.path()).unwrap();
        let truncated_len = data.len() - 3; // remove last 3 bytes
        std::fs::write(tf.path(), &data[..truncated_len]).unwrap();

        let reader = WalReader::open(tf.path()).unwrap();
        let records: Vec<_> = reader.records().collect();
        // Only the first record should survive.
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].lsn(), 1);
    }
}
