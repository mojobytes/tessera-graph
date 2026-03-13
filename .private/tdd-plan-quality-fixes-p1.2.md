# TDD Plan: Quality Fixes for Phase 1.2 (Concurrency & Transactions)

**Date**: 2026-03-13
**Scope**: `tessera-graph` (MIT) and `tessera-storage-enterprise` (proprietary)
**Branch**: `feature/quality-fixes-p1.2` from `develop`

---

## Context

Phase 1.2 delivered a working concurrency and transaction foundation. A code review
identified 13 issues across two crates. This plan resolves all of them via strictly
ordered TDD cycles. Every cycle follows the canonical RED -> GREEN -> REFACTOR
structure: write a failing test first, make it pass with the minimal correct
implementation, then clean up.

**Stack detected**: Rust 2024 edition, MSRV 1.85, `forbid(unsafe_code)`,
`deny(clippy::all)`, `warn(clippy::pedantic, clippy::nursery)`.

**Convenciones observadas**:
- Unit tests live in `#[cfg(test)] mod tests` at the bottom of each source file.
- Integration tests live in `tests/` directories at the crate root.
- All public APIs document panics, errors, and `#[must_use]` as appropriate.
- `pub(crate)` is the default visibility for cross-module internals.
- Lock poison paths use `expect("... lock poisoned")` — to be replaced by `?` once
  `EnterpriseError::LockPoisoned` exists (Cycle E-R5).

**Affects hot path**: YES — `commit()` / `rollback()` are called on every transaction
boundary. `AdjCache::get()` is called on every adjacency lookup. `BufferPool::get_page()`
is called on every node/edge read. Performance tests are mandatory.

---

## Dependency Order and Grouping

The issues cannot be tackled in arbitrary order. Dependencies are:

```
Enterprise C1 (WAL field)
    └── Enterprise C4 (Begin record)   -- needs WAL field to write from begin()
    └── Enterprise R5 (LockPoisoned)   -- needs error variant before propagating

Enterprise C3 (Arc<HashSet>)           -- independent of C1, touches committed field
Enterprise R6 (private fields)         -- independent, handle.rs only
Enterprise R7 (remove const fn)        -- trivial, snapshot.rs only
Enterprise R8 (concurrent test)        -- needs C1+C4 done (WAL as field)
Enterprise O9-O11 (polish)             -- independent

Graph C2 (PageBuf return, no PageRef)  -- independent of adj_cache
Graph C1 (clock-hand AdjCache::get)   -- independent of buffer_pool
Graph R1 (lock poison constants)       -- depends on Graph C1 being done (new code)
Graph R4 (gate throughput test)        -- independent, graph.rs only
Graph R5 (WAL recovery comment)        -- trivial, file.rs only
Graph R6 (remove #[allow(dead_code)])  -- depends on Graph C1 (new code)
```

**Final cycle order**:

| Cycle | Tag        | Title                                       | Crate         |
|-------|------------|---------------------------------------------|---------------|
| 1     | E-C1       | WAL as `Mutex<WalWriter>` field             | enterprise    |
| 2     | E-R5       | `LockPoisoned` error variant                | enterprise    |
| 3     | E-C4       | Write `WalRecord::Begin` in `begin()`       | enterprise    |
| 4     | E-C3       | `Arc<HashSet<u64>>` for O(1) snapshot clone | enterprise    |
| 5     | E-R6       | Private fields + `set_state()` accessor     | enterprise    |
| 6     | E-R7       | Remove `const fn` from `Snapshot::new`      | enterprise    |
| 7     | E-O9-O11   | `Display`/`Hash`/`Debug` polish             | enterprise    |
| 8     | E-R8       | Real concurrency integration test (16t)     | enterprise    |
| 9     | G-C2       | Return `PageBuf` from `get_page`, drop `PageRef` | graph    |
| 10    | G-C1       | Clock-hand eviction in `AdjCache`           | graph         |
| 11    | G-R1       | Centralize lock-poison message constants    | graph         |
| 12    | G-R4       | Gate throughput test with `#[ignore]`       | graph         |
| 13    | G-R5       | Improve WAL recovery comment                | graph         |
| 14    | G-R6       | Remove `#[allow(dead_code)]` on pub methods | graph         |
| 15    | WIRING     | End-to-end wiring verification              | both          |

---

## Cycle E-C1 — WAL as `Mutex<WalWriter>` field (CRITICAL)

**Files**:
- `crates/tessera-storage-enterprise/src/txn/manager.rs`
- `crates/tessera-storage-enterprise/src/error.rs`

**Problem**:
`commit()` calls `WalWriter::open(wal_path)` at line 88 and `rollback()` at line 121.
Every call re-reads the entire WAL file to recover `next_lsn` (see
`tessera-graph/src/wal/writer.rs` lines 87-108: `recover_next_lsn` reads the full
file). Two concurrent commits can open two `WalWriter` instances simultaneously,
producing interleaved byte streams and a corrupt WAL. The `wal_path: &Path`
parameter leaks storage topology into the API contract.

**Current signatures** (manager.rs lines 79-131):
```rust
pub fn commit(&self, handle: &mut TransactionHandle, wal_path: &Path) -> Result<()>
pub fn rollback(&self, handle: &mut TransactionHandle, wal_path: &Path) -> Result<()>
```

**Current struct** (manager.rs lines 16-19):
```rust
pub struct TransactionManager {
    next_txn_id: AtomicU64,
    committed: RwLock<HashSet<u64>>,
}
```

### RED — Write Failing Tests (15 min)

Add to `manager.rs` `#[cfg(test)] mod tests`:

```rust
// Verifies constructor returns Result and WAL path is encapsulated.
#[test]
fn open_constructs_manager_with_wal() {
    let tmp = NamedTempFile::new().unwrap();
    let mgr = TransactionManager::open(tmp.path()).unwrap();
    // If open() returns Ok, the WAL field is properly held.
    let mut h = mgr.begin(IsolationLevel::ReadCommitted);
    // commit() no longer takes wal_path — this must compile without wal_path arg.
    mgr.commit(&mut h).unwrap();
}

#[test]
fn commit_without_wal_path_arg() {
    let tmp = NamedTempFile::new().unwrap();
    let mgr = TransactionManager::open(tmp.path()).unwrap();
    let mut h = mgr.begin(IsolationLevel::ReadCommitted);
    assert!(mgr.commit(&mut h).is_ok());
}

#[test]
fn rollback_without_wal_path_arg() {
    let tmp = NamedTempFile::new().unwrap();
    let mgr = TransactionManager::open(tmp.path()).unwrap();
    let mut h = mgr.begin(IsolationLevel::ReadCommitted);
    assert!(mgr.rollback(&mut h).is_ok());
}

// Concurrent commits must not corrupt the WAL (serialised via Mutex).
#[test]
fn concurrent_commits_are_serialised() {
    use std::sync::Arc;
    use std::thread;

    let tmp = NamedTempFile::new().unwrap();
    let mgr = Arc::new(TransactionManager::open(tmp.path()).unwrap());

    let threads: Vec<_> = (0..8).map(|_| {
        let mgr = Arc::clone(&mgr);
        thread::spawn(move || {
            let mut h = mgr.begin(IsolationLevel::ReadCommitted);
            mgr.commit(&mut h).unwrap();
        })
    }).collect();

    for t in threads { t.join().unwrap(); }
    // If WAL was corrupted, re-opening would fail or return wrong lsn.
    // The manager itself must still report 8 committed ids.
    assert_eq!(
        mgr.committed.lock().unwrap().len(),  // direct field access in test
        8
    );
}
```

