// SPDX-License-Identifier: MIT

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::RwLock;

use crate::Error;
use crate::error::Result;
use crate::storage::backend::{DataFile, PageId};
use crate::storage::page::{PAGE_SIZE, PageBuf, magic, new_page_buf, validate_page_buf};

/// Lock-poison message for the buffer pool's `RwLock`.
const LOCK_POISON_MSG: &str = "buffer_pool lock poisoned";

/// Default memory limit: 64 MB = 16384 pages.
#[cfg(test)]
const DEFAULT_MAX_PAGES: usize = 16384;

/// Minimum pool size: 8 pages (enforced by validation).
const MIN_POOL_PAGES: usize = 8;

/// Maps a [`DataFile`] to the page magic its pages must carry on disk.
const fn expected_magic(data_file: DataFile) -> [u8; 4] {
    match data_file {
        DataFile::Nodes => magic::NODES,
        DataFile::Edges => magic::EDGES,
        DataFile::Adjacency => magic::ADJACENCY,
        DataFile::Strings => magic::STRINGS,
        DataFile::Overflow => magic::OVERFLOW,
    }
}

/// A single cached page frame in the buffer pool.
struct BufferFrame {
    data: PageBuf,
    dirty: bool,
    pin_count: u32,
}

/// LRU buffer pool for page-level caching.
///
/// Sits between `FileBackend` and the OS file handles. Pages are cached
/// in memory, evicted when the pool reaches its capacity limit. Dirty
/// pages are flushed to disk on eviction or explicit flush.
///
/// The pool holds references to all data file handles so that eviction
/// can write dirty pages to the correct file regardless of which file
/// triggered the capacity check.
///
/// Internal state is wrapped in `RwLock` so that read operations
/// (`get_page`) can take `&self`. This enables `Graph::node()` and
/// similar read methods to take `&self` instead of `&mut self`,
/// which is required for Phase 4 traversal patterns. The `RwLock`
/// also makes `BufferPool` `Send + Sync` for multi-threaded use.
pub struct BufferPool {
    inner: RwLock<PoolInner>,
    max_pages: usize,
}

/// One entry in the intrusive doubly-linked LRU list. Both links are the
/// page's own `(DataFile, PageId)` key, which addresses the neighbour node in
/// [`PoolInner::lru_nodes`] — the key acts as a safe logical pointer, so the
/// list needs no raw pointers (the crate forbids `unsafe`).
struct LruNode {
    /// Towards the most-recently-used end (`lru_head`). `None` at the head.
    prev: Option<(DataFile, PageId)>,
    /// Towards the least-recently-used end (`lru_tail`). `None` at the tail.
    next: Option<(DataFile, PageId)>,
}

struct PoolInner {
    frames: HashMap<(DataFile, PageId), BufferFrame>,
    /// Intrusive doubly-linked LRU list keyed by page. `lru_head` is the
    /// most-recently-used page, `lru_tail` the least-recently-used (the
    /// eviction end). Every operation (touch, insert, unlink) is O(1): the
    /// node is located directly by key, never by scanning.
    lru_nodes: HashMap<(DataFile, PageId), LruNode>,
    lru_head: Option<(DataFile, PageId)>,
    lru_tail: Option<(DataFile, PageId)>,
    files: HashMap<DataFile, File>,
    /// Test-only counter recording how many LRU entries the most recent
    /// `touch_lru_inner` call had to visit. Used by the complexity-regression
    /// test to assert that touching a cached page is O(1), independent of how
    /// many pages are cached. An `AtomicUsize` (not a `Cell`) so `PoolInner`
    /// stays `Sync`, which `BufferPool` requires. Compiled out entirely in
    /// non-test builds.
    #[cfg(test)]
    touch_steps: std::sync::atomic::AtomicUsize,
    /// Instrumentation counters for the issue #54 thrashing verification.
    /// Record cache hits, cache misses (disk reads) and evictions (dirty-page
    /// flushes) so a benchmark can separate in-memory cost from I/O cost as the
    /// working set crosses the pool capacity. Compiled only under the
    /// `pool-instrumentation` feature.
    #[cfg(feature = "pool-instrumentation")]
    cache_hits: std::sync::atomic::AtomicU64,
    #[cfg(feature = "pool-instrumentation")]
    cache_misses: std::sync::atomic::AtomicU64,
    #[cfg(feature = "pool-instrumentation")]
    evictions: std::sync::atomic::AtomicU64,
}

impl BufferPool {
    /// Creates a new buffer pool with the given memory limit in bytes.
    ///
    /// The limit is converted to a page count. Values below the minimum
    /// (8 pages = 32 KB) are clamped up.
    ///
    /// File handles are registered later via `register_file`.
    #[must_use]
    pub fn new(memory_limit_bytes: usize) -> Self {
        let max_pages = (memory_limit_bytes / PAGE_SIZE).max(MIN_POOL_PAGES);
        Self {
            inner: RwLock::new(PoolInner {
                frames: HashMap::new(),
                lru_nodes: HashMap::new(),
                lru_head: None,
                lru_tail: None,
                files: HashMap::new(),
                #[cfg(test)]
                touch_steps: std::sync::atomic::AtomicUsize::new(0),
                #[cfg(feature = "pool-instrumentation")]
                cache_hits: std::sync::atomic::AtomicU64::new(0),
                #[cfg(feature = "pool-instrumentation")]
                cache_misses: std::sync::atomic::AtomicU64::new(0),
                #[cfg(feature = "pool-instrumentation")]
                evictions: std::sync::atomic::AtomicU64::new(0),
            }),
            max_pages,
        }
    }

