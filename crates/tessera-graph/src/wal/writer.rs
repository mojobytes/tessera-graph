// SPDX-License-Identifier: Apache-2.0

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::Result;
use crate::wal::record::{self, WalRecord};

/// Appends WAL records to a file with auto-incrementing LSN.
///
/// Uses `BufWriter` to batch multiple `write_all()` calls into fewer
/// syscalls. The userspace buffer is flushed before `sync_data()`.
pub struct WalWriter {
    file: BufWriter<File>,
    next_lsn: u64,
    /// Bytes appended to this WAL since it was last truncated (issue #58).
    ///
    /// Backs the size-triggered checkpoint: without it, deciding whether the
    /// WAL has outgrown its threshold would mean asking the filesystem for the
    /// file size on every write. The encoder already computes each record's
    /// length, so accumulating it here costs nothing.
    ///
    /// Seeded from the file's existing contents on [`Self::open`] — bytes
    /// already on disk count towards the threshold — and reset to zero by
    /// [`Self::truncate`]. Note this differs from `next_lsn`, which
    /// deliberately keeps increasing across a truncate.
    bytes_written: u64,
}

impl WalWriter {
    /// Opens (or creates) the WAL file.
    ///
    /// If the file already contains records, `next_lsn` is set to one past
    /// the highest LSN found. Otherwise it starts at 1.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened/created or if existing
    /// records are unreadable (corrupt WAL).
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;

        let (next_lsn, bytes_on_disk) = Self::recover_next_lsn(&mut file)?;

        // Position at the end for appending.
        file.seek(SeekFrom::End(0))?;

        Ok(Self {
            // 64 KB holds ~15 page-sized WAL records (each ~4 KB + header)
            // before triggering an auto-flush. Durability is only guaranteed
            // after an explicit sync() call (flush + fsync).
            file: BufWriter::with_capacity(64 * 1024, file),
            next_lsn,
            bytes_written: bytes_on_disk,
        })
    }

    /// Appends a record to the WAL, stamping it with the next LSN.
    ///
    /// Returns the LSN assigned to this record.
    ///
    /// # Errors
    ///
    /// Returns an error if the write to disk fails.
    pub fn append(&mut self, mut record: WalRecord) -> Result<u64> {
        let lsn = self.next_lsn;
        set_lsn(&mut record, lsn);
        let bytes = record::encode(&record);
        self.file.write_all(&bytes)?;
        self.next_lsn += 1;
        // Issue #58: the encoded length is already in hand, so tracking the
        // journal's size costs an addition rather than a filesystem query.
        self.bytes_written += bytes.len() as u64;
        Ok(lsn)
    }

    /// Flushes the `BufWriter` userspace buffer and then calls `fsync`.
    ///
    /// # Errors
    ///
    /// Returns an error if flush or `fsync` fails.
    pub fn sync(&mut self) -> Result<()> {
        self.file.flush()?;
        self.file.get_ref().sync_data()?;
        Ok(())
    }

    /// Truncates the WAL file to zero length (used after a successful flush).
    ///
    /// # Errors
    ///
    /// Returns an error if the file truncation or seek fails.
    pub fn truncate(&mut self) -> Result<()> {
        self.file.flush()?;
        self.file.get_mut().set_len(0)?;
        self.file.get_mut().seek(SeekFrom::Start(0))?;
        // Make the truncation durable so a crash cannot leave stale tail bytes.
        self.file.get_ref().sync_data()?;
        // The journal is empty again, so the size that drives the
        // size-triggered checkpoint starts over (issue #58). Without this the
        // WAL would still read as oversized immediately after being emptied
        // and the checkpoint would fire repeatedly.
        self.bytes_written = 0;
        // Keep the current `next_lsn` — LSNs never go backwards. The two
        // counters diverge here on purpose: bytes describe what is on disk
        // right now, the LSN describes ordering across the WAL's whole life.
        Ok(())
    }

    /// Returns the next LSN that will be assigned.
    #[must_use]
    pub const fn next_lsn(&self) -> u64 {
        self.next_lsn
    }

    /// Scans the file to find the highest LSN, returning `max_lsn + 1`.
    /// Returns 1 if the file is empty.
    ///
    /// # Memory
    ///
    /// Reads the entire WAL into a `Vec<u8>` via `read_to_end`. This is
    /// safe in practice because `truncate()` is called on every successful
    /// checkpoint, bounding WAL size to the records written since the last
    /// checkpoint. If unbounded WAL growth becomes a concern, streaming
    /// decode or memory-mapped I/O should be considered.
    /// Scans the existing WAL to recover the next LSN and the number of bytes
    /// occupied by intact records.
    ///
    /// The byte count deliberately excludes a corrupt or truncated tail: those
    /// bytes are not replayable, so counting them towards the checkpoint
    /// threshold would misreport how much live journal is really there.
    fn recover_next_lsn(file: &mut File) -> Result<(u64, u64)> {
        file.seek(SeekFrom::Start(0))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;

        let mut offset = 0;
        let mut max_lsn: u64 = 0;

        while offset < buf.len() {
            match record::decode(&buf[offset..]) {
                Ok((rec, consumed)) => {
                    let lsn = rec.lsn();
                    if lsn > max_lsn {
                        max_lsn = lsn;
                    }
                    offset += consumed;
                }
                Err(_) => break, // corrupt or truncated tail — stop
            }
        }

        // `offset` stops at the first unreadable record, so it is exactly the
        // extent of the intact prefix.
        let intact_bytes = offset as u64;
        Ok((if max_lsn == 0 { 1 } else { max_lsn + 1 }, intact_bytes))
    }

    /// Bytes appended since the WAL was last truncated, including any already
    /// on disk when it was opened. Drives the size-triggered checkpoint
    /// (issue #58).
    #[must_use]
    pub const fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