These tests **fail to compile** because:
1. `TransactionManager::open` does not exist (only `new()`).
2. `commit()` and `rollback()` signatures still require `wal_path: &Path`.
3. `committed` is `RwLock<HashSet<u64>>` not `Mutex<...>`.

### GREEN — Minimal Correct Implementation (25 min)

**Changes to `manager.rs`**:

1. Add `use std::sync::Mutex;` import.
2. Change `committed` field type from `RwLock<HashSet<u64>>` to
   `RwLock<HashSet<u64>>` (kept) — only WAL field is added; `committed` stays
   `RwLock` for read-heavy `is_committed()`.
3. Add `wal: Mutex<WalWriter>` field.
4. Add `TransactionManager::open(path: &Path) -> Result<Self>` — returns `Err`
   if `WalWriter::open` fails.
5. Keep `TransactionManager::new()` as a convenience for tests that don't need
   WAL — it panics if called: remove `Default` impl entirely, forcing callers
   to use `open()`. (Alternative: keep `new()` for in-memory-only testing by
   wrapping an in-memory WalWriter — this is cleaner. See note below.)
6. Remove `wal_path: &Path` from `commit()` and `rollback()`.
7. Lock `self.wal` via `self.wal.lock()` inside `commit()` and `rollback()`.

**Note on `new()` removal**: The existing unit tests and integration test use
`TransactionManager::new()` and pass `wal_path` to `commit`/`rollback`. All
those call sites must be updated in this same GREEN step to use
`TransactionManager::open(tmp.path())` instead. The `Default` impl is removed.

**New struct**:
```rust
pub struct TransactionManager {
    next_txn_id: AtomicU64,
    committed: RwLock<HashSet<u64>>,
    wal: Mutex<WalWriter>,
}
```

**New constructor**:
```rust
/// Opens or creates a `TransactionManager` backed by a WAL at `path`.
///
/// # Errors
///
/// Returns an I/O error if the WAL file cannot be opened or created.
pub fn open(path: &Path) -> Result<Self> {
    let wal = WalWriter::open(path).map_err(EnterpriseError::Graph)?;
    Ok(Self {
        next_txn_id: AtomicU64::new(1),
        committed: RwLock::new(HashSet::new()),
        wal: Mutex::new(wal),
    })
}
```

**Updated `commit()`** (no `wal_path` param, locks `self.wal`):
```rust
pub fn commit(&self, handle: &mut TransactionHandle) -> Result<()> {
    if handle.state != TxnState::Active {
        return Err(EnterpriseError::TransactionNotActive(handle.txn_id));
    }
    let mut wal = self.wal.lock().expect("wal lock poisoned");
    wal.append(WalRecord::Commit { lsn: 0, txn_id: handle.txn_id })
        .map_err(EnterpriseError::Graph)?;
    wal.sync().map_err(EnterpriseError::Graph)?;
    drop(wal);

    self.committed
        .write()
        .expect("commit log lock poisoned")
        .insert(handle.txn_id);
    handle.state = TxnState::Committed;
    Ok(())
}
```

**Updated `rollback()`**: same pattern, no `wal_path`, lock `self.wal`.

**Update all existing test call sites** in `manager.rs` and
`tests/txn_integration.rs` to use `TransactionManager::open(tmp.path())` and
drop the `wal_path` argument from `commit()`/`rollback()` calls.

### REFACTOR (10 min)

- Verify `TransactionManager` is still `Send + Sync` — `Mutex<WalWriter>` is
  `Send + Sync` as long as `WalWriter` is `Send`. Confirm `WalWriter` has no
  `Rc` or non-Send fields (it holds `File` and `u64` — both `Send`).
- Remove the now-dead `use std::path::Path;` import from `manager.rs` (it was
  only used in `commit`/`rollback` signatures).
- Run `cargo clippy` in enterprise crate — fix any new lint.

**Estimated time**: 50 min total.

---

## Cycle E-R5 — `LockPoisoned` error variant (RECOMMENDED)

**Files**:
- `crates/tessera-storage-enterprise/src/error.rs`
- `crates/tessera-storage-enterprise/src/txn/manager.rs`

**Problem**: `expect("... lock poisoned")` at manager.rs lines 53, 98, 142
panics the thread instead of returning a recoverable `Result`. After Cycle E-C1
adds `self.wal.lock().expect(...)`, there are four panic sites.

### RED — Write Failing Tests (10 min)

Add to `error.rs` tests:

```rust
#[test]
fn lock_poisoned_formats_message() {
    use std::fmt::Write as _;
    let e = EnterpriseError::LockPoisoned("commit log");
    let mut s = String::new();
    write!(s, "{e}").unwrap();
    assert!(s.contains("commit log"));
}
```

Add to `manager.rs` tests:

```rust
// After a panic in one thread, subsequent operations must return
// LockPoisoned instead of propagating the panic to other threads.
#[test]
fn poisoned_wal_lock_returns_error() {
    use std::sync::Arc;
    use std::thread;

    let tmp = NamedTempFile::new().unwrap();
    let mgr = Arc::new(TransactionManager::open(tmp.path()).unwrap());
    let mgr2 = Arc::clone(&mgr);

    // Poison the WAL lock by panicking while holding it.
    let _ = thread::spawn(move || {
        let _guard = mgr2.wal.lock().unwrap(); // hold lock
        panic!("intentional poison");
    }).join(); // join() returns Err — the thread panicked

    // Now commit() must return LockPoisoned instead of panicking.
    let mut h = mgr.begin(IsolationLevel::ReadCommitted);
    let result = mgr.commit(&mut h);
    assert!(matches!(result, Err(EnterpriseError::LockPoisoned(_))));
}
```

Note: the test accesses `mgr.wal` directly — either make the field
`pub(crate)` for testing, or expose a `#[cfg(test)]` helper. Making
`wal: pub(crate) Mutex<WalWriter>` is simpler and consistent with how
`committed` is accessed in existing tests.

### GREEN — Minimal Correct Implementation (20 min)

**`error.rs`**: Add variant:
```rust
/// A lock was poisoned by a panicking thread.
#[error("lock poisoned: {0}")]
LockPoisoned(&'static str),
```

**`manager.rs`**: Replace every `expect("... lock poisoned")` with:
```rust
self.wal.lock().map_err(|_| EnterpriseError::LockPoisoned("wal"))?
self.committed.write().map_err(|_| EnterpriseError::LockPoisoned("commit log"))?
self.committed.read().map_err(|_| EnterpriseError::LockPoisoned("commit log"))?
```

This propagates four sites:
- Line 53 in `begin()` — `committed.read()`
- Equivalent of old line 98 in `commit()` — `committed.write()`
- New WAL mutex in `commit()` — `wal.lock()`
- New WAL mutex in `rollback()` — `wal.lock()`
- Line 142 in `is_committed()` — `committed.read()`

**Note**: `begin()` currently returns `TransactionHandle` (infallible). After
this change, the `committed.read()` in `begin()` can return `LockPoisoned`.
Change `begin()` return type to `Result<TransactionHandle>`. This cascades to
all callers in tests and integration tests — update them to `.unwrap()` or `?`.

### REFACTOR (10 min)

- Audit all `expect()` calls remaining in both files — none should remain.
- Update existing test code that calls `mgr.begin(...)` without `?` to use
  `mgr.begin(...).unwrap()`.

**Estimated time**: 40 min total.

---

## Cycle E-C4 — Write `WalRecord::Begin` in `begin()` (CRITICAL)