    /// Snapshot of the instrumentation counters: `(hits, misses, evictions)`.
    /// Only present under the `pool-instrumentation` feature.
    #[cfg(feature = "pool-instrumentation")]
    #[must_use]
    pub fn instrumentation(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let inner = self.inner.read().expect(LOCK_POISON_MSG);
        (
            inner.cache_hits.load(Relaxed),
            inner.cache_misses.load(Relaxed),
            inner.evictions.load(Relaxed),
        )
    }

    /// Resets the instrumentation counters to zero.
    /// Only present under the `pool-instrumentation` feature.
    #[cfg(feature = "pool-instrumentation")]
    pub fn reset_instrumentation(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let inner = self.inner.read().expect(LOCK_POISON_MSG);
        inner.cache_hits.store(0, Relaxed);
        inner.cache_misses.store(0, Relaxed);
        inner.evictions.store(0, Relaxed);
    }

    /// Creates a buffer pool with the default 64 MB limit.
    #[must_use]
    #[cfg(test)]
    pub fn with_default_limit() -> Self {
        Self::new(DEFAULT_MAX_PAGES * PAGE_SIZE)
    }

    /// Registers a file handle for a data file type.
    pub fn register_file(&self, data_file: DataFile, file: File) {
        self.inner
            .write()
            .expect(LOCK_POISON_MSG)
            .files
            .insert(data_file, file);
    }

    /// Returns an owned copy of the cached page, loading from disk if needed.
    ///
    /// Uses **two-phase locking** for concurrency: a shared read lock is
    /// tried first for cache hits (the common case in read-heavy workloads),
    /// falling back to an exclusive write lock only on cache miss.
    ///
    /// On the read-lock fast path the LRU touch is skipped. This is
    /// acceptable because LRU ordering is an eviction-quality optimisation,
    /// not a correctness requirement — the page data returned is always
    /// correct. Pages loaded via the write-lock slow path still receive a
    /// proper LRU touch.
    ///
    /// There is no deadlock risk: the read lock is dropped entirely before
    /// a fresh write lock is acquired (standard double-check locking
    /// pattern, not an in-place upgrade).
    ///
    /// If the page is not cached and the pool is full, an LRU victim with
    /// `pin_count == 0` is evicted (flushed if dirty).
    ///
    /// Returns a `PageBuf` (owned) so callers never hold a lock guard,
    /// eliminating the race where an eviction between the lock release and
    /// a subsequent acquire could remove the page.
    #[allow(clippy::significant_drop_tightening)]
    pub fn get_page(&self, file: DataFile, page_id: PageId) -> Result<PageBuf> {
        let key = (file, page_id);

        // Phase 1: try read lock for cache hit (fast path).
        {
            let inner = self.inner.read().expect(LOCK_POISON_MSG);
            if let Some(frame) = inner.frames.get(&key) {
                #[cfg(feature = "pool-instrumentation")]
                inner
                    .cache_hits
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut copy = new_page_buf();
                copy.copy_from_slice(frame.data.as_ref());
                return Ok(copy);
                // LRU touch skipped — acceptable for read-heavy workloads.
            }
        }
        // Read lock dropped here.

        // Phase 2: write lock for cache miss (slow path).
        let mut inner = self.inner.write().expect(LOCK_POISON_MSG);

        // Re-check after acquiring write lock — another thread may have
        // loaded the page between the read-lock drop and write-lock acquire.
        if !inner.frames.contains_key(&key) {
            #[cfg(feature = "pool-instrumentation")]
            inner
                .cache_misses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self::ensure_capacity_inner(&mut inner, self.max_pages)?;
            let disk_file = inner
                .files
                .get_mut(&file)
                .ok_or_else(|| Error::CorruptPage {
                    file: file.file_name(),
                    page_id: 0,
                    reason: "data file not registered",
                })?;
            let data = Self::read_from_disk(disk_file, file, page_id)?;
            let frame = BufferFrame {
                data,
                dirty: false,
                pin_count: 0,
            };
            inner.frames.insert(key, frame);
            // No explicit LRU insert here: the `touch_lru_inner` below links
            // the freshly loaded page at the MRU head (unlink is a no-op for a
            // key not yet in the list, then push_front inserts it).
        }

        Self::touch_lru_inner(&mut inner, key);
        Self::debug_assert_lru_synced(&inner);

        let mut copy = new_page_buf();
        copy.copy_from_slice(inner.frames[&key].data.as_ref());
        drop(inner);
        Ok(copy)
    }

    /// Writes page data into the cache, marking it dirty.
    ///
    /// The page is not immediately written to disk — it will be flushed
    /// on eviction or explicit `flush_all`.
    #[allow(clippy::significant_drop_tightening)]
    pub fn put_page(&self, file: DataFile, page_id: PageId, data: &PageBuf) -> Result<()> {
        let mut inner = self.inner.write().expect(LOCK_POISON_MSG);
        let key = (file, page_id);

        if let Some(frame) = inner.frames.get_mut(&key) {
            frame.data.copy_from_slice(data.as_ref());
            frame.dirty = true;
            #[cfg(feature = "pool-instrumentation")]
            inner
                .cache_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self::touch_lru_inner(&mut inner, key);
            return Ok(());
        }

        // Not cached — evict if full, then insert
        #[cfg(feature = "pool-instrumentation")]
        inner
            .cache_misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self::ensure_capacity_inner(&mut inner, self.max_pages)?;
        let mut buf = new_page_buf();
        buf.copy_from_slice(data.as_ref());
        let frame = BufferFrame {
            data: buf,
            dirty: true,
            pin_count: 0,
        };
        inner.frames.insert(key, frame);
        Self::lru_push_front_inner(&mut inner, key);
        Self::debug_assert_lru_synced(&inner);

        Ok(())
    }

