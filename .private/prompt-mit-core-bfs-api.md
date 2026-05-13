# MIT Core — Expose Edge Endpoint API (replace BFS optimization branch)

**Date**: 2026-04-05
**Target repo**: `tessera-graph` (MIT core)
**Replaces**: branch `feature/bfs-traversal-optimization` (commit `6ce8d77`)

---

## Context

The branch `feature/bfs-traversal-optimization` implements a BFS/DFS traversal
optimization that reads only 16 bytes (source+target) per edge slot instead of
materializing the full Edge. This is a **performance optimization** and per the
architecture decision in `tessera-graph-enterprise/docs/architecture/ROADMAP.md`,
performance optimizations belong in the enterprise repo, not the MIT core.

However, the optimization requires access to raw edge slot data that only `Graph`
can read (it owns `self.storage`). The MIT core must expose a minimal **data
access API** that enterprise can build on. Exposing "read the endpoints of an
edge" is a legitimate data access primitive, not an optimization.

---

## What to do

### 1. DELETE the feature branch

```bash
git branch -D feature/bfs-traversal-optimization
```

The commit `6ce8d77` is only on this branch (not merged to develop). Nothing
is lost — the enterprise repo will reimplement the optimization using the new
API.

### 2. Create a new branch from develop

```bash
git checkout develop
git checkout -b feature/edge-endpoint-api
```

### 3. Expose TWO public methods on `Graph`

**File**: `src/graph.rs`

Add these two methods to the `impl Graph` block. The implementations are
taken directly from commit `6ce8d77` but with `pub` visibility:

```rust
/// Reads only `(source_id, target_id)` from an edge slot without
/// decoding label or properties.
///
/// This is a low-level data access primitive. For traversal, use the
/// `traverse()` builder which provides BFS/DFS with depth limits and
/// direction filtering.
///
/// # Errors
///
/// Returns [`Error::EdgeNotFound`] if the edge slot does not exist or
/// the page cannot be read.
pub fn read_edge_endpoints(&self, id: u64) -> Result<(u64, u64)> {
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
/// Returns [`Error::EdgeNotFound`] if the edge slot does not exist.
pub fn read_edge_label_hash(&self, id: u64) -> Result<u32> {
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
```

### 4. Make edge_codec offset constants `pub(crate)`

**File**: `src/storage/codec/edge_codec.rs`

Change these three constants from private to `pub(crate)`:

```rust
pub(crate) const OFF_SOURCE: usize = 9;
pub(crate) const OFF_TARGET: usize = 17;
pub(crate) const OFF_LABEL_HASH: usize = 25;
```

These are slot layout constants, not implementation secrets. They need to
be accessible from `graph.rs` which is in the same crate.

### 5. Write tests

**File**: `src/graph.rs` (inline tests) or `tests/integration/edge_api.rs`

```rust
#[test]
fn read_edge_endpoints_returns_source_and_target() {
    let mut g = Graph::new();
    let a = g.add_node("N", props! {}).unwrap();
    let b = g.add_node("N", props! {}).unwrap();
    let eid = g.add_edge("E", a, b, props! {}).unwrap();
    let (src, tgt) = g.read_edge_endpoints(eid.as_u64()).unwrap();
    assert_eq!(src, a.as_u64());
    assert_eq!(tgt, b.as_u64());
}

#[test]
fn read_edge_label_hash_is_consistent() {
    let mut g = Graph::new();
    let a = g.add_node("N", props! {}).unwrap();
    let b = g.add_node("N", props! {}).unwrap();
    let e1 = g.add_edge("KNOWS", a, b, props! {}).unwrap();
    let e2 = g.add_edge("KNOWS", b, a, props! {}).unwrap();
    let h1 = g.read_edge_label_hash(e1.as_u64()).unwrap();
    let h2 = g.read_edge_label_hash(e2.as_u64()).unwrap();
    assert_eq!(h1, h2, "same label must produce same hash");
}

#[test]
fn read_edge_endpoints_invalid_id_returns_error() {
    let g = Graph::new();
    assert!(g.read_edge_endpoints(9999).is_err());
}
```

### 6. Verify

```bash
cargo test -p tessera-graph
cargo clippy -p tessera-graph -- -D warnings
```

---

## What NOT to do

- **Do NOT** implement `neighbor_endpoints_for_direction` — that's enterprise optimization logic
- **Do NOT** modify `traversal.rs` — enterprise will provide an optimized traversal
- **Do NOT** modify `filtered_neighbors` — enterprise wraps this
- **Do NOT** merge the old `feature/bfs-traversal-optimization` branch — delete it

---

## After merge

Once `read_edge_endpoints` and `read_edge_label_hash` are public in the
core's develop branch, the enterprise repo will:

1. Import them via the `tessera-graph` dependency
2. Implement `neighbor_endpoints_for_direction` as an enterprise extension
3. Build the optimized BFS/DFS traversal using these primitives
4. The optimization becomes enterprise value, not MIT giveaway