**Files**:
- `crates/tessera-storage-enterprise/src/txn/manager.rs`

**Problem**: `begin()` at lines 45-66 does not write a `WalRecord::Begin` to
the WAL. Recovery code in `tessera-graph/src/storage/file.rs` lines 207-213
already handles `WalRecord::Begin` by ignoring it, so the record type exists.
Without it, a crash between `begin()` and `commit()`/`rollback()` leaves no
trace of the transaction — recovery cannot determine which writes were
in-flight and need to be rolled back at the application level.

**Dependency**: Requires Cycle E-C1 (WAL as field) and E-R5 (`begin()` returns
`Result`) to be complete.

### RED — Write Failing Test (10 min)

Add to `manager.rs` tests:

```rust
// begin() must write a WAL record so recovery can detect in-flight txns.
#[test]
fn begin_writes_wal_begin_record() {
    use tessera_graph::WalReader;

    let tmp = NamedTempFile::new().unwrap();
    let mgr = TransactionManager::open(tmp.path()).unwrap();
    let h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
    let txn_id = h.txn_id();

    // Force flush so we can read the WAL file from scratch.
    drop(mgr); // WalWriter is dropped, OS flushes write buffer

    // Re-open and read records.
    let reader = WalReader::open(tmp.path()).unwrap();
    let records: Vec<_> = reader.records().collect();

    // Must contain exactly one Begin record with the right txn_id.
    let found = records.iter().any(|r| matches!(r,
        tessera_graph::WalRecord::Begin { txn_id: id, .. } if *id == txn_id
    ));
    assert!(found, "WAL must contain a Begin record for txn {txn_id}");
}
```

This test fails because `begin()` does not call `wal.append(WalRecord::Begin {...})`.

### GREEN — Minimal Correct Implementation (15 min)

In `begin()`, after allocating `txn_id` and before building the snapshot, acquire
the WAL lock and append a `Begin` record:

```rust
pub fn begin(&self, isolation: IsolationLevel) -> Result<TransactionHandle> {
    let txn_id = self.next_txn_id.fetch_add(1, Ordering::Relaxed);

    // Write Begin record before capturing snapshot (atomicity boundary).
    {
        let mut wal = self.wal.lock().map_err(|_| EnterpriseError::LockPoisoned("wal"))?;
        wal.append(WalRecord::Begin { lsn: 0, txn_id })
            .map_err(EnterpriseError::Graph)?;
        // No sync here — Begin is advisory; durability comes from Commit/Rollback sync.
    }

    let snapshot = match isolation {
        IsolationLevel::SnapshotIsolation => {
            let committed = self.committed.read()
                .map_err(|_| EnterpriseError::LockPoisoned("commit log"))?
                .clone();
            Some(Snapshot::new(committed, txn_id))
        }
        IsolationLevel::ReadCommitted => None,
    };

    Ok(TransactionHandle {
        txn_id,
        isolation,
        state: TxnState::Active,
        snapshot,
    })
}
```

**Important**: Do NOT `sync()` after `Begin`. The `Commit`/`Rollback` records are
the durability points. This keeps `begin()` fast.

Update all test callers of `begin()` (now returns `Result`) to add `.unwrap()`.

### REFACTOR (5 min)

- Confirm `WalRecord::Begin` variant signature matches what is imported. From
  `tessera-graph/src/wal/record.rs` the variant is `Begin { lsn: u64, txn_id: u64 }`.
- Confirm `tessera_graph::WalRecord` is in scope in manager.rs (already imported
  at line 6: `use tessera_graph::{WalRecord, WalWriter};`).

**Estimated time**: 30 min total.

---

## Cycle E-C3 — `Arc<HashSet<u64>>` for O(1) snapshot clone (RECOMMENDED)

**Files**:
- `crates/tessera-storage-enterprise/src/txn/manager.rs`
- `crates/tessera-storage-enterprise/src/txn/snapshot.rs`

**Problem**: `begin()` with `SnapshotIsolation` clones the entire `HashSet<u64>`
under a read lock (manager.rs lines 50-54). With N committed transactions, this
is O(N) work under the lock. Using `Arc<HashSet<u64>>` makes `begin()` O(1):
`Arc::clone` is a single atomic increment.

### RED — Write Failing Test (10 min)

Add to `manager.rs` tests:

```rust
// With many committed txns, begin(SnapshotIsolation) is O(1)
// by using Arc::clone instead of HashSet::clone.
// Verify that snapshots taken at different times see different committed sets.
#[test]
fn arc_snapshot_isolation_sees_correct_commits() {
    let tmp = NamedTempFile::new().unwrap();
    let mgr = TransactionManager::open(tmp.path()).unwrap();

    // Commit 1000 transactions.
    for _ in 0..1_000 {
        let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        mgr.commit(&mut h).unwrap();
    }

    // T_snap begins — its snapshot Arc points to the 1000-element set.
    let t_snap = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
    let committed_count_at_begin = t_snap
        .snapshot()
        .unwrap()
        .committed_count(); // new method on Snapshot

    assert_eq!(committed_count_at_begin, 1_000);

    // New commits after t_snap began must NOT be visible in t_snap's snapshot.
    let mut t_after = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
    mgr.commit(&mut t_after).unwrap();

    // t_snap's snapshot still sees 1000, not 1001.
    assert_eq!(
        t_snap.snapshot().unwrap().committed_count(),
        1_000,
        "snapshot must be immutable after begin"
    );
}
```

The test fails because `Snapshot::committed_count()` does not exist yet, and
because the `Arc<HashSet>` change hasn't been made.

### GREEN — Minimal Correct Implementation (20 min)

**`snapshot.rs`**: Change field type and add helper method:

```rust
use std::sync::Arc;

pub struct Snapshot {
    committed_at_begin: Arc<HashSet<u64>>,
    owner_txn_id: u64,
}

impl Snapshot {
    pub(crate) fn new(committed_at_begin: Arc<HashSet<u64>>, owner_txn_id: u64) -> Self {
        Self { committed_at_begin, owner_txn_id }
    }

    pub fn is_visible(&self, writer_txn_id: u64) -> bool {
        writer_txn_id == self.owner_txn_id
            || self.committed_at_begin.contains(&writer_txn_id)
    }

    /// Returns the number of committed transactions visible to this snapshot.
    #[must_use]
    pub fn committed_count(&self) -> usize {
        self.committed_at_begin.len()
    }
}
```

**`manager.rs`**: Change the `committed` field and snapshot creation:

```rust
committed: RwLock<Arc<HashSet<u64>>>,

// In constructor:
committed: RwLock::new(Arc::new(HashSet::new())),

// In begin() for SnapshotIsolation:
let arc = Arc::clone(
    &self.committed.read().map_err(|_| EnterpriseError::LockPoisoned("commit log"))?
);
Some(Snapshot::new(arc, txn_id))

// In commit(), update committed by creating a new Arc:
let mut guard = self.committed.write()
    .map_err(|_| EnterpriseError::LockPoisoned("commit log"))?;
let mut new_set = (**guard).clone();    // clone the HashSet once per commit
new_set.insert(handle.txn_id);
*guard = Arc::new(new_set);
```

**Note**: Commit is O(N) in the number of committed transactions — this is the
correct trade-off. The hot path is `begin()` (called frequently in concurrent
workloads), and it is now O(1). Each commit replaces the Arc atomically.

**Update `snapshot.rs` tests**: The existing tests construct `Snapshot::new`
directly with `HashSet` — update to wrap in `Arc::new(...)`.