    /// Flushes dirty pages for a single data file.
    #[allow(clippy::significant_drop_tightening)]
    pub fn flush_file(&self, file: DataFile) -> Result<()> {
        let mut inner = self.inner.write().expect(LOCK_POISON_MSG);
        // Collect dirty page ids first to avoid borrow conflict between
        // files (needs &mut File) and frames (needs &mut BufferFrame).
        let dirty_pages: Vec<PageId> = inner
            .frames
            .iter()
            .filter(|((df, _), frame)| *df == file && frame.dirty)
            .map(|((_, pid), _)| *pid)
            .collect();
        for pid in dirty_pages {
            // Split the borrow: get file handle and frame data separately.
            let frame = inner.frames.get(&(file, pid)).expect("just collected");
            let mut page_copy = new_page_buf();
            page_copy.copy_from_slice(frame.data.as_ref());

            let disk_file = inner
                .files
                .get_mut(&file)
                .ok_or_else(|| Error::CorruptPage {
                    file: file.file_name(),
                    page_id: 0,
                    reason: "data file not registered",
                })?;
            Self::write_to_disk(disk_file, pid, &page_copy)?;
            inner
                .frames
                .get_mut(&(file, pid))
                .expect("just collected")
                .dirty = false;
        }
        Ok(())
    }

    /// Returns the number of pages currently cached.
    #[must_use]
    #[cfg(test)]
    pub fn cached_count(&self) -> usize {
        self.inner.read().expect(LOCK_POISON_MSG).frames.len()
    }

    /// Returns the maximum number of pages this pool can hold.
    #[must_use]
    #[cfg(test)]
    pub const fn max_pages(&self) -> usize {
        self.max_pages
    }

    /// Returns `true` if the given page is cached and dirty.
    #[must_use]
    #[cfg(test)]
    pub fn is_dirty(&self, file: DataFile, page_id: PageId) -> bool {
        self.inner
            .read()
            .expect(LOCK_POISON_MSG)
            .frames
            .get(&(file, page_id))
            .is_some_and(|f| f.dirty)
    }

    /// Test-only: pins a cached page (increments its `pin_count`) so eviction
    /// must skip it. Returns `true` if the page was cached and pinned.
    #[cfg(test)]
    pub(crate) fn pin_for_test(&self, file: DataFile, page_id: PageId) -> bool {
        let mut inner = self.inner.write().expect(LOCK_POISON_MSG);
        if let Some(frame) = inner.frames.get_mut(&(file, page_id)) {
            frame.pin_count += 1;
            true
        } else {
            false
        }
    }

    /// Invalidates (removes) a cached page without flushing.
    #[cfg(test)]
    #[allow(clippy::significant_drop_tightening)]
    pub fn invalidate(&self, file: DataFile, page_id: PageId) {
        let mut inner = self.inner.write().expect(LOCK_POISON_MSG);
        let key = (file, page_id);
        inner.frames.remove(&key);
        Self::lru_unlink_inner(&mut inner, key);
        Self::debug_assert_lru_synced(&inner);
    }

    // ── Internal helpers ────────────────────────────────────────────

    /// Debug-only invariant: the frame map and the LRU node map hold exactly
    /// the same set of pages. A mismatch means some code path mutated one
    /// without the other (a logical leak) — the class of bug most likely when
    /// maintaining an intrusive list by hand. Compiled out in release.
    #[inline]
    fn debug_assert_lru_synced(inner: &PoolInner) {
        debug_assert_eq!(
            inner.frames.len(),
            inner.lru_nodes.len(),
            "buffer pool LRU desync: frames and lru_nodes hold different page sets"
        );
    }

    /// Removes `key` from the intrusive LRU list, re-joining its neighbours.
    ///
    /// O(1): the node is found directly in `lru_nodes` and only the two
    /// neighbours (and possibly `lru_head`/`lru_tail`) are updated. A no-op if
    /// `key` is not currently linked, so it is safe to call unconditionally
    /// (e.g. from `invalidate`).
    fn lru_unlink_inner(inner: &mut PoolInner, key: (DataFile, PageId)) {
        let Some(node) = inner.lru_nodes.remove(&key) else {
            return;
        };
        // Re-join the MRU-side neighbour (`prev`) to the LRU-side one (`next`).
        match node.prev {
            Some(prev_key) => {
                if let Some(prev_node) = inner.lru_nodes.get_mut(&prev_key) {
                    prev_node.next = node.next;
                }
            }
            None => inner.lru_head = node.next,
        }
        match node.next {
            Some(next_key) => {
                if let Some(next_node) = inner.lru_nodes.get_mut(&next_key) {
                    next_node.prev = node.prev;
                }
            }
            None => inner.lru_tail = node.prev,
        }
    }

    /// Inserts `key` at the most-recently-used end (`lru_head`).
    ///
    /// O(1). `key` must not already be linked (callers `unlink` first when
    /// re-touching an existing page).
    fn lru_push_front_inner(inner: &mut PoolInner, key: (DataFile, PageId)) {
        let old_head = inner.lru_head;
        inner.lru_nodes.insert(
            key,
            LruNode {
                prev: None,
                next: old_head,
            },
        );
        match old_head {
            Some(head_key) => {
                if let Some(head_node) = inner.lru_nodes.get_mut(&head_key) {
                    head_node.prev = Some(key);
                }
            }
            None => inner.lru_tail = Some(key),
        }
        inner.lru_head = Some(key);
    }

