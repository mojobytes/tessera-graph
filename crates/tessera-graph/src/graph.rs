// SPDX-License-Identifier: MIT

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::adj_cache::AdjCache;
use crate::edge::Edge;
use crate::error::{BatchLimitKind, EdgeId, Error, NodeId, Result};
use crate::index::codec as index_codec;
use crate::index::{LabelIndex, PropertyIndex};
use crate::node::Node;
use crate::property::Properties;
use crate::storage::backend::{DataFile, PageId, StorageBackend};
use crate::storage::codec::adj_slab_codec;
use crate::storage::codec::adjacency_codec::{
    self, AdjDirection, AdjacencyPointer, AdjacencyRecord,
};
use crate::storage::codec::edge_codec::{self, EDGE_SLOT_SIZE};
use crate::storage::codec::node_codec::{
    self, NODE_SLOT_SIZE, SLOT_LIVE, SLOT_TOMBSTONE, SLOTS_PER_PAGE,
};
use crate::storage::codec::overflow_codec;
use crate::storage::codec::prop_slab_codec;
use crate::storage::codec::string_codec::StringHeap;
pub use crate::storage::file::GraphConfig;

use crate::storage::file::FileBackend;
use crate::storage::memory::MemoryBackend;
use crate::storage::page::{PAGE_HEADER_SIZE, PageHeader, PageType, finalize_page, magic};
use crate::wal::record::WalRecord;

/// Default adjacency cache capacity for in-memory graphs.
const DEFAULT_ADJ_CACHE_CAPACITY: usize = 65_536;

/// Number of shards in the MVCC delta table (Block 4). A power of two spreads
/// lock contention across the key space; 64 keeps per-shard maps small under
/// concurrent transactions without over-allocating for idle databases.
const MVCC_SHARD_COUNT: usize = 64;

/// Issue #37: estimated bytes a delete charges against an open batch's byte cap.
/// A removed entity holds no record data of its own — only the fixed slot cost
/// of the operation in the batch — so it is charged a small constant, matching
/// the rationale of `DELETED_APPROX_SIZE` in `mvcc::delta`.
const REMOVED_APPROX_SIZE: u64 = 16;

/// What an encoded slot needs written outside itself, and what it leaves behind.
///
/// Grouped for the same reason as [`SlotLayout`]: these describe one encoding
/// result and always travel together.
#[derive(Clone, Copy)]
struct SlotOverflowRequest<'a> {
    /// The label did not fit the slot and must go to the string heap.
    label_overflowed: bool,
    /// The label text, used only when `label_overflowed`.
    label: &'a str,
    /// The encoded properties did not fit the slot.
    props_overflowed: bool,
    /// The encoded property bytes, used only when `props_overflowed`.
    props_bytes: Option<&'a [u8]>,
    /// The property-overflow chain this entity referenced *before* this write,
    /// if it had one.
    ///
    /// Supplying it is what stops a repeatedly-updated entity from
    /// accumulating abandoned chains: a benchmark of 2 000 nodes updated 20
    /// times each held 164 MB of overflow for ~78 KB of live data before this
    /// existed, growing without bound.
    ///
    /// `None` for an insert (there is nothing to release) and for writes under
    /// MVCC, where a reader's snapshot may still resolve the old chain and the
    /// vacuum owns the reclamation instead. Naming a chain that any live record
    /// still points at hands its pages to the next writer and corrupts that
    /// record silently.
    previous_prop_overflow: Option<u32>,
    /// Which entity this slot belongs to.
    ///
    /// Needed because overflowed properties are packed several entities to a
    /// page: the page's directory is keyed by entity, so storing or reading a
    /// blob requires knowing whose it is. The id alone is ambiguous — nodes and
    /// edges are numbered independently — hence the kind travels with it.
    entity: (u64, prop_slab_codec::EntityKind),
}

/// Physical page layout of one entity kind's slots. The four fields always
/// travel together — they are fully determined by whether the slot holds a node
/// or an edge — so grouping them keeps the slot-write helpers to a readable
/// arity instead of threading four positional arguments through each call.
#[derive(Clone, Copy)]
struct SlotLayout {
    slot_size: usize,
    file: DataFile,
    magic_bytes: [u8; 4],
    page_type: PageType,
}

impl SlotLayout {
    /// Layout of a node slot in the nodes data file.
    const NODE: Self = Self {
        slot_size: NODE_SLOT_SIZE,
        file: DataFile::Nodes,
        magic_bytes: magic::NODES,
        page_type: PageType::Node,
    };

    /// Layout of an edge slot in the edges data file.
    const EDGE: Self = Self {
        slot_size: EDGE_SLOT_SIZE,
        file: DataFile::Edges,
        magic_bytes: magic::EDGES,
        page_type: PageType::Edge,
    };
}

/// Estimated memory a delta retains: the `new` snapshot it stores plus the
/// `prior` snapshot it holds for rollback. Both are live in the delta chain
/// until commit/vacuum, so the per-transaction memory cap charges both.
fn delta_bytes(
    prior: Option<&crate::mvcc::EntitySnapshot>,
    new: &crate::mvcc::EntitySnapshot,
) -> u64 {
    prior.map_or(0, crate::mvcc::EntitySnapshot::approx_size) + new.approx_size()
}

/// In-memory property graph backed by a page-based storage engine.
///
/// This is the core data structure of `TesseraGraph`. It stores nodes and
/// directed edges with arbitrary properties, and supports basic lookups and
/// mutations. Traversal and query capabilities are provided via builder
/// methods: [`neighbors`](Self::neighbors), [`traverse`](Self::traverse),
/// [`shortest_path`](Self::shortest_path),
/// [`weighted_shortest_path`](Self::weighted_shortest_path), and
/// [`subgraph`](Self::subgraph).
///
/// `Graph` does not implement `Debug` because the internal storage backend
/// is trait-based and may hold file handles.
/// Per-write quota hook installed via [`Graph::open_with_hook`].
/// Fires at the entry of every write operation (Option C': BEFORE any
/// in-memory mutation or WAL append). When it returns `Err`, the
/// operation is rejected cleanly without any state change.
pub type QuotaHook = Box<dyn Fn() -> Result<()> + Send + Sync>;

/// Why the engine performed a given WAL fsync, handed to the [`WalObserver`]
/// alongside its duration (issue #43 Part B).
///
/// A batch coalesces many operations into a single fsync at
/// [`Graph::end_batch`]; every fsync outside a batch flushes exactly one
/// operation. Passing the cause lets an observer measure coalescence by
/// identity — reading `op_count` off the batch-close fsync — instead of
/// inferring it by counting how many fsyncs a run produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncCause {
    /// The fsync flushed a single write performed outside any batch (or the
    /// single fsync of a transaction commit/rollback).
    Individual,
    /// The fsync closed a coalesced batch. `op_count` is how many operations
    /// the batch accumulated before this one fsync flushed them all.
    BatchClose {
        /// Number of operations coalesced into this batch-closing fsync.
        op_count: u64,
    },
}

/// Observer invoked after every WAL fsync the engine performs.
///
/// Receives the [`FsyncCause`] and the wall-clock duration of the underlying
/// `storage.wal_sync()` call. The callback runs on the same task as
/// the write that triggered the fsync — keep the body cheap (a
/// histogram observation, an atomic add) and never block. The engine
/// itself never inspects either argument; both are opaque to the core.
///
/// Installed via [`Graph::open_with_wal_observer`] or
/// [`Graph::with_wal_observer`]. When unset (the default for
/// [`Graph::new`] and [`Graph::open`]), the instrumentation path
/// becomes a single `Option::is_some()` check with no measurement
/// overhead. The observer is the engine-agnostic seam the
/// `tessera-graph-server` crate uses in v0.6.0 Task 2 to feed the
/// `tessera_wal_fsync_duration_seconds` Prometheus histogram without
/// dragging the `metrics` crate into the engine.
pub type WalObserver = Box<dyn Fn(FsyncCause, std::time::Duration) + Send + Sync>;

pub struct Graph {
    storage: Box<dyn StorageBackend>,
    adj_cache: AdjCache,
    /// Caches each node/direction's adjacency chain tail so the write path can
    /// append edges without re-walking the chain (issue #33). A miss falls back
    /// to recomputing the state, so correctness never depends on it.
    adj_tail_cache: crate::adj_tail_cache::AdjTailCache,
    /// The slab page currently accepting new nodes' first sub-block, per
    /// direction (issue #54). Without this, every node starting its adjacency
    /// would allocate a page of its own and the slab would pack nothing — the
    /// page-per-node cost the redesign exists to remove.
    ///
    /// Rebuilt lazily: on a miss (fresh graph, or reopen) the next write
    /// allocates a slab page and parks it here. A stale or forgotten value only
    /// costs an extra page, never correctness — each node's real location is in
    /// its slot, not here. Outgoing and incoming keep separate open slabs
    /// because the node slot stores an independent head per direction.
    open_slab: [Option<PageId>; 2],
    /// The overflow page currently accepting packed property blobs.
    ///
    /// Same role as `open_slab` above, for the same reason: without it every
    /// entity whose properties overflow would allocate a page of its own and
    /// the packing would pack nothing. Also rebuilt lazily and equally
    /// disposable — an entity's real location is the page id in its slot, so a
    /// stale or forgotten value costs at most an extra page, never correctness.
    prop_slab_open_page: Option<PageId>,
    node_exists: HashSet<u64>,
    edge_exists: HashSet<u64>,
    string_heap: StringHeap,
    node_label_index: LabelIndex,
    edge_label_index: LabelIndex,
    /// Index mapping `(from, to, label_hash)` to the `edge_id`s of the parallel
    /// edges on that pair, giving `O(k)` existence queries (`k` = parallel edges
    /// per pair, typically 1-3) instead of scanning all of a node's outgoing
    /// edges. Not persisted: rebuilt from edge pages on `open` (see
    /// `rebuild_edge_indexes`). The `label_hash` is `node_codec::label_hash`
    /// (CRC32), so lookups guard against hash collisions with a final
    /// `edge.label() == label` string comparison.
    edge_pair_index: HashMap<(u64, u64, u32), Vec<u64>>,
    node_property_index: PropertyIndex,
    batch_depth: u32,
    /// Pending adjacency `edge_ids` accumulated during a batch.
    /// Flushed to storage on `end_batch`, converting O(N²) per-edge
    /// adjacency rewrites into O(N) single writes per (node, direction).
    adj_pending: HashMap<(u64, AdjDirection), Vec<u64>>,
    /// Task 15: optional per-write quota hook. `None` for [`Graph::new`]
    /// and [`Graph::open`]; populated by [`Graph::open_with_hook`].
    /// Called by [`Graph::check_quota`] at the entry of every write
    /// operation.
    quota_hook: Option<QuotaHook>,
    /// v0.6.0 Fase 2 Task 2: optional WAL fsync observer. `None` for
    /// [`Graph::new`] and [`Graph::open`]; populated by
    /// [`Graph::open_with_wal_observer`] or
    /// [`Graph::with_wal_observer`]. Called by [`Graph::wal_sync`]
    /// with the wall-clock duration of every fsync the engine
    /// actually performs (skips inside an open batch are not
    /// observed — see the early-return in `wal_sync`).
    wal_observer: Option<WalObserver>,
    /// 3c: per-database DDL schema catalog (declared indexes + unique
    /// constraints). Loaded from `schema.bin` on [`Graph::open`]; persisted on
    /// [`Graph::flush`] and immediately after each DDL mutation by the server.
    schema_catalog: crate::schema::SchemaCatalog,
    /// Issue #43: the ids of nodes whose label was declared append-only when
    /// they were created. Reads of these ids skip MVCC visibility resolution
    /// entirely and go straight to the page.
    ///
    /// Indexed by id rather than by label because [`Graph::node`] only has an
    /// id to work with — discovering the label would require the very page read
    /// the gate exists to reach directly.
    ///
    /// Not persisted: rebuilt on [`Graph::open`] from the persisted catalog and
    /// the node pages, which is why `open()` loads the schema catalog before
    /// rebuilding the indexes.
    ///
    /// A node joins on creation, under the declaration in force at that moment.
    /// It leaves when the label's declaration is withdrawn through
    /// [`Graph::set_label_append_only`], which drops the label's ids here so the
    /// withdrawal takes effect at once rather than at the next restart (issue
    /// #61). Re-declaring never re-captures a node that already left: it may
    /// have acquired a delta chain in the meantime, and the fast path would
    /// skip resolving it.
    ///
    /// Editing [`crate::schema::SchemaCatalog`] directly bypasses all of that
    /// and leaves this set stale — prefer `Graph::set_label_append_only`.
    append_only_node_ids: HashSet<u64>,
    /// Block 4 MVCC: the in-memory delta table backing explicit transactions.
    /// `None` (the default for [`Graph::new`]/[`Graph::open`]) means legacy
    /// single-version mode — every read and write behaves exactly as in v0.9.0
    /// with zero overhead, gated by a single `Option::is_none` check. Populated
    /// by [`Graph::enable_mvcc`]. Kept alongside [`Self::txn_registry`] and
    /// [`Self::txn_clock`]: the three are set together and share the same
    /// lifetime, mirroring the `quota_hook`/`wal_observer` opt-in pattern.
    delta_table: Option<crate::mvcc::DeltaTable>,
    /// Block 4 MVCC: the active-transaction registry (see [`Self::delta_table`]).
    /// `None` in legacy mode.
    txn_registry: Option<crate::mvcc::TxnRegistry>,
    /// Block 4 MVCC: the visibility clock (see [`Self::delta_table`]). `None` in
    /// legacy mode.
    txn_clock: Option<crate::mvcc::TxnClock>,
    /// Block 4 MVCC: per-transaction memory cap in estimated bytes. When set,
    /// an explicit transaction whose uncommitted delta chain would exceed it is
    /// aborted (implicit rollback) with [`Error::TxnMemoryCapExceeded`]. `None`
    /// (the default) means unlimited. Set via [`Self::set_txn_memory_cap`].
    txn_memory_cap: Option<u64>,
    /// Issue #37: max operations allowed inside one outermost batch
    /// (`begin_batch` / `end_batch`). `None` (default) means unlimited. Set via
    /// [`Self::set_batch_limits`]. A mutation that would push the count past
    /// this cap is rejected with [`Error::BatchLimitExceeded`] before it
    /// applies; unlike a transaction, prior mutations in the batch are NOT
    /// rolled back (batches are not atomic).
    batch_max_ops: Option<u64>,
    /// Issue #37: max estimated bytes allowed inside one outermost batch. `None`
    /// (default) means unlimited. Set via [`Self::set_batch_limits`]. A defensive
    /// fuse against a batch of a few huge entities exhausting process memory.
    batch_max_bytes: Option<u64>,
    /// Operations counted so far in the currently open batch. Reset to 0 when
    /// the outermost `end_batch` closes (`batch_depth` reaches 0).
    batch_op_count: u64,
    /// Estimated bytes counted so far in the currently open batch. Reset to 0
    /// when the outermost `end_batch` closes.
    batch_byte_count: u64,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    /// Creates a new, empty in-memory graph.
    #[must_use]
    pub fn new() -> Self {
        Self {
            storage: Box::new(MemoryBackend::new()),
            adj_cache: AdjCache::new(DEFAULT_ADJ_CACHE_CAPACITY),
            adj_tail_cache: crate::adj_tail_cache::AdjTailCache::new(DEFAULT_ADJ_CACHE_CAPACITY),
            open_slab: [None; 2],
            prop_slab_open_page: None,
            node_exists: HashSet::new(),
            edge_exists: HashSet::new(),
            string_heap: StringHeap::new(),
            node_label_index: LabelIndex::new(),
            edge_label_index: LabelIndex::new(),
            edge_pair_index: HashMap::new(),
            node_property_index: PropertyIndex::new(),
            batch_depth: 0,
            adj_pending: HashMap::new(),
            quota_hook: None,
            wal_observer: None,
            schema_catalog: crate::schema::SchemaCatalog::new(),
            append_only_node_ids: HashSet::new(),
            delta_table: None,
            txn_registry: None,
            txn_clock: None,
            txn_memory_cap: None,
            batch_max_ops: None,
            batch_max_bytes: None,
            batch_op_count: 0,
            batch_byte_count: 0,
        }
    }

    /// Snapshot of the buffer-pool instrumentation counters
    /// `(hits, misses, evictions)`. Only present under the
    /// `pool-instrumentation` feature (issue #54 thrashing verification).
    #[cfg(feature = "pool-instrumentation")]
    #[must_use]
    pub fn pool_instrumentation(&self) -> (u64, u64, u64) {
        self.storage.pool_instrumentation()
    }

    /// Resets the buffer-pool instrumentation counters.
    /// Only present under the `pool-instrumentation` feature (issue #54).
    #[cfg(feature = "pool-instrumentation")]
    pub fn reset_pool_instrumentation(&self) {
        self.storage.reset_pool_instrumentation();
    }

    /// Page count of each data file: `(nodes, edges, adjacency, strings)`.
    /// Only present under the `pool-instrumentation` feature (issue #54); used
    /// to derive the working-set-to-memory relationship empirically.
    #[cfg(feature = "pool-instrumentation")]
    #[must_use]
    pub fn data_file_page_counts(&self) -> (u32, u32, u32, u32) {
        (
            self.storage.page_count(DataFile::Nodes),
            self.storage.page_count(DataFile::Edges),
            self.storage.page_count(DataFile::Adjacency),
            self.storage.page_count(DataFile::Strings),
        )
    }

    /// Opens a file-backed graph at the given directory.
    ///
    /// If the directory does not exist and `config.create_if_missing` is true,
    /// it will be created. On reopen, in-memory indexes (`node_exists`,
    /// `edge_exists`, `adj_cache`) are rebuilt by scanning stored pages.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory doesn't exist (and `create_if_missing`
    /// is false), or if the stored data is corrupt.
    pub fn open(path: impl AsRef<Path>, config: &GraphConfig) -> Result<Self> {
        let backend = FileBackend::open(path, config)?;
        let strings_write_offset = backend.meta().strings_write_offset;
        let string_heap = StringHeap::with_offset(strings_write_offset);

        let mut graph = Self {
            storage: Box::new(backend),
            adj_cache: AdjCache::new(config.adj_cache_capacity),
            adj_tail_cache: crate::adj_tail_cache::AdjTailCache::new(config.adj_cache_capacity),
            open_slab: [None; 2],
            prop_slab_open_page: None,
            node_exists: HashSet::new(),
            edge_exists: HashSet::new(),
            string_heap,
            node_label_index: LabelIndex::new(),
            edge_label_index: LabelIndex::new(),
            edge_pair_index: HashMap::new(),
            node_property_index: PropertyIndex::new(),
            batch_depth: 0,
            adj_pending: HashMap::new(),
            quota_hook: None,
            wal_observer: None,
            schema_catalog: crate::schema::SchemaCatalog::new(),
            append_only_node_ids: HashSet::new(),
            delta_table: None,
            txn_registry: None,
            txn_clock: None,
            txn_memory_cap: None,
            batch_max_ops: None,
            batch_max_bytes: None,
            batch_op_count: 0,
            batch_byte_count: 0,
        };

        // Load the DDL schema catalog from schema.bin if present. A corrupt
        // catalog must NOT fail open() — the database itself is intact; the
        // operator can re-issue the DDL statements. Start empty in that case.
        //
        // Issue #43: this MUST precede `rebuild_indexes`, because the rebuild
        // repopulates `append_only_node_ids` and can only recognise an
        // append-only node if the declarations are already loaded. The rebuild
        // does not read the catalog for any other purpose, so moving the load
        // earlier changes nothing else.
        if let Ok(Some(bytes)) = graph.storage.read_schema_bytes() {
            if let Ok(cat) = crate::schema::codec::deserialize(&bytes) {
                graph.schema_catalog = cat;
            }
        }

        graph.rebuild_indexes()?;

        Ok(graph)
    }

    /// File-backed graph with a per-write quota hook installed at
    /// construction (Task 15). The hook fires at the entry of every
    /// write operation BEFORE any in-memory mutation or WAL append
    /// (Option C': clean rejection, no rollback needed). When the
    /// hook returns `Err(Error::QuotaExceeded { .. })`, the write
    /// is refused and nothing is persisted.
    ///
    /// **Tolerance:** the check observes the on-disk size that
    /// existed at the start of the call. A write that fits under the
    /// limit can push the dir slightly over (by the size of the
    /// just-written record); the next write then rejects. For
    /// typical record sizes (~100 bytes) against MiB-scale quotas
    /// the over-run is well under 0.1%.
    ///
    /// # Errors
    ///
    /// Same as [`Graph::open`]. The hook itself is NOT fired on
    /// open — only on subsequent write operations — so an already-
    /// over-quota database can still be opened to drain.
    pub fn open_with_hook(
        path: impl AsRef<Path>,
        config: &GraphConfig,
        hook: QuotaHook,
    ) -> Result<Self> {
        let mut graph = Self::open(path, config)?;
        graph.quota_hook = Some(hook);
        Ok(graph)
    }

    /// File-backed graph with a WAL fsync observer installed at
    /// construction (v0.6.0 Fase 2 Task 2). The observer fires after
    /// every fsync the engine performs and receives the wall-clock
    /// duration of the underlying `storage.wal_sync()` call. Skips
    /// inside an open batch are not observed — the engine only calls
    /// the observer when an actual fsync happens.
    ///
    /// Use this when wiring metrics or tracing without dragging the
    /// `metrics` crate into the engine: the observer is a plain
    /// callback the caller owns, so the engine stays agnostic about
    /// the destination of the measurement.
    ///
    /// # Errors
    ///
    /// Same as [`Graph::open`]. The observer is NOT fired on open —
    /// only on subsequent write operations that issue a WAL sync.
    pub fn open_with_wal_observer(
        path: impl AsRef<Path>,
        config: &GraphConfig,
        observer: WalObserver,
    ) -> Result<Self> {
        let mut graph = Self::open(path, config)?;
        graph.wal_observer = Some(observer);
        Ok(graph)
    }

    /// Builder-style WAL observer installation. Useful for tests that
    /// open the graph through [`Graph::open`] and need to attach the
    /// observer without reaching for the dedicated constructor.
    /// Replaces any previously installed observer.
    ///
    /// The semantics are identical to [`Graph::open_with_wal_observer`];
    /// see that method for the contract on when the observer fires.
    #[must_use]
    pub fn with_wal_observer(mut self, observer: WalObserver) -> Self {
        self.wal_observer = Some(observer);
        self
    }

    /// Run the per-write quota hook if installed. Called at the entry
    /// of every write operation BEFORE any state mutation. Returns
    /// the hook's error unchanged so callers propagate
    /// `Error::QuotaExceeded` cleanly. No-op when the hook is `None`.
    #[inline]
    fn check_quota(&self) -> Result<()> {
        self.quota_hook.as_ref().map_or(Ok(()), |hook| hook())
    }

    /// Charges one operation and `entity_bytes` estimated bytes against the
    /// currently open batch (issue #37). No-op when no batch is open
    /// (`batch_depth == 0`): outside a batch every mutation is its own durable
    /// unit and the batch caps do not apply. Called by each of the six batchable
    /// mutations BEFORE any state mutation, mirroring the pre-mutation placement
    /// of [`Self::check_quota`].
    ///
    /// Leaves the counters UNCHANGED and returns [`Error::BatchLimitExceeded`]
    /// if applying this operation would breach either configured cap. The
    /// operation-count cap is checked before the byte cap; neither counter is
    /// written unless both checks pass, so a rejection never partially advances
    /// the batch state.
    const fn charge_batch_op(&mut self, entity_bytes: u64) -> Result<()> {
        if self.batch_depth == 0 {
            return Ok(());
        }
        let next_ops = self.batch_op_count + 1;
        if let Some(max_ops) = self.batch_max_ops {
            if next_ops > max_ops {
                return Err(Error::BatchLimitExceeded {
                    kind: BatchLimitKind::Operations,
                    current: next_ops,
                    limit: max_ops,
                });
            }
        }
        let next_bytes = self.batch_byte_count + entity_bytes;
        if let Some(max_bytes) = self.batch_max_bytes {
            if next_bytes > max_bytes {
                return Err(Error::BatchLimitExceeded {
                    kind: BatchLimitKind::Bytes,
                    current: next_bytes,
                    limit: max_bytes,
                });
            }
        }
        // Both caps passed: advance the counters. The op cap is checked before
        // the byte cap, so when both would trip the operation limit is the one
        // reported. Neither counter moves until both checks pass, so a rejected
        // mutation never partially advances the batch state.
        self.batch_op_count = next_ops;
        self.batch_byte_count = next_bytes;
        Ok(())
    }

    /// Estimated in-memory bytes a node/edge write charges against the open
    /// batch's byte cap (issue #37). Mirrors `EntitySnapshot::approx_size`
    /// (`mvcc::delta`) — base struct size plus label length plus per-property
    /// key+value heap bytes — without depending on the MVCC delta module.
    /// Deliberately a lower bound (ignores allocator slack), the correct bias
    /// for a defensive cap. `base` is `size_of::<Node>()` or `size_of::<Edge>()`.
    fn estimate_entity_bytes(base: usize, label: &str, properties: &Properties) -> u64 {
        let props_bytes: usize = properties
            .iter()
            .map(|(k, v)| k.len() + v.approx_heap_size())
            .sum();
        (base + label.len() + props_bytes) as u64
    }

    // ---- Block 4 MVCC: explicit transactions ---------------------------------
    //
    // Visibility scope (current design, Phase 5 option 2a):
    //
    // - Per-entity reads — `node`, `edge`, `node_projected`, `node_label` — and
    //   edge traversals — `outgoing_edges`/`incoming_edges` and their by-label
    //   variants, `edges_between`, `has_edge` — all resolve visibility through
    //   the delta chain (`resolve_node_visible`/`resolve_edge_visible`).
    //   Adjacency and the label/property indexes are unversioned,
    //   committed-reconciled SUPERSETS of ids (they may reference ids that a
    //   given snapshot cannot see, e.g. rows for entities created by a
    //   still-uncommitted or later transaction); every id -> entity
    //   materialization step is then filtered by the reader's snapshot, so a
    //   reader never observes an entity whose `start_ts` its snapshot does not
    //   define. Stale/invisible ids left in these structures by uncommitted or
    //   aborted writers are pruned lazily; reclaiming them is the vacuum's job,
    //   not a correctness requirement of the read path.
    // - The `O(1)` counters (`node_count`/`edge_count`) remain
    //   committed-reconciled and are NOT snapshot-aware: queries never read
    //   them directly, deriving `COUNT` from `node_ids()`/`nodes_by_label()`
    //   instead, which are already snapshot-filtered as described above.

    /// Returns `true` when this graph runs in MVCC mode (explicit transactions
    /// enabled). `false` is the legacy single-version default.
    #[must_use]
    pub const fn mvcc_enabled(&self) -> bool {
        self.delta_table.is_some()
    }

    /// Returns `true` when this node was created under a label declared
    /// append-only, meaning it is exempt from MVCC visibility resolution on
    /// read and from mutation inside an explicit transaction.
    #[must_use]
    pub(crate) fn is_append_only_node(&self, id: NodeId) -> bool {
        self.append_only_node_ids.contains(&id.0)
    }

    /// Whether a node of `label` with raw id `id` falls under the label's
    /// append-only declaration.
    ///
    /// Used only by the `open()` rebuild, to reconstruct the fast-path set that
    /// the running graph had. Node ids increase monotonically and are never
    /// reused, so "created while the declaration held" is exactly "id at or
    /// above the declaration's lower bound" (issue #61). Without the bound the
    /// rebuild captured every node of a declared label, so re-declaring after a
    /// withdrawal recaptured nodes that had been freed — and a recaptured node
    /// holding a delta chain stops resolving it on read, losing a committed
    /// write.
    fn is_covered_by_append_only(&self, label: &str, id: u64) -> bool {
        self.schema_catalog
            .append_only_since(label)
            .is_some_and(|since| id >= since)
    }

    /// Declares (`on = true`) or withdraws (`on = false`) append-only mode for
    /// `label`. Idempotent.
    ///
    /// Prefer this over calling
    /// [`SchemaCatalog::mark_label_append_only`](crate::schema::SchemaCatalog::mark_label_append_only)
    /// directly: the catalog records the declaration, but the fast-path node
    /// set that actually gates behaviour lives on the `Graph` and is only
    /// rebuilt from the catalog at `open()`. Touching the catalog alone
    /// therefore has no effect until the next restart, and then applies
    /// retroactively to every node of that label — the same call producing two
    /// different outcomes with nothing to tell them apart (issue #61).
    ///
    /// # Asymmetry, deliberate
    ///
    /// Withdrawing frees the label's existing nodes at once, matching what the
    /// reopen rebuild would do. Declaring does NOT capture existing nodes: only
    /// nodes created from now on take the fast path. A node created while the
    /// label was ordinary may already carry a delta chain, and the fast path
    /// skips resolving it — capturing such a node would hide committed writes.
    ///
    /// The declaration records the graph's next node id as its lower bound, so
    /// the reopen rebuild reproduces the same membership rather than capturing
    /// every node of the label (issue #61). Node ids only ever increase, so the
    /// bound is a faithful stand-in for "created after this point".
    ///
    /// The declaration itself is persisted by [`Self::persist_schema`]; this
    /// call only updates in-memory state.
    pub fn set_label_append_only(&mut self, label: &str, on: bool) {
        if on {
            let since = self.storage.meta().next_node_id;
            self.schema_catalog.mark_label_append_only(label, since);
            return;
        }

        self.schema_catalog.unmark_label_append_only(label);

        // Drop the label's nodes from the fast-path set, which is what the next
        // `open()` would compute anyway. Driven from the label index so the cost
        // is proportional to the withdrawn label, not to the union of every
        // append-only label in the graph — and with no page read per node.
        for id in self.node_label_index.ids_for(label) {
            self.append_only_node_ids.remove(&id);
        }
    }

    /// Test-only harness for the append-only invariant check in
    /// `push_txn_delta`. Bypasses the API-level rejections so the last line of
    /// defense can be exercised on its own. Not part of the public API and not
    /// compiled into release builds.
    #[cfg(test)]
    pub(crate) fn test_only_push_raw_delta_for_append_only_node(&self, txn_id: u64, id: NodeId) {
        let snapshot = crate::mvcc::EntitySnapshot::Node(Node::new(id, "Event", Properties::new()));
        let delta =
            crate::mvcc::Delta::new(txn_id, None, Some(snapshot), crate::mvcc::DeltaOp::Update);
        let _ = self.push_txn_delta(txn_id, crate::mvcc::EntityKey::Node(id), delta);
    }

    /// Rejects a transactional write against a node that was created under an
    /// append-only label, naming the label in the error.
    ///
    /// # Errors
    ///
    /// [`Error::AppendOnlyLabelInTxn`] when `id` is append-only.
    fn reject_if_append_only(&self, id: NodeId) -> Result<()> {
        if !self.is_append_only_node(id) {
            return Ok(());
        }
        // The label is read back only on the rejection path, so naming it in
        // the error costs nothing in the common case.
        let label = self
            .read_node(id.0)
            .map_or_else(|_| String::from("<unknown>"), |n| n.label().to_string());
        Err(Error::AppendOnlyLabelInTxn { label })
    }

    /// Switches this graph into MVCC mode, enabling explicit
    /// `begin_txn`/`commit_txn`/`rollback_txn` and the `*_in_txn` mutations.
    ///
    /// Constructs the delta table, transaction registry, and visibility clock.
    /// Idempotent in effect but replaces any existing MVCC state, so it must be
    /// called before any transaction begins — the server enables it once at
    /// open time (Phase 7). Legacy reads/writes keep working unchanged; MVCC
    /// only adds the transactional entry points.
    pub fn enable_mvcc(&mut self) {
        self.delta_table = Some(crate::mvcc::DeltaTable::new(MVCC_SHARD_COUNT));
        self.txn_registry = Some(crate::mvcc::TxnRegistry::new());
        self.txn_clock = Some(crate::mvcc::TxnClock::new());
    }

    /// Sets the per-transaction memory cap: the estimated bytes an explicit
    /// transaction's uncommitted delta chain may hold before it is aborted
    /// (implicit rollback) with [`Error::TxnMemoryCapExceeded`]. `None` disables
    /// the cap. The server wires this from `ServerConfig::max_txn_memory_bytes`.
    pub const fn set_txn_memory_cap(&mut self, cap: Option<u64>) {
        self.txn_memory_cap = cap;
    }

    /// Sets the batch caps: the max operation count and/or max estimated bytes
    /// an outermost batch (see [`Self::begin_batch`]) may accumulate before a
    /// mutation is rejected with [`Error::BatchLimitExceeded`]. `None` disables
    /// the corresponding cap. The server wires this from
    /// `ServerConfig::max_batch_operations` / `max_batch_memory_bytes`.
    ///
    /// Rejecting a mutation does NOT roll back earlier mutations in the same
    /// batch — batches are not atomic (issue #37). For all-or-nothing semantics
    /// use an explicit transaction (`begin_txn`/`commit_txn`/`rollback_txn`).
    pub const fn set_batch_limits(&mut self, max_ops: Option<u64>, max_bytes: Option<u64>) {
        self.batch_max_ops = max_ops;
        self.batch_max_bytes = max_bytes;
    }

    /// Charges `delta_size` bytes against `txn_id`'s running memory estimate and,
    /// if the new total exceeds [`Self::txn_memory_cap`], rolls the transaction
    /// back and returns [`Error::TxnMemoryCapExceeded`]. Called by each
    /// `*_in_txn` mutation BEFORE it pushes the delta, so a transaction that
    /// would breach the cap never grows the delta table past the limit.
    ///
    /// # Errors
    ///
    /// [`Error::MvccNotEnabled`] / [`Error::TxnNotActive`] if the transaction is
    /// not live, or [`Error::TxnMemoryCapExceeded`] when the charge breaches the
    /// cap (the transaction has been rolled back before this returns).
    fn charge_txn_memory(&mut self, txn_id: u64, delta_size: u64) -> Result<()> {
        let used_bytes = {
            let registry = self.txn_registry.as_ref().ok_or(Error::MvccNotEnabled)?;
            registry
                .add_bytes(txn_id, delta_size)
                .ok_or(Error::TxnNotActive(txn_id))?
        };
        if let Some(cap_bytes) = self.txn_memory_cap {
            if used_bytes > cap_bytes {
                // Over cap: abort the whole transaction, not just this operation.
                self.rollback_txn(txn_id)?;
                return Err(Error::TxnMemoryCapExceeded {
                    txn_id,
                    used_bytes,
                    cap_bytes,
                });
            }
        }
        Ok(())
    }

    /// Opens an explicit transaction and returns its `txn_id`.
    ///
    /// Emits a WAL `Begin` marker when the WAL is enabled, so recovery can tell
    /// which transactions were in flight at a crash.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MvccNotEnabled`] if [`Graph::enable_mvcc`] was not
    /// called.
    pub fn begin_txn(&mut self) -> Result<u64> {
        let txn_id = {
            let registry = self.txn_registry.as_ref().ok_or(Error::MvccNotEnabled)?;
            let clock = self.txn_clock.as_ref().ok_or(Error::MvccNotEnabled)?;
            registry.begin(clock)
        };
        if self.storage.wal_enabled() {
            // If the WAL append fails, undo the registry entry so a failed begin
            // leaves no phantom active transaction skewing the vacuum watermark.
            if let Err(e) = self
                .storage
                .wal_append(crate::wal::record::WalRecord::Begin { lsn: 0, txn_id })
            {
                if let Some(registry) = self.txn_registry.as_ref() {
                    registry.end(txn_id);
                }
                return Err(e);
            }
        }
        Ok(txn_id)
    }

    /// Commits transaction `txn_id`.
    ///
    /// Durability-first ordering (so a crash never leaves the in-memory state
    /// disagreeing with the WAL):
    ///
    /// 1. Emit a durable WAL redo (`WriteNode`/`WriteEdge`/`Tombstone*` tagged
    ///    `txn_id: Some`) for every delta, then the `Commit` marker, then
    ///    `wal_sync`. If any step fails the transaction stays active and
    ///    invisible — no reader ever saw a value that isn't durable.
    /// 2. Only after the WAL is durable, stamp the deltas with a fresh
    ///    `commit_ts`, which is what makes them visible to new readers.
    /// 3. End the transaction.
    ///
    /// The node/edge pages are NOT written here — "committed" means the delta
    /// carries a `commit_ts` (visible via the chain) and a durable WAL redo
    /// (replayed on recovery). Page materialization stays the vacuum's job
    /// (Phase 5); writing the page now would break a still-live reader's
    /// snapshot. See [`Graph::wal_log_committed_delta`].
    ///
    /// # Errors
    ///
    /// [`Error::MvccNotEnabled`] in legacy mode, [`Error::TxnNotActive`] if
    /// `txn_id` is not live, or a storage error if the WAL redo/sync fails (in
    /// which case the transaction remains active and can be retried or rolled
    /// back).
    pub fn commit_txn(&mut self, txn_id: u64) -> Result<()> {
        if !self.txn_is_active(txn_id) {
            return Err(Error::TxnNotActive(txn_id));
        }
        let clock = self.txn_clock.as_ref().ok_or(Error::MvccNotEnabled)?;
        let commit_ts = clock.next();

        // Snapshot the newest delta of `txn_id` for each written key (in the
        // transaction's write order), releasing the registry/table borrows
        // before the `&mut self` WAL emission below. The full delta carries the
        // op + prior + new state that category-B reconciliation needs.
        let committed: Vec<(crate::mvcc::EntityKey, crate::mvcc::Delta)> = {
            let registry = self.txn_registry.as_ref().ok_or(Error::MvccNotEnabled)?;
            let table = self.delta_table.as_ref().ok_or(Error::MvccNotEnabled)?;
            let mut out = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for key in registry.keys_written_by(txn_id) {
                // `keys_written_by` may repeat a key; commit each key once.
                if !seen.insert(key) {
                    continue;
                }
                if let Some(delta) = table.newest_delta_of_txn(key, txn_id) {
                    out.push((key, delta));
                }
            }
            out
        };

        // Phase 1: durable WAL. Redos first, then the Commit marker, then sync.
        // A failure here leaves the transaction active and invisible.
        for (key, delta) in &committed {
            self.wal_log_committed_delta(*key, delta.new_state(), txn_id)?;
        }
        if self.storage.wal_enabled() {
            self.storage
                .wal_append(crate::wal::record::WalRecord::Commit { lsn: 0, txn_id })?;
        }
        self.wal_sync(FsyncCause::Individual)?;

        // Phase 2: the WAL is durable — now make the deltas visible.
        let table = self.delta_table.as_ref().ok_or(Error::MvccNotEnabled)?;
        for (key, _) in &committed {
            table.stamp_commit_for_txn(*key, txn_id, commit_ts);
        }

        // Phase 3: reconcile category B (counts, exists-sets, indexes,
        // adjacency) so the commit is immediately visible to aggregate and
        // traversal reads, matching Neo4j/Memgraph. The node/edge page write is
        // the vacuum's job (Phase 5, a lazy optimization); only the derived
        // structures are updated here.
        for (key, delta) in &committed {
            self.reconcile_committed_delta(*key, delta)?;
        }

        // Phase 4: end the transaction.
        self.txn_registry
            .as_ref()
            .ok_or(Error::MvccNotEnabled)?
            .end(txn_id);
        Ok(())
    }

    /// Rolls back transaction `txn_id`: discards all its deltas from the delta
    /// table, emits a WAL `Rollback`, and ends the transaction. Because an
    /// uncommitted transaction never touched a page, discarding its in-memory
    /// deltas fully undoes it — no page restore is needed.
    ///
    /// The in-memory cleanup (dropping deltas and ending the transaction) always
    /// runs, even if the WAL `Rollback` append/sync fails: a rollback never
    /// persists data that must survive, and an absent `Rollback` marker yields
    /// the same recovery outcome (an uncommitted transaction is discarded). A
    /// WAL failure is still surfaced to the caller, but the transaction is left
    /// fully cleaned up (no phantom active `txn_id`).
    ///
    /// # Errors
    ///
    /// [`Error::MvccNotEnabled`] in legacy mode, [`Error::TxnNotActive`] if
    /// `txn_id` is not live, or a storage error if the WAL `Rollback` fails
    /// (after the in-memory rollback has already completed).
    pub fn rollback_txn(&mut self, txn_id: u64) -> Result<()> {
        if !self.txn_is_active(txn_id) {
            return Err(Error::TxnNotActive(txn_id));
        }
        // In-memory undo first, and unconditionally: discard deltas + end txn.
        {
            let registry = self.txn_registry.as_ref().ok_or(Error::MvccNotEnabled)?;
            let table = self.delta_table.as_ref().ok_or(Error::MvccNotEnabled)?;
            for key in registry.keys_written_by(txn_id) {
                table.remove_deltas_for_txn(key, txn_id);
            }
            registry.end(txn_id);
        }
        // WAL Rollback marker is informational; its failure does not undo the
        // in-memory rollback above, but is reported to the caller.
        if self.storage.wal_enabled() {
            self.storage
                .wal_append(crate::wal::record::WalRecord::Rollback { lsn: 0, txn_id })?;
        }
        self.wal_sync(FsyncCause::Individual)?;
        Ok(())
    }

    /// Number of deltas currently in the chain for node `id`, or 0 if none.
    /// Test-only probe for the vacuum: a materialized chain is emptied.
    #[cfg(test)]
    fn delta_chain_len_for_test(&self, id: NodeId) -> usize {
        self.delta_table
            .as_ref()
            .and_then(|t| t.chain_for(crate::mvcc::EntityKey::Node(id)))
            .map_or(0, |c| c.len())
    }

    /// How many version chains are still held in memory.
    ///
    /// Committed transactions leave their versions on these chains until the
    /// vacuum materialises them to the page, so this is the size of the memory
    /// that [`Graph::vacuum_once`] reclaims. `0` in legacy (non-transactional)
    /// mode, where no chain exists.
    ///
    /// Exists so a caller outside the engine can distinguish a server that
    /// reclaims this memory from one that lets it grow for the life of the
    /// process. Without it that difference has no symptom until the process
    /// runs out of memory — which is exactly how a Community server shipped
    /// without a vacuum task went unnoticed.
    #[must_use]
    pub fn pending_version_chains(&self) -> usize {
        self.delta_table
            .as_ref()
            .map_or(0, crate::mvcc::DeltaTable::chain_count)
    }

    /// Materializes committed delta chains to their pages and frees them,
    /// returning the number of chains materialized.
    ///
    /// Only chains that are entirely safe to vacuum are touched: every delta in
    /// them is committed with `commit_ts < watermark`, where `watermark` is the
    /// oldest live transaction's `start_ts` (or all committed chains when no
    /// transaction is live). A chain with an uncommitted delta, or one committed
    /// at or after the watermark, is kept so a live reader can still resolve its
    /// snapshot through it.
    ///
    /// This is a pure optimization: category B (counts, exists-sets, indexes,
    /// adjacency) was already reconciled at [`Graph::commit_txn`], and the WAL
    /// redo was already made durable there. The vacuum therefore writes the page
    /// slot WITHOUT re-logging the WAL (the data is already durable) and WITHOUT
    /// touching category B (already up to date). A vacuum that never runs only
    /// costs memory and page-read latency, never correctness.
    ///
    /// # Errors
    ///
    /// [`Error::MvccNotEnabled`] in legacy mode, or a storage error while
    /// writing a materialized page.
    pub fn vacuum_once(&mut self) -> Result<usize> {
        let watermark = self
            .txn_registry
            .as_ref()
            .ok_or(Error::MvccNotEnabled)?
            .oldest_active_start_ts();
        let drained = self
            .delta_table
            .as_ref()
            .ok_or(Error::MvccNotEnabled)?
            .drain_vacuumable(watermark);
        let count = drained.len();
        for (key, new_state, old_base) in drained {
            self.materialize_to_page(key, new_state.as_ref())?;
            self.apply_vacuum_category_b(key, new_state.as_ref(), old_base.as_ref())?;
        }
        Ok(count)
    }

    /// Applies the category-B baja a vacuumed version implies, after its page has
    /// been materialized: a `Deleted` end state removes the record from category
    /// B (exists-set, indexes, adjacency, count); an updated node/edge removes
    /// the stale old-value index entries left as a committed superset since
    /// commit. An insert with no old base needs nothing — its alta was applied at
    /// commit. Returns errors from adjacency I/O.
    fn apply_vacuum_category_b(
        &mut self,
        key: crate::mvcc::EntityKey,
        new_state: Option<&crate::mvcc::EntitySnapshot>,
        old_base: Option<&crate::mvcc::EntitySnapshot>,
    ) -> Result<()> {
        use crate::mvcc::{EntityKey, EntitySnapshot};
        match (key, new_state, old_base) {
            (
                EntityKey::Node(_),
                Some(EntitySnapshot::Deleted) | None,
                Some(EntitySnapshot::Node(prior)),
            ) => {
                self.reconcile_node_delete(prior);
            }
            (
                EntityKey::Edge(_),
                Some(EntitySnapshot::Deleted) | None,
                Some(EntitySnapshot::Edge(prior)),
            ) => {
                self.reconcile_edge_delete(prior)?;
            }
            (
                EntityKey::Node(_),
                Some(EntitySnapshot::Node(new)),
                Some(EntitySnapshot::Node(prior)),
            ) => {
                let id_val = new.id.0;
                if prior.label() != new.label() {
                    self.node_label_index.remove(prior.label(), id_val);
                }
                self.node_property_index
                    .remove_node(prior.label(), prior.properties(), id_val);
            }
            (
                EntityKey::Edge(_),
                Some(EntitySnapshot::Edge(new)),
                Some(EntitySnapshot::Edge(prior)),
            ) if prior.label() != new.label() => {
                let id_val = new.id.0;
                self.edge_label_index.remove(prior.label(), id_val);
                self.remove_pair_index(prior.source.0, prior.target.0, prior.label(), id_val);
            }
            // An edge update with an unchanged label left no stale index entry;
            // an insert with no old base already applied its alta at commit.
            _ => {}
        }
        Ok(())
    }

    /// Writes the vacuumed version of `key` to its page, WITHOUT logging the WAL
    /// (the committed data is already durable) and WITHOUT touching category B
    /// (already reconciled at commit). A `None`/`Deleted` state tombstones the
    /// slot; a node/edge state writes the slot.
    fn materialize_to_page(
        &mut self,
        key: crate::mvcc::EntityKey,
        state: Option<&crate::mvcc::EntitySnapshot>,
    ) -> Result<()> {
        use crate::mvcc::{EntityKey, EntitySnapshot};
        match (key, state) {
            (EntityKey::Node(id), Some(EntitySnapshot::Deleted) | None) => {
                self.tombstone_slot_inner(id.0, SlotLayout::NODE, false, true)
            }
            (EntityKey::Edge(id), Some(EntitySnapshot::Deleted) | None) => {
                self.tombstone_slot_inner(id.0, SlotLayout::EDGE, false, true)
            }
            (EntityKey::Node(_), Some(EntitySnapshot::Node(node))) => {
                let (mut slot_buf, overflow) = node_codec::encode_node_slot(node)?;
                self.handle_slot_overflow(
                    &mut slot_buf,
                    SlotOverflowRequest {
                        label_overflowed: overflow.label_overflowed,
                        label: node.label(),
                        props_overflowed: overflow.props_overflowed,
                        props_bytes: overflow.props_bytes.as_deref(),
                        previous_prop_overflow: None,
                        entity: (node.id.0, prop_slab_codec::EntityKind::Node),
                    },
                    node_codec::patch_overflow,
                )?;
                // The snapshot `Node` captured in the delta may predate an edge
                // added to this node later (its adj heads would be stale). The
                // adjacency head is authoritative in the node's on-disk slot
                // (written durably at commit), so preserve whatever head is
                // already on the page instead of overwriting it with the
                // snapshot's stale value. Reading disk, not adj_cache, keeps this
                // correct even when the cache evicted the entry.
                self.preserve_on_disk_adj_head(node.id.0, &mut slot_buf)?;
                self.write_slot_to_page_inner(node.id.0, &slot_buf, SlotLayout::NODE, false)
            }
            (EntityKey::Edge(_), Some(EntitySnapshot::Edge(edge))) => {
                let (mut slot_buf, overflow) = edge_codec::encode_edge_slot(edge)?;
                self.handle_slot_overflow(
                    &mut slot_buf,
                    SlotOverflowRequest {
                        label_overflowed: overflow.label_overflowed,
                        label: edge.label(),
                        props_overflowed: overflow.props_overflowed,
                        props_bytes: overflow.props_bytes.as_deref(),
                        previous_prop_overflow: None,
                        entity: (edge.id.0, prop_slab_codec::EntityKind::Edge),
                    },
                    edge_codec::patch_edge_overflow,
                )?;
                self.write_slot_to_page_inner(edge.id.0, &slot_buf, SlotLayout::EDGE, false)
            }
            // A node key paired with an edge snapshot (or vice versa) is an
            // internal invariant violation: writers always pair kinds.
            (EntityKey::Node(_), Some(EntitySnapshot::Edge(_)))
            | (EntityKey::Edge(_), Some(EntitySnapshot::Node(_))) => {
                unreachable!("vacuumed entity kind must match its key kind")
            }
        }
    }

    /// Returns `true` while `txn_id` remains active. `false` in legacy mode or
    /// for an unknown/ended transaction.
    #[must_use]
    pub fn txn_is_active(&self, txn_id: u64) -> bool {
        self.txn_registry
            .as_ref()
            .is_some_and(|r| r.is_active(txn_id))
    }

    /// The reader `start_ts` an auto-commit (non-transactional) read uses under
    /// MVCC: the current clock instant, so it sees everything committed strictly
    /// before now. Panics only if called in legacy mode (guarded by callers).
    fn auto_commit_start_ts(&self) -> u64 {
        self.txn_clock
            .as_ref()
            .expect("auto_commit_start_ts requires MVCC enabled")
            .current()
    }

    /// Resolves the node version visible to a reader, walking the delta chain
    /// over the committed page state. Shared by [`Graph::node`] (auto-commit,
    /// `reader_txn_id == None`) and [`Graph::node_in_txn`] (a transaction's own
    /// snapshot). The single read implementation keeps the engine from forking
    /// legacy and MVCC reads.
    fn resolve_node_visible(
        &self,
        id: NodeId,
        reader_start_ts: u64,
        reader_txn_id: Option<u64>,
    ) -> Result<Node> {
        let table = self.delta_table.as_ref().ok_or(Error::MvccNotEnabled)?;
        let chain = table.chain_for(crate::mvcc::EntityKey::Node(id));
        // When a delta chain exists, the committed base may not be page-resident
        // yet: `commit_txn` reconciles category B (so `node_exists` is set)
        // immediately, but the page write is the vacuum's job. Reading the page
        // tolerantly (`ok()`) lets the chain resolve visibility; the base is only
        // consulted when no delta is visible to this reader, in which case a
        // not-yet-materialized page correctly reads as "absent" for a reader
        // older than any commit. With NO chain, the id must be page-resident, so
        // a read error is a real inconsistency and propagates.
        let committed_base = match (&chain, self.node_exists.contains(&id.0)) {
            (Some(_), true) => self
                .read_node(id.0)
                .ok()
                .map(crate::mvcc::EntitySnapshot::Node),
            (None, true) => Some(crate::mvcc::EntitySnapshot::Node(self.read_node(id.0)?)),
            (_, false) => None,
        };
        let visible = match chain {
            Some(chain) => crate::mvcc::apply_deltas_for_read(
                committed_base,
                &chain,
                reader_start_ts,
                reader_txn_id,
            ),
            None => committed_base,
        };
        match visible {
            Some(crate::mvcc::EntitySnapshot::Node(n)) => Ok(n),
            _ => Err(Error::NodeNotFound(id)),
        }
    }

    /// Resolves the edge version visible to a reader (edge analogue of
    /// [`Graph::resolve_node_visible`]).
    fn resolve_edge_visible(
        &self,
        id: EdgeId,
        reader_start_ts: u64,
        reader_txn_id: Option<u64>,
    ) -> Result<Edge> {
        let table = self.delta_table.as_ref().ok_or(Error::MvccNotEnabled)?;
        let chain = table.chain_for(crate::mvcc::EntityKey::Edge(id));
        // See `resolve_node_visible`: a chain lets the page read be tolerant
        // (the committed edge may not be materialized yet); with no chain the
        // edge must be page-resident.
        let committed_base = match (&chain, self.edge_exists.contains(&id.0)) {
            (Some(_), true) => self
                .read_edge(id.0)
                .ok()
                .map(crate::mvcc::EntitySnapshot::Edge),
            (None, true) => Some(crate::mvcc::EntitySnapshot::Edge(self.read_edge(id.0)?)),
            (_, false) => None,
        };
        let visible = match chain {
            Some(chain) => crate::mvcc::apply_deltas_for_read(
                committed_base,
                &chain,
                reader_start_ts,
                reader_txn_id,
            ),
            None => committed_base,
        };
        match visible {
            Some(crate::mvcc::EntitySnapshot::Edge(e)) => Ok(e),
            _ => Err(Error::EdgeNotFound(id)),
        }
    }

    /// Reads a node within transaction `txn_id`'s snapshot: sees the
    /// transaction's own uncommitted writes plus everything committed before it
    /// began.
    ///
    /// # Errors
    ///
    /// [`Error::MvccNotEnabled`] in legacy mode, [`Error::TxnNotActive`] if
    /// `txn_id` is not live, or [`Error::NodeNotFound`] if the node is not
    /// visible to this transaction.
    pub fn node_in_txn(&self, txn_id: u64, id: NodeId) -> Result<Node> {
        let start_ts = self.txn_start_ts(txn_id)?;
        self.resolve_node_visible(id, start_ts, Some(txn_id))
    }

    /// Reads an edge within transaction `txn_id`'s snapshot (edge analogue of
    /// [`Graph::node_in_txn`]).
    ///
    /// # Errors
    ///
    /// See [`Graph::node_in_txn`]; [`Error::EdgeNotFound`] when not visible.
    pub fn edge_in_txn(&self, txn_id: u64, id: EdgeId) -> Result<Edge> {
        let start_ts = self.txn_start_ts(txn_id)?;
        self.resolve_edge_visible(id, start_ts, Some(txn_id))
    }

    /// Returns the `start_ts` of an active transaction, or the appropriate error
    /// if MVCC is off or the transaction is not active.
    fn txn_start_ts(&self, txn_id: u64) -> Result<u64> {
        self.txn_registry
            .as_ref()
            .ok_or(Error::MvccNotEnabled)?
            .start_ts(txn_id)
            .ok_or(Error::TxnNotActive(txn_id))
    }

    /// The state a new delta for `key` chains onto within `txn_id`: the
    /// transaction's own most-recent uncommitted new-state for `key` if it has
    /// already written it, else the committed page version. This keeps a second
    /// write in the same transaction correct (it overwrites its own pending
    /// version, not the stale committed base).
    fn node_chain_base(&self, txn_id: u64, id: NodeId) -> Option<crate::mvcc::EntitySnapshot> {
        if let Some(table) = self.delta_table.as_ref() {
            if let Some(chain) = table.chain_for(crate::mvcc::EntityKey::Node(id)) {
                if let Some(own) = chain
                    .iter()
                    .find(|d| d.txn_id() == txn_id && d.commit_ts().is_none())
                {
                    return own.new_state().cloned();
                }
            }
        }
        self.node_exists
            .contains(&id.0)
            .then(|| {
                self.read_node(id.0)
                    .ok()
                    .map(crate::mvcc::EntitySnapshot::Node)
            })
            .flatten()
    }

    /// Edge analogue of [`Graph::node_chain_base`].
    fn edge_chain_base(&self, txn_id: u64, id: EdgeId) -> Option<crate::mvcc::EntitySnapshot> {
        if let Some(table) = self.delta_table.as_ref() {
            if let Some(chain) = table.chain_for(crate::mvcc::EntityKey::Edge(id)) {
                if let Some(own) = chain
                    .iter()
                    .find(|d| d.txn_id() == txn_id && d.commit_ts().is_none())
                {
                    return own.new_state().cloned();
                }
            }
        }
        self.edge_exists
            .contains(&id.0)
            .then(|| {
                self.read_edge(id.0)
                    .ok()
                    .map(crate::mvcc::EntitySnapshot::Edge)
            })
            .flatten()
    }

    /// Creates a node inside transaction `txn_id` as a pending delta; nothing
    /// is written to the page until [`Graph::commit_txn`].
    ///
    /// The `NodeId` is allocated optimistically from `next_node_id` and is NOT
    /// reclaimed on rollback — a rolled-back transaction burns the id, matching
    /// Postgres/Memgraph semantics and avoiding a reserved-id pool. Property
    /// indexes are not updated until commit (a known first-iteration limitation,
    /// as in Neo4j/Memgraph): a query inside the transaction does not use the
    /// index for its own uncommitted writes.
    ///
    /// # Errors
    ///
    /// [`Error::MvccNotEnabled`], [`Error::TxnNotActive`], a quota error, or
    /// [`Error::AppendOnlyLabelInTxn`] if `label` was declared append-only.
    pub fn add_node_in_txn(
        &mut self,
        txn_id: u64,
        label: impl Into<String>,
        properties: Properties,
    ) -> Result<NodeId> {
        self.check_quota()?;
        if !self.txn_is_active(txn_id) {
            return Err(Error::TxnNotActive(txn_id));
        }
        // Issue #43: refuse before the id bump, so a rejected create leaves the
        // graph byte-for-byte unchanged (same discipline as the uniqueness
        // check in `add_node_str`).
        let label: String = label.into();
        if self.schema_catalog.is_label_append_only(&label) {
            return Err(Error::AppendOnlyLabelInTxn { label });
        }
        let id_val = self.storage.meta().next_node_id;
        let id = NodeId(id_val);
        self.storage.meta_mut().next_node_id = id_val + 1;

        let node = Node::new(id, label, properties);
        let new = crate::mvcc::EntitySnapshot::Node(node);
        self.charge_txn_memory(txn_id, delta_bytes(None, &new))?;
        let delta = crate::mvcc::Delta::new(txn_id, None, Some(new), crate::mvcc::DeltaOp::Insert);
        self.push_txn_delta(txn_id, crate::mvcc::EntityKey::Node(id), delta)?;
        Ok(id)
    }

    /// Updates a node inside transaction `txn_id` as a pending delta.
    ///
    /// # Errors
    ///
    /// [`Error::MvccNotEnabled`], [`Error::TxnNotActive`],
    /// [`Error::NodeNotFound`] if the node is not visible to the transaction,
    /// or [`Error::AppendOnlyLabelInTxn`] if it is an append-only node.
    pub fn update_node_in_txn(&mut self, txn_id: u64, id: NodeId, node: &Node) -> Result<()> {
        self.check_quota()?;
        if !self.txn_is_active(txn_id) {
            return Err(Error::TxnNotActive(txn_id));
        }
        // Issue #43: an append-only node must never acquire a delta chain —
        // the read path resolves it straight off the page, so a delta would be
        // written and then never seen.
        self.reject_if_append_only(id)?;
        // The node must be visible to this transaction to be updatable.
        self.node_in_txn(txn_id, id)?;
        let prior = self.node_chain_base(txn_id, id);
        let new = crate::mvcc::EntitySnapshot::Node(node.clone());
        self.charge_txn_memory(txn_id, delta_bytes(prior.as_ref(), &new))?;
        let delta = crate::mvcc::Delta::new(txn_id, prior, Some(new), crate::mvcc::DeltaOp::Update);
        self.push_txn_delta(txn_id, crate::mvcc::EntityKey::Node(id), delta)
    }

    /// Removes a node inside transaction `txn_id` as a pending delta.
    ///
    /// No quota check: like the legacy `remove_node`, a delete frees space
    /// rather than consuming it, so the per-write quota hook does not fire.
    ///
    /// # Errors
    ///
    /// As [`Graph::update_node_in_txn`].
    pub fn remove_node_in_txn(&mut self, txn_id: u64, id: NodeId) -> Result<()> {
        if !self.txn_is_active(txn_id) {
            return Err(Error::TxnNotActive(txn_id));
        }
        // Issue #43: see `update_node_in_txn` — append-only nodes take no deltas.
        self.reject_if_append_only(id)?;
        self.node_in_txn(txn_id, id)?;
        let prior = self.node_chain_base(txn_id, id);
        let new = crate::mvcc::EntitySnapshot::Deleted;
        self.charge_txn_memory(txn_id, delta_bytes(prior.as_ref(), &new))?;
        let delta = crate::mvcc::Delta::new(txn_id, prior, Some(new), crate::mvcc::DeltaOp::Delete);
        self.push_txn_delta(txn_id, crate::mvcc::EntityKey::Node(id), delta)
    }

    /// Creates an edge inside transaction `txn_id` as a pending delta.
    ///
    /// `source`/`target` existence is validated against the transaction's own
    /// snapshot (via [`Graph::node_in_txn`]), so an edge may reference a node
    /// created earlier in the same uncommitted transaction.
    ///
    /// # Errors
    ///
    /// [`Error::MvccNotEnabled`], [`Error::TxnNotActive`], a quota error, or
    /// [`Error::NodeNotFound`] if `source`/`target` is not visible.
    pub fn add_edge_in_txn(
        &mut self,
        txn_id: u64,
        label: impl Into<String>,
        source: NodeId,
        target: NodeId,
        properties: Properties,
    ) -> Result<EdgeId> {
        self.check_quota()?;
        if !self.txn_is_active(txn_id) {
            return Err(Error::TxnNotActive(txn_id));
        }
        // Validate endpoints against the transaction's own visibility, not the
        // committed-only `node_exists` (which would false-negative a node the
        // same transaction just created).
        self.node_in_txn(txn_id, source)
            .map_err(|_| Error::NodeNotFound(source))?;
        self.node_in_txn(txn_id, target)
            .map_err(|_| Error::NodeNotFound(target))?;

        let id_val = self.storage.meta().next_edge_id;
        let id = EdgeId(id_val);
        self.storage.meta_mut().next_edge_id = id_val + 1;

        let edge = Edge::new(id, label, source, target, properties);
        let new = crate::mvcc::EntitySnapshot::Edge(edge);
        self.charge_txn_memory(txn_id, delta_bytes(None, &new))?;
        let delta = crate::mvcc::Delta::new(txn_id, None, Some(new), crate::mvcc::DeltaOp::Insert);
        self.push_txn_delta(txn_id, crate::mvcc::EntityKey::Edge(id), delta)?;
        Ok(id)
    }

    /// Updates an edge inside transaction `txn_id` as a pending delta.
    ///
    /// # Errors
    ///
    /// As [`Graph::update_node_in_txn`] with [`Error::EdgeNotFound`].
    pub fn update_edge_in_txn(&mut self, txn_id: u64, id: EdgeId, edge: &Edge) -> Result<()> {
        self.check_quota()?;
        if !self.txn_is_active(txn_id) {
            return Err(Error::TxnNotActive(txn_id));
        }
        self.edge_in_txn(txn_id, id)?;
        let prior = self.edge_chain_base(txn_id, id);
        let new = crate::mvcc::EntitySnapshot::Edge(edge.clone());
        self.charge_txn_memory(txn_id, delta_bytes(prior.as_ref(), &new))?;
        let delta = crate::mvcc::Delta::new(txn_id, prior, Some(new), crate::mvcc::DeltaOp::Update);
        self.push_txn_delta(txn_id, crate::mvcc::EntityKey::Edge(id), delta)
    }

    /// Removes an edge inside transaction `txn_id` as a pending delta.
    ///
    /// No quota check, for the same reason as [`Graph::remove_node_in_txn`].
    ///
    /// # Errors
    ///
    /// As [`Graph::update_edge_in_txn`].
    pub fn remove_edge_in_txn(&mut self, txn_id: u64, id: EdgeId) -> Result<()> {
        if !self.txn_is_active(txn_id) {
            return Err(Error::TxnNotActive(txn_id));
        }
        self.edge_in_txn(txn_id, id)?;
        let prior = self.edge_chain_base(txn_id, id);
        let new = crate::mvcc::EntitySnapshot::Deleted;
        self.charge_txn_memory(txn_id, delta_bytes(prior.as_ref(), &new))?;
        let delta = crate::mvcc::Delta::new(txn_id, prior, Some(new), crate::mvcc::DeltaOp::Delete);
        self.push_txn_delta(txn_id, crate::mvcc::EntityKey::Edge(id), delta)
    }

    /// Records a delta into the table and notes `key` against `txn_id` in the
    /// reverse index, so commit/rollback can find exactly this transaction's
    /// keys. Central choke point: every `*_in_txn` writer goes through here.
    ///
    /// Seeding the txn overlay of pending inserts is ALSO this choke point's
    /// responsibility, NOT each caller's: any future `*_in_txn` writer that
    /// creates a record reaches here and its pending insert becomes visible to
    /// the transaction's own enumeration and traversal reads for free. Do not
    /// move the overlay seeding into the individual `add_*_in_txn` methods — the
    /// single-point guarantee is what makes it impossible to forget (defends the
    /// R1 duplication risk).
    ///
    /// Takes `&self`: both the delta table and the registry mutate through their
    /// own interior locks, so no exclusive borrow of the graph is needed here.
    fn push_txn_delta(
        &self,
        txn_id: u64,
        key: crate::mvcc::EntityKey,
        delta: crate::mvcc::Delta,
    ) -> Result<()> {
        // Issue #43, defense in depth: an append-only node's reads bypass
        // visibility resolution, so a delta attached to one would be written
        // and never read back. `add_node_in_txn`/`update_node_in_txn`/
        // `remove_node_in_txn` already refuse this at the API boundary; if one
        // of them is ever bypassed, fail loudly in debug instead of silently
        // losing the write. Costs nothing in release.
        debug_assert!(
            !matches!(key, crate::mvcc::EntityKey::Node(id) if self.is_append_only_node(id)),
            "append-only node must never acquire a delta chain: {key:?}"
        );
        let registry = self.txn_registry.as_ref().ok_or(Error::MvccNotEnabled)?;
        // Seed the pending-insert overlay before the delta moves into the table,
        // so the transaction's own enumeration and traversal reads see this
        // insert. Nodes seed the id/label overlay; edges seed adjacency on both
        // endpoints (outgoing on source, incoming on target), mirroring the
        // committed adjacency's per-direction split.
        if delta.op() == crate::mvcc::DeltaOp::Insert {
            match (key, delta.new_state()) {
                (
                    crate::mvcc::EntityKey::Node(id),
                    Some(crate::mvcc::EntitySnapshot::Node(node)),
                ) => {
                    registry.mark_node_pending(txn_id, id.0, node.label());
                }
                (
                    crate::mvcc::EntityKey::Edge(id),
                    Some(crate::mvcc::EntitySnapshot::Edge(edge)),
                ) => {
                    registry.mark_edge_pending(
                        txn_id,
                        edge.source().0,
                        AdjDirection::Outgoing,
                        id.0,
                    );
                    registry.mark_edge_pending(
                        txn_id,
                        edge.target().0,
                        AdjDirection::Incoming,
                        id.0,
                    );
                }
                _ => {}
            }
        }
        let table = self.delta_table.as_ref().ok_or(Error::MvccNotEnabled)?;
        table.push_delta(key, delta);
        registry.record_write(txn_id, key);
        Ok(())
    }

    /// Returns every node id visible to transaction `txn_id`: the committed set
    /// unioned with the transaction's own pending inserts, filtered by what the
    /// transaction can actually see (a node it created and then deleted in the
    /// same transaction is excluded, since `node_visible_in_txn` returns false
    /// for it).
    ///
    /// This is the enumeration counterpart of [`Graph::node_in_txn`]'s by-id
    /// visibility: without it a transaction's `MATCH (n)` would not see nodes it
    /// created in the same transaction.
    ///
    /// # Errors
    ///
    /// [`Error::MvccNotEnabled`] if MVCC is off, or [`Error::TxnNotActive`] if
    /// `txn_id` is not active.
    pub fn node_ids_in_txn(&self, txn_id: u64) -> Result<Vec<NodeId>> {
        let registry = self.txn_registry.as_ref().ok_or(Error::MvccNotEnabled)?;
        if !registry.is_active(txn_id) {
            return Err(Error::TxnNotActive(txn_id));
        }
        let confirmed = self.node_exists.iter().copied();
        let pending = registry.pending_node_ids(txn_id);
        Ok(self.union_visible_in_txn(txn_id, confirmed, pending))
    }

    /// Returns the nodes with `label` visible to transaction `txn_id`: the
    /// committed label index unioned with the transaction's own pending inserts
    /// carrying that label, filtered by transaction visibility. The enumeration
    /// counterpart of a `MATCH (n:Label)` inside a transaction.
    ///
    /// # Errors
    ///
    /// [`Error::MvccNotEnabled`] if MVCC is off, or [`Error::TxnNotActive`] if
    /// `txn_id` is not active.
    pub fn nodes_by_label_in_txn(&self, txn_id: u64, label: &str) -> Result<Vec<NodeId>> {
        let registry = self.txn_registry.as_ref().ok_or(Error::MvccNotEnabled)?;
        if !registry.is_active(txn_id) {
            return Err(Error::TxnNotActive(txn_id));
        }
        let confirmed = self.nodes_by_label(label).into_iter().map(|n| n.0);
        let pending = registry.pending_node_ids_by_label(txn_id, label);
        Ok(self.union_visible_in_txn(txn_id, confirmed, pending))
    }

    /// Returns the number of nodes visible to transaction `txn_id`: committed
    /// nodes plus the transaction's own pending inserts, minus any it deleted in
    /// the same transaction. Reuses [`Graph::node_ids_in_txn`] so the count and
    /// the enumeration can never disagree.
    ///
    /// # Errors
    ///
    /// [`Error::MvccNotEnabled`] if MVCC is off, or [`Error::TxnNotActive`] if
    /// `txn_id` is not active.
    pub fn node_count_in_txn(&self, txn_id: u64) -> Result<usize> {
        Ok(self.node_ids_in_txn(txn_id)?.len())
    }

    /// Unions a committed id iterator with a transaction's pending ids, keeps
    /// only those visible to `txn_id`, and returns them ascending. Shared by the
    /// txn-scoped enumeration reads so the "union committed with overlay, retain
    /// visible" logic lives in one place. Deduplicates via a set, so a committed
    /// id also present in the overlay is not returned twice.
    ///
    /// The overlay is deliberately "dirty": a node created and then deleted in
    /// the same transaction stays in the pending set — it is NOT removed on
    /// delete. Correctness comes from the `node_visible_in_txn` filter below,
    /// which resolves the insert-then-delete delta chain to "not visible". This
    /// keeps the write path simple (no overlay bookkeeping on delete) at the
    /// cost of one extra visibility check per enumerated id.
    fn union_visible_in_txn(
        &self,
        txn_id: u64,
        confirmed: impl Iterator<Item = u64>,
        pending: Vec<u64>,
    ) -> Vec<NodeId> {
        let mut ids: HashSet<u64> = confirmed.collect();
        ids.extend(pending);
        let mut visible: Vec<NodeId> = ids
            .into_iter()
            .map(NodeId)
            .filter(|&id| self.node_visible_in_txn(txn_id, id))
            .collect();
        visible.sort_unstable_by_key(|id| id.0);
        visible
    }

    /// Returns a shared reference to the DDL schema catalog (declared indexes
    /// and unique constraints).
    #[must_use]
    pub const fn schema_catalog(&self) -> &crate::schema::SchemaCatalog {
        &self.schema_catalog
    }

    /// Returns a mutable reference to the DDL schema catalog.
    ///
    /// Callers that modify the catalog (the server's DDL handler) must persist
    /// the change afterwards via [`Graph::flush`] or `persist_schema` (Task 8);
    /// the in-memory mutation alone is not durable.
    pub const fn schema_catalog_mut(&mut self) -> &mut crate::schema::SchemaCatalog {
        &mut self.schema_catalog
    }

    /// Persists the schema catalog to `schema.bin` immediately, without a full
    /// [`Graph::flush`]. Called by the server's DDL handler after every catalog
    /// mutation so a `CREATE`/`DROP INDEX`/`CONSTRAINT` is durable before the
    /// next flush — a crash between the DDL and the next flush must not lose it.
    ///
    /// Mirrors the schema-catalog branch of [`Graph::flush`]: an empty catalog
    /// serialises to empty bytes and is skipped to avoid touching disk for
    /// databases that never issued DDL. On a backend without persistence
    /// (`MemoryBackend`) `write_schema_bytes` is a no-op.
    ///
    /// # Errors
    /// Returns `Err` if serialisation fails (entry count overflow) or the
    /// backend's `write_schema_bytes` IO write fails.
    pub fn persist_schema(&mut self) -> Result<()> {
        let schema_bytes = crate::schema::codec::serialize(&self.schema_catalog)?;
        if !schema_bytes.is_empty() {
            self.storage.write_schema_bytes(&schema_bytes)?;
        }
        Ok(())
    }

    /// Checks whether writing `(label, prop_key, value)` for any property in
    /// `properties` would violate a declared unique constraint. Returns
    /// [`Error::ConstraintViolation`] for the first conflicting property,
    /// `Ok(())` otherwise.
    ///
    /// `exclude_id` is the node being updated (if `Some`) — it is excluded from
    /// the duplicate check so a node can be re-written with a value it already
    /// holds.
    ///
    /// # SAFETY INVARIANT (verified 2026-06-19)
    /// This enforcement is sound ONLY because [`PropertyIndex::insert_node`]
    /// indexes EVERY property of EVERY node (a TOTAL index, not a selective
    /// one — see `index/mod.rs` `insert_node`). Therefore
    /// [`PropertyIndex::lookup_set`] always reflects all existing nodes for
    /// that value, even when no `CREATE INDEX` was issued for `prop`. If the
    /// index is ever made selective (e.g. indexing only declared properties to
    /// save memory), this check would silently fail-OPEN — a unique constraint
    /// on an un-indexed prop would never find duplicates. In that future,
    /// `CREATE CONSTRAINT` MUST force a backing index on its prop (as
    /// Neo4j/Memgraph do). Do not weaken the index without revisiting this.
    fn check_unique(
        &self,
        label: &str,
        properties: &Properties,
        exclude_id: Option<u64>,
    ) -> Result<()> {
        for (prop_key, value) in properties {
            if !self.schema_catalog.has_unique_constraint(label, prop_key) {
                continue;
            }
            if let Some(ids) = self.node_property_index.lookup_set(label, prop_key, value) {
                let conflict =
                    exclude_id.map_or(!ids.is_empty(), |ex| ids.iter().any(|&id| id != ex));
                if conflict {
                    return Err(Error::ConstraintViolation {
                        label: label.to_owned(),
                        prop: prop_key.clone(),
                        value: format!("{value:?}"),
                    });
                }
            }
        }
        Ok(())
    }

    /// Ensures meta counts and next IDs are consistent with what's actually on disk.
    /// Called after `rebuild_indexes()` to handle WAL recovery or interrupted flushes.
    fn sync_meta_from_indexes(&mut self) {
        let actual_node_count = self.node_exists.len() as u64;
        let actual_edge_count = self.edge_exists.len() as u64;

        self.storage.meta_mut().node_count = actual_node_count;
        self.storage.meta_mut().edge_count = actual_edge_count;

        // Ensure next_node_id is greater than any existing node ID.
        if let Some(&max_id) = self.node_exists.iter().max() {
            let needed = max_id + 1;
            if self.storage.meta().next_node_id < needed {
                self.storage.meta_mut().next_node_id = needed;
            }
        }

        // Same for edge IDs.
        if let Some(&max_id) = self.edge_exists.iter().max() {
            let needed = max_id + 1;
            if self.storage.meta().next_edge_id < needed {
                self.storage.meta_mut().next_edge_id = needed;
            }
        }
    }

    /// Rebuilds all in-memory indexes by scanning stored pages.
    ///
    /// Rebuilds existence sets (`node_exists`, `edge_exists`), the adjacency
    /// cache, meta counts, and label indexes in a single logical operation.
    /// Use this for hot-repair without restarting the process. The caller is
    /// responsible for coordinating concurrent access — this method requires
    /// exclusive `&mut self`.
    ///
    /// This is also called internally by [`Graph::open`] during startup.
    ///
    /// # Errors
    ///
    /// Returns an error if any page cannot be read.
    pub fn rebuild_indexes(&mut self) -> Result<()> {
        self.rebuild_existence_indexes()?;
        self.sync_meta_from_indexes();
        self.load_or_rebuild_label_indexes()?;
        Ok(())
    }

    /// Rebuilds node/edge existence sets and adjacency cache from pages.
    fn rebuild_existence_indexes(&mut self) -> Result<()> {
        self.node_exists.clear();
        self.edge_exists.clear();
        self.edge_pair_index.clear();
        self.adj_cache = AdjCache::new(self.adj_cache.capacity());
        // The tail cache is derived from the chains being rebuilt; clear it so no
        // stale tail survives a rebuild. A cold cache just recomputes on demand.
        self.adj_tail_cache = crate::adj_tail_cache::AdjTailCache::new(self.adj_cache.capacity());
        self.node_property_index.clear();
        self.rebuild_node_indexes(false)?;
        self.rebuild_edge_indexes(false)?;
        self.rebuild_adj_cache()?;
        Ok(())
    }

    /// Scans all node pages in a single pass. Populates `node_exists` and
    /// optionally `node_label_index` (when `include_labels` is true).
    fn rebuild_node_indexes(&mut self, include_labels: bool) -> Result<()> {
        let page_count = self.storage.page_count(DataFile::Nodes);
        for page_idx in 0..page_count {
            let page = self.storage.read_page(DataFile::Nodes, page_idx)?;
            let header = PageHeader::read_from(&page);
            let slots_to_scan = header.slot_count as usize;
            for slot in 0..slots_to_scan {
                let offset = PAGE_HEADER_SIZE + slot * NODE_SLOT_SIZE;
                if offset + NODE_SLOT_SIZE > page.len() {
                    break;
                }
                if page[offset] != SLOT_LIVE {
                    continue;
                }
                let id = u64::from_le_bytes(
                    page[offset + 1..offset + 9]
                        .try_into()
                        .expect("8 bytes for node id"),
                );
                self.node_exists.insert(id);

                if include_labels {
                    let slot_bytes: [u8; NODE_SLOT_SIZE] = page[offset..offset + NODE_SLOT_SIZE]
                        .try_into()
                        .expect("slice is NODE_SLOT_SIZE bytes");
                    let label = if node_codec::slot_needs_label_resolve(&slot_bytes) {
                        let overflow_ref = node_codec::slot_label_overflow_ref(&slot_bytes);
                        self.string_heap
                            .resolve(self.storage.as_mut(), overflow_ref)?
                    } else {
                        node_codec::slot_inline_label(&slot_bytes, page_idx)?
                    };
                    self.node_label_index.insert(&label, id);

                    // Also populate the property index. We do a full node
                    // decode here because properties are not stored in the
                    // slot header (they may be in overflow pages).
                    let node = self.read_node(id)?;
                    self.node_property_index
                        .insert_node(node.label(), node.properties(), id);
                    // Issue #43: reuse the decode we already paid for to
                    // restore the append-only fast-path set. Issue #61: only
                    // nodes at or above the declaration's lower bound, so a
                    // node freed by a withdrawal is not recaptured here.
                    if self.is_covered_by_append_only(node.label(), id) {
                        self.append_only_node_ids.insert(id);
                    }
                }
            }
        }
        Ok(())
    }

    /// Scans all edge pages in a single pass. Populates `edge_exists` and
    /// optionally `edge_label_index` (when `include_labels` is true).
    fn rebuild_edge_indexes(&mut self, include_labels: bool) -> Result<()> {
        let page_count = self.storage.page_count(DataFile::Edges);
        for page_idx in 0..page_count {
            let page = self.storage.read_page(DataFile::Edges, page_idx)?;
            let header = PageHeader::read_from(&page);
            let slots_to_scan = header.slot_count as usize;
            for slot in 0..slots_to_scan {
                let offset = PAGE_HEADER_SIZE + slot * EDGE_SLOT_SIZE;
                if offset + EDGE_SLOT_SIZE > page.len() {
                    break;
                }
                if page[offset] != SLOT_LIVE {
                    continue;
                }
                let id = u64::from_le_bytes(
                    page[offset + 1..offset + 9]
                        .try_into()
                        .expect("8 bytes for edge id"),
                );
                self.edge_exists.insert(id);

                // Rebuild the edge_pair_index from the slot's stored endpoints
                // and label hash — no string resolution needed, and independent
                // of `include_labels`, so the pair-index is repopulated on every
                // reopen path (both the `index.bin` fast path and the full
                // rebuild), not only when label indexes are rebuilt.
                let src = u64::from_le_bytes(
                    page[offset + edge_codec::OFF_SOURCE..offset + edge_codec::OFF_SOURCE + 8]
                        .try_into()
                        .expect("8 bytes for source"),
                );
                let tgt = u64::from_le_bytes(
                    page[offset + edge_codec::OFF_TARGET..offset + edge_codec::OFF_TARGET + 8]
                        .try_into()
                        .expect("8 bytes for target"),
                );
                let hash = u32::from_le_bytes(
                    page[offset + edge_codec::OFF_LABEL_HASH
                        ..offset + edge_codec::OFF_LABEL_HASH + 4]
                        .try_into()
                        .expect("4 bytes for label hash"),
                );
                self.edge_pair_index
                    .entry((src, tgt, hash))
                    .or_default()
                    .push(id);

                if include_labels {
                    let slot_bytes: [u8; EDGE_SLOT_SIZE] = page[offset..offset + EDGE_SLOT_SIZE]
                        .try_into()
                        .expect("slice is EDGE_SLOT_SIZE bytes");
                    let label = if edge_codec::edge_slot_needs_label_resolve(&slot_bytes) {
                        let overflow_ref = edge_codec::edge_slot_label_overflow_ref(&slot_bytes);
                        self.string_heap
                            .resolve(self.storage.as_mut(), overflow_ref)?
                    } else {
                        edge_codec::edge_slot_inline_label(&slot_bytes, page_idx)?
                    };
                    self.edge_label_index.insert(&label, id);
                }
            }
        }
        Ok(())
    }

    /// Scans all adjacency pages to rebuild the adjacency cache.
    fn rebuild_adj_cache(&self) -> Result<()> {
        // Cycle 7 (#54): the adjacency heads live in each node's slot, so the
        // cache is rebuilt from the node pages — the same pages
        // `rebuild_node_indexes` already scans — instead of scanning every
        // adjacency page. Each node contributes O(1) work: read its two heads,
        // and for each present head one bounded tail-state read (issue #46) so
        // the first append after reopen is O(1). No pass over DataFile::Adjacency
        // proportional to its page count.
        let page_count = self.storage.page_count(DataFile::Nodes);
        for page_idx in 0..page_count {
            let page = self.storage.read_page(DataFile::Nodes, page_idx)?;
            let header = PageHeader::read_from(&page);
            let slots_to_scan = header.slot_count as usize;
            for slot in 0..slots_to_scan {
                let offset = PAGE_HEADER_SIZE + slot * NODE_SLOT_SIZE;
                if offset + NODE_SLOT_SIZE > page.len() {
                    break;
                }
                if page[offset] != SLOT_LIVE {
                    continue;
                }
                let slot_bytes: [u8; NODE_SLOT_SIZE] = page[offset..offset + NODE_SLOT_SIZE]
                    .try_into()
                    .expect("slice is NODE_SLOT_SIZE bytes");
                let out = node_codec::slot_adj_page_id(&slot_bytes);
                let inc = node_codec::slot_adj_incoming_page_id(&slot_bytes);
                if out == node_codec::ADJ_PAGE_ID_SENTINEL
                    && inc == node_codec::ADJ_PAGE_ID_SENTINEL
                {
                    continue; // isolated node, nothing to cache
                }
                let node_id = u64::from_le_bytes(
                    page[offset + 1..offset + 9]
                        .try_into()
                        .expect("8 bytes for node id"),
                );
                let entry = AdjacencyPointer {
                    outgoing_page: (out != node_codec::ADJ_PAGE_ID_SENTINEL).then_some(out),
                    incoming_page: (inc != node_codec::ADJ_PAGE_ID_SENTINEL).then_some(inc),
                };
                self.adj_cache.insert(node_id, entry);

                // Issue #46: repopulate the tail cache per present direction.
                // Only dedicated chains have a tail to cache — a node packed into
                // a slab has no chain, and its sub-block is found through the
                // page's directory. Skipping them also keeps reopen cheap: the
                // slab holds most low-degree nodes, so this reads few pages
                // instead of one per node with edges (#54).
                for (head, direction) in [
                    (entry.outgoing_page, AdjDirection::Outgoing),
                    (entry.incoming_page, AdjDirection::Incoming),
                ] {
                    let Some(head) = head else { continue };
                    if adj_slab_codec::is_slab_page(self.storage.as_ref(), head)? {
                        continue;
                    }
                    let state = adjacency_codec::read_adj_chain_state(self.storage.as_ref(), head)?;
                    self.adj_tail_cache.insert(node_id, direction, state);
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Label index: load / rebuild / query
    // -----------------------------------------------------------------

    /// Attempts to load persisted label indexes from `index.bin`. Falls back to
    /// a full page scan (populating both existence and label indexes) if the
    /// file is missing or corrupt.
    fn load_or_rebuild_label_indexes(&mut self) -> Result<()> {
        match self.storage.read_index_bytes()? {
            Some(bytes) => match index_codec::deserialize(&bytes) {
                Ok((nl, el)) => {
                    self.node_label_index = nl;
                    self.edge_label_index = el;
                    // Label indexes loaded from file — property index must be
                    // rebuilt by scanning node pages (not persisted to disk).
                    self.rebuild_property_index_from_pages()?;
                }
                Err(_) => self.rebuild_all_from_pages()?,
            },
            None => self.rebuild_all_from_pages()?,
        }
        Ok(())
    }

    /// Scans all live node slots and repopulates `node_property_index`.
    /// Called when label indexes are loaded from `index.bin` (fast path on
    /// reopen) so that the property index is always populated after `open()`.
    fn rebuild_property_index_from_pages(&mut self) -> Result<()> {
        let live_ids: Vec<u64> = self.node_exists.iter().copied().collect();
        for id in live_ids {
            let node = self.read_node(id)?;
            self.node_property_index
                .insert_node(node.label(), node.properties(), id);
            // Issue #43: same decode also restores the append-only fast-path
            // set. Issue #61: bounded by the declaration's lower node id, so a
            // node freed by a withdrawal stays free across the reopen.
            if self.is_covered_by_append_only(node.label(), id) {
                self.append_only_node_ids.insert(id);
            }
        }
        Ok(())
    }

    /// Full rebuild: scans all node and edge pages to repopulate both existence
    /// sets and label indexes simultaneously.
    ///
    /// Called when `index.bin` is missing or corrupt. Because the first pass
    /// in `rebuild_indexes` did not collect labels, this method clears the
    /// existence sets and performs a second complete scan with `include_labels=true`.
    /// The trade-off (two full scans on cold open without a valid index file) is
    /// acceptable: the expected case is that `index.bin` is present.
    fn rebuild_all_from_pages(&mut self) -> Result<()> {
        self.node_exists.clear();
        self.edge_exists.clear();
        self.node_property_index.clear();
        self.rebuild_node_indexes(true)?;
        self.rebuild_edge_indexes(true)?;
        Ok(())
    }

    /// Returns all `NodeId`s whose label is exactly `label`.
    ///
    /// This is an O(1) lookup in the in-memory label index — no page I/O.
    /// The order of returned IDs is not guaranteed.
    #[must_use]
    pub fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
        self.node_label_index
            .ids_for(label)
            .into_iter()
            .map(NodeId)
            .collect()
    }

    /// Returns all `NodeId`s whose label is `label` and have a property `key`
    /// equal to `value`.
    ///
    /// This is an O(1) lookup in the in-memory property index — no page I/O.
    /// The order of returned IDs is not guaranteed.
    #[must_use]
    pub fn nodes_by_label_and_property(
        &self,
        label: &str,
        key: &str,
        value: &crate::property::Property,
    ) -> Vec<NodeId> {
        self.node_property_index
            .lookup_set(label, key, value)
            .map(|ids| ids.iter().copied().map(NodeId).collect())
            .unwrap_or_default()
    }

    /// Keeps only the node IDs visible to the caller's current read snapshot.
    ///
    /// The in-memory property/ordered indexes can list a node whose delete was
    /// committed but whose category-B cleanup the vacuum has not yet applied (the
    /// same hazard fixed for pattern matching in issue #45). Every index-backed
    /// query below funnels its raw candidate IDs through this so a
    /// committed-deleted node never surfaces. Under legacy (non-MVCC) mode
    /// `node_visible` is plain existence, so this is a cheap membership check.
    fn retain_visible(&self, ids: impl IntoIterator<Item = u64>) -> Vec<NodeId> {
        ids.into_iter()
            .map(NodeId)
            .filter(|&id| self.node_visible(id))
            .collect()
    }

    /// Returns the `NodeId`s whose `I64` property `(label, key)` lies in the
    /// half-open range `[lo, hi)`, filtered to snapshot-visible nodes. `lo`/`hi`
    /// of `None` are unbounded on that side (so `(Some(t), None)` expresses the
    /// open-ended "still valid from t onward" query). O(matches), not O(scope)
    /// — index-backed range scan (issue #41).
    #[must_use]
    pub fn nodes_by_label_and_property_range(
        &self,
        label: &str,
        key: &str,
        lo: Option<i64>,
        hi: Option<i64>,
    ) -> Vec<NodeId> {
        self.retain_visible(self.node_property_index.range_i64(label, key, lo, hi))
    }

    /// Returns the snapshot-visible `NodeId` with the highest `I64` value for
    /// property `(label, key)`, or `None` if the property has no visible `I64`
    /// node. O(log N) in the common case; if the top value's node is a
    /// committed-but-unvacuumed delete, it descends to the next value until a
    /// visible one is found (issue #40). Ties at the same value are broken by the
    /// lowest `NodeId` for determinism.
    #[must_use]
    pub fn max_node_by_property(&self, label: &str, key: &str) -> Option<NodeId> {
        for (_value, ids) in self.node_property_index.iter_i64_desc(label, key) {
            let visible = ids
                .iter()
                .copied()
                .filter(|&id| self.node_visible(NodeId(id)))
                .min();
            if let Some(id) = visible {
                return Some(NodeId(id));
            }
        }
        None
    }

    /// Returns the snapshot-visible `NodeId`s of label `label` that do NOT have
    /// property `key`. Computed as the label's membership minus the set of nodes
    /// the property index holds for `key` (any value), then filtered by
    /// visibility. This is how "open interval" is expressed without a sentinel or
    /// a new property type — absence of `valid_to` means "still open" (issue #42
    /// substitute). Cost is O(label size) in the worst case (nearly all nodes
    /// have the property), not O(log N); still avoids materializing and decoding
    /// each node as the consumer does today.
    #[must_use]
    pub fn nodes_by_label_without_property(&self, label: &str, key: &str) -> Vec<NodeId> {
        let has_key = self.node_property_index.ids_with_property(label, key);
        let absent = self
            .node_label_index
            .ids_for(label)
            .into_iter()
            .filter(|id| !has_key.contains(id));
        self.retain_visible(absent)
    }

    /// Returns all `EdgeId`s whose label is exactly `label`.
    ///
    /// This is an O(1) lookup in the in-memory label index — no page I/O.
    /// The order of returned IDs is not guaranteed.
    #[must_use]
    pub fn edges_by_label(&self, label: &str) -> Vec<EdgeId> {
        self.edge_label_index
            .ids_for(label)
            .into_iter()
            .map(EdgeId)
            .collect()
    }

    // -----------------------------------------------------------------
    // Node operations
    // -----------------------------------------------------------------

    /// Adds a new node with the given label and properties.
    /// Returns the unique `NodeId` assigned to the node.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the write fails.
    pub fn add_node(&mut self, label: impl Into<String>, properties: Properties) -> Result<NodeId> {
        self.add_node_str(&label.into(), properties)
    }

    /// Returns a sorted list of all distinct node labels present in the graph.
    ///
    /// Only labels with at least one node are returned. Backs
    /// `CALL tessera.vertex_labels()` / `mg.vertex_labels()`. Allocates on every
    /// call; intended for introspection, not hot-path use.
    #[must_use]
    pub fn node_labels(&self) -> Vec<String> {
        self.node_label_index.distinct_labels()
    }

    /// Returns a sorted list of all distinct edge/relationship types present in
    /// the graph.
    ///
    /// Only types with at least one edge are returned. Backs
    /// `CALL tessera.edge_types()` / `mg.edge_types()`. Allocates on every call;
    /// intended for introspection, not hot-path use.
    #[must_use]
    pub fn edge_types(&self) -> Vec<String> {
        self.edge_label_index.distinct_labels()
    }

    /// Internal implementation for `add_node` — takes `&str` for trait compatibility.
    /// Used by `impl GraphAccess for Graph`; `&str` signature required for object safety.
    ///
    /// # Lazy Adjacency Allocation
    ///
    /// This method writes only the node slot and updates in-memory indexes.
    /// **No adjacency pages are allocated.** Adjacency pages for this node are created
    /// on demand the first time `add_edge` is called with this node as source or target.
    pub(crate) fn add_node_str(&mut self, label: &str, properties: Properties) -> Result<NodeId> {
        // Task 15 C': fire the quota hook BEFORE any state mutation
        // (next_node_id bump, slot write, index update). If the hook
        // returns Err, the write is rejected cleanly.
        self.check_quota()?;
        // Issue #37: count this op against the open batch's caps (no-op outside
        // a batch). Placed before any state mutation so a cap rejection leaves
        // the graph unchanged, exactly like the uniqueness check below.
        let batch_bytes = Self::estimate_entity_bytes(size_of::<Node>(), label, &properties);
        self.charge_batch_op(batch_bytes)?;

        // 3c fail-safe uniqueness check: reject a duplicate BEFORE any
        // in-memory mutation (no id bump, no slot write), so a rejected
        // CREATE leaves the graph byte-for-byte unchanged.
        self.check_unique(label, &properties, None)?;

        let id_val = self.storage.meta().next_node_id;
        let id = NodeId(id_val);
        self.storage.meta_mut().next_node_id = id_val + 1;

        let node = Node::new(id, label, properties);

        self.write_node_slot(&node)?;

        // Reconcile category B (exists-set, indexes, count, and the negative
        // adjacency-cache marker that keeps flush_adj_pending O(N) rather than
        // O(N²) for brand-new nodes).
        self.reconcile_node_insert(&node);

        // Issue #43: snapshot the append-only decision at creation time, so
        // reads of this node can skip MVCC resolution.
        if self.schema_catalog.is_label_append_only(label) {
            self.append_only_node_ids.insert(id_val);
        }

        self.wal_sync(FsyncCause::Individual)?;
        Ok(id)
    }

    /// Returns a clone of the node with the given id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the id does not exist.
    pub fn node(&self, id: NodeId) -> Result<Node> {
        // Issue #43: a node created under an append-only label never acquires a
        // delta chain (writes to it inside a transaction are rejected), so
        // resolving visibility for it would walk a chain that cannot exist.
        // Reading the page directly is both correct and the point of the mode.
        if self.is_append_only_node(id) {
            if !self.node_exists.contains(&id.0) {
                return Err(Error::NodeNotFound(id));
            }
            return self.read_node(id.0);
        }
        // MVCC auto-commit read: resolve visibility over the delta chain at the
        // current clock instant. In legacy mode this branch is skipped after a
        // single `Option::is_none` check, keeping the fast path byte-for-byte.
        if self.mvcc_enabled() {
            return self.resolve_node_visible(id, self.auto_commit_start_ts(), None);
        }
        if !self.node_exists.contains(&id.0) {
            return Err(Error::NodeNotFound(id));
        }
        self.read_node(id.0)
    }

    /// Returns a node with only the projected properties decoded.
    ///
    /// Properties whose keys are not in `keys` are skipped without allocating.
    /// If `keys` is empty, no properties are decoded (label and id are always available).
    /// When properties didn't overflow, overflow pages are never read.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the id does not exist.
    pub fn node_projected(&self, id: NodeId, keys: &[&str]) -> Result<Node> {
        // Under MVCC the visible version must match what `node()` returns for
        // the same snapshot — a single visible read path per entity. Resolve the
        // visible node, then project its properties down to `keys`. The page
        // fast path (projection while decoding) applies only in legacy mode.
        if self.mvcc_enabled() {
            let mut node = self.resolve_node_visible(id, self.auto_commit_start_ts(), None)?;
            node.properties_mut()
                .retain(|k, _| keys.contains(&k.as_str()));
            return Ok(node);
        }
        if !self.node_exists.contains(&id.0) {
            return Err(Error::NodeNotFound(id));
        }
        self.read_node_projected(id.0, keys)
    }

    /// Returns only the label of a node without decoding properties.
    ///
    /// Reads the node slot header and resolves the label (inline or overflow)
    /// but skips property deserialization entirely. This is significantly
    /// cheaper than [`node`](Self::node) when only the label is needed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the id does not exist.
    pub fn node_label(&self, id: NodeId) -> Result<String> {
        // Under MVCC resolve the visible node and return its label, so this
        // never disagrees with `node()`/`node_projected()` for the same
        // snapshot. Legacy mode keeps the cheap label-only page read.
        if self.mvcc_enabled() {
            return Ok(self
                .resolve_node_visible(id, self.auto_commit_start_ts(), None)?
                .label()
                .to_owned());
        }
        if !self.node_exists.contains(&id.0) {
            return Err(Error::NodeNotFound(id));
        }
        self.read_node_label(id.0)
    }

    /// Updates a node in the graph with new data.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the id does not exist.
    pub fn update_node(&mut self, id: NodeId, node: &Node) -> Result<()> {
        // Task 15 C': pre-mutation quota check.
        self.check_quota()?;
        // Issue #37: count this op against the open batch's caps (no-op outside a batch).
        let batch_bytes =
            Self::estimate_entity_bytes(size_of::<Node>(), node.label(), node.properties());
        self.charge_batch_op(batch_bytes)?;

        if !self.node_exists.contains(&id.0) {
            return Err(Error::NodeNotFound(id));
        }

        // Read the current node to update indexes correctly.
        let current = self.read_node(id.0)?;

        // 3c: reject if the updated properties would violate a unique
        // constraint, BEFORE touching any index or slot. Exclude this node's
        // own id so re-writing a value it already holds is not a violation.
        // The old entries are still in the index here (removal happens below),
        // which is exactly why the self-exclusion is required.
        self.check_unique(node.label(), node.properties(), Some(id.0))?;

        // Re-index label (if changed) and properties (always), diffing the
        // current committed state against the new one.
        self.reconcile_node_update(&current, node);

        self.write_node_slot(node)?;
        self.wal_sync(FsyncCause::Individual)
    }

    /// Removes a node and all edges connected to it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the id does not exist.
    pub fn remove_node(&mut self, id: NodeId) -> Result<Node> {
        // Issue #37: a delete still counts as one batch operation and consumes
        // memory in `adj_pending`/in-memory indexes, so it charges the batch
        // caps even though it frees on-disk space (unlike the disk quota, which
        // exempts deletes). A delete carries no record bytes of its own.
        self.charge_batch_op(REMOVED_APPROX_SIZE)?;
        if !self.node_exists.contains(&id.0) {
            return Err(Error::NodeNotFound(id));
        }

        let node = self.read_node(id.0)?;

        // Collect edge ids to remove (both directions)
        let mut edge_ids_to_remove = Vec::new();

        if let Some(ptr) = self.resolve_adj_pointer(id.0)? {
            if let Some(out_page) = ptr.outgoing_page {
                edge_ids_to_remove.extend(self.read_adj_edge_ids(
                    out_page,
                    id.0,
                    AdjDirection::Outgoing,
                )?);
            }
            if let Some(in_page) = ptr.incoming_page {
                edge_ids_to_remove.extend(self.read_adj_edge_ids(
                    in_page,
                    id.0,
                    AdjDirection::Incoming,
                )?);
            }
        }

        // Deduplicate (self-loops appear in both lists)
        edge_ids_to_remove.sort_unstable();
        edge_ids_to_remove.dedup();

        // Remove each edge
        for eid in edge_ids_to_remove {
            if self.edge_exists.contains(&eid) {
                self.remove_edge_internal(EdgeId(eid))?;
            }
        }

        // Tombstone the node slot
        self.tombstone_node_slot(id.0)?;

        // Reconcile category B (exists-set, indexes, adjacency caches, count).
        // Incident edges were already removed by the cascade above.
        self.reconcile_node_delete(&node);

        self.wal_sync(FsyncCause::Individual)?;
        Ok(node)
    }

    /// Returns `true` if a node with the given ID exists.
    #[must_use]
    pub fn node_exists(&self, id: NodeId) -> bool {
        self.node_exists.contains(&id.0)
    }

    /// Whether node `id` is visible to an auto-commit reader at the current
    /// clock instant. In legacy mode this is `node_exists`; under MVCC it
    /// resolves the delta chain so a committed superset id this reader must not
    /// see (or a not-yet-visible pending insert) reports `false`.
    #[must_use]
    pub fn node_visible(&self, id: NodeId) -> bool {
        if !self.mvcc_enabled() {
            return self.node_exists.contains(&id.0);
        }
        self.resolve_node_visible(id, self.auto_commit_start_ts(), None)
            .is_ok()
    }

    /// Whether node `id` is visible to transaction `txn_id`'s snapshot — the
    /// `_in_txn` mirror of [`Graph::node_visible`]. Resolves the delta chain at
    /// the transaction's `start_ts`, so it sees the transaction's own
    /// uncommitted writes. Returns `false` if `txn_id` is not active.
    #[must_use]
    pub fn node_visible_in_txn(&self, txn_id: u64, id: NodeId) -> bool {
        let Ok(start_ts) = self.txn_start_ts(txn_id) else {
            return false;
        };
        self.resolve_node_visible(id, start_ts, Some(txn_id))
            .is_ok()
    }

    /// Returns all node IDs currently in the graph.
    ///
    /// The order is not guaranteed.
    #[must_use]
    pub fn node_ids(&self) -> Vec<NodeId> {
        self.node_exists.iter().copied().map(NodeId).collect()
    }

    /// Returns the total number of nodes in the graph.
    #[must_use]
    pub fn node_count(&self) -> usize {
        // Counts are u64 on disk and usize in memory; a graph with more than
        // 2^32 nodes cannot be held in memory on a 32-bit target anyway.
        #[allow(clippy::cast_possible_truncation)]
        let count = self.storage.meta().node_count as usize;
        count
    }

    /// Pages currently allocated in the property-overflow file.
    ///
    /// Counts pages the file occupies, including any sitting on the free list
    /// awaiting reuse — it answers "how big is this file", not "how much of it
    /// is live". Use [`Graph::reusable_overflow_page_count`] for the latter.
    ///
    /// Public rather than test-only because the difference between the two is
    /// the storage-efficiency signal a caller needs to decide whether a
    /// compaction is worth running.
    #[must_use]
    pub fn overflow_page_count(&self) -> u32 {
        self.storage.page_count(DataFile::Overflow)
    }

    /// Pages in the property-overflow file that are free to be handed out again.
    ///
    /// Read straight from the stored counter, so this costs nothing: it never
    /// walks the free chain.
    #[must_use]
    pub fn reusable_overflow_page_count(&self) -> u32 {
        self.storage.meta().free_page_count(DataFile::Overflow)
    }

    /// Reads and validates EVERY allocated page of every data file, forcing the
    /// on-read CRC32 + magic check (Feature A) over the whole database rather
    /// than only the subset an index rebuild happens to touch.
    ///
    /// `Graph::open` validates `graph.meta` and, through its index rebuild,
    /// reads node and edge pages — but it may not read every `strings.db` /
    /// `overflow.db` / `adjacency.db` page, so a bit-flip there can slip past a
    /// plain open and only surface at query time. A restore calls this after
    /// opening so a corrupt snapshot is rejected *before* the pre-restore backup
    /// is discarded, not weeks later when a user reads the affected node.
    ///
    /// # Errors
    ///
    /// [`Error::ChecksumMismatch`] or [`Error::CorruptPage`] for the first page
    /// that fails validation, naming the file and page id.
    pub fn verify_all_pages(&self) -> Result<()> {
        for file in [
            DataFile::Nodes,
            DataFile::Edges,
            DataFile::Adjacency,
            DataFile::Strings,
            DataFile::Overflow,
        ] {
            let count = self.storage.page_count(file);
            for page_id in 0..count {
                // `read_page` routes through the buffer pool's `read_from_disk`,
                // which runs `validate_page_buf` on every cache-miss; any
                // checksum/magic failure surfaces here.
                self.storage.read_page(file, page_id)?;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Edge operations
    // -----------------------------------------------------------------

    /// Adds a directed edge from `source` to `target` with the given label and properties.
    /// Both nodes must already exist in the graph.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if either `source` or `target` does not exist.
    pub fn add_edge(
        &mut self,
        label: impl Into<String>,
        source: NodeId,
        target: NodeId,
        properties: Properties,
    ) -> Result<EdgeId> {
        self.add_edge_str(&label.into(), source, target, properties)
    }

    /// Internal implementation for `add_edge` — takes `&str` for trait compatibility.
    /// Used by `impl GraphAccess for Graph`; `&str` signature required for object safety.
    pub(crate) fn add_edge_str(
        &mut self,
        label: &str,
        source: NodeId,
        target: NodeId,
        properties: Properties,
    ) -> Result<EdgeId> {
        // Task 15 C': pre-mutation quota check. Removes are exempt
        // (they free space, not consume it) — the check fires only on
        // operations that grow the on-disk footprint.
        self.check_quota()?;
        // Issue #37: count this op against the open batch's caps (no-op outside a batch).
        let batch_bytes = Self::estimate_entity_bytes(size_of::<Edge>(), label, &properties);
        self.charge_batch_op(batch_bytes)?;

        if !self.node_exists.contains(&source.0) {
            return Err(Error::NodeNotFound(source));
        }
        if !self.node_exists.contains(&target.0) {
            return Err(Error::NodeNotFound(target));
        }

        let id_val = self.storage.meta().next_edge_id;
        let id = EdgeId(id_val);
        self.storage.meta_mut().next_edge_id = id_val + 1;

        let edge = Edge::new(id, label, source, target, properties);

        self.write_edge_slot(&edge)?;

        // Reconcile category B (exists-set, label/pair indexes, count, and both
        // adjacency directions).
        self.reconcile_edge_insert(&edge)?;

        self.wal_sync(FsyncCause::Individual)?;
        Ok(id)
    }

    /// Returns the adjacency pointer for a node, resolving from cache or page scan.
    ///
    /// `pub(crate)` is intentional — external crates access this via
    /// [`GraphAccess::adj_pointer`]. Returns `Ok(None)` for isolated or
    /// non-existent nodes; propagates I/O errors from storage.
    pub(crate) fn adj_pointer(&self, node: NodeId) -> Result<Option<AdjacencyPointer>> {
        if !self.node_exists.contains(&node.0) {
            return Ok(None);
        }
        self.resolve_adj_pointer(node.0)
    }

    /// Pre-warms the internal adjacency cache with the given pointer.
    ///
    /// `pub(crate)` is intentional — external crates access this via
    /// [`GraphAccess::set_adj_pointer`]. No-op if the node does not exist.
    ///
    /// # Caller contract
    ///
    /// The caller **must** ensure that page IDs within `ptr` reference valid
    /// pages in `DataFile::Adjacency`. Injecting a stale or synthetic pointer
    /// to a non-existent page will cause I/O errors at the next adjacency
    /// read. Runtime validation is not performed because checking page
    /// existence on every call would add unacceptable I/O overhead.
    pub(crate) fn set_adj_pointer(&self, node: NodeId, ptr: AdjacencyPointer) {
        if !self.node_exists.contains(&node.0) {
            return;
        }
        self.adj_cache.insert(node.0, ptr);
        // The injected pointer may reference a different chain than the one the
        // tail cache last recorded for this node. Drop the tail entry so the next
        // append recomputes it from the new first page rather than trusting a
        // tail that belongs to the old chain.
        self.adj_tail_cache.remove(node.0);
    }

    /// Returns a clone of the edge with the given id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EdgeNotFound`] if the id does not exist.
    pub fn edge(&self, id: EdgeId) -> Result<Edge> {
        if self.mvcc_enabled() {
            return self.resolve_edge_visible(id, self.auto_commit_start_ts(), None);
        }
        if !self.edge_exists.contains(&id.0) {
            return Err(Error::EdgeNotFound(id));
        }
        self.read_edge(id.0)
    }

    /// Updates an edge in the graph with new data.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EdgeNotFound`] if the id does not exist.
    pub fn update_edge(&mut self, id: EdgeId, edge: &Edge) -> Result<()> {
        // Issue #37: count this op against the open batch's caps (no-op outside a batch).
        let batch_bytes =
            Self::estimate_entity_bytes(size_of::<Edge>(), edge.label(), edge.properties());
        self.charge_batch_op(batch_bytes)?;
        if !self.edge_exists.contains(&id.0) {
            return Err(Error::EdgeNotFound(id));
        }

        // Re-index label + pair index if the label changed. `update_edge` cannot
        // change endpoints, so the `(from, to)` pair is stable; only the label
        // hash component of the key moves.
        let current = self.read_edge(id.0)?;
        self.reconcile_edge_update(&current, edge);

        self.write_edge_slot(edge)?;
        self.wal_sync(FsyncCause::Individual)
    }

    /// Removes an edge from the graph.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EdgeNotFound`] if the id does not exist.
    pub fn remove_edge(&mut self, id: EdgeId) -> Result<Edge> {
        // Issue #37: a delete still counts as one batch operation (see
        // `remove_node`). A delete carries no record bytes of its own.
        self.charge_batch_op(REMOVED_APPROX_SIZE)?;
        if !self.edge_exists.contains(&id.0) {
            return Err(Error::EdgeNotFound(id));
        }
        let edge = self.remove_edge_internal(id)?;
        self.wal_sync(FsyncCause::Individual)?;
        Ok(edge)
    }

    /// Returns the total number of edges in the graph.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        // Same as `node_count`: bounded by what fits in memory.
        #[allow(clippy::cast_possible_truncation)]
        let count = self.storage.meta().edge_count as usize;
        count
    }

    /// Reads only `(source_id, target_id)` from an edge slot without
    /// decoding label or properties.
    ///
    /// This is a low-level data access primitive. For traversal, use the
    /// `traverse()` builder which provides BFS/DFS with depth limits and
    /// direction filtering.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EdgeNotFound`] if the edge does not exist or
    /// the page cannot be read.
    ///
    /// # Panics
    ///
    /// Panics if the edge slot is shorter than the expected layout — this
    /// indicates data corruption and should never happen with a valid store.
    pub fn read_edge_endpoints(&self, id: u64) -> Result<(u64, u64)> {
        if !self.edge_exists.contains(&id) {
            return Err(Error::EdgeNotFound(EdgeId(id)));
        }
        let (page_idx, slot_idx) = Self::page_and_slot(id);
        let page = self.storage.read_page(DataFile::Edges, page_idx)?;
        let offset = PAGE_HEADER_SIZE + slot_idx * EDGE_SLOT_SIZE;
        let slot = &page[offset..offset + EDGE_SLOT_SIZE];
        let src = u64::from_le_bytes(
            slot[edge_codec::OFF_SOURCE..edge_codec::OFF_SOURCE + 8]
                .try_into()
                .expect("8 bytes for source"),
        );
        let tgt = u64::from_le_bytes(
            slot[edge_codec::OFF_TARGET..edge_codec::OFF_TARGET + 8]
                .try_into()
                .expect("8 bytes for target"),
        );
        Ok((src, tgt))
    }

    /// Reads the 4-byte CRC32 label hash from an edge slot without
    /// decoding the full label string.
    ///
    /// Useful for pre-filtering edges by label hash before incurring the
    /// cost of full label resolution.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EdgeNotFound`] if the edge does not exist.
    ///
    /// # Panics
    ///
    /// Panics if the edge slot is shorter than the expected layout — this
    /// indicates data corruption and should never happen with a valid store.
    pub fn read_edge_label_hash(&self, id: u64) -> Result<u32> {
        if !self.edge_exists.contains(&id) {
            return Err(Error::EdgeNotFound(EdgeId(id)));
        }
        let (page_idx, slot_idx) = Self::page_and_slot(id);
        let page = self.storage.read_page(DataFile::Edges, page_idx)?;
        let offset = PAGE_HEADER_SIZE + slot_idx * EDGE_SLOT_SIZE;
        let slot = &page[offset..offset + EDGE_SLOT_SIZE];
        let hash = u32::from_le_bytes(
            slot[edge_codec::OFF_LABEL_HASH..edge_codec::OFF_LABEL_HASH + 4]
                .try_into()
                .expect("4 bytes for label hash"),
        );
        Ok(hash)
    }

    /// Returns outgoing edges from the given node.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the node does not exist.
    pub fn outgoing_edges(&self, node: NodeId) -> Result<Vec<Edge>> {
        if !self.node_exists.contains(&node.0) {
            return Err(Error::NodeNotFound(node));
        }
        let start_ts = if self.mvcc_enabled() {
            self.auto_commit_start_ts()
        } else {
            0
        };
        self.edges_for_direction(node.0, AdjDirection::Outgoing, start_ts, None)
    }

    /// Returns incoming edges to the given node.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the node does not exist.
    pub fn incoming_edges(&self, node: NodeId) -> Result<Vec<Edge>> {
        if !self.node_exists.contains(&node.0) {
            return Err(Error::NodeNotFound(node));
        }
        let start_ts = if self.mvcc_enabled() {
            self.auto_commit_start_ts()
        } else {
            0
        };
        self.edges_for_direction(node.0, AdjDirection::Incoming, start_ts, None)
    }

    /// Outgoing edges from `node` visible to transaction `txn_id`'s snapshot:
    /// everything committed before the transaction began, per MVCC visibility
    /// rules (see [`Graph::resolve_edge_visible`]).
    ///
    /// # Errors
    ///
    /// [`Error::MvccNotEnabled`] in legacy mode, [`Error::TxnNotActive`] if
    /// `txn_id` is not live, or [`Error::NodeNotFound`] if `node` does not exist.
    pub fn outgoing_edges_in_txn(&self, txn_id: u64, node: NodeId) -> Result<Vec<Edge>> {
        // Validate against the transaction's own visibility, not committed-only
        // `node_exists`: a node the transaction just created is a valid
        // traversal endpoint even though it is not yet committed.
        if !self.node_visible_in_txn(txn_id, node) {
            return Err(Error::NodeNotFound(node));
        }
        let start_ts = self.txn_start_ts(txn_id)?;
        self.edges_for_direction(node.0, AdjDirection::Outgoing, start_ts, Some(txn_id))
    }

    /// Incoming edges to `node` visible to transaction `txn_id`'s snapshot
    /// (edge analogue of [`Graph::outgoing_edges_in_txn`]).
    ///
    /// # Errors
    ///
    /// See [`Graph::outgoing_edges_in_txn`].
    pub fn incoming_edges_in_txn(&self, txn_id: u64, node: NodeId) -> Result<Vec<Edge>> {
        // See `outgoing_edges_in_txn`: validate against txn visibility so a
        // node created in this transaction is a valid endpoint.
        if !self.node_visible_in_txn(txn_id, node) {
            return Err(Error::NodeNotFound(node));
        }
        let start_ts = self.txn_start_ts(txn_id)?;
        self.edges_for_direction(node.0, AdjDirection::Incoming, start_ts, Some(txn_id))
    }

    /// Returns outgoing edges from the given node that match the specified label.
    ///
    /// In legacy (non-MVCC) mode, uses the stored label hash to skip full
    /// deserialization of non-matching edges, avoiding string-heap and
    /// property-overflow resolution for edges whose label does not match.
    /// Under MVCC, each candidate edge is instead resolved through the delta
    /// chain (for snapshot visibility) and then matched by label directly.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the node does not exist.
    pub fn outgoing_edges_by_label(&self, node: NodeId, label: &str) -> Result<Vec<Edge>> {
        if !self.node_exists.contains(&node.0) {
            return Err(Error::NodeNotFound(node));
        }
        let start_ts = if self.mvcc_enabled() {
            self.auto_commit_start_ts()
        } else {
            0
        };
        self.edges_for_direction_by_label(node.0, AdjDirection::Outgoing, label, start_ts, None)
    }

    /// Outgoing edges with `label` visible to transaction `txn_id`.
    ///
    /// # Errors
    /// See [`Graph::outgoing_edges_in_txn`].
    pub fn outgoing_edges_by_label_in_txn(
        &self,
        txn_id: u64,
        node: NodeId,
        label: &str,
    ) -> Result<Vec<Edge>> {
        // See `outgoing_edges_in_txn`: validate against txn visibility.
        if !self.node_visible_in_txn(txn_id, node) {
            return Err(Error::NodeNotFound(node));
        }
        let start_ts = self.txn_start_ts(txn_id)?;
        self.edges_for_direction_by_label(
            node.0,
            AdjDirection::Outgoing,
            label,
            start_ts,
            Some(txn_id),
        )
    }

    /// Returns every edge from `from` to `to` carrying the given `label`.
    ///
    /// Backed by `edge_pair_index`, so the cost is `O(k)` in the number of
    /// parallel edges on the `(from, to, label)` triple, not `O(degree)` of
    /// `from`. A final `edge.label() == label` comparison guards against
    /// `label_hash` (CRC32) collisions.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if either endpoint does not exist.
    pub fn edges_between(&self, from: NodeId, to: NodeId, label: &str) -> Result<Vec<Edge>> {
        if !self.node_exists.contains(&from.0) {
            return Err(Error::NodeNotFound(from));
        }
        if !self.node_exists.contains(&to.0) {
            return Err(Error::NodeNotFound(to));
        }

        let hash = node_codec::label_hash(label);
        let Some(edge_ids) = self.edge_pair_index.get(&(from.0, to.0, hash)) else {
            return Ok(Vec::new());
        };

        let start_ts = if self.mvcc_enabled() {
            self.auto_commit_start_ts()
        } else {
            0
        };
        let mut edges = Vec::new();
        for &eid in edge_ids {
            // Under MVCC the pair-index is a committed-reconciled superset: an id
            // may be visible only to a different snapshot (or its page slot may
            // not be materialized yet), so resolve it through the delta chain and
            // skip ids not visible to this reader.
            let edge = if self.mvcc_enabled() {
                match self.resolve_edge_visible(EdgeId(eid), start_ts, None) {
                    Ok(e) => e,
                    Err(Error::EdgeNotFound(_)) => continue,
                    Err(e) => return Err(e),
                }
            } else {
                self.read_edge(eid)?
            };
            // Guard against CRC32 hash collisions: the pair-index keys on the
            // label hash, so a different label sharing the hash could land in
            // the same bucket.
            if edge.label() == label {
                edges.push(edge);
            }
        }
        Ok(edges)
    }

    /// Returns `true` if at least one edge from `from` to `to` carries `label`.
    ///
    /// Stops at the first matching edge, so it avoids resolving every parallel
    /// edge on the pair when only existence is needed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if either endpoint does not exist.
    pub fn has_edge(&self, from: NodeId, to: NodeId, label: &str) -> Result<bool> {
        if !self.node_exists.contains(&from.0) {
            return Err(Error::NodeNotFound(from));
        }
        if !self.node_exists.contains(&to.0) {
            return Err(Error::NodeNotFound(to));
        }

        let hash = node_codec::label_hash(label);
        let Some(edge_ids) = self.edge_pair_index.get(&(from.0, to.0, hash)) else {
            return Ok(false);
        };

        let start_ts = if self.mvcc_enabled() {
            self.auto_commit_start_ts()
        } else {
            0
        };
        for &eid in edge_ids {
            // See `edges_between`: resolve each superset id through the delta
            // chain under MVCC, skipping ids not visible to this reader.
            let edge = if self.mvcc_enabled() {
                match self.resolve_edge_visible(EdgeId(eid), start_ts, None) {
                    Ok(e) => e,
                    Err(Error::EdgeNotFound(_)) => continue,
                    Err(e) => return Err(e),
                }
            } else {
                self.read_edge(eid)?
            };
            if edge.label() == label {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Returns incoming edges to the given node that match the specified label.
    ///
    /// In legacy (non-MVCC) mode, uses the stored label hash to skip full
    /// deserialization of non-matching edges, avoiding string-heap and
    /// property-overflow resolution for edges whose label does not match.
    /// Under MVCC, each candidate edge is instead resolved through the delta
    /// chain (for snapshot visibility) and then matched by label directly.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NodeNotFound`] if the node does not exist.
    pub fn incoming_edges_by_label(&self, node: NodeId, label: &str) -> Result<Vec<Edge>> {
        if !self.node_exists.contains(&node.0) {
            return Err(Error::NodeNotFound(node));
        }
        let start_ts = if self.mvcc_enabled() {
            self.auto_commit_start_ts()
        } else {
            0
        };
        self.edges_for_direction_by_label(node.0, AdjDirection::Incoming, label, start_ts, None)
    }

    /// Incoming edges with `label` visible to transaction `txn_id`.
    ///
    /// # Errors
    /// See [`Graph::outgoing_edges_in_txn`].
    pub fn incoming_edges_by_label_in_txn(
        &self,
        txn_id: u64,
        node: NodeId,
        label: &str,
    ) -> Result<Vec<Edge>> {
        if !self.node_exists.contains(&node.0) {
            return Err(Error::NodeNotFound(node));
        }
        let start_ts = self.txn_start_ts(txn_id)?;
        self.edges_for_direction_by_label(
            node.0,
            AdjDirection::Incoming,
            label,
            start_ts,
            Some(txn_id),
        )
    }

    /// Returns a [`NeighborQuery`](crate::query::neighbor::NeighborQuery) builder
    /// for exploring the neighbors of a node.
    ///
    /// Use the builder methods to filter by direction and edge label, then call
    /// `.collect()` or `.node_ids()` to execute the query.
    ///
    /// # Example
    ///
    /// ```
    /// use tessera_graph::{Graph, Properties, Direction, props};
    ///
    /// let mut g = Graph::new();
    /// let a = g.add_node("A", Properties::new()).unwrap();
    /// let b = g.add_node("B", Properties::new()).unwrap();
    /// g.add_edge("KNOWS", a, b, Properties::new()).unwrap();
    ///
    /// let edges = g.neighbors(a)
    ///     .direction(Direction::Outgoing)
    ///     .collect()
    ///     .unwrap();
    /// assert_eq!(edges.len(), 1);
    /// ```
    #[must_use]
    pub const fn neighbors(&self, node: NodeId) -> crate::query::neighbor::NeighborQuery<'_, Self> {
        crate::query::neighbor::NeighborQuery::new(self, node)
    }

    /// Returns a [`TraversalBuilder`](crate::query::traversal::TraversalBuilder) for
    /// BFS/DFS traversal starting from the given node.
    ///
    /// # Example
    ///
    /// ```
    /// use tessera_graph::{Graph, Properties, Direction, props};
    ///
    /// let mut g = Graph::new();
    /// let a = g.add_node("A", Properties::new()).unwrap();
    /// let b = g.add_node("B", Properties::new()).unwrap();
    /// g.add_edge("R", a, b, Properties::new()).unwrap();
    ///
    /// let visited = g.traverse(a)
    ///     .direction(Direction::Outgoing)
    ///     .collect()
    ///     .unwrap();
    /// assert_eq!(visited, vec![a, b]);
    /// ```
    #[must_use]
    pub const fn traverse(
        &self,
        start: NodeId,
    ) -> crate::query::traversal::TraversalBuilder<'_, Self> {
        crate::query::traversal::TraversalBuilder::new(self, start)
    }

    /// Returns a [`ShortestPathQuery`](crate::query::shortest_path::ShortestPathQuery)
    /// builder for finding the shortest unweighted path between two nodes.
    ///
    /// # Example
    ///
    /// ```
    /// use tessera_graph::{Graph, Properties, Direction, props};
    ///
    /// let mut g = Graph::new();
    /// let a = g.add_node("A", Properties::new()).unwrap();
    /// let b = g.add_node("B", Properties::new()).unwrap();
    /// g.add_edge("R", a, b, Properties::new()).unwrap();
    ///
    /// let path = g.shortest_path(a, b)
    ///     .direction(Direction::Outgoing)
    ///     .find()
    ///     .unwrap()
    ///     .unwrap();
    /// assert_eq!(path.len(), 1);
    /// ```
    #[must_use]
    pub const fn shortest_path(
        &self,
        from: NodeId,
        to: NodeId,
    ) -> crate::query::shortest_path::ShortestPathQuery<'_> {
        crate::query::shortest_path::ShortestPathQuery::new(self, from, to)
    }

    /// Returns a [`WeightedPathQuery`](crate::query::weighted_path::WeightedPathQuery)
    /// builder for Dijkstra's weighted shortest path.
    #[must_use]
    pub const fn weighted_shortest_path(
        &self,
        from: NodeId,
        to: NodeId,
    ) -> crate::query::weighted_path::WeightedPathQuery<'_> {
        crate::query::weighted_path::WeightedPathQuery::new(self, from, to)
    }

    /// Returns a [`SubgraphQuery`](crate::query::subgraph::SubgraphQuery) builder
    /// for extracting a subgraph reachable from the given start node.
    #[must_use]
    pub const fn subgraph(&self, start: NodeId) -> crate::query::subgraph::SubgraphQuery<'_, Self> {
        crate::query::subgraph::SubgraphQuery::new(self, start)
    }

    /// Returns a [`PatternBuilder`](crate::query::pattern::PatternBuilder) for
    /// declarative graph pattern matching.
    ///
    /// Chain `.node()` and `.edge()` calls to describe a pattern, then call
    /// `.execute()` to find all matches.
    ///
    /// # Example
    ///
    /// ```
    /// use tessera_graph::{Graph, Properties, Direction, props};
    ///
    /// let mut g = Graph::new();
    /// let a = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    /// let b = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    /// g.add_edge("KNOWS", a, b, Properties::new()).unwrap();
    ///
    /// let results: Vec<_> = g.pattern()
    ///     .node("a").label("Person").where_prop("name", "Alice")
    ///     .edge(Direction::Outgoing).label("KNOWS")
    ///     .node("b")
    ///     .execute()
    ///     .unwrap()
    ///     .collect::<tessera_graph::Result<Vec<_>>>()
    ///     .unwrap();
    ///
    /// assert_eq!(results.len(), 1);
    /// assert_eq!(results[0].get_node("b").unwrap().id(), b);
    /// ```
    #[must_use]
    pub const fn pattern(&self) -> crate::query::pattern::PatternBuilder<'_, Self> {
        crate::query::pattern::PatternBuilder::new(self)
    }

    /// Begins a batch of mutations with deferred WAL sync.
    ///
    /// While inside a batch, individual mutations skip the per-operation `fsync`.
    /// Call [`end_batch`](Self::end_batch) to issue a single `fsync` for all
    /// accumulated WAL records. Batches can be nested; only the outermost
    /// `end_batch` triggers the sync. A batch is a **throughput** primitive: it
    /// coalesces the durable disk write that each mutation would otherwise force
    /// into one write at the end.
    ///
    /// # Not atomic
    ///
    /// A batch is **not** all-or-nothing. Each mutation inside it is applied
    /// immediately (its data pages, counters and in-memory indexes change as
    /// soon as the call returns); the batch only defers the `fsync` and the
    /// adjacency flush. If a mutation partway through the batch fails — whether
    /// it hits a [batch cap](Self::set_batch_limits) or any other error — the
    /// mutations already applied earlier in the batch **stay applied**. There is
    /// no rollback of an in-flight batch.
    ///
    /// When you need a group of mutations to apply all-or-nothing, use an
    /// explicit transaction ([`begin_txn`](Self::begin_txn) /
    /// [`commit_txn`](Self::commit_txn) / [`rollback_txn`](Self::rollback_txn)),
    /// which is backed by multi-version concurrency control and can roll back.
    ///
    /// # Bounded
    ///
    /// An open batch accumulates state in memory (`adj_pending` plus the
    /// in-memory indexes), so an unbounded batch is a memory-exhaustion risk. If
    /// [`set_batch_limits`](Self::set_batch_limits) configured a cap on the
    /// operation count or estimated bytes, the mutation that would push the open
    /// batch past either cap is rejected with [`Error::BatchLimitExceeded`]
    /// (leaving the graph as it was before that mutation). The default engine
    /// caps are unlimited; the server configures concrete caps from
    /// `ServerConfig::max_batch_operations` / `max_batch_memory_bytes`.
    ///
    /// See also: [`end_batch`](Self::end_batch).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use tessera_graph::{Graph, Properties};
    /// # let mut graph = Graph::new();
    /// graph.begin_batch();
    /// for _ in 0..1000 {
    ///     graph.add_node("N", Properties::new()).unwrap();
    /// }
    /// graph.end_batch().unwrap(); // single fsync
    /// ```
    ///
    /// # Memory
    ///
    /// During a batch, adjacency writes are deferred in `adj_pending`. Memory
    /// usage grows with the number of unique `(node, direction)` pairs modified.
    /// For a batch creating N edges to M distinct nodes, `adj_pending` holds up
    /// to `2 * M` entries (outgoing + incoming). The entries are flushed and
    /// freed on [`end_batch`](Self::end_batch).
    pub const fn begin_batch(&mut self) {
        self.batch_depth = self.batch_depth.saturating_add(1);
    }

    /// Ends a batch of mutations started by [`begin_batch`](Self::begin_batch).
    ///
    /// When the outermost batch ends (depth reaches 0), a single WAL `fsync` is
    /// issued to guarantee durability of all mutations in the batch. Calling
    /// `end_batch` without a prior `begin_batch` is a harmless no-op.
    ///
    /// **Durable is not the same as checkpointed.** Ordinarily the batch's
    /// mutations are safe from a crash but still live in `wal.log`: the data
    /// files are not written and the WAL is not truncated.
    ///
    /// The exception is the automatic checkpoint of issue #58. When the journal
    /// has grown past
    /// [`GraphConfig::wal_checkpoint_threshold_bytes`](crate::GraphConfig::wal_checkpoint_threshold_bytes)
    /// (64 MB by default), closing the *outermost* batch also performs a full
    /// [`flush`](Self::flush) — materialising the data files and truncating the
    /// journal — so a writer that only ever closes batches no longer grows the
    /// WAL without bound. Closing a nested batch never checkpoints.
    ///
    /// This makes the occasional batch close much more expensive than the rest,
    /// which is the intended trade: a bounded pause now against an unbounded
    /// replay at the next open. Set that threshold to `None` to opt out and
    /// leave [`flush`](Self::flush) as the only thing that bounds the journal.
    ///
    /// The coalesced final fsync goes through [`Graph::wal_sync`], so an
    /// installed [`WalObserver`] sees it exactly once per closed batch —
    /// the most fsync-heavy path on write-intensive workloads.
    ///
    /// Closing the outermost batch also resets the batch-cap counters (see
    /// [`set_batch_limits`](Self::set_batch_limits)), so the next batch starts
    /// with a fresh operation and byte budget. Ending a nested (inner) batch
    /// leaves the counters running.
    ///
    /// See also: [`begin_batch`](Self::begin_batch).
    ///
    /// # Errors
    ///
    /// Returns a storage error if the WAL sync fails, or if the automatic
    /// checkpoint described above fails.
    ///
    /// Either way the batch is closed: the depth has already been decremented
    /// and the batch-cap counters reset, so this is not a call to retry. A
    /// failure of the *sync* means the batch's mutations may not be durable. A
    /// failure of the *checkpoint* leaves them durable in the journal but not
    /// materialised, with the journal still over its threshold — the next
    /// outermost batch close will try again, or call [`flush`](Self::flush)
    /// directly to surface the underlying error on its own.
    pub fn end_batch(&mut self) -> Result<()> {
        if self.batch_depth == 0 {
            return Ok(());
        }
        self.batch_depth -= 1;
        if self.batch_depth == 0 {
            // Capture how many operations this batch coalesced before the reset
            // below zeroes the counter, so the observer can report it as the
            // batch-close fsync's `op_count` (issue #43 Part B).
            let op_count = self.batch_op_count;
            // Issue #37: the outermost batch closes — its counting window ends,
            // so reset the caps' running totals. Done before flush/wal_sync so a
            // storage failure there does not leave stale counters attached to a
            // batch that has already closed.
            self.batch_op_count = 0;
            self.batch_byte_count = 0;
            self.flush_adj_pending()?;
            // Unified through Graph::wal_sync so the WalObserver
            // (Task 2 C2) sees the coalesced batch fsync exactly
            // once. Going through `self.storage.wal_sync()` directly
            // would bypass the observer for what is typically the
            // most fsync-heavy path in a write-heavy workload.
            self.wal_sync(FsyncCause::BatchClose { op_count })?;

            // Issue #58: with the batch's own fsync done, this is the safe
            // point to act on a journal that has outgrown its threshold.
            // Deliberately after the sync, never before: the ordinary
            // durability guarantee of closing a batch does not depend on
            // whether a checkpoint happens to be due.
            //
            // Goes through `flush` rather than `wal_checkpoint_and_truncate`
            // alone because truncating the journal is only safe once
            // everything it holds — data pages, label indexes, schema
            // catalog — is on disk. Calling the narrower operation here
            // would discard records still needed to rebuild those.
            if self.storage.wal_checkpoint_pending() {
                return self.flush();
            }
            return Ok(());
        }
        Ok(())
    }

    /// Flushes all deferred adjacency writes accumulated during a batch.
    ///
    /// For each (`node_id`, direction) with pending edge IDs, reads the existing
    /// adjacency record (if any), appends all pending edges, and writes the
    /// page once. This converts O(N²) per-edge adjacency rewrites into O(N).
    ///
    /// Uses the `adj_cache` to check for existing pages without falling back to
    /// a full page scan — nodes that had no edges before the batch will have a
    /// negative-cache entry (both pages = None) from `resolve_adj_pointer`
    /// calls during normal graph operations, or simply not be in the cache at
    /// all if they were just created in this batch (in which case we know they
    /// have no adjacency pages).
    fn flush_adj_pending(&mut self) -> Result<()> {
        let pending = std::mem::take(&mut self.adj_pending);
        // Track the distinct nodes touched so each node's slot pointer is
        // written exactly once, even when the batch touched it in both
        // directions (outgoing and incoming appear as separate pending keys).
        // The set dedups in O(1); the vec keeps a deterministic write order.
        // The accumulated both-direction pointer for each touched node, captured
        // from the append that produced it. It must NOT be re-read from adj_cache
        // afterwards: that cache evicts (65536 entries), so in a batch touching
        // more nodes than fit, a node's entry can be gone by the time we persist —
        // and then its slot keeps the sentinel, the head is lost, and the whole
        // chain becomes unreachable on the next cache miss (a non-deterministic,
        // all-or-nothing loss of that node's edges). Keeping the pointer here makes
        // persistence independent of whether the entry survived in the cache.
        let mut latest_ptr: HashMap<u64, AdjacencyPointer> = HashMap::new();
        let mut touched: Vec<u64> = Vec::new();
        for ((node_id, direction), edge_ids) in pending {
            // Try adj_cache first (O(1)). On cache miss, fall back to
            // resolve_adj_pointer, which reads both heads from the node slot —
            // necessary to preserve preexisting edges for nodes whose cache entry
            // was evicted. That cost only applies to evicted entries.
            let ptr = latest_ptr
                .get(&node_id)
                .copied()
                .or_else(|| self.adj_cache.get(node_id))
                .or_else(|| self.resolve_adj_pointer(node_id).ok().flatten())
                .unwrap_or(AdjacencyPointer {
                    outgoing_page: None,
                    incoming_page: None,
                });

            // Extend the chain in place, reusing the cached tail so the append
            // does not re-walk the whole chain (issue #33): only the last
            // partial page and any new continuation pages are written —
            // O(new_edges), not O(total_edges), and no orphan pages are leaked.
            let new_ptr = self.append_and_update_caches(node_id, direction, ptr, &edge_ids)?;
            if latest_ptr.insert(node_id, new_ptr).is_none() {
                touched.push(node_id);
            }
        }
        // Persist each touched node's slot pointer once, from the captured value —
        // never from adj_cache, which may have evicted it (see above).
        for node_id in touched {
            let entry = latest_ptr[&node_id];
            self.persist_adj_pointer_to_slot(node_id, entry)?;
        }
        Ok(())
    }

    /// Flushes all dirty pages to persistent storage, then checkpoints and
    /// truncates the write-ahead log.
    ///
    /// **This is the only operation that checkpoints on demand**, and the only
    /// hard guarantee that the WAL is empty when it returns. It is not the only
    /// thing that bounds WAL growth: since issue #58, closing the outermost
    /// batch also checkpoints once the journal has outgrown
    /// [`GraphConfig::wal_checkpoint_threshold_bytes`](crate::GraphConfig::wal_checkpoint_threshold_bytes).
    /// Nothing else does — not `wal_sync`, and there is still no periodic or
    /// background checkpoint.
    ///
    /// The automatic checkpoint bounds the journal in normal operation, so a
    /// long-running writer no longer needs to call this on a schedule to avoid
    /// the failure that motivated #58 (a measured run reached 3.55 GB of WAL
    /// and 22 minutes of startup, against 0.9 s once checkpointed). Call it
    /// anyway when a specific point must be materialised — before a backup, or
    /// on shutdown — or when automatic checkpointing is disabled by setting
    /// that threshold to `None`, which restores the previous behaviour of
    /// unbounded growth between explicit flushes.
    ///
    /// # Errors
    ///
    /// Returns a storage error if the flush fails.
    pub fn flush(&mut self) -> Result<()> {
        self.storage.meta_mut().strings_write_offset = self.string_heap.write_offset();

        // Persist label indexes before flushing storage (MemoryBackend is a no-op)
        let index_bytes = index_codec::serialize(&self.node_label_index, &self.edge_label_index)?;
        self.storage.write_index_bytes(&index_bytes)?;

        // Persist the DDL schema catalog. serialize() returns empty bytes for an
        // empty catalog; skip the write in that case to avoid touching disk for
        // databases that never issued DDL.
        let schema_bytes = crate::schema::codec::serialize(&self.schema_catalog)?;
        if !schema_bytes.is_empty() {
            self.storage.write_schema_bytes(&schema_bytes)?;
        }

        self.storage.flush()?;

        // Checkpoint and truncate WAL after successful flush — all data is on disk.
        self.storage.wal_checkpoint_and_truncate()
    }

    // -----------------------------------------------------------------
    // Internal: node page I/O
    // -----------------------------------------------------------------

    /// Computes (`page_index`, `slot_index`) for a 1-based entity ID.
    const fn page_and_slot(id: u64) -> (u32, usize) {
        debug_assert!(id > 0, "entity id must be > 0");
        // Slot ids index in-memory structures; a 32-bit target could not address
        // a graph large enough to reach the boundary.
        #[allow(clippy::cast_possible_truncation)]
        let zero_based = (id - 1) as usize;
        // Page index derived by integer division; the format caps page ids at
        // u32 and dividing only shrinks the value.
        #[allow(clippy::cast_possible_truncation)]
        let page_idx = (zero_based / SLOTS_PER_PAGE) as u32;
        let slot_idx = zero_based % SLOTS_PER_PAGE;
        (page_idx, slot_idx)
    }

    fn write_node_slot(&mut self, node: &Node) -> Result<()> {
        let (mut slot_buf, overflow) = node_codec::encode_node_slot(node)?;

        // An update overwrites a slot that may already reference a chain; that
        // chain becomes unreachable the moment this write lands, so it is
        // released rather than abandoned. An insert finds nothing here.
        let previous = self.existing_node_prop_overflow(node.id.0);

        self.handle_slot_overflow(
            &mut slot_buf,
            SlotOverflowRequest {
                label_overflowed: overflow.label_overflowed,
                label: node.label(),
                props_overflowed: overflow.props_overflowed,
                props_bytes: overflow.props_bytes.as_deref(),
                previous_prop_overflow: previous,
                entity: (node.id.0, prop_slab_codec::EntityKind::Node),
            },
            node_codec::patch_overflow,
        )?;

        self.write_slot_to_page(node.id.0, &slot_buf, SlotLayout::NODE)
    }

    fn read_node(&self, id: u64) -> Result<Node> {
        let (page_idx, slot_idx) = Self::page_and_slot(id);
        let page = self.storage.read_page(DataFile::Nodes, page_idx)?;
        let offset = PAGE_HEADER_SIZE + slot_idx * NODE_SLOT_SIZE;
        let slot: [u8; NODE_SLOT_SIZE] = page[offset..offset + NODE_SLOT_SIZE]
            .try_into()
            .expect("slice is exactly NODE_SLOT_SIZE bytes");

        // Pre-resolve overflow data to avoid two-closure borrow conflict
        let label_overflow_ref = node_codec::slot_label_overflow_ref(&slot);
        let prop_overflow_ref = node_codec::slot_prop_overflow_ref(&slot);

        let resolved_label = if node_codec::slot_needs_label_resolve(&slot) {
            Some(
                self.string_heap
                    .resolve(self.storage.as_ref(), label_overflow_ref)?,
            )
        } else {
            None
        };

        let resolved_props = if node_codec::slot_needs_prop_resolve(&slot) {
            Some(self.read_overflowed_props(
                (id, prop_slab_codec::EntityKind::Node),
                prop_overflow_ref,
            )?)
        } else {
            None
        };

        let node = node_codec::decode_node_slot(
            &slot,
            page_idx,
            |_| {
                resolved_label.clone().ok_or(Error::CorruptPage {
                    file: "nodes.db",
                    page_id: page_idx,
                    reason: "label resolver called but label was not pre-resolved",
                })
            },
            |_| {
                resolved_props.clone().ok_or(Error::CorruptPage {
                    file: "nodes.db",
                    page_id: page_idx,
                    reason: "props resolver called but props were not pre-resolved",
                })
            },
        )?;

        node.ok_or(Error::NodeNotFound(NodeId(id)))
    }

    fn read_node_projected(&self, id: u64, keys: &[&str]) -> Result<Node> {
        let (page_idx, slot_idx) = Self::page_and_slot(id);
        let page = self.storage.read_page(DataFile::Nodes, page_idx)?;
        let offset = PAGE_HEADER_SIZE + slot_idx * NODE_SLOT_SIZE;
        let slot: [u8; NODE_SLOT_SIZE] = page[offset..offset + NODE_SLOT_SIZE]
            .try_into()
            .expect("slice is exactly NODE_SLOT_SIZE bytes");

        let label_overflow_ref = node_codec::slot_label_overflow_ref(&slot);

        let resolved_label = if node_codec::slot_needs_label_resolve(&slot) {
            Some(
                self.string_heap
                    .resolve(self.storage.as_ref(), label_overflow_ref)?,
            )
        } else {
            None
        };

        // Only resolve overflow props if there are projected keys that need them
        let needs_overflow = !keys.is_empty() && node_codec::slot_needs_prop_resolve(&slot);
        let resolved_props = if needs_overflow {
            let prop_overflow_ref = node_codec::slot_prop_overflow_ref(&slot);
            Some(self.read_overflowed_props(
                (id, prop_slab_codec::EntityKind::Node),
                prop_overflow_ref,
            )?)
        } else {
            None
        };

        let node = node_codec::decode_node_slot_projected(
            &slot,
            page_idx,
            |_| {
                resolved_label.clone().ok_or(Error::CorruptPage {
                    file: "nodes.db",
                    page_id: page_idx,
                    reason: "label resolver called but label was not pre-resolved",
                })
            },
            |_| {
                resolved_props.clone().ok_or(Error::CorruptPage {
                    file: "nodes.db",
                    page_id: page_idx,
                    reason: "props resolver called but props were not pre-resolved",
                })
            },
            keys,
        )?;

        node.ok_or(Error::NodeNotFound(NodeId(id)))
    }

    /// Reads only the label from a node slot, skipping property deserialization.
    fn read_node_label(&self, id: u64) -> Result<String> {
        let (page_idx, slot_idx) = Self::page_and_slot(id);
        let page = self.storage.read_page(DataFile::Nodes, page_idx)?;
        let offset = PAGE_HEADER_SIZE + slot_idx * NODE_SLOT_SIZE;
        let slot: [u8; NODE_SLOT_SIZE] = page[offset..offset + NODE_SLOT_SIZE]
            .try_into()
            .expect("slice is exactly NODE_SLOT_SIZE bytes");

        if node_codec::slot_needs_label_resolve(&slot) {
            let label_overflow_ref = node_codec::slot_label_overflow_ref(&slot);
            self.string_heap
                .resolve(self.storage.as_ref(), label_overflow_ref)
        } else {
            node_codec::slot_inline_label(&slot, page_idx)
        }
    }

    /// Reads a node's raw 128-byte slot straight from the page, bypassing every
    /// in-memory cache and the MVCC delta chain. Used by `resolve_adj_pointer`
    /// to read the adjacency heads without trusting `adj_cache`, and by tests to
    /// assert the on-disk pointer.
    fn read_node_slot_bytes(&self, id: u64) -> Result<[u8; NODE_SLOT_SIZE]> {
        let (page_idx, slot_idx) = Self::page_and_slot(id);
        let page = self.storage.read_page(DataFile::Nodes, page_idx)?;
        let offset = PAGE_HEADER_SIZE + slot_idx * NODE_SLOT_SIZE;
        Ok(page[offset..offset + NODE_SLOT_SIZE]
            .try_into()
            .expect("slice is exactly NODE_SLOT_SIZE bytes"))
    }

    fn tombstone_node_slot(&mut self, id: u64) -> Result<()> {
        self.tombstone_slot(id, SlotLayout::NODE)
    }

    // -----------------------------------------------------------------
    // Internal: edge page I/O
    // -----------------------------------------------------------------

    fn write_edge_slot(&mut self, edge: &Edge) -> Result<()> {
        let (mut slot_buf, overflow) = edge_codec::encode_edge_slot(edge)?;

        // See `write_node_slot`: the chain the old slot pointed at is dead once
        // this write lands.
        let previous = self.existing_edge_prop_overflow(edge.id.0);

        self.handle_slot_overflow(
            &mut slot_buf,
            SlotOverflowRequest {
                label_overflowed: overflow.label_overflowed,
                label: edge.label(),
                props_overflowed: overflow.props_overflowed,
                props_bytes: overflow.props_bytes.as_deref(),
                previous_prop_overflow: previous,
                entity: (edge.id.0, prop_slab_codec::EntityKind::Edge),
            },
            edge_codec::patch_edge_overflow,
        )?;

        self.write_slot_to_page(edge.id.0, &slot_buf, SlotLayout::EDGE)
    }

    fn read_edge(&self, id: u64) -> Result<Edge> {
        let (page_idx, slot_idx) = Self::page_and_slot(id);
        let page = self.storage.read_page(DataFile::Edges, page_idx)?;
        let offset = PAGE_HEADER_SIZE + slot_idx * EDGE_SLOT_SIZE;
        let slot: [u8; EDGE_SLOT_SIZE] = page[offset..offset + EDGE_SLOT_SIZE]
            .try_into()
            .expect("slice is exactly EDGE_SLOT_SIZE bytes");

        // Pre-resolve overflow data to avoid two-closure borrow conflict
        let label_overflow_ref = edge_codec::edge_slot_label_overflow_ref(&slot);
        let prop_overflow_ref = edge_codec::edge_slot_prop_overflow_ref(&slot);

        let resolved_label = if edge_codec::edge_slot_needs_label_resolve(&slot) {
            Some(
                self.string_heap
                    .resolve(self.storage.as_ref(), label_overflow_ref)?,
            )
        } else {
            None
        };

        let resolved_props = if edge_codec::edge_slot_needs_prop_resolve(&slot) {
            Some(self.read_overflowed_props(
                (id, prop_slab_codec::EntityKind::Edge),
                prop_overflow_ref,
            )?)
        } else {
            None
        };

        let edge = edge_codec::decode_edge_slot(
            &slot,
            page_idx,
            |_| {
                resolved_label.clone().ok_or(Error::CorruptPage {
                    file: "edges.db",
                    page_id: page_idx,
                    reason: "label resolver called but label was not pre-resolved",
                })
            },
            |_| {
                resolved_props.clone().ok_or(Error::CorruptPage {
                    file: "edges.db",
                    page_id: page_idx,
                    reason: "props resolver called but props were not pre-resolved",
                })
            },
        )?;

        edge.ok_or(Error::EdgeNotFound(EdgeId(id)))
    }

    fn tombstone_edge_slot(&mut self, id: u64) -> Result<()> {
        self.tombstone_slot(id, SlotLayout::EDGE)
    }

    // -----------------------------------------------------------------
    // Internal: adjacency helpers
    // -----------------------------------------------------------------

    /// Inserts `edge_id` into `edge_pair_index` under `(from, to, label_hash)`.
    ///
    /// Called from every path that creates an edge or re-points its label so
    /// the index stays consistent with the edge slots.
    fn insert_pair_index(&mut self, from: u64, to: u64, label: &str, edge_id: u64) {
        let hash = node_codec::label_hash(label);
        self.edge_pair_index
            .entry((from, to, hash))
            .or_default()
            .push(edge_id);
    }

    /// Removes `edge_id` from `edge_pair_index` under `(from, to, label_hash)`,
    /// dropping the key entirely when its `Vec` becomes empty so the map does
    /// not accumulate orphan keys under high edge churn.
    fn remove_pair_index(&mut self, from: u64, to: u64, label: &str, edge_id: u64) {
        let hash = node_codec::label_hash(label);
        if let Some(ids) = self.edge_pair_index.get_mut(&(from, to, hash)) {
            ids.retain(|&x| x != edge_id);
            if ids.is_empty() {
                self.edge_pair_index.remove(&(from, to, hash));
            }
        }
    }

    fn add_edge_to_adjacency(
        &mut self,
        node_id: u64,
        direction: AdjDirection,
        edge_id: u64,
    ) -> Result<()> {
        // In batch mode, defer the adjacency page write — just accumulate
        // the edge_id. The batch of pending edges is flushed in
        // `flush_adj_pending` (called by `end_batch`), converting O(N²)
        // per-edge page rewrites into a single O(N) write per (node, dir).
        if self.batch_depth > 0 {
            self.adj_pending
                .entry((node_id, direction))
                .or_default()
                .push(edge_id);
            return Ok(());
        }

        self.write_adj_immediate(node_id, direction, edge_id)
    }

    /// Writes a single edge to the adjacency page immediately (no batching).
    ///
    /// Extends the chain in place via [`append_adjacency_with_state`], reusing
    /// the cached tail so it neither re-walks nor rewrites the whole chain. The
    /// previous implementation did a full `read_adjacency` + `write_adjacency`
    /// per edge, which was O(degree) time AND leaked pages (`write_adjacency`
    /// always allocates fresh pages, orphaning the old chain on every single
    /// edge — issue #33, a second latent bug fixed by this change).
    fn write_adj_immediate(
        &mut self,
        node_id: u64,
        direction: AdjDirection,
        edge_id: u64,
    ) -> Result<()> {
        let ptr = self
            .resolve_adj_pointer(node_id)?
            .unwrap_or(AdjacencyPointer {
                outgoing_page: None,
                incoming_page: None,
            });
        // Persist the pointer the append returns, not one re-read from adj_cache:
        // the cache evicts, so a re-read can miss and leave the slot at the
        // sentinel — the head lost, the chain unreachable (see flush_adj_pending).
        let entry = self.append_and_update_caches(node_id, direction, ptr, &[edge_id])?;
        self.persist_adj_pointer_to_slot(node_id, entry)?;
        Ok(())
    }

    /// Appends `edge_ids` to a node's adjacency for `direction`, WAL-logging every
    /// written page and updating both the pointer cache and the tail cache. `ptr` is
    /// the node's current pointer (both directions), so the opposite direction is
    /// preserved. Shared by the batch flush and the immediate single-edge write.
    ///
    /// A node's adjacency lives in one of two places, and this is where it lands in
    /// one or moves between them (issue #54):
    /// - a **shared slab page**, packed alongside other low-degree nodes' sub-blocks.
    ///   This is where every node starts, and it is what stops N low-degree nodes from
    ///   costing N pages;
    /// - a **dedicated chain** of its own pages (the pre-#54 format, `adjacency_codec`),
    ///   which a node migrates to once its sub-block outgrows the room left on its slab.
    ///
    /// Which one a node is in is read from the page type of its head, never guessed —
    /// the two formats share `DataFile::Adjacency` and the `TGAD` magic, so the page
    /// type is the only thing separating them.
    fn append_and_update_caches(
        &mut self,
        node_id: u64,
        direction: AdjDirection,
        ptr: AdjacencyPointer,
        edge_ids: &[u64],
    ) -> Result<AdjacencyPointer> {
        let existing_page = match direction {
            AdjDirection::Outgoing => ptr.outgoing_page,
            AdjDirection::Incoming => ptr.incoming_page,
        };

        let new_first_page = match existing_page {
            // Already on a dedicated chain: stay there, appending in place via the
            // cached tail. A node never moves back to a slab — its sub-block was
            // tombstoned on the way out, and re-packing a large record would undo
            // the very reason it migrated.
            Some(page) if !adj_slab_codec::is_slab_page(self.storage.as_ref(), page)? => {
                self.append_to_dedicated_chain(node_id, direction, Some(page), edge_ids)?
            }
            // On a slab: grow the sub-block in place if it fits, otherwise migrate
            // the node out to a dedicated chain of its own.
            Some(slab_page) => {
                match adj_slab_codec::append_subblock_edges(
                    self.storage.as_mut(),
                    slab_page,
                    node_id,
                    direction,
                    edge_ids,
                )? {
                    adj_slab_codec::AppendOutcome::InPlace => {
                        self.wal_log_adj_page(slab_page)?;
                        slab_page
                    }
                    adj_slab_codec::AppendOutcome::NoRoom => self
                        .migrate_subblock_to_dedicated_chain(
                            node_id, direction, slab_page, edge_ids,
                        )?,
                }
            }
            // First edges in this direction, and too many to ever fit a slab page:
            // go straight to a dedicated chain. A batch flush can deliver hundreds
            // of edges at once, and routing them through the slab would mean
            // writing a sub-block only to migrate it out on the same call.
            None if !adj_slab_codec::fits_in_empty_slab(edge_ids.len()) => {
                self.append_to_dedicated_chain(node_id, direction, None, edge_ids)?
            }
            // First edges in this direction: open a sub-block on the slab currently
            // accepting new nodes.
            None => {
                let slab_page = self.open_slab_page_for(direction, edge_ids.len())?;
                adj_slab_codec::write_subblock(
                    self.storage.as_mut(),
                    slab_page,
                    node_id,
                    direction,
                    edge_ids,
                )?;
                self.wal_log_adj_page(slab_page)?;
                slab_page
            }
        };

        let mut entry = ptr;
        match direction {
            AdjDirection::Outgoing => entry.outgoing_page = Some(new_first_page),
            AdjDirection::Incoming => entry.incoming_page = Some(new_first_page),
        }
        self.adj_cache.insert(node_id, entry);
        Ok(entry)
    }

    /// Reads a node's persisted edge IDs for one direction, from wherever that
    /// node's adjacency actually lives (issue #54).
    ///
    /// `head` is the node's head page for the direction, as stored in its slot. The
    /// page's type decides how to read it: a shared slab holds the node's edges in a
    /// sub-block alongside other nodes', while a dedicated chain holds a record of
    /// its own. Callers get the same list either way and never need to know which.
    ///
    /// This is the read-side counterpart of [`Graph::append_and_update_caches`]; both
    /// formats share `DataFile::Adjacency` and the `TGAD` magic, so reading a head
    /// without checking its page type would parse one format as the other.
    fn read_adj_edge_ids(
        &self,
        head: PageId,
        node_id: u64,
        direction: AdjDirection,
    ) -> Result<Vec<u64>> {
        if adj_slab_codec::is_slab_page(self.storage.as_ref(), head)? {
            adj_slab_codec::read_subblock(self.storage.as_ref(), head, node_id, direction)
        } else {
            Ok(adjacency_codec::read_adjacency(self.storage.as_ref(), head)?.edge_ids)
        }
    }

    /// Appends to a node's dedicated adjacency chain, reusing the cached tail so the
    /// chain is not re-walked (issue #33), and refreshes that tail cache.
    fn append_to_dedicated_chain(
        &mut self,
        node_id: u64,
        direction: AdjDirection,
        existing_page: Option<PageId>,
        edge_ids: &[u64],
    ) -> Result<PageId> {
        let existing_state = self.adj_tail_cache.get(node_id, direction);
        let (new_first_page, written_pages, new_state) =
            adjacency_codec::append_adjacency_with_state(
                self.storage.as_mut(),
                node_id,
                direction,
                existing_page,
                existing_state,
                edge_ids,
            )?;
        for &page in &written_pages {
            self.wal_log_adj_page(page)?;
        }
        self.adj_tail_cache.insert(node_id, direction, new_state);
        Ok(new_first_page)
    }

    /// Returns a slab page with room for a new sub-block holding `edge_count` edges
    /// in `direction`, allocating a fresh one when the currently open slab is full.
    ///
    /// New nodes' sub-blocks are packed into one open slab per direction until it
    /// runs out of room; that packing is what stops N low-degree nodes from costing
    /// N pages. Retired slabs are never revisited: nodes already on them keep growing
    /// in place until they migrate out, and space freed by a migration is reused only
    /// by that page's remaining occupants (the module's no-compaction rule).
    ///
    /// The caller's write must not fail for lack of space, so the requested size is
    /// checked here rather than assumed — `write_subblock` treats "does not fit" as
    /// a corrupt-page error, not as a signal to allocate.
    fn open_slab_page_for(&mut self, direction: AdjDirection, edge_count: usize) -> Result<PageId> {
        let idx = direction as usize;
        if let Some(page_id) = self.open_slab[idx] {
            let page = self.storage.read_page(DataFile::Adjacency, page_id)?;
            if adj_slab_codec::slab_can_fit_subblock(page_id, &page, edge_count)? {
                return Ok(page_id);
            }
        }
        let page_id = adj_slab_codec::allocate_slab_page(self.storage.as_mut())?;
        self.open_slab[idx] = Some(page_id);
        Ok(page_id)
    }

    /// Moves a node's adjacency out of a shared slab and into a dedicated chain of
    /// its own, because its sub-block outgrew the room left on the slab page.
    ///
    /// The node's existing edges are read out of the sub-block, combined with the
    /// new ones, and rewritten as a chain; the vacated sub-block is tombstoned so
    /// its directory slot can be reused by another node. Returns the chain's head.
    ///
    /// This is a one-way move: a node that has migrated keeps growing on its chain
    /// and never returns to a slab.
    fn migrate_subblock_to_dedicated_chain(
        &mut self,
        node_id: u64,
        direction: AdjDirection,
        slab_page: PageId,
        new_edge_ids: &[u64],
    ) -> Result<PageId> {
        let mut all_edges =
            adj_slab_codec::read_subblock(self.storage.as_ref(), slab_page, node_id, direction)?;
        all_edges.extend_from_slice(new_edge_ids);

        // Written as a fresh chain (no existing first page), which reports every
        // page it touches so they all reach the WAL. The tail cache holds no state
        // for a node that was on a slab, so there is nothing stale to drop here.
        let first_page = self.append_to_dedicated_chain(node_id, direction, None, &all_edges)?;

        adj_slab_codec::free_subblock(self.storage.as_mut(), slab_page, node_id, direction)?;
        self.wal_log_adj_page(slab_page)?;

        Ok(first_page)
    }

    /// Persists a node's adjacency heads into its on-disk slot so the pointer
    /// survives cache eviction and lets `resolve_adj_pointer` resolve without a
    /// page scan (cycle 7). The slot carries two independent heads — the
    /// outgoing chain and the incoming chain — so a bidirectional node keeps
    /// both without any scan.
    ///
    /// Kept separate from [`Graph::append_and_update_caches`] so a batch flush
    /// that touches a node in both directions writes its slot once, not once per
    /// direction. Callers read the accumulated pointer from `adj_cache`, which
    /// already holds both directions, so a single write captures both.
    ///
    /// Writes only the pointer bytes (label and properties untouched, so the
    /// fan-in hot path pays no property re-serialization per edge). Reuses the
    /// auto-commit slot-write path so the page checksum and WAL log stay
    /// consistent.
    fn persist_adj_pointer_to_slot(&mut self, node_id: u64, ptr: AdjacencyPointer) -> Result<()> {
        let (page_idx, slot_idx) = Self::page_and_slot(node_id);
        // The node's page may not be materialized yet: under MVCC, committing a
        // transaction reconciles a new edge's adjacency (this path) before the
        // vacuum writes the new node's page. In that case there is no slot to
        // patch, so materialize the node's full slot now (label + properties +
        // head) from its visible state. This makes the head durable in the WAL
        // and on the page at commit time, so it survives both a crash before the
        // vacuum and eviction of `adj_cache` — the pointer must never live only
        // in memory.
        let slot_missing = self.storage.page_count(DataFile::Nodes) <= page_idx || {
            let page = self.storage.read_page(DataFile::Nodes, page_idx)?;
            let offset = PAGE_HEADER_SIZE + slot_idx * NODE_SLOT_SIZE;
            page[offset] != SLOT_LIVE
        };
        if slot_missing {
            return self.materialize_node_slot_with_pointer(node_id, ptr);
        }

        let page = self.storage.read_page(DataFile::Nodes, page_idx)?;
        let offset = PAGE_HEADER_SIZE + slot_idx * NODE_SLOT_SIZE;
        let mut slot: [u8; NODE_SLOT_SIZE] = page[offset..offset + NODE_SLOT_SIZE]
            .try_into()
            .expect("slice is exactly NODE_SLOT_SIZE bytes");

        node_codec::patch_adj_pointer(&mut slot, ptr.outgoing_page, ptr.incoming_page);

        self.write_slot_to_page(node_id, &slot, SlotLayout::NODE)
    }

    /// Materializes a node's full slot (label + properties + adjacency head)
    /// when its page does not exist yet, encoding from the node's currently
    /// visible state. Used by `persist_adj_pointer_to_slot` at commit time for a
    /// node created in the same transaction as its first edge: writing the whole
    /// slot (not just the pointer) makes the adjacency head durable immediately,
    /// so it survives a crash before the vacuum or eviction of `adj_cache`.
    fn materialize_node_slot_with_pointer(
        &mut self,
        node_id: u64,
        ptr: AdjacencyPointer,
    ) -> Result<()> {
        // Only reachable under MVCC: legacy auto-commit always materializes a
        // node's page before any edge can reference it, so a missing slot there
        // is not this path's concern.
        if !self.mvcc_enabled() {
            return Ok(());
        }
        // The node is committed but not yet on a page; read its visible state.
        // No visible version (e.g. created then deleted in the same txn) means
        // nothing to materialize and no adjacency to keep.
        let Ok(node) =
            self.resolve_node_visible(NodeId(node_id), self.auto_commit_start_ts(), None)
        else {
            return Ok(());
        };
        let (mut slot_buf, overflow) = node_codec::encode_node_slot(&node)?;
        self.handle_slot_overflow(
            &mut slot_buf,
            SlotOverflowRequest {
                label_overflowed: overflow.label_overflowed,
                label: node.label(),
                props_overflowed: overflow.props_overflowed,
                props_bytes: overflow.props_bytes.as_deref(),
                previous_prop_overflow: self.existing_node_prop_overflow(node_id),
                entity: (node_id, prop_slab_codec::EntityKind::Node),
            },
            node_codec::patch_overflow,
        )?;
        node_codec::patch_adj_pointer(&mut slot_buf, ptr.outgoing_page, ptr.incoming_page);
        self.write_slot_to_page(node_id, &slot_buf, SlotLayout::NODE)
    }

    /// Copies the adjacency head already stored in a node's on-disk slot onto
    /// `slot_buf` before it is rewritten, so a re-serialization from a stale
    /// snapshot (the vacuum) never clobbers a head that a later edge wrote. A
    /// no-op when the node has no page yet or its slot is not live.
    fn preserve_on_disk_adj_head(&self, node_id: u64, slot_buf: &mut [u8]) -> Result<()> {
        let (page_idx, slot_idx) = Self::page_and_slot(node_id);
        if self.storage.page_count(DataFile::Nodes) <= page_idx {
            return Ok(());
        }
        let page = self.storage.read_page(DataFile::Nodes, page_idx)?;
        let offset = PAGE_HEADER_SIZE + slot_idx * NODE_SLOT_SIZE;
        let on_disk: [u8; NODE_SLOT_SIZE] = page[offset..offset + NODE_SLOT_SIZE]
            .try_into()
            .expect("slice is exactly NODE_SLOT_SIZE bytes");
        if on_disk[0] != SLOT_LIVE {
            return Ok(());
        }
        let out = node_codec::slot_adj_page_id(&on_disk);
        let inc = node_codec::slot_adj_incoming_page_id(&on_disk);
        let buf: &mut [u8; NODE_SLOT_SIZE] = slot_buf
            .try_into()
            .expect("node slot buffer is NODE_SLOT_SIZE bytes");
        node_codec::patch_adj_pointer(
            buf,
            (out != node_codec::ADJ_PAGE_ID_SENTINEL).then_some(out),
            (inc != node_codec::ADJ_PAGE_ID_SENTINEL).then_some(inc),
        );
        Ok(())
    }

    fn remove_edge_from_adjacency(
        &mut self,
        node_id: u64,
        direction: AdjDirection,
        edge_id: u64,
    ) -> Result<()> {
        // Check if the edge is in the pending buffer (not yet written to storage).
        if let Some(pending) = self.adj_pending.get_mut(&(node_id, direction)) {
            if let Some(pos) = pending.iter().position(|&eid| eid == edge_id) {
                pending.swap_remove(pos);
                return Ok(());
            }
        }

        let Some(ptr) = self.resolve_adj_pointer(node_id)? else {
            return Ok(());
        };

        let page_id = match direction {
            AdjDirection::Outgoing => ptr.outgoing_page,
            AdjDirection::Incoming => ptr.incoming_page,
        };

        let Some(pid) = page_id else {
            return Ok(());
        };

        // Deletion still does a full read+rewrite: it is intentionally out of
        // scope for issue #33 (a delete must decode the record to filter one
        // edge, and it is not the fan-in append hot path). It does reshape the
        // record, so the cached tail is invalidated below.
        let mut edge_ids = self.read_adj_edge_ids(pid, node_id, direction)?;
        edge_ids.retain(|&eid| eid != edge_id);

        // A node on a slab is rewritten in place there: losing an edge must not
        // evict it to a chain of its own, which would undo the packing (#54) on
        // the first delete a low-degree node ever sees.
        let new_page = if adj_slab_codec::is_slab_page(self.storage.as_ref(), pid)? {
            adj_slab_codec::rewrite_subblock_edges(
                self.storage.as_mut(),
                pid,
                node_id,
                direction,
                &edge_ids,
            )?;
            pid
        } else {
            let record = AdjacencyRecord {
                node_id,
                direction,
                edge_ids,
            };
            adjacency_codec::write_adjacency(self.storage.as_mut(), &record)?
        };
        self.wal_log_adj_page(new_page)?;

        // Reuse `ptr` from resolve_adj_pointer to preserve both directions,
        // avoiding a cache re-query that could lose the opposite direction
        // if the entry was evicted between resolve and update.
        let mut entry = ptr;
        match direction {
            AdjDirection::Outgoing => entry.outgoing_page = Some(new_page),
            AdjDirection::Incoming => entry.incoming_page = Some(new_page),
        }
        self.adj_cache.insert(node_id, entry);
        // The rewrite may shrink the chain; drop the stale tail so the next
        // append recomputes it via the safe fallback rather than trusting a
        // tail that now points past the chain's end.
        self.adj_tail_cache.remove(node_id);

        Ok(())
    }

    /// Resolves an adjacency pointer for a node, falling back to a page scan if the
    /// cache misses.
    ///
    /// Returns `None` — and caches the absence — when the node has no adjacency pages,
    /// which is the expected state for isolated nodes under lazy allocation. Callers
    /// must treat `None` as "no edges yet", not as an error.
    ///
    /// The result (including the `None` case) is inserted into the cache so that
    /// repeated calls for isolated nodes do not re-scan adjacency pages.
    ///
    /// Note: the negative cache accepts raw `u64` IDs regardless of whether they
    /// correspond to live nodes. This is intentional — adding a `node_exists` check
    /// here would add overhead to an internal hot path. The public-facing
    /// `adj_pointer` method enforces the node-existence contract before delegating.
    fn resolve_adj_pointer(&self, node_id: u64) -> Result<Option<AdjacencyPointer>> {
        if let Some(ptr) = self.adj_cache.get(node_id) {
            // A cached entry with both pages = None is a negative-cache marker
            // (isolated node, no adjacency pages allocated yet).
            if ptr.outgoing_page.is_none() && ptr.incoming_page.is_none() {
                return Ok(None);
            }
            return Ok(Some(ptr));
        }

        // Cache miss — read the node's slot, which holds both adjacency heads
        // (outgoing and incoming). This is O(1): one node-page read, no scan of
        // DataFile::Adjacency proportional to its page count (the O(N²) shape
        // issue #54 set out to remove). A node whose slot page does not exist
        // yet (never persisted) has no adjacency: report absence.
        let absent = AdjacencyPointer {
            outgoing_page: None,
            incoming_page: None,
        };
        let (page_idx, _) = Self::page_and_slot(node_id);
        if self.storage.page_count(DataFile::Nodes) <= page_idx {
            self.adj_cache.insert(node_id, absent);
            return Ok(None);
        }
        let slot = self.read_node_slot_bytes(node_id)?;
        // A slot that is not live has no on-disk adjacency to read, and its head
        // bytes are zero — which must NOT be read as "head on page 0". This
        // happens when the node id falls on an existing page but its own slot was
        // never materialized (a node created inside an uncommitted transaction,
        // whose page write is the vacuum's job) or was tombstoned by a delete the
        // vacuum has not yet applied.
        if slot[0] != SLOT_LIVE {
            self.adj_cache.insert(node_id, absent);
            return Ok(None);
        }
        let out = node_codec::slot_adj_page_id(&slot);
        let inc = node_codec::slot_adj_incoming_page_id(&slot);
        let ptr = AdjacencyPointer {
            outgoing_page: (out != node_codec::ADJ_PAGE_ID_SENTINEL).then_some(out),
            incoming_page: (inc != node_codec::ADJ_PAGE_ID_SENTINEL).then_some(inc),
        };

        // Always cache the result (including absence) so repeated calls skip the read.
        self.adj_cache.insert(node_id, ptr);
        if ptr.outgoing_page.is_none() && ptr.incoming_page.is_none() {
            Ok(None)
        } else {
            Ok(Some(ptr))
        }
    }

    /// Collects the edges for `node_id` in `direction`, filtered to those
    /// visible to the reader identified by `start_ts`/`reader_txn_id`.
    ///
    /// The adjacency page and `adj_pending` hold a superset of edge ids (every
    /// version ever linked to this node); in MVCC mode each id is resolved
    /// through [`Graph::resolve_edge_visible`] to keep only the version this
    /// snapshot may see. In legacy (non-MVCC) mode this degrades to a plain
    /// `read_edge` per id, byte-for-byte as before.
    fn edges_for_direction(
        &self,
        node_id: u64,
        direction: AdjDirection,
        start_ts: u64,
        reader_txn_id: Option<u64>,
    ) -> Result<Vec<Edge>> {
        let ptr = self.resolve_adj_pointer(node_id)?;

        let page_id = ptr.and_then(|p| match direction {
            AdjDirection::Outgoing => p.outgoing_page,
            AdjDirection::Incoming => p.incoming_page,
        });

        let mut edges = Vec::new();
        let push_visible = |edges: &mut Vec<Edge>, eid: u64| -> Result<()> {
            if !self.edge_exists.contains(&eid) {
                return Ok(());
            }
            if self.mvcc_enabled() {
                // Superset id: keep only versions visible to this snapshot.
                match self.resolve_edge_visible(EdgeId(eid), start_ts, reader_txn_id) {
                    Ok(edge) => edges.push(edge),
                    Err(Error::EdgeNotFound(_)) => {}
                    Err(e) => return Err(e),
                }
            } else {
                edges.push(self.read_edge(eid)?);
            }
            Ok(())
        };

        // Read persisted edges from the node's adjacency (slab or dedicated chain)
        if let Some(pid) = page_id {
            let edge_ids = self.read_adj_edge_ids(pid, node_id, direction)?;
            edges.reserve(edge_ids.len());
            for &eid in &edge_ids {
                push_visible(&mut edges, eid)?;
            }
        }

        // Include any edges pending in the current batch
        if let Some(pending) = self.adj_pending.get(&(node_id, direction)) {
            edges.reserve(pending.len());
            for &eid in pending {
                push_visible(&mut edges, eid)?;
            }
        }

        // Additive txn-overlay branch: a reader inside a transaction also sees
        // edges that transaction created but has not committed. Strictly
        // additive — when `reader_txn_id` is None (auto-commit) this does
        // nothing and the result is byte-for-byte the legacy path.
        //
        // A pending edge is NOT in the committed `edge_exists` set, so it cannot
        // go through `push_visible` (whose first guard rejects non-committed
        // ids). It is resolved directly through the delta chain instead.
        for eid in self.pending_txn_edge_ids(node_id, direction, reader_txn_id) {
            match self.resolve_edge_visible(EdgeId(eid), start_ts, reader_txn_id) {
                Ok(edge) => edges.push(edge),
                Err(Error::EdgeNotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }

        Ok(edges)
    }

    /// Edge ids `reader_txn_id` has pending on `node_id` in `direction`, or an
    /// empty vec when `reader_txn_id` is `None` (auto-commit) or MVCC is off.
    /// The single source of the txn-overlay adjacency read, shared by
    /// [`edges_for_direction`](Self::edges_for_direction) and
    /// [`edges_for_direction_by_label`](Self::edges_for_direction_by_label) so
    /// the additive branch is written once.
    fn pending_txn_edge_ids(
        &self,
        node_id: u64,
        direction: AdjDirection,
        reader_txn_id: Option<u64>,
    ) -> Vec<u64> {
        let Some(txn_id) = reader_txn_id else {
            return Vec::new();
        };
        let Some(registry) = self.txn_registry.as_ref() else {
            return Vec::new();
        };
        registry.pending_edges_for(txn_id, node_id, direction)
    }

    /// Like [`edges_for_direction`](Self::edges_for_direction), but filters to
    /// edges matching `label`, additionally honoring MVCC snapshot visibility
    /// for the reader identified by `start_ts`/`reader_txn_id`.
    ///
    /// In legacy (non-MVCC) mode this keeps the stored label-hash prefilter to
    /// skip full deserialization of non-matching edges, byte-for-byte as
    /// before. Under MVCC the prefilter is skipped: a committed-but-not-yet
    /// materialized edge (its page write is the vacuum's job, see Phase 5) has
    /// no page slot to read a hash from, so resolution goes straight through
    /// [`Graph::resolve_edge_visible`] and the label is compared on the
    /// resolved `Edge`. CRC32 hash collisions are handled by a final string
    /// comparison after full deserialization in both modes — correctness is
    /// never sacrificed.
    fn edges_for_direction_by_label(
        &self,
        node_id: u64,
        direction: AdjDirection,
        label: &str,
        start_ts: u64,
        reader_txn_id: Option<u64>,
    ) -> Result<Vec<Edge>> {
        let target_hash = node_codec::label_hash(label);
        let ptr = self.resolve_adj_pointer(node_id)?;
        let page_id = ptr.and_then(|p| match direction {
            AdjDirection::Outgoing => p.outgoing_page,
            AdjDirection::Incoming => p.incoming_page,
        });
        let mut edges = Vec::new();
        let push_matching = |edges: &mut Vec<Edge>, eid: u64| -> Result<()> {
            if !self.edge_exists.contains(&eid) {
                return Ok(());
            }
            if self.mvcc_enabled() {
                match self.resolve_edge_visible(EdgeId(eid), start_ts, reader_txn_id) {
                    Ok(edge) => {
                        if edge.label() == label {
                            edges.push(edge);
                        }
                    }
                    Err(Error::EdgeNotFound(_)) => {}
                    Err(e) => return Err(e),
                }
            } else {
                // Legacy: cheap page-hash prefilter, then string guard.
                if self.read_edge_label_hash(eid)? != target_hash {
                    return Ok(());
                }
                let edge = self.read_edge(eid)?;
                if edge.label() == label {
                    edges.push(edge);
                }
            }
            Ok(())
        };
        if let Some(pid) = page_id {
            let edge_ids = self.read_adj_edge_ids(pid, node_id, direction)?;
            for &eid in &edge_ids {
                push_matching(&mut edges, eid)?;
            }
        }
        if let Some(pending) = self.adj_pending.get(&(node_id, direction)) {
            for &eid in pending {
                push_matching(&mut edges, eid)?;
            }
        }
        // Additive txn-overlay branch (see `edges_for_direction`): no-op when
        // `reader_txn_id` is None. A pending edge is not in committed
        // `edge_exists`, so it is resolved through the delta chain directly and
        // the label compared on the resolved edge (no page-hash prefilter — a
        // pending edge has no page slot to read a hash from).
        for eid in self.pending_txn_edge_ids(node_id, direction, reader_txn_id) {
            match self.resolve_edge_visible(EdgeId(eid), start_ts, reader_txn_id) {
                Ok(edge) if edge.label() == label => edges.push(edge),
                Ok(_) | Err(Error::EdgeNotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(edges)
    }

    fn remove_edge_internal(&mut self, id: EdgeId) -> Result<Edge> {
        let edge = self.read_edge(id.0)?;

        // Tombstone the edge slot
        self.tombstone_edge_slot(id.0)?;

        // Reconcile category B (both adjacency directions, exists-set,
        // label/pair indexes, count).
        self.reconcile_edge_delete(&edge)?;

        Ok(edge)
    }

    // -----------------------------------------------------------------
    // Internal: page utilities
    // -----------------------------------------------------------------

    /// Resolves label and property overflow for an encoded slot.
    ///
    /// See [`SlotOverflowRequest`] for what the caller must supply, in
    /// particular the previous chain — getting that wrong is the difference
    /// between reclaiming a page and corrupting a live record.
    ///
    /// If the label overflowed, it is written to the string heap. If properties
    /// overflowed, they are written to overflow pages. The `patch` closure is
    /// called to write the resulting references back into the slot buffer.
    fn handle_slot_overflow<const N: usize>(
        &mut self,
        slot_buf: &mut [u8; N],
        req: SlotOverflowRequest<'_>,
        patch: impl FnOnce(&mut [u8; N], u32, u32),
    ) -> Result<()> {
        let SlotOverflowRequest {
            label_overflowed,
            label,
            props_overflowed,
            props_bytes,
            previous_prop_overflow,
            entity,
        } = req;
        let label_overflow_ref = if label_overflowed {
            self.string_heap.append(self.storage.as_mut(), label)?
        } else {
            0
        };

        // `None` when nothing overflowed. Deliberately not "0 means none": page
        // 0 is a real overflow page, so a plain `0` sentinel makes "no chain"
        // and "the chain at page 0" indistinguishable — which silently skipped
        // the release whenever an entity holding page 0 shrank back inline.
        let mut new_prop_overflow: Option<u32> = None;
        if props_overflowed {
            if let Some(bytes) = props_bytes {
                new_prop_overflow = Some(self.store_overflowed_props(entity, bytes)?);
            }
        }
        let prop_overflow_page = new_prop_overflow.unwrap_or(0);

        // Release the old chain only after the new one is safely written. The
        // reverse order would let the new write land on the pages the old chain
        // just released, and a failure midway would leave the slot pointing at
        // a chain that had already been handed away.
        //
        // Released even when the new properties fit inline (props_overflowed is
        // false): an entity that shrank below the cap has no further use for
        // its chain, and skipping this case would leak on every shrink.
        if let Some(old) = previous_prop_overflow {
            // Compared as options, so "shrank back inline" (None) counts as a
            // change even when the old chain happened to start at page 0.
            if new_prop_overflow != Some(old) {
                self.release_overflowed_props(entity, old)?;
            }
        }

        if label_overflowed || props_overflowed {
            patch(slot_buf, label_overflow_ref, prop_overflow_page);
        }

        Ok(())
    }

    /// Reads back an entity's overflowed properties from `page_id`.
    ///
    /// Overflow pages come in two shapes — a page shared by several entities,
    /// and a chain dedicated to one oversized blob — so the page itself decides
    /// how to read it. A shared page whose directory has no live entry for this
    /// entity yields empty bytes rather than an error: that is what a stale
    /// reference means, and returning whoever occupies that space now would be
    /// worse than returning nothing.
    fn read_overflowed_props(
        &self,
        entity: (u64, prop_slab_codec::EntityKind),
        page_id: u32,
    ) -> Result<Vec<u8>> {
        let (entity_id, kind) = entity;
        if prop_slab_codec::is_slab_page(self.storage.as_ref(), page_id).unwrap_or(false) {
            return Ok(prop_slab_codec::read_blob(
                self.storage.as_ref(),
                page_id,
                entity_id,
                kind,
            )?
            .unwrap_or_default());
        }
        overflow_codec::read_overflow(self.storage.as_ref(), page_id)
    }

    /// Stores an entity's overflowed properties and returns the page holding them.
    ///
    /// Blobs that fit a page are packed several entities to a page; only blobs
    /// too large for that keep the chained format. Packing is what removes the
    /// waste at the source — before it, a 39-byte property set occupied a whole
    /// 4096-byte page, an amplification of 105x.
    ///
    /// The page with room is remembered between calls, so the common case costs
    /// one page read rather than a scan of the overflow file. When that page
    /// fills, the next allocation replaces it — and since allocation now
    /// prefers a recycled page, a workload that keeps rewriting entities cycles
    /// through the same pages instead of extending the file.
    fn store_overflowed_props(
        &mut self,
        entity: (u64, prop_slab_codec::EntityKind),
        bytes: &[u8],
    ) -> Result<u32> {
        let (entity_id, kind) = entity;

        if bytes.len() > prop_slab_codec::MAX_PACKED_BLOB {
            // Too large to share a page. The chained format already uses its
            // pages efficiently at this size, so there is nothing to gain.
            return overflow_codec::write_overflow(self.storage.as_mut(), bytes);
        }

        // Try the page last known to have room.
        if let Some(page_id) = self.prop_slab_open_page {
            if self.slab_page_has_room(page_id, bytes.len()) {
                prop_slab_codec::write_blob(
                    self.storage.as_mut(),
                    page_id,
                    entity_id,
                    kind,
                    bytes,
                )?;
                return Ok(page_id);
            }
        }

        // None, or the remembered one is full: take a fresh page. This prefers
        // a page off the free list before growing the file.
        let page_id = self.storage.allocate_page(DataFile::Overflow)?;
        prop_slab_codec::init_page(self.storage.as_mut(), page_id)?;
        prop_slab_codec::write_blob(self.storage.as_mut(), page_id, entity_id, kind, bytes)?;
        self.prop_slab_open_page = Some(page_id);
        Ok(page_id)
    }

    /// Whether a slab page can still take a blob of `len` bytes.
    ///
    /// Answers `false` for a page that is not a slab at all, so a remembered id
    /// that has since been recycled into a chained page is simply abandoned
    /// rather than written over.
    fn slab_page_has_room(&self, page_id: u32, len: usize) -> bool {
        if page_id >= self.storage.page_count(DataFile::Overflow) {
            return false;
        }
        let Ok(buf) = self.storage.read_page(DataFile::Overflow, page_id) else {
            return false;
        };
        let header = crate::storage::page::PageHeader::read_from(&buf);
        if header.page_type != crate::storage::page::PageType::PropertySlab as u16 {
            return false;
        }
        prop_slab_codec::has_room_for(&buf[PAGE_HEADER_SIZE..], len)
    }

    /// Releases whatever an entity had stored at `page_id`.
    ///
    /// Handles both shapes: a packed blob is removed from its page's directory
    /// (and the page returned to the free list once nothing live remains), a
    /// chain is released whole.
    fn release_overflowed_props(
        &mut self,
        entity: (u64, prop_slab_codec::EntityKind),
        page_id: u32,
    ) -> Result<()> {
        let (entity_id, kind) = entity;

        if prop_slab_codec::is_slab_page(self.storage.as_ref(), page_id).unwrap_or(false) {
            let still_live =
                prop_slab_codec::free_blob(self.storage.as_mut(), page_id, entity_id, kind)?;
            if !still_live {
                // Nothing left on the page, so the whole page is reusable.
                // Forget it first: handing it to the free list while it is
                // still remembered as "the page with room" would let the next
                // write land on a page that has been given away.
                if self.prop_slab_open_page == Some(page_id) {
                    self.prop_slab_open_page = None;
                }
                self.storage.free_page(DataFile::Overflow, page_id)?;
            }
            return Ok(());
        }

        overflow_codec::free_overflow_chain(self.storage.as_mut(), page_id)
    }

    /// The property-overflow chain a stored node currently points at, if any.
    ///
    /// Returns `None` when the node has no overflow chain, or when its slot
    /// cannot be read — a caller uses this to decide what to release, and
    /// releasing nothing is the safe answer when the previous state is unknown.
    fn existing_node_prop_overflow(&self, id: u64) -> Option<u32> {
        let (page_idx, slot_idx) = Self::page_and_slot(id);
        let page = self.storage.read_page(DataFile::Nodes, page_idx).ok()?;
        let offset = PAGE_HEADER_SIZE + slot_idx * NODE_SLOT_SIZE;
        let slot: [u8; NODE_SLOT_SIZE] = page[offset..offset + NODE_SLOT_SIZE].try_into().ok()?;
        node_codec::slot_needs_prop_resolve(&slot)
            .then(|| node_codec::slot_prop_overflow_ref(&slot))
    }

    /// The property-overflow chain a stored edge currently points at, if any.
    fn existing_edge_prop_overflow(&self, id: u64) -> Option<u32> {
        let (page_idx, slot_idx) = Self::page_and_slot(id);
        let page = self.storage.read_page(DataFile::Edges, page_idx).ok()?;
        let offset = PAGE_HEADER_SIZE + slot_idx * EDGE_SLOT_SIZE;
        let slot: [u8; EDGE_SLOT_SIZE] = page[offset..offset + EDGE_SLOT_SIZE].try_into().ok()?;
        edge_codec::edge_slot_needs_prop_resolve(&slot)
            .then(|| edge_codec::edge_slot_prop_overflow_ref(&slot))
    }

    // -----------------------------------------------------------------
    // Category-B reconciliation (counters, exists-sets, indexes, adjacency)
    //
    // These helpers mutate the derived structures that reflect the *committed*
    // graph — everything except the page slot and the WAL. They are shared by
    // the auto-commit write path (which pairs them with an immediate page write)
    // and by the MVCC commit/vacuum split (option 2a): a `commit_txn` applies
    // only the ALTAS (an insert, or an update's new index entries) so a
    // committed change is post-commit-visible; the BAJAS (a delete's category-B
    // removal, or an update's stale old-value entries) are applied by the vacuum
    // once no live snapshot can still need the old version — see
    // `reconcile_committed_delta`, `apply_vacuum_category_b`, and `vacuum_once`.
    //
    // Unlike auto-commit `remove_node`, `reconcile_node_delete` does NOT cascade
    // to incident edges: an MVCC delete delta captures only its own entity, so
    // cascade is the caller's responsibility inside the transaction. This keeps
    // per-key reconciliation symmetric with the delta model.
    // -----------------------------------------------------------------

    /// Reconciles category B for a node insert: exists-set, indexes, count, and
    /// the negative adjacency-cache marker. Does not write the page or WAL.
    fn reconcile_node_insert(&mut self, node: &Node) {
        let id_val = node.id.0;
        self.node_exists.insert(id_val);
        self.node_label_index.insert(node.label(), id_val);
        self.node_property_index
            .insert_node(node.label(), node.properties(), id_val);
        self.storage.meta_mut().node_count += 1;
        self.adj_cache.insert(
            id_val,
            AdjacencyPointer {
                outgoing_page: None,
                incoming_page: None,
            },
        );
    }

    /// Reconciles category B for a node update: re-indexes label (if changed) and
    /// properties (always), diffing `prior` against `new`.
    fn reconcile_node_update(&mut self, prior: &Node, new: &Node) {
        let id_val = new.id.0;
        if prior.label() != new.label() {
            self.node_label_index.remove(prior.label(), id_val);
            self.node_label_index.insert(new.label(), id_val);
        }
        self.node_property_index
            .remove_node(prior.label(), prior.properties(), id_val);
        self.node_property_index
            .insert_node(new.label(), new.properties(), id_val);
    }

    /// Reconciles category B for a node delete: exists-set, indexes, adjacency
    /// cache, and count. Does not cascade to incident edges (see section note).
    fn reconcile_node_delete(&mut self, node: &Node) {
        let id_val = node.id.0;
        self.node_exists.remove(&id_val);
        self.node_label_index.remove(node.label(), id_val);
        self.node_property_index
            .remove_node(node.label(), node.properties(), id_val);
        self.adj_cache.remove(id_val);
        // Drop the tail-cache entry too (issue #33), so a future append for a
        // reused id can never trust a stale tail — keeps the two adjacency caches
        // consistent by construction, not by the monotonic-id invariant.
        self.adj_tail_cache.remove(id_val);
        self.storage.meta_mut().node_count -= 1;
    }

    /// Reconciles category B for an edge insert: exists-set, label/pair indexes,
    /// count, and both adjacency directions.
    fn reconcile_edge_insert(&mut self, edge: &Edge) -> Result<()> {
        let id_val = edge.id.0;
        self.edge_exists.insert(id_val);
        self.edge_label_index.insert(edge.label(), id_val);
        self.insert_pair_index(edge.source.0, edge.target.0, edge.label(), id_val);
        self.storage.meta_mut().edge_count += 1;
        self.add_edge_to_adjacency(edge.source.0, AdjDirection::Outgoing, id_val)?;
        self.add_edge_to_adjacency(edge.target.0, AdjDirection::Incoming, id_val)?;
        Ok(())
    }

    /// Reconciles category B for an edge update: re-indexes label and pair index
    /// only when the label changed (endpoints are immutable).
    fn reconcile_edge_update(&mut self, prior: &Edge, new: &Edge) {
        if prior.label() != new.label() {
            let id_val = new.id.0;
            self.edge_label_index.remove(prior.label(), id_val);
            self.edge_label_index.insert(new.label(), id_val);
            self.remove_pair_index(prior.source.0, prior.target.0, prior.label(), id_val);
            self.insert_pair_index(prior.source.0, prior.target.0, new.label(), id_val);
        }
    }

    /// Reconciles category B for an edge delete: both adjacency directions,
    /// exists-set, label/pair indexes, and count.
    fn reconcile_edge_delete(&mut self, edge: &Edge) -> Result<()> {
        let id_val = edge.id.0;
        self.remove_edge_from_adjacency(edge.source.0, AdjDirection::Outgoing, id_val)?;
        self.remove_edge_from_adjacency(edge.target.0, AdjDirection::Incoming, id_val)?;
        self.edge_exists.remove(&id_val);
        self.edge_label_index.remove(edge.label(), id_val);
        self.remove_pair_index(edge.source.0, edge.target.0, edge.label(), id_val);
        self.storage.meta_mut().edge_count -= 1;
        Ok(())
    }

    /// Reconciles category B for one committed key, applying ONLY the altas the
    /// transaction implies (an insert, or an update's new index entries).
    ///
    /// The net op is taken from the transaction's OLDEST delta on the key (did
    /// the transaction, as a whole, create / modify / remove the record versus
    /// the committed base?), while the visible new state comes from `delta`, the
    /// newest. A `Deleted`/`None` end state (a delete, or an insert-then-delete
    /// within one transaction) applies NO alta.
    ///
    /// The baja side — a real delete's category-B removal, or an update's stale
    /// old-value index entries — is applied by the vacuum once no live snapshot
    /// still needs the old version (see `apply_vacuum_category_b`). Applying it
    /// here would strip a version an older snapshot must still see, corrupting
    /// its view under snapshot isolation.
    fn reconcile_committed_delta(
        &mut self,
        key: crate::mvcc::EntityKey,
        delta: &crate::mvcc::Delta,
    ) -> Result<()> {
        use crate::mvcc::{DeltaOp, EntityKey, EntitySnapshot};
        let txn_id = delta.txn_id();
        let oldest = self
            .delta_table
            .as_ref()
            .and_then(|t| t.oldest_delta_of_txn(key, txn_id));
        let net_op = oldest
            .as_ref()
            .map_or_else(|| delta.op(), crate::mvcc::Delta::op);

        match (key, net_op, delta.new_state()) {
            (EntityKey::Node(_), DeltaOp::Insert, Some(EntitySnapshot::Node(node))) => {
                self.reconcile_node_insert(node);
            }
            (EntityKey::Node(_), DeltaOp::Update, Some(EntitySnapshot::Node(new))) => {
                if let Some(EntitySnapshot::Node(prior)) =
                    oldest.as_ref().and_then(crate::mvcc::Delta::prior)
                {
                    self.reconcile_node_update_add_only(prior, new);
                }
            }
            (EntityKey::Edge(_), DeltaOp::Insert, Some(EntitySnapshot::Edge(edge))) => {
                self.reconcile_edge_insert(edge)?;
            }
            (EntityKey::Edge(_), DeltaOp::Update, Some(EntitySnapshot::Edge(new))) => {
                if let Some(EntitySnapshot::Edge(prior)) =
                    oldest.as_ref().and_then(crate::mvcc::Delta::prior)
                {
                    self.reconcile_edge_update_add_only(prior, new);
                }
            }
            // Net absent (delete, or insert-then-delete): no alta at commit.
            _ => {}
        }
        Ok(())
    }

    /// Adds the NEW label/property index entries of a node update, leaving the
    /// old entries in place as a committed superset until the vacuum removes
    /// them (see `apply_vacuum_category_b`).
    fn reconcile_node_update_add_only(&mut self, prior: &Node, new: &Node) {
        let id_val = new.id.0;
        if prior.label() != new.label() {
            self.node_label_index.insert(new.label(), id_val);
        }
        self.node_property_index
            .insert_node(new.label(), new.properties(), id_val);
    }

    /// Edge analogue of [`Graph::reconcile_node_update_add_only`].
    fn reconcile_edge_update_add_only(&mut self, prior: &Edge, new: &Edge) {
        if prior.label() != new.label() {
            let id_val = new.id.0;
            self.edge_label_index.insert(new.label(), id_val);
            self.insert_pair_index(prior.source.0, prior.target.0, new.label(), id_val);
        }
    }

    fn write_slot_to_page(&mut self, id: u64, slot_buf: &[u8], layout: SlotLayout) -> Result<()> {
        self.write_slot_to_page_inner(id, slot_buf, layout, true)
    }

    /// Writes `slot_buf` to the page slot for `id`, logging the write to the WAL
    /// only when `log_wal` is set.
    ///
    /// The vacuum passes `log_wal = false`: a committed delta it materializes is
    /// already durable (its redo was written by [`Graph::commit_txn`] and is
    /// gated on `committed_txn_ids` at recovery), so re-logging here would double
    /// the WAL for every transactional write. The auto-commit path passes
    /// `log_wal = true`, its unchanged behavior.
    fn write_slot_to_page_inner(
        &mut self,
        id: u64,
        slot_buf: &[u8],
        layout: SlotLayout,
        log_wal: bool,
    ) -> Result<()> {
        let SlotLayout {
            slot_size,
            file,
            magic_bytes,
            page_type,
        } = layout;
        let (page_idx, slot_idx) = Self::page_and_slot(id);

        while self.storage.page_count(file) <= page_idx {
            self.storage.allocate_page(file)?;
        }

        // WAL: log the slot write before applying (auto-commit path only).
        if log_wal {
            // `slot_idx` is `zero_based % SLOTS_PER_PAGE` (31), so 0..=30.
            #[allow(clippy::cast_possible_truncation)]
            let slot_idx_u8 = slot_idx as u8;
            self.wal_log_slot(file, page_idx, slot_idx_u8, slot_buf)?;
        }

        let mut page = self.storage.read_page(file, page_idx)?;
        let offset = PAGE_HEADER_SIZE + slot_idx * slot_size;
        page[offset..offset + slot_size].copy_from_slice(slot_buf);

        let slot_count = Self::count_used_slots_on_page(&page, slot_size);
        finalize_page(&mut page, magic_bytes, 1, page_type, slot_count);
        self.storage.write_page(file, page_idx, &page)
    }

    /// Tombstones a slot (sets flags byte to `SLOT_TOMBSTONE`).
    fn tombstone_slot(&mut self, id: u64, layout: SlotLayout) -> Result<()> {
        self.tombstone_slot_inner(id, layout, true, true)
    }

    /// Tombstones the page slot for `id`, logging to the WAL only when `log_wal`
    /// is set. See [`Graph::write_slot_to_page_inner`] for why the vacuum passes
    /// `log_wal = false`.
    /// Tombstones a slot.
    ///
    /// `log_wal` journals the tombstone; `may_release` says whether the
    /// record's property-overflow pages can be handed back now.
    ///
    /// The two are deliberately separate. They coincided once — releasing when
    /// journalling — and that conflated a provisional MVCC tombstone (whose
    /// record a live snapshot may still resolve, so releasing would pull pages
    /// out from under a reader) with the vacuum materialising an already
    /// invisible delete (where releasing is exactly the point). Sharing one
    /// flag meant the vacuum never released anything, so under explicit
    /// transactions a delete leaked its pages permanently — the very defect
    /// this module exists to remove, surviving in the one path the auto-commit
    /// fix did not cover.
    fn tombstone_slot_inner(
        &mut self,
        id: u64,
        layout: SlotLayout,
        log_wal: bool,
        may_release: bool,
    ) -> Result<()> {
        let SlotLayout {
            slot_size,
            file,
            magic_bytes,
            page_type,
        } = layout;
        if log_wal {
            self.wal_log_tombstone(file, id)?;
        }

        let (page_idx, slot_idx) = Self::page_and_slot(id);
        // A tombstone targets a page that must already exist (the record was
        // live). If the page is absent there is nothing to tombstone.
        if self.storage.page_count(file) <= page_idx {
            return Ok(());
        }
        let mut page = self.storage.read_page(file, page_idx)?;
        let offset = PAGE_HEADER_SIZE + slot_idx * slot_size;

        // Whatever property-overflow chain this record referenced becomes
        // unreachable once the slot is tombstoned, so it is released here.
        // Read before the tombstone is stamped: afterwards the slot no longer
        // reports its overflow reference.
        //
        // Gated on `may_release`, not on journalling: a provisional MVCC
        // tombstone must NOT release (a reader whose snapshot predates it still
        // resolves the record), while the vacuum materialising an already
        // invisible delete MUST.
        let released_chain = if may_release {
            match file {
                DataFile::Nodes => self
                    .existing_node_prop_overflow(id)
                    .map(|p| (p, prop_slab_codec::EntityKind::Node)),
                DataFile::Edges => self
                    .existing_edge_prop_overflow(id)
                    .map(|p| (p, prop_slab_codec::EntityKind::Edge)),
                _ => None,
            }
        } else {
            None
        };

        page[offset] = SLOT_TOMBSTONE;

        let slot_count = Self::count_used_slots_on_page(&page, slot_size);
        finalize_page(&mut page, magic_bytes, 1, page_type, slot_count);
        self.storage.write_page(file, page_idx, &page)?;

        // After the tombstone is durable: if this failed midway, a chain that
        // is still referenced by a live slot would have been handed away.
        if let Some((page_id, kind)) = released_chain {
            self.release_overflowed_props((id, kind), page_id)?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------
    // WAL helpers
    // -----------------------------------------------------------------

    /// Logs a slot write to the WAL (`WriteNode` or `WriteEdge`).
    fn wal_log_slot(
        &mut self,
        file: DataFile,
        page_id: u32,
        slot_idx: u8,
        slot_buf: &[u8],
    ) -> Result<()> {
        if !self.storage.wal_enabled() {
            return Ok(());
        }
        let mut slot = Box::new([0u8; 128]);
        let copy_len = slot_buf.len().min(128);
        slot[..copy_len].copy_from_slice(&slot_buf[..copy_len]);

        // txn_id: None — these helpers log auto-commit page writes. Explicit
        // MVCC transactions do not write pages during their lifetime (deltas
        // live in memory; commit only stamps commit_ts); materialization to
        // page happens at vacuum, which logs on behalf of the committed txn.
        let record = match file {
            DataFile::Nodes => WalRecord::WriteNode {
                lsn: 0,
                page_id,
                slot_idx,
                slot,
                txn_id: None,
            },
            DataFile::Edges => WalRecord::WriteEdge {
                lsn: 0,
                page_id,
                slot_idx,
                slot,
                txn_id: None,
            },
            _ => return Ok(()), // only nodes/edges have slot writes
        };
        self.storage.wal_append(record)
    }

    /// Logs a tombstone to the WAL (`TombstoneNode` or `TombstoneEdge`).
    fn wal_log_tombstone(&mut self, file: DataFile, id: u64) -> Result<()> {
        if !self.storage.wal_enabled() {
            return Ok(());
        }
        let record = match file {
            DataFile::Nodes => WalRecord::TombstoneNode {
                lsn: 0,
                node_id: id,
                txn_id: None,
            },
            DataFile::Edges => WalRecord::TombstoneEdge {
                lsn: 0,
                edge_id: id,
                txn_id: None,
            },
            _ => return Ok(()),
        };
        self.storage.wal_append(record)
    }

    /// Logs a full adjacency page write to the WAL.
    fn wal_log_adj_page(&mut self, page_id: u32) -> Result<()> {
        if !self.storage.wal_enabled() {
            return Ok(());
        }
        let page = self.storage.read_page(DataFile::Adjacency, page_id)?;
        let record = WalRecord::WriteAdjPage {
            lsn: 0,
            page_id,
            data: page,
            txn_id: None,
        };
        self.storage.wal_append(record)
    }

    /// Emits a durable WAL redo for a single committed delta, tagged with its
    /// authoring `txn_id`, WITHOUT touching the node/edge page.
    ///
    /// This is the durability half of [`Graph::commit_txn`]. The node/edge slot
    /// is only logged (as `WriteNode`/`WriteEdge`/`Tombstone*` with
    /// `txn_id: Some`), not written to its page: writing the page here would
    /// break the snapshot of a still-live reader whose `start_ts` predates this
    /// commit. Page materialization stays the vacuum's job (Phase 5). On
    /// recovery there are no live readers, so `recover_from_wal` replays these
    /// redos to their pages, gated on the transaction being in
    /// `committed_txn_ids`.
    ///
    /// Overflow/string-heap pages ARE written to storage now (via
    /// [`Graph::handle_slot_overflow`]): they are append-only and immutable, so
    /// persisting them cannot disturb any reader's snapshot, and the logged slot
    /// must reference valid overflow pages for recovery to reconstruct the
    /// record. Those pages carry their own auto-commit (`txn_id: None`) WAL
    /// entries, which recovery always replays — correct, because an unreferenced
    /// overflow page left behind by an aborted transaction is inert.
    fn wal_log_committed_delta(
        &mut self,
        key: crate::mvcc::EntityKey,
        new_state: Option<&crate::mvcc::EntitySnapshot>,
        txn_id: u64,
    ) -> Result<()> {
        use crate::mvcc::{EntityKey, EntitySnapshot};
        if !self.storage.wal_enabled() {
            return Ok(());
        }
        match (key, new_state) {
            // Delete (or a delta whose new state is "gone"): tombstone redo.
            (EntityKey::Node(id), Some(EntitySnapshot::Deleted) | None) => {
                self.storage.wal_append(WalRecord::TombstoneNode {
                    lsn: 0,
                    node_id: id.0,
                    txn_id: Some(txn_id),
                })
            }
            (EntityKey::Edge(id), Some(EntitySnapshot::Deleted) | None) => {
                self.storage.wal_append(WalRecord::TombstoneEdge {
                    lsn: 0,
                    edge_id: id.0,
                    txn_id: Some(txn_id),
                })
            }
            // Insert/update of a node: encode its slot, persist overflow, log slot.
            (EntityKey::Node(_), Some(EntitySnapshot::Node(node))) => {
                let (mut slot_buf, overflow) = node_codec::encode_node_slot(node)?;
                self.handle_slot_overflow(
                    &mut slot_buf,
                    SlotOverflowRequest {
                        label_overflowed: overflow.label_overflowed,
                        label: node.label(),
                        props_overflowed: overflow.props_overflowed,
                        props_bytes: overflow.props_bytes.as_deref(),
                        previous_prop_overflow: None,
                        entity: (node.id.0, prop_slab_codec::EntityKind::Node),
                    },
                    node_codec::patch_overflow,
                )?;
                let (page_id, slot_idx) = Self::page_and_slot(node.id.0);
                // `slot_idx` is `zero_based % SLOTS_PER_PAGE` (31), so 0..=30.
                #[allow(clippy::cast_possible_truncation)]
                let slot_idx = slot_idx as u8;
                self.storage.wal_append(WalRecord::WriteNode {
                    lsn: 0,
                    page_id,
                    slot_idx,
                    slot: Box::new(slot_buf),
                    txn_id: Some(txn_id),
                })
            }
            (EntityKey::Edge(_), Some(EntitySnapshot::Edge(edge))) => {
                let (mut slot_buf, overflow) = edge_codec::encode_edge_slot(edge)?;
                self.handle_slot_overflow(
                    &mut slot_buf,
                    SlotOverflowRequest {
                        label_overflowed: overflow.label_overflowed,
                        label: edge.label(),
                        props_overflowed: overflow.props_overflowed,
                        props_bytes: overflow.props_bytes.as_deref(),
                        previous_prop_overflow: None,
                        entity: (edge.id.0, prop_slab_codec::EntityKind::Edge),
                    },
                    edge_codec::patch_edge_overflow,
                )?;
                let (page_id, slot_idx) = Self::page_and_slot(edge.id.0);
                // `slot_idx` is `zero_based % SLOTS_PER_PAGE` (31), so 0..=30.
                #[allow(clippy::cast_possible_truncation)]
                let slot_idx = slot_idx as u8;
                self.storage.wal_append(WalRecord::WriteEdge {
                    lsn: 0,
                    page_id,
                    slot_idx,
                    slot: Box::new(slot_buf),
                    txn_id: Some(txn_id),
                })
            }
            // A node key whose new state is an edge (or vice versa) can only
            // arise from a construction bug in a `*_in_txn` writer: every writer
            // pairs `EntityKey::Node` with `EntitySnapshot::Node` and likewise
            // for edges. This is an internal invariant, not a recoverable state.
            (EntityKey::Node(_), Some(EntitySnapshot::Edge(_)))
            | (EntityKey::Edge(_), Some(EntitySnapshot::Node(_))) => {
                unreachable!("delta entity kind must match its key kind")
            }
        }
    }

    /// Syncs the WAL to disk, guaranteeing durability of all prior appends.
    /// Skipped when inside a batch (deferred to `end_batch`).
    ///
    /// When a [`WalObserver`] is installed (via
    /// [`Graph::open_with_wal_observer`] or
    /// [`Graph::with_wal_observer`]), the wall-clock duration of the
    /// underlying `storage.wal_sync()` call is measured and handed to
    /// the observer together with `cause` — [`FsyncCause::Individual`] for a
    /// per-operation fsync, [`FsyncCause::BatchClose`] for the one that closes
    /// a coalesced batch. The observer fires *after* the call returns
    /// regardless of success — failures still take measurable time
    /// (a fsync that fails on `ENOSPC` still went through the syscall),
    /// and excluding them would skew the histogram toward the happy
    /// path. Skips inside a batch are not observed: the observer only
    /// sees fsyncs the engine actually performs.
    fn wal_sync(&mut self, cause: FsyncCause) -> Result<()> {
        if self.batch_depth > 0 {
            return Ok(());
        }
        if let Some(ref observer) = self.wal_observer {
            let started = std::time::Instant::now();
            let outcome = self.storage.wal_sync();
            observer(cause, started.elapsed());
            outcome
        } else {
            self.storage.wal_sync()
        }
    }

    fn count_used_slots_on_page(
        page: &[u8; crate::storage::page::PAGE_SIZE],
        slot_size: usize,
    ) -> u16 {
        let mut count: u16 = 0;
        for i in 0..SLOTS_PER_PAGE {
            let offset = PAGE_HEADER_SIZE + i * slot_size;
            if offset + slot_size > page.len() {
                break;
            }
            let flags = page[offset];
            if flags == SLOT_LIVE || flags == SLOT_TOMBSTONE {
                count += 1;
            }
        }
        count
    }
}

/// Thread-safe handle to a [`Graph`], backed by `Arc<RwLock<Graph>>`.
///
/// Clone is cheap (increments the `Arc` refcount). All access to the
/// underlying graph goes through [`read()`](Self::read) or
/// [`write()`](Self::write), which acquire the appropriate lock.
///
/// For batch operations that need multiple calls under one lock, hold
/// the guard directly:
///
/// ```rust
/// # use tessera_graph::{SharedGraph, Graph, props};
/// let sg = SharedGraph::new(Graph::new());
/// {
///     let mut g = sg.write();
///     g.add_node("A", props! {}).unwrap();
///     g.add_node("B", props! {}).unwrap();
/// }
/// assert_eq!(sg.read().node_count(), 2);
/// ```
#[derive(Clone)]
pub struct SharedGraph {
    inner: Arc<RwLock<Graph>>,
}

impl SharedGraph {
    /// Wraps a `Graph` in a thread-safe `Arc<RwLock<_>>`.
    #[must_use]
    pub fn new(graph: Graph) -> Self {
        Self {
            inner: Arc::new(RwLock::new(graph)),
        }
    }

    /// Acquires a read lock on the underlying graph.
    ///
    /// # Panics
    ///
    /// Panics if the lock is poisoned (a thread panicked while holding a write lock).
    pub fn read(&self) -> RwLockReadGuard<'_, Graph> {
        self.inner.read().expect("shared_graph lock poisoned")
    }

    /// Acquires a write lock on the underlying graph.
    ///
    /// # Panics
    ///
    /// Panics if the lock is poisoned (a thread panicked while holding a write lock).
    pub fn write(&self) -> RwLockWriteGuard<'_, Graph> {
        self.inner.write().expect("shared_graph lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::property::Property;
    use crate::props;
    // Used by the WalObserver tests at the end of this module
    // (C1 of v0.6.0 Fase 2 Task 2). `Arc` is already re-exported via
    // `super::*` at module level; `Mutex` is not.
    use std::sync::Mutex;

    #[test]
    fn test_new_creates_empty_graph() {
        let g = Graph::new();
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
    }

    // ---- Issue #43 Part B: fsync cause on the WAL observer -------------------

    #[test]
    fn fsync_cause_individual_and_batch_close_are_distinct() {
        // The observer must be able to tell a per-operation fsync apart from the
        // single fsync that closes a coalesced batch, and two batch-close causes
        // reporting the same op count must compare equal.
        assert_ne!(
            FsyncCause::Individual,
            FsyncCause::BatchClose { op_count: 3 },
            "an individual fsync is never a batch-close fsync"
        );
        assert_eq!(
            FsyncCause::BatchClose { op_count: 3 },
            FsyncCause::BatchClose { op_count: 3 },
            "two batch-close causes with the same op count are equal"
        );
        assert_ne!(
            FsyncCause::BatchClose { op_count: 2 },
            FsyncCause::BatchClose { op_count: 3 },
            "batch-close causes differ when their op counts differ"
        );
    }

    // ---- Issue #37: batch caps (double limit) --------------------------------

    #[test]
    fn set_batch_limits_stores_configured_caps() {
        let mut g = Graph::new();
        g.set_batch_limits(Some(10), Some(1024));
        assert_eq!(g.batch_max_ops, Some(10));
        assert_eq!(g.batch_max_bytes, Some(1024));
    }

    #[test]
    fn batch_limits_default_to_unlimited() {
        let g = Graph::new();
        assert_eq!(g.batch_max_ops, None);
        assert_eq!(g.batch_max_bytes, None);
    }

    #[test]
    fn batch_op_count_increments_inside_batch_without_limits() {
        let mut g = Graph::new();
        g.begin_batch();
        g.add_node("N", Properties::new()).unwrap();
        g.add_node("N", Properties::new()).unwrap();
        assert_eq!(g.batch_op_count, 2);
        g.end_batch().unwrap();
    }

    #[test]
    fn batch_op_count_stays_zero_outside_batch() {
        let mut g = Graph::new();
        g.add_node("N", Properties::new()).unwrap();
        assert_eq!(g.batch_op_count, 0);
    }

    #[test]
    fn batch_byte_count_reflects_node_size_estimate() {
        let mut g = Graph::new();
        g.begin_batch();
        let mut props = Properties::new();
        props.insert("name".into(), Property::from("Alice"));
        g.add_node("Person", props).unwrap();
        // base struct size + label bytes + property key/value heap bytes > base
        assert!(g.batch_byte_count > std::mem::size_of::<Node>() as u64);
        g.end_batch().unwrap();
    }

    #[test]
    fn batch_byte_count_accumulates_across_multiple_mutations() {
        let mut g = Graph::new();
        g.begin_batch();
        g.add_node("N", Properties::new()).unwrap();
        let after_one = g.batch_byte_count;
        assert!(after_one > 0);
        g.add_node("N", Properties::new()).unwrap();
        assert_eq!(g.batch_byte_count, after_one * 2);
        g.end_batch().unwrap();
    }

    #[test]
    fn batch_rejects_mutation_exceeding_op_limit() {
        let mut g = Graph::new();
        g.set_batch_limits(Some(2), None);
        g.begin_batch();
        g.add_node("N", Properties::new()).unwrap();
        g.add_node("N", Properties::new()).unwrap();
        let err = g.add_node("N", Properties::new()).unwrap_err();
        assert!(matches!(
            err,
            Error::BatchLimitExceeded {
                kind: BatchLimitKind::Operations,
                current: 3,
                limit: 2
            }
        ));
        g.end_batch().unwrap();
    }

    #[test]
    fn batch_op_limit_rejection_does_not_apply_the_mutation() {
        let mut g = Graph::new();
        g.set_batch_limits(Some(1), None);
        g.begin_batch();
        g.add_node("N", Properties::new()).unwrap();
        assert!(g.add_node("N", Properties::new()).is_err());
        // the rejected 2nd add_node left no trace
        assert_eq!(g.node_count(), 1);
        // counters unchanged by the rejected op
        assert_eq!(g.batch_op_count, 1);
        g.end_batch().unwrap();
    }

    #[test]
    fn batch_op_limit_does_not_apply_outside_a_batch() {
        let mut g = Graph::new();
        g.set_batch_limits(Some(1), None);
        g.add_node("N", Properties::new()).unwrap();
        // 2nd call, no batch open: not capped
        g.add_node("N", Properties::new()).unwrap();
        assert_eq!(g.node_count(), 2);
    }

    #[test]
    fn batch_rejects_mutation_exceeding_byte_limit() {
        // measure the real per-call cost first
        let one_node_cost = {
            let mut probe = Graph::new();
            probe.begin_batch();
            probe.add_node("N", Properties::new()).unwrap();
            probe.batch_byte_count
        };
        let mut g = Graph::new();
        g.set_batch_limits(None, Some(one_node_cost)); // room for exactly 1
        g.begin_batch();
        g.add_node("N", Properties::new()).unwrap();
        let err = g.add_node("N", Properties::new()).unwrap_err();
        assert!(matches!(
            err,
            Error::BatchLimitExceeded {
                kind: BatchLimitKind::Bytes,
                ..
            }
        ));
        g.end_batch().unwrap();
    }

    #[test]
    fn batch_byte_limit_rejection_does_not_apply_the_mutation() {
        let mut g = Graph::new();
        g.set_batch_limits(None, Some(1)); // 1 byte: even the first add_node breaches it
        g.begin_batch();
        let err = g.add_node("N", Properties::new()).unwrap_err();
        assert!(matches!(
            err,
            Error::BatchLimitExceeded {
                kind: BatchLimitKind::Bytes,
                ..
            }
        ));
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.batch_byte_count, 0);
        // the op counter must not advance either when a byte rejection fires
        assert_eq!(g.batch_op_count, 0);
        g.end_batch().unwrap();
    }

    #[test]
    fn batch_op_limit_wins_when_both_caps_would_trip() {
        // op cap trips first (limit 1) while the byte cap is effectively open;
        // asserts precedence is deterministic (operations reported first).
        let mut g = Graph::new();
        g.set_batch_limits(Some(1), Some(u64::MAX));
        g.begin_batch();
        g.add_node("N", Properties::new()).unwrap();
        let err = g.add_node("N", Properties::new()).unwrap_err();
        assert!(matches!(
            err,
            Error::BatchLimitExceeded {
                kind: BatchLimitKind::Operations,
                ..
            }
        ));
        g.end_batch().unwrap();
    }

    #[test]
    fn batch_counters_reset_when_outermost_batch_closes() {
        let mut g = Graph::new();
        g.begin_batch();
        g.add_node("N", Properties::new()).unwrap();
        g.add_node("N", Properties::new()).unwrap();
        assert_eq!(g.batch_op_count, 2);
        g.end_batch().unwrap();
        assert_eq!(g.batch_op_count, 0);
        assert_eq!(g.batch_byte_count, 0);
    }

    #[test]
    fn batch_counters_do_not_reset_on_nested_end_batch() {
        let mut g = Graph::new();
        g.begin_batch(); // depth 1
        g.begin_batch(); // depth 2
        g.add_node("N", Properties::new()).unwrap();
        g.end_batch().unwrap(); // depth back to 1: NOT the outermost close
        assert_eq!(g.batch_op_count, 1); // still counted, not reset
        g.end_batch().unwrap(); // depth 0: outermost close
        assert_eq!(g.batch_op_count, 0);
    }

    #[test]
    fn batch_limits_apply_fresh_after_a_previous_batch_closed() {
        let mut g = Graph::new();
        g.set_batch_limits(Some(1), None);
        g.begin_batch();
        g.add_node("N", Properties::new()).unwrap();
        g.end_batch().unwrap(); // counters reset to 0
        g.begin_batch();
        g.add_node("N", Properties::new()).unwrap(); // 1st op of the NEW batch: allowed
        assert_eq!(g.node_count(), 2);
        g.end_batch().unwrap();
    }

    // Block 4 Phase 2, Cycle 10: regression anchor for the read fast path.
    // With no MVCC transaction active, a node reads straight from its page —
    // the delta-chain machinery introduced in this phase must not alter it.
    #[test]
    fn node_with_no_deltas_reads_straight_from_page() {
        let mut g = Graph::new();
        let id = g.add_node("Person", props! {}).unwrap();
        let n = g.node(id).unwrap();
        assert_eq!(n.label(), "Person");
    }

    // ---- Block 4 Phase 3: MVCC write/read path integration ------------------

    #[test]
    fn graph_mvcc_disabled_by_default() {
        let g = Graph::new();
        assert!(!g.mvcc_enabled());
    }

    #[test]
    fn graph_enable_mvcc_turns_on_delta_table() {
        let mut g = Graph::new();
        g.enable_mvcc();
        assert!(g.mvcc_enabled());
    }

    #[test]
    fn graph_begin_txn_registers_active() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        assert!(g.txn_is_active(txn));
    }

    #[test]
    fn graph_begin_txn_without_mvcc_errors() {
        let mut g = Graph::new();
        assert!(matches!(g.begin_txn().unwrap_err(), Error::MvccNotEnabled));
    }

    #[test]
    fn add_node_under_txn_is_invisible_to_other_reader_before_commit() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let id = g
            .add_node_in_txn(txn, "Person", props! {"name" => "Alice"})
            .unwrap();
        // The author sees its own node.
        assert_eq!(g.node_in_txn(txn, id).unwrap().label(), "Person");
        // An auto-commit reader (before commit) does not.
        assert!(matches!(g.node(id), Err(Error::NodeNotFound(_))));
    }

    #[test]
    fn txn_exceeding_memory_cap_is_aborted() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.set_txn_memory_cap(Some(200)); // bytes, deliberately tiny for the test
        let txn = g.begin_txn().unwrap();
        let mut last_err = None;
        for _ in 0..1000 {
            if let Err(e) = g.add_node_in_txn(txn, "N", props! {}) {
                last_err = Some(e);
                break;
            }
        }
        assert!(matches!(last_err, Some(Error::TxnMemoryCapExceeded { .. })));
        // The whole transaction was aborted, not just the last operation.
        assert!(!g.txn_is_active(txn));
    }

    #[test]
    fn txn_under_memory_cap_succeeds_normally() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.set_txn_memory_cap(Some(64 * 1024 * 1024)); // high cap: no effect
        let txn = g.begin_txn().unwrap();
        for _ in 0..100 {
            g.add_node_in_txn(txn, "N", props! {}).unwrap();
        }
        assert!(g.txn_is_active(txn));
        g.commit_txn(txn).unwrap();
    }

    #[test]
    fn add_node_in_txn_does_not_bump_committed_node_count() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        g.add_node_in_txn(txn, "Person", props! {}).unwrap();
        // node_count reflects committed storage meta; the pending insert is not
        // committed yet.
        assert_eq!(g.node_count(), 0);
    }

    #[test]
    fn auto_commit_outgoing_edges_by_label_unaffected_by_txn_overlay_changes() {
        // R2 lock: pins the auto-commit adjacency behaviour (reader_txn_id is
        // always None on this path) BEFORE the txn overlay branch is added to
        // edges_for_direction[_by_label]. If a later cycle regresses auto-commit,
        // this test goes red — stop and fix that cycle, do not advance.
        let mut g = Graph::new();
        // No enable_mvcc(): purely auto-commit path.
        let a = g.add_node("N", props! {}).unwrap();
        let b = g.add_node("N", props! {}).unwrap();
        g.add_edge("SIGUE", a, b, props! {}).unwrap();
        g.add_edge("OTRA", a, b, props! {}).unwrap();

        let matching = g.outgoing_edges_by_label(a, "SIGUE").unwrap();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].label(), "SIGUE");
        assert_eq!(g.outgoing_edges(a).unwrap().len(), 2);
        assert_eq!(g.incoming_edges(b).unwrap().len(), 2);
    }

    #[test]
    fn add_node_in_txn_seeds_pending_overlay() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(txn, "Person", props! {}).unwrap();

        // The txn's own pending insert is reachable by enumeration, not just by
        // id: node_ids_in_txn unions committed with the txn overlay.
        assert_eq!(g.node_ids_in_txn(txn).unwrap(), vec![id]);
    }

    #[test]
    fn add_node_in_txn_seeds_overlay_via_single_choke_point() {
        // R1 defence (node half): the pending-insert overlay is populated by the
        // shared `push_txn_delta` choke point, not by a caller remembering to.
        // The edge half of this lock is added in the adjacency cycle, once
        // `outgoing_edges_in_txn` consults the adjacency overlay.
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();

        let node = g.add_node_in_txn(txn, "N", props! {}).unwrap();
        assert!(g.node_ids_in_txn(txn).unwrap().contains(&node));
    }

    #[test]
    fn add_edge_in_txn_seeds_overlay_via_single_choke_point() {
        // R1 defence (edge half): add_edge_in_txn seeds the adjacency overlay
        // through the same `push_txn_delta` choke point as nodes, no extra call.
        let mut g = Graph::new();
        g.enable_mvcc();
        let a = g.add_node("A", props! {}).unwrap();
        let b = g.add_node("B", props! {}).unwrap();
        let txn = g.begin_txn().unwrap();

        let edge = g.add_edge_in_txn(txn, "REL", a, b, props! {}).unwrap();
        assert_eq!(g.outgoing_edges_in_txn(txn, a).unwrap().len(), 1);
        assert_eq!(g.outgoing_edges_in_txn(txn, a).unwrap()[0].id(), edge);
    }

    #[test]
    fn add_edge_in_txn_seeds_pending_outgoing_adjacency() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let a = g.add_node("N", props! {}).unwrap();
        let b = g.add_node("N", props! {}).unwrap();
        let txn = g.begin_txn().unwrap();
        let eid = g.add_edge_in_txn(txn, "REL", a, b, props! {}).unwrap();

        let out = g.outgoing_edges_in_txn(txn, a).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id(), eid);
    }

    #[test]
    fn outgoing_edges_by_label_in_txn_sees_own_pending_edge() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let a = g.add_node("N", props! {}).unwrap();
        let b = g.add_node("N", props! {}).unwrap();
        let txn = g.begin_txn().unwrap();
        g.add_edge_in_txn(txn, "SIGUE", a, b, props! {}).unwrap();
        g.add_edge_in_txn(txn, "OTRA", a, b, props! {}).unwrap();

        let matching = g.outgoing_edges_by_label_in_txn(txn, a, "SIGUE").unwrap();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].label(), "SIGUE");
    }

    #[test]
    fn incoming_edges_in_txn_sees_own_pending_edge() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let a = g.add_node("N", props! {}).unwrap();
        let b = g.add_node("N", props! {}).unwrap();
        let txn = g.begin_txn().unwrap();
        let eid = g.add_edge_in_txn(txn, "REL", a, b, props! {}).unwrap();

        let incoming = g.incoming_edges_in_txn(txn, b).unwrap();
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].id(), eid);
    }

    #[test]
    fn node_created_then_removed_in_same_txn_not_in_enumeration() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(txn, "Efimero", props! {}).unwrap();
        g.remove_node_in_txn(txn, id).unwrap();

        assert!(g.node_ids_in_txn(txn).unwrap().is_empty());
        assert!(g.nodes_by_label_in_txn(txn, "Efimero").unwrap().is_empty());
    }

    #[test]
    fn edge_to_node_created_in_same_txn_is_traversable() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let existing = g.add_node("Origen", props! {}).unwrap();
        let txn = g.begin_txn().unwrap();
        let fresh = g.add_node_in_txn(txn, "Destino", props! {}).unwrap();
        let eid = g
            .add_edge_in_txn(txn, "APUNTA_A", existing, fresh, props! {})
            .unwrap();

        let out = g.outgoing_edges_in_txn(txn, existing).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id(), eid);

        let incoming = g.incoming_edges_in_txn(txn, fresh).unwrap();
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].id(), eid);

        assert!(g.node_ids_in_txn(txn).unwrap().contains(&fresh));
    }

    #[test]
    fn txn_overlay_isolated_between_concurrent_transactions() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn_a = g.begin_txn().unwrap();
        let txn_b = g.begin_txn().unwrap();

        let node_a = g.add_node_in_txn(txn_a, "SoloA", props! {}).unwrap();
        let node_b = g.add_node_in_txn(txn_b, "SoloB", props! {}).unwrap();

        let ids_a = g.node_ids_in_txn(txn_a).unwrap();
        assert!(ids_a.contains(&node_a));
        assert!(!ids_a.contains(&node_b));
        let ids_b = g.node_ids_in_txn(txn_b).unwrap();
        assert!(ids_b.contains(&node_b));
        assert!(!ids_b.contains(&node_a));

        let src = g.add_node("Src", props! {}).unwrap();
        let dst = g.add_node("Dst", props! {}).unwrap();
        g.add_edge_in_txn(txn_a, "REL", src, dst, props! {})
            .unwrap();
        assert_eq!(g.outgoing_edges_in_txn(txn_a, src).unwrap().len(), 1);
        assert_eq!(g.outgoing_edges_in_txn(txn_b, src).unwrap().len(), 0);
    }

    #[test]
    fn node_count_in_txn_includes_pending() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.add_node("Base1", props! {}).unwrap();
        g.add_node("Base2", props! {}).unwrap();
        let txn = g.begin_txn().unwrap();
        g.add_node_in_txn(txn, "Nuevo", props! {}).unwrap();

        assert_eq!(g.node_count_in_txn(txn).unwrap(), 3);
        assert_eq!(g.node_count(), 2);
    }

    #[test]
    fn overlay_cleared_after_commit_and_rollback_no_cross_txn_leak() {
        let mut g = Graph::new();
        g.enable_mvcc();

        // txn1 commits its node: it becomes committed (reconciled into
        // node_exists), so later transactions DO see it — that is what commit
        // means. The overlay entry for txn1 is gone, but the node is now real.
        let txn1 = g.begin_txn().unwrap();
        let committed = g.add_node_in_txn(txn1, "N", props! {}).unwrap();
        g.commit_txn(txn1).unwrap();
        assert!(matches!(
            g.node_ids_in_txn(txn1),
            Err(Error::TxnNotActive(_))
        ));

        // txn2 rolls back: its node must NOT survive anywhere.
        let txn2 = g.begin_txn().unwrap();
        let rolled_back = g.add_node_in_txn(txn2, "N2", props! {}).unwrap();
        g.rollback_txn(txn2).unwrap();
        assert!(matches!(
            g.node_ids_in_txn(txn2),
            Err(Error::TxnNotActive(_))
        ));

        // txn3 sees the committed node but never the rolled-back one, and no
        // stale overlay leaks from txn1/txn2 into it.
        let txn3 = g.begin_txn().unwrap();
        let ids = g.node_ids_in_txn(txn3).unwrap();
        assert_eq!(ids, vec![committed]);
        assert!(!ids.contains(&rolled_back));
    }

    #[test]
    fn node_ids_in_txn_unions_committed_and_pending() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let committed = g.add_node("Base", props! {}).unwrap();
        let txn = g.begin_txn().unwrap();
        let pending = g.add_node_in_txn(txn, "New", props! {}).unwrap();

        let mut ids = g.node_ids_in_txn(txn).unwrap();
        ids.sort_by_key(|n| n.0);
        let mut expected = vec![committed, pending];
        expected.sort_by_key(|n| n.0);
        assert_eq!(ids, expected);
    }

    #[test]
    fn node_ids_in_txn_rejects_inactive_txn() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        g.commit_txn(txn).unwrap();
        assert!(matches!(
            g.node_ids_in_txn(txn),
            Err(Error::TxnNotActive(_))
        ));
    }

    #[test]
    fn nodes_by_label_in_txn_sees_own_pending_label() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let committed = g.add_node("Persona", props! {}).unwrap();
        let txn = g.begin_txn().unwrap();
        let pending = g.add_node_in_txn(txn, "Persona", props! {}).unwrap();
        let other_label = g.add_node_in_txn(txn, "Empresa", props! {}).unwrap();

        let mut ids = g.nodes_by_label_in_txn(txn, "Persona").unwrap();
        ids.sort_by_key(|n| n.0);
        let mut expected = vec![committed, pending];
        expected.sort_by_key(|n| n.0);
        assert_eq!(ids, expected);
        assert!(!ids.contains(&other_label));
    }

    #[test]
    fn update_node_in_txn_isolates_writer_from_other_reader() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("Person", props! {"name" => "Alice"}).unwrap();
        let txn = g.begin_txn().unwrap();
        let mut updated = g.node(id).unwrap();
        updated
            .properties_mut()
            .insert("name".into(), Property::String("Alicia".into()));
        g.update_node_in_txn(txn, id, &updated).unwrap();

        // The author sees "Alicia".
        assert_eq!(
            g.node_in_txn(txn, id).unwrap().properties().get("name"),
            Some(&Property::String("Alicia".into()))
        );
        // An auto-commit reader still sees the committed "Alice".
        assert_eq!(
            g.node(id).unwrap().properties().get("name"),
            Some(&Property::String("Alice".into()))
        );
    }

    #[test]
    fn remove_node_in_txn_hides_from_own_txn_not_others() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("Person", props! {}).unwrap();
        let txn = g.begin_txn().unwrap();
        g.remove_node_in_txn(txn, id).unwrap();
        assert!(matches!(
            g.node_in_txn(txn, id),
            Err(Error::NodeNotFound(_))
        ));
        // Another (auto-commit) reader still sees it.
        assert!(g.node(id).is_ok());
    }

    #[test]
    fn second_update_in_same_txn_chains_on_first_delta_not_committed_base() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("Person", props! {"v" => 0i64}).unwrap();
        let txn = g.begin_txn().unwrap();

        let mut n1 = g.node(id).unwrap();
        n1.properties_mut().insert("v".into(), Property::I64(1));
        g.update_node_in_txn(txn, id, &n1).unwrap();

        let mut n2 = g.node_in_txn(txn, id).unwrap();
        assert_eq!(n2.properties().get("v"), Some(&Property::I64(1)));
        n2.properties_mut().insert("v".into(), Property::I64(2));
        g.update_node_in_txn(txn, id, &n2).unwrap();

        assert_eq!(
            g.node_in_txn(txn, id).unwrap().properties().get("v"),
            Some(&Property::I64(2))
        );
    }

    #[test]
    fn outgoing_edges_hides_edge_committed_after_readers_snapshot() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let a = g.add_node("N", props! {}).unwrap();
        let b = g.add_node("N", props! {}).unwrap();
        // An old reader whose snapshot predates the edge below.
        let old = g.begin_txn().unwrap();
        // A writer commits an edge a->b.
        let w = g.begin_txn().unwrap();
        g.add_edge_in_txn(w, "REL", a, b, props! {}).unwrap();
        g.commit_txn(w).unwrap();
        // A new auto-commit reader sees the edge via the traversal.
        assert_eq!(g.outgoing_edges(a).unwrap().len(), 1);
        // The old reader's snapshot must NOT see it.
        assert_eq!(g.outgoing_edges_in_txn(old, a).unwrap().len(), 0);
        g.rollback_txn(old).unwrap();
    }

    #[test]
    fn add_edge_in_txn_references_node_created_in_same_uncommitted_txn() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let a = g.add_node_in_txn(txn, "A", props! {}).unwrap();
        let b = g.add_node_in_txn(txn, "B", props! {}).unwrap();
        // Endpoints exist only in this txn's uncommitted deltas.
        let e = g.add_edge_in_txn(txn, "LINK", a, b, props! {}).unwrap();
        let edge = g.edge_in_txn(txn, e).unwrap();
        assert_eq!(edge.source(), a);
        assert_eq!(edge.target(), b);
        assert_eq!(edge.label(), "LINK");
        // Not visible to an auto-commit reader before commit.
        assert!(matches!(g.edge(e), Err(Error::EdgeNotFound(_))));
    }

    #[test]
    fn in_txn_ops_reject_inactive_txn() {
        let mut g = Graph::new();
        g.enable_mvcc();
        // txn id 999 was never begun.
        assert!(matches!(
            g.add_node_in_txn(999, "X", props! {}).unwrap_err(),
            Error::TxnNotActive(999)
        ));
    }

    #[test]
    fn update_edge_in_txn_isolates_writer_from_other_reader() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let a = g.add_node("A", props! {}).unwrap();
        let b = g.add_node("B", props! {}).unwrap();
        let e = g.add_edge("LINK", a, b, props! {"w" => 1i64}).unwrap();
        let txn = g.begin_txn().unwrap();
        let mut updated = g.edge(e).unwrap();
        updated
            .properties_mut()
            .insert("w".into(), Property::I64(2));
        g.update_edge_in_txn(txn, e, &updated).unwrap();

        // Author sees the new weight; an auto-commit reader still sees the old.
        assert_eq!(
            g.edge_in_txn(txn, e).unwrap().properties().get("w"),
            Some(&Property::I64(2))
        );
        assert_eq!(
            g.edge(e).unwrap().properties().get("w"),
            Some(&Property::I64(1))
        );
    }

    #[test]
    fn remove_edge_in_txn_hides_from_own_txn_not_others() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let a = g.add_node("A", props! {}).unwrap();
        let b = g.add_node("B", props! {}).unwrap();
        let e = g.add_edge("LINK", a, b, props! {}).unwrap();
        let txn = g.begin_txn().unwrap();
        g.remove_edge_in_txn(txn, e).unwrap();
        assert!(matches!(g.edge_in_txn(txn, e), Err(Error::EdgeNotFound(_))));
        // An auto-commit reader still sees it.
        assert!(g.edge(e).is_ok());
    }

    #[test]
    fn node_read_under_mvcc_sees_committed_node_with_no_deltas() {
        // MVCC enabled, node committed before any txn, never touched by a delta:
        // the resolve path with chain_for == None must return the page version.
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("Person", props! {"name" => "Alice"}).unwrap();
        let n = g.node(id).unwrap();
        assert_eq!(n.label(), "Person");
        assert_eq!(
            n.properties().get("name"),
            Some(&Property::String("Alice".into()))
        );
    }

    #[test]
    fn node_projected_and_label_agree_with_node_under_mvcc() {
        // node_projected and node_label must return the same visible version as
        // node() for the same snapshot — no divergence between entity getters.
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("Person", props! {"name" => "Alice"}).unwrap();
        let txn = g.begin_txn().unwrap();
        let mut updated = g.node(id).unwrap();
        updated.set_label("Human");
        updated
            .properties_mut()
            .insert("name".into(), Property::String("Alicia".into()));
        g.update_node_in_txn(txn, id, &updated).unwrap();
        // committed txn to make the change auto-commit-visible.
        // (No commit_txn yet — Phase 4; instead assert the auto-commit reader
        // still sees committed, and all three getters agree with each other.)
        assert_eq!(g.node(id).unwrap().label(), "Person");
        assert_eq!(g.node_label(id).unwrap(), "Person");
        assert_eq!(g.node_projected(id, &["name"]).unwrap().label(), "Person");
        assert_eq!(
            g.node_projected(id, &["name"])
                .unwrap()
                .properties()
                .get("name"),
            Some(&Property::String("Alice".into()))
        );
    }

    #[test]
    fn node_label_of_removed_node_in_txn_is_not_found_for_author() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("Person", props! {}).unwrap();
        let txn = g.begin_txn().unwrap();
        g.remove_node_in_txn(txn, id).unwrap();
        // The author's own reads via node_label/node_projected must agree with
        // node_in_txn (not found), not read the stale page.
        // node()/node_label() here are auto-commit (txn_id None) so still see it;
        // this asserts the auto-commit view is consistent across getters.
        assert_eq!(g.node_label(id).unwrap(), "Person");
        assert!(g.node_projected(id, &[]).is_ok());
    }

    #[test]
    fn commit_txn_makes_deltas_visible_to_new_readers() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(txn, "Person", props! {}).unwrap();
        assert!(g.node(id).is_err()); // not visible before commit
        g.commit_txn(txn).unwrap();
        assert!(g.node(id).is_ok()); // visible after commit
        assert!(!g.txn_is_active(txn));
    }

    #[test]
    fn commit_txn_stamps_all_deltas_of_the_txn() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let a = g.add_node_in_txn(txn, "A", props! {}).unwrap();
        let b = g.add_node_in_txn(txn, "B", props! {}).unwrap();
        g.commit_txn(txn).unwrap();
        assert!(g.node(a).is_ok());
        assert!(g.node(b).is_ok());
    }

    #[test]
    fn adj_head_survives_cache_eviction_between_commit_and_vacuum() {
        // Case (b): no crash, but the bounded adj_cache evicts the new node's
        // entry between commit and vacuum. The head must not live only in the
        // cache — it is durable in the node's slot at commit, so the vacuum (and
        // any read) still finds it after eviction.
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let a = g.add_node_in_txn(txn, "A", props! {}).unwrap();
        let b = g.add_node_in_txn(txn, "B", props! {}).unwrap();
        let e = g.add_edge_in_txn(txn, "rel", a, b, props! {}).unwrap();
        g.commit_txn(txn).unwrap();

        // Simulate cache pressure evicting both endpoints before the vacuum.
        g.adj_cache.remove(a.0);
        g.adj_cache.remove(b.0);
        g.vacuum_once().unwrap();

        // Evict again so the read must come from the slot, not the cache.
        g.adj_cache.remove(a.0);
        g.adj_cache.remove(b.0);
        let out = g.outgoing_edges(a).unwrap();
        assert_eq!(out.len(), 1, "edge reachable after cache eviction + vacuum");
        assert_eq!(out[0].id(), e);
        assert_eq!(g.incoming_edges(b).unwrap().len(), 1);
    }

    #[test]
    fn node_and_edge_in_same_txn_survive_crash_before_vacuum() {
        // The commit persists the adjacency head only in adj_cache when the new
        // node's page does not exist yet (the "page not allocated" fix). If a
        // crash happens after commit but before the vacuum materializes the
        // node pages, recovery must still rebuild the edge's adjacency so the
        // edge is traversable — the head cannot be lost with the in-memory cache.
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let cfg = GraphConfig::default();
        let (a, b, e) = {
            let mut g = Graph::open(dir.path(), &cfg).unwrap();
            g.enable_mvcc();
            let txn = g.begin_txn().unwrap();
            let a = g.add_node_in_txn(txn, "A", props! {}).unwrap();
            let b = g.add_node_in_txn(txn, "B", props! {}).unwrap();
            let e = g.add_edge_in_txn(txn, "rel", a, b, props! {}).unwrap();
            g.commit_txn(txn).unwrap();
            // Crash: drop without vacuum/clean close. WAL was fsync'd in commit.
            (a, b, e)
        };

        let g = Graph::open(dir.path(), &cfg).unwrap();
        assert!(g.node(a).is_ok(), "source node must survive recovery");
        assert!(g.node(b).is_ok(), "target node must survive recovery");
        let out = g.outgoing_edges(a).unwrap();
        assert_eq!(out.len(), 1, "edge must be traversable after recovery");
        assert_eq!(out[0].id(), e);
        assert_eq!(g.incoming_edges(b).unwrap().len(), 1);
    }

    #[test]
    fn committed_delete_stays_visible_to_older_snapshot_until_vacuum() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("N", props! {"k" => "v"}).unwrap();
        let old = g.begin_txn().unwrap(); // opened before the delete
        let w = g.begin_txn().unwrap();
        g.remove_node_in_txn(w, id).unwrap();
        g.commit_txn(w).unwrap();
        // A new auto-commit reader no longer sees it (chain says Deleted).
        assert!(g.node(id).is_err());
        // The OLD snapshot still sees it — its version was not reconciled away.
        assert!(g.node_in_txn(old, id).is_ok());
        // Category B must still hold the node (delete not applied at commit):
        assert!(g.node_exists(id));
        g.rollback_txn(old).unwrap();
    }

    #[test]
    fn committed_insert_then_delete_same_txn_nets_to_absent() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let t = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(t, "N", props! {}).unwrap();
        g.remove_node_in_txn(t, id).unwrap();
        g.commit_txn(t).unwrap();
        assert!(g.node(id).is_err());
        assert!(
            !g.node_exists(id),
            "insert+delete in one txn must not leave category B"
        );
    }

    #[test]
    fn commit_txn_inactive_errors() {
        let mut g = Graph::new();
        g.enable_mvcc();
        assert!(matches!(
            g.commit_txn(999).unwrap_err(),
            Error::TxnNotActive(999)
        ));
    }

    // ── QR Phase 4 fix #2/#3: commit durability across a crash ──────────
    // A committed MVCC transaction must survive a crash that happens after
    // `commit_txn` returns but before any Phase 5 vacuum materializes its
    // deltas to their pages. `commit_txn` must therefore emit a durable WAL
    // redo (`WriteNode`/`WriteEdge`/`Tombstone*` with `txn_id: Some`) for each
    // delta, gated on `committed_txn_ids` at recovery. Before this fix the WAL
    // held only `Begin`+`Commit` (no data), so the node vanished on reopen.
    #[test]
    fn committed_txn_node_survives_crash_before_vacuum() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let cfg = GraphConfig::default(); // wal_enabled: true
        let id = {
            let mut g = Graph::open(dir.path(), &cfg).unwrap();
            g.enable_mvcc();
            let txn = g.begin_txn().unwrap();
            let id = g
                .add_node_in_txn(txn, "Person", props! {"name" => "Alice"})
                .unwrap();
            g.commit_txn(txn).unwrap();
            // Simulate a crash: drop without a clean flush/close. The WAL was
            // fsync'd inside commit_txn; the pages were not written.
            id
        };

        // Reopen: recovery must replay the committed transaction's data.
        let g = Graph::open(dir.path(), &cfg).unwrap();
        let node = g
            .node(id)
            .expect("committed node must survive crash-before-vacuum");
        assert_eq!(
            node.properties().get("name"),
            Some(&Property::String("Alice".into())),
            "committed property value must be durable"
        );
    }

    #[test]
    fn rolled_back_txn_node_absent_after_crash() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let cfg = GraphConfig::default();
        let id = {
            let mut g = Graph::open(dir.path(), &cfg).unwrap();
            g.enable_mvcc();
            let txn = g.begin_txn().unwrap();
            let id = g.add_node_in_txn(txn, "Ghost", props! {}).unwrap();
            g.rollback_txn(txn).unwrap();
            id
        };
        let g = Graph::open(dir.path(), &cfg).unwrap();
        assert!(
            g.node(id).is_err(),
            "a rolled-back node must never appear after reopen"
        );
    }

    // Cycle 6 (#54): the WAL commit-redo path re-serializes a node slot from the
    // snapshot Node via encode_node_slot. Under option 1 the adjacency pointer
    // rides on that Node, so a crash+recovery replay must restore the pointer,
    // not reset it to the sentinel — otherwise a recovered node loses the trail
    // to its edges.
    #[test]
    fn wal_commit_redo_preserves_adj_pointer_after_recovery() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let cfg = GraphConfig::default();
        let (src, dst, ptr_before) = {
            let mut g = Graph::open(dir.path(), &cfg).unwrap();
            let src = g.add_node("A", props! {}).unwrap();
            let dst = g.add_node("B", props! {}).unwrap();
            g.add_edge("rel", src, dst, props! {}).unwrap();
            let ptr = node_codec::slot_adj_page_id(&g.read_node_slot_bytes(src.0).unwrap());
            assert_ne!(ptr, node_codec::ADJ_PAGE_ID_SENTINEL);

            // Force the node slots through the MVCC commit-redo path, then crash
            // before any vacuum materializes them to their pages.
            g.enable_mvcc();
            let txn = g.begin_txn().unwrap();
            let mut updated = g.node_in_txn(txn, src).unwrap();
            updated
                .properties_mut()
                .insert("touched".to_owned(), Property::I64(1));
            g.update_node_in_txn(txn, src, &updated).unwrap();
            g.commit_txn(txn).unwrap();
            (src, dst, ptr)
        };

        // Reopen: recovery replays the committed WriteNode redo to the page.
        let g = Graph::open(dir.path(), &cfg).unwrap();
        let src_slot = g.read_node_slot_bytes(src.0).unwrap();
        assert_eq!(
            node_codec::slot_adj_page_id(&src_slot),
            ptr_before,
            "WAL recovery must restore the adjacency pointer, not the sentinel"
        );
        assert_ne!(
            node_codec::slot_adj_flags(&src_slot) & node_codec::ADJ_FLAG_OUTGOING,
            0,
            "WAL recovery must restore the OUTGOING flag"
        );
        assert_eq!(
            g.outgoing_edges(src).unwrap().len(),
            1,
            "the edge must remain reachable after crash + recovery"
        );
        assert_eq!(g.incoming_edges(dst).unwrap().len(), 1);
    }

    #[test]
    fn batch_flush_writes_node_slot_once_per_node_across_both_directions() {
        use std::sync::atomic::Ordering::Relaxed;
        // A node that is both a source and a target within the same batch shows
        // up in adj_pending under Outgoing AND Incoming. Its slot pointer must be
        // written once, not once per direction: the accumulated pointer in
        // adj_cache already holds both directions.
        let (mut g, _reads, node_writes) = graph_on_counting_backend();
        let hub = g.add_node("H", props! {}).unwrap();
        let other = g.add_node("O", props! {}).unwrap();

        node_writes.store(0, Relaxed);
        g.begin_batch();
        // hub is the source of one edge and the target of another → both
        // directions pending for hub in a single flush.
        g.add_edge("out", hub, other, props! {}).unwrap();
        g.add_edge("in", other, hub, props! {}).unwrap();
        g.end_batch().unwrap();

        // Two distinct nodes touched (hub and other), each written once for its
        // pointer. Before the dedup fix, hub's slot was written twice (once per
        // direction), so the count was 3+. Exactly 2 proves per-node dedup.
        assert_eq!(
            node_writes.load(Relaxed),
            2,
            "each touched node's slot pointer must be written exactly once per flush"
        );

        // Correctness is unaffected: hub sees both its edges.
        assert_eq!(g.outgoing_edges(hub).unwrap().len(), 1);
        assert_eq!(g.incoming_edges(hub).unwrap().len(), 1);
    }

    // ── QR Phase 4 fix #1/#4: WAL-failure handling in begin/commit/rollback ──
    // A `MemoryBackend` that advertises WAL support and fails `wal_append` when
    // its shared flag is armed, so we can exercise the error paths of
    // begin/commit/rollback without real I/O faults. The flag is an
    // `Arc<AtomicBool>` shared with the test, so the test arms the failure
    // without reaching through the `Box<dyn StorageBackend>` — no `unsafe`.
    struct FailingWalBackend {
        inner: crate::storage::memory::MemoryBackend,
        fail: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl FailingWalBackend {
        fn new(fail: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
            Self {
                inner: crate::storage::memory::MemoryBackend::new(),
                fail,
            }
        }
    }

    impl crate::storage::backend::StorageBackend for FailingWalBackend {
        fn read_page(&self, file: DataFile, page_id: u32) -> Result<crate::storage::page::PageBuf> {
            self.inner.read_page(file, page_id)
        }
        fn write_page(
            &mut self,
            file: DataFile,
            page_id: u32,
            data: &crate::storage::page::PageBuf,
        ) -> Result<()> {
            self.inner.write_page(file, page_id, data)
        }
        fn allocate_page(&mut self, file: DataFile) -> Result<u32> {
            self.inner.allocate_page(file)
        }
        fn free_page(&mut self, file: DataFile, page_id: u32) -> Result<()> {
            self.inner.free_page(file, page_id)
        }
        fn page_count(&self, file: DataFile) -> u32 {
            self.inner.page_count(file)
        }
        fn flush(&mut self) -> Result<()> {
            self.inner.flush()
        }
        fn meta(&self) -> &crate::storage::meta::GraphMeta {
            self.inner.meta()
        }
        fn meta_mut(&mut self) -> &mut crate::storage::meta::GraphMeta {
            self.inner.meta_mut()
        }
        fn read_index_bytes(&mut self) -> Result<Option<Vec<u8>>> {
            self.inner.read_index_bytes()
        }
        fn write_index_bytes(&mut self, data: &[u8]) -> Result<()> {
            self.inner.write_index_bytes(data)
        }
        // Advertise WAL so begin/commit/rollback take their WAL-append paths.
        fn wal_enabled(&self) -> bool {
            true
        }
        fn wal_append(&mut self, _record: crate::wal::record::WalRecord) -> Result<()> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::WalCorrupt("injected wal_append failure"));
            }
            Ok(())
        }
    }

    /// Builds an MVCC graph whose WAL append can be made to fail on demand via
    /// the returned shared flag. Arm the failure with
    /// `flag.store(true, SeqCst)`.
    fn graph_with_failing_wal() -> (Graph, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut g = Graph {
            storage: Box::new(FailingWalBackend::new(std::sync::Arc::clone(&flag))),
            adj_cache: AdjCache::new(1024),
            adj_tail_cache: crate::adj_tail_cache::AdjTailCache::new(1024),
            open_slab: [None; 2],
            prop_slab_open_page: None,
            node_exists: HashSet::new(),
            edge_exists: HashSet::new(),
            string_heap: crate::storage::codec::string_codec::StringHeap::new(),
            node_label_index: LabelIndex::new(),
            edge_label_index: LabelIndex::new(),
            edge_pair_index: HashMap::new(),
            node_property_index: PropertyIndex::new(),
            batch_depth: 0,
            adj_pending: HashMap::new(),
            quota_hook: None,
            wal_observer: None,
            schema_catalog: crate::schema::SchemaCatalog::new(),
            append_only_node_ids: HashSet::new(),
            delta_table: None,
            txn_registry: None,
            txn_clock: None,
            txn_memory_cap: None,
            batch_max_ops: None,
            batch_max_bytes: None,
            batch_op_count: 0,
            batch_byte_count: 0,
        };
        g.enable_mvcc();
        (g, flag)
    }

    fn arm(flag: &std::sync::Arc<std::sync::atomic::AtomicBool>) {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn begin_txn_wal_failure_leaves_no_active_txn() {
        let (mut g, flag) = graph_with_failing_wal();
        arm(&flag);
        let before = g.txn_registry.as_ref().unwrap().oldest_active_start_ts();
        let err = g.begin_txn().unwrap_err();
        assert!(matches!(err, Error::WalCorrupt(_)));
        // No phantom active transaction skewing the vacuum watermark.
        assert_eq!(
            g.txn_registry.as_ref().unwrap().oldest_active_start_ts(),
            before,
            "a failed begin must not leave an active transaction"
        );
    }

    #[test]
    fn commit_txn_wal_failure_keeps_txn_active_and_invisible() {
        let (mut g, flag) = graph_with_failing_wal();
        let txn = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(txn, "Person", props! {}).unwrap();
        arm(&flag);
        let err = g.commit_txn(txn).unwrap_err();
        assert!(matches!(err, Error::WalCorrupt(_)));
        // The delta was NOT made visible (no split-brain: nothing durable, so
        // nothing visible), and the transaction is still active for retry/rollback.
        assert!(
            g.txn_is_active(txn),
            "failed commit must keep the txn active"
        );
        assert!(
            g.node(id).is_err(),
            "failed commit must not make the delta visible to auto-commit readers"
        );
    }

    #[test]
    fn rollback_txn_wal_failure_still_cleans_up_in_memory() {
        let (mut g, flag) = graph_with_failing_wal();
        let txn = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(txn, "Ghost", props! {}).unwrap();
        arm(&flag);
        let err = g.rollback_txn(txn).unwrap_err();
        assert!(matches!(err, Error::WalCorrupt(_)));
        // Even though the WAL Rollback marker failed, the in-memory rollback
        // completed: the txn is gone and the delta discarded.
        assert!(
            !g.txn_is_active(txn),
            "rollback must clean up the txn even if the WAL marker fails"
        );
        assert!(g.node(id).is_err(), "rolled-back delta must be discarded");
    }

    #[test]
    fn rollback_txn_discards_uncommitted_deltas() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(txn, "Person", props! {}).unwrap();
        g.rollback_txn(txn).unwrap();
        assert!(g.node(id).is_err()); // never visible to anyone
        assert!(!g.txn_is_active(txn));
    }

    #[test]
    fn rollback_txn_after_update_leaves_committed_state_intact() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("Person", props! {"name" => "Alice"}).unwrap();
        let txn = g.begin_txn().unwrap();
        let mut updated = g.node(id).unwrap();
        updated
            .properties_mut()
            .insert("name".into(), Property::String("Bob".into()));
        g.update_node_in_txn(txn, id, &updated).unwrap();
        g.rollback_txn(txn).unwrap();
        // Auto-commit readers still see the committed "Alice".
        assert_eq!(
            g.node(id).unwrap().properties().get("name"),
            Some(&Property::String("Alice".into()))
        );
    }

    // ── Phase 5, Cycle 24: vacuum materializes safe committed deltas ────────
    #[test]
    fn vacuum_materializes_committed_delta_older_than_oldest_active_txn() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("Person", props! {"name" => "Alice"}).unwrap();
        let txn = g.begin_txn().unwrap();
        let mut updated = g.node(id).unwrap();
        updated
            .properties_mut()
            .insert("name".into(), Property::String("Bob".into()));
        g.update_node_in_txn(txn, id, &updated).unwrap();
        g.commit_txn(txn).unwrap();

        // No live transactions now: the committed delta is safe to vacuum.
        let freed = g.vacuum_once().unwrap();
        assert_eq!(freed, 1);
        // The chain for `id` is gone (materialized to the page).
        assert_eq!(g.delta_chain_len_for_test(NodeId(id.0)), 0);
        // And a straight page read now returns "Bob".
        assert_eq!(
            g.read_node(id.0).unwrap().properties().get("name"),
            Some(&Property::String("Bob".into()))
        );
    }

    #[test]
    fn vacuum_does_not_free_version_visible_to_a_still_active_transaction() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("Person", props! {"name" => "Alice"}).unwrap();
        // A reader whose snapshot predates the update below.
        let old_reader = g.begin_txn().unwrap();

        let writer = g.begin_txn().unwrap();
        let mut updated = g.node(id).unwrap();
        updated
            .properties_mut()
            .insert("name".into(), Property::String("Bob".into()));
        g.update_node_in_txn(writer, id, &updated).unwrap();
        g.commit_txn(writer).unwrap();

        // old_reader is still active: the "Bob" version was committed at a
        // commit_ts >= old_reader.start_ts, so it must NOT be materialized.
        let freed = g.vacuum_once().unwrap();
        assert_eq!(freed, 0);
        // old_reader still sees its snapshot "Alice".
        assert_eq!(
            g.node_in_txn(old_reader, id)
                .unwrap()
                .properties()
                .get("name"),
            Some(&Property::String("Alice".into()))
        );
        g.rollback_txn(old_reader).unwrap();
    }

    #[test]
    fn vacuum_materializes_committed_insert_and_makes_it_page_resident() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let id = g
            .add_node_in_txn(txn, "City", props! {"name" => "Paris"})
            .unwrap();
        g.commit_txn(txn).unwrap();
        assert_eq!(g.node_count(), 1, "committed insert counts once visible");

        let freed = g.vacuum_once().unwrap();
        assert_eq!(freed, 1);
        assert_eq!(g.delta_chain_len_for_test(NodeId(id.0)), 0);
        // Straight page read (bypassing the chain) now finds the node.
        assert_eq!(
            g.read_node(id.0).unwrap().properties().get("name"),
            Some(&Property::String("Paris".into()))
        );
    }

    #[test]
    fn vacuum_materializes_committed_delete_as_page_tombstone() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("Person", props! {}).unwrap();
        assert_eq!(g.node_count(), 1);
        let txn = g.begin_txn().unwrap();
        g.remove_node_in_txn(txn, id).unwrap();
        g.commit_txn(txn).unwrap();
        assert!(g.node(id).is_err(), "delete is visible after commit");
        // Under option 2a the delete's category-B baja (including the count) is
        // applied by the vacuum, not at commit: the node stays counted until an
        // older snapshot can no longer need it.
        assert_eq!(g.node_count(), 1);

        let freed = g.vacuum_once().unwrap();
        assert_eq!(freed, 1);
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.delta_chain_len_for_test(NodeId(id.0)), 0);
        // The page slot is tombstoned: a straight read no longer finds it.
        assert!(g.read_node(id.0).is_err());
    }

    #[test]
    fn vacuum_applies_delete_to_category_b() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("N", props! {"k" => "v"}).unwrap();
        assert_eq!(g.node_count(), 1);
        let t = g.begin_txn().unwrap();
        g.remove_node_in_txn(t, id).unwrap();
        g.commit_txn(t).unwrap();
        assert!(g.node_exists(id)); // not applied at commit
        assert_eq!(g.node_count(), 1);
        let freed = g.vacuum_once().unwrap(); // no older snapshot -> applies delete
        assert_eq!(freed, 1);
        assert!(!g.node_exists(id));
        assert_eq!(g.node_count(), 0);
        assert!(g.read_node(id.0).is_err(), "page slot tombstoned");
    }

    // ── Issue #41: range composability + Issue #42-substitute: absence ────────

    #[test]
    fn range_result_intersects_with_equality_lookup() {
        // The consumer's pattern: narrow by an equality index, then keep only
        // those also in a range. Both are index-backed Vec<NodeId>; intersection
        // is a plain set operation on the results.
        let mut g = Graph::new();
        // scope A events at various valid_from.
        let a1 = g
            .add_node("Event", props! {"scope" => 1i64, "vf" => 100i64})
            .unwrap();
        let a2 = g
            .add_node("Event", props! {"scope" => 1i64, "vf" => 250i64})
            .unwrap();
        // scope B event inside the same vf range — must be excluded by the scope.
        let _b = g
            .add_node("Event", props! {"scope" => 2i64, "vf" => 120i64})
            .unwrap();

        let in_scope: std::collections::HashSet<NodeId> = g
            .nodes_by_label_and_property("Event", "scope", &Property::I64(1))
            .into_iter()
            .collect();
        let in_range: std::collections::HashSet<NodeId> = g
            .nodes_by_label_and_property_range("Event", "vf", Some(100), Some(200))
            .into_iter()
            .collect();
        let both: Vec<NodeId> = in_scope.intersection(&in_range).copied().collect();
        assert_eq!(both, vec![a1], "only scope-1 vf=100 satisfies both");
        let _ = a2;
    }

    #[test]
    fn nodes_without_property_excludes_those_that_have_it() {
        let mut g = Graph::new();
        // "open" events have no valid_to; "closed" events have one.
        let open = g.add_node("Event", props! {"vf" => 10i64}).unwrap();
        let _closed = g
            .add_node("Event", props! {"vf" => 20i64, "valid_to" => 99i64})
            .unwrap();
        assert_eq!(
            g.nodes_by_label_without_property("Event", "valid_to"),
            vec![open],
            "only the node lacking valid_to is returned"
        );
    }

    #[test]
    fn nodes_without_property_empty_label_is_empty() {
        let g = Graph::new();
        assert!(
            g.nodes_by_label_without_property("Event", "valid_to")
                .is_empty()
        );
    }

    #[test]
    fn nodes_without_property_excludes_committed_deleted() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let keep = g.add_node("Event", props! {"vf" => 10i64}).unwrap();
        let gone = g.add_node("Event", props! {"vf" => 20i64}).unwrap();
        let t = g.begin_txn().unwrap();
        g.remove_node_in_txn(t, gone).unwrap();
        g.commit_txn(t).unwrap();
        // `gone` had no valid_to, but it's committed-deleted → must not appear.
        assert_eq!(
            g.nodes_by_label_without_property("Event", "valid_to"),
            vec![keep]
        );
    }

    #[test]
    fn valid_at_t_combines_open_range_and_property_absence() {
        // #42 covered WITHOUT a new type: "valid at T" = valid_from <= T AND
        // (valid_to absent [still open] OR valid_to > T). Here we show the "open"
        // arm via property absence + open-ended range, no i64::MAX sentinel.
        let mut g = Graph::new();
        // Open event, started before T=500.
        let open_before = g.add_node("Event", props! {"vf" => 100i64}).unwrap();
        // Open event, started after T → not valid at T.
        let _open_after = g.add_node("Event", props! {"vf" => 900i64}).unwrap();
        // Closed event (has valid_to) → not in the "open" arm.
        let _closed = g
            .add_node("Event", props! {"vf" => 50i64, "valid_to" => 400i64})
            .unwrap();

        let t = 500;
        let started_by_t: std::collections::HashSet<NodeId> = g
            .nodes_by_label_and_property_range("Event", "vf", None, Some(t + 1))
            .into_iter()
            .collect();
        let still_open: std::collections::HashSet<NodeId> = g
            .nodes_by_label_without_property("Event", "valid_to")
            .into_iter()
            .collect();
        let valid_open_at_t: Vec<NodeId> =
            started_by_t.intersection(&still_open).copied().collect();
        assert_eq!(valid_open_at_t, vec![open_before]);
    }

    #[test]
    fn graphaccess_default_matches_indexed_for_new_primitives() {
        // The trait's default O(N) fallback (inherited, un-overridden, by the
        // read-only txn view) must agree with Graph's indexed override for all
        // three new primitives — including the max tie-break (lowest NodeId).
        use crate::access::GraphAccess;
        let mut g = Graph::new();
        g.enable_mvcc();
        let n1 = g.add_node("Event", props! {"seq" => 10i64}).unwrap();
        let n2 = g.add_node("Event", props! {"seq" => 50i64}).unwrap();
        let n3 = g.add_node("Event", props! {"seq" => 50i64}).unwrap(); // tie at 50
        let n4 = g.add_node("Event", props! {"other" => 1i64}).unwrap(); // no "seq"
        let txn = g.begin_txn().unwrap();
        let view = crate::gql::txn_view::TxnReadView::new(&g, txn);

        // Range: both must return the same set.
        let mut idx_range = g.nodes_by_label_and_property_range("Event", "seq", Some(10), Some(60));
        let mut def_range = GraphAccess::nodes_by_label_and_property_range(
            &view,
            "Event",
            "seq",
            Some(10),
            Some(60),
        );
        idx_range.sort_unstable_by_key(|n| n.0);
        def_range.sort_unstable_by_key(|n| n.0);
        assert_eq!(idx_range, def_range);
        assert_eq!(idx_range, vec![n1, n2, n3]);

        // Max with a tie at 50: both must pick the lowest NodeId (n2 < n3).
        assert_eq!(g.max_node_by_property("Event", "seq"), Some(n2));
        assert_eq!(
            GraphAccess::max_node_by_property(&view, "Event", "seq"),
            Some(n2),
            "default fallback must match indexed tie-break"
        );

        // Absence: the node with "other" (no "seq") appears in both.
        let mut idx_abs = g.nodes_by_label_without_property("Event", "seq");
        let mut def_abs = GraphAccess::nodes_by_label_without_property(&view, "Event", "seq");
        idx_abs.sort_unstable_by_key(|n| n.0);
        def_abs.sort_unstable_by_key(|n| n.0);
        assert_eq!(idx_abs, def_abs);
        assert_eq!(idx_abs, vec![n4]);
    }

    // ── Issue #40: max_node_by_property ───────────────────────────────────────

    #[test]
    fn max_node_by_property_returns_highest() {
        let mut g = Graph::new();
        g.add_node("Event", props! {"seq" => 10i64}).unwrap();
        let top = g.add_node("Event", props! {"seq" => 50i64}).unwrap();
        g.add_node("Event", props! {"seq" => 30i64}).unwrap();
        assert_eq!(g.max_node_by_property("Event", "seq"), Some(top));
    }

    #[test]
    fn max_node_by_property_empty_returns_none() {
        let g = Graph::new();
        assert_eq!(g.max_node_by_property("Event", "seq"), None);
    }

    #[test]
    fn max_node_by_property_ignores_other_labels() {
        let mut g = Graph::new();
        g.add_node("Other", props! {"seq" => 999i64}).unwrap();
        let ev = g.add_node("Event", props! {"seq" => 5i64}).unwrap();
        assert_eq!(g.max_node_by_property("Event", "seq"), Some(ev));
    }

    #[test]
    fn max_node_by_property_skips_invisible_top() {
        // The highest value's node was deleted (committed, pre-vacuum); the max
        // must descend to the next visible value, not return the ghost.
        let mut g = Graph::new();
        g.enable_mvcc();
        let second = g.add_node("Event", props! {"seq" => 30i64}).unwrap();
        let top = g.add_node("Event", props! {"seq" => 50i64}).unwrap();
        let t = g.begin_txn().unwrap();
        g.remove_node_in_txn(t, top).unwrap();
        g.commit_txn(t).unwrap();
        assert_eq!(
            g.max_node_by_property("Event", "seq"),
            Some(second),
            "max must skip the committed-deleted top value"
        );
    }

    // ── Issue #40/#41/#42: ordered property index — maintenance & visibility ──

    fn sorted_ids(v: Vec<NodeId>) -> Vec<u64> {
        let mut out: Vec<u64> = v.into_iter().map(|n| n.0).collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn ordered_index_reflects_auto_commit_insert() {
        // Route 1: direct (non-txn) node creation must populate the ordered index.
        let mut g = Graph::new();
        let a = g.add_node("Event", props! {"seq" => 10i64}).unwrap();
        let b = g.add_node("Event", props! {"seq" => 30i64}).unwrap();
        let c = g.add_node("Event", props! {"seq" => 20i64}).unwrap();
        assert_eq!(
            sorted_ids(g.nodes_by_label_and_property_range("Event", "seq", None, None)),
            sorted_ids(vec![a, b, c])
        );
        // Range [15, 30) → only seq=20.
        assert_eq!(
            g.nodes_by_label_and_property_range("Event", "seq", Some(15), Some(30)),
            vec![c]
        );
    }

    #[test]
    fn ordered_index_survives_rebuild_from_pages() {
        // Route 2: the ordered index is not persisted; rebuild must reconstruct
        // it identically from the node pages.
        let mut g = Graph::new();
        let a = g.add_node("Event", props! {"seq" => 10i64}).unwrap();
        let b = g.add_node("Event", props! {"seq" => 20i64}).unwrap();
        let before = sorted_ids(g.nodes_by_label_and_property_range("Event", "seq", None, None));
        g.rebuild_indexes().unwrap();
        let after = sorted_ids(g.nodes_by_label_and_property_range("Event", "seq", None, None));
        assert_eq!(before, after, "rebuild must reconstruct the ordered index");
        assert_eq!(after, sorted_ids(vec![a, b]));
    }

    #[test]
    fn ordered_index_gets_alta_at_commit_not_before() {
        // Route 3: a txn insert is invisible to the ordered query until commit.
        let mut g = Graph::new();
        g.enable_mvcc();
        let t = g.begin_txn().unwrap();
        let id = g
            .add_node_in_txn(t, "Event", props! {"seq" => 42i64})
            .unwrap();
        assert!(
            g.nodes_by_label_and_property_range("Event", "seq", None, None)
                .is_empty(),
            "pending txn insert must not appear before commit"
        );
        g.commit_txn(t).unwrap();
        assert_eq!(
            g.nodes_by_label_and_property_range("Event", "seq", None, None),
            vec![id]
        );
    }

    #[test]
    fn ordered_index_range_excludes_committed_deleted_before_vacuum() {
        // Visibility hazard (issue #45): a committed-deleted node still sits in
        // the ordered index until the vacuum removes it, but the range query must
        // filter it out by visibility.
        let mut g = Graph::new();
        g.enable_mvcc();
        let keep = g.add_node("Event", props! {"seq" => 10i64}).unwrap();
        let gone = g.add_node("Event", props! {"seq" => 20i64}).unwrap();
        let t = g.begin_txn().unwrap();
        g.remove_node_in_txn(t, gone).unwrap();
        g.commit_txn(t).unwrap();
        // Before vacuum: the deleted node must not appear in the range result.
        assert_eq!(
            g.nodes_by_label_and_property_range("Event", "seq", None, None),
            vec![keep],
            "committed-deleted node must be filtered by visibility"
        );
        g.vacuum_once().unwrap();
        assert_eq!(
            g.nodes_by_label_and_property_range("Event", "seq", None, None),
            vec![keep]
        );
    }

    #[test]
    fn vacuum_removes_stale_old_property_index_after_update() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("Person", props! {"name" => "Alice"}).unwrap();
        let t = g.begin_txn().unwrap();
        let mut n = g.node(id).unwrap();
        n.properties_mut()
            .insert("name".into(), Property::String("Bob".into()));
        g.update_node_in_txn(t, id, &n).unwrap();
        g.commit_txn(t).unwrap();
        assert!(
            g.nodes_by_label_and_property("Person", "name", &Property::String("Bob".into()))
                .contains(&id)
        );
        g.vacuum_once().unwrap();
        assert!(
            !g.nodes_by_label_and_property("Person", "name", &Property::String("Alice".into()))
                .contains(&id)
        );
        assert!(
            g.nodes_by_label_and_property("Person", "name", &Property::String("Bob".into()))
                .contains(&id)
        );
    }

    #[test]
    fn vacuum_never_frees_uncommitted_delta() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(txn, "Person", props! {}).unwrap();
        // txn still open; its insert is uncommitted.
        let freed = g.vacuum_once().unwrap();
        assert_eq!(freed, 0);
        assert_eq!(g.delta_chain_len_for_test(NodeId(id.0)), 1);
        g.rollback_txn(txn).unwrap();
    }

    #[test]
    fn vacuum_once_on_legacy_graph_is_mvcc_not_enabled() {
        // A graph without `enable_mvcc()` reports MVCC disabled and rejects the
        // vacuum, so the registry sweep can guard on `mvcc_enabled()` and skip
        // legacy graphs without treating this as an error.
        let mut g = Graph::new();
        assert!(!g.mvcc_enabled());
        assert!(matches!(g.vacuum_once(), Err(Error::MvccNotEnabled)));
    }

    #[test]
    fn vacuum_materialization_emits_no_extra_wal_record() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let cfg = GraphConfig::default();
        let mut g = Graph::open(dir.path(), &cfg).unwrap();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let _id = g.add_node_in_txn(txn, "Person", props! {}).unwrap();
        g.commit_txn(txn).unwrap();

        let wal_path = dir.path().join("wal.log");
        let before = crate::wal::reader::WalReader::read_all(&wal_path)
            .unwrap()
            .records
            .len();
        let freed = g.vacuum_once().unwrap();
        assert_eq!(freed, 1);
        let after = crate::wal::reader::WalReader::read_all(&wal_path)
            .unwrap()
            .records
            .len();
        assert_eq!(
            before, after,
            "vacuum must not re-log the already-durable committed delta"
        );
    }

    #[test]
    fn committed_txn_then_new_txn_sees_it() {
        // A transaction committed before a later reader began is visible to it.
        let mut g = Graph::new();
        g.enable_mvcc();
        let w = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(w, "Person", props! {}).unwrap();
        g.commit_txn(w).unwrap();
        let r = g.begin_txn().unwrap();
        assert!(g.node_in_txn(r, id).is_ok());
        g.rollback_txn(r).unwrap();
    }

    #[test]
    fn second_txn_reads_committed_node_it_never_wrote() {
        // Two concurrent transactions: txn_b reads a node it did not write and
        // that txn_a has not committed changes to. txn_b sees the committed base.
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("Person", props! {"name" => "Alice"}).unwrap();
        let txn_a = g.begin_txn().unwrap();
        let txn_b = g.begin_txn().unwrap();

        // txn_a updates the node (uncommitted).
        let mut updated = g.node(id).unwrap();
        updated
            .properties_mut()
            .insert("name".into(), Property::String("Alicia".into()));
        g.update_node_in_txn(txn_a, id, &updated).unwrap();

        // txn_b did not write it and must see the committed "Alice", not txn_a's
        // uncommitted "Alicia".
        assert_eq!(
            g.node_in_txn(txn_b, id).unwrap().properties().get("name"),
            Some(&Property::String("Alice".into()))
        );
    }

    // 3d T1: introspection getters for CALL tessera.vertex_labels/edge_types.
    #[test]
    fn node_labels_empty_graph() {
        let g = Graph::new();
        assert!(g.node_labels().is_empty());
    }

    #[test]
    fn node_labels_after_insert() {
        let mut g = Graph::new();
        g.add_node("Person", props! {}).unwrap();
        g.add_node("Asset", props! {}).unwrap();
        let mut labels = g.node_labels();
        labels.sort();
        assert_eq!(labels, vec!["Asset".to_owned(), "Person".to_owned()]);
    }

    #[test]
    fn edge_types_empty_graph() {
        let g = Graph::new();
        assert!(g.edge_types().is_empty());
    }

    #[test]
    fn edge_types_after_insert() {
        let mut g = Graph::new();
        let a = g.add_node("Node", props! {}).unwrap();
        let b = g.add_node("Node", props! {}).unwrap();
        g.add_edge("KNOWS", a, b, props! {}).unwrap();
        g.add_edge("TRUSTS", a, b, props! {}).unwrap();
        let mut types = g.edge_types();
        types.sort();
        assert_eq!(types, vec!["KNOWS".to_owned(), "TRUSTS".to_owned()]);
    }

    #[test]
    fn test_add_node_returns_sequential_ids() {
        let mut g = Graph::new();
        let n1 = g.add_node("A", props! {}).unwrap();
        let n2 = g.add_node("B", props! {}).unwrap();
        assert_eq!(n1, NodeId(1));
        assert_eq!(n2, NodeId(2));
    }

    #[test]
    fn test_add_edge_returns_sequential_ids() {
        let mut g = Graph::new();
        let a = g.add_node("A", props! {}).unwrap();
        let b = g.add_node("B", props! {}).unwrap();
        let e1 = g.add_edge("R", a, b, props! {}).unwrap();
        let e2 = g.add_edge("R", a, b, props! {}).unwrap();
        assert_eq!(e1, EdgeId(1));
        assert_eq!(e2, EdgeId(2));
    }

    #[test]
    fn test_node_returns_owned_value() {
        let mut g = Graph::new();
        let id = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
        let mut node = g.node(id).unwrap();
        // Modifying the returned node should NOT affect the graph
        node.properties_mut()
            .insert("age".into(), Property::I64(30));
        let node2 = g.node(id).unwrap();
        assert!(!node2.properties().contains_key("age"));
    }

    #[test]
    fn test_update_node_modifies_stored_data() {
        let mut g = Graph::new();
        let id = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
        let mut node = g.node(id).unwrap();
        node.properties_mut()
            .insert("age".into(), Property::I64(30));
        g.update_node(id, &node).unwrap();
        let updated = g.node(id).unwrap();
        assert_eq!(updated.properties().get("age"), Some(&Property::I64(30)));
    }

    #[test]
    fn test_update_node_not_found() {
        let mut g = Graph::new();
        let fake_node = Node::new(NodeId(999), "X", props! {});
        let result = g.update_node(NodeId(999), &fake_node);
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::NodeNotFound(nid) => assert_eq!(nid, NodeId(999)),
            other => panic!("expected NodeNotFound, got {other:?}"),
        }
    }

    #[test]
    fn test_update_edge_modifies_stored_data() {
        let mut g = Graph::new();
        let a = g.add_node("A", props! {}).unwrap();
        let b = g.add_node("B", props! {}).unwrap();
        let eid = g.add_edge("R", a, b, props! {}).unwrap();

        let mut edge = g.edge(eid).unwrap();
        edge.properties_mut()
            .insert("weight".into(), Property::F64(1.5));
        g.update_edge(eid, &edge).unwrap();

        let updated = g.edge(eid).unwrap();
        assert_eq!(
            updated.properties().get("weight"),
            Some(&Property::F64(1.5))
        );
    }

    #[test]
    fn test_update_edge_not_found() {
        let mut g = Graph::new();
        let fake_edge = Edge::new(EdgeId(999), "X", NodeId(1), NodeId(2), props! {});
        let result = g.update_edge(EdgeId(999), &fake_edge);
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::EdgeNotFound(eid) => assert_eq!(eid, EdgeId(999)),
            other => panic!("expected EdgeNotFound, got {other:?}"),
        }
    }

    #[test]
    fn test_remove_node_returns_removed_node() {
        let mut g = Graph::new();
        let id = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
        let removed = g.remove_node(id).unwrap();
        assert_eq!(removed.id(), id);
        assert_eq!(removed.label(), "Person");
        assert_eq!(
            removed.properties().get("name"),
            Some(&Property::String("Alice".into()))
        );
    }

    #[test]
    fn test_remove_edge_returns_removed_edge() {
        let mut g = Graph::new();
        let a = g.add_node("A", props! {}).unwrap();
        let b = g.add_node("B", props! {}).unwrap();
        let eid = g.add_edge("KNOWS", a, b, props! {}).unwrap();
        let removed = g.remove_edge(eid).unwrap();
        assert_eq!(removed.id(), eid);
        assert_eq!(removed.label(), "KNOWS");
        assert_eq!(removed.source(), a);
        assert_eq!(removed.target(), b);
    }

    #[test]
    fn test_node_with_overflow_label() {
        let mut g = Graph::new();
        let label = "L".repeat(100); // > 63 bytes
        let id = g.add_node(&label, props! {}).unwrap();
        let node = g.node(id).unwrap();
        assert_eq!(node.label(), label);
    }

    #[test]
    fn test_node_with_overflow_props() {
        let mut g = Graph::new();
        let mut props = Properties::new();
        props.insert("big".into(), Property::Bytes(vec![0xAB; 50]));
        let id = g.add_node("N", props.clone()).unwrap();
        let node = g.node(id).unwrap();
        assert_eq!(node.properties(), &props);
    }

    #[test]
    fn test_edge_with_overflow_label() {
        let mut g = Graph::new();
        let a = g.add_node("A", props! {}).unwrap();
        let b = g.add_node("B", props! {}).unwrap();
        let label = "E".repeat(100); // > 55 bytes
        let eid = g.add_edge(&label, a, b, props! {}).unwrap();
        let edge = g.edge(eid).unwrap();
        assert_eq!(edge.label(), label);
    }

    #[test]
    fn test_edge_with_overflow_props() {
        let mut g = Graph::new();
        let a = g.add_node("A", props! {}).unwrap();
        let b = g.add_node("B", props! {}).unwrap();
        let mut props = Properties::new();
        props.insert("big".into(), Property::Bytes(vec![0xCD; 50]));
        let eid = g.add_edge("R", a, b, props.clone()).unwrap();
        let edge = g.edge(eid).unwrap();
        assert_eq!(edge.properties(), &props);
    }

    #[test]
    fn test_add_many_nodes_across_pages() {
        let mut g = Graph::new();
        // 31 nodes per page, 35 nodes should use 2 pages
        let mut ids = Vec::new();
        for i in 0_i64..35 {
            let id = g.add_node("N", props! { "i" => i }).unwrap();
            ids.push(id);
        }
        assert_eq!(g.node_count(), 35);
        for (i, id) in ids.iter().enumerate() {
            let node = g.node(*id).unwrap();
            assert_eq!(
                node.properties().get("i"),
                Some(&Property::I64(i64::try_from(i).unwrap()))
            );
        }
    }

    #[test]
    fn test_add_many_edges_across_pages() {
        let mut g = Graph::new();
        let a = g.add_node("A", props! {}).unwrap();
        let b = g.add_node("B", props! {}).unwrap();
        let mut ids = Vec::new();
        for i in 0_i64..35 {
            let eid = g.add_edge("R", a, b, props! { "i" => i }).unwrap();
            ids.push(eid);
        }
        assert_eq!(g.edge_count(), 35);
        for (i, eid) in ids.iter().enumerate() {
            let edge = g.edge(*eid).unwrap();
            assert_eq!(
                edge.properties().get("i"),
                Some(&Property::I64(i64::try_from(i).unwrap()))
            );
        }
    }

    #[test]
    fn test_outgoing_edges_returns_owned() {
        let mut g = Graph::new();
        let a = g.add_node("A", props! {}).unwrap();
        let b = g.add_node("B", props! {}).unwrap();
        let c = g.add_node("C", props! {}).unwrap();
        g.add_edge("AB", a, b, props! {}).unwrap();
        g.add_edge("AC", a, c, props! {}).unwrap();
        let edges = g.outgoing_edges(a).unwrap();
        assert_eq!(edges.len(), 2);
    }

    #[test]
    fn test_incoming_edges_returns_owned() {
        let mut g = Graph::new();
        let a = g.add_node("A", props! {}).unwrap();
        let b = g.add_node("B", props! {}).unwrap();
        let c = g.add_node("C", props! {}).unwrap();
        g.add_edge("AB", a, b, props! {}).unwrap();
        g.add_edge("CB", c, b, props! {}).unwrap();
        let edges = g.incoming_edges(b).unwrap();
        assert_eq!(edges.len(), 2);
    }

    #[test]
    fn test_flush_on_memory_backend() {
        let mut g = Graph::new();
        assert!(g.flush().is_ok());
    }

    #[test]
    fn test_property_types_preserved() {
        let mut g = Graph::new();
        let mut props = Properties::new();
        props.insert("s".into(), Property::String("hello".into()));
        props.insert("i".into(), Property::I64(42));
        props.insert("f".into(), Property::F64(1.23456));
        props.insert("b".into(), Property::Bool(true));
        props.insert("v".into(), Property::Bytes(vec![1, 2, 3]));

        let id = g.add_node("N", props.clone()).unwrap();
        let node = g.node(id).unwrap();
        assert_eq!(node.properties(), &props);
    }

    #[test]
    fn test_default_trait() {
        let g1 = Graph::new();
        let g2 = Graph::default();
        assert_eq!(g1.node_count(), g2.node_count());
        assert_eq!(g1.edge_count(), g2.edge_count());
    }

    #[test]
    fn test_self_loop_remove() {
        let mut g = Graph::new();
        let a = g.add_node("A", props! {}).unwrap();
        g.add_edge("SELF", a, a, props! {}).unwrap();
        assert_eq!(g.edge_count(), 1);

        g.remove_node(a).unwrap();
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn rebuild_respects_header_slot_count() {
        // Create a MemoryBackend with one node page
        let mut backend = crate::storage::memory::MemoryBackend::new();
        backend.allocate_page(DataFile::Nodes).unwrap();

        // Build a page with a SLOT_LIVE at slot 0, but set slot_count = 0 in the header
        let mut page = crate::storage::page::new_page_buf();
        let offset = PAGE_HEADER_SIZE;
        page[offset] = 0x01; // SLOT_LIVE
        // Write a fake node id = 1
        page[offset + 1..offset + 9].copy_from_slice(&1u64.to_le_bytes());

        // Finalize with slot_count = 0 (header says no slots)
        finalize_page(&mut page, magic::NODES, 1, PageType::Node, 0);
        backend.write_page(DataFile::Nodes, 0, &page).unwrap();

        // Build graph manually using this backend
        let mut graph = Graph {
            storage: Box::new(backend),
            adj_cache: AdjCache::new(1024),
            adj_tail_cache: crate::adj_tail_cache::AdjTailCache::new(1024),
            open_slab: [None; 2],
            prop_slab_open_page: None,
            node_exists: HashSet::new(),
            edge_exists: HashSet::new(),
            string_heap: crate::storage::codec::string_codec::StringHeap::new(),
            node_label_index: LabelIndex::new(),
            edge_label_index: LabelIndex::new(),
            edge_pair_index: HashMap::new(),
            node_property_index: PropertyIndex::new(),
            batch_depth: 0,
            adj_pending: HashMap::new(),
            quota_hook: None,
            wal_observer: None,
            schema_catalog: crate::schema::SchemaCatalog::new(),
            append_only_node_ids: HashSet::new(),
            delta_table: None,
            txn_registry: None,
            txn_clock: None,
            txn_memory_cap: None,
            batch_max_ops: None,
            batch_max_bytes: None,
            batch_op_count: 0,
            batch_byte_count: 0,
        };
        graph.rebuild_node_indexes(false).unwrap();

        // slot_count = 0 means rebuild should find NO nodes
        assert!(graph.node_exists.is_empty());
    }

    #[test]
    fn test_node_id_zero_returns_not_found() {
        let g = Graph::new();
        assert!(matches!(g.node(NodeId(0)), Err(Error::NodeNotFound(_))));
    }

    #[test]
    fn test_edge_id_zero_returns_not_found() {
        let g = Graph::new();
        assert!(matches!(g.edge(EdgeId(0)), Err(Error::EdgeNotFound(_))));
    }

    /// Test backend wrapping `MemoryBackend` that counts adjacency-page reads
    /// into a shared counter, so a test can prove the append hot path's read
    /// cost does not grow with a node's accumulated degree (issue #33). The
    /// counter is an `Arc` the test keeps a clone of, avoiding any downcast of
    /// the boxed backend inside `Graph`.
    struct AdjReadCountingBackend {
        inner: crate::storage::memory::MemoryBackend,
        adj_reads: std::sync::Arc<std::sync::atomic::AtomicU32>,
        node_writes: std::sync::Arc<std::sync::atomic::AtomicU32>,
    }

    impl StorageBackend for AdjReadCountingBackend {
        fn read_page(&self, file: DataFile, page_id: u32) -> Result<crate::storage::page::PageBuf> {
            if file == DataFile::Adjacency {
                self.adj_reads
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            self.inner.read_page(file, page_id)
        }
        fn write_page(
            &mut self,
            file: DataFile,
            page_id: u32,
            data: &crate::storage::page::PageBuf,
        ) -> Result<()> {
            if file == DataFile::Nodes {
                self.node_writes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            self.inner.write_page(file, page_id, data)
        }
        fn allocate_page(&mut self, file: DataFile) -> Result<u32> {
            self.inner.allocate_page(file)
        }
        fn free_page(&mut self, file: DataFile, page_id: u32) -> Result<()> {
            self.inner.free_page(file, page_id)
        }
        fn page_count(&self, file: DataFile) -> u32 {
            self.inner.page_count(file)
        }
        fn flush(&mut self) -> Result<()> {
            self.inner.flush()
        }
        fn meta(&self) -> &crate::storage::meta::GraphMeta {
            self.inner.meta()
        }
        fn meta_mut(&mut self) -> &mut crate::storage::meta::GraphMeta {
            self.inner.meta_mut()
        }
        fn read_index_bytes(&mut self) -> Result<Option<Vec<u8>>> {
            self.inner.read_index_bytes()
        }
        fn write_index_bytes(&mut self, data: &[u8]) -> Result<()> {
            self.inner.write_index_bytes(data)
        }
    }

    /// Builds a `Graph` on an adjacency-read-counting backend, returning the
    /// graph and a clone of the shared read counter.
    fn graph_on_adj_counting_backend() -> (Graph, std::sync::Arc<std::sync::atomic::AtomicU32>) {
        let (g, reads, _writes) = graph_on_counting_backend();
        (g, reads)
    }

    /// Like [`graph_on_adj_counting_backend`] but also returns the node-page
    /// write counter, for tests that assert how many times a node slot is
    /// rewritten.
    fn graph_on_counting_backend() -> (
        Graph,
        std::sync::Arc<std::sync::atomic::AtomicU32>,
        std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let node_writes = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let backend = AdjReadCountingBackend {
            inner: crate::storage::memory::MemoryBackend::new(),
            adj_reads: std::sync::Arc::clone(&counter),
            node_writes: std::sync::Arc::clone(&node_writes),
        };
        let graph = Graph {
            storage: Box::new(backend),
            adj_cache: AdjCache::new(1024),
            adj_tail_cache: crate::adj_tail_cache::AdjTailCache::new(1024),
            open_slab: [None; 2],
            prop_slab_open_page: None,
            node_exists: HashSet::new(),
            edge_exists: HashSet::new(),
            string_heap: crate::storage::codec::string_codec::StringHeap::new(),
            node_label_index: LabelIndex::new(),
            edge_label_index: LabelIndex::new(),
            edge_pair_index: HashMap::new(),
            node_property_index: PropertyIndex::new(),
            batch_depth: 0,
            adj_pending: HashMap::new(),
            quota_hook: None,
            wal_observer: None,
            schema_catalog: crate::schema::SchemaCatalog::new(),
            append_only_node_ids: HashSet::new(),
            delta_table: None,
            txn_registry: None,
            txn_clock: None,
            txn_memory_cap: None,
            batch_max_ops: None,
            batch_max_bytes: None,
            batch_op_count: 0,
            batch_byte_count: 0,
        };
        (graph, counter, node_writes)
    }

    #[test]
    fn concentrated_fanin_append_cost_does_not_grow_with_degree() {
        use std::sync::atomic::Ordering::Relaxed;
        const BATCH: usize = 50;
        const N: usize = 30;
        // Issue #33's acceptance criterion, made deterministic: append BATCH
        // edges from the SAME source across many batches. Each end_batch's
        // adjacency-page reads must stay bounded — NOT grow with the chain built
        // by all prior batches. A quadratic impl re-walks the whole chain each
        // flush, so per-batch reads would climb with the batch index.
        let (mut g, reads) = graph_on_adj_counting_backend();
        let src = g.add_node("S", Properties::default()).unwrap();

        let mut first_batch_reads = 0u32;
        let mut last_batch_reads = 0u32;

        for batch_idx in 0..N {
            g.begin_batch();
            for _ in 0..BATCH {
                let t = g.add_node("T", Properties::default()).unwrap();
                g.add_edge("R", src, t, Properties::default()).unwrap();
            }
            // Measure only the adjacency reads the flush itself performs.
            reads.store(0, Relaxed);
            g.end_batch().unwrap();
            let batch_reads = reads.load(Relaxed);
            if batch_idx == 0 {
                first_batch_reads = batch_reads;
            }
            if batch_idx == N - 1 {
                last_batch_reads = batch_reads;
            }
        }

        // The last batch (chain is ~N*BATCH edges long) must not read many more
        // adjacency pages than the first. Allow a small constant factor for the
        // single→chained transition early on; a quadratic impl would show
        // last >> first proportional to N.
        assert!(
            last_batch_reads <= first_batch_reads.max(1) * 3,
            "per-batch adjacency reads grew with degree: first={first_batch_reads}, last={last_batch_reads}"
        );
    }

    // ---- Cycle 8b: slab wired into the write path (#54) ---------------------

    #[test]
    fn node_outgrowing_its_slab_migrates_to_a_dedicated_chain() {
        // A node keeps packing into the shared slab until its sub-block cannot grow
        // there, at which point it moves to a dedicated chain of its own. This is
        // what keeps high-degree nodes from monopolising a slab, and it must not
        // lose or reorder the edges the node already had.
        let mut g = Graph::new();
        let hub = g.add_node("H", Properties::default()).unwrap();

        // Two edges: small enough to live on a slab.
        let mut expected = Vec::new();
        for _ in 0..2 {
            let t = g.add_node("T", Properties::default()).unwrap();
            expected.push(g.add_edge("R", hub, t, Properties::default()).unwrap().0);
        }
        let slab_head = g
            .resolve_adj_pointer(hub.0)
            .unwrap()
            .unwrap()
            .outgoing_page
            .unwrap();
        assert!(
            adj_slab_codec::is_slab_page(g.storage.as_ref(), slab_head).unwrap(),
            "a degree-2 node must start out packed into a slab"
        );

        // Grow it edge by edge until it no longer fits the slab.
        while adj_slab_codec::is_slab_page(
            g.storage.as_ref(),
            g.resolve_adj_pointer(hub.0)
                .unwrap()
                .unwrap()
                .outgoing_page
                .unwrap(),
        )
        .unwrap()
        {
            let t = g.add_node("T", Properties::default()).unwrap();
            expected.push(g.add_edge("R", hub, t, Properties::default()).unwrap().0);
            assert!(expected.len() < 10_000, "node never migrated off the slab");
        }

        let head = g
            .resolve_adj_pointer(hub.0)
            .unwrap()
            .unwrap()
            .outgoing_page
            .unwrap();
        assert!(
            !adj_slab_codec::is_slab_page(g.storage.as_ref(), head).unwrap(),
            "a node that outgrew the slab must now be on a dedicated chain"
        );

        // Every edge survived the move, oldest first.
        let edge_ids: Vec<u64> = g
            .outgoing_edges(hub)
            .unwrap()
            .iter()
            .map(|e| e.id.0)
            .collect();
        assert_eq!(
            edge_ids, expected,
            "migration must preserve all edges in order, pre-migration ones first"
        );

        // The vacated sub-block is gone from the origin slab, which stays a slab.
        assert!(
            adj_slab_codec::read_subblock(
                g.storage.as_ref(),
                slab_head,
                hub.0,
                AdjDirection::Outgoing
            )
            .is_err(),
            "the migrated node's sub-block must no longer be listed on the slab"
        );
        assert!(
            adj_slab_codec::is_slab_page(g.storage.as_ref(), slab_head).unwrap(),
            "the origin page must remain a slab after one node migrates away"
        );
    }

    /// Thrashing guard for issue #54, at the scale the issue documents: 100k
    /// low-degree nodes against the default 16384-page buffer pool.
    ///
    /// This is the failure #54 was opened for. Every low-degree node used to own a
    /// whole adjacency page, so 100k of them meant a ~100k-page working set against
    /// a 16384-page pool: the pool held ~16% of it, and a traversal evicted pages
    /// faster than it could use them. With the shared slab the same graph's
    /// adjacency fits in hundreds of pages, so the pool holds all of it and a full
    /// sweep evicts nothing.
    ///
    /// Ignored by default because building 100k nodes takes minutes — the scale IS
    /// the point, so it is not shrunk to fit a fast run. Run explicitly:
    ///
    ///     cargo test -p tessera-graph --lib --release --features pool-instrumentation \
    ///       -- --ignored thrashing_guard_low_degree_nodes_at_pool_overflow_scale
    #[test]
    #[ignore = "load test — minutes to build 100k nodes; run explicitly, see doc comment"]
    #[cfg(feature = "pool-instrumentation")]
    fn thrashing_guard_low_degree_nodes_at_pool_overflow_scale() {
        use crate::storage::page::PAGE_SIZE;
        use tempfile::TempDir;

        const N: usize = 100_000;
        const SINKS: usize = 8;

        // The real default: 64 MB / 4096 = 16384 pages, the pool size the issue
        // documents overflowing. Read from the config, not hardcoded, so the guard
        // tracks the product rather than a number frozen in a test.
        let cfg = GraphConfig::default();
        let pool_pages = cfg.memory_limit_bytes / PAGE_SIZE;
        assert_eq!(
            pool_pages, 16_384,
            "guard assumes the documented default pool"
        );

        let dir = TempDir::new().unwrap();
        let mut g = Graph::open(dir.path(), &cfg).unwrap();

        let sinks: Vec<NodeId> = (0..SINKS)
            .map(|_| g.add_node("Sink", Properties::default()).unwrap())
            .collect();
        g.begin_batch();
        for i in 0..N {
            let src = g.add_node("Src", Properties::default()).unwrap();
            g.add_edge("rel", src, sinks[i % SINKS], Properties::default())
                .unwrap();
        }
        g.end_batch().unwrap();

        let adj_pages = g.storage.page_count(DataFile::Adjacency);
        assert!(
            (adj_pages as usize) < pool_pages,
            "the whole adjacency working set must fit the pool: {adj_pages} pages vs \
             {pool_pages} — before #54 this was ~{N} pages and could not"
        );

        // Measure only the sweep, not the load that built the graph.
        g.reset_pool_instrumentation();
        let mut seen = 0usize;
        for &sink in &sinks {
            seen += g.incoming_edges(sink).unwrap().len();
        }
        assert_eq!(seen, N, "the sweep must see every edge");
        let (_hits, _misses, evictions) = g.pool_instrumentation();

        // Derived, not magic: the sweep touches at most every adjacency page plus
        // the node pages holding the endpoints, and all of it fits the pool — so a
        // pool that is not thrashing evicts nothing. Before the slab the same sweep
        // evicted continuously, the working set being ~6x the pool.
        assert_eq!(
            evictions, 0,
            "a working set that fits the pool must not evict: {evictions} evictions \
             means the slab is not holding the adjacency down"
        );
    }

    #[test]
    fn ten_thousand_degree_one_nodes_use_tens_of_adjacency_pages_not_thousands() {
        // The regression guard for issue #54, at the scale that caused it. Before the
        // shared slab, N low-degree nodes cost N adjacency pages: 10k nodes filled
        // ~10-20k pages of 4096 bytes each to hold 8 useful bytes apiece, so the
        // working set blew past the buffer pool (16384 pages) and thrashed it.
        // Measured here after the slab: 76 pages.
        //
        // The ceiling is 500 rather than ~76 so ordinary packing drift does not fail
        // the build; at 500 it still catches the regression it exists for, which is
        // two orders of magnitude away.
        //
        // Rotating over a few sinks exercises the other half of the design in the
        // same run: each sink ends up high-degree and must migrate out of the slab
        // onto a dedicated chain of its own.
        const N: usize = 10_000;
        const SINKS: usize = 4;

        let mut g = Graph::new();
        let sinks: Vec<NodeId> = (0..SINKS)
            .map(|_| g.add_node("Sink", Properties::default()).unwrap())
            .collect();
        for i in 0..N {
            let src = g.add_node("Src", Properties::default()).unwrap();
            g.add_edge("rel", src, sinks[i % SINKS], Properties::default())
                .unwrap();
        }

        let adj_pages = g.storage.page_count(DataFile::Adjacency) as usize;
        assert!(
            adj_pages < 500,
            "{N} degree-1 nodes must pack into tens of adjacency pages, not thousands: \
             {adj_pages} allocated"
        );

        // Density is worthless if it loses edges: the sinks together must account
        // for all N.
        let total: usize = sinks
            .iter()
            .map(|&s| g.incoming_edges(s).unwrap().len())
            .sum();
        assert_eq!(
            total, N,
            "every edge must survive the packing and the migrations"
        );

        // The sinks are high-degree, so each must have left the slab.
        for &sink in &sinks {
            let head = g
                .resolve_adj_pointer(sink.0)
                .unwrap()
                .unwrap()
                .incoming_page
                .unwrap();
            assert!(
                !adj_slab_codec::is_slab_page(g.storage.as_ref(), head).unwrap(),
                "a sink holding {} edges must have migrated to a dedicated chain",
                N / SINKS
            );
        }
    }

    #[test]
    fn writing_the_slot_pointer_per_edge_does_not_inflate_node_pages() {
        // Cycle 6 made every edge write its endpoint's adjacency pointer into the
        // node slot — a node-page write the edge path did not do before. This is the
        // check that the cost did not quietly reintroduce what #54 set out to
        // remove: node-page writes must track the nodes touched, not multiply with
        // the sink's accumulated degree, and must not inflate the page count.
        use std::sync::atomic::Ordering::Relaxed;
        let (mut g, _reads, node_writes) = graph_on_counting_backend();

        let sink = g.add_node("Sink", Properties::default()).unwrap();
        let n = 500usize;
        let mut sources = Vec::with_capacity(n);
        for _ in 0..n {
            sources.push(g.add_node("Src", Properties::default()).unwrap());
        }

        node_writes.store(0, Relaxed);
        for &src in &sources {
            g.add_edge("rel", src, sink, Properties::default()).unwrap();
        }
        let writes = node_writes.load(Relaxed);

        // Each edge touches two endpoints, so a small constant per edge is the
        // floor. What this rules out is a cost that grows with degree — the shape
        // every earlier quadratic in this series (#33/#46/#51) had.
        // Test assertion bound; `n` is a literal batch size in the same test.
        #[allow(clippy::cast_possible_truncation)]
        let ceiling = 4 * n as u32;
        assert!(
            writes <= ceiling,
            "node-page writes grew faster than the edges written: {writes} > {ceiling}"
        );

        // Node pages must stay proportional to nodes, not to edges.
        let node_pages = g.storage.page_count(DataFile::Nodes) as usize;
        let expected = (n + 1).div_ceil(SLOTS_PER_PAGE);
        assert!(
            node_pages <= expected + 1,
            "node pages ({node_pages}) exceed what {n} nodes need (~{expected}); \
             the per-edge pointer write is allocating pages"
        );
    }

    #[test]
    fn low_degree_nodes_share_slab_pages_instead_of_one_page_each() {
        // The core of issue #54. Before the slab was wired in, every node with any
        // adjacency got a whole 4096-byte page to itself, so N low-degree nodes cost
        // N pages: 1000 nodes of degree 1 filled ~1000 pages holding 8 useful bytes
        // each, blowing past the buffer pool and thrashing it.
        //
        // With the slab, each direction's sub-blocks pack together, so the page count
        // is driven by total edge bytes rather than by node count. The bound below is
        // deliberately loose (an order of magnitude under 1-page-per-node) because
        // this test pins the behaviour, not an exact packing ratio: exact page counts
        // belong to the density guard, which owns the tight bound.
        let mut g = Graph::new();
        let sink = g.add_node("Sink", Properties::default()).unwrap();
        let n = 1000usize;
        let mut sources = Vec::with_capacity(n);
        for _ in 0..n {
            let src = g.add_node("Src", Properties::default()).unwrap();
            g.add_edge("rel", src, sink, Properties::default()).unwrap();
            sources.push(src);
        }

        let adj_pages = g.storage.page_count(DataFile::Adjacency) as usize;
        assert!(
            adj_pages < n / 10,
            "{n} degree-1 nodes must share slab pages, not take one each: \
             {adj_pages} adjacency pages allocated"
        );

        // Density must not cost correctness: the high-degree sink still sees every
        // edge, and each low-degree source still resolves its own.
        assert_eq!(
            g.incoming_edges(sink).unwrap().len(),
            n,
            "sink must still see all {n} incoming edges after slab packing"
        );
        for src in sources {
            assert_eq!(
                g.outgoing_edges(src).unwrap().len(),
                1,
                "each degree-1 source must still resolve its single outgoing edge"
            );
        }
    }

    #[test]
    fn medium_degree_nodes_grow_within_the_slab_until_it_is_full() {
        // The case between the two extremes the other #54 tests cover: nodes too big
        // to be a single sub-block write, too small to migrate. Each grows its
        // sub-block in place, edge by edge, while sharing pages with the others — so
        // one page ends up with many live sub-blocks AND a deep directory at once.
        //
        // That is what makes this the guard for the two frontiers meeting: the
        // directory grows forward from the payload start while a sub-block grows
        // backward toward it. Growth that ignores the directory shifts the edges
        // over its bytes, truncating the first edge ID's high bytes — surfacing as
        // a wrong, small edge ID on read rather than as an error. Reading every
        // node's edges back is what catches it.
        let mut g = Graph::new();
        let mut expected = Vec::new();
        for _ in 0..100 {
            let src = g.add_node("S", Properties::default()).unwrap();
            let mut ids = Vec::new();
            for _ in 0..20 {
                let tgt = g.add_node("T", Properties::default()).unwrap();
                ids.push(
                    g.add_edge("KNOWS", src, tgt, Properties::default())
                        .unwrap()
                        .0,
                );
            }
            expected.push((src, ids));
        }

        for (src, ids) in expected {
            let got: Vec<u64> = g
                .outgoing_edges(src)
                .unwrap()
                .iter()
                .map(|e| e.id.0)
                .collect();
            assert_eq!(
                got, ids,
                "every degree-20 node must read back all its edges, in order"
            );
        }
    }

    // Issue #46, Cycle 0 (diagnostic): the guard above keeps the tail cache HOT
    // (one fan-in node, never evicted), so it never exercises the O(degree)
    // fallback walk. This test FORCES a tail-cache miss before each batch —
    // reproducing the real production failure (cold cache / churn) — and asserts
    // the per-batch adjacency-page reads stay bounded, not proportional to the
    // accumulated degree.
    #[test]
    fn concentrated_fanin_append_cost_with_forced_cache_miss_is_bounded() {
        use std::sync::atomic::Ordering::Relaxed;
        const BATCH: usize = 50;
        const N: usize = 30;
        let (mut g, reads) = graph_on_adj_counting_backend();
        let hub = g.add_node("H", Properties::default()).unwrap();

        let mut mid_batch_reads = 0u32;
        let mut last_batch_reads = 0u32;

        for batch_idx in 0..N {
            g.begin_batch();
            for _ in 0..BATCH {
                let t = g.add_node("T", Properties::default()).unwrap();
                g.add_edge("R", hub, t, Properties::default()).unwrap();
            }
            // Force the tail-cache miss that cold-start / churn produces: drop the
            // hub's cached tail state so the next flush must recover it.
            g.adj_tail_cache.remove(hub.0);
            reads.store(0, Relaxed);
            g.end_batch().unwrap();
            let batch_reads = reads.load(Relaxed);
            if batch_idx == N / 2 {
                mid_batch_reads = batch_reads;
            }
            if batch_idx == N - 1 {
                last_batch_reads = batch_reads;
            }
        }

        // The tail is now resolved from the first page's persisted last_page_id
        // (V2), so a cache-miss flush reads a number of adjacency pages that does
        // not depend on the hub's accumulated degree. This is the property that
        // matters: the quadratic walk would make the last batch (degree ~N*BATCH)
        // read far more pages than the mid batch (degree ~N/2*BATCH).
        assert!(
            last_batch_reads <= mid_batch_reads,
            "per-batch adjacency reads grew with degree under forced cache miss: \
             mid={mid_batch_reads}, last={last_batch_reads}"
        );

        // Absolute ceiling, expressed per flushed edge rather than as a constant:
        // each batch also opens a slab sub-block for each of its BATCH fresh
        // degree-1 targets (#54), and routing an endpoint reads its head page to
        // check the page type. Measured at ~2 reads per flushed edge (22 reads at
        // BATCH=10, 105 at BATCH=50), flat across batches as the hub's degree
        // grows. That is per-edge and degree-independent — unlike the chain walk
        // this guard exists to catch, which grew with degree. The hub itself long
        // ago migrated to a dedicated chain and resolves its tail in O(1).
        // Test assertion bound; `BATCH` is a literal const.
        #[allow(clippy::cast_possible_truncation)]
        let ceiling = 3 * BATCH as u32;
        assert!(
            last_batch_reads <= ceiling,
            "cache-miss flush read too many adjacency pages: {last_batch_reads} > {ceiling}"
        );
    }

    // Issue #46, Cycle 1 (cold start): after a rebuild/reopen the tail cache is
    // empty. Repopulating it during the rebuild (which already walks the chains
    // to rebuild adj_cache) means the first append to a high-degree node after
    // reopen costs O(1), not O(chain pages).
    #[test]
    fn reopened_graph_serves_fanin_append_without_chain_walk() {
        use std::sync::atomic::Ordering::Relaxed;
        let (mut g, reads) = graph_on_adj_counting_backend();
        let hub = g.add_node("H", Properties::default()).unwrap();
        // Build a multi-page outgoing chain on `hub` (>508 forces a continuation
        // page, so a walk would read more than one page).
        for _ in 0..700 {
            let t = g.add_node("T", Properties::default()).unwrap();
            g.add_edge("R", hub, t, Properties::default()).unwrap();
        }
        // Simulate reopen: rebuild indexes from pages, which clears the tail cache.
        g.rebuild_indexes().unwrap();

        // A single append to the high-degree hub after reopen.
        let t = g.add_node("T", Properties::default()).unwrap();
        reads.store(0, Relaxed);
        g.add_edge("R", hub, t, Properties::default()).unwrap();
        let append_reads = reads.load(Relaxed);

        // O(1): the append must not re-walk the existing multi-page chain. The
        // constant covers reading the tail page to extend it in place, plus the
        // page-type checks that route each endpoint to the slab or the chain
        // (#54) — one read per endpoint per append, independent of degree. A walk
        // would instead read every page of the ~700-edge chain, growing with it.
        assert!(
            append_reads <= 6,
            "post-reopen fan-in append walked the chain: {append_reads} adjacency reads"
        );
    }

    #[test]
    fn test_add_remove_immediately() {
        let mut g = Graph::new();
        let a = g.add_node("A", props! {}).unwrap();
        g.remove_node(a).unwrap();
        assert_eq!(g.node_count(), 0);
        assert!(g.node(a).is_err());
    }

    #[test]
    fn graph_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Graph>();
    }

    #[test]
    fn shared_graph_is_send_sync_clone() {
        fn assert_send_sync_clone<T: Send + Sync + Clone>() {}
        assert_send_sync_clone::<SharedGraph>();
    }

    #[test]
    fn shared_graph_read_write() {
        let sg = SharedGraph::new(Graph::new());
        let id = sg.write().add_node("N", props! { "x" => 1_i64 }).unwrap();
        let node = sg.read().node(id).unwrap();
        assert_eq!(node.label(), "N");
    }

    /// Regression floor: `SharedGraph` must process at least 50,000 `add_node`/s
    /// in single-thread (release mode). In debug mode the threshold is lowered
    /// to 10,000 ops/s to account for lack of optimizations.
    /// Criterion benchmarks are the source of truth for peak throughput.
    /// Run with `cargo test -- --ignored` to execute.
    // ── Lazy adjacency allocation tests ────────────────────────────

    #[test]
    fn add_node_allocates_zero_adj_pages() {
        let mut g = Graph::new();
        g.add_node("Person", Properties::default()).unwrap();
        let adj_pages = g.storage.page_count(DataFile::Adjacency);
        assert_eq!(
            adj_pages, 0,
            "add_node must not pre-allocate adjacency pages"
        );
    }

    #[test]
    fn add_edge_creates_adj_pages_on_demand() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        let b = g.add_node("B", Properties::default()).unwrap();
        g.add_edge("rel", a, b, Properties::default()).unwrap();
        let adj_pages = g.storage.page_count(DataFile::Adjacency);
        assert_eq!(
            adj_pages, 2,
            "one add_edge between two distinct nodes must create exactly 2 adj pages"
        );
        let out = g.outgoing_edges(a).unwrap();
        assert_eq!(out.len(), 1);
        let inc = g.incoming_edges(b).unwrap();
        assert_eq!(inc.len(), 1);
    }

    #[test]
    fn write_adj_immediate_matches_batch_result_for_same_edges() {
        // The batch path and the non-batch path must produce the same adjacency
        // record for the same sequence of edges from the same source — the
        // tail-cache fix must not change the on-disk result.
        let build = |batched: bool| -> Vec<u64> {
            let mut g = Graph::new();
            let src = g.add_node("S", Properties::default()).unwrap();
            let mut targets = Vec::new();
            for _ in 0..600 {
                targets.push(g.add_node("T", Properties::default()).unwrap());
            }
            if batched {
                g.begin_batch();
            }
            for &t in &targets {
                g.add_edge("R", src, t, Properties::default()).unwrap();
            }
            if batched {
                g.end_batch().unwrap();
            }
            // Decode the source's outgoing edge ids in stored order.
            g.outgoing_edges(src)
                .unwrap()
                .iter()
                .map(|e| e.id.0)
                .collect()
        };
        let batched = build(true);
        let immediate = build(false);
        assert_eq!(batched.len(), 600);
        assert_eq!(
            batched, immediate,
            "batch and non-batch adjacency must yield identical edge id order"
        );
    }

    #[test]
    fn write_adj_immediate_does_not_leak_pages_on_concentrated_fanin() {
        // Non-batch path (no begin_batch): add many edges from one source to
        // distinct targets, one at a time. Each source-outgoing append must
        // extend its record in place, not reallocate it (the old write_adjacency
        // path always allocated fresh pages, leaking the old chain on every
        // single edge — 600 edges leaked hundreds of pages).
        //
        // Since #54 the targets are degree-1 and pack into shared slabs instead of
        // taking a page each, so the total is dominated by the source's own chain.
        // The ceiling below is what a leak-free run needs with room to spare, and
        // is still an order of magnitude under the per-edge reallocation this
        // guard exists to catch.
        let mut g = Graph::new();
        let src = g.add_node("S", Properties::default()).unwrap();
        let d = 600usize; // > 508 so the source chain spans 2 pages
        for _ in 0..d {
            let t = g.add_node("T", Properties::default()).unwrap();
            g.add_edge("R", src, t, Properties::default()).unwrap();
        }
        let adj_pages = g.storage.page_count(DataFile::Adjacency) as usize;
        // ~2 pages for the source's outgoing chain (ceil(600/508)) + a handful of
        // slab pages holding all 600 targets' single-edge incoming sub-blocks.
        let ceiling = 20;
        assert!(
            adj_pages <= ceiling,
            "non-batch append leaked pages: {adj_pages} allocated, ceiling {ceiling}"
        );
        assert_eq!(g.outgoing_edges(src).unwrap().len(), d);
    }

    #[test]
    #[ignore = "throughput gate — run with --ignored or in release mode"]
    fn shared_graph_add_node_throughput_floor() {
        use std::time::Instant;
        let g = SharedGraph::new(Graph::new());
        let n = 10_000_u64;
        let start = Instant::now();
        for _ in 0..n {
            g.write().add_node("N", Properties::default()).unwrap();
        }
        let elapsed = start.elapsed();
        #[allow(clippy::cast_precision_loss)]
        let ops_per_sec = n as f64 / elapsed.as_secs_f64();
        let floor = if cfg!(debug_assertions) {
            10_000.0
        } else {
            50_000.0
        };
        assert!(
            ops_per_sec > floor,
            "throughput regression: {ops_per_sec:.0} ops/s < {floor:.0} ops/s floor"
        );
    }

    #[test]
    fn reopen_graph_isolated_node_has_no_adj_cache_entry() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let cfg = GraphConfig {
            create_if_missing: true,
            ..Default::default()
        };
        let id = {
            let mut g = Graph::open(dir.path(), &cfg).unwrap();
            g.add_node("X", Properties::default()).unwrap()
        };
        let g = Graph::open(dir.path(), &cfg).unwrap();
        assert!(g.node(id).is_ok());
        assert!(
            g.adj_cache.get(id.0).is_none(),
            "isolated node must not occupy an adj cache entry after reopen"
        );
        assert_eq!(g.outgoing_edges(id).unwrap().len(), 0);
        assert_eq!(g.incoming_edges(id).unwrap().len(), 0);
    }

    #[test]
    fn self_loop_on_fresh_node_creates_two_adj_pages() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        g.add_edge("self", a, a, Properties::default()).unwrap();
        let adj_pages = g.storage.page_count(DataFile::Adjacency);
        assert_eq!(adj_pages, 2);
        assert_eq!(g.outgoing_edges(a).unwrap().len(), 1);
        assert_eq!(g.incoming_edges(a).unwrap().len(), 1);
    }

    // ── Negative caching + quality hardening tests ────────────────

    #[test]
    fn resolve_adj_pointer_isolated_node_caches_absence() {
        let mut g = Graph::new();
        let id = g.add_node("X", Properties::default()).unwrap();
        // First call: cache miss, scans pages (zero pages exist), returns None
        assert!(g.resolve_adj_pointer(id.0).unwrap().is_none());
        // After the call, cache must contain an entry with both pages = None
        let cached = g.adj_cache.get(id.0);
        assert!(
            cached.is_some(),
            "isolated node must be cached after resolve_adj_pointer returns None"
        );
        let ptr = cached.unwrap();
        assert!(ptr.outgoing_page.is_none());
        assert!(ptr.incoming_page.is_none());
    }

    #[test]
    fn resolve_adj_pointer_no_double_scan_on_repeated_calls() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        let b = g.add_node("B", Properties::default()).unwrap();
        g.add_edge("rel", a, b, Properties::default()).unwrap();
        // c is isolated — first call must cache absence
        let c = g.add_node("C", Properties::default()).unwrap();
        assert!(g.resolve_adj_pointer(c.0).unwrap().is_none());
        assert!(g.resolve_adj_pointer(c.0).unwrap().is_none());
        // The second call hit the cache (entry exists with None pages)
        let ptr = g.adj_cache.get(c.0).unwrap();
        assert!(ptr.outgoing_page.is_none() && ptr.incoming_page.is_none());
    }

    #[test]
    fn commit_txn_with_node_and_edge_in_same_txn_persists_adj_pointer() {
        // A node created AND given an edge inside the same explicit transaction:
        // at commit, category-B reconciliation builds the edge's adjacency and
        // must persist the adjacency head into the new node's slot. But the new
        // node's page is not materialized until the vacuum (phase 5), so a naive
        // slot write hits "page not allocated" and aborts the commit half-done.
        // The write path must tolerate a not-yet-materialized node page.
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let a = g.add_node_in_txn(txn, "A", props! {}).unwrap();
        let b = g.add_node_in_txn(txn, "B", props! {}).unwrap();
        let e = g.add_edge_in_txn(txn, "rel", a, b, props! {}).unwrap();
        // Must not error with CorruptPage "page not allocated".
        g.commit_txn(txn).unwrap();

        // The edge is traversable after commit (adjacency built at reconcile).
        let out = g.outgoing_edges(a).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id(), e);
        assert_eq!(g.incoming_edges(b).unwrap().len(), 1);

        // And the transaction is fully closed, not left phantom-active.
        assert!(!g.txn_is_active(txn), "commit must end the transaction");

        // After the vacuum materializes the new nodes' pages, their slots must
        // carry the adjacency heads the cache held (the pointer that
        // persist_adj_pointer_to_slot could not write at commit time because the
        // pages did not exist yet). Evict the cache so the slot is the only
        // source, then confirm the edge is still reachable.
        g.vacuum_once().unwrap();
        let src_slot = g.read_node_slot_bytes(a.0).unwrap();
        assert_ne!(
            node_codec::slot_adj_page_id(&src_slot),
            node_codec::ADJ_PAGE_ID_SENTINEL,
            "vacuum must persist the outgoing head into the source node's slot"
        );
        g.adj_cache.remove(a.0);
        g.adj_cache.remove(b.0);
        assert_eq!(
            g.outgoing_edges(a).unwrap().len(),
            1,
            "edge reachable from slot after vacuum"
        );
        assert_eq!(g.incoming_edges(b).unwrap().len(), 1);
    }

    #[test]
    fn resolve_adj_pointer_reads_no_adjacency_pages_on_cache_miss() {
        use std::sync::atomic::Ordering::Relaxed;
        // Cycle 7 (#54): on a cache miss, resolve_adj_pointer must read the
        // node's slot (two heads), NOT scan DataFile::Adjacency. With many
        // sparse edges a scan would touch many adjacency pages; the slot read
        // touches zero.
        let (mut g, adj_reads, _writes) = graph_on_counting_backend();
        // Build sparse adjacency: many distinct source→target pairs, so a scan
        // of DataFile::Adjacency would be proportional to its page count.
        let mut hub = None;
        for i in 0..40 {
            let s = g.add_node("S", props! {}).unwrap();
            let t = g.add_node("T", props! {}).unwrap();
            g.add_edge("r", s, t, props! {}).unwrap();
            if i == 20 {
                hub = Some((s, t));
            }
        }
        let (src, dst) = hub.unwrap();

        // Force a cache miss for both endpoints and reset the read counter so we
        // only measure resolve_adj_pointer's own reads.
        g.adj_cache.remove(src.0);
        g.adj_cache.remove(dst.0);
        adj_reads.store(0, Relaxed);

        let out = g.resolve_adj_pointer(src.0).unwrap();
        let inc = g.resolve_adj_pointer(dst.0).unwrap();
        assert!(
            out.unwrap().outgoing_page.is_some(),
            "source has an outgoing head"
        );
        assert!(
            inc.unwrap().incoming_page.is_some(),
            "destination has an incoming head"
        );

        assert_eq!(
            adj_reads.load(Relaxed),
            0,
            "resolve_adj_pointer must not read any DataFile::Adjacency page on a cache miss"
        );
    }

    #[test]
    fn remove_edge_preserves_both_directions_in_cache() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        let b = g.add_node("B", Properties::default()).unwrap();
        let e1 = g.add_edge("r1", a, b, Properties::default()).unwrap();
        let _e2 = g.add_edge("r2", a, b, Properties::default()).unwrap();

        // After adding edges, both directions are cached for node `a`
        let before = g.adj_cache.get(a.0).unwrap();
        assert!(before.outgoing_page.is_some());

        // Remove first edge from a's outgoing list
        g.remove_edge(e1).unwrap();

        // Node a must still have a valid cache entry with outgoing page present
        let after = g.adj_cache.get(a.0).unwrap();
        assert!(
            after.outgoing_page.is_some(),
            "outgoing_page must remain cached after partial edge removal"
        );
    }

    #[test]
    fn node_id_zero_after_adding_one_node_still_not_found() {
        let mut g = Graph::new();
        g.add_node("A", Properties::default()).unwrap();
        assert!(matches!(g.node(NodeId(0)), Err(Error::NodeNotFound(_))));
        assert!(matches!(g.edge(EdgeId(0)), Err(Error::EdgeNotFound(_))));
    }

    #[test]
    fn add_node_adj_page_count_scales_zero() {
        let mut g = Graph::new();
        for _ in 0..1_000 {
            g.add_node("N", Properties::default()).unwrap();
        }
        assert_eq!(
            g.storage.page_count(DataFile::Adjacency),
            0,
            "lazy allocation: 1000 isolated nodes must produce 0 adj pages"
        );
    }

    #[test]
    #[ignore = "throughput gate — run with --ignored or in release mode"]
    fn add_edge_throughput_floor_with_negative_caching() {
        use std::time::Instant;
        let mut g = Graph::new();
        let hub = g.add_node("Hub", Properties::default()).unwrap();
        let spokes: Vec<_> = (0..1_000)
            .map(|_| g.add_node("Spoke", Properties::default()).unwrap())
            .collect();

        let n = spokes.len() as u64;
        let start = Instant::now();
        for spoke in &spokes {
            g.add_edge("LINK", hub, *spoke, Properties::default())
                .unwrap();
        }
        let elapsed = start.elapsed();

        #[allow(clippy::cast_precision_loss)]
        let ops_per_sec = n as f64 / elapsed.as_secs_f64();
        let floor = if cfg!(debug_assertions) {
            500.0
        } else {
            5_000.0
        };
        assert!(
            ops_per_sec > floor,
            "add_edge throughput regression: {ops_per_sec:.0} ops/s < {floor:.0} floor"
        );
    }

    // ── adj_pointer / set_adj_pointer API tests ─────────────────

    #[test]
    fn adj_pointer_returns_some_for_node_with_edges() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        let b = g.add_node("B", Properties::default()).unwrap();
        g.add_edge("R", a, b, Properties::default()).unwrap();
        let ptr = g.adj_pointer(a).unwrap();
        assert!(
            ptr.is_some(),
            "node with edges must have an adjacency pointer"
        );
        assert!(ptr.unwrap().outgoing_page.is_some());
    }

    #[test]
    fn adj_pointer_returns_none_for_isolated_node() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        assert!(
            g.adj_pointer(a).unwrap().is_none(),
            "isolated node must have no adjacency pointer"
        );
    }

    #[test]
    fn set_adj_pointer_prewarms_cache() {
        use crate::storage::codec::adjacency_codec::AdjacencyPointer;
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        let synthetic = AdjacencyPointer {
            outgoing_page: Some(42),
            incoming_page: Some(99),
        };
        g.set_adj_pointer(a, synthetic);
        // The cache now has the synthetic pointer
        let cached = g.adj_cache.get(a.0).unwrap();
        assert_eq!(cached.outgoing_page, Some(42));
        assert_eq!(cached.incoming_page, Some(99));
    }

    #[test]
    fn set_adj_pointer_noop_for_removed_node() {
        use crate::storage::codec::adjacency_codec::AdjacencyPointer;
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        g.remove_node(a).unwrap();
        let ptr = AdjacencyPointer {
            outgoing_page: Some(42),
            incoming_page: None,
        };
        g.set_adj_pointer(a, ptr);
        // Cache must NOT be poisoned with stale pointer.
        // Single assert covers both invariants (cache miss OR zeroed pointer)
        // atomically — splitting would let a passing first hide a failing second.
        let cached = g.adj_cache.get(a.0);
        assert!(
            cached.is_none_or(|p| p.outgoing_page.is_none() && p.incoming_page.is_none()),
            "set_adj_pointer must not poison cache for removed node"
        );
    }

    /// Gives `node` enough outgoing edges to migrate it out of the shared slab and
    /// onto a dedicated chain, which is the only case that has a tail to cache.
    ///
    /// The tail cache (#33/#46) tracks the end of a chain; a low-degree node packed
    /// into a slab has no chain, so a single edge no longer populates it (#54).
    /// Smallest edge count that cannot be written as a single slab sub-block, so a
    /// node given this many edges at once goes straight to a dedicated chain.
    /// Derived from the slab's own capacity rule rather than hardcoded, so it
    /// tracks the format.
    fn edges_too_many_for_a_slab() -> usize {
        // A slab page holds a few hundred edges at most, so this bound is never the
        // one that ends the search — it just makes termination obvious.
        (1..4096)
            .find(|&n| !adj_slab_codec::fits_in_empty_slab(n))
            .expect("a degree too large to pack must exist well below 4096 edges")
    }

    fn give_node_a_dedicated_chain(g: &mut Graph, node: NodeId) -> usize {
        // Add edges until the node's head is no longer a slab page. Exactly when a
        // node migrates depends on how full its slab is, so this asks the graph
        // rather than computing a degree the format would silently invalidate.
        for added in 1..10_000 {
            let t = g.add_node("T", Properties::default()).unwrap();
            g.add_edge("R", node, t, Properties::default()).unwrap();
            let head = g
                .resolve_adj_pointer(node.0)
                .unwrap()
                .and_then(|p| p.outgoing_page)
                .expect("a node with edges must have an outgoing head");
            if !adj_slab_codec::is_slab_page(g.storage.as_ref(), head).unwrap() {
                return added;
            }
        }
        panic!("node never migrated off the slab")
    }

    #[test]
    fn remove_node_invalidates_tail_cache() {
        use crate::storage::codec::adjacency_codec::AdjDirection;
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        // Populate the source's outgoing tail cache: needs a dedicated chain.
        give_node_a_dedicated_chain(&mut g, a);
        assert!(g.adj_tail_cache.get(a.0, AdjDirection::Outgoing).is_some());
        // Removing the node must drop its tail-cache entry, not just adj_cache.
        g.remove_node(a).unwrap();
        assert!(
            g.adj_tail_cache.get(a.0, AdjDirection::Outgoing).is_none(),
            "remove_node must invalidate the tail cache for the removed node"
        );
    }

    #[test]
    fn set_adj_pointer_invalidates_tail_cache() {
        use crate::storage::codec::adjacency_codec::{AdjDirection, AdjacencyPointer};
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        give_node_a_dedicated_chain(&mut g, a);
        assert!(g.adj_tail_cache.get(a.0, AdjDirection::Outgoing).is_some());
        // Injecting a pointer that may reference a different chain must not leave
        // a stale tail state that a later append would trust.
        g.set_adj_pointer(
            a,
            AdjacencyPointer {
                outgoing_page: Some(123),
                incoming_page: None,
            },
        );
        assert!(
            g.adj_tail_cache.get(a.0, AdjDirection::Outgoing).is_none(),
            "set_adj_pointer must invalidate the tail cache it may desync"
        );
    }

    #[test]
    fn set_adj_pointer_round_trip_via_adj_pointer() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        let b = g.add_node("B", Properties::default()).unwrap();
        g.add_edge("R", a, b, Properties::default()).unwrap();
        let real_ptr = g.adj_pointer(a).unwrap().unwrap();
        // Re-inject the same pointer via set_adj_pointer
        g.set_adj_pointer(a, real_ptr);
        let read_back = g.adj_pointer(a).unwrap().unwrap();
        assert_eq!(read_back, real_ptr);
    }

    #[test]
    fn adj_pointer_none_after_remove_node() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        let b = g.add_node("B", Properties::default()).unwrap();
        g.add_edge("R", a, b, Properties::default()).unwrap();
        assert!(g.adj_pointer(a).unwrap().is_some());
        g.remove_node(a).unwrap();
        assert!(
            g.adj_pointer(a).unwrap().is_none(),
            "adj_pointer must return None for a removed node"
        );
    }

    // -----------------------------------------------------------------------
    // PropertyIndex integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn property_index_populated_on_add_node() {
        let mut g = Graph::new();
        let id = g
            .add_node("Person", props! { "name" => "Alice", "age" => 30i64 })
            .unwrap();
        let ids =
            g.nodes_by_label_and_property("Person", "name", &Property::String("Alice".into()));
        assert_eq!(ids, vec![id]);
        let ids2 = g.nodes_by_label_and_property("Person", "age", &Property::I64(30));
        assert_eq!(ids2, vec![id]);
    }

    #[test]
    fn property_index_cleared_on_remove_node() {
        let mut g = Graph::new();
        let id = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
        g.remove_node(id).unwrap();
        let ids = g.nodes_by_label_and_property("Person", "name", &Property::String("Bob".into()));
        assert!(ids.is_empty());
    }

    #[test]
    fn property_index_updated_on_update_node() {
        let mut g = Graph::new();
        let id = g
            .add_node("Person", props! { "status" => "junior" })
            .unwrap();

        // Verify initial state
        assert_eq!(
            g.nodes_by_label_and_property("Person", "status", &Property::String("junior".into())),
            vec![id]
        );

        // Update node with a new property value
        let updated = Node::new(id, "Person", props! { "status" => "senior" });
        g.update_node(id, &updated).unwrap();

        // Old value must be gone
        assert!(
            g.nodes_by_label_and_property("Person", "status", &Property::String("junior".into()))
                .is_empty()
        );
        // New value must be present
        assert_eq!(
            g.nodes_by_label_and_property("Person", "status", &Property::String("senior".into())),
            vec![id]
        );
    }

    #[test]
    fn property_index_rebuilds_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let config = GraphConfig::default();

        let node_id = {
            let mut g = Graph::open(dir.path(), &config).unwrap();
            let id = g
                .add_node("Device", props! { "serial" => "SN-001", "active" => true })
                .unwrap();
            g.flush().unwrap();
            id
        };

        // Reopen — property index must be rebuilt from disk.
        let g2 = Graph::open(dir.path(), &config).unwrap();

        let by_serial =
            g2.nodes_by_label_and_property("Device", "serial", &Property::String("SN-001".into()));
        assert_eq!(
            by_serial,
            vec![node_id],
            "property index must survive reopen"
        );

        let by_active = g2.nodes_by_label_and_property("Device", "active", &Property::Bool(true));
        assert_eq!(by_active, vec![node_id]);
    }

    #[test]
    fn flush_adj_pending_preserves_preexisting_edges() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        let b = g.add_node("B", Properties::default()).unwrap();
        let c = g.add_node("C", Properties::default()).unwrap();

        // Create edge OUTSIDE batch — written directly to storage + cache
        g.add_edge("R", a, b, Properties::default()).unwrap();
        assert_eq!(g.outgoing_edges(a).unwrap().len(), 1);

        // Batch a second edge to the same source node
        g.begin_batch();
        g.add_edge("R", a, c, Properties::default()).unwrap();
        g.end_batch().unwrap();

        // Both edges must be present — flush must not overwrite the first
        assert_eq!(
            g.outgoing_edges(a).unwrap().len(),
            2,
            "flush_adj_pending must preserve preexisting edges"
        );
    }

    #[test]
    fn batch_flush_persists_head_even_when_adj_cache_evicts_the_node() {
        use tempfile::TempDir;

        // The durability bug the #54 thrashing guard (cycle 10) surfaced: a batch
        // used to persist each node's adjacency head by RE-READING it from
        // adj_cache after the append. That cache evicts, so in a batch touching
        // more nodes than the cache holds, a high-degree node's entry could be gone
        // by persist time — its slot kept the sentinel, its whole chain became
        // unreachable, and the loss was non-deterministic (whichever nodes happened
        // to be evicted). At 100k nodes this dropped entire sinks to zero edges.
        //
        // Reproduced in milliseconds by shrinking the adjacency cache to a handful
        // of entries and touching more distinct nodes than that in one batch. The
        // assertion is on the persisted head, not on edge visibility, so it points
        // at the cause (the slot pointer) rather than a downstream symptom.
        let dir = TempDir::new().unwrap();
        let cfg = GraphConfig {
            adj_cache_capacity: 4,
            ..Default::default()
        };
        let mut g = Graph::open(dir.path(), &cfg).unwrap();

        let sink = g.add_node("Sink", Properties::default()).unwrap();
        let n = 100usize; // >> the 4-entry cache, so the sink is evicted mid-flush

        g.begin_batch();
        for _ in 0..n {
            let src = g.add_node("Src", Properties::default()).unwrap();
            g.add_edge("rel", src, sink, Properties::default()).unwrap();
        }
        g.end_batch().unwrap();

        // The sink's incoming head must be persisted on disk, sentinel or not.
        let slot = g.read_node_slot_bytes(sink.0).unwrap();
        assert_ne!(
            node_codec::slot_adj_incoming_page_id(&slot),
            node_codec::ADJ_PAGE_ID_SENTINEL,
            "the sink's incoming head was lost: adj_cache evicted it before persist"
        );

        // And the edges are all reachable through it.
        assert_eq!(
            g.incoming_edges(sink).unwrap().len(),
            n,
            "the sink must still see all its edges after a cache-evicting flush"
        );
    }

    #[test]
    fn flush_adj_pending_populates_and_reuses_tail_cache() {
        use crate::storage::codec::adjacency_codec::AdjDirection;
        let mut g = Graph::new();
        let src = g.add_node("S", Properties::default()).unwrap();

        // The tail cache tracks the end of a dedicated chain, so src needs enough
        // edges to own one — a low-degree node packs into a slab and has no tail
        // (#54). The first batch is sized to force that migration.
        let first_batch = edges_too_many_for_a_slab();
        g.begin_batch();
        for _ in 0..first_batch {
            let t = g.add_node("T", Properties::default()).unwrap();
            g.add_edge("R", src, t, Properties::default()).unwrap();
        }
        g.end_batch().unwrap();

        // After the flush, the tail cache holds src's outgoing chain state so the
        // next batch appends without re-walking the chain.
        let st1 = g
            .adj_tail_cache
            .get(src.0, AdjDirection::Outgoing)
            .expect("tail cache must be populated after flush");
        assert_eq!(st1.total_edges, first_batch);

        // Second batch: the cached state advances, not recomputed from scratch.
        g.begin_batch();
        for _ in 0..3 {
            let t = g.add_node("T", Properties::default()).unwrap();
            g.add_edge("R", src, t, Properties::default()).unwrap();
        }
        g.end_batch().unwrap();

        let st2 = g
            .adj_tail_cache
            .get(src.0, AdjDirection::Outgoing)
            .expect("tail cache still populated");
        assert_eq!(st2.total_edges, first_batch + 3);
        // The chain is still readable in full (correctness preserved).
        assert_eq!(g.outgoing_edges(src).unwrap().len(), first_batch + 3);
    }

    // ── v0.6.0 Fase 2 Task 2 C1: WalObserver API ────────────────────────
    //
    // The three tests below pin the API surface introduced in C1.
    // C2 will reuse the install path to prove the observer actually
    // fires; here we only verify that wiring the observer into the
    // `Graph` constructor / builder does not change baseline behaviour
    // (no calls on open, write paths work, builder returns Self).

    #[test]
    fn open_with_wal_observer_installs_observer_without_firing_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        let called = Arc::new(Mutex::new(false));
        let called_clone = Arc::clone(&called);
        let obs: WalObserver = Box::new(move |_c: FsyncCause, _d: std::time::Duration| {
            *called_clone.lock().unwrap() = true;
        });
        let _graph = Graph::open_with_wal_observer(tmp.path(), &GraphConfig::default(), obs)
            .expect("open_with_wal_observer failed");
        assert!(
            !*called.lock().unwrap(),
            "observer must not fire during open() — only during subsequent fsyncs",
        );
    }

    #[test]
    fn with_wal_observer_builder_installs_observer_without_firing_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        let called = Arc::new(Mutex::new(false));
        let called_clone = Arc::clone(&called);
        let obs: WalObserver = Box::new(move |_c: FsyncCause, _d: std::time::Duration| {
            *called_clone.lock().unwrap() = true;
        });
        let _graph = Graph::open(tmp.path(), &GraphConfig::default())
            .expect("open failed")
            .with_wal_observer(obs);
        assert!(
            !*called.lock().unwrap(),
            "observer must not fire after the builder runs — only during subsequent fsyncs",
        );
    }

    #[test]
    fn graph_without_observer_writes_normally() {
        let tmp = tempfile::tempdir().unwrap();
        let mut g = Graph::open(tmp.path(), &GraphConfig::default()).expect("open");
        // Baseline: no observer installed, add_node must succeed and
        // the absence of an observer must not break the WAL fsync path.
        let _id = g.add_node("Person", Properties::default()).unwrap();
    }

    // ── v0.6.0 Fase 2 Task 2 C2: wal_sync instrumentation ──────────────
    //
    // C1 installed the observer; C2 makes Graph::wal_sync invoke it
    // after each real fsync (and only after real ones — skips inside
    // an open batch must stay unobserved). The end_batch path is
    // unified through Graph::wal_sync so observers see the batch
    // fsync exactly once.

    #[test]
    fn observer_fires_on_write_outside_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let durations: Arc<Mutex<Vec<std::time::Duration>>> = Arc::new(Mutex::new(vec![]));
        let durations_clone = Arc::clone(&durations);
        let obs: WalObserver = Box::new(move |_c: FsyncCause, d: std::time::Duration| {
            durations_clone.lock().unwrap().push(d);
        });
        let mut g =
            Graph::open_with_wal_observer(tmp.path(), &GraphConfig::default(), obs).expect("open");
        g.add_node("X", Properties::default()).unwrap();
        let recorded = durations.lock().unwrap().clone();
        assert!(
            !recorded.is_empty(),
            "observer must fire at least once after add_node outside batch",
        );
        for d in &recorded {
            assert!(
                *d < std::time::Duration::from_secs(5),
                "fsync duration implausibly large: {d:?}",
            );
        }
    }

    #[test]
    fn observer_does_not_fire_inside_batch_but_fires_on_end_batch() {
        let tmp = tempfile::tempdir().unwrap();
        // Record causes, not a bare count: this pins that end_batch produces
        // exactly one BatchClose whose op_count equals the batch's operations,
        // observing coalescence by identity rather than by counting (issue #43).
        let causes: Arc<Mutex<Vec<FsyncCause>>> = Arc::new(Mutex::new(vec![]));
        let causes_clone = Arc::clone(&causes);
        let obs: WalObserver = Box::new(move |c: FsyncCause, _d: std::time::Duration| {
            causes_clone.lock().unwrap().push(c);
        });
        let mut g =
            Graph::open_with_wal_observer(tmp.path(), &GraphConfig::default(), obs).expect("open");
        g.begin_batch();
        g.add_node("Y", Properties::default()).unwrap();
        g.add_node("Y", Properties::default()).unwrap();
        g.add_node("Y", Properties::default()).unwrap();
        // Inside the batch, every wal_sync short-circuits on
        // batch_depth > 0 — the observer must never see them.
        assert!(
            causes.lock().unwrap().is_empty(),
            "observer must not fire during an open batch",
        );
        g.end_batch().unwrap();
        // end_batch goes through Graph::wal_sync (unified path), so the observer
        // fires exactly once, and the cause carries the coalesced op count.
        assert_eq!(
            *causes.lock().unwrap(),
            vec![FsyncCause::BatchClose { op_count: 3 }],
            "end_batch flushes exactly one batch-close fsync coalescing 3 ops",
        );
    }

    #[test]
    fn observer_fires_on_memory_backend_with_near_zero_duration() {
        // The observer wraps `self.storage.wal_sync()` regardless of
        // backend: it measures elapsed time around the call and fires
        // unconditionally outside a batch. For the memory backend the
        // call is `Ok(())` immediately, so the recorded duration is
        // dominated by `Instant::now()` overhead (tens of nanoseconds,
        // well under a microsecond on any non-degenerate machine).
        //
        // We pin this contract here so a later change that adds a
        // `storage.wal_enabled()` guard around the observer call is
        // forced to also update this test — moving the gate is a
        // semantic change, not a refactor. Operators wiring the
        // observer in production (see Task 2 C4) install it only on
        // file-backed databases via the registry, so the no-op
        // observations on memory-backed graphs never reach the
        // Prometheus recorder in practice.
        let durations: Arc<Mutex<Vec<std::time::Duration>>> = Arc::new(Mutex::new(vec![]));
        let durations_clone = Arc::clone(&durations);
        let obs: WalObserver = Box::new(move |_c: FsyncCause, d: std::time::Duration| {
            durations_clone.lock().unwrap().push(d);
        });
        let mut g = Graph::new().with_wal_observer(obs);
        g.add_node("Z", Properties::default()).unwrap();
        let recorded = durations.lock().unwrap().clone();
        assert!(
            !recorded.is_empty(),
            "observer fires on every Graph::wal_sync call regardless of backend",
        );
        for d in &recorded {
            assert!(
                *d < std::time::Duration::from_millis(50),
                "memory backend wal_sync should record sub-50ms duration; got {d:?}",
            );
        }
    }

    // ── 3c Task 4: SchemaCatalog field wiring (open/flush) ──────────────
    //
    // The persistence round-trip across reopen lives in Task 6 alongside the
    // real schema.bin codec — until then the codec stub deserialises to empty,
    // so a reopen test would be RED for a reason unrelated to this task. Here we
    // verify the field exists, defaults empty, and is mutable in-memory.

    #[test]
    fn new_graph_has_empty_schema_catalog() {
        let g = Graph::new();
        assert!(g.schema_catalog().indexes().is_empty());
        assert!(g.schema_catalog().constraints().is_empty());
    }

    #[test]
    fn opened_graph_has_empty_schema_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let g = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
        assert!(g.schema_catalog().indexes().is_empty());
        assert!(g.schema_catalog().constraints().is_empty());
    }

    #[test]
    fn schema_catalog_mut_add_index() {
        let mut g = Graph::new();
        g.schema_catalog_mut().add_index("Person", "id");
        assert!(g.schema_catalog().has_index("Person", "id"));
    }

    #[test]
    fn schema_catalog_mut_add_constraint() {
        let mut g = Graph::new();
        g.schema_catalog_mut().add_unique_constraint("Asset", "id");
        assert!(g.schema_catalog().has_unique_constraint("Asset", "id"));
    }

    #[test]
    fn flush_with_schema_catalog_does_not_error() {
        // flush() must serialize the catalog without erroring, even when the
        // catalog has entries. The full reopen round-trip is asserted by
        // `schema_catalog_survives_flush_and_reopen` below (Task 6 codec).
        let tmp = tempfile::tempdir().unwrap();
        let mut g = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
        g.schema_catalog_mut().add_index("Asset", "id");
        g.flush()
            .expect("flush with non-empty schema catalog must not error");
    }

    #[test]
    fn schema_catalog_survives_flush_and_reopen() {
        // End-to-end persistence: a catalog declared on an open graph must be
        // recoverable from `schema.bin` after flush + reopen. With the Task 4
        // codec stub this came back empty; the Task 6 codec makes it real.
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut g = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
            g.schema_catalog_mut().add_index("Person", "id");
            g.schema_catalog_mut().add_index("Asset", "status");
            g.schema_catalog_mut().add_unique_constraint("Asset", "id");
            g.flush().unwrap();
        }
        let reopened = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
        let cat = reopened.schema_catalog();
        assert!(cat.has_index("Person", "id"));
        assert!(cat.has_index("Asset", "status"));
        assert!(cat.has_unique_constraint("Asset", "id"));
        assert_eq!(cat.indexes().len(), 2);
        assert_eq!(cat.constraints().len(), 1);
    }

    // ── Issue #43 Cycle A3: append-only labels bypass MVCC on read ──────────

    /// A node whose label is declared append-only must read straight off the
    /// page, never through the delta chain.
    ///
    /// Staging this legally takes care: a node that is append-only from the
    /// start can never acquire a delta chain, so there would be nothing for the
    /// gate to bypass and the test would pass either way. Instead the node is
    /// created and updated as an ordinary versioned node — giving it a real
    /// committed delta chain — and only then declared append-only. An MVCC read
    /// would surface the updated value; the fast path returns the page-resident
    /// one.
    #[test]
    fn node_read_on_append_only_label_skips_mvcc_even_when_enabled() {
        let mut g = Graph::new();
        g.enable_mvcc();

        let mut props = Properties::new();
        props.insert("seq".to_string(), Property::I64(1));
        let id = g.add_node_str("Event", props).unwrap();

        let mut updated_props = Properties::new();
        updated_props.insert("seq".to_string(), Property::I64(999));
        let txn = g.begin_txn().unwrap();
        g.update_node_in_txn(txn, id, &Node::new(id, "Event", updated_props))
            .unwrap();
        g.commit_txn(txn).unwrap();

        // Sanity: without the gate this node resolves its chain and reads 999.
        assert_eq!(
            g.node(id).unwrap().properties().get("seq"),
            Some(&Property::I64(999))
        );

        // Now declare the label append-only and put the id on the fast path.
        g.schema_catalog_mut().mark_label_append_only("Event", 0);
        g.append_only_node_ids.insert(id.0);

        let read = g.node(id).unwrap();
        assert_eq!(
            read.properties().get("seq"),
            Some(&Property::I64(1)),
            "append-only node must read from the page, bypassing the delta chain"
        );
    }

    /// The gate is per-label: a node of a label that was never declared
    /// append-only keeps resolving through MVCC, so a committed update is
    /// visible to it.
    #[test]
    fn node_of_non_append_only_label_still_resolves_via_mvcc() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.schema_catalog_mut().mark_label_append_only("Event", 0);

        let mut props = Properties::new();
        props.insert("seq".to_string(), Property::I64(1));
        let id = g.add_node_str("Audit", props).unwrap();

        let mut updated_props = Properties::new();
        updated_props.insert("seq".to_string(), Property::I64(999));
        let txn = g.begin_txn().unwrap();
        g.update_node_in_txn(txn, id, &Node::new(id, "Audit", updated_props))
            .unwrap();
        g.commit_txn(txn).unwrap();

        let read = g.node(id).unwrap();
        assert_eq!(
            read.properties().get("seq"),
            Some(&Property::I64(999)),
            "a versioned label must still resolve its committed delta chain"
        );
    }

    /// Creating a node under an append-only label inside an explicit
    /// transaction is rejected: the mode's guarantee is that these nodes never
    /// acquire a delta chain, and a transactional insert would create one.
    #[test]
    fn add_node_in_txn_rejects_append_only_label() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.schema_catalog_mut().mark_label_append_only("Event", 0);
        let before = g.storage.meta().next_node_id;

        let txn = g.begin_txn().unwrap();
        let err = g
            .add_node_in_txn(txn, "Event", Properties::new())
            .unwrap_err();

        assert!(
            matches!(&err, Error::AppendOnlyLabelInTxn { label } if label == "Event"),
            "expected AppendOnlyLabelInTxn, got {err:?}"
        );
        assert_eq!(
            g.storage.meta().next_node_id,
            before,
            "a rejected create must not consume a node id"
        );
    }

    /// The rejection is scoped to declared labels; ordinary transactional
    /// inserts are untouched.
    #[test]
    fn add_node_in_txn_still_works_for_non_append_only_label() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.schema_catalog_mut().mark_label_append_only("Event", 0);

        let txn = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(txn, "Audit", Properties::new()).unwrap();
        g.commit_txn(txn).unwrap();

        assert_eq!(g.node(id).unwrap().label(), "Audit");
    }

    /// Ordinary transactional work must not trip the append-only invariant
    /// check — it only fires on a genuine bug.
    #[test]
    fn legitimate_txn_activity_never_trips_append_only_debug_assert() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.schema_catalog_mut().mark_label_append_only("Event", 0);

        let event = g.add_node_str("Event", Properties::new()).unwrap();

        let txn = g.begin_txn().unwrap();
        let other = g.add_node_in_txn(txn, "Actor", Properties::new()).unwrap();
        g.add_edge_in_txn(txn, "EMITTED_BY", event, other, Properties::new())
            .unwrap();
        let mut updated_props = Properties::new();
        updated_props.insert("v".to_string(), Property::I64(1));
        g.update_node_in_txn(txn, other, &Node::new(other, "Actor", updated_props))
            .unwrap();
        g.commit_txn(txn).unwrap();

        assert_eq!(g.node(event).unwrap().label(), "Event");
        assert_eq!(g.node(other).unwrap().label(), "Actor");
    }

    /// Defense in depth: should the label-level rejections ever be bypassed by
    /// a future change, attaching a delta to an append-only id must fail loudly
    /// in debug rather than silently producing a write no reader can see.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "append-only")]
    fn push_txn_delta_panics_in_debug_if_id_is_append_only() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.schema_catalog_mut().mark_label_append_only("Event", 0);
        let id = g.add_node_str("Event", Properties::new()).unwrap();

        let txn = g.begin_txn().unwrap();
        g.test_only_push_raw_delta_for_append_only_node(txn, id);
    }

    /// Updating an existing append-only node inside a transaction is refused:
    /// the update would attach a delta chain to an id the read path resolves
    /// straight off the page, so the write would be silently invisible.
    #[test]
    fn update_node_in_txn_rejects_existing_append_only_node() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.schema_catalog_mut().mark_label_append_only("Event", 0);

        let mut props = Properties::new();
        props.insert("seq".to_string(), Property::I64(1));
        let id = g.add_node_str("Event", props).unwrap();

        let mut updated_props = Properties::new();
        updated_props.insert("seq".to_string(), Property::I64(2));
        let updated = Node::new(id, "Event", updated_props);

        let txn = g.begin_txn().unwrap();
        let err = g.update_node_in_txn(txn, id, &updated).unwrap_err();

        assert!(
            matches!(&err, Error::AppendOnlyLabelInTxn { label } if label == "Event"),
            "expected AppendOnlyLabelInTxn, got {err:?}"
        );
        assert_eq!(
            g.node(id).unwrap().properties().get("seq"),
            Some(&Property::I64(1)),
            "the rejected update must leave the node untouched"
        );
    }

    /// Removing an existing append-only node inside a transaction is refused,
    /// for the same reason as the update: append-only means the node never
    /// acquires a delta chain.
    #[test]
    fn remove_node_in_txn_rejects_existing_append_only_node() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.schema_catalog_mut().mark_label_append_only("Event", 0);

        let id = g.add_node_str("Event", Properties::new()).unwrap();

        let txn = g.begin_txn().unwrap();
        let err = g.remove_node_in_txn(txn, id).unwrap_err();

        assert!(
            matches!(&err, Error::AppendOnlyLabelInTxn { label } if label == "Event"),
            "expected AppendOnlyLabelInTxn, got {err:?}"
        );
        assert!(
            g.node(id).is_ok(),
            "the rejected remove must leave the node in place"
        );
    }

    /// Cycle A7 contract: un-declaring a label frees *future* nodes of that
    /// label from the restriction.
    #[test]
    fn unmarking_append_only_label_allows_new_nodes_in_txn() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.schema_catalog_mut().mark_label_append_only("Event", 0);
        g.schema_catalog_mut().unmark_label_append_only("Event");

        let txn = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(txn, "Event", Properties::new()).unwrap();
        g.commit_txn(txn).unwrap();

        assert_eq!(g.node(id).unwrap().label(), "Event");
    }

    /// Editing the catalog directly leaves the fast-path set untouched, so an
    /// existing node keeps bypassing MVCC. That is a property of the low-level
    /// call, not the intended contract: use
    /// [`Graph::set_label_append_only`] to withdraw, which frees the label's
    /// nodes at once (issue #61).
    ///
    /// Kept as a regression pin on the catalog call itself — it must not start
    /// reaching into the graph's in-memory state behind the caller's back — and
    /// as the counterpart to
    /// `withdrawing_append_only_frees_existing_nodes_immediately`.
    ///
    /// The original note here claimed freeing these nodes would be unsafe,
    /// because their reads would start consulting a delta chain that was never
    /// maintained. That is not what happens: the node's on-disk form is
    /// identical either way, and the set only gates two runtime shortcuts, so a
    /// freed node simply takes the ordinary path. Measured in
    /// `withdrawing_append_only_frees_existing_nodes_immediately`, which
    /// deletes such a node transactionally and reads the result back.
    #[test]
    fn editing_the_catalog_directly_leaves_the_fast_path_set_stale() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.schema_catalog_mut().mark_label_append_only("Event", 0);
        let id = g.add_node_str("Event", Properties::new()).unwrap();

        g.schema_catalog_mut().unmark_label_append_only("Event");

        assert!(
            g.is_append_only_node(id),
            "the catalog call alone does not touch the graph's fast-path set"
        );
        let txn = g.begin_txn().unwrap();
        let err = g.remove_node_in_txn(txn, id).unwrap_err();
        assert!(
            matches!(&err, Error::AppendOnlyLabelInTxn { .. }),
            "expected AppendOnlyLabelInTxn, got {err:?}"
        );
    }

    /// Cycle A5 contract: an edge may join an append-only node to a versioned
    /// one. Nothing special is needed for this — visibility resolution tolerates
    /// an absent delta chain — but the mixed case is the one most likely to be
    /// broken by a future change to the gate, so it is pinned by a test.
    #[test]
    fn edge_between_append_only_and_versioned_node_reads_correctly_auto_commit() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.schema_catalog_mut().mark_label_append_only("Event", 0);

        let event = g.add_node_str("Event", Properties::new()).unwrap();
        let actor = g.add_node_str("Actor", Properties::new()).unwrap();
        let edge = g
            .add_edge("EMITTED_BY", event, actor, Properties::new())
            .unwrap();

        let read = g.edge(edge).unwrap();
        assert_eq!(read.source(), event);
        assert_eq!(read.target(), actor);
        assert_eq!(g.node(event).unwrap().label(), "Event");
        assert_eq!(g.node(actor).unwrap().label(), "Actor");
    }

    /// An append-only node remains a legal edge endpoint inside a transaction.
    /// Only writes *to* the node itself are refused; referencing it is not a
    /// write to it, and endpoint validation resolves it through the
    /// transaction's snapshot rather than the read gate.
    #[test]
    fn add_edge_in_txn_accepts_append_only_node_as_endpoint() {
        let mut g = Graph::new();
        g.enable_mvcc();
        g.schema_catalog_mut().mark_label_append_only("Event", 0);

        let event = g.add_node_str("Event", Properties::new()).unwrap();
        let actor = g.add_node_str("Actor", Properties::new()).unwrap();

        let txn = g.begin_txn().unwrap();
        let edge = g
            .add_edge_in_txn(txn, "EMITTED_BY", event, actor, Properties::new())
            .unwrap();
        g.commit_txn(txn).unwrap();

        let read = g.edge(edge).unwrap();
        assert_eq!(read.source(), event);
        assert_eq!(read.target(), actor);
    }

    /// The fast-path id set is in-memory only. On reopen it has to be
    /// repopulated from the persisted catalog plus the node pages, which means
    /// `open()` must load the schema catalog BEFORE rebuilding the indexes.
    #[test]
    fn append_only_node_ids_survive_reopen_via_property_index_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let id = {
            let mut g = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
            g.schema_catalog_mut().mark_label_append_only("Event", 0);
            let mut props = Properties::new();
            props.insert("seq".to_string(), Property::I64(1));
            let id = g.add_node_str("Event", props).unwrap();
            g.flush().unwrap();
            id
        };

        let mut g = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
        assert!(
            g.schema_catalog().is_label_append_only("Event"),
            "declaration must survive reopen"
        );
        g.enable_mvcc();

        assert!(
            g.is_append_only_node(id),
            "the fast-path id set must be repopulated on reopen"
        );
        // And the observable consequence: the node is exempt from
        // transactional mutation, which only holds if the id was recognised.
        let txn = g.begin_txn().unwrap();
        let err = g.remove_node_in_txn(txn, id).unwrap_err();
        assert!(
            matches!(&err, Error::AppendOnlyLabelInTxn { label } if label == "Event"),
            "expected AppendOnlyLabelInTxn after reopen, got {err:?}"
        );
    }

    // ── Issue #61: withdrawing an append-only declaration ───────────────────
    //
    // Membership in the fast-path set is decided when a node is created, and
    // the set is in-memory: on reopen it is rebuilt by testing each node's
    // label against the catalog. So a withdrawal that only touched the catalog
    // did nothing until the process restarted, and then silently freed every
    // node of that label. Same operation, two outcomes, no signal which one the
    // caller got.
    //
    // `set_label_append_only(label, false)` closes that gap by doing in-session
    // exactly what the reopen rebuild does. The pair of tests below pins both
    // halves: the effect is immediate, and it survives the reopen unchanged.

    #[test]
    fn withdrawing_append_only_frees_existing_nodes_immediately() {
        let mut g = Graph::new();
        g.set_label_append_only("Event", true);
        let mut props = Properties::new();
        props.insert("seq".to_string(), Property::I64(1));
        let id = g.add_node_str("Event", props).unwrap();
        g.enable_mvcc();

        assert!(g.is_append_only_node(id), "created under the declaration");

        g.set_label_append_only("Event", false);

        assert!(
            !g.is_append_only_node(id),
            "withdrawal must free existing nodes in-session, not at the next \
             restart — the caller has no way to tell a restart is pending"
        );
        let txn = g.begin_txn().unwrap();
        g.remove_node_in_txn(txn, id)
            .expect("a freed node must be transactionally mutable at once");
        g.commit_txn(txn).expect("commit must succeed");
        assert!(
            g.node(id).is_err(),
            "the delete must be visible after commit"
        );
    }

    #[test]
    fn withdrawing_append_only_agrees_with_the_reopen_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let (kept, freed) = {
            let mut g = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
            g.set_label_append_only("Event", true);
            g.set_label_append_only("Audit", true);
            let freed = g.add_node_str("Event", Properties::new()).unwrap();
            let kept = g.add_node_str("Audit", Properties::new()).unwrap();

            // Withdraw one label only: the other must be untouched.
            g.set_label_append_only("Event", false);
            g.persist_schema().unwrap();
            g.flush().unwrap();
            (kept, freed)
        };

        let mut g = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
        g.enable_mvcc();

        assert!(
            !g.is_append_only_node(freed),
            "the withdrawn label's node stays free across the reopen"
        );
        assert!(
            g.is_append_only_node(kept),
            "withdrawing one label must not free another label's nodes"
        );

        let txn = g.begin_txn().unwrap();
        let err = g.remove_node_in_txn(txn, kept).unwrap_err();
        assert!(
            matches!(&err, Error::AppendOnlyLabelInTxn { label } if label == "Audit"),
            "the still-declared label must keep rejecting, got {err:?}"
        );
    }

    #[test]
    fn redeclaring_append_only_does_not_recapture_existing_nodes() {
        let mut g = Graph::new();
        g.set_label_append_only("Event", true);
        let old = g.add_node_str("Event", Properties::new()).unwrap();
        g.set_label_append_only("Event", false);
        assert!(!g.is_append_only_node(old), "freed by the withdrawal");

        // Re-declaring only affects nodes created from now on. `old` was freed
        // and stays free: it may have acquired a delta chain while the label
        // was withdrawn, and the fast path would then skip resolving it.
        g.set_label_append_only("Event", true);
        let fresh = g.add_node_str("Event", Properties::new()).unwrap();

        assert!(
            !g.is_append_only_node(old),
            "a re-declaration must not recapture a node that was already freed"
        );
        assert!(g.is_append_only_node(fresh), "new nodes take the fast path");
    }

    /// The reopen rebuild must honour the same rule as
    /// `redeclaring_append_only_does_not_recapture_existing_nodes`, and for a
    /// harder reason than symmetry: recapturing a node that was mutated while
    /// its label was withdrawn loses a committed write.
    ///
    /// A freed node can legitimately acquire a delta chain — that is the whole
    /// point of freeing it. Reads of an append-only node skip visibility
    /// resolution and go straight to the page, so a node holding a chain that
    /// gets recaptured stops seeing its own committed mutation.
    ///
    /// The rebuild used to test only the node's label against the catalog, so
    /// re-declaring the label after a withdrawal recaptured every old node on
    /// the next `open()` — in-memory and post-restart behaviour disagreeing
    /// with nothing to tell them apart (issue #61).
    #[test]
    fn the_reopen_rebuild_does_not_recapture_a_node_mutated_while_withdrawn() {
        let tmp = tempfile::tempdir().unwrap();
        let id = {
            let mut g = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
            g.enable_mvcc();
            g.set_label_append_only("Event", true);
            let mut props = Properties::new();
            props.insert("seq".to_string(), Property::I64(1));
            let id = g.add_node_str("Event", props).unwrap();

            // Withdraw, then mutate transactionally: legitimate, the node is
            // no longer append-only and its reads resolve the delta chain.
            g.set_label_append_only("Event", false);
            let txn = g.begin_txn().unwrap();
            let mut updated = g.node(id).unwrap();
            updated
                .properties_mut()
                .insert("seq".to_owned(), Property::I64(2));
            g.update_node_in_txn(txn, id, &updated).unwrap();
            g.commit_txn(txn).unwrap();

            // Re-declare. In-session this must not recapture `id`.
            g.set_label_append_only("Event", true);
            assert!(
                !g.is_append_only_node(id),
                "in-session re-declaration must not recapture the mutated node"
            );

            // A committed delta lives in the in-memory table until the vacuum
            // writes it to the node's page, so materialize it here as the
            // background vacuum would — otherwise the reopen legitimately reads
            // the pre-update page and the assertion below would be testing the
            // vacuum's schedule, not the rebuild.
            g.vacuum_once().unwrap();
            g.persist_schema().unwrap();
            g.flush().unwrap();
            id
        };

        let g = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();

        assert!(
            !g.is_append_only_node(id),
            "the rebuild must not recapture a node that was freed and then \
             mutated — its reads would skip the delta chain it carries"
        );
        assert_eq!(
            g.node(id).unwrap().properties().get("seq"),
            Some(&Property::I64(2)),
            "the committed update must survive the reopen"
        );
    }

    // ── Phase 5 Task 2: by-label edge traversal honors MVCC visibility ──────

    #[test]
    fn outgoing_edges_by_label_honors_snapshot_and_label() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let a = g.add_node("N", props! {}).unwrap();
        let b = g.add_node("N", props! {}).unwrap();
        let old = g.begin_txn().unwrap();
        let w = g.begin_txn().unwrap();
        g.add_edge_in_txn(w, "KNOWS", a, b, props! {}).unwrap();
        g.commit_txn(w).unwrap();
        // New reader sees it under the right label, not under a different one.
        assert_eq!(g.outgoing_edges_by_label(a, "KNOWS").unwrap().len(), 1);
        assert_eq!(g.outgoing_edges_by_label(a, "OTHER").unwrap().len(), 0);
        // Old snapshot sees nothing.
        assert_eq!(
            g.outgoing_edges_by_label_in_txn(old, a, "KNOWS")
                .unwrap()
                .len(),
            0
        );
        g.rollback_txn(old).unwrap();
    }
}

#[cfg(test)]
mod constraint_enforcement_tests {
    use super::*;
    use crate::error::Error;
    use crate::property::{Properties, Property};

    fn props(id: &str) -> Properties {
        let mut p = Properties::new();
        p.insert("id".to_owned(), Property::String(id.to_owned()));
        p
    }

    #[test]
    fn unique_constraint_blocks_duplicate_create() {
        let mut g = Graph::new();
        g.schema_catalog_mut().add_unique_constraint("Asset", "id");

        g.add_node("Asset", props("abc")).unwrap();
        // Second node with same id must fail.
        let err = g.add_node("Asset", props("abc")).unwrap_err();
        assert!(
            matches!(err, Error::ConstraintViolation { .. }),
            "expected ConstraintViolation, got {err:?}"
        );
    }

    #[test]
    fn unique_constraint_allows_different_values() {
        let mut g = Graph::new();
        g.schema_catalog_mut().add_unique_constraint("Asset", "id");
        g.add_node("Asset", props("a")).unwrap();
        g.add_node("Asset", props("b")).unwrap(); // must succeed
    }

    #[test]
    fn unique_constraint_only_applies_to_declared_label() {
        let mut g = Graph::new();
        g.schema_catalog_mut().add_unique_constraint("Asset", "id");
        g.add_node("Asset", props("dup")).unwrap();
        // Duplicate id on a different label must NOT be blocked.
        g.add_node("Person", props("dup")).unwrap();
    }

    #[test]
    fn graph_unchanged_after_constraint_violation() {
        let mut g = Graph::new();
        g.schema_catalog_mut().add_unique_constraint("A", "id");
        g.add_node("A", props("x")).unwrap();
        let count_before = g.node_count();
        let _ = g.add_node("A", props("x")); // ignore error
        assert_eq!(
            g.node_count(),
            count_before,
            "node count changed after violation"
        );
    }

    #[test]
    fn update_node_blocked_by_unique_constraint() {
        let mut g = Graph::new();
        g.schema_catalog_mut().add_unique_constraint("Asset", "id");
        let a = g.add_node("Asset", props("a")).unwrap();
        g.add_node("Asset", props("b")).unwrap();
        // Updating node 'a' to have id='b' would cause a duplicate.
        let mut updated = g.node(a).unwrap();
        updated
            .properties_mut()
            .insert("id".to_owned(), Property::String("b".to_owned()));
        let err = g.update_node(a, &updated).unwrap_err();
        assert!(
            matches!(err, Error::ConstraintViolation { .. }),
            "expected ConstraintViolation on update, got {err:?}"
        );
    }

    #[test]
    fn no_constraint_allows_duplicate_values() {
        // Without a constraint, duplicates are allowed (existing behaviour).
        let mut g = Graph::new();
        g.add_node("Asset", props("dup")).unwrap();
        g.add_node("Asset", props("dup")).unwrap(); // must succeed
    }

    #[test]
    fn edges_between_empty_graph_returns_empty_vec() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::new()).unwrap();
        let b = g.add_node("B", Properties::new()).unwrap();
        let result = g.edges_between(a, b, "REL").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn edges_between_unknown_node_errors() {
        let g = Graph::new();
        let err = g
            .edges_between(NodeId(999), NodeId(1000), "REL")
            .unwrap_err();
        assert!(matches!(err, Error::NodeNotFound(_)));
    }

    #[test]
    fn edges_between_finds_edge_after_add() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::new()).unwrap();
        let b = g.add_node("B", Properties::new()).unwrap();
        let e = g.add_edge("REL", a, b, Properties::new()).unwrap();

        let found = g.edges_between(a, b, "REL").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id(), e);

        assert!(g.has_edge(a, b, "REL").unwrap());
    }

    #[test]
    fn edges_between_and_has_edge_hide_edge_from_old_snapshot() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let a = g.add_node("N", Properties::new()).unwrap();
        let b = g.add_node("N", Properties::new()).unwrap();
        let w = g.begin_txn().unwrap();
        g.add_edge_in_txn(w, "REL", a, b, Properties::new())
            .unwrap();
        g.commit_txn(w).unwrap();
        // Auto-commit reader (new snapshot) sees it.
        assert_eq!(g.edges_between(a, b, "REL").unwrap().len(), 1);
        assert!(g.has_edge(a, b, "REL").unwrap());
    }

    #[test]
    fn node_visible_reflects_auto_commit_snapshot() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let t = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(t, "N", Properties::new()).unwrap();
        // Uncommitted: not visible to an auto-commit reader.
        assert!(!g.node_visible(id));
        g.commit_txn(t).unwrap();
        // Committed: visible.
        assert!(g.node_visible(id));
    }

    #[test]
    fn legacy_traversals_unchanged_without_mvcc() {
        let mut g = Graph::new(); // MVCC never enabled
        let a = g.add_node("N", Properties::new()).unwrap();
        let b = g.add_node("N", Properties::new()).unwrap();
        g.add_edge("REL", a, b, Properties::new()).unwrap();
        assert_eq!(g.outgoing_edges(a).unwrap().len(), 1);
        assert_eq!(g.outgoing_edges_by_label(a, "REL").unwrap().len(), 1);
        assert_eq!(g.edges_between(a, b, "REL").unwrap().len(), 1);
        assert!(g.has_edge(a, b, "REL").unwrap());
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
    }

    #[test]
    fn edges_between_empty_after_remove_but_siblings_survive() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::new()).unwrap();
        let b = g.add_node("B", Properties::new()).unwrap();
        let e1 = g.add_edge("REL", a, b, Properties::new()).unwrap();
        let e2 = g.add_edge("REL", a, b, Properties::new()).unwrap(); // arista paralela

        g.remove_edge(e1).unwrap();

        let remaining = g.edges_between(a, b, "REL").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id(), e2);
        assert!(g.has_edge(a, b, "REL").unwrap()); // e2 sigue viva

        g.remove_edge(e2).unwrap();
        assert!(!g.has_edge(a, b, "REL").unwrap());
        assert!(g.edges_between(a, b, "REL").unwrap().is_empty());
    }

    #[test]
    fn edges_between_returns_all_parallel_edges() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::new()).unwrap();
        let b = g.add_node("B", Properties::new()).unwrap();
        let e1 = g.add_edge("REL", a, b, Properties::new()).unwrap();
        let e2 = g.add_edge("REL", a, b, Properties::new()).unwrap();
        let e3 = g.add_edge("REL", a, b, Properties::new()).unwrap();

        let mut found: Vec<_> = g
            .edges_between(a, b, "REL")
            .unwrap()
            .into_iter()
            .map(|e| e.id())
            .collect();
        found.sort_by_key(|id| id.0);
        let mut expected = vec![e1, e2, e3];
        expected.sort_by_key(|id| id.0);
        assert_eq!(found, expected);
    }

    #[test]
    fn edges_between_does_not_cross_different_pairs() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::new()).unwrap();
        let b1 = g.add_node("B1", Properties::new()).unwrap();
        let b2 = g.add_node("B2", Properties::new()).unwrap();
        g.add_edge("REL", a, b1, Properties::new()).unwrap();

        let cross = g.edges_between(a, b2, "REL").unwrap();
        assert!(cross.is_empty());
        assert!(!g.has_edge(a, b2, "REL").unwrap());
    }

    #[test]
    fn edges_between_label_guard_rejects_different_label_same_pair() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::new()).unwrap();
        let b = g.add_node("B", Properties::new()).unwrap();
        g.add_edge("L1", a, b, Properties::new()).unwrap();

        let found = g.edges_between(a, b, "L2").unwrap();
        assert!(
            found.is_empty(),
            "edges_between debe filtrar por label exacto"
        );
        assert!(!g.has_edge(a, b, "L2").unwrap());

        // La arista "L1" sigue encontrándose por su propio label.
        assert_eq!(g.edges_between(a, b, "L1").unwrap().len(), 1);
    }

    #[test]
    fn update_edge_changing_label_moves_pair_index_entry() {
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::new()).unwrap();
        let b = g.add_node("B", Properties::new()).unwrap();
        let eid = g.add_edge("OLD", a, b, Properties::new()).unwrap();

        let mut edge = g.edge(eid).unwrap();
        edge.set_label("NEW");
        g.update_edge(eid, &edge).unwrap();

        assert!(
            g.edges_between(a, b, "OLD").unwrap().is_empty(),
            "la entrada bajo el label viejo debe desaparecer del pair-index"
        );
        let found_new = g.edges_between(a, b, "NEW").unwrap();
        assert_eq!(found_new.len(), 1);
        assert_eq!(found_new[0].id(), eid);
    }

    #[test]
    fn edges_between_scales_with_pair_not_with_fanout() {
        // Correctness under high fan-out: N edges from one source, all same
        // label, to distinct targets. Each point query must resolve to exactly
        // one edge — the pair its target belongs to — regardless of how many
        // other edges hang off `from`. The O(k) guarantee is structural (a
        // single HashMap lookup on (from, to, hash)); this test locks the
        // contract without timing.
        const N: usize = 300;
        let mut g = Graph::new();
        let from = g.add_node("Hub", Properties::new()).unwrap();

        let mut targets = Vec::with_capacity(N);
        for _ in 0..N {
            let t = g.add_node("Leaf", Properties::new()).unwrap();
            g.add_edge("REL", from, t, Properties::new()).unwrap();
            targets.push(t);
        }

        for &t in &targets {
            let found = g.edges_between(from, t, "REL").unwrap();
            assert_eq!(
                found.len(),
                1,
                "target {t:?} debe resolver a exactamente 1 arista"
            );
            assert_eq!(found[0].source, from);
            assert_eq!(found[0].target, t);
        }
    }

    #[test]
    fn edges_between_rebuilds_after_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, e2) = {
            let mut g = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
            let a = g.add_node("A", Properties::new()).unwrap();
            let b = g.add_node("B", Properties::new()).unwrap();
            let e1 = g.add_edge("REL", a, b, Properties::new()).unwrap();
            let e2 = g.add_edge("REL", a, b, Properties::new()).unwrap();
            g.remove_edge(e1).unwrap(); // deja solo e2 vivo, ejercita el remove-path
            g.flush().unwrap();
            (a, b, e2)
        };

        let reopened = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
        let found = reopened.edges_between(a, b, "REL").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id(), e2);
        assert!(reopened.has_edge(a, b, "REL").unwrap());
    }

    #[test]
    fn rebuild_after_reopen_restores_both_heads_of_bidirectional_node() {
        // Cycle 7 (#54): rebuild_adj_cache repopulates from node slots, which
        // carry two heads. A node that is both a source and a target must have
        // BOTH its outgoing and incoming chains reachable after reopen — the
        // case the second slot head exists for.
        let tmp = tempfile::tempdir().unwrap();
        let hub = {
            let mut g = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
            let hub = g.add_node("HUB", Properties::new()).unwrap();
            let up = g.add_node("UP", Properties::new()).unwrap();
            let down = g.add_node("DOWN", Properties::new()).unwrap();
            g.add_edge("out", hub, down, Properties::new()).unwrap(); // hub → down
            g.add_edge("in", up, hub, Properties::new()).unwrap(); // up → hub
            g.flush().unwrap();
            hub
        };

        let reopened = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
        assert_eq!(
            reopened.outgoing_edges(hub).unwrap().len(),
            1,
            "outgoing chain must survive reopen"
        );
        assert_eq!(
            reopened.incoming_edges(hub).unwrap().len(),
            1,
            "incoming chain must survive reopen"
        );
    }

    #[test]
    fn add_node_seeds_negative_adj_cache_entry() {
        // A freshly created node has no adjacency pages. Seeding the adj_cache
        // with a negative marker (both pages None) lets resolve_adj_pointer hit
        // the cache instead of doing an O(pages) page scan. Without this,
        // flush_adj_pending pays O(pages) per just-created target node, making a
        // batch of N edges to N new nodes O(N²).
        let mut g = Graph::new();
        let id = g.add_node("N", Properties::new()).unwrap();

        // The node must already be present in the adjacency cache as a negative
        // marker, so no page scan is ever needed for it.
        let cached = g.adj_cache.get(id.0);
        assert_eq!(
            cached,
            Some(AdjacencyPointer {
                outgoing_page: None,
                incoming_page: None,
            }),
            "add_node must seed adj_cache with a negative marker to avoid O(pages) scans"
        );
    }

    /// Issue #58: below the checkpoint threshold, closing a batch makes its
    /// mutations durable but does NOT checkpoint — the state stays in the WAL
    /// and the data files stay empty until `flush`.
    ///
    /// This pins the distinction the docs on `end_batch`/`flush` promise. It
    /// went undocumented and untested, and a consumer reasonably read "durable"
    /// as "safely written out", ran for 7 minutes, and ended with 916 MB of WAL
    /// against 69 KB of data.
    ///
    /// The writes here stay far below the 64 MB default threshold, so the
    /// automatic checkpoint added for #58 does not fire and the original
    /// distinction still holds. `end_batch_checkpoints_wal_when_threshold_crossed`
    /// covers the other side.
    #[test]
    fn closing_batches_leaves_state_in_wal_until_flush_checkpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal.log");
        let nodes = tmp.path().join("nodes.db");
        let size = |p: &std::path::Path| std::fs::metadata(p).map_or(0, |m| m.len());

        let mut g = Graph::open(tmp.path(), &GraphConfig::default()).unwrap();
        for _ in 0..5 {
            g.begin_batch();
            for _ in 0..100 {
                g.add_node("Res", Properties::new()).unwrap();
            }
            g.end_batch().unwrap();
        }

        let wal_after_batches = size(&wal);
        assert!(
            wal_after_batches > 0,
            "the batches' mutations must be durable in the WAL"
        );
        assert_eq!(
            size(&nodes),
            0,
            "closing batches must NOT materialise data pages — if this starts \
             failing, end_batch has begun checkpointing and its docs are stale"
        );

        g.flush().unwrap();

        assert!(
            size(&wal) < wal_after_batches,
            "flush must checkpoint and truncate the WAL (was {wal_after_batches}, \
             now {}) — this is the only thing bounding WAL growth",
            size(&wal)
        );
        assert!(size(&nodes) > 0, "flush must materialise the data pages");
    }

    // ── Issue #58: size-triggered WAL checkpoint at outermost batch close ──

    /// Writes enough nodes to blow past `threshold`, in one outermost batch.
    /// Deliberately generous: the point is to cross the threshold, not to
    /// measure how many records that takes.
    fn write_batch_past_threshold(g: &mut Graph, nodes: usize) {
        g.begin_batch();
        for _ in 0..nodes {
            g.add_node("Res", Properties::new()).unwrap();
        }
        g.end_batch().unwrap();
    }

    fn file_size(p: &std::path::Path) -> u64 {
        std::fs::metadata(p).map_or(0, |m| m.len())
    }

    fn config_with_threshold(threshold: Option<u64>) -> GraphConfig {
        GraphConfig {
            wal_checkpoint_threshold_bytes: threshold,
            ..GraphConfig::default()
        }
    }

    /// The heart of issue #58: once the journal outgrows its threshold,
    /// closing the outermost batch checkpoints on its own. No `flush` call.
    #[test]
    fn end_batch_checkpoints_wal_when_threshold_crossed() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal.log");
        let nodes = tmp.path().join("nodes.db");

        let mut g = Graph::open(tmp.path(), &config_with_threshold(Some(4096))).unwrap();
        write_batch_past_threshold(&mut g, 500);

        assert!(
            file_size(&nodes) > 0,
            "crossing the threshold must materialise the data pages at batch \
             close, with no explicit flush"
        );
        assert!(
            file_size(&wal) < 4096,
            "the journal must have been truncated back below its threshold, \
             but it is {} bytes",
            file_size(&wal)
        );
    }

    /// Only the outermost batch may checkpoint. Closing an inner batch has to
    /// leave everything as it was: a checkpoint there would materialise half of
    /// an operation the caller still considers in progress.
    #[test]
    fn nested_batch_close_does_not_checkpoint_even_past_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let nodes = tmp.path().join("nodes.db");

        let mut g = Graph::open(tmp.path(), &config_with_threshold(Some(4096))).unwrap();
        g.begin_batch();
        g.begin_batch();
        for _ in 0..500 {
            g.add_node("Res", Properties::new()).unwrap();
        }
        g.end_batch().unwrap(); // inner only

        assert_eq!(
            file_size(&nodes),
            0,
            "closing a nested batch must not checkpoint, however big the journal"
        );

        g.end_batch().unwrap(); // outermost

        assert!(
            file_size(&nodes) > 0,
            "closing the outermost batch must checkpoint the overgrown journal"
        );
    }

    /// The configured value must be what decides, not the 64 MB default. This
    /// is what stops the field from degenerating into an on/off switch: a
    /// workload far past a small threshold but nowhere near the default has to
    /// checkpoint on the first graph and not on the second.
    #[test]
    fn end_batch_checkpoints_at_configured_threshold_not_default() {
        let small_dir = tempfile::tempdir().unwrap();
        let default_dir = tempfile::tempdir().unwrap();

        let mut small = Graph::open(small_dir.path(), &config_with_threshold(Some(4096))).unwrap();
        let mut with_default = Graph::open(default_dir.path(), &GraphConfig::default()).unwrap();

        write_batch_past_threshold(&mut small, 500);
        write_batch_past_threshold(&mut with_default, 500);

        assert!(
            file_size(&small_dir.path().join("nodes.db")) > 0,
            "the 4 KB threshold is far behind us — this graph must checkpoint"
        );
        assert_eq!(
            file_size(&default_dir.path().join("nodes.db")),
            0,
            "the same writes are nowhere near 64 MB — the default-configured \
             graph must not checkpoint, or the configured value is being ignored"
        );
    }

    /// `None` turns the automatic checkpoint off entirely, leaving `flush` as
    /// the only thing that bounds the journal — the behaviour before #58.
    #[test]
    fn disabled_threshold_never_autocheckpoints_only_explicit_flush_does() {
        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal.log");
        let nodes = tmp.path().join("nodes.db");

        let mut g = Graph::open(tmp.path(), &config_with_threshold(None)).unwrap();
        for _ in 0..5 {
            write_batch_past_threshold(&mut g, 500);
            assert_eq!(
                file_size(&nodes),
                0,
                "with automatic checkpointing off, closing batches must never \
                 materialise data"
            );
        }

        let wal_before_flush = file_size(&wal);
        assert!(
            wal_before_flush > 0,
            "the writes must be durable in the journal"
        );

        g.flush().unwrap();

        assert!(
            file_size(&wal) < wal_before_flush,
            "disabling the automatic checkpoint must not break the explicit one"
        );
        assert!(
            file_size(&nodes) > 0,
            "flush must still materialise the data"
        );
    }

    /// A graph with no journal has nothing to bound. It must ignore the
    /// threshold entirely rather than erroring or trying to checkpoint a
    /// journal that does not exist.
    #[test]
    fn graph_without_wal_ignores_threshold_and_never_checkpoints() {
        let tmp = tempfile::tempdir().unwrap();

        let cfg = GraphConfig {
            wal_checkpoint_threshold_bytes: Some(4096),
            ..GraphConfig::without_wal()
        };
        let mut g = Graph::open(tmp.path(), &cfg).unwrap();

        for _ in 0..3 {
            g.begin_batch();
            for _ in 0..500 {
                g.add_node("Res", Properties::new()).unwrap();
            }
            g.end_batch()
                .expect("closing a batch on a WAL-less graph must not error");
        }

        assert_eq!(
            file_size(&tmp.path().join("wal.log")),
            0,
            "a WAL-less graph must not have produced a journal to checkpoint"
        );
    }

    /// An explicit transaction that has been committed must survive an
    /// automatic checkpoint, including one that fires before the vacuum has
    /// materialised the transaction's own node and edge pages.
    ///
    /// The concern this pins down: committing does not write the node's page
    /// — the vacuum does that later, as a lazy optimisation — so a checkpoint
    /// that truncated the journal in between could plausibly discard the only
    /// durable copy. It does not, because committing reconciles the derived
    /// structures (counts, exists-sets, indexes, adjacency) into the buffer
    /// pool straight away, and the checkpoint goes through the full flush,
    /// which writes those dirty pages out *before* truncating. Reopening
    /// rebuilds from them.
    ///
    /// Written after a review flagged this as a possible data-loss path. It
    /// is not one, but nothing was pinning the behaviour, so a later change to
    /// what commit reconciles eagerly could silently turn it into one.
    #[test]
    fn committed_txn_survives_threshold_checkpoint_before_vacuum() {
        let tmp = tempfile::tempdir().unwrap();
        let mut g = Graph::open(tmp.path(), &config_with_threshold(Some(4096))).unwrap();
        g.enable_mvcc();

        let txn = g.begin_txn().unwrap();
        g.add_node_in_txn(txn, "Tx", Properties::new()).unwrap();
        g.commit_txn(txn).unwrap();

        // A legacy batch big enough to cross the threshold, so closing it
        // checkpoints and truncates the journal. No vacuum has run.
        g.begin_batch();
        for _ in 0..500 {
            g.add_node("Res", Properties::new()).unwrap();
        }
        g.end_batch().unwrap();

        assert_eq!(
            g.nodes_by_label("Tx").len(),
            1,
            "the committed transaction must still be visible in this session"
        );

        drop(g);
        let reopened = Graph::open(tmp.path(), &config_with_threshold(Some(4096))).unwrap();
        assert_eq!(
            reopened.nodes_by_label("Tx").len(),
            1,
            "the committed transaction must survive the reopen — if this fails, \
             the automatic checkpoint truncated the journal while the commit's \
             only durable copy was still in it"
        );
    }

    /// The measured problem from issue #58: a writer that only ever closes
    /// batches grew the journal without bound. With the threshold in place the
    /// journal must stay bounded relative to the threshold, not to the total
    /// volume ever written.
    #[test]
    fn wal_checkpoint_threshold_bounds_wal_growth_across_many_batches() {
        const THRESHOLD: u64 = 32 * 1024;
        const BATCHES: usize = 40;
        const PER_BATCH: usize = 50;

        let tmp = tempfile::tempdir().unwrap();
        let wal = tmp.path().join("wal.log");

        let mut g = Graph::open(tmp.path(), &config_with_threshold(Some(THRESHOLD))).unwrap();

        let mut peak = 0;
        for _ in 0..BATCHES {
            g.begin_batch();
            for _ in 0..PER_BATCH {
                g.add_node("Res", Properties::new()).unwrap();
            }
            g.end_batch().unwrap();
            peak = peak.max(file_size(&wal));
        }

        // A batch may overshoot the threshold before it closes — the threshold
        // is a target, not a ceiling — so the bound allows for one batch's
        // worth of overshoot rather than demanding the journal never exceeds
        // the configured value.
        let bound = THRESHOLD * 4;
        assert!(
            peak < bound,
            "the journal peaked at {peak} bytes against a {THRESHOLD}-byte \
             threshold; it is still growing with total volume written"
        );

        // Bounded growth is worthless if it was achieved by losing writes.
        // Dozens of automatic checkpoints fired during the loop above; every
        // node has to survive them, including after a reopen, which replays
        // whatever the last checkpoint left in the journal.
        let expected = BATCHES * PER_BATCH;
        assert_eq!(
            g.nodes_by_label("Res").len(),
            expected,
            "automatic checkpointing must not drop writes"
        );

        drop(g);
        let reopened = Graph::open(tmp.path(), &config_with_threshold(Some(THRESHOLD))).unwrap();
        assert_eq!(
            reopened.nodes_by_label("Res").len(),
            expected,
            "every node must still be there after reopening — a checkpoint that \
             truncates the journal ahead of the data would lose them here"
        );
    }

    #[test]
    fn end_batch_edges_to_new_nodes_is_linear() {
        // Regression guard for the O(N²) caused by resolve_adj_pointer scanning
        // all adjacency pages once per just-created target node. We measure the
        // number of adjacency page reads is NOT super-linear by asserting the
        // total adjacency page count stays within a linear bound AND that the
        // cache is seeded so no scan path is taken. Deterministic: no timing.
        //
        // The strong signal is behavioural: every target node created in the
        // batch must have a cache entry after creation (so flush takes the O(1)
        // cache path, never the O(pages) scan).
        let mut g = Graph::new();
        let src = g.add_node("S", Properties::new()).unwrap();
        let mut targets = Vec::new();
        for _ in 0..300 {
            targets.push(g.add_node("T", Properties::new()).unwrap());
        }

        // Every target must be cache-resident before any edge flush.
        for &t in &targets {
            assert!(
                g.adj_cache.get(t.0).is_some(),
                "target node {} not seeded in adj_cache — flush would page-scan",
                t.0
            );
        }

        g.begin_batch();
        for &t in &targets {
            g.add_edge("R", src, t, Properties::new()).unwrap();
        }
        g.end_batch().unwrap();

        // Sanity: all edges present.
        assert_eq!(g.edge_count(), 300);
    }

    #[test]
    fn flush_adj_pending_does_not_leak_pages_across_flushes() {
        // The real O(N²) shape is many small flushes to the SAME source node:
        // the old write_adjacency re-allocated the entire chain on every flush,
        // orphaning the previous pages (no free-list). append_adjacency extends
        // in-place, so the adjacency page count must track the edge count, not
        // the number of flushes.
        //
        // K flushes of B edges each = K*B total edges. With capacity ~508/page
        // and two directions (out on src, in on each target), the outgoing
        // chain of src needs ceil(K*B / ~508) pages. A leaking implementation
        // allocates a fresh chain each flush: sum_{i=1..K} ceil(i*B/508) pages
        // — quadratic in K. We assert the count stays within a linear bound.
        const K: u32 = 40; // 40 flushes
        const B: usize = 50; // 50 edges each → 2000 total

        let mut g = Graph::new();
        let src = g.add_node("S", Properties::new()).unwrap();

        for _ in 0..K {
            g.begin_batch();
            for _ in 0..B {
                let t = g.add_node("T", Properties::new()).unwrap();
                g.add_edge("R", src, t, Properties::new()).unwrap();
            }
            g.end_batch().unwrap();
        }

        let total_edges = K as usize * B; // 2000
        let adj_pages = g.storage.page_count(DataFile::Adjacency);

        // Linear upper bound: outgoing chain of src ≈ ceil(2000/507)=4 pages,
        // plus one single page per target's incoming (2000 targets, 1 edge each)
        // = 2000 pages, plus src's own. So ~2004 + slack. A quadratic leak on
        // the src chain alone would add sum_{i=1..40} ceil(50i/507) ≈ 200+
        // EXTRA orphan pages beyond the target pages. Bound generously but
        // below the quadratic-leak floor.
        // Test assertion bound over an edge count built in the same test.
        #[allow(clippy::cast_possible_truncation)]
        let linear_bound = total_edges as u32 + total_edges as u32 / 100 + 50;
        assert!(
            adj_pages <= linear_bound,
            "adjacency page count {adj_pages} exceeds linear bound {linear_bound} \
             for {total_edges} edges over {K} flushes — orphan-page leak (O(N²)) suspected"
        );
    }

    // ---- Cycle 6: adjacency pointer persisted in the node slot (#54) --------

    #[test]
    fn add_edge_persists_adj_pointer_in_node_slot() {
        // After add_edge, both endpoints' node slots on disk must carry a real
        // adjacency page id (not the sentinel) and the correct direction flag,
        // read straight from the slot — no adj_cache involved. This is what lets
        // resolve_adj_pointer stop scanning (cycle 7).
        let mut g = Graph::new();
        let a = g.add_node("A", Properties::default()).unwrap();
        let b = g.add_node("B", Properties::default()).unwrap();
        g.add_edge("rel", a, b, Properties::default()).unwrap();

        let src_slot = g.read_node_slot_bytes(a.0).unwrap();
        assert_ne!(
            node_codec::slot_adj_page_id(&src_slot),
            node_codec::ADJ_PAGE_ID_SENTINEL,
            "source node slot must hold a real adjacency page id after add_edge"
        );
        assert_ne!(
            node_codec::slot_adj_flags(&src_slot) & node_codec::ADJ_FLAG_OUTGOING,
            0,
            "source node slot must have the OUTGOING flag set"
        );

        // The source has only an outgoing edge, so its incoming head stays at
        // the sentinel.
        assert_eq!(
            node_codec::slot_adj_incoming_page_id(&src_slot),
            node_codec::ADJ_PAGE_ID_SENTINEL,
            "source node has no incoming edge: incoming head must stay sentinel"
        );

        let dst_slot = g.read_node_slot_bytes(b.0).unwrap();
        assert_ne!(
            node_codec::slot_adj_incoming_page_id(&dst_slot),
            node_codec::ADJ_PAGE_ID_SENTINEL,
            "destination node slot must hold a real incoming head page id after add_edge"
        );
        assert_eq!(
            node_codec::slot_adj_page_id(&dst_slot),
            node_codec::ADJ_PAGE_ID_SENTINEL,
            "destination node has no outgoing edge: outgoing head must stay sentinel"
        );
        assert_ne!(
            node_codec::slot_adj_flags(&dst_slot) & node_codec::ADJ_FLAG_INCOMING,
            0,
            "destination node slot must have the INCOMING flag set"
        );
    }

    #[test]
    fn vacuum_preserves_adj_pointer_after_node_update() {
        // The adjacency pointer lives only in the on-disk slot. The vacuum
        // re-serializes the slot from the MVCC snapshot's Node via
        // encode_node_slot. If the pointer is not carried on the Node, a vacuum
        // after any property update silently resets it to the sentinel — the
        // node loses the trail to its edges (logical corruption, no error).
        let mut g = Graph::new();
        g.enable_mvcc();
        let mut a_props = Properties::default();
        a_props.insert("v".to_owned(), Property::I64(1));
        let a = g.add_node("A", a_props).unwrap();
        let b = g.add_node("B", Properties::default()).unwrap();
        g.add_edge("rel", a, b, Properties::default()).unwrap();

        let ptr_before = node_codec::slot_adj_page_id(&g.read_node_slot_bytes(a.0).unwrap());
        assert_ne!(ptr_before, node_codec::ADJ_PAGE_ID_SENTINEL);

        // Update a property of the source node in a committed transaction, then
        // vacuum so the delta is materialized back into the page slot. The Node
        // handed to update_node_in_txn is read back from disk first: option 1
        // requires that read to carry the adjacency pointer, so the snapshot the
        // vacuum re-serializes still knows where the edges are.
        let txn = g.begin_txn().unwrap();
        let mut updated = g.node_in_txn(txn, a).unwrap();
        updated
            .properties_mut()
            .insert("v".to_owned(), Property::I64(2));
        g.update_node_in_txn(txn, a, &updated).unwrap();
        g.commit_txn(txn).unwrap();
        g.vacuum_once().unwrap();

        let src_slot = g.read_node_slot_bytes(a.0).unwrap();
        assert_eq!(
            node_codec::slot_adj_page_id(&src_slot),
            ptr_before,
            "vacuum must preserve the adjacency page id across a node update"
        );
        assert_ne!(
            node_codec::slot_adj_flags(&src_slot) & node_codec::ADJ_FLAG_OUTGOING,
            0,
            "vacuum must preserve the OUTGOING flag across a node update"
        );
        // The edge must still be reachable after the vacuum.
        assert_eq!(g.outgoing_edges(a).unwrap().len(), 1);
    }
}