### REFACTOR (10 min)

- Add `use std::sync::Arc;` to `snapshot.rs` and `manager.rs`.
- Remove the now-dead `committed_at_begin: HashSet<u64>` path in snapshot tests.
- Confirm `Snapshot` is still `Clone` if needed (it is if `Arc<HashSet>` is Clone).
  Add `#[derive(Clone)]` to `Snapshot` if it was not there before.

**Estimated time**: 40 min total.

---

## Cycle E-R6 — Private fields + `set_state()` accessor (RECOMMENDED)

**Files**:
- `crates/tessera-storage-enterprise/src/txn/handle.rs`
- `crates/tessera-storage-enterprise/src/txn/manager.rs`

**Problem**: `TransactionHandle` fields are `pub(crate)` (lines 29-33 of handle.rs).
Only `manager.rs` needs to mutate `state`. The fields `txn_id`, `isolation`, and
`snapshot` are read-only after construction and can become private with the
existing `const fn` accessors covering external reads.

### RED — Write Failing Test (5 min)

Add to `handle.rs` tests:

```rust
#[test]
fn set_state_transitions_correctly() {
    // Construct via manager so fields are private.
    // This test only checks that set_state works via the method.
    // We build a handle manually here only to test the method directly.
    let mut h = TransactionHandle::new_for_test(10, IsolationLevel::ReadCommitted);
    assert_eq!(h.state(), TxnState::Active);
    h.set_state(TxnState::Committed);
    assert_eq!(h.state(), TxnState::Committed);
}
```

Add `#[cfg(test)]` constructor `new_for_test` to `handle.rs` to avoid exposing
a public constructor.

### GREEN — Minimal Correct Implementation (15 min)

**`handle.rs`**: Make fields private, add mutating accessor:

```rust
pub struct TransactionHandle {
    txn_id: u64,
    isolation: IsolationLevel,
    state: TxnState,
    snapshot: Option<Snapshot>,
}

impl TransactionHandle {
    /// Sets the transaction state. Only `TransactionManager` should call this.
    pub(crate) fn set_state(&mut self, new_state: TxnState) {
        self.state = new_state;
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(txn_id: u64, isolation: IsolationLevel) -> Self {
        Self { txn_id, isolation, state: TxnState::Active, snapshot: None }
    }
}
```

**`manager.rs`**: Replace all direct field assignments to `handle.state` with
`handle.set_state(...)`. There are two sites:
- `commit()`: `handle.state = TxnState::Committed;` -> `handle.set_state(TxnState::Committed);`
- `rollback()`: `handle.state = TxnState::RolledBack;` -> `handle.set_state(TxnState::RolledBack);`

Also replace struct literal construction in `begin()`:
```rust
Ok(TransactionHandle {
    txn_id,
    isolation,
    state: TxnState::Active,
    snapshot,
})
```
This is `pub(crate)` construction from within the same crate — it still works
because `txn/manager.rs` and `txn/handle.rs` are in the same crate. No change
needed here as long as the struct literal is inside `crate::txn::manager`.

**Verify**: `manager.rs` is in `crate::txn::manager`, `handle.rs` is in
`crate::txn::handle`. Both are in the same crate, so private fields are
accessible within the crate via struct literal in `manager.rs`. This is correct
Rust visibility semantics.

### REFACTOR (5 min)

- Confirm no external code (e.g., integration tests) accesses `.txn_id`, `.state`,
  `.snapshot`, `.isolation` directly. They must all use the public accessor methods.
- Replace any remaining `.txn_id` field accesses in test code with `.txn_id()`.

**Estimated time**: 25 min total.

---

## Cycle E-R7 — Remove `const fn` from `Snapshot::new` (RECOMMENDED)

**Files**:
- `crates/tessera-storage-enterprise/src/txn/snapshot.rs`

**Problem**: `Snapshot::new` at line 14 of snapshot.rs is declared `pub(crate) const fn`.
`HashSet` cannot be used in a `const` context; the compiler currently accepts this
only because `Snapshot::new` is never called from a `const` context. After Cycle E-C3
changes the field to `Arc<HashSet<u64>>`, `const fn` is doubly invalid (`Arc::new`
is not `const`). Even before E-C3, the `const fn` is misleading and unsound.

### RED — Write Failing Test (5 min)

This is a purely compilability fix. The RED phase is:

After removing `const`, verify the existing snapshot tests still compile and pass.
Write a new doc-level test that uses `Snapshot::new` in a non-const context to confirm
no regressions. The "failing test" in this cycle is: the compiler warning/error you
would get if you tried `const S: Snapshot = Snapshot::new(...)` after the change.

Add a negative compile-test comment:
```rust
// Snapshot::new is intentionally NOT const — HashSet/Arc cannot be used in
// const context. Do not add const fn here.
```

### GREEN — Minimal Correct Implementation (5 min)

Remove `const` keyword from line 14 of snapshot.rs:

Before: `pub(crate) const fn new(committed_at_begin: HashSet<u64>, owner_txn_id: u64) -> Self {`

After (with E-C3 changes already applied):
`pub(crate) fn new(committed_at_begin: Arc<HashSet<u64>>, owner_txn_id: u64) -> Self {`

If E-C3 has not been applied yet, remove `const` from the `HashSet<u64>` version.

### REFACTOR (2 min)

Run `cargo clippy` on the enterprise crate. No further changes expected.

**Estimated time**: 12 min total.

---

## Cycle E-O9-O11 — Polish: `Display`/`Hash`, `Snapshot::Debug`, `lsn:0` comment (OPTIONAL)

**Files**:
- `crates/tessera-storage-enterprise/src/txn/handle.rs` (IsolationLevel Display, Hash)
- `crates/tessera-storage-enterprise/src/txn/snapshot.rs` (Debug derive)
- `crates/tessera-storage-enterprise/src/txn/manager.rs` (lsn: 0 comment)

### RED — Write Failing Tests (10 min)

```rust
// handle.rs tests
#[test]
fn isolation_level_display() {
    use std::fmt::Write as _;
    let mut s = String::new();
    write!(s, "{}", IsolationLevel::ReadCommitted).unwrap();
    assert_eq!(s, "ReadCommitted");
    s.clear();
    write!(s, "{}", IsolationLevel::SnapshotIsolation).unwrap();
    assert_eq!(s, "SnapshotIsolation");
}

#[test]
fn isolation_level_hashable() {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    map.insert(IsolationLevel::ReadCommitted, 1_u32);
    map.insert(IsolationLevel::SnapshotIsolation, 2_u32);
    assert_eq!(*map.get(&IsolationLevel::ReadCommitted).unwrap(), 1);
}

// snapshot.rs tests
#[test]
fn snapshot_debug_does_not_panic() {
    let snap = Snapshot::new(Arc::new(HashSet::new()), 42);
    let _ = format!("{snap:?}"); // must not panic
}
```

### GREEN — Minimal Correct Implementation (10 min)

**`handle.rs`**:
- Add `Hash` to `IsolationLevel` derive: `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]`
- Implement `Display` for `IsolationLevel`:

```rust
impl std::fmt::Display for IsolationLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadCommitted => f.write_str("ReadCommitted"),
            Self::SnapshotIsolation => f.write_str("SnapshotIsolation"),
        }
    }
}
```

**`snapshot.rs`**:
- Add `#[derive(Debug)]` to `Snapshot`. The `Arc<HashSet<u64>>` and `u64` fields
  both implement `Debug`.