impl Drop for WalWriter {
    fn drop(&mut self) {
        // `BufWriter::drop()` calls flush() internally but silently discards
        // errors. We call flush() here explicitly so any I/O failure is
        // visible via stderr instead of being lost silently.
        // Note: `sync_data()` is intentionally NOT called — callers must call
        // `sync()` explicitly for durability guarantees.
        if let Err(e) = self.file.flush() {
            eprintln!("[WalWriter] flush on drop failed: {e}");
        }
    }
}

/// Sets the LSN on a `WalRecord` variant.
const fn set_lsn(record: &mut WalRecord, lsn: u64) {
    match record {
        WalRecord::WriteNode { lsn: l, .. }
        | WalRecord::WriteEdge { lsn: l, .. }
        | WalRecord::TombstoneNode { lsn: l, .. }
        | WalRecord::TombstoneEdge { lsn: l, .. }
        | WalRecord::WriteAdjPage { lsn: l, .. }
        | WalRecord::WriteStringPage { lsn: l, .. }
        | WalRecord::WriteOverflowPage { lsn: l, .. }
        | WalRecord::Checkpoint { lsn: l }
        | WalRecord::Begin { lsn: l, .. }
        | WalRecord::Commit { lsn: l, .. }
        | WalRecord::Rollback { lsn: l, .. }
        | WalRecord::FreeListState { lsn: l, .. } => *l = lsn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::record::WalRecord;
    use tempfile::NamedTempFile;

    fn wal_path() -> NamedTempFile {
        NamedTempFile::new().unwrap()
    }

    fn checkpoint() -> WalRecord {
        WalRecord::Checkpoint { lsn: 0 }
    }

    // ── Issue #58: byte counter backing the size-triggered checkpoint ──────

    /// The writer tracks how many bytes it has appended, so the checkpoint
    /// threshold can be evaluated without asking the filesystem on every
    /// write. The expected value is derived from the encoder rather than
    /// hardcoded, so the test does not break when the record layout changes.
    #[test]
    fn append_accumulates_bytes_written() {
        let tf = wal_path();
        let mut w = WalWriter::open(tf.path()).unwrap();
        assert_eq!(w.bytes_written(), 0, "a fresh WAL has written nothing");

        let mut first = checkpoint();
        set_lsn(&mut first, 1);
        let first_len = record::encode(&first).len() as u64;

        w.append(checkpoint()).unwrap();
        assert_eq!(
            w.bytes_written(),
            first_len,
            "the counter must reflect the encoded size of the appended record"
        );

        w.append(checkpoint()).unwrap();
        assert_eq!(
            w.bytes_written(),
            first_len * 2,
            "successive appends accumulate; the counter is not per-record"
        );
    }

    /// Truncating empties the journal, so the byte counter must go back to
    /// zero with it. Without this the WAL would still look oversized right
    /// after being emptied, and the size-triggered checkpoint would fire in a
    /// loop — checkpoint, still "over threshold", checkpoint again.
    #[test]
    fn truncate_resets_bytes_written_counter() {
        let tf = wal_path();
        let mut w = WalWriter::open(tf.path()).unwrap();
        w.append(checkpoint()).unwrap();
        w.append(checkpoint()).unwrap();
        assert!(w.bytes_written() > 0, "precondition: the counter has moved");

        w.truncate().unwrap();

        assert_eq!(
            w.bytes_written(),
            0,
            "an emptied WAL must report zero bytes, or the checkpoint loops"
        );
    }

    /// The two counters behave differently on truncate, on purpose: the byte
    /// counter measures what is on disk now (so it resets), while the sequence
    /// number must keep increasing for recovery to stay correct.
    #[test]
    fn truncate_resets_bytes_but_not_the_sequence_number() {
        let tf = wal_path();
        let mut w = WalWriter::open(tf.path()).unwrap();
        w.append(checkpoint()).unwrap();
        w.append(checkpoint()).unwrap();

        w.truncate().unwrap();

        assert_eq!(w.bytes_written(), 0);
        assert_eq!(
            w.append(checkpoint()).unwrap(),
            3,
            "the sequence number must survive a truncate"
        );
    }

    /// Reopening a WAL that still holds records must not reset the counter to
    /// zero: the bytes on disk are real and still count towards the threshold.
    /// Starting from zero here would silently under-report the WAL size for
    /// the whole lifetime of the reopened graph.
    #[test]
    fn open_seeds_counter_from_existing_wal_contents() {
        let tf = wal_path();
        let on_disk = {
            let mut w = WalWriter::open(tf.path()).unwrap();
            w.append(checkpoint()).unwrap();
            w.append(checkpoint()).unwrap();
            w.sync().unwrap();
            w.bytes_written()
        };
        assert!(on_disk > 0, "precondition: the WAL holds records");

        let reopened = WalWriter::open(tf.path()).unwrap();
        assert_eq!(
            reopened.bytes_written(),
            on_disk,
            "reopening must account for the bytes already on disk"
        );
    }

    #[test]
    fn write_record_appends_to_file() {
        let tf = wal_path();
        let mut w = WalWriter::open(tf.path()).unwrap();
        let lsn = w.append(checkpoint()).unwrap();
        assert_eq!(lsn, 1);

        // Data is in BufWriter's userspace buffer; sync flushes it to disk.
        w.sync().unwrap();
        let meta = std::fs::metadata(tf.path()).unwrap();
        assert!(meta.len() > 0);
    }

    #[test]
    fn write_multiple_records_all_persisted_after_sync() {
        let tf = wal_path();
        let mut w = WalWriter::open(tf.path()).unwrap();

        w.append(WalRecord::TombstoneNode { lsn: 0, node_id: 1, txn_id: None }).unwrap();
        w.append(WalRecord::TombstoneEdge { lsn: 0, edge_id: 2, txn_id: None }).unwrap();
        w.append(checkpoint()).unwrap();
        w.sync().unwrap();

        // Read back via decoder.
        let data = std::fs::read(tf.path()).unwrap();
        let mut offset = 0;
        let mut count = 0;
        while offset < data.len() {
            let (_, consumed) = record::decode(&data[offset..]).unwrap();
            offset += consumed;
            count += 1;
        }
        assert_eq!(count, 3);
    }

    #[test]
    fn truncate_empties_the_file() {
        let tf = wal_path();
        let mut w = WalWriter::open(tf.path()).unwrap();
        w.append(checkpoint()).unwrap();
        w.truncate().unwrap();

        let meta = std::fs::metadata(tf.path()).unwrap();
        assert_eq!(meta.len(), 0);
    }

    #[test]
    fn lsn_auto_increments() {
        let tf = wal_path();
        let mut w = WalWriter::open(tf.path()).unwrap();

        assert_eq!(w.append(checkpoint()).unwrap(), 1);
        assert_eq!(w.append(checkpoint()).unwrap(), 2);
        assert_eq!(w.append(checkpoint()).unwrap(), 3);
        assert_eq!(w.next_lsn(), 4);
    }

    #[test]
    fn reopen_resumes_lsn() {
        let tf = wal_path();

        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            w.append(checkpoint()).unwrap(); // LSN 1
            w.append(checkpoint()).unwrap(); // LSN 2
            w.append(checkpoint()).unwrap(); // LSN 3
            w.sync().unwrap();
        }

        let w = WalWriter::open(tf.path()).unwrap();
        assert_eq!(w.next_lsn(), 4);
    }

    #[test]
    fn truncate_preserves_lsn_monotonicity() {
        let tf = wal_path();
        let mut w = WalWriter::open(tf.path()).unwrap();
        w.append(checkpoint()).unwrap(); // LSN 1
        w.append(checkpoint()).unwrap(); // LSN 2
        w.truncate().unwrap();

        assert_eq!(w.next_lsn(), 3);
        let lsn = w.append(checkpoint()).unwrap();
        assert_eq!(lsn, 3);
    }

    #[test]
    fn buf_writer_data_on_disk_only_after_sync() {
        let tf = wal_path();
        let mut w = WalWriter::open(tf.path()).unwrap();
        w.append(checkpoint()).unwrap();

        // PRE-SYNC: Checkpoint record (~21 bytes) fits in the 64 KB buffer,
        // so it must still be in userspace — the file on disk is empty.
        let size_before = std::fs::metadata(tf.path()).unwrap().len();
        assert_eq!(size_before, 0, "append() must not write to disk before sync()");

        w.sync().unwrap();

        // POST-SYNC: flush() + sync_data() must have drained the buffer.
        let data = std::fs::read(tf.path()).unwrap();
        assert!(!data.is_empty(), "sync() must flush BufWriter before fsync");
    }

    #[test]
    fn drop_flushes_pending_data_to_os_buffer() {
        // When WalWriter is dropped without an explicit sync(), the Drop
        // impl must flush the BufWriter so pending bytes reach the OS.
        let tf = wal_path();
        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            w.append(checkpoint()).unwrap();
            // Intentionally NO sync() before drop.
        }
        let data = std::fs::read(tf.path()).unwrap();
        assert!(!data.is_empty(), "Drop impl must flush BufWriter to OS buffer");
    }

    #[test]
    fn truncate_is_durable_after_reopen() {
        // Verifies that after truncate() + drop, reopening sees 0 bytes.
        // The sync_data() call in truncate() ensures set_len(0) is persisted.
        let tf = wal_path();
        {
            let mut w = WalWriter::open(tf.path()).unwrap();
            w.append(checkpoint()).unwrap();
            w.sync().unwrap();
            assert!(std::fs::metadata(tf.path()).unwrap().len() > 0);
            w.truncate().unwrap();
        }
        let len = std::fs::metadata(tf.path()).unwrap().len();
        assert_eq!(len, 0, "truncate() must durably persist set_len(0)");
    }

    #[test]
    fn bufwriter_holds_records_up_to_64kb_without_auto_flush() {
        // Each WriteAdjPage record is ~4117 bytes (PAGE_SIZE + header overhead).
        // Two records = ~8234 bytes. With the default 8 KB BufWriter this would
        // trigger an auto-flush. With a 64 KB buffer both records stay in
        // userspace memory until an explicit sync().
        let tf = wal_path();
        let mut w = WalWriter::open(tf.path()).unwrap();
        for _ in 0..2 {
            w.append(WalRecord::WriteAdjPage {
                lsn: 0,
                page_id: 0,
                data: Box::new([0u8; 4096]),
                txn_id: None,
            })
            .unwrap();
        }
        let size_before = std::fs::metadata(tf.path()).unwrap().len();
        assert_eq!(size_before, 0, "BufWriter should hold both records without auto-flush");
    }

    #[test]
    fn hundred_appends_all_on_disk_after_single_sync() {
        let tf = wal_path();
        let mut w = WalWriter::open(tf.path()).unwrap();
        for _ in 0..100 {
            w.append(checkpoint()).unwrap();
        }
        w.sync().unwrap();
        let data = std::fs::read(tf.path()).unwrap();
        let mut offset = 0;
        let mut count = 0u32;
        while offset < data.len() {
            let (_, consumed) = record::decode(&data[offset..]).unwrap();
            offset += consumed;
            count += 1;
        }
        assert_eq!(count, 100);
    }
}