    /// Marks `key` as most-recently-used: unlink from its current position and
    /// re-insert at the head. O(1), independent of how many pages are cached.
    fn touch_lru_inner(inner: &mut PoolInner, key: (DataFile, PageId)) {
        #[cfg(test)]
        inner.touch_steps.store(
            usize::from(inner.lru_nodes.contains_key(&key)),
            std::sync::atomic::Ordering::Relaxed,
        );
        Self::lru_unlink_inner(inner, key);
        Self::lru_push_front_inner(inner, key);
    }

    /// Test-only: number of LRU entries the most recent `touch_lru_inner`
    /// visited. See [`PoolInner::touch_steps`].
    #[cfg(test)]
    pub(crate) fn last_touch_steps(&self) -> usize {
        self.inner
            .read()
            .expect(LOCK_POISON_MSG)
            .touch_steps
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Ensures at least one free slot by evicting LRU victims if needed.
    fn ensure_capacity_inner(inner: &mut PoolInner, max_pages: usize) -> Result<()> {
        while inner.frames.len() >= max_pages {
            Self::evict_one_inner(inner)?;
        }
        Ok(())
    }

    /// Evicts the least recently used unpinned page.
    ///
    /// Walks the intrusive LRU list from `lru_tail` (the least-recently-used
    /// end) towards `lru_head`, following each node's `prev` link, and evicts
    /// the first page whose `pin_count` is `0`. In the normal case (few or no
    /// pinned pages) this is O(1) amortised — the first or second candidate is
    /// evictable. In the pathological case where many pages nearest the tail
    /// are pinned simultaneously, it degrades to O(k) where `k` is the run of
    /// pinned pages from the tail — still strictly better than the previous
    /// `VecDeque` scan, which was `O(pool_size)` on every call regardless of
    /// how many pages were pinned. If every page is pinned, returns
    /// [`Error::BufferPoolExhausted`].
    fn evict_one_inner(inner: &mut PoolInner) -> Result<()> {
        let mut cursor = inner.lru_tail;
        let victim = loop {
            let Some(key) = cursor else {
                return Err(Error::BufferPoolExhausted);
            };
            if inner.frames.get(&key).is_some_and(|f| f.pin_count == 0) {
                break key;
            }
            // Advance towards the most-recently-used end.
            cursor = inner.lru_nodes.get(&key).and_then(|node| node.prev);
        };

        // Unlink and drop the frame before the (possibly failing) flush, so a
        // write error never leaves the node orphaned in one map but not the
        // other.
        Self::lru_unlink_inner(inner, victim);
        #[cfg(feature = "pool-instrumentation")]
        inner
            .evictions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (victim_file, victim_page_id) = victim;
        let removed = inner.frames.remove(&victim);
        // Both maps have now dropped the victim, so they are back in sync even
        // if the flush below fails and returns early.
        Self::debug_assert_lru_synced(inner);
        if let Some(frame) = removed {
            if frame.dirty {
                let disk_file =
                    inner
                        .files
                        .get_mut(&victim_file)
                        .ok_or_else(|| Error::CorruptPage {
                            file: victim_file.file_name(),
                            page_id: 0,
                            reason: "data file not registered",
                        })?;
                Self::write_to_disk(disk_file, victim_page_id, &frame.data)?;
            }
        }

        Ok(())
    }

    /// Reads a page from disk at the given page offset, validating its magic
    /// and CRC32 against the expected values for `data_file`.
    ///
    /// # Errors
    ///
    /// - [`Error::CorruptPage`] if the page has a non-zero, unrecognized magic
    ///   (data written with a wrong/garbled magic).
    /// - [`Error::ChecksumMismatch`] if the page magic is valid but the stored
    ///   CRC32 does not match the CRC32 computed over the payload (data written
    ///   and then corrupted on disk).
    ///
    /// A fully zeroed page is a legitimate, never-written page: `allocate_page`
    /// reserves a zeroed page that may be flushed to disk before its first
    /// slot-write. Such a page is returned as-is, without magic/CRC validation,
    /// so the allocate -> evict -> read sequence in
    /// `Graph::write_slot_to_page` stays valid.
    fn read_from_disk(file: &mut File, data_file: DataFile, page_id: PageId) -> Result<PageBuf> {
        let offset = u64::from(page_id) * PAGE_SIZE as u64;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = new_page_buf();
        file.read_exact(buf.as_mut())?;

        validate_page_buf(
            &buf,
            expected_magic(data_file),
            data_file.file_name(),
            page_id,
        )?;
        Ok(buf)
    }

    /// Writes a page to disk at the given page offset.
    fn write_to_disk(file: &mut File, page_id: PageId, data: &PageBuf) -> Result<()> {
        let offset = u64::from(page_id) * PAGE_SIZE as u64;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data.as_ref())?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::significant_drop_tightening)]
mod tests {
    use super::*;
    use crate::storage::page::PAGE_HEADER_SIZE;
    use std::io::Write as IoWrite;
    use tempfile::NamedTempFile;