**`manager.rs`**:
- Add comment above `lsn: 0` in `commit()`, `rollback()`, and `begin()`:
  ```rust
  // lsn: 0 — the WalWriter overwrites this with the real LSN in append().
  wal.append(WalRecord::Commit { lsn: 0, txn_id: handle.txn_id })
  ```

### REFACTOR (5 min)

No structural changes. Verify with `cargo clippy`.

**Estimated time**: 25 min total.

---

## Cycle E-R8 — Real Concurrency Integration Test (RECOMMENDED)

**Files**:
- `crates/tessera-storage-enterprise/tests/txn_integration.rs`

**Problem**: The existing integration test `concurrent_transactions_have_unique_ids`
(lines 71-82) never calls `commit()`. It tests ID generation in isolation. There is
no test verifying concurrent `begin()`/`commit()` pairs produce correct WAL records
and leave the manager in a consistent state.

**Dependency**: Requires Cycles E-C1, E-C4, and E-R5 to be complete (WAL as field,
`begin()` writes `Begin` record, `begin()` returns `Result`).

### RED — Write Failing Test (15 min)

Add to `tests/txn_integration.rs`:

```rust
#[test]
fn sixteen_threads_concurrent_begin_commit() {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::thread;
    use tessera_storage_enterprise::txn::{IsolationLevel, TransactionManager};

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mgr = Arc::new(TransactionManager::open(tmp.path()).unwrap());
    const THREAD_COUNT: usize = 16;

    let handles: Vec<_> = (0..THREAD_COUNT)
        .map(|_| {
            let mgr = Arc::clone(&mgr);
            thread::spawn(move || {
                let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
                let id = h.txn_id();
                mgr.commit(&mut h).unwrap();
                id
            })
        })
        .collect();

    let ids: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // All IDs must be unique.
    let unique: HashSet<u64> = ids.iter().copied().collect();
    assert_eq!(unique.len(), THREAD_COUNT, "duplicate txn IDs detected");

    // All must be committed.
    for id in &ids {
        assert!(mgr.is_committed(*id), "txn {id} not in commit log");
    }

    // Committed set count must match exactly.
    assert_eq!(
        mgr.committed_count(), // new helper method
        THREAD_COUNT,
        "committed set has wrong size"
    );
}

#[test]
fn sixteen_threads_concurrent_begin_rollback() {
    use std::sync::Arc;
    use std::thread;
    use tessera_storage_enterprise::txn::{IsolationLevel, TransactionManager, TxnState};

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mgr = Arc::new(TransactionManager::open(tmp.path()).unwrap());

    let handles: Vec<_> = (0..16)
        .map(|_| {
            let mgr = Arc::clone(&mgr);
            thread::spawn(move || {
                let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
                mgr.rollback(&mut h).unwrap();
                assert_eq!(h.state(), TxnState::RolledBack);
            })
        })
        .collect();

    for h in handles { h.join().unwrap(); }

    // None were committed.
    assert_eq!(mgr.committed_count(), 0);
}
```

These tests fail because `TransactionManager::committed_count()` does not exist.

### GREEN — Minimal Correct Implementation (10 min)

Add to `manager.rs`:

```rust
/// Returns the number of committed transactions.
///
/// # Errors
///
/// Returns `LockPoisoned` if the lock is poisoned.
pub fn committed_count(&self) -> Result<usize> {
    Ok(self.committed
        .read()
        .map_err(|_| EnterpriseError::LockPoisoned("commit log"))?
        .len())
}
```

Update integration tests to use `mgr.committed_count().unwrap()`.

### REFACTOR (5 min)

- Run the tests under `cargo test -- --test-threads=1` to confirm no flakiness.
- Run again without the flag (parallel) to surface any remaining races.
- Update `is_committed()` to return `Result<bool>` for consistency (propagates
  `LockPoisoned`). Update callers to add `.unwrap()` or `?`.

**Throughput regression check for the hot path**:

Add to `manager.rs` tests:

```rust
/// Regression guard: commit throughput must exceed 10,000 ops/s in debug mode.
/// In release mode the floor is much higher but we don't gate on it in unit tests
/// (Criterion benchmarks own that contract).
#[test]
fn commit_throughput_regression_guard() {
    use std::time::Instant;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mgr = TransactionManager::open(tmp.path()).unwrap();
    let n = 500_u64;
    let start = Instant::now();
    for _ in 0..n {
        let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
        mgr.commit(&mut h).unwrap();
    }
    let elapsed = start.elapsed();
    #[allow(clippy::cast_precision_loss)]
    let ops_per_sec = n as f64 / elapsed.as_secs_f64();
    let floor = if cfg!(debug_assertions) { 200.0 } else { 2_000.0 };
    assert!(
        ops_per_sec > floor,
        "commit throughput regression: {ops_per_sec:.0} ops/s < {floor:.0} ops/s"
    );
}
```

**Estimated time**: 30 min total.

---

## Cycle G-C2 — Return `PageBuf` from `get_page`, remove `PageRef` (CRITICAL)

**Files**:
- `tessera-graph/src/storage/buffer_pool.rs`
- `tessera-graph/src/storage/file.rs` (callers of `get_page`)

**Problem**: `get_page()` returns `PageRef<'_>` (lines 57-70 of buffer_pool.rs),
a guard holding an `RwLockReadGuard`. The race is:

1. Thread A: acquires write lock, loads page, releases write lock (end of scope at
   line 136 of buffer_pool.rs).
2. Thread B: evicts that page (write lock, evict, release).
3. Thread A: acquires read lock (line 139). The key is gone — `borrow.frames[&self.key]`
   in `PageRef::Deref` panics because `HashMap::index` panics on missing key.

The fix is to return `PageBuf` (a copy) instead of holding a lock reference.
`FileBackend::read_page()` already makes this copy at lines 398-401:
```rust
let page = self.pool.get_page(file, page_id)?;
let mut copy = new_page_buf();
copy.copy_from_slice(page.as_ref());
drop(page);
Ok(copy)
```
With `get_page()` returning `PageBuf` directly, this copy is made once inside
the pool under the write lock, the lock is released, and callers own the data.

### RED — Write Failing Tests (15 min)

Add to `buffer_pool.rs` tests:

```rust
// get_page() must return an owned PageBuf, not a guard.
// This test verifies that two reads can happen without the second
// causing a deadlock (which the guard approach causes if not dropped first).
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
        assert_eq!(page[0], 0xAA);
    });

    let p1 = Arc::clone(&pool);
    let t1 = thread::spawn(move || {
        let page: PageBuf = p1.get_page(DataFile::Nodes, 1).unwrap();
        assert_eq!(page[1 % PAGE_SIZE], 0xBB);
    });

    t0.join().unwrap();
    t1.join().unwrap();
}

// get_page returns PageBuf (owned, not reference).
#[test]
fn get_page_returns_owned_buf() {
    let (pool, mut tf) = pool_with_file(16, 2);
    write_page_to_file(tf.as_file_mut(), 0, 0xDE);
    let buf: PageBuf = pool.get_page(DataFile::Nodes, 0).unwrap();
    assert_eq!(buf[0], 0xDE);
}

// Holding the returned PageBuf does not block subsequent put_page calls.
#[test]
fn held_page_buf_does_not_block_writes() {
    let (pool, mut tf) = pool_with_file(16, 2);
    write_page_to_file(tf.as_file_mut(), 0, 0x01);

    let buf = pool.get_page(DataFile::Nodes, 0).unwrap(); // owned — no lock held
    // This must not deadlock (would if buf held a read lock):
    let mut write_data = new_page_buf();
    write_data[0] = 0x02;
    pool.put_page(DataFile::Nodes, 0, &write_data).unwrap();
    drop(buf); // buf is just a Box<[u8]> — dropping is trivial
}
```

