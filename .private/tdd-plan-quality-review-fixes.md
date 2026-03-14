# TDD Plan: Quality Review Fixes

**Date**: 2026-03-14
**Scope**: tessera-graph (MIT, `../tessera-graph/`) and tessera-storage-enterprise (proprietary, `crates/tessera-storage-enterprise/`)
**Branch**: `feature/quality-fixes-p1.2` (create from `develop`)
**Stack**: Rust 2024 edition, MSRV 1.85, `forbid(unsafe_code)`, `deny(clippy::all)`, `warn(clippy::pedantic, clippy::nursery)`

---

## Dependency Order

```
C3  (test access field directly — no code deps, just test cleanup)
  └─ R7  (thread count in same test — same test, do together with C3)
C4  (atomicity gap comment — no deps)
R3  (extract LSN_PLACEHOLDER — no deps, pure refactor of manager.rs)
  └─ depends on: none; all 6 lsn:0 sites are in manager.rs
R4  (remove redundant drop — no deps, same file)
R6  (document sync() decision in rollback — no deps)
R1  (TxnState::Display — no deps, handle.rs)
O2  (TransactionHandle::Drop warning — depends on R1 being done first so state is printable)
O1  (Snapshot::Clone — no deps, snapshot.rs)
C1  (AdjCache insert invariant fix — graph crate, no enterprise deps)
C2  (clock_sweep loop guard + clock_hand overflow — depends on C1 being fixed first)
R5  (AdjCache::len/is_empty promote to production — graph crate, no deps)
R2  (BufferPool::get_page write lock — document trade-off as TODO, no deps)
O3  (document 4096-byte copy as future optimization — no deps)
O4  (remove obsolete RefCell comment — no deps)
O5  (read_page_or_new log error — graph crate, no deps)
O6  (merge GraphConfig impl blocks — graph crate, no deps)
--- WIRING VERIFICATION (final cycle) ---
```

**Execution order** (grouped to minimize context switching between crates):

Enterprise crate first (manager.rs, handle.rs, snapshot.rs, error.rs):
1. Cycle E-C3 — Test uses `committed_count()` (C3 + R7 together)
2. Cycle E-C4 — Document atomicity gap in `commit()`
3. Cycle E-R3 — Extract `LSN_PLACEHOLDER` constant
4. Cycle E-R4 — Remove redundant `drop(h)` in test
5. Cycle E-R6 — Document `sync()` in `rollback()`
6. Cycle E-R1 — Add `TxnState::Display`
7. Cycle E-O2 — `TransactionHandle::Drop` warning
8. Cycle E-O1 — `Snapshot` derives `Clone`

Graph crate second (adj_cache.rs, buffer_pool.rs, file.rs):
9.  Cycle G-C1  — Fix `AdjCache::insert` free-slot invariant
10. Cycle G-C2  — Fix `clock_sweep` infinite loop risk + `clock_hand` overflow
11. Cycle G-R5  — Promote `AdjCache::len()` to production visibility
12. Cycle G-R2  — Document `BufferPool::get_page` write-lock trade-off
13. Cycle G-O3  — Document 4096-byte copy as future optimization
14. Cycle G-O4  — Remove obsolete `RefCell` comment in buffer_pool test
15. Cycle G-O5  — Log error in `read_page_or_new`
16. Cycle G-O6  — Merge `GraphConfig` impl blocks

Final:
17. Cycle WIRE  — Full compilation + test suite verification

---

## Cycle E-C3 + E-R7 — Test Uses Public API + Thread Count Consistency

**Files**: `crates/tessera-storage-enterprise/src/txn/manager.rs`
**Findings**: C3, R7
**Problem**:
- C3: `concurrent_commits_are_serialised` accesses `mgr.committed.read()` directly instead of the public `committed_count()` method. This couples the test to the private field layout.
- R7: The same test spawns 8 threads but the description says 16 and the integration tests use 16. The unit test should be consistent.

### RED — Write Failing Tests

The existing test `concurrent_commits_are_serialised` compiles today because `committed` is `pub(crate)` and the test module is in the same crate. After the GREEN step, the field access will be replaced, but first capture the desired behavior with an assertion that uses the public API:

```rust
// In manager.rs tests, REPLACE the existing test body:
#[test]
fn concurrent_commits_are_serialised() {
    use std::sync::Arc;
    use std::thread;

    let tmp = NamedTempFile::new().unwrap();
    let mgr = Arc::new(TransactionManager::open(tmp.path()).unwrap());

    let threads: Vec<_> = (0..16)   // was: 0..8
        .map(|_| {
            let mgr = Arc::clone(&mgr);
            thread::spawn(move || {
                let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
                mgr.commit(&mut h).unwrap();
            })
        })
        .collect();

    for t in threads {
        t.join().unwrap();
    }

    // Use public API — not the private field
    assert_eq!(mgr.committed_count().unwrap(), 16);  // was: direct field access, was: 8
}
```

Run `cargo test concurrent_commits_are_serialised` — it will FAIL because the current test body
uses `mgr.committed.read()` (direct field access) and asserts `8`. That is the RED state.

### GREEN — Minimal Correct Implementation

Replace the test body in `manager.rs` (lines 302-331). No production code changes needed.

```rust
#[test]
fn concurrent_commits_are_serialised() {
    use std::sync::Arc;
    use std::thread;

    let tmp = NamedTempFile::new().unwrap();
    let mgr = Arc::new(TransactionManager::open(tmp.path()).unwrap());

    let threads: Vec<_> = (0..16)
        .map(|_| {
            let mgr = Arc::clone(&mgr);
            thread::spawn(move || {
                let mut h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
                mgr.commit(&mut h).unwrap();
            })
        })
        .collect();

    for t in threads {
        t.join().unwrap();
    }

    assert_eq!(mgr.committed_count().unwrap(), 16);
}
```

The `use crate::error::EnterpriseError` import inside the old test body is no longer needed
(it was only used for the `map_err` in the direct field access). Remove it from the test body.