    /// Creates a temporary file with `n` zeroed pages and registers it.
    fn pool_with_file(max_pages: usize, num_disk_pages: u32) -> (BufferPool, NamedTempFile) {
        let mut f = NamedTempFile::new().unwrap();
        let zeroed = [0u8; PAGE_SIZE];
        for _ in 0..num_disk_pages {
            f.write_all(&zeroed).unwrap();
        }
        f.flush().unwrap();

        // Bypass MIN_POOL_PAGES clamping for tests that need small pools
        let pool = BufferPool {
            inner: RwLock::new(PoolInner {
                frames: HashMap::new(),
                lru_nodes: HashMap::new(),
                lru_head: None,
                lru_tail: None,
                files: HashMap::new(),
                touch_steps: std::sync::atomic::AtomicUsize::new(0),
                #[cfg(feature = "pool-instrumentation")]
                cache_hits: std::sync::atomic::AtomicU64::new(0),
                #[cfg(feature = "pool-instrumentation")]
                cache_misses: std::sync::atomic::AtomicU64::new(0),
                #[cfg(feature = "pool-instrumentation")]
                evictions: std::sync::atomic::AtomicU64::new(0),
            }),
            max_pages,
        };

        // Clone the file handle for pool registration
        let pool_file = f.as_file().try_clone().unwrap();
        pool.register_file(DataFile::Nodes, pool_file);

        (pool, f)
    }

    /// Writes a finalized (valid magic + CRC) NODES page into `page_id` of the
    /// temp file, with `marker` stamped into the first payload byte. The marker
    /// is read back at offset `PAGE_HEADER_SIZE`, since byte 0 now holds the
    /// page magic (validated on read by [`BufferPool::read_from_disk`]).
    fn write_page_to_file(f: &mut File, page_id: u32, marker: u8) {
        use crate::storage::page::{PageType, finalize_page, magic};
        let offset = u64::from(page_id) * PAGE_SIZE as u64;
        f.seek(SeekFrom::Start(offset)).unwrap();
        let mut buf = new_page_buf();
        buf[PAGE_HEADER_SIZE] = marker;
        finalize_page(buf.as_mut(), magic::NODES, 1, PageType::Node, 0);
        f.write_all(buf.as_ref()).unwrap();
        f.flush().unwrap();
    }

    #[test]
    fn new_pool_is_empty() {
        let pool = BufferPool::new(64 * 1024);
        assert_eq!(pool.cached_count(), 0);
        assert_eq!(pool.max_pages(), 16);
    }

    #[test]
    fn min_pool_size_clamped() {
        let pool = BufferPool::new(1);
        assert_eq!(pool.max_pages(), MIN_POOL_PAGES);
    }

    #[test]
    fn default_limit() {
        let pool = BufferPool::with_default_limit();
        assert_eq!(pool.max_pages(), DEFAULT_MAX_PAGES);
    }

    #[test]
    fn get_page_loads_from_disk() {
        let (pool, mut tf) = pool_with_file(16, 4);
        write_page_to_file(tf.as_file_mut(), 2, 0xAB);

        let page = pool.get_page(DataFile::Nodes, 2).unwrap();
        assert_eq!(page[PAGE_HEADER_SIZE], 0xAB);
        assert_eq!(pool.cached_count(), 1);
    }

    #[test]
    fn get_page_returns_cached() {
        let (pool, mut tf) = pool_with_file(16, 4);
        write_page_to_file(tf.as_file_mut(), 1, 0xCD);

        pool.get_page(DataFile::Nodes, 1).unwrap();
        // Modify file directly — cached version should be returned
        write_page_to_file(tf.as_file_mut(), 1, 0xEF);

        let page = pool.get_page(DataFile::Nodes, 1).unwrap();
        assert_eq!(page[PAGE_HEADER_SIZE], 0xCD);
    }

    #[test]
    fn get_page_takes_shared_ref() {
        let (pool, mut tf) = pool_with_file(16, 4);
        write_page_to_file(tf.as_file_mut(), 0, 0xAA);
        // &self is sufficient for reading
        let page = pool.get_page(DataFile::Nodes, 0).unwrap();
        assert_eq!(page[PAGE_HEADER_SIZE], 0xAA);
    }

    #[test]
    fn two_sequential_reads_do_not_conflict() {
        let (pool, mut tf) = pool_with_file(16, 4);
        write_page_to_file(tf.as_file_mut(), 0, 0x11);
        write_page_to_file(tf.as_file_mut(), 1, 0x22);
        // Two sequential reads from the same shared reference — each returns
        // an owned PageBuf so there is no lock contention between them.
        let p0 = pool.get_page(DataFile::Nodes, 0).unwrap();
        assert_eq!(p0[PAGE_HEADER_SIZE], 0x11);
        drop(p0);
        let p1 = pool.get_page(DataFile::Nodes, 1).unwrap();
        assert_eq!(p1[PAGE_HEADER_SIZE], 0x22);
    }

    #[test]
    fn put_page_marks_dirty() {
        let (pool, _tf) = pool_with_file(16, 4);

        let mut data = new_page_buf();
        data[0] = 0x42;
        pool.put_page(DataFile::Nodes, 0, &data).unwrap();

        assert!(pool.is_dirty(DataFile::Nodes, 0));
        assert_eq!(pool.cached_count(), 1);
    }

    #[test]
    fn put_page_overwrites_cached() {
        let (pool, _tf) = pool_with_file(16, 4);

        let mut data1 = new_page_buf();
        data1[0] = 0x11;
        pool.put_page(DataFile::Nodes, 0, &data1).unwrap();

        let mut data2 = new_page_buf();
        data2[0] = 0x22;
        pool.put_page(DataFile::Nodes, 0, &data2).unwrap();

        let page = pool.get_page(DataFile::Nodes, 0).unwrap();
        assert_eq!(page[0], 0x22);
        assert_eq!(pool.cached_count(), 1);
    }

