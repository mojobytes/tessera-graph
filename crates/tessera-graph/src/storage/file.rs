// SPDX-License-Identifier: MIT

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use crate::Error;
use crate::error::Result;
use crate::storage::backend::{DataFile, PageId, StorageBackend};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::codec::node_codec::{SLOT_TOMBSTONE, SLOTS_PER_PAGE};
use crate::storage::meta::GraphMeta;
use crate::storage::page::{
    PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE, PAGE_SIZE, PageBuf, PageHeader, PageType, finalize_page,
    finalize_page_with_lsn, magic, new_page_buf,
};
use crate::wal::reader::WalReader;
use crate::wal::record::WalRecord;
use crate::wal::writer::WalWriter;

/// Configuration for opening a file-backed graph.
#[derive(Debug, Clone)]
pub struct GraphConfig {
    /// Maximum memory for the buffer pool. Default: 64 MB.
    pub memory_limit_bytes: usize,
    /// Create the store directory if it doesn't exist. Default: true.
    pub create_if_missing: bool,
    /// Maximum number of entries in the adjacency cache. Default: 65536.
    pub adj_cache_capacity: usize,
    /// Enable write-ahead log for crash recovery. Default: true.
    pub wal_enabled: bool,
    /// WAL size that triggers an automatic checkpoint, in bytes.
    /// Default: `Some(64 MB)`. `None` disables automatic checkpointing, leaving
    /// [`crate::Graph::flush`] as the only thing that bounds the journal.
    ///
    /// # Why this exists
    ///
    /// Without it, a caller that writes continuously and never flushes grows
    /// the WAL without limit. Reopening then has to replay the whole journal:
    /// a measured production run reached 3.55 GB of WAL and **22 minutes** of
    /// startup, against 0.9 s once the data was checkpointed (issue #58).
    ///
    /// # Calibrating it
    ///
    /// Checkpointing ~90 MB of journal costs ~2 s, and that cost is flat — it
    /// scales with what was written since the last checkpoint, not with the
    /// graph's total size. The 64 MB default keeps the expected pause under
    /// ~1.5 s. Raise it to checkpoint less often at the cost of longer pauses
    /// and slower reopens; lower it for shorter, more frequent pauses.
    ///
    /// Per-graph rather than global on purpose: the same write cadence cost
    /// 10x more to checkpoint on one of the consumer's graphs than on another,
    /// so a single shared value would either over-pause the small graph or let
    /// the big one grow unchecked.
    ///
    /// # Not a hard ceiling
    ///
    /// Crossing the threshold does not checkpoint immediately — it happens at
    /// the next outermost batch close, so the journal may exceed this value
    /// while a batch is open. That is deliberate: checkpointing mid-batch
    /// would stall a write at an unpredictable point and leave the batch
    /// half-materialised.
    ///
    /// `Some(0)` needs no special handling: the first write exceeds it, so
    /// every batch close checkpoints. That is coherent, if rarely useful.
    pub wal_checkpoint_threshold_bytes: Option<u64>,
}

impl GraphConfig {
    /// Creates a new `GraphConfig` with the given memory limit and default settings.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            memory_limit_bytes: 64 * 1024 * 1024,
            create_if_missing: true,
            adj_cache_capacity: 65_536,
            wal_enabled: true,
            wal_checkpoint_threshold_bytes: Some(64 * 1024 * 1024),
        }
    }

    /// Returns a config identical to [`Self::new()`] but with WAL disabled.
    #[must_use]
    pub const fn without_wal() -> Self {
        Self {
            memory_limit_bytes: 64 * 1024 * 1024,
            create_if_missing: true,
            adj_cache_capacity: 65_536,
            wal_enabled: false,
            wal_checkpoint_threshold_bytes: Some(64 * 1024 * 1024),
        }
    }
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// File-backed storage backend with buffer pool caching.
///
/// Manages 5 data files (`nodes.db`, `edges.db`, `adjacency.db`, `strings.db`,
/// `overflow.db`) and a `graph.meta` file, all in a single directory.
///
/// Pages are cached through a `BufferPool` with LRU eviction. The pool owns
/// all file handles so that eviction can write dirty pages to the correct file.
pub struct FileBackend {
    dir: PathBuf,
    meta: GraphMeta,
    pool: BufferPool,
    wal: Option<WalWriter>,
    /// WAL size that triggers an automatic checkpoint, carried over from the
    /// config the graph was opened with (issue #58). `None` disables it.
    ///
    /// Held here because the opening config is not retained anywhere else, and
    /// this is the layer that owns the journal and therefore knows its size.
    wal_checkpoint_threshold_bytes: Option<u64>,
    /// Set once the journal grows past the threshold above, cleared by the
    /// checkpoint that acts on it (issue #58).
    ///
    /// A plain `bool` suffices: `FileBackend` is only ever reached through a
    /// `&mut Graph`, so there is no unsynchronised concurrent access here.
    wal_checkpoint_pending: bool,
}

impl FileBackend {
    /// Opens or creates a file-backed graph store at the given directory.
    pub fn open(path: impl AsRef<Path>, config: &GraphConfig) -> Result<Self> {
        let dir = path.as_ref().to_path_buf();

        if !dir.exists() {
            if config.create_if_missing {
                fs::create_dir_all(&dir)?;
            } else {
                return Err(Error::NotPersisted);
            }
        }

        let meta_path = dir.join("graph.meta");
        let is_new = !meta_path.exists();

        let pool = BufferPool::new(config.memory_limit_bytes);

        // Register all data file handles with the pool
        for df in crate::storage::backend::ALL_DATA_FILES {
            let file = open_data_file(&dir, df.file_name())?;
            pool.register_file(df, file);
        }

        let mut meta = if is_new {
            GraphMeta::new()
        } else {
            read_meta(&meta_path)?
        };

        // WAL recovery: replay any un-checkpointed records before normal open.
        let wal_path = dir.join("wal.log");
        let needs_truncate = if config.wal_enabled && wal_path.exists() {
            Self::recover_from_wal(&wal_path, &pool, &mut meta)? > 0
        } else {
            false
        };

        // Recalculate strings_write_offset from actual page data.
        // After recovery, meta's value may be stale.
        if needs_truncate {
            meta.strings_write_offset = Self::recalculate_strings_write_offset(&pool, &meta)?;
        }

        // Dirty flag without WAL recovery: previous shutdown was not clean (container
        // kill, OOM, SIGKILL) but all flushed data on disk is consistent. The dirty
        // flag only indicates "close() was not called", not data corruption — data
        // files are only modified during flush(), which writes data then clears the
        // dirty flag atomically via flush_meta(). Warn and continue.
        if !is_new && meta.is_dirty() && !needs_truncate {
            eprintln!(
                "[tessera-graph] WARN: store was not closed cleanly (dirty flag set). \
                 No WAL entries to recover — proceeding with data on disk. \
                 This is expected after container restarts or SIGKILL."
            );
        }

        // Set dirty flag on open (cleared on clean close)
        meta.set_dirty();
        write_meta(&meta_path, &meta)?;

        let wal = if config.wal_enabled {
            let mut w = WalWriter::open(&wal_path)?;
            if needs_truncate {
                w.truncate()?;
            }
            Some(w)
        } else {
            None
        };

        Ok(Self {
            dir,
            meta,
            pool,
            wal,
            wal_checkpoint_threshold_bytes: config.wal_checkpoint_threshold_bytes,
            wal_checkpoint_pending: false,
        })
    }