These fail to compile because `get_page()` currently returns `Result<PageRef<'_>>`.

### GREEN — Minimal Correct Implementation (20 min)

**`buffer_pool.rs`**:

1. Delete the `PageRef` struct and its `Deref` impl entirely (lines 57-70).
2. Change `get_page()` signature:

```rust
pub fn get_page(&self, file: DataFile, page_id: PageId) -> Result<PageBuf>
```

3. Replace the two-phase lock (write then read) with a single write-lock that
   copies the page before releasing:

```rust
pub fn get_page(&self, file: DataFile, page_id: PageId) -> Result<PageBuf> {
    let key = (file, page_id);
    let mut inner = self.inner.write().expect("buffer_pool lock poisoned");

    if !inner.frames.contains_key(&key) {
        Self::ensure_capacity_inner(&mut inner, self.max_pages)?;
        let disk_file = inner.files.get_mut(&file).ok_or_else(|| Error::CorruptPage {
            file: file.file_name(),
            page_id: 0,
            reason: "data file not registered",
        })?;
        let data = Self::read_from_disk(disk_file, page_id)?;
        inner.frames.insert(key, BufferFrame { data, dirty: false, pin_count: 0 });
        inner.lru_order.push_back(key);
    }

    Self::touch_lru_inner(&mut inner, key);

    // Copy data under the write lock — then release.
    let mut copy = new_page_buf();
    copy.copy_from_slice(&inner.frames[&key].data);
    Ok(copy)
}
```

**`file.rs`**: Update `read_page()` and `read_page_or_new()` which currently
wrap `get_page()`:

`read_page()` (lines 388-402): `get_page()` now returns `PageBuf` directly, so
the copy-dance at lines 398-401 becomes:
```rust
fn read_page(&self, file: DataFile, page_id: PageId) -> Result<PageBuf> {
    if page_id >= self.file_page_count(file) { ... }
    self.pool.get_page(file, page_id)  // already returns PageBuf
}
```

`read_page_or_new()` (lines 331-340): becomes:
```rust
fn read_page_or_new(pool: &BufferPool, file: DataFile, page_id: u32) -> PageBuf {
    pool.get_page(file, page_id).unwrap_or_else(|_| new_page_buf())
}
```

`recalculate_strings_write_offset()` (lines 252-254): the `drop(page)` at line 255
was needed to release the `PageRef` guard before returning. Remove it:
```rust
let page = pool.get_page(DataFile::Strings, last_page_id)?;  // now PageBuf
let header = PageHeader::read_from(&page);
// No drop(page) needed — it's an owned value, dropped at end of scope
Ok((base_offset + used_on_last) as u32)
```

**Update `buffer_pool.rs` tests**: Replace `PageRef` return type annotations in
tests. `pool_with_file` helper accesses `pool.inner` directly for some eviction
tests — those use `RwLock` read guards on `inner`, not `PageRef`, so they are
unaffected.

### REFACTOR (10 min)

- Remove `use std::sync::RwLockReadGuard;` from buffer_pool.rs if it is now unused.
- Confirm the `#[allow(dead_code)]` at line 1 of buffer_pool.rs can be narrowed or
  removed after the `PageRef` type is gone.
- Run `cargo clippy` and `cargo test` in `tessera-graph`.

**Throughput regression check** (BufferPool is hot path for reads):

Add to buffer_pool.rs tests:

```rust
#[test]
fn get_page_throughput_regression_guard() {
    use std::time::Instant;
    let (pool, mut tf) = pool_with_file(16, 4);
    write_page_to_file(tf.as_file_mut(), 0, 0xAB);
    // Warm up
    let _ = pool.get_page(DataFile::Nodes, 0).unwrap();

    let n = 10_000_u64;
    let start = Instant::now();
    for _ in 0..n {
        let _ = pool.get_page(DataFile::Nodes, 0).unwrap();
    }
    let elapsed = start.elapsed();
    #[allow(clippy::cast_precision_loss)]
    let ops_per_sec = n as f64 / elapsed.as_secs_f64();
    let floor = if cfg!(debug_assertions) { 50_000.0 } else { 500_000.0 };
    assert!(
        ops_per_sec > floor,
        "get_page throughput regression: {ops_per_sec:.0} ops/s < {floor:.0} ops/s"
    );
}
```

**Estimated time**: 45 min total.

---

## Cycle G-C1 — Clock-Hand Eviction in `AdjCache::get()` (CRITICAL)

**Files**:
- `tessera-graph/src/adj_cache.rs`

**Problem**: `AdjCache::get()` at line 44-52 acquires a write lock to call
`touch_inner()` which does `inner.lru.retain(|&id| id != node_id)` — O(N)
linear scan under the write lock. This serialises ALL readers: a read in one
thread blocks all reads and writes in all other threads, not just writes.

The fix is the **clock-hand algorithm**:
- Each entry carries an atomic `recently_used: AtomicBool`.
- `get()` acquires a read lock, checks the map, and if found sets `recently_used`
  via `store(true, Relaxed)`. No write lock needed for reads.
- A separate clock pointer (atomically maintained) scans entries during `insert()`
  to find an eviction candidate: entries with `recently_used == true` get their
  flag cleared and are spared; the first entry with `recently_used == false` is
  evicted. This requires a write lock only during eviction.
- The `VecDeque` LRU is replaced by a `Vec` (clock hand operates on a stable
  index-based structure).

### RED — Write Failing Tests (20 min)

Add to `adj_cache.rs` tests:

```rust
// After the clock-hand fix, get() must use a read lock (not write lock).
// We verify this by showing two concurrent gets do not block each other.
#[test]
fn concurrent_reads_do_not_block() {
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    let cache = Arc::new(AdjCache::new(64));
    cache.insert(1, ptr(Some(0), None));
    cache.insert(2, ptr(Some(1), None));

    let c1 = Arc::clone(&cache);
    let c2 = Arc::clone(&cache);

    let t1 = thread::spawn(move || {
        for _ in 0..1_000 { c1.get(1); }
    });
    let t2 = thread::spawn(move || {
        for _ in 0..1_000 { c2.get(2); }
    });

    // Both must complete well within 1 second even under contention.
    t1.join().unwrap();
    t2.join().unwrap();
}

// Clock hand must evict an entry that was NOT recently used.
#[test]
fn clock_hand_evicts_unreferenced_entry() {
    let cache = AdjCache::new(8); // capacity 8 (MIN_CAPACITY)
    for i in 0..8 {
        cache.insert(i, ptr(Some(i as u32), None));
    }

    // Access nodes 1-7 so they are marked recently_used; node 0 is not.
    for i in 1..8 { cache.get(i); }

    // Insert 8th entry — clock hand should evict node 0 (not recently used).
    cache.insert(100, ptr(Some(100), None));

    assert!(cache.get(0).is_none(), "node 0 should have been evicted");
    assert!(cache.get(100).is_some());
}

// get() throughput must not regress vs a naive write-lock implementation.
#[test]
fn get_throughput_regression_guard() {
    use std::time::Instant;
    let cache = AdjCache::new(1024);
    for i in 0..512_u64 {
        cache.insert(i, ptr(Some(i as u32), None));
    }

    let n = 100_000_u64;
    let start = Instant::now();
    for i in 0..n {
        let _ = cache.get(i % 512);
    }
    let elapsed = start.elapsed();
    #[allow(clippy::cast_precision_loss)]
    let ops_per_sec = n as f64 / elapsed.as_secs_f64();
    let floor = if cfg!(debug_assertions) { 500_000.0 } else { 5_000_000.0 };
    assert!(
        ops_per_sec > floor,
        "adj_cache get throughput regression: {ops_per_sec:.0} ops/s < {floor:.0} ops/s"
    );
}
```