    #[test]
    fn flush_file_clears_dirty() {
        let (pool, mut tf) = pool_with_file(16, 4);

        let mut data = new_page_buf();
        data[0] = 0x77;
        pool.put_page(DataFile::Nodes, 0, &data).unwrap();
        assert!(pool.is_dirty(DataFile::Nodes, 0));

        pool.flush_file(DataFile::Nodes).unwrap();
        assert!(!pool.is_dirty(DataFile::Nodes, 0));

        // Verify written to disk
        let f = tf.as_file_mut();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut disk_buf = new_page_buf();
        f.read_exact(disk_buf.as_mut()).unwrap();
        assert_eq!(disk_buf[0], 0x77);
    }

    #[test]
    fn eviction_flushes_dirty_page() {
        let (pool, mut tf) = pool_with_file(2, 16);

        let mut data0 = new_page_buf();
        data0[0] = 0xAA;
        pool.put_page(DataFile::Nodes, 0, &data0).unwrap();

        let mut data1 = new_page_buf();
        data1[0] = 0xBB;
        pool.put_page(DataFile::Nodes, 1, &data1).unwrap();

        // This should evict page 0 (LRU)
        let mut data2 = new_page_buf();
        data2[0] = 0xCC;
        pool.put_page(DataFile::Nodes, 2, &data2).unwrap();

        assert_eq!(pool.cached_count(), 2);
        assert!(
            !pool
                .inner
                .read()
                .expect("lock")
                .frames
                .contains_key(&(DataFile::Nodes, 0))
        );

        // Page 0 should have been flushed to disk
        let f = tf.as_file_mut();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut on_disk = new_page_buf();
        f.read_exact(on_disk.as_mut()).unwrap();
        assert_eq!(on_disk[0], 0xAA);
    }

    #[test]
    fn lru_order_updated_on_put() {
        // put_page (write path) still touches LRU, so re-putting a page
        // moves it to the back and protects it from eviction.
        let (pool, _tf) = pool_with_file(3, 16);

        for i in 0..3 {
            let mut data = new_page_buf();
            data[0] = i;
            pool.put_page(DataFile::Nodes, u32::from(i), &data).unwrap();
        }
        // LRU order: [0, 1, 2]

        // Re-put page 0 — moves it to most recently used via write path.
        let mut data0 = new_page_buf();
        data0[0] = 0x00;
        pool.put_page(DataFile::Nodes, 0, &data0).unwrap();
        // LRU order: [1, 2, 0]

        // Insert page 3 — should evict page 1 (now LRU).
        let mut data3 = new_page_buf();
        data3[0] = 0x33;
        pool.put_page(DataFile::Nodes, 3, &data3).unwrap();

        let inner = pool.inner.read().expect("lock");
        assert!(inner.frames.contains_key(&(DataFile::Nodes, 0)));
        assert!(!inner.frames.contains_key(&(DataFile::Nodes, 1)));
        assert!(inner.frames.contains_key(&(DataFile::Nodes, 2)));
        assert!(inner.frames.contains_key(&(DataFile::Nodes, 3)));
    }

    #[test]
    fn get_page_cache_hit_skips_lru_touch() {
        // get_page uses the read-lock fast path for cache hits, which
        // intentionally skips the LRU touch. Verify that a read does NOT
        // protect a page from eviction.
        let (pool, _tf) = pool_with_file(3, 16);

        for i in 0..3 {
            let mut data = new_page_buf();
            data[0] = i;
            pool.put_page(DataFile::Nodes, u32::from(i), &data).unwrap();
        }
        // LRU order: [0, 1, 2]

        // Read page 0 — does NOT update LRU (read-lock fast path).
        pool.get_page(DataFile::Nodes, 0).unwrap();

        // Insert page 3 — evicts page 0 (still LRU despite the read).
        let mut data3 = new_page_buf();
        data3[0] = 0x33;
        pool.put_page(DataFile::Nodes, 3, &data3).unwrap();

        let inner = pool.inner.read().expect("lock");
        assert!(!inner.frames.contains_key(&(DataFile::Nodes, 0)));
        assert!(inner.frames.contains_key(&(DataFile::Nodes, 1)));
        assert!(inner.frames.contains_key(&(DataFile::Nodes, 2)));
        assert!(inner.frames.contains_key(&(DataFile::Nodes, 3)));
    }

    #[test]
    fn invalidate_removes_page() {
        let (pool, _tf) = pool_with_file(16, 4);

        let data = new_page_buf();
        pool.put_page(DataFile::Nodes, 0, &data).unwrap();
        assert_eq!(pool.cached_count(), 1);

        pool.invalidate(DataFile::Nodes, 0);
        assert_eq!(pool.cached_count(), 0);
        assert!(!pool.is_dirty(DataFile::Nodes, 0));
    }