    /// The WAL size that triggers an automatic checkpoint on this graph, or
    /// `None` when automatic checkpointing is disabled (issue #58).
    ///
    /// Only the tests read this directly. Production code never needs the
    /// number itself — it consults [`Self::wal_checkpoint_pending`], the
    /// decision the number feeds into.
    #[cfg(test)]
    const fn wal_checkpoint_threshold_bytes(&self) -> Option<u64> {
        self.wal_checkpoint_threshold_bytes
    }

    /// Re-evaluates whether the journal has outgrown its threshold, raising
    /// [`Self::wal_checkpoint_pending`] if so (issue #58).
    ///
    /// Called after every WAL append. Once raised the flag stays up until a
    /// checkpoint clears it, so this never has to un-set anything: the journal
    /// only shrinks by being checkpointed.
    ///
    /// A no-op when there is no journal or no threshold configured, matching
    /// how the rest of the WAL operations behave when the WAL is off.
    const fn note_wal_growth(&mut self) {
        if self.wal_checkpoint_pending {
            return;
        }
        let (Some(wal), Some(threshold)) = (self.wal.as_ref(), self.wal_checkpoint_threshold_bytes)
        else {
            return;
        };
        if wal.bytes_written() > threshold {
            self.wal_checkpoint_pending = true;
        }
    }

    /// Applies one journalled free-list snapshot to `meta`.
    ///
    /// This bookkeeping lives in the metadata page, which only `flush` writes,
    /// so replaying it is what stops a page freed since the last flush from
    /// coming back neither live nor reusable.
    ///
    /// Each record carries a file's whole state, so replaying in order leaves
    /// the last one per file standing — the state as of the crash — and a torn
    /// sequence can never half-apply.
    ///
    /// Returns whether anything was applied. An unrecognised file index is
    /// skipped rather than treated as corruption: a journal written by a build
    /// with more data files must not stop this one from opening.
    fn replay_free_list_state(
        meta: &mut GraphMeta,
        file_index: u8,
        directory_head: u32,
        spare_page: u32,
        free_count: u32,
    ) -> bool {
        let Some(file) = crate::storage::backend::ALL_DATA_FILES
            .get(file_index as usize)
            .copied()
        else {
            return false;
        };
        meta.set_free_directory_head(file, directory_head);
        meta.set_free_spare_page(file, spare_page);
        meta.set_free_page_count(file, free_count);
        true
    }

    /// Applies already-read WAL records to pages and metadata.
    ///
    /// Split from [`Self::recover_from_wal`] so that reading the journal and
    /// applying it stay separately readable; the dispatch below is long by
    /// nature — one arm per record type.
    #[allow(clippy::too_many_lines)] // The exhaustive WAL dispatch is clearer as one auditable unit.
    fn replay_records(
        pool: &BufferPool,
        meta: &mut GraphMeta,
        records: Vec<WalRecord>,
        committed: &std::collections::HashSet<u64>,
    ) -> Result<u64> {
        let mut replayed = 0u64;

        // A data record is replayed only if durable: an auto-commit write
        // (`txn_id: None`) always is; an explicit-transaction write
        // (`txn_id: Some(id)`) only when that transaction committed (a valid
        // `Commit` exists, per `committed_txn_ids`). An in-flight write whose
        // transaction begun but never committed across a crash is dropped,
        // giving atomic transactional recovery.
        let is_durable = |txn_id: Option<u64>| txn_id.is_none_or(|id| committed.contains(&id));

        for record in records {
            match record {
                WalRecord::WriteNode {
                    lsn,
                    page_id,
                    slot_idx,
                    slot,
                    txn_id,
                } => {
                    if !is_durable(txn_id) {
                        continue;
                    }
                    Self::ensure_page_count(meta, DataFile::Nodes, page_id + 1);
                    Self::replay_slot(
                        pool,
                        DataFile::Nodes,
                        page_id,
                        slot_idx,
                        &slot,
                        magic::NODES,
                        PageType::Node,
                        lsn,
                    )?;
                    replayed += 1;
                }
                WalRecord::WriteEdge {
                    lsn,
                    page_id,
                    slot_idx,
                    slot,
                    txn_id,
                } => {
                    if !is_durable(txn_id) {
                        continue;
                    }
                    Self::ensure_page_count(meta, DataFile::Edges, page_id + 1);
                    Self::replay_slot(
                        pool,
                        DataFile::Edges,
                        page_id,
                        slot_idx,
                        &slot,
                        magic::EDGES,
                        PageType::Edge,
                        lsn,
                    )?;
                    replayed += 1;
                }
                WalRecord::TombstoneNode {
                    node_id, txn_id, ..
                } => {
                    if !is_durable(txn_id) {
                        continue;
                    }
                    Self::replay_tombstone(
                        pool,
                        meta,
                        DataFile::Nodes,
                        node_id,
                        magic::NODES,
                        PageType::Node,
                    )?;
                    replayed += 1;
                }
                WalRecord::TombstoneEdge {
                    edge_id, txn_id, ..
                } => {
                    if !is_durable(txn_id) {
                        continue;
                    }
                    Self::replay_tombstone(
                        pool,
                        meta,
                        DataFile::Edges,
                        edge_id,
                        magic::EDGES,
                        PageType::Edge,
                    )?;
                    replayed += 1;
                }
                WalRecord::WriteAdjPage {
                    page_id,
                    data,
                    txn_id,
                    ..
                } => {
                    if !is_durable(txn_id) {
                        continue;
                    }
                    Self::ensure_page_count(meta, DataFile::Adjacency, page_id + 1);
                    pool.put_page(DataFile::Adjacency, page_id, &data)?;
                    replayed += 1;
                }
                WalRecord::WriteStringPage {
                    page_id,
                    data,
                    txn_id,
                    ..
                } => {
                    if !is_durable(txn_id) {
                        continue;
                    }
                    Self::ensure_page_count(meta, DataFile::Strings, page_id + 1);
                    pool.put_page(DataFile::Strings, page_id, &data)?;
                    replayed += 1;
                }
                WalRecord::WriteOverflowPage {
                    page_id,
                    data,
                    txn_id,
                    ..
                } => {
                    if !is_durable(txn_id) {
                        continue;
                    }
                    Self::ensure_page_count(meta, DataFile::Overflow, page_id + 1);
                    pool.put_page(DataFile::Overflow, page_id, &data)?;
                    replayed += 1;
                }
                WalRecord::FreeListState {
                    file_index,
                    directory_head,
                    spare_page,
                    free_count,
                    ..
                } => {
                    if Self::replay_free_list_state(
                        meta,
                        file_index,
                        directory_head,
                        spare_page,
                        free_count,
                    ) {
                        replayed += 1;
                    }
                }
                WalRecord::Checkpoint { .. }
                | WalRecord::Begin { .. }
                | WalRecord::Commit { .. }
                | WalRecord::Rollback { .. } => {
                    // Checkpoint marks that pages before this point were already
                    // flushed; we keep replaying because a crash mid-flush can
                    // leave a Checkpoint followed by un-replayed records.
                    // Begin/Commit/Rollback are transaction boundary markers with
                    // no page data — Commit's effect is applied by gating each
                    // data record's replay on `committed_txn_ids` (see below).
                }
            }
        }

        Ok(replayed)
    }