The concurrent test is not flaky by design — it just measures that reads
complete without deadlocking or starving. The throughput guard catches regressions.

### GREEN — Minimal Correct Implementation (35 min)

Replace the interior of `adj_cache.rs` with the clock-hand implementation.

**New data structures**:

```rust
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

struct CacheEntry {
    node_id: u64,
    ptr: AdjacencyPointer,
    recently_used: AtomicBool,
}

struct CacheInner {
    map: HashMap<u64, usize>,       // node_id -> slot index in `slots`
    slots: Vec<Option<Arc<CacheEntry>>>,
    clock_hand: usize,
    count: usize,
}

pub struct AdjCache {
    inner: RwLock<CacheInner>,
    capacity: usize,
}
```

**`get()` implementation** — read lock, atomic flag:

```rust
pub fn get(&self, node_id: u64) -> Option<AdjacencyPointer> {
    let inner = self.inner.read().expect(LOCK_POISON_MSG);
    if let Some(&slot) = inner.map.get(&node_id) {
        if let Some(entry) = &inner.slots[slot] {
            entry.recently_used.store(true, Ordering::Relaxed);
            return Some(entry.ptr);
        }
    }
    None
}
```

**`insert()` implementation** — write lock, clock sweep on eviction:

```rust
pub fn insert(&self, node_id: u64, ptr: AdjacencyPointer) {
    let mut inner = self.inner.write().expect(LOCK_POISON_MSG);

    // Update existing entry.
    if let Some(&slot) = inner.map.get(&node_id) {
        if let Some(entry) = &inner.slots[slot] {
            // Replace Arc with new value (entries are Arc so readers keep old value safely).
            inner.slots[slot] = Some(Arc::new(CacheEntry {
                node_id, ptr, recently_used: AtomicBool::new(true)
            }));
        }
        return;
    }

    // Find a free slot or evict via clock hand.
    let slot = if inner.count < inner.slots.len() {
        // Find first None slot.
        inner.slots.iter().position(|s| s.is_none()).unwrap_or(inner.slots.len())
    } else {
        // Clock-hand eviction.
        loop {
            let hand = inner.clock_hand % inner.slots.len();
            inner.clock_hand = hand + 1;
            if let Some(Some(entry)) = inner.slots.get(hand) {
                if entry.recently_used.swap(false, Ordering::Relaxed) {
                    continue; // Give it a second chance.
                }
                // Evict this slot.
                let evicted_id = entry.node_id;
                inner.map.remove(&evicted_id);
                inner.count -= 1;
                break hand;
            }
        }
    };

    // Grow slots vec if needed (only on first fill, not eviction).
    if slot == inner.slots.len() {
        inner.slots.push(None);
    }
    inner.slots[slot] = Some(Arc::new(CacheEntry {
        node_id, ptr, recently_used: AtomicBool::new(false)
    }));
    inner.map.insert(node_id, slot);
    inner.count += 1;
}
```

**`remove()` and `clear()`**: acquire write lock, remove from map and set slot to `None`.

**Note**: The `Arc<CacheEntry>` wrapping each slot ensures that a `get()` holding a
read lock can safely read the entry's data even if a concurrent `insert()` evicts
it (the eviction sets the slot to `None` but does not drop the `Arc` while the read
guard's copy of the pointer exists). This is the key safety property.

However, given `forbid(unsafe_code)`, the `Arc` wrapping is the cleanest approach.
The read guard + Arc::clone means reads pay one `Arc` clone. This is acceptable.

Simplification for initial implementation: since `AdjacencyPointer` is `Copy`
(it's two `Option<u32>` fields — confirmed at adjacency_codec.rs line 30),
no `Arc` is needed. The copy is made under the read lock. This reduces the
implementation to:

```rust
pub fn get(&self, node_id: u64) -> Option<AdjacencyPointer> {
    let inner = self.inner.read().expect(LOCK_POISON_MSG);
    if let Some(&slot) = inner.map.get(&node_id) {
        if let Some((ptr, recently_used)) = &inner.slots[slot] {
            recently_used.store(true, Ordering::Relaxed);
            return Some(*ptr);
        }
    }
    None
}
```

Where `slots: Vec<Option<(AdjacencyPointer, AtomicBool)>>`. This is simpler
and avoids Arc overhead.

**Update existing tests**: `eviction_when_at_capacity` and `evicted_entry_on_get_returns_none`
test LRU order specifically. Clock-hand eviction has slightly different ordering.
Update these tests to verify general eviction properties (something is evicted when
at capacity) rather than specific LRU order, which is an implementation detail.

### REFACTOR (10 min)

- Remove the `VecDeque` import and `lru` field.
- Remove `touch_inner()` helper (no longer needed).
- Update `evict_one_inner()` or remove it (replaced by inline clock sweep).
- Run `cargo clippy --all-features` and `cargo test`.

**Estimated time**: 65 min total.

---

## Cycle G-R1 — Centralize Lock-Poison Message Constants (RECOMMENDED)

**Files**:
- `tessera-graph/src/adj_cache.rs`
- `tessera-graph/src/storage/buffer_pool.rs`

**Problem**: Scattered `expect("adj_cache lock poisoned")` and
`expect("buffer_pool lock poisoned")` strings across both files. After Cycle G-C1,
`adj_cache.rs` uses the string in `get()`, `insert()`, `remove()`, `clear()`,
`len()`, `is_empty()`. `buffer_pool.rs` uses it in 7 places.

**Dependency**: Do this AFTER Cycle G-C1 so the new clock-hand code uses the
constant from the start.

### RED — Write Failing Test (5 min)

This is a purely structural refactor with no behavioral change. The RED test is:

```rust
// Verify the constant is used (not a literal) — checked by grep in CI.
// Behavioral test: lock panic message contains the module name.
// (No runtime test needed for a constant string — compilation is the test.)
```

Alternatively, add a test that accesses the constant to prevent it from being
dead-code-eliminated:

```rust
#[test]
fn lock_poison_constants_are_defined() {
    // Ensures constants are not accidentally removed.
    assert!(!LOCK_POISON_MSG.is_empty());
}
```

### GREEN — Minimal Correct Implementation (10 min)

**`adj_cache.rs`**: Add at the top of the file (before `use` statements):

```rust
const LOCK_POISON_MSG: &str = "adj_cache lock poisoned";
```

Replace all `expect("adj_cache lock poisoned")` with `expect(LOCK_POISON_MSG)`.

**`buffer_pool.rs`**: Add:

```rust
const LOCK_POISON_MSG: &str = "buffer_pool lock poisoned";
```

Replace all `expect("buffer_pool lock poisoned")` with `expect(LOCK_POISON_MSG)`.

### REFACTOR (5 min)

Run `cargo clippy`. No other changes.

**Estimated time**: 20 min total.

---

## Cycle G-R4 — Gate Throughput Test with `#[ignore]` (RECOMMENDED)

**Files**:
- `tessera-graph/src/graph.rs`

**Problem**: `shared_graph_add_node_throughput_floor` at line 1658 is a timing-
sensitive test that can flake in CI under load, virtualization, or low-resource
environments. It should be gated with `#[ignore]` so it does not block CI but
remains runnable manually (`cargo test -- --ignored`).

### RED — Write Failing Test (5 min)

The "test" here is observing CI flakiness. In the plan, RED means:

Create a new test that is explicitly `#[ignore]`-gated and verifies the `#[ignore]`
attribute is respected:

```rust
// This test documents the intent: throughput tests are opt-in.
#[test]
fn throughput_test_is_gated_by_ignore_attribute() {
    // Verify via inspection: `shared_graph_add_node_throughput_floor`
    // must have #[ignore] above it. If CI is failing due to timing, the
    // #[ignore] attribute was removed — restore it.
}
```

### GREEN — Minimal Correct Implementation (5 min)

Add `#[ignore]` above `#[test]` on `shared_graph_add_node_throughput_floor`
at line 1657:

```rust
#[ignore = "timing-sensitive: run with `cargo test -- --ignored` locally"]
#[test]
fn shared_graph_add_node_throughput_floor() {
```

### REFACTOR (2 min)

Verify `cargo test` no longer runs this test by default. Verify
`cargo test -- --ignored` still runs it.

**Estimated time**: 12 min total.

---

## Cycle G-R5 — Improve WAL Recovery Comment (RECOMMENDED)

**Files**:
- `tessera-graph/src/storage/file.rs`

**Problem**: The comment at lines 207-212 about transaction boundary markers is
too brief. Developers unfamiliar with the recovery flow will not understand
why `Begin`/`Commit`/`Rollback` records are no-ops during recovery, or what the
atomicity limitation is.

### GREEN — Inline Fix (10 min)

Replace the comment block at lines 207-213 in `file.rs`:

Current:
```rust
WalRecord::Begin { .. }
| WalRecord::Commit { .. }
| WalRecord::Rollback { .. } => {
    // Transaction boundary markers — no data to replay.
    // Enterprise transaction manager handles these.
}
```

Replace with:
```rust
WalRecord::Begin { .. }
| WalRecord::Commit { .. }
| WalRecord::Rollback { .. } => {
    // Transaction boundary markers carry no page data to replay.
    //
    // ATOMICITY LIMITATION: This recovery path replays ALL WriteNode /
    // WriteEdge records regardless of whether their enclosing transaction
    // was committed or rolled back. True transactional recovery would require
    // filtering replay to only committed transactions by first scanning the
    // WAL for Commit/Rollback records before replaying data records.
    //
    // That optimization is deferred to a future milestone. Until then,
    // the caller (enterprise TransactionManager) is responsible for
    // discarding in-flight writes that crossed a crash boundary.
}
```

This cycle has no RED phase — it is a doc-only change. Run `cargo clippy`
to verify no lint on the new comment.

**Estimated time**: 10 min total.

---

## Cycle G-R6 — Remove `#[allow(dead_code)]` on Public Methods (RECOMMENDED)

**Files**:
- `tessera-graph/src/adj_cache.rs`

**Problem**: `clear()` at line 80, `len()` at line 89, `is_empty()` at line 96,
and `capacity()` at line 103 all carry `#[allow(dead_code)]` attributes. These
are public methods — `dead_code` lint does not apply to public items. The
attributes are cargo-culted and hide real dead-code warnings if they were to appear.

**Dependency**: Do this AFTER Cycle G-C1 since the clock-hand rewrite may change
which methods exist.

### GREEN — Inline Fix (5 min)

Remove all four `#[allow(dead_code)]` attributes from the public methods.

If after removal any method genuinely has no callers and triggers a lint error,
either:
a) Add a `#[cfg(test)]` usage, or
b) Accept that the method exists as part of the public API (it is `pub` — the
   compiler does not warn on pub items).