    #[test]
    fn buffer_pool_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BufferPool>();
    }

    #[test]
    fn is_dirty_false_for_uncached() {
        let pool = BufferPool::new(64 * 1024);
        assert!(!pool.is_dirty(DataFile::Nodes, 0));
    }

    // --- Cycle G-C2: get_page returns PageBuf (owned) ---

    #[test]
    fn get_page_returns_owned_buf() {
        let (pool, mut tf) = pool_with_file(16, 2);
        write_page_to_file(tf.as_file_mut(), 0, 0xDE);
        let buf: PageBuf = pool.get_page(DataFile::Nodes, 0).unwrap();
        assert_eq!(buf[PAGE_HEADER_SIZE], 0xDE);
    }

    #[test]
    fn held_page_buf_does_not_block_writes() {
        let (pool, mut tf) = pool_with_file(16, 2);
        write_page_to_file(tf.as_file_mut(), 0, 0x01);

        let buf = pool.get_page(DataFile::Nodes, 0).unwrap();
        // This must not deadlock (would if buf held a read lock):
        let mut write_data = new_page_buf();
        write_data[0] = 0x02;
        pool.put_page(DataFile::Nodes, 0, &write_data).unwrap();
        drop(buf);
    }

    #[test]
    fn concurrent_reads_do_not_serialize() {
        use std::sync::Arc;
        use std::thread;

        let (pool, mut tf) = pool_with_file(16, 4);
        write_page_to_file(tf.as_file_mut(), 0, 0xAA);
        write_page_to_file(tf.as_file_mut(), 1, 0xBB);
        write_page_to_file(tf.as_file_mut(), 2, 0xCC);

        // Pre-load pages into cache so all reads hit the read-lock fast path.
        pool.get_page(DataFile::Nodes, 0).unwrap();
        pool.get_page(DataFile::Nodes, 1).unwrap();
        pool.get_page(DataFile::Nodes, 2).unwrap();

        let pool = Arc::new(pool);
        let handles: Vec<_> = (0..4)
            .map(|thread_id| {
                let pool = Arc::clone(&pool);
                thread::spawn(move || {
                    let page_id = thread_id % 3; // spread across 3 pages
                    // allow: test fixture
                    #[allow(clippy::cast_sign_loss)]
                    let expected = [0xAA, 0xBB, 0xCC][page_id as usize];
                    for _ in 0..1000 {
                        // allow: test fixture
                        #[allow(clippy::cast_sign_loss)]
                        let page = pool.get_page(DataFile::Nodes, page_id as u32).unwrap();
                        assert_eq!(page[PAGE_HEADER_SIZE], expected);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn two_concurrent_reads_from_threads() {
        use std::sync::Arc;
        use std::thread;

        let (pool, mut tf) = pool_with_file(16, 4);
        write_page_to_file(tf.as_file_mut(), 0, 0xAA);
        write_page_to_file(tf.as_file_mut(), 1, 0xBB);
        let pool = Arc::new(pool);

        let p0 = Arc::clone(&pool);
        let t0 = thread::spawn(move || {
            let page: PageBuf = p0.get_page(DataFile::Nodes, 0).unwrap();
            assert_eq!(page[PAGE_HEADER_SIZE], 0xAA);
        });

        let p1 = Arc::clone(&pool);
        let t1 = thread::spawn(move || {
            let page: PageBuf = p1.get_page(DataFile::Nodes, 1).unwrap();
            assert_eq!(page[PAGE_HEADER_SIZE], 0xBB);
        });

        t0.join().unwrap();
        t1.join().unwrap();
    }

    // --- Feature A: checksums on-read (Cycle A-1 / A-2) ---

    #[test]
    fn read_from_disk_corrupt_crc_returns_checksum_mismatch() {
        // Write a valid finalized NODES page, then corrupt a single payload
        // byte on disk (without recomputing the CRC). get_page must return
        // Error::ChecksumMismatch.
        let (pool, mut tf) = pool_with_file(16, 4);
        write_page_to_file(tf.as_file_mut(), 0, 0xAA);

        // Corrupt one payload byte in place (page 0, payload offset 0).
        tf.as_file_mut()
            .seek(SeekFrom::Start(PAGE_HEADER_SIZE as u64))
            .unwrap();
        tf.as_file_mut().write_all(&[0xFF]).unwrap();
        tf.as_file_mut().flush().unwrap();

        let result = pool.get_page(DataFile::Nodes, 0);
        assert!(
            matches!(result, Err(Error::ChecksumMismatch { .. })),
            "expected ChecksumMismatch, got {result:?}"
        );
    }

    #[test]
    fn read_zeroed_page_is_valid_never_written_page() {
        // A fully zeroed page is a never-written page: allocate_page reserves a
        // zeroed page that can be flushed to disk before its first slot-write
        // (e.g. evicted under memory pressure). Reading it back must return the
        // zeroed page as-is, NOT an error — an allocated-but-unwritten page is
        // legitimate engine state, not corruption.
        let (pool, _tf) = pool_with_file(16, 4);

        let page = pool
            .get_page(DataFile::Nodes, 0)
            .expect("zeroed never-written page must be readable");
        assert!(page.iter().all(|&b| b == 0));
    }

    #[test]
    fn read_nonzero_page_with_invalid_magic_reports_corrupt() {
        // A page that carries actual data but an unrecognized (non-zero) magic
        // is corruption — data written with a wrong/garbled magic. It must
        // produce CorruptPage, distinct from both a valid empty page (all-zero)
        // and a CRC mismatch (valid magic, bad payload).
        let mut f = NamedTempFile::new().unwrap();
        let mut buf = [0u8; PAGE_SIZE];
        buf[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // bogus magic
        buf[PAGE_HEADER_SIZE] = 0x42; // some payload so it is not all-zero
        f.write_all(&buf).unwrap();
        f.flush().unwrap();

        let pool = BufferPool {
            inner: RwLock::new(PoolInner {
                frames: HashMap::new(),
                lru_nodes: HashMap::new(),
                lru_head: None,
                lru_tail: None,
                files: HashMap::new(),
                touch_steps: std::sync::atomic::AtomicUsize::new(0),
                #[cfg(feature = "pool-instrumentation")]
                cache_hits: std::sync::atomic::AtomicU64::new(0),
                #[cfg(feature = "pool-instrumentation")]
                cache_misses: std::sync::atomic::AtomicU64::new(0),
                #[cfg(feature = "pool-instrumentation")]
                evictions: std::sync::atomic::AtomicU64::new(0),
            }),
            max_pages: 16,
        };
        pool.register_file(DataFile::Nodes, f.as_file().try_clone().unwrap());

        let result = pool.get_page(DataFile::Nodes, 0);
        assert!(
            matches!(result, Err(Error::CorruptPage { .. })),
            "expected CorruptPage for non-zero page with invalid magic, got {result:?}"
        );
    }

    /// Fills a pool with `n` cached pages via `put_page` (each a fresh miss),
    /// then re-touches the oldest page (page 0 — worst case for a front-to-back
    /// scan) and returns how many LRU entries that touch had to visit.
    #[cfg(test)]
    fn touch_steps_for_cached_pages(cached: u32) -> usize {
        // Pool capacity well above `cached` so no eviction happens: every page
        // stays resident and the LRU list length equals `cached`.
        let (pool, _tf) = pool_with_file(cached as usize * 2, cached);
        for i in 0..cached {
            let mut data = new_page_buf();
            data[0] = u8::try_from(i % 256).unwrap();
            pool.put_page(DataFile::Nodes, i, &data).unwrap();
        }
        // Re-put page 0 (oldest / least-recently-used): a cache hit that fires
        // `touch_lru_inner`. Before the fix this scanned the whole LRU list.
        let mut data0 = new_page_buf();
        data0[0] = 0xAB;
        pool.put_page(DataFile::Nodes, 0, &data0).unwrap();
        pool.last_touch_steps()
    }

    #[test]
    fn invalidate_unlinks_from_lru_without_breaking_chain() {
        // Invalidating a page from the MIDDLE of the LRU chain must re-join its
        // neighbours, so eviction order stays correct for the rest.
        let (pool, _tf) = pool_with_file(3, 16);
        for i in 0..3 {
            let mut data = new_page_buf();
            data[0] = i;
            pool.put_page(DataFile::Nodes, u32::from(i), &data).unwrap();
        }
        // LRU (tail→head): 0, 1, 2.  Invalidate the middle page (1).
        pool.invalidate(DataFile::Nodes, 1);
        assert_eq!(pool.cached_count(), 2);

        // Insert two more pages into a capacity-3 pool. First insert (3) fills
        // the freed slot; second (4) forces eviction — the victim must be 0
        // (still the least-recently-used after 1 was removed), proving 0 and 2
        // stayed correctly linked across the middle removal.
        let mut d3 = new_page_buf();
        d3[0] = 0x33;
        pool.put_page(DataFile::Nodes, 3, &d3).unwrap();
        let mut d4 = new_page_buf();
        d4[0] = 0x44;
        pool.put_page(DataFile::Nodes, 4, &d4).unwrap();

        let inner = pool.inner.read().expect("lock");
        assert!(
            !inner.frames.contains_key(&(DataFile::Nodes, 0)),
            "0 should be evicted"
        );
        assert!(
            inner.frames.contains_key(&(DataFile::Nodes, 2)),
            "2 should survive"
        );
        assert!(inner.frames.contains_key(&(DataFile::Nodes, 3)));
        assert!(inner.frames.contains_key(&(DataFile::Nodes, 4)));
    }

    #[test]
    fn eviction_skips_pinned_pages_walking_from_tail() {
        // The LRU (tail) page is pinned; eviction must skip it and evict the
        // next unpinned page towards the head instead.
        let (pool, _tf) = pool_with_file(3, 16);
        for i in 0..3 {
            let mut data = new_page_buf();
            data[0] = i;
            pool.put_page(DataFile::Nodes, u32::from(i), &data).unwrap();
        }
        // LRU (tail→head): 0, 1, 2.  Pin page 0 (the tail / least-recently-used).
        assert!(pool.pin_for_test(DataFile::Nodes, 0));

        // Insert page 3 → must evict 1 (first unpinned from the tail), NOT 0.
        let mut d3 = new_page_buf();
        d3[0] = 0x33;
        pool.put_page(DataFile::Nodes, 3, &d3).unwrap();

        let inner = pool.inner.read().expect("lock");
        assert!(
            inner.frames.contains_key(&(DataFile::Nodes, 0)),
            "pinned 0 must survive"
        );
        assert!(
            !inner.frames.contains_key(&(DataFile::Nodes, 1)),
            "1 should be evicted"
        );
        assert!(inner.frames.contains_key(&(DataFile::Nodes, 2)));
        assert!(inner.frames.contains_key(&(DataFile::Nodes, 3)));
    }

    #[test]
    fn touch_lru_is_o1_independent_of_pool_size() {
        // Touching a cached page must cost the same regardless of how many
        // pages are cached — otherwise a bulk insert (which re-touches pages
        // as their slots fill) is O(N^2). The independent variable is the
        // number of *cached* pages (the real LRU list length), which grows
        // with the data during a bulk insert.
        let steps_small = touch_steps_for_cached_pages(8);
        let steps_large = touch_steps_for_cached_pages(4000);

        assert_eq!(
            steps_small, steps_large,
            "touch cost must not scale with cached-page count \
             (small={steps_small}, large={steps_large}) — LRU touch is not O(1)"
        );
        // And it must be a small constant, not merely equal by coincidence.
        assert!(
            steps_large <= 2,
            "touch must visit a constant, tiny number of entries, got {steps_large}"
        );
    }
}