    /// Replays WAL records into pages, extending page counts as needed.
    /// Returns the number of records replayed. The caller is responsible
    /// for truncating the WAL after recovery.
    ///
    /// Uses [`WalReader::read_all`] for forward-scanning: corrupt records
    /// are skipped and valid records after them are still recovered.
    fn recover_from_wal(
        wal_path: &std::path::Path,
        pool: &BufferPool,
        meta: &mut GraphMeta,
    ) -> Result<u64> {
        let result = WalReader::read_all(wal_path)?;

        if result.skipped_corrupt_regions > 0 {
            eprintln!(
                "[tessera-graph] WARN: WAL recovery skipped {} corrupt region(s); \
                 {} valid record(s) recovered",
                result.skipped_corrupt_regions,
                result.records.len(),
            );
        }

        let replayed = Self::replay_records(pool, meta, result.records, &result.committed_txn_ids)?;

        if replayed > 0 {
            // Flush recovered pages to disk.
            for df in crate::storage::backend::ALL_DATA_FILES {
                pool.flush_file(df)?;
            }
        }

        Ok(replayed)
    }

    /// Recalculates `strings_write_offset` by scanning string pages.
    ///
    /// For each page, the header's `slot_count` field stores the number of
    /// payload bytes used on that page. The total offset is the sum across
    /// all fully-used pages plus the last page's used bytes.
    fn recalculate_strings_write_offset(pool: &BufferPool, meta: &GraphMeta) -> Result<u32> {
        let page_count = meta.strings_page_count;
        if page_count == 0 {
            return Ok(0);
        }

        // All pages before the last are fully used.
        let full_pages = (page_count - 1) as usize;
        let base_offset = full_pages * PAGE_PAYLOAD_SIZE;

        // Read the last page to get its used bytes.
        let last_page_id = page_count - 1;
        let page = pool.get_page(DataFile::Strings, last_page_id)?;
        let header = PageHeader::read_from(&page);
        let used_on_last = header.slot_count as usize;

        // Checked rather than assumed. This RECONSTRUCTS the cursor from page
        // counts on disk, so it is not covered by the check in
        // `StringHeap::append`: that one bounds new writes, and says nothing
        // about a file written by an older build, or one whose metadata is
        // damaged. A wrapped value here would silently resume writing on top of
        // live heap bytes — the exact failure mode issue #65 exists to remove.
        u32::try_from(base_offset + used_on_last).map_err(|_| Error::CorruptPage {
            file: "strings.db",
            page_id: last_page_id,
            reason: "string heap cursor exceeds u32 — page count and slot count disagree",
        })
    }

    /// Ensures the meta page count is at least `min_count` for the given file.
    const fn ensure_page_count(meta: &mut GraphMeta, file: DataFile, min_count: u32) {
        let current = match file {
            DataFile::Nodes => &mut meta.nodes_page_count,
            DataFile::Edges => &mut meta.edges_page_count,
            DataFile::Adjacency => &mut meta.adj_page_count,
            DataFile::Strings => &mut meta.strings_page_count,
            DataFile::Overflow => &mut meta.overflow_page_count,
        };
        if *current < min_count {
            *current = min_count;
        }
    }

    /// Replays a slot write: reads the page (or creates a new one), writes the
    /// slot at the correct offset, finalizes, and marks dirty in the pool.
    #[allow(clippy::too_many_arguments)]
    fn replay_slot(
        pool: &BufferPool,
        file: DataFile,
        page_id: u32,
        slot_idx: u8,
        slot: &[u8; 128],
        magic_bytes: [u8; 4],
        page_type: PageType,
        lsn: u64,
    ) -> Result<()> {
        let mut page = Self::read_page_or_new(pool, file, page_id);

        let offset = PAGE_HEADER_SIZE + (slot_idx as usize) * 128;
        if offset + 128 <= PAGE_SIZE {
            page[offset..offset + 128].copy_from_slice(slot);
        }

        // slot_count must track the highest used slot index + 1 (not the count
        // of live slots) so that rebuild_indexes scans far enough to find all
        // occupied slots — including those that follow gaps left by missing
        // WAL records (e.g. when forward-scanning skips a corrupt record).
        let header = PageHeader::read_from(&page);
        let slot_count = header.slot_count.max(u16::from(slot_idx) + 1);
        #[allow(clippy::cast_possible_truncation)]
        let lsn_low = lsn as u16;
        finalize_page_with_lsn(&mut page, magic_bytes, 1, page_type, slot_count, lsn_low);
        pool.put_page(file, page_id, &page)?;
        Ok(())
    }

    /// Replays a tombstone: sets the slot's flag byte to tombstone.
    fn replay_tombstone(
        pool: &BufferPool,
        meta: &mut GraphMeta,
        file: DataFile,
        id: u64,
        magic_bytes: [u8; 4],
        page_type: PageType,
    ) -> Result<()> {
        // Same as `graph.rs`: slot id used as an in-memory index.
        #[allow(clippy::cast_possible_truncation)]
        let zero_based = (id.wrapping_sub(1)) as usize;
        // Page index derived by integer division from a slot id; the file format
        // caps page ids at u32 and division only shrinks the value.
        #[allow(clippy::cast_possible_truncation)]
        let page_idx = (zero_based / SLOTS_PER_PAGE) as u32;
        let slot_idx = zero_based % SLOTS_PER_PAGE;

        Self::ensure_page_count(meta, file, page_idx + 1);

        let mut page = Self::read_page_or_new(pool, file, page_idx);

        let offset = PAGE_HEADER_SIZE + slot_idx * 128;
        if offset < PAGE_SIZE {
            page[offset] = SLOT_TOMBSTONE;
        }

        // Use max(existing, slot_idx + 1) — not count_live_slots — so that
        // rebuild_indexes scans far enough to find all slots, even after gaps
        // left by skipped corrupt WAL records during forward-scanning recovery.
        let header = PageHeader::read_from(&page);
        #[allow(clippy::cast_possible_truncation)]
        let slot_count = header.slot_count.max((slot_idx as u16) + 1);
        finalize_page(&mut page, magic_bytes, 1, page_type, slot_count);
        pool.put_page(file, page_idx, &page)?;
        Ok(())
    }