Run `cargo clippy` and `cargo test`. No new failures expected.

**Estimated time**: 5 min total.

---

## Cycle WIRING — End-to-End Wiring Verification

**Files**: All modified files in both crates.

This final cycle verifies that all changes compose correctly across the dependency
chain. It is not a new feature cycle — it is a compilation and integration check.

### Checklist (30 min)

```
[ ] cargo build --all in tessera-graph — zero errors, zero warnings.
[ ] cargo clippy --all-targets -- -D warnings in tessera-graph — clean.
[ ] cargo test --all in tessera-graph — all tests pass including
    buffer_pool, adj_cache, file, graph, wal modules.
[ ] cargo build --all in tessera-graph-enterprise — zero errors, zero warnings.
[ ] cargo clippy --all-targets -- -D warnings in tessera-graph-enterprise — clean.
[ ] cargo test --all in tessera-graph-enterprise — all tests pass including
    manager, handle, snapshot unit tests AND txn_integration.rs.
[ ] Run: cargo test -- --test-threads=4 in enterprise to trigger real concurrency.
[ ] Run: cargo test -- --ignored in tessera-graph to verify throughput floor
    still passes locally (not gated, but verifiable manually).
[ ] Verify TransactionManager is Send + Sync — the existing manager_is_send_sync
    test must still pass.
[ ] Verify BufferPool is Send + Sync — the existing buffer_pool_is_send_sync test
    must still pass.
[ ] Verify AdjCache is Send + Sync — the existing adj_cache_is_send_sync test
    must still pass.
```

If any step fails, return to the appropriate cycle and fix before proceeding.

---

## Estimation Summary

| Cycle   | Tag        | Impl  | Tests | Total |
|---------|------------|-------|-------|-------|
| E-C1    | WAL field  | 25    | 15    | 40    |
| E-R5    | LockPoisoned | 20  | 10    | 30    |
| E-C4    | Begin WAL  | 15    | 10    | 25    |
| E-C3    | Arc snapshot | 20  | 10    | 30    |
| E-R6    | Private fields | 15 | 5    | 20    |
| E-R7    | Remove const | 5   | 5     | 10    |
| E-O9-O11 | Polish     | 10   | 10    | 20    |
| E-R8    | Concurrency test | 10 | 15  | 25    |
| G-C2    | PageBuf return | 20 | 15   | 35    |
| G-C1    | Clock-hand  | 35   | 20    | 55    |
| G-R1    | Lock constants | 10 | 5   | 15    |
| G-R4    | Gate test   | 5    | 5     | 10    |
| G-R5    | WAL comment | 10   | 0     | 10    |
| G-R6    | dead_code   | 5    | 0     | 5     |
| WIRING  | Verification | 30  | 0     | 30    |
| **TOTAL** |           | **235** | **125** | **360 min (~6h)** |

---

## Criteria de Exito

- [ ] `cargo test --all` pases without skips in both crates.
- [ ] `cargo clippy --all-targets -- -D warnings` is clean in both crates.
- [ ] Zero `expect("... lock poisoned")` calls remain — all replaced by `?` with
      `LockPoisoned` variant.
- [ ] `TransactionManager::commit()` and `rollback()` take no `wal_path` parameter.
- [ ] `begin()` writes a `WalRecord::Begin` verifiable via WAL re-read.
- [ ] `AdjCache::get()` acquires a read lock (not write lock).
- [ ] `BufferPool::get_page()` returns `PageBuf` — `PageRef` type deleted entirely.
- [ ] 16-thread concurrent commit test passes deterministically.
- [ ] Throughput guards pass: commit >= 200 ops/s (debug), get_page >= 50k ops/s
      (debug), adj_cache get >= 500k ops/s (debug).
- [ ] No throughput regression > 10% vs baseline (measured before and after G-C1
      and G-C2 changes using the inline regression tests).
