// Copyright 2026 BelowZero Security OU. All rights reserved.

//! `AdjacencyIndex` — enterprise O(1) adjacency pointer lookup.
//!
//! The MIT core's `AdjCache` has a fixed capacity of 65K entries. For graphs
//! with more than 65K nodes, any operation on a node outside the cache
//! degenerates to an O(N) page scan via `resolve_adj_pointer`. This index
//! provides an unbounded `HashMap<NodeId, AdjacencyPointer>` that is kept
//! in sync with every mutation, giving O(1) lookups regardless of graph size.

use std::collections::HashMap;

use tessera_graph::{AdjacencyPointer, NodeId};

/// Unbounded adjacency pointer index.
///
/// Maps every node that has adjacency pages to its `AdjacencyPointer`.
/// Nodes without edges are not stored (implicit `None`).
pub struct AdjacencyIndex {
    inner: HashMap<NodeId, AdjacencyPointer>,
}

impl AdjacencyIndex {
    /// Creates an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    /// Returns the adjacency pointer for `node`, or `None` if the node has no
    /// adjacency pages (isolated node or not yet indexed).
    #[must_use]
    pub fn get(&self, node: NodeId) -> Option<AdjacencyPointer> {
        self.inner.get(&node).copied()
    }

    /// Inserts or replaces the adjacency pointer for `node`.
    pub fn insert(&mut self, node: NodeId, ptr: AdjacencyPointer) {
        self.inner.insert(node, ptr);
    }

    /// Removes the adjacency pointer for `node`.
    ///
    /// Returns `true` if the entry existed.
    pub fn remove(&mut self, node: NodeId) -> bool {
        self.inner.remove(&node).is_some()
    }

    /// Updates only the outgoing page for `node`, preserving the incoming page.
    ///
    /// If the node has no entry yet, creates one with `incoming_page = None`.
    pub fn update_outgoing(&mut self, node: NodeId, page: Option<u32>) {
        let entry = self.inner.entry(node).or_insert(AdjacencyPointer {
            outgoing_page: None,
            incoming_page: None,
        });
        entry.outgoing_page = page;
    }

    /// Updates only the incoming page for `node`, preserving the outgoing page.
    ///
    /// If the node has no entry yet, creates one with `outgoing_page = None`.
    pub fn update_incoming(&mut self, node: NodeId, page: Option<u32>) {
        let entry = self.inner.entry(node).or_insert(AdjacencyPointer {
            outgoing_page: None,
            incoming_page: None,
        });
        entry.incoming_page = page;
    }

    /// Returns the number of indexed nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the index contains no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Default for AdjacencyIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ptr(out: Option<u32>, inc: Option<u32>) -> AdjacencyPointer {
        AdjacencyPointer {
            outgoing_page: out,
            incoming_page: inc,
        }
    }

    // ── Ciclo 1: struct exists and starts empty ──

    #[test]
    fn adj_index_starts_empty() {
        let index = AdjacencyIndex::new();
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);
        assert!(index.get(NodeId::from_raw(0)).is_none());
    }

    // ── Ciclo 2: insert and lookup ──

    #[test]
    fn adj_index_insert_and_get() {
        let mut index = AdjacencyIndex::new();
        let node = NodeId::from_raw(1);
        let ptr = make_ptr(Some(10), Some(20));
        index.insert(node, ptr);

        let got = index.get(node).expect("entry must exist"); // OK: test
        assert_eq!(got.outgoing_page, Some(10));
        assert_eq!(got.incoming_page, Some(20));
        assert!(index.get(NodeId::from_raw(99)).is_none());
        assert_eq!(index.len(), 1);
    }

    // ── Ciclo 3: remove ──

    #[test]
    fn adj_index_remove_clears_entry() {
        let mut index = AdjacencyIndex::new();
        let node = NodeId::from_raw(1);
        index.insert(node, make_ptr(Some(10), Some(20)));
        assert!(index.remove(node));
        assert!(index.get(node).is_none());
        assert!(index.is_empty());
    }

    #[test]
    fn adj_index_remove_nonexistent_returns_false() {
        let mut index = AdjacencyIndex::new();
        assert!(!index.remove(NodeId::from_raw(42)));
    }

    // ── Ciclo 4: partial update preserves other page ──

    #[test]
    fn adj_index_update_outgoing_preserves_incoming() {
        let mut index = AdjacencyIndex::new();
        let node = NodeId::from_raw(1);
        index.insert(node, make_ptr(Some(10), Some(20)));
        index.update_outgoing(node, Some(99));

        let ptr = index.get(node).expect("entry must exist"); // OK: test
        assert_eq!(ptr.outgoing_page, Some(99));
        assert_eq!(ptr.incoming_page, Some(20), "incoming must be preserved");
    }

    #[test]
    fn adj_index_update_incoming_preserves_outgoing() {
        let mut index = AdjacencyIndex::new();
        let node = NodeId::from_raw(1);
        index.insert(node, make_ptr(Some(10), Some(20)));
        index.update_incoming(node, Some(77));

        let ptr = index.get(node).expect("entry must exist"); // OK: test
        assert_eq!(ptr.outgoing_page, Some(10), "outgoing must be preserved");
        assert_eq!(ptr.incoming_page, Some(77));
    }

    #[test]
    fn adj_index_update_outgoing_creates_entry_if_missing() {
        let mut index = AdjacencyIndex::new();
        let node = NodeId::from_raw(5);
        index.update_outgoing(node, Some(42));

        let ptr = index.get(node).expect("entry must be created"); // OK: test
        assert_eq!(ptr.outgoing_page, Some(42));
        assert!(ptr.incoming_page.is_none());
    }

    #[test]
    fn adj_index_update_incoming_creates_entry_if_missing() {
        let mut index = AdjacencyIndex::new();
        let node = NodeId::from_raw(5);
        index.update_incoming(node, Some(42));

        let ptr = index.get(node).expect("entry must be created"); // OK: test
        assert!(ptr.outgoing_page.is_none());
        assert_eq!(ptr.incoming_page, Some(42));
    }
}