    /// Reads a page from the pool, or returns a zeroed page if not available.
    ///
    /// During WAL replay, it is normal for a `WriteNode` record to reference a
    /// page that does not yet exist on disk (e.g. after a crash before the first
    /// flush). Returning a zeroed page in that case is correct behaviour.
    ///
    /// # Limitation — real I/O errors are silently masked
    ///
    /// This method returns a zeroed page on ANY `get_page` error, including
    /// genuine I/O failures (permissions, hardware). Distinguishing "page not
    /// yet allocated" from a real I/O error requires adding a `page_exists`
    /// predicate to `BufferPool` and is deferred to a future milestone.
    fn read_page_or_new(pool: &BufferPool, file: DataFile, page_id: u32) -> PageBuf {
        pool.get_page(file, page_id)
            .unwrap_or_else(|_| new_page_buf())
    }

    /// Writes the meta page and clears the dirty flag.
    fn flush_meta(&mut self) -> Result<()> {
        self.meta.clear_dirty();
        let meta_path = self.dir.join("graph.meta");
        write_meta(&meta_path, &self.meta)?;
        Ok(())
    }

    const fn file_page_count(&self, file: DataFile) -> u32 {
        match file {
            DataFile::Nodes => self.meta.nodes_page_count,
            DataFile::Edges => self.meta.edges_page_count,
            DataFile::Adjacency => self.meta.adj_page_count,
            DataFile::Strings => self.meta.strings_page_count,
            DataFile::Overflow => self.meta.overflow_page_count,
        }
    }

    const fn set_meta_page_count(&mut self, file: DataFile, count: u32) {
        match file {
            DataFile::Nodes => self.meta.nodes_page_count = count,
            DataFile::Edges => self.meta.edges_page_count = count,
            DataFile::Adjacency => self.meta.adj_page_count = count,
            DataFile::Strings => self.meta.strings_page_count = count,
            DataFile::Overflow => self.meta.overflow_page_count = count,
        }
    }
}

impl StorageBackend for FileBackend {
    fn read_page(&self, file: DataFile, page_id: PageId) -> Result<PageBuf> {
        if page_id >= self.file_page_count(file) {
            return Err(Error::CorruptPage {
                file: file.file_name(),
                page_id,
                reason: "page not allocated",
            });
        }

        self.pool.get_page(file, page_id)
    }

    fn write_page(&mut self, file: DataFile, page_id: PageId, data: &PageBuf) -> Result<()> {
        if page_id >= self.file_page_count(file) {
            return Err(Error::CorruptPage {
                file: file.file_name(),
                page_id,
                reason: "page not allocated",
            });
        }

        // WAL-log string and overflow page writes for crash recovery.
        match file {
            DataFile::Strings => {
                if let Some(ref mut wal) = self.wal {
                    // txn_id: None — string/overflow page writes are backend-level
                    // auto-commit writes, not part of an explicit MVCC transaction.
                    let record = WalRecord::WriteStringPage {
                        lsn: 0,
                        page_id,
                        data: data.clone(),
                        txn_id: None,
                    };
                    wal.append(record)?;
                    self.note_wal_growth();
                }
            }
            DataFile::Overflow => {
                if let Some(ref mut wal) = self.wal {
                    let record = WalRecord::WriteOverflowPage {
                        lsn: 0,
                        page_id,
                        data: data.clone(),
                        txn_id: None,
                    };
                    wal.append(record)?;
                    self.note_wal_growth();
                }
            }
            _ => {}
        }

        self.pool.put_page(file, page_id, data)?;
        Ok(())
    }

    fn allocate_page(&mut self, file: DataFile) -> Result<PageId> {
        // Reuse before growing: a workload that rewrites the same entities
        // must not extend the file on every rewrite.
        if let Some(recycled) = crate::storage::free_list::take_free_page(self, file)? {
            // Hand back a blank page so a recycled one cannot be mistaken for
            // its previous occupant by a caller that reads before writing.
            let buf = new_page_buf();
            self.pool.put_page(file, recycled, &buf)?;
            return Ok(recycled);
        }

        let count = self.file_page_count(file);
        let page_id = count;
        self.set_meta_page_count(file, count + 1);

        // Write a zeroed page into the cache
        let buf = new_page_buf();
        self.pool.put_page(file, page_id, &buf)?;

        Ok(page_id)
    }

    fn free_page(&mut self, file: DataFile, page_id: PageId) -> Result<()> {
        crate::storage::free_list::release_page(self, file, page_id)
    }

    fn page_count(&self, file: DataFile) -> u32 {
        self.file_page_count(file)
    }

    fn flush(&mut self) -> Result<()> {
        for df in crate::storage::backend::ALL_DATA_FILES {
            self.pool.flush_file(df)?;
        }

        // Meta is written last (per architecture doc)
        self.flush_meta()?;

        // Checkpoint + truncate the WAL after all data is durable.
        self.wal_checkpoint_and_truncate()?;

        Ok(())
    }

    fn meta(&self) -> &GraphMeta {
        &self.meta
    }

    fn meta_mut(&mut self) -> &mut GraphMeta {
        &mut self.meta
    }

    fn read_index_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        let path = self.dir.join("index.bin");
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read(&path)?;
        Ok(Some(data))
    }

    fn wal_enabled(&self) -> bool {
        self.wal.is_some()
    }

    fn wal_append(&mut self, record: WalRecord) -> Result<()> {
        if let Some(ref mut wal) = self.wal {
            wal.append(record)?;
            self.note_wal_growth();
        }
        Ok(())
    }

    fn wal_checkpoint_pending(&self) -> bool {
        self.wal_checkpoint_pending
    }

    fn wal_sync(&mut self) -> Result<()> {
        if let Some(ref mut wal) = self.wal {
            wal.sync()?;
        }
        Ok(())
    }

    fn wal_checkpoint_and_truncate(&mut self) -> Result<()> {
        if let Some(ref mut wal) = self.wal {
            wal.append(WalRecord::Checkpoint { lsn: 0 })?;
            wal.sync()?;
            wal.truncate()?;
            // The journal is empty again, so whatever made a checkpoint due has
            // just been satisfied. Leaving this set would re-trigger a
            // checkpoint on the next batch close, over and over (issue #58).
            self.wal_checkpoint_pending = false;
        }
        Ok(())
    }

    fn write_index_bytes(&mut self, data: &[u8]) -> Result<()> {
        let tmp_path = self.dir.join("index.bin.tmp");
        let final_path = self.dir.join("index.bin");

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);

        fs::rename(&tmp_path, &final_path)?;

        // Sync the directory entry so the rename is durable on crash.
        let dir_file = File::open(&self.dir)?;
        dir_file.sync_all()?;

        Ok(())
    }

    fn read_schema_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        let path = self.dir.join("schema.bin");
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read(&path)?;
        Ok(Some(data))
    }

    fn write_schema_bytes(&mut self, data: &[u8]) -> Result<()> {
        // Atomic write mirroring write_index_bytes: tmp + rename + dir fsync.
        let tmp_path = self.dir.join("schema.bin.tmp");
        let final_path = self.dir.join("schema.bin");

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);

        fs::rename(&tmp_path, &final_path)?;

        let dir_file = File::open(&self.dir)?;
        dir_file.sync_all()?;

        Ok(())
    }

    #[cfg(feature = "pool-instrumentation")]
    fn pool_instrumentation(&self) -> (u64, u64, u64) {
        self.pool.instrumentation()
    }

    #[cfg(feature = "pool-instrumentation")]
    fn reset_pool_instrumentation(&self) {
        self.pool.reset_instrumentation();
    }
}

