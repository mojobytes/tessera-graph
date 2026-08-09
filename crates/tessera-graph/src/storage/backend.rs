// SPDX-License-Identifier: MIT

use crate::error::Result;
use crate::storage::meta::GraphMeta;
use crate::storage::page::PageBuf;
use crate::wal::record::WalRecord;

/// Identifies which data file a page belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataFile {
    Nodes,
    Edges,
    Adjacency,
    Strings,
    Overflow,
}

/// Every data file, in the order their per-file metadata is serialized.
///
/// The order is part of the on-disk format: [`crate::storage::meta::GraphMeta`]
/// writes one free-directory head and one free-page count per file at an offset
/// derived from [`DataFile::index`]. Reordering this array silently reassigns
/// those slots, so a store written before the change would report another
/// file's free pages as its own. Append new files at the end.
pub const ALL_DATA_FILES: [DataFile; 5] = [
    DataFile::Nodes,
    DataFile::Edges,
    DataFile::Adjacency,
    DataFile::Strings,
    DataFile::Overflow,
];

impl DataFile {
    /// Returns the on-disk filename for this data file.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Nodes => "nodes.db",
            Self::Edges => "edges.db",
            Self::Adjacency => "adjacency.db",
            Self::Strings => "strings.db",
            Self::Overflow => "overflow.db",
        }
    }

    /// Position of this file's per-file metadata slots.
    ///
    /// Matches this file's position in [`ALL_DATA_FILES`]. Written explicitly
    /// rather than derived from the enum discriminant so that the on-disk
    /// meaning of each slot cannot change by reordering the enum variants.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Nodes => 0,
            Self::Edges => 1,
            Self::Adjacency => 2,
            Self::Strings => 3,
            Self::Overflow => 4,
        }
    }
}

/// Page identifier within a single data file.
pub type PageId = u32;

/// Abstract storage backend for page-level I/O.
///
/// Two implementations exist:
/// - `MemoryBackend`: HashMap-based, no I/O. Used by `Graph::new()`.
/// - `FileBackend`: Buffer pool + file handles. Used by `Graph::open()`. (Step 12)
pub trait StorageBackend: Send + Sync {
    /// Reads a page by file and page ID. Returns a copy of the page data.
    fn read_page(&self, file: DataFile, page_id: PageId) -> Result<PageBuf>;

    /// Writes page data to the given file and page ID.
    fn write_page(&mut self, file: DataFile, page_id: PageId, data: &PageBuf) -> Result<()>;

    /// Allocates a page in the given file. Returns the page's ID.
    ///
    /// Prefers a page previously released via [`Self::free_page`]; only when
    /// none is available does the file grow. Reusing first is what keeps a
    /// workload that repeatedly rewrites the same entities from growing its
    /// files without bound.
    ///
    /// The returned page's contents are unspecified — a recycled page still
    /// holds its previous occupant's bytes. Callers must write the whole page
    /// before reading it back.
    fn allocate_page(&mut self, file: DataFile) -> Result<PageId>;

    /// Releases a page so a later [`Self::allocate_page`] may hand it out again.
    ///
    /// The caller guarantees `page_id` is no longer reachable from any live
    /// record. Releasing a page that is still referenced is silent data loss:
    /// the next allocation overwrites it and the original owner reads back
    /// another entity's bytes.
    ///
    /// Releasing is idempotent per page only in the sense that the
    /// implementation may reject a double release; callers must not rely on
    /// double-freeing being harmless.
    fn free_page(&mut self, file: DataFile, page_id: PageId) -> Result<()>;

    /// Returns the number of allocated pages in the given file.
    fn page_count(&self, file: DataFile) -> u32;

    /// Flushes all dirty pages to persistent storage.
    fn flush(&mut self) -> Result<()>;

    /// Returns a shared reference to the graph metadata.
    fn meta(&self) -> &GraphMeta;

    /// Returns a mutable reference to the graph metadata.
    fn meta_mut(&mut self) -> &mut GraphMeta;

    /// Reads the persisted label index bytes.
    /// Returns `Ok(None)` if the index file does not exist (first run).
    fn read_index_bytes(&mut self) -> Result<Option<Vec<u8>>>;

    /// Writes the serialized label index bytes to persistent storage.
    fn write_index_bytes(&mut self, data: &[u8]) -> Result<()>;

    /// Reads the persisted DDL schema catalog bytes (`schema.bin`).
    /// Returns `Ok(None)` if the file does not exist (new database, or no DDL
    /// ever issued). The default implementation returns `None` for backends
    /// that do not persist (in-memory).
    fn read_schema_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    /// Writes the serialized DDL schema catalog bytes to persistent storage.
    /// The default implementation is a no-op for non-persistent backends.
    fn write_schema_bytes(&mut self, _data: &[u8]) -> Result<()> {
        Ok(())
    }

    /// Returns `true` if this backend writes WAL records.
    ///
    /// When `false`, callers can skip constructing `WalRecord` objects entirely,
    /// avoiding heap allocations for the 128-byte slot buffer. The default
    /// implementation returns `false` (in-memory backends don't persist WAL).
    fn wal_enabled(&self) -> bool {
        false
    }

    /// Appends a WAL record before applying a mutation. No-op for in-memory backends.
    fn wal_append(&mut self, _record: WalRecord) -> Result<()> {
        Ok(())
    }

    /// Syncs the WAL to disk (`fsync`). Must be called after each complete
    /// logical operation (`write_slot`, tombstone, `adj_page` update) to guarantee
    /// durability. No-op for in-memory backends.
    fn wal_sync(&mut self) -> Result<()> {
        Ok(())
    }

    /// Writes a checkpoint and truncates the WAL. No-op for in-memory backends.
    fn wal_checkpoint_and_truncate(&mut self) -> Result<()> {
        Ok(())
    }

    /// Returns `true` when the WAL has grown past its configured checkpoint
    /// threshold and a checkpoint is therefore due (issue #58).
    ///
    /// Crossing the threshold does not checkpoint on the spot — that would
    /// stall a mutation mid-write. The flag is raised here and acted on at the
    /// next outermost batch close, which is already a synchronisation point.
    ///
    /// The default is `false`: backends with no journal have nothing to bound.
    fn wal_checkpoint_pending(&self) -> bool {
        false
    }

    /// Snapshot of the buffer-pool instrumentation counters
    /// `(hits, misses, evictions)`. Default returns zeros for backends with no
    /// pool. Only present under the `pool-instrumentation` feature (issue #54).
    #[cfg(feature = "pool-instrumentation")]
    fn pool_instrumentation(&self) -> (u64, u64, u64) {
        (0, 0, 0)
    }

    /// Resets the buffer-pool instrumentation counters.
    /// Only present under the `pool-instrumentation` feature (issue #54).
    #[cfg(feature = "pool-instrumentation")]
    fn reset_pool_instrumentation(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_backend_dyn_is_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn StorageBackend>();
    }
}