### REFACTOR

Run `cargo clippy -p tessera-storage-enterprise` — verify no new warnings.
The `EnterpriseError` import inside the test block (lines 326-327 in the original) can be deleted
because `committed_count()` already handles the lock internally.

**Estimated time**: 15 min

---

## Cycle E-C4 — Document Atomicity Gap in `commit()`

**Files**: `crates/tessera-storage-enterprise/src/txn/manager.rs`
**Finding**: C4
**Problem**: Between `wal.sync()` (line 101) and `*guard = Arc::new(new_set)` (line 110) there is a crash window: the Commit record is on disk but the in-memory commit log is not updated. If the process crashes here, recovery will replay the Commit WAL record but the live `committed` set will have been rebuilt from scratch. Currently there is no comment documenting this known gap or the future recovery path.

### RED — Write Failing Tests

This is a documentation-only change. The "test" is a `cargo doc --no-deps` run that currently passes but does not verify the comment exists. We assert the invariant with a compile-time-visible doc comment. After writing the new comment the review is satisfied.

There is no test that can programmatically fail for a missing comment; the RED state is the current absence of the comment — confirm by reading the code (already done above).

### GREEN — Minimal Correct Implementation

Add a block comment immediately before `drop(wal)` inside `commit()`:

```rust
wal.sync().map_err(EnterpriseError::Graph)?;
// ATOMICITY GAP: The Commit WAL record is now durable on disk, but the
// in-memory `committed` set has not yet been updated.  A crash here will
// leave the WAL with a Commit entry whose txn_id is absent from the
// committed set.
//
// Recovery path (not yet implemented): on open, scan WAL records; for any
// `Commit { txn_id }` record whose txn_id is absent from the meta-log,
// replay the commit into the in-memory set before accepting new transactions.
// Until that recovery path exists, a crash in this window produces a
// transaction that is durable in the WAL but invisible to new snapshots.
drop(wal);
```

### REFACTOR

Run `cargo clippy -p tessera-storage-enterprise`. No functional changes — no new warnings expected.

**Estimated time**: 10 min

---

## Cycle E-R3 — Extract `LSN_PLACEHOLDER` Constant

**Files**: `crates/tessera-storage-enterprise/src/txn/manager.rs`
**Finding**: R3
**Problem**: The literal `lsn: 0` appears 6 times (in `begin`, `commit`, `rollback`), each accompanied by a repetitive comment `// lsn: 0 is a placeholder — WalWriter assigns the real LSN in append()`. Extracting a named constant removes the duplication and makes the intent self-documenting.

### RED — Write Failing Tests

This is a pure refactor with no behavioral change. The RED state is confirmed by counting occurrences:

```
// current state: grep "lsn: 0" manager.rs → 6 hits
```

After the change there should be 0 hits for `lsn: 0` (literal) and 6 hits for `LSN_PLACEHOLDER`.
We validate with `cargo test -p tessera-storage-enterprise` (all existing tests must still pass).

### GREEN — Minimal Correct Implementation

Add the constant near the top of `manager.rs`, after the `use` block:

```rust
/// Placeholder LSN value passed to `WalRecord` constructors.
///
/// `WalWriter::append` assigns the real Log Sequence Number when the record
/// is serialised. The value `0` is intentional and harmless — it is never
/// stored persistently as the real LSN.
const LSN_PLACEHOLDER: u64 = 0;
```

Replace all 6 occurrences of `lsn: 0` in the three methods:

In `begin()`:
```rust
wal.append(WalRecord::Begin { lsn: LSN_PLACEHOLDER, txn_id })
```

In `commit()`:
```rust
wal.append(WalRecord::Commit {
    lsn: LSN_PLACEHOLDER,
    txn_id: handle.txn_id(),
})
```

In `rollback()`:
```rust
wal.append(WalRecord::Rollback {
    lsn: LSN_PLACEHOLDER,
    txn_id: handle.txn_id(),
})
```