/// Gives the shared free-list algorithm page access, bypassing this backend's
/// own `allocate_page` (which calls into that algorithm, so routing back
/// through it would recurse).
///
/// Both halves of a release are journalled, so it survives a crash whole.
///
/// A directory page's CONTENTS travel as an ordinary page write. The
/// bookkeeping that makes those contents reachable — which page heads the
/// directory, which page sits in the metadata spare slot, how many are free —
/// lives in [`GraphMeta`], which only `flush` writes, so it travels separately
/// as [`WalRecord::FreeListState`] (replayed in `recover_from_wal`).
///
/// Journalling only the page was not enough, and the gap was not hypothetical:
/// a release not followed by a flush left the page neither live nor reusable
/// after recovery — leaked, i.e. the very defect the free list exists to
/// remove, reappearing on the recovery path. Verified by running it before the
/// fix, and pinned by `freed_pages_survive_a_crash_before_flush`.
///
/// Each record carries the file's whole free-list state rather than a delta.
/// Replay is therefore idempotent and order-independent per file — the last
/// record wins — so a torn sequence can never leave the list half-applied.
impl crate::storage::free_list::FreeListStore for FileBackend {
    fn read_page_raw(&self, file: DataFile, page_id: PageId) -> Result<PageBuf> {
        self.pool.get_page(file, page_id)
    }

    fn write_page_raw(&mut self, file: DataFile, page_id: PageId, data: &PageBuf) -> Result<()> {
        // Deliberately not `StorageBackend::write_page`: that rejects a page id
        // at or beyond the file's page count, and a release may legitimately
        // name the last page. The journalling below mirrors what it does.
        match file {
            DataFile::Strings => {
                if let Some(ref mut wal) = self.wal {
                    wal.append(WalRecord::WriteStringPage {
                        lsn: 0,
                        page_id,
                        data: data.clone(),
                        txn_id: None,
                    })?;
                    self.note_wal_growth();
                }
            }
            DataFile::Overflow => {
                if let Some(ref mut wal) = self.wal {
                    wal.append(WalRecord::WriteOverflowPage {
                        lsn: 0,
                        page_id,
                        data: data.clone(),
                        txn_id: None,
                    })?;
                    self.note_wal_growth();
                }
            }
            _ => {}
        }
        self.pool.put_page(file, page_id, data)
    }

    fn meta_ref(&self) -> &GraphMeta {
        &self.meta
    }

    fn meta_mut_ref(&mut self) -> &mut GraphMeta {
        &mut self.meta
    }

    fn journal_free_list_state(&mut self, file: DataFile) -> Result<()> {
        let Some(ref mut wal) = self.wal else {
            return Ok(());
        };
        // File index fits a u8: there are five data files.
        #[allow(clippy::cast_possible_truncation)]
        let file_index = file.index() as u8;
        wal.append(WalRecord::FreeListState {
            lsn: 0,
            file_index,
            directory_head: self.meta.free_directory_head(file),
            spare_page: self.meta.free_spare_page(file),
            free_count: self.meta.free_page_count(file),
        })?;
        self.note_wal_growth();
        Ok(())
    }
}

// ── File helpers ──────────────────────────────────────────────────────

fn open_data_file(dir: &Path, name: &str) -> Result<File> {
    let path = dir.join(name);
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?)
}

fn read_meta(path: &Path) -> Result<GraphMeta> {
    let mut file = File::open(path)?;
    let mut buf = [0u8; PAGE_SIZE];
    file.read_exact(&mut buf)?;
    GraphMeta::deserialize(&buf)
}

fn write_meta(path: &Path, meta: &GraphMeta) -> Result<()> {
    let buf = meta.serialize();
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(buf.as_ref())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── Issue #58: size-triggered WAL checkpoint threshold ────────────────

    /// The threshold ships enabled with a default calibrated against the
    /// consumer's measurements, so a graph that is never tuned still bounds
    /// its journal.
    #[test]
    fn default_config_has_64mb_wal_checkpoint_threshold() {
        assert_eq!(
            GraphConfig::new().wal_checkpoint_threshold_bytes,
            Some(64 * 1024 * 1024),
            "the default threshold must be 64 MB, enabled"
        );
    }

    /// The WAL-less variant carries the same threshold value. It is never
    /// consulted there (no journal, nothing to checkpoint), but keeping the
    /// defaults identical except for the WAL switch itself matches how the
    /// other fields already behave.
    #[test]
    fn without_wal_config_has_same_default_threshold() {
        assert_eq!(
            GraphConfig::without_wal().wal_checkpoint_threshold_bytes,
            GraphConfig::new().wal_checkpoint_threshold_bytes,
        );
    }

    /// A configured threshold must survive the trip from the caller's config
    /// into the open backend. This is the test that stops the field from being
    /// declared, documented, and then never read — in which case every graph
    /// would silently use the default no matter what was asked for.
    #[test]
    fn open_with_custom_threshold_stores_configured_value() {
        let tmp = TempDir::new().unwrap();
        let cfg = GraphConfig {
            wal_checkpoint_threshold_bytes: Some(4096),
            ..GraphConfig::new()
        };
        let backend = FileBackend::open(tmp.path(), &cfg).unwrap();

        assert_eq!(
            backend.wal_checkpoint_threshold_bytes(),
            Some(4096),
            "the configured value must win over the 64 MB default"
        );
    }

    /// Disabling automatic checkpointing must also survive the trip.
    #[test]
    fn open_with_disabled_threshold_stores_none() {
        let tmp = TempDir::new().unwrap();
        let cfg = GraphConfig {
            wal_checkpoint_threshold_bytes: None,
            ..GraphConfig::new()
        };
        let backend = FileBackend::open(tmp.path(), &cfg).unwrap();

        assert_eq!(backend.wal_checkpoint_threshold_bytes(), None);
    }

    /// Crossing the threshold must only *mark* the graph as needing a
    /// checkpoint — it must not truncate the WAL there and then. Checkpointing
    /// mid-write would stall a mutation at an unpredictable point; the actual
    /// work belongs at the next outermost batch close.
    #[test]
    fn crossing_threshold_marks_pending_without_checkpointing_immediately() {
        let tmp = TempDir::new().unwrap();
        let cfg = GraphConfig {
            wal_checkpoint_threshold_bytes: Some(100),
            ..test_config()
        };
        let mut backend = FileBackend::open(tmp.path(), &cfg).unwrap();

        assert!(
            !backend.wal_checkpoint_pending(),
            "precondition: nothing written yet, nothing pending"
        );

        // Write through the ordinary append path until the 100-byte threshold
        // is behind us. One node record is already well past it, but loop so
        // the test does not depend on the encoded size of a single record.
        while backend
            .wal
            .as_ref()
            .is_some_and(|w| w.bytes_written() <= 100)
        {
            backend
                .wal_append(WalRecord::WriteNode {
                    lsn: 0,
                    page_id: 0,
                    slot_idx: 0,
                    slot: Box::new(live_node_slot()),
                    txn_id: None,
                })
                .unwrap();
        }

        assert!(
            backend.wal_checkpoint_pending(),
            "past the threshold the backend must flag that a checkpoint is due"
        );

        // Push the records out of the writer's userspace buffer so the file on
        // disk reflects them — the same sync a closing batch performs. Nothing
        // here truncates, so what lands on disk is what the marking left alone.
        backend.wal_sync().unwrap();
        let wal_len = fs::metadata(tmp.path().join("wal.log")).unwrap().len();
        assert!(
            wal_len > 0,
            "the journal must still hold its records — marking is not checkpointing"
        );
    }

    /// With automatic checkpointing disabled, no amount of writing may raise
    /// the flag; only an explicit flush bounds the journal, exactly as before.
    #[test]
    fn disabled_threshold_never_marks_pending() {
        let tmp = TempDir::new().unwrap();
        let cfg = GraphConfig {
            wal_checkpoint_threshold_bytes: None,
            ..test_config()
        };
        let mut backend = FileBackend::open(tmp.path(), &cfg).unwrap();

        for _ in 0..64 {
            backend
                .wal_append(WalRecord::WriteNode {
                    lsn: 0,
                    page_id: 0,
                    slot_idx: 0,
                    slot: Box::new(live_node_slot()),
                    txn_id: None,
                })
                .unwrap();
        }

        assert!(
            !backend.wal_checkpoint_pending(),
            "a disabled threshold must never be crossed"
        );
    }

    /// Checkpointing clears the flag. Without this the very next batch close
    /// would checkpoint again on an already-empty journal, in a loop.
    #[test]
    fn checkpointing_clears_the_pending_flag() {
        let tmp = TempDir::new().unwrap();
        let cfg = GraphConfig {
            wal_checkpoint_threshold_bytes: Some(100),
            ..test_config()
        };
        let mut backend = FileBackend::open(tmp.path(), &cfg).unwrap();

        while !backend.wal_checkpoint_pending() {
            backend
                .wal_append(WalRecord::WriteNode {
                    lsn: 0,
                    page_id: 0,
                    slot_idx: 0,
                    slot: Box::new(live_node_slot()),
                    txn_id: None,
                })
                .unwrap();
        }

        backend.wal_checkpoint_and_truncate().unwrap();

        assert!(
            !backend.wal_checkpoint_pending(),
            "after checkpointing there is nothing left to checkpoint"
        );
    }

    #[test]
    fn file_backend_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FileBackend>();
    }

    const fn test_config() -> GraphConfig {
        GraphConfig {
            memory_limit_bytes: 64 * 1024,
            create_if_missing: true,
            adj_cache_capacity: 1024,
            wal_enabled: true,
            ..GraphConfig::new()
        }
    }

    /// Builds a finalized (valid magic + CRC) page for `file`, with `marker`
    /// stamped into the first payload byte. Required because on-read validation
    /// now rejects raw, unfinalized pages; production always finalizes before
    /// writing. The marker is read back at offset `PAGE_HEADER_SIZE`.
    fn finalized_marker_page(file: DataFile, marker: u8) -> PageBuf {
        let (magic_bytes, page_type) = match file {
            DataFile::Nodes => (magic::NODES, PageType::Node),
            DataFile::Edges => (magic::EDGES, PageType::Edge),
            DataFile::Adjacency => (magic::ADJACENCY, PageType::Adjacency),
            DataFile::Strings => (magic::STRINGS, PageType::String),
            DataFile::Overflow => (magic::OVERFLOW, PageType::Overflow),
        };
        let mut page = new_page_buf();
        page[PAGE_HEADER_SIZE] = marker;
        finalize_page(&mut page, magic_bytes, 1, page_type, 0);
        page
    }

    #[test]
    fn open_creates_directory() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("mystore");
        assert!(!store_dir.exists());

        let _backend = FileBackend::open(&store_dir, &test_config()).unwrap();
        assert!(store_dir.exists());
        assert!(store_dir.join("graph.meta").exists());
        assert!(store_dir.join("nodes.db").exists());
        assert!(store_dir.join("edges.db").exists());
        assert!(store_dir.join("adjacency.db").exists());
        assert!(store_dir.join("strings.db").exists());
        assert!(store_dir.join("overflow.db").exists());
    }

    #[test]
    fn open_not_persisted_error() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("nonexistent");

        let config = GraphConfig {
            create_if_missing: false,
            ..test_config()
        };
        let result = FileBackend::open(&store_dir, &config);
        assert!(matches!(result, Err(Error::NotPersisted)));
    }

    #[test]
    fn new_store_has_empty_meta() {
        let tmp = TempDir::new().unwrap();
        let backend = FileBackend::open(tmp.path(), &test_config()).unwrap();

        assert_eq!(backend.meta().next_node_id, 1);
        assert_eq!(backend.meta().next_edge_id, 1);
        assert_eq!(backend.meta().node_count, 0);
        assert_eq!(backend.meta().edge_count, 0);
        assert_eq!(backend.page_count(DataFile::Nodes), 0);
    }

    #[test]
    fn dirty_flag_set_on_open() {
        let tmp = TempDir::new().unwrap();
        let backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
        assert!(backend.meta().is_dirty());
    }

    #[test]
    fn flush_clears_dirty_flag() {
        let tmp = TempDir::new().unwrap();
        let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
        backend.flush().unwrap();

        let meta = read_meta(&tmp.path().join("graph.meta")).unwrap();
        assert!(!meta.is_dirty());
    }

    #[test]
    fn allocate_and_read_page() {
        let tmp = TempDir::new().unwrap();
        let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();

        let page_id = backend.allocate_page(DataFile::Nodes).unwrap();
        assert_eq!(page_id, 0);
        assert_eq!(backend.page_count(DataFile::Nodes), 1);

        let page = backend.read_page(DataFile::Nodes, 0).unwrap();
        assert!(page.iter().all(|&b| b == 0));
    }

    #[test]
    fn write_then_read_page() {
        let tmp = TempDir::new().unwrap();
        let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();

        let page_id = backend.allocate_page(DataFile::Nodes).unwrap();
        let mut data = new_page_buf();
        data[0] = 0xAA;
        data[100] = 0xBB;
        data[PAGE_SIZE - 1] = 0xCC;
        backend.write_page(DataFile::Nodes, page_id, &data).unwrap();

        let read_back = backend.read_page(DataFile::Nodes, page_id).unwrap();
        assert_eq!(read_back[0], 0xAA);
        assert_eq!(read_back[100], 0xBB);
        assert_eq!(read_back[PAGE_SIZE - 1], 0xCC);
    }

    #[test]
    fn allocated_unwritten_page_survives_eviction() {
        // Reproduces the latent bug from on-read validation (A-3): an allocated
        // but never-written page is zeroed (magic [0,0,0,0]). allocate_page puts
        // it dirty into the pool; if it is evicted before its first slot-write,
        // the zeroed page is flushed to disk. A later read_page incurs a cache
        // miss and read_from_disk rejects the zeroed magic as CorruptPage —
        // even though an allocated-but-unwritten page is a legitimate state.
        // This is exactly the allocate->read sequence in
        // Graph::write_slot_to_page (graph.rs:1927-1934).
        let tmp = TempDir::new().unwrap();
        // Minimum-size pool (clamped to MIN_POOL_PAGES = 8) to force eviction.
        let config = GraphConfig {
            memory_limit_bytes: 4096,
            ..test_config()
        };
        let mut backend = FileBackend::open(tmp.path(), &config).unwrap();

        // Allocate page 0 (zeroed, dirty in pool) but never write a slot to it.
        let page0 = backend.allocate_page(DataFile::Nodes).unwrap();
        assert_eq!(page0, 0);

        // Allocate enough further pages to evict page 0 from the pool (each
        // allocate_page does a dirty put_page). With MIN_POOL_PAGES = 8, ~16
        // allocations guarantee page 0 is evicted and flushed (still zeroed).
        for _ in 0..16 {
            backend.allocate_page(DataFile::Nodes).unwrap();
        }

        // Read page 0 back: cache miss -> read_from_disk -> zeroed magic.
        // Must NOT error — an allocated-but-unwritten page is valid.
        let page = backend
            .read_page(DataFile::Nodes, 0)
            .expect("allocated-but-unwritten page must be readable after eviction");
        assert!(page.iter().all(|&b| b == 0));
    }

    #[test]
    fn read_unallocated_page_errors() {
        let tmp = TempDir::new().unwrap();
        let backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
        assert!(backend.read_page(DataFile::Nodes, 0).is_err());
    }

    #[test]
    fn write_unallocated_page_errors() {
        let tmp = TempDir::new().unwrap();
        let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
        let data = new_page_buf();
        assert!(backend.write_page(DataFile::Nodes, 0, &data).is_err());
    }

    #[test]
    fn sequential_page_ids() {
        let tmp = TempDir::new().unwrap();
        let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();

        assert_eq!(backend.allocate_page(DataFile::Nodes).unwrap(), 0);
        assert_eq!(backend.allocate_page(DataFile::Nodes).unwrap(), 1);
        assert_eq!(backend.allocate_page(DataFile::Nodes).unwrap(), 2);
        assert_eq!(backend.page_count(DataFile::Nodes), 3);
    }

    #[test]
    fn independent_file_page_counts() {
        let tmp = TempDir::new().unwrap();
        let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();

        backend.allocate_page(DataFile::Nodes).unwrap();
        backend.allocate_page(DataFile::Nodes).unwrap();
        backend.allocate_page(DataFile::Edges).unwrap();

        assert_eq!(backend.page_count(DataFile::Nodes), 2);
        assert_eq!(backend.page_count(DataFile::Edges), 1);
        assert_eq!(backend.page_count(DataFile::Adjacency), 0);
    }

    #[test]
    fn flush_persists_data_to_disk() {
        let tmp = TempDir::new().unwrap();

        {
            let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
            let page_id = backend.allocate_page(DataFile::Nodes).unwrap();
            let data = finalized_marker_page(DataFile::Nodes, 0x42);
            backend.write_page(DataFile::Nodes, page_id, &data).unwrap();
            backend.meta_mut().next_node_id = 100;
            backend.meta_mut().node_count = 50;
            backend.flush().unwrap();
        }

        {
            let backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
            assert_eq!(backend.meta().next_node_id, 100);
            assert_eq!(backend.meta().node_count, 50);
            assert_eq!(backend.page_count(DataFile::Nodes), 1);

            let page = backend.read_page(DataFile::Nodes, 0).unwrap();
            assert_eq!(page[PAGE_HEADER_SIZE], 0x42);
        }
    }

    #[test]
    fn meta_persists_across_reopen() {
        let tmp = TempDir::new().unwrap();

        {
            let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
            backend.meta_mut().next_node_id = 42;
            backend.meta_mut().next_edge_id = 99;
            backend.meta_mut().node_count = 41;
            backend.meta_mut().edge_count = 98;
            backend.allocate_page(DataFile::Nodes).unwrap();
            backend.allocate_page(DataFile::Edges).unwrap();
            backend.allocate_page(DataFile::Edges).unwrap();
            backend.flush().unwrap();
        }

        {
            let backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
            assert_eq!(backend.meta().next_node_id, 42);
            assert_eq!(backend.meta().next_edge_id, 99);
            assert_eq!(backend.meta().node_count, 41);
            assert_eq!(backend.meta().edge_count, 98);
            assert_eq!(backend.page_count(DataFile::Nodes), 1);
            assert_eq!(backend.page_count(DataFile::Edges), 2);
        }
    }

    #[test]
    fn overwrite_page() {
        let tmp = TempDir::new().unwrap();
        let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();

        let page_id = backend.allocate_page(DataFile::Edges).unwrap();

        let mut data1 = new_page_buf();
        data1[0] = 0x11;
        backend
            .write_page(DataFile::Edges, page_id, &data1)
            .unwrap();

        let mut data2 = new_page_buf();
        data2[0] = 0x22;
        backend
            .write_page(DataFile::Edges, page_id, &data2)
            .unwrap();

        let read_back = backend.read_page(DataFile::Edges, page_id).unwrap();
        assert_eq!(read_back[0], 0x22);
    }

    #[test]
    fn all_data_file_variants() {
        let tmp = TempDir::new().unwrap();
        let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();

        let files = [
            DataFile::Nodes,
            DataFile::Edges,
            DataFile::Adjacency,
            DataFile::Strings,
            DataFile::Overflow,
        ];

        for (i, &file) in files.iter().enumerate() {
            let page_id = backend.allocate_page(file).unwrap();
            let mut data = new_page_buf();
            // Test fixture: `i` indexes a 5-element array.
            #[allow(clippy::cast_possible_truncation)]
            let marker = (i + 1) as u8;
            data[0] = marker;
            backend.write_page(file, page_id, &data).unwrap();
        }

        for (i, &file) in files.iter().enumerate() {
            let read_back = backend.read_page(file, 0).unwrap();
            // Test fixture: same 5-element array as the write above.
            #[allow(clippy::cast_possible_truncation)]
            let marker = (i + 1) as u8;
            assert_eq!(read_back[0], marker);
            assert_eq!(backend.page_count(file), 1);
        }
    }

    #[test]
    fn multiple_pages_persist() {
        let tmp = TempDir::new().unwrap();

        {
            let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
            for i in 0..10_u8 {
                let page_id = backend.allocate_page(DataFile::Nodes).unwrap();
                let data = finalized_marker_page(DataFile::Nodes, i);
                backend.write_page(DataFile::Nodes, page_id, &data).unwrap();
            }
            backend.flush().unwrap();
        }

        {
            let backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
            assert_eq!(backend.page_count(DataFile::Nodes), 10);
            for i in 0..10_u8 {
                let page = backend.read_page(DataFile::Nodes, u32::from(i)).unwrap();
                assert_eq!(page[PAGE_HEADER_SIZE], i);
            }
        }
    }

    // ─── restart resilience ──────────────────────────────────────────

    #[test]
    fn dirty_flag_no_wal_open_succeeds() {
        // Simulate unclean shutdown: flush data (clears dirty), then manually
        // set dirty flag in graph.meta (as if process was killed after open
        // but before close). Re-open must succeed — data on disk is consistent.
        let tmp = TempDir::new().unwrap();

        // Create store with data and flush
        {
            let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
            let page_id = backend.allocate_page(DataFile::Nodes).unwrap();
            let data = finalized_marker_page(DataFile::Nodes, 0xAB);
            backend.write_page(DataFile::Nodes, page_id, &data).unwrap();
            backend.meta_mut().next_node_id = 42;
            backend.flush().unwrap();
        }

        // Simulate unclean shutdown: set dirty flag manually
        {
            let meta_path = tmp.path().join("graph.meta");
            let mut meta = read_meta(&meta_path).unwrap();
            assert!(!meta.is_dirty(), "flush should have cleared dirty flag");
            meta.set_dirty();
            write_meta(&meta_path, &meta).unwrap();
        }

        // Re-open: must succeed despite dirty flag (no WAL entries to recover)
        let backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
        assert_eq!(backend.meta().next_node_id, 42);
        assert_eq!(backend.page_count(DataFile::Nodes), 1);
        let page = backend.read_page(DataFile::Nodes, 0).unwrap();
        assert_eq!(
            page[PAGE_HEADER_SIZE], 0xAB,
            "Data must survive unclean shutdown"
        );
    }

    #[test]
    fn clean_shutdown_cycle_clears_dirty_on_disk() {
        let tmp = TempDir::new().unwrap();

        // Open (sets dirty), flush (clears dirty), verify on disk
        {
            let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
            assert!(
                backend.meta().is_dirty(),
                "dirty flag should be set on open"
            );
            backend.flush().unwrap();
        }

        let meta = read_meta(&tmp.path().join("graph.meta")).unwrap();
        assert!(!meta.is_dirty(), "flush must clear dirty flag on disk");
    }

    #[test]
    fn open_rejects_pre_slab_format_store_instead_of_misreading_it() {
        let tmp = TempDir::new().unwrap();

        // A store created by the current build, then aged back to the pre-#54 format
        // version on disk. Only the version marker is rewritten: the point is that a
        // store whose node slots predate the adjacency-pointer layout must never be
        // opened and reinterpreted with the current offsets.
        {
            let mut backend = FileBackend::open(tmp.path(), &test_config()).unwrap();
            backend.flush().unwrap();
        }

        let meta_path = tmp.path().join("graph.meta");
        {
            let mut buf = [0u8; PAGE_SIZE];
            let mut f = std::fs::File::open(&meta_path).unwrap();
            f.read_exact(&mut buf).unwrap();
            buf[4..6].copy_from_slice(&1_u16.to_le_bytes());
            let crc = crate::storage::page::compute_crc32(&buf);
            buf[8..12].copy_from_slice(&crc.to_le_bytes());
            std::fs::write(&meta_path, buf).unwrap();
        }

        // `FileBackend` is not `Debug`, so `unwrap_err` is unavailable here.
        let Err(err) = FileBackend::open(tmp.path(), &test_config()) else {
            panic!("opening a pre-#54 store must fail, but it succeeded");
        };

        assert!(
            matches!(err, Error::IncompatibleVersion { found: 1, .. }),
            "opening a pre-#54 store must fail cleanly, got {err:?}"
        );
    }

    // ---- Block 4 Phase 4, Cycles 22-23: transactional WAL recovery ---------

    /// A live node slot with `SLOT_LIVE` in its flags byte, for asserting a
    /// `WriteNode` replayed into a page.
    fn live_node_slot() -> [u8; 128] {
        let mut slot = [0u8; 128];
        slot[0] = crate::storage::codec::node_codec::SLOT_LIVE;
        slot
    }

    /// Reads the flags byte of slot 0 on page 0 of `file` after reopen.
    fn slot0_flags(backend: &FileBackend, file: DataFile) -> u8 {
        if backend.page_count(file) == 0 {
            return crate::storage::codec::node_codec::SLOT_EMPTY;
        }
        backend.read_page(file, 0).unwrap()[PAGE_HEADER_SIZE]
    }

    #[test]
    fn recover_from_wal_discards_uncommitted_txn_writes() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        // Force directory init so open() below reopens rather than creates.
        FileBackend::open(dir, &test_config())
            .unwrap()
            .flush()
            .unwrap();

        {
            let mut w = WalWriter::open(&dir.join("wal.log")).unwrap();
            w.append(WalRecord::Begin { lsn: 0, txn_id: 1 }).unwrap();
            w.append(WalRecord::WriteNode {
                lsn: 0,
                page_id: 0,
                slot_idx: 0,
                slot: Box::new(live_node_slot()),
                txn_id: Some(1),
            })
            .unwrap();
            // No Commit: simulates a crash mid-transaction.
        }

        let backend = FileBackend::open(
            dir,
            &GraphConfig {
                create_if_missing: false,
                ..test_config()
            },
        )
        .unwrap();
        // The uncommitted node must NOT be in the page.
        assert_eq!(
            slot0_flags(&backend, DataFile::Nodes),
            crate::storage::codec::node_codec::SLOT_EMPTY
        );
    }

    #[test]
    fn recover_from_wal_replays_committed_txn_writes() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        FileBackend::open(dir, &test_config())
            .unwrap()
            .flush()
            .unwrap();

        {
            let mut w = WalWriter::open(&dir.join("wal.log")).unwrap();
            w.append(WalRecord::Begin { lsn: 0, txn_id: 1 }).unwrap();
            w.append(WalRecord::WriteNode {
                lsn: 0,
                page_id: 0,
                slot_idx: 0,
                slot: Box::new(live_node_slot()),
                txn_id: Some(1),
            })
            .unwrap();
            w.append(WalRecord::Commit { lsn: 0, txn_id: 1 }).unwrap();
        }

        let backend = FileBackend::open(
            dir,
            &GraphConfig {
                create_if_missing: false,
                ..test_config()
            },
        )
        .unwrap();
        assert_eq!(
            slot0_flags(&backend, DataFile::Nodes),
            crate::storage::codec::node_codec::SLOT_LIVE
        );
    }

    #[test]
    fn recover_from_wal_replays_auto_commit_writes_unconditionally() {
        // A WriteNode with txn_id: None (auto-commit) and no Begin/Commit must
        // always replay — the v0.9.0 behavior for 100% of today's writes.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        FileBackend::open(dir, &test_config())
            .unwrap()
            .flush()
            .unwrap();

        {
            let mut w = WalWriter::open(&dir.join("wal.log")).unwrap();
            w.append(WalRecord::WriteNode {
                lsn: 0,
                page_id: 0,
                slot_idx: 0,
                slot: Box::new(live_node_slot()),
                txn_id: None,
            })
            .unwrap();
        }

        let backend = FileBackend::open(
            dir,
            &GraphConfig {
                create_if_missing: false,
                ..test_config()
            },
        )
        .unwrap();
        assert_eq!(
            slot0_flags(&backend, DataFile::Nodes),
            crate::storage::codec::node_codec::SLOT_LIVE
        );
    }
}