Remove the three inline `// lsn: 0 is a placeholder` comments (now superseded by the constant's doc comment).

### REFACTOR

Run `cargo clippy -p tessera-storage-enterprise`. Verify zero `lsn: 0` literals remain.

**Estimated time**: 15 min

---

## Cycle E-R4 — Remove Redundant `drop(h)` in Test

**Files**: `crates/tessera-storage-enterprise/src/txn/manager.rs`
**Finding**: R4
**Problem**: In `begin_writes_wal_begin_record` (lines 396-397), `drop(h)` appears before `drop(mgr)`. `TransactionHandle` does not hold or affect `WalWriter`; the WAL is owned by `TransactionManager`. Dropping `h` before `mgr` is neither necessary nor harmful, but it implies a false dependency that misleads readers.

### RED — Write Failing Tests

Behavioral: this is a pure cleanup. The RED state is the misleading `drop(h)` being present.
Verify no test depends on drop order by running `cargo test begin_writes_wal_begin_record` before and after the change — both must pass.

### GREEN — Minimal Correct Implementation

In the test `begin_writes_wal_begin_record`, remove the line `drop(h);` so the drop order becomes:

```rust
let h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
let txn_id = h.txn_id();

// Drop manager to flush WAL writer (handle drops implicitly at end of scope)
drop(mgr);

let reader = WalReader::open(tmp.path()).unwrap();
```

`h` will be dropped implicitly at end of scope after `mgr` (Rust drops in reverse declaration order within a scope, so `mgr` declared after `h` drops first — wait, `mgr` is declared before `h` so it drops *after* `h` by LIFO). Actually the LIFO rule means: `mgr` is declared first, `h` second — so `h` drops first, `mgr` drops second. The explicit `drop(mgr)` before reading was the key. We keep `drop(mgr)` and simply remove the redundant `drop(h)`:

```rust
#[test]
fn begin_writes_wal_begin_record() {
    use tessera_graph::WalReader;

    let tmp = NamedTempFile::new().unwrap();
    let mgr = TransactionManager::open(tmp.path()).unwrap();
    let h = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
    let txn_id = h.txn_id();

    // Drop manager to flush WAL writer; handle drops implicitly.
    drop(mgr);

    let reader = WalReader::open(tmp.path()).unwrap();
    let records: Vec<_> = reader.records().collect();

    let found = records.iter().any(|r| {
        matches!(r, WalRecord::Begin { txn_id: id, .. } if *id == txn_id)
    });
    assert!(found, "WAL must contain a Begin record for txn {txn_id}");
}
```

### REFACTOR

Run `cargo clippy -p tessera-storage-enterprise`. No new warnings expected.

**Estimated time**: 10 min

---

## Cycle E-R6 — Document `sync()` in `rollback()`

**Files**: `crates/tessera-storage-enterprise/src/txn/manager.rs`
**Finding**: R6
**Problem**: `rollback()` calls `wal.sync()` (line 140). This incurs an `fsync` cost on every rollback. The reason for syncing rollbacks is not documented: for crash recovery to be correct, a Rollback record must also be durable — otherwise on recovery we cannot distinguish "transaction was in-flight at crash" from "transaction rolled back before crash". However, with the current lack of a recovery implementation, this sync provides durability cost without delivering the full recovery benefit yet.

### RED — Write Failing Tests

Documentation-only change. RED state: the `sync()` call exists without explanation.
Verify `cargo test -p tessera-storage-enterprise -- rollback` passes before and after.

### GREEN — Minimal Correct Implementation

Add a comment above `wal.sync()` in `rollback()`:

```rust
// Sync the Rollback record to disk.  This has an fsync cost on every
// rollback but is necessary for correct crash recovery: without this sync,
// a crash after writing the Rollback record but before the OS flushes the
// buffer could leave the WAL in a state where the rollback is invisible,
// causing the recovery path to erroneously re-apply aborted writes.
//
// Current limitation: the recovery implementation (not yet present) does not
// yet replay Rollback records.  Until it does, this sync is a correctness
// invariant that pays its cost in advance.  If performance profiling shows
// rollback sync as a bottleneck, consider batching with `sync_data()` or
// deferring to a background checkpoint.
wal.sync().map_err(EnterpriseError::Graph)?;
```

### REFACTOR

Run `cargo clippy -p tessera-storage-enterprise`. No changes to production logic.

**Estimated time**: 10 min

---

## Cycle E-R1 — Add `TxnState::Display`

**Files**: `crates/tessera-storage-enterprise/src/txn/handle.rs`
**Finding**: R1
**Problem**: `TxnState` derives `Debug` but has no `Display` impl. When `TransactionNotActive` is returned as an error, the error message shows only the txn_id (`"transaction 42 is not active"`) with no indication of what state the transaction is actually in. A `Display` impl enables richer diagnostics.

The secondary part of R1 (enrich `TransactionNotActive` to include current state) requires changing `error.rs` and `manager.rs` as well. Evaluate after seeing the test:

**Decision**: Enrich the error variant to `TransactionNotActive(u64, TxnState)` to include the actual state. This requires: (a) `Display` on `TxnState`, (b) updating `EnterpriseError::TransactionNotActive`, (c) updating both call sites in `manager.rs`.

### RED — Write Failing Tests

Add to `handle.rs` test module:

```rust
#[test]
fn txn_state_display() {
    use std::fmt::Write as _;
    let mut s = String::new();
    write!(s, "{}", TxnState::Active).unwrap();
    assert_eq!(s, "Active");
    s.clear();
    write!(s, "{}", TxnState::Committed).unwrap();
    assert_eq!(s, "Committed");
    s.clear();
    write!(s, "{}", TxnState::RolledBack).unwrap();
    assert_eq!(s, "RolledBack");
}
```

Add to `error.rs` test module:

```rust
#[test]
fn transaction_not_active_includes_state() {
    use crate::txn::handle::TxnState;
    use std::fmt::Write as _;
    let e = EnterpriseError::TransactionNotActive(99, TxnState::Committed);
    let mut s = String::new();
    write!(s, "{e}").unwrap();
    assert!(s.contains("99"), "must contain txn_id");
    assert!(s.contains("Committed"), "must contain state");
}
```

Run `cargo test -p tessera-storage-enterprise txn_state_display` — FAILS (no `Display` for `TxnState`).
Run `cargo test -p tessera-storage-enterprise transaction_not_active_includes_state` — FAILS (variant has wrong arity).

### GREEN — Minimal Correct Implementation

**Step 1**: Add `Display` for `TxnState` in `handle.rs`, after the `enum` definition:

```rust
impl std::fmt::Display for TxnState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => f.write_str("Active"),
            Self::Committed => f.write_str("Committed"),
            Self::RolledBack => f.write_str("RolledBack"),
        }
    }
}
```

**Step 2**: Update `EnterpriseError` in `error.rs`:

```rust
/// Attempted to commit/rollback a transaction that is not active.
#[error("transaction {0} is not active (state: {1})")]
TransactionNotActive(u64, crate::txn::handle::TxnState),
```

**Step 3**: Update both call sites in `manager.rs`:

In `commit()`:
```rust
return Err(EnterpriseError::TransactionNotActive(handle.txn_id(), handle.state()));
```

In `rollback()`:
```rust
return Err(EnterpriseError::TransactionNotActive(handle.txn_id(), handle.state()));
```

**Step 4**: Update existing tests in `manager.rs` that pattern-match `TransactionNotActive(id)` to `TransactionNotActive(id, _)`. Search for all `TransactionNotActive` uses in tests:
- `double_commit_returns_error` — uses `is_err()`, no change needed
- `commit_after_rollback_returns_error` — uses `is_err()`, no change needed
- `poisoned_wal_lock_returns_error` — matches `Err(EnterpriseError::LockPoisoned(_))`, no change needed

If any test does `matches!(result, Err(EnterpriseError::TransactionNotActive(_)))`, update to `TransactionNotActive(_, _)`.

### REFACTOR

Run `cargo clippy -p tessera-storage-enterprise`. Check that the new `Display` impl compiles without `clippy::pedantic` warnings. The `TxnState` type is now `pub` (it already was) and implements both `Debug` and `Display`.

**Estimated time**: 20 min

---

## Cycle E-O2 — `TransactionHandle::Drop` Warning for Abandoned Transactions

**Files**: `crates/tessera-storage-enterprise/src/txn/handle.rs`
**Finding**: O2
**Problem**: If a `TransactionHandle` is dropped while still `Active` (i.e., the caller forgot to commit or rollback), the transaction silently disappears. A `Drop` impl that emits a `tracing::warn!` makes this programming error visible in logs.

**Dependency**: Requires `TxnState::Display` (Cycle E-R1) to format the state in the warning message.

### RED — Write Failing Tests

There is no direct runtime test for `tracing::warn!` output without a tracing subscriber.
The RED state is confirmed by the absence of a `Drop` impl. We write a test that verifies
the handle can be dropped while active without panicking (the warning is a side effect, not an assertion):

```rust
// In handle.rs tests:
#[test]
fn dropping_active_handle_does_not_panic() {
    let h = TransactionHandle::new(
        99,
        IsolationLevel::ReadCommitted,
        TxnState::Active,
        None,
    );
    drop(h); // must not panic; tracing warning is emitted if subscriber configured
}
```

This test passes even today (no `Drop` impl, no panic). The RED state is that the warn is missing — we accept this as a best-effort observability change rather than a testable behavior change. The test documents the expected non-panic contract.

### GREEN — Minimal Correct Implementation

**Step 1**: Add `tracing` as a dependency in `tessera-storage-enterprise/Cargo.toml`:

```toml
[dependencies]
# ... existing deps
tracing = "0.1"
```

**Step 2**: Add `Drop` impl in `handle.rs`:

```rust
impl Drop for TransactionHandle {
    fn drop(&mut self) {
        if self.state == TxnState::Active {
            tracing::warn!(
                txn_id = self.txn_id,
                isolation = %self.isolation,
                "TransactionHandle dropped while Active — commit or rollback was never called"
            );
        }
    }
}
```

### REFACTOR

Run `cargo clippy -p tessera-storage-enterprise`.

The `Drop` impl introduces a potential issue: the existing test `begin_writes_wal_begin_record`
in `manager.rs` holds an `h` that is never committed or rolled back. After the `drop(mgr)` call,
`h` is still `Active` — so the warning fires. This is expected and harmless; add a comment to that test acknowledging it:

```rust
// Note: `h` is intentionally left without commit/rollback to test WAL record isolation.
// A tracing warning about the abandoned handle is expected and can be ignored in tests.
```

**Estimated time**: 20 min

---

## Cycle E-O1 — `Snapshot` Derives `Clone`

**Files**: `crates/tessera-storage-enterprise/src/txn/snapshot.rs`
**Finding**: O1
**Problem**: `Snapshot` wraps `Arc<HashSet<u64>>` which is `Clone` in O(1) (increments the reference count). Deriving `Clone` for `Snapshot` enables callers to cheaply duplicate a snapshot view without re-acquiring the commit log lock.

### RED — Write Failing Tests

```rust
// In snapshot.rs tests:
#[test]
fn snapshot_clone_is_cheap_and_equal() {
    let committed = Arc::new(HashSet::from([1u64, 2, 3]));
    let snap = Snapshot::new(committed, 10);
    let snap2 = snap.clone();
    // Both snapshots see the same committed set
    assert!(snap2.is_visible(1));
    assert!(snap2.is_visible(10)); // owner_txn_id preserved
    assert_eq!(snap.committed_count(), snap2.committed_count());
}
```

Run `cargo test -p tessera-storage-enterprise snapshot_clone_is_cheap_and_equal` — FAILS (`Snapshot` does not implement `Clone`).

### GREEN — Minimal Correct Implementation

Add `Clone` to the derive macro on `Snapshot` in `snapshot.rs`:

```rust
#[derive(Debug, Clone)]
pub struct Snapshot {
    committed_at_begin: Arc<HashSet<u64>>,
    owner_txn_id: u64,
}
```

`Arc<HashSet<u64>>` implements `Clone` (atomic reference count increment, O(1)).
`u64` implements `Clone`.

### REFACTOR

Run `cargo clippy -p tessera-storage-enterprise`. `#[derive(Clone)]` on a struct with `Arc<T>` is idiomatic and generates no clippy warnings.

**Estimated time**: 10 min

---

## Cycle G-C1 — Fix `AdjCache::insert` Free-Slot Invariant

**Files**: `../tessera-graph/src/adj_cache.rs`
**Finding**: C1
**Problem**: In `insert()` (lines 93-106), the branch `inner.count < self.capacity` tries to find a `None` slot first, then falls back to `push(None)` as a new slot. But when `count < capacity` yet all existing slots are `Some(_)` (because removed entries leave `None` holes but the current code tracks `count` as live entries, not slot count), the `push(None)` appends a new slot and returns it — but this can grow `slots.len()` beyond `capacity` if called when `slots` is already at capacity length but has no `None` holes.

Correct logic:
1. If `slots.len() < capacity` → grow `slots` by pushing a new `None` and use that index.
2. Else if any `slots[i]` is `None` → use that index (there must be one since `count < capacity` implies fewer live entries than capacity, meaning at least one hole).
3. Else → `count < capacity` but no `None` slot exists — this is an invariant violation, `panic!`.

### RED — Write Failing Tests

```rust
// In adj_cache.rs tests:
#[test]
fn insert_after_remove_does_not_grow_slots_beyond_capacity() {
    let cap = 8_usize;
    let cache = AdjCache::new(cap);

    // Fill to capacity
    for i in 0..cap as u64 {
        cache.insert(i, ptr(Some(i as u32), None));
    }
    assert_eq!(cache.len(), cap);

    // Remove two entries — creates two None holes, count drops to 6
    cache.remove(0);
    cache.remove(1);
    assert_eq!(cache.len(), cap - 2);

    // Insert two new entries — must reuse holes, NOT grow slots
    cache.insert(100, ptr(Some(100), None));
    cache.insert(101, ptr(Some(101), None));

    // slots.len() must still be == capacity (reused holes, did not push)
    {
        let inner = cache.inner.read().expect("lock");
        assert_eq!(
            inner.slots.len(),
            cap,
            "slots grew beyond capacity after insert-into-hole"
        );
    }
    assert_eq!(cache.len(), cap);
}

#[test]
fn insert_into_fresh_cache_grows_slots_incrementally() {
    let cache = AdjCache::new(8);
    // Insert entries one by one — slots should grow from 0 to 8, never beyond
    for i in 0..8_u64 {
        cache.insert(i, ptr(Some(i as u32), None));
        let inner = cache.inner.read().expect("lock");
        assert_eq!(
            inner.slots.len(),
            i as usize + 1,
            "slots should grow by 1 per insert"
        );
    }
}
```

Run `cargo test -p tessera-graph insert_after_remove_does_not_grow_slots_beyond_capacity` — current code will FAIL because `push(None)` is called even when holes exist.

### GREEN — Minimal Correct Implementation

Replace the `count < self.capacity` branch in `insert()`:

```rust
// Find a free slot or evict via clock hand.
let slot_idx = if inner.count < self.capacity {
    if inner.slots.len() < self.capacity {
        // The vec has not yet reached its capacity length — append a new slot.
        inner.slots.push(None);
        inner.slots.len() - 1
    } else {
        // Vec is at full length but count < capacity means at least one hole exists.
        inner
            .slots
            .iter()
            .position(Option::is_none)
            .expect("invariant: count < capacity implies a None slot exists")
    }
} else {
    // Clock-hand eviction.
    Self::clock_sweep(&mut inner)
};
```

### REFACTOR

Run `cargo clippy -p tessera-graph`. The `expect()` call requires documenting the panic in the function docs. Add to `insert`'s doc comment:

```rust
/// # Panics
///
/// Panics if the internal invariant `count < capacity → ∃ None slot` is violated,
/// which indicates a bug in the eviction or remove logic.
pub fn insert(&self, node_id: u64, ptr: AdjacencyPointer) {
```

**Estimated time**: 25 min

---

## Cycle G-C2 — Fix `clock_sweep` Infinite Loop Risk + `clock_hand` Overflow

**Files**: `../tessera-graph/src/adj_cache.rs`
**Finding**: C2
**Problem**:
1. `clock_sweep` loops without an iteration limit. If every slot has `recently_used = true`, the loop clears each flag and then re-encounters the same slot before it can be evicted — a theoretical infinite loop if flags are concurrently re-set (they can be via `get()` which uses `Ordering::Relaxed`).
2. `clock_hand` grows unbounded: `inner.clock_hand = hand + 1` and `hand = inner.clock_hand % len`. After `usize::MAX / len` full rotations, `clock_hand` wraps via integer overflow. On 64-bit systems this is cosmetically harmless but is a latent bug on 32-bit or embedded targets.

Fix:
1. Cap iterations at `2 * len`. After one full pass to clear all flags and a second pass to find a cleared slot, if still no victim is found, the caller has a bug (all slots are `recently_used` and no eviction is possible).
2. After writing `clock_hand`, normalize with modulo: `inner.clock_hand = (hand + 1) % len`.

**Dependency**: Cycle G-C1 must be done first (the `clock_sweep` path is reached correctly only after G-C1 ensures the `else` branch is the eviction branch).

### RED — Write Failing Tests

```rust
// In adj_cache.rs tests:
#[test]
fn clock_sweep_terminates_when_all_recently_used() {
    // Fill cache so all slots have recently_used = true via get()
    let cache = AdjCache::new(8);
    for i in 0..8_u64 {
        cache.insert(i, ptr(Some(i as u32), None));
    }
    // Mark all as recently used
    for i in 0..8_u64 {
        cache.get(i);
    }
    // Insert one more — clock_sweep must terminate (give each entry a second chance,
    // then evict the first one that has been un-flagged on the second pass).
    cache.insert(99, ptr(Some(99), None));
    assert_eq!(cache.len(), 8);
    assert!(cache.get(99).is_some());
}

#[test]
fn clock_hand_does_not_grow_unbounded_after_many_evictions() {
    let cache = AdjCache::new(8);
    // Perform many insert/evict cycles — clock_hand should stay within [0, len)
    for i in 0..10_000_u64 {
        cache.insert(i, ptr(Some(i as u32 % u32::MAX), None));
        // Access a different node each time to vary recently_used state
        cache.get(i.saturating_sub(1));
    }
    let inner = cache.inner.read().expect("lock");
    // After normalization, hand should be in [0, slots.len())
    assert!(
        inner.clock_hand < inner.slots.len(),
        "clock_hand {} out of bounds for slots len {}",
        inner.clock_hand,
        inner.slots.len()
    );
}
```

Run `cargo test -p tessera-graph clock_sweep_terminates` and `clock_hand_does_not_grow_unbounded` — the second test will FAIL when `clock_hand` accumulates.

### GREEN — Minimal Correct Implementation

Replace `clock_sweep` in `adj_cache.rs`:

```rust
/// Clock-hand sweep: find a slot to evict.
///
/// Entries marked `recently_used` get one second chance (flag cleared, hand
/// moves on). The first entry found with `recently_used == false` is evicted.
///
/// The sweep is bounded to `2 * slots.len()` iterations: one pass to clear
/// all recently-used flags and a second pass to find an eviction candidate.
/// If no candidate is found within this bound, the invariant that at least
/// one slot is evictable has been violated.
///
/// `clock_hand` is normalised with modulo after each step to prevent unbounded
/// growth on long-running instances.
///
/// # Panics
///
/// Panics if all slots are occupied and none can be evicted after two full
/// passes — this indicates a concurrency bug where `recently_used` flags
/// are continuously re-set faster than the sweep can clear them, or that
/// the caller invoked `clock_sweep` with fewer occupied slots than expected.
fn clock_sweep(inner: &mut CacheInner) -> usize {
    let len = inner.slots.len();
    let max_iters = 2 * len;
    for _ in 0..max_iters {
        let hand = inner.clock_hand % len;
        inner.clock_hand = (hand + 1) % len;
        if let Some(slot) = &inner.slots[hand] {
            if slot.recently_used.swap(false, Ordering::Relaxed) {
                continue; // Second chance — spare this entry.
            }
            // Evict this slot.
            inner.map.remove(&slot.node_id);
            inner.count -= 1;
            return hand;
        }
    }
    panic!("clock_sweep: no evictable slot found after {max_iters} iterations — invariant violated");
}
```

### REFACTOR

Run `cargo clippy -p tessera-graph`. The `panic!` requires the `# Panics` section in the doc comment (already added above). Verify the existing `eviction_when_at_capacity`, `evicted_entry_on_get_returns_none`, and `clock_hand_evicts_unreferenced_entry` tests still pass.

**Estimated time**: 25 min

---

## Cycle G-R5 — Promote `AdjCache::len()` to Production Visibility

**Files**: `../tessera-graph/src/adj_cache.rs`
**Finding**: R5
**Problem**: `len()` and `is_empty()` are `#[cfg(test)]` only. Cache size is valuable for monitoring and health-check endpoints at runtime. `len()` should be production-visible. `is_empty()` and `capacity()` are less critical but having `len()` production-visible is the key ask.

**Decision**: Promote `len()` and `is_empty()` to unconditional `pub`. Keep `capacity()` production-visible too since it is read-only and useful for telemetry ratios. `clear()` stays `#[cfg(test)]` — production clear is a dangerous operation.

### RED — Write Failing Tests

The test does not fail at the test level — the issue is that `len()` is gated. The RED state is that calling `cache.len()` from production code (outside `#[cfg(test)]`) does not compile. We verify this intent with a doc test in the function:

```rust
// This doctest would fail to compile if len() is cfg(test) only:
/// ```
/// use tessera_graph::AdjCache;
/// let cache = AdjCache::new(16);
/// assert_eq!(cache.len(), 0);
/// ```
```

However, since `AdjCache` is internal to the crate (`pub` but tested from same crate), the simpler RED state is: the current `#[cfg(test)]` attribute on `len()` means it cannot be called from non-test code. After removing the attribute, a new test confirms it compiles in a hypothetical production context.

Add to `adj_cache.rs` tests (to run today, passing confirms the attribute change works):

```rust
#[test]
fn len_and_is_empty_reflect_count() {
    let cache = AdjCache::new(16);
    assert!(cache.is_empty());
    assert_eq!(cache.len(), 0);
    cache.insert(1, ptr(Some(0), None));
    assert!(!cache.is_empty());
    assert_eq!(cache.len(), 1);
    cache.remove(1);
    assert!(cache.is_empty());
}
```

This test already exists implicitly via other tests; the key action is removing `#[cfg(test)]`.

### GREEN — Minimal Correct Implementation

In `adj_cache.rs`, remove `#[cfg(test)]` from `len()`, `is_empty()`, and `capacity()`.
The `clear()` method retains `#[cfg(test)]`.

Before the change:
```rust
#[must_use]
#[cfg(test)]
pub fn len(&self) -> usize { ... }

#[must_use]
#[cfg(test)]
pub fn is_empty(&self) -> bool { ... }

#[must_use]
#[cfg(test)]
pub const fn capacity(&self) -> usize { ... }
```

After:
```rust
/// Returns the number of entries currently in the cache.
#[must_use]
pub fn len(&self) -> usize {
    self.inner.read().expect(LOCK_POISON_MSG).count
}

/// Returns `true` if the cache has no entries.
#[must_use]
pub fn is_empty(&self) -> bool {
    self.inner.read().expect(LOCK_POISON_MSG).count == 0
}

/// Returns the configured maximum capacity.
#[must_use]
pub const fn capacity(&self) -> usize {
    self.capacity
}
```

Add `# Panics` doc sections to `len()` and `is_empty()` (they call `.expect()`):

```rust
/// # Panics
///
/// Panics if the internal `RwLock` has been poisoned.
```

### REFACTOR

Run `cargo clippy -p tessera-graph`. `clippy::pedantic` requires `#[must_use]` on functions returning values — already present. Check that `missing_panics_doc` lint is satisfied by the `# Panics` sections.

**Estimated time**: 15 min

---

## Cycle G-R2 — Document `BufferPool::get_page` Write-Lock Trade-off

**Files**: `../tessera-graph/src/storage/buffer_pool.rs`
**Finding**: R2
**Problem**: `get_page` always acquires a write lock (line 101). For cache hits, a read lock would suffice for copying the data — the only write-lock-requiring operation is the LRU `touch`. The current implementation is correct but sub-optimal for read-heavy workloads.

**Decision** (as specified in the finding): Do NOT implement the optimization now. Add a `TODO` comment documenting the trade-off for a future performance work item.

### RED — Write Failing Tests

There is no behavioral failure — this is a documentation-only change. Run `cargo test -p tessera-graph` before and after to confirm no regression.

### GREEN — Minimal Correct Implementation

In `buffer_pool.rs`, add a comment block immediately before `let mut inner = self.inner.write()` in `get_page`:

```rust
// PERFORMANCE NOTE (TODO): This always acquires a write lock even for cache
// hits, because the LRU `touch_lru_inner` call requires mutation.
//
// A two-phase approach would be more efficient for read-heavy workloads:
//   1. Acquire read lock → check if key exists → copy data → release.
//   2. Acquire write lock → LRU touch only.
//
// The complication is the ABA race: a page evicted between phase 1 and
// phase 2 would require re-loading from disk under the write lock anyway.
// Implementing this optimisation safely requires either a seqlock or a
// separate "pin" counter to prevent eviction during phase 1.
//
// Benchmark before implementing: the 4096-byte memcpy under the write lock
// is the dominant cost, not the lock acquisition itself in low-contention
// scenarios. Profile with criterion before spending time on this.
let mut inner = self.inner.write().expect(LOCK_POISON_MSG);
```

### REFACTOR

Run `cargo clippy -p tessera-graph`. Comment-only change — no lint impact.

**Estimated time**: 10 min

---

## Cycle G-O3 — Document 4096-Byte Copy as Future Optimization

**Files**: `../tessera-graph/src/storage/buffer_pool.rs`
**Finding**: O3
**Problem**: The 4096-byte `copy_from_slice` on line 124 happens under the write lock, blocking all other threads that need any page. The comment exists (`// Copy data under the write lock, then release`) but does not explain why this is a problem or what the future path is.

### RED — Write Failing Tests

Documentation-only. Run tests before and after to confirm no regression.

### GREEN — Minimal Correct Implementation

Expand the existing comment around lines 122-125:

```rust
// Copy the page data while holding the write lock, then release immediately.
//
// PERFORMANCE NOTE: This 4096-byte memcpy blocks all concurrent readers and
// writers for its duration. For read-heavy workloads, consider:
//   - Storing pages as `Arc<PageBuf>` so the copy happens outside the lock
//     (callers get a cheap Arc clone; eviction can proceed independently).
//   - Alternatively, use a reader-writer lock with a "pinning" mechanism:
//     readers pin a page (preventing eviction) before releasing the lock,
//     then copy outside the lock, then unpin.
// Neither approach is implemented here because the bottleneck has not been
// confirmed by profiling. Measure with criterion before optimizing.
let mut copy = new_page_buf();
copy.copy_from_slice(inner.frames[&key].data.as_ref());
drop(inner);
Ok(copy)
```

### REFACTOR

Run `cargo clippy -p tessera-graph`. No code changes.

**Estimated time**: 10 min

---

## Cycle G-O4 — Remove Obsolete `RefCell` Comment in `buffer_pool` Test

**Files**: `../tessera-graph/src/storage/buffer_pool.rs`
**Finding**: O4
**Problem**: Lines 389-390 in the test `two_sequential_reads_do_not_conflict` contain a comment that references `RefCell` and `borrow_mut`, which was the previous implementation before the `RwLock` migration. This comment is misleading because the current implementation uses `RwLock`, not `RefCell`.

### RED — Write Failing Tests

Documentation-only. Read the stale comment at lines 389-390:

```rust
// Two reads from the same shared reference (sequential — RefCell
// doesn't allow two simultaneous Ref borrows from get_page because
// the first call does borrow_mut internally before borrow)
```

This comment is false: the current impl uses `RwLock`, not `RefCell`, and two concurrent reads CAN proceed simultaneously (they both take a read lock). The comment describes old behavior.

### GREEN — Minimal Correct Implementation

Replace the stale comment with an accurate one:

```rust
// Two sequential reads on the same pool — each call takes the write lock,
// reads from disk (or cache), copies the page, and releases the lock before
// the next read.  No deadlock is possible because the lock is not held
// across calls.
```

### REFACTOR

Run `cargo clippy -p tessera-graph`. No code changes.

**Estimated time**: 10 min

---

## Cycle G-O5 — `read_page_or_new` Should Log Errors

**Files**: `../tessera-graph/src/storage/file.rs`
**Finding**: O5
**Problem**: `read_page_or_new` (line 339-341) silently swallows any error from `pool.get_page()`:

```rust
fn read_page_or_new(pool: &BufferPool, file: DataFile, page_id: u32) -> PageBuf {
    pool.get_page(file, page_id).unwrap_or_else(|_| new_page_buf())
}
```

If `get_page` fails due to I/O corruption or a missing file, WAL replay continues on a zeroed page, potentially producing incorrect data silently. At minimum, the error should be logged.

**Decision**: The function signature cannot change (it returns `PageBuf`, not `Result<PageBuf>`) without touching all call sites in `replay_slot` and `replay_tombstone`. Log the error via `tracing::warn!` and continue with the zeroed page. Add `tracing` as a dependency to `tessera-graph` if not already present.

### RED — Write Failing Tests

The current behavior is silent swallow. There is no runtime test for "log was emitted". The behavioral contract is: if `get_page` fails, return a zeroed page AND emit a warning. We verify the zeroed-page behavior with a test:

```rust
// In file.rs or a new integration test — this test already works today:
// If pool.get_page returns Err, read_page_or_new must return new_page_buf()
// This is more of a documentation test; the log emission is the new behavior.
```

The RED state is the silent swallow. After GREEN, a tracing warning is emitted. Since we cannot assert on log output without a subscriber, we document this as an observability improvement only.

### GREEN — Minimal Correct Implementation

**Step 1**: Check if `tessera-graph` already has `tracing` as a dependency.
```
grep "tracing" ../tessera-graph/Cargo.toml
```

If absent, add `tracing = "0.1"` to `[dependencies]` in `../tessera-graph/Cargo.toml`.

**Step 2**: Add `use tracing;` import in `file.rs` (or rely on the macro's own `use` statement).

**Step 3**: Update `read_page_or_new`:

```rust
/// Reads a page from the pool, or returns a zeroed page if not available.
///
/// If `pool.get_page` returns an error (e.g., the file is not yet allocated
/// for the page being read during WAL replay), a zeroed page is returned and
/// a warning is emitted via `tracing`. Callers should treat the returned page
/// as potentially empty and handle absent data gracefully.
fn read_page_or_new(pool: &BufferPool, file: DataFile, page_id: u32) -> PageBuf {
    pool.get_page(file, page_id).unwrap_or_else(|err| {
        tracing::warn!(
            ?file,
            page_id,
            error = %err,
            "read_page_or_new: get_page failed, returning zeroed page"
        );
        new_page_buf()
    })
}
```

### REFACTOR

Run `cargo clippy -p tessera-graph`. The `?file` structured field uses the `Debug` format for `DataFile` — ensure `DataFile` derives `Debug` (it should already). Check for `clippy::used_underscore_binding` on the `err` variable — it is used in the `tracing::warn!` call so no issue.

**Estimated time**: 15 min

---

## Cycle G-O6 — Merge `GraphConfig` Impl Blocks

**Files**: `../tessera-graph/src/storage/file.rs`
**Finding**: O6
**Problem**: `GraphConfig` has two separate `impl GraphConfig` blocks (lines 32-43 and 45-56). One contains `new()` and the other contains `without_wal()`. There is no reason to have two separate blocks for the same type in the same file. Rust and clippy allow it, but it is non-idiomatic and makes reading the API surface harder.

### RED — Write Failing Tests

Structural-only change. Run `cargo test -p tessera-graph` before and after to confirm no regression. The "failing" state is confirmed by reading the file (two `impl GraphConfig` blocks exist) and `cargo clippy` will confirm no warning for this (it is legal). The fix is stylistic but directly requested.

### GREEN — Minimal Correct Implementation

Merge the two `impl GraphConfig` blocks into one. The result:

```rust
impl GraphConfig {
    /// Creates a new `GraphConfig` with the given memory limit and default settings.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            memory_limit_bytes: 64 * 1024 * 1024,
            create_if_missing: true,
            adj_cache_capacity: 65_536,
            wal_enabled: true,
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
        }
    }
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self::new()
    }
}
```

### REFACTOR

Run `cargo clippy -p tessera-graph`. Confirm no warnings. The `Default` impl is a separate `impl` block (different trait) — that is correct and idiomatic.

**Estimated time**: 10 min

---

## Cycle WIRE — Full Compilation and Test Suite Verification

**Files**: All modified files across both crates
**Purpose**: Confirm the complete set of changes compiles cleanly, all existing tests pass, and no new clippy warnings appear.

### Verification Steps

**Step 1 — Enterprise crate**

```bash
cd /path/to/tessera-graph-enterprise
cargo clippy -p tessera-storage-enterprise -- -D warnings 2>&1
cargo test -p tessera-storage-enterprise 2>&1
```

Expected: 0 errors, 0 warnings, all tests pass.

**Step 2 — Graph crate**

```bash
cd ../tessera-graph
cargo clippy -- -D warnings 2>&1
cargo test 2>&1
```

Expected: 0 errors, 0 warnings, all tests pass.

**Step 3 — Integration tests (enterprise)**

```bash
cd /path/to/tessera-graph-enterprise
cargo test --test txn_integration 2>&1
```

Expected: all 7 integration tests pass (including `sixteen_threads_concurrent_begin_commit` with 16 threads).

**Step 4 — Throughput floor (graph, ignored by default)**

```bash
cd ../tessera-graph
cargo test -- --ignored shared_graph_add_node_throughput_floor 2>&1
```

Expected: passes if run in release mode. In debug mode it uses the 10k floor.

**Step 5 — Documentation build**

```bash
cargo doc -p tessera-storage-enterprise --no-deps 2>&1
cargo doc -p tessera-graph --no-deps 2>&1
```

Expected: 0 warnings (all public APIs documented, `#[must_use]`, `# Panics`, `# Errors` in place).

### Definition of Done

- [ ] `cargo clippy -D warnings` passes for both crates
- [ ] All unit tests in `manager.rs`, `handle.rs`, `snapshot.rs`, `adj_cache.rs`, `buffer_pool.rs` pass
- [ ] All integration tests in `tests/txn_integration.rs` pass
- [ ] `cargo doc --no-deps` completes without warnings for both crates
- [ ] 17 findings fully addressed: C1, C2, C3, C4, R1, R2, R3, R4, R5, R6, R7, O1, O2, O3, O4, O5, O6

**Estimated time**: 15 min

---

## Estimation Summary

| Cycle | Finding(s) | Crate | Est. Time |
|-------|-----------|-------|-----------|
| E-C3 + E-R7 | C3, R7 | enterprise | 15 min |
| E-C4 | C4 | enterprise | 10 min |
| E-R3 | R3 | enterprise | 15 min |
| E-R4 | R4 | enterprise | 10 min |
| E-R6 | R6 | enterprise | 10 min |
| E-R1 | R1 | enterprise | 20 min |
| E-O2 | O2 | enterprise | 20 min |
| E-O1 | O1 | enterprise | 10 min |
| G-C1 | C1 | graph | 25 min |
| G-C2 | C2 | graph | 25 min |
| G-R5 | R5 | graph | 15 min |
| G-R2 | R2 | graph | 10 min |
| G-O3 | O3 | graph | 10 min |
| G-O4 | O4 | graph | 10 min |
| G-O5 | O5 | graph | 15 min |
| G-O6 | O6 | graph | 10 min |
| WIRE | all | both | 15 min |
| **Total** | | | **~4.1 h** |

---

## Notes for the Implementer

1. Always run `cargo clippy -p <crate> -- -D warnings` after each cycle. Do not accumulate cycles without verifying.

2. The `deny(clippy::all)` + `warn(clippy::pedantic, clippy::nursery)` configuration means warnings from `pedantic`/`nursery` do not fail the build but should still be addressed. For new `pub fn` additions, verify `#[must_use]` is present if the return value is meaningful.

3. Cycles E-R1 and E-O2 touch the public API surface (new `Display` impl, new `Drop` impl, new error variant arity). After completing E-R1, audit all pattern-match destructures of `TransactionNotActive` across both crates' test files and integration tests.

4. Cycle G-O5 requires `tracing` in `tessera-graph`. Check `../tessera-graph/Cargo.toml` before adding — it may already be present as a transitive dependency. If so, only add `tracing` explicitly if it is not already in `[dependencies]`.

5. After completing all cycles, create a commit on `feature/quality-fixes-p1.2` and open a PR to `develop`. The commit message should reference all 17 findings by tag.
