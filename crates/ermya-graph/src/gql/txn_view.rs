// SPDX-License-Identifier: MIT

//! `TxnView`: a [`GraphAccess`](crate::access::GraphAccess) adapter that binds a
//! `&mut Graph` to a specific explicit-transaction `txn_id`.
//!
//! It lets the existing generic GQL executor run *inside* a transaction's MVCC
//! snapshot without any change to the compiler: every read routes to the
//! matching `_in_txn` engine method (reads at the transaction's `start_ts`,
//! seeing its own uncommitted writes) and every write routes to the `_in_txn`
//! mutation that records a delta instead of touching the committed page. The
//! auto-commit path is unaffected — it keeps running the same executor over a
//! plain `&Graph`.

use crate::access::GraphAccess;
use crate::edge::Edge;
use crate::error::{EdgeId, NodeId, Result};
use crate::graph::Graph;
use crate::node::Node;
use crate::property::Properties;

/// Binds a `&mut Graph` to a `txn_id` and exposes it as a [`GraphAccess`].
///
/// Routes every read/write to the graph's `_in_txn` methods so the generic GQL
/// executor runs inside the transaction's MVCC snapshot.
///
/// Live in production: the transactional mutation executors in
/// `gql::mutation_exec` and `gql::compiler` build a `TxnView` to run the read
/// phase of a write (and to evaluate SET right-hand sides) inside the open
/// transaction, so a statement sees its own uncommitted writes. The auto-commit
/// path never constructs one — it runs the same executor over a plain `&Graph`.
///
/// Exposed (`pub`) so the server's transactional accessor can build the
/// write-phase view under its write lock, mirroring how it holds a plain
/// `&mut Graph` for auto-commit writes.
pub struct TxnView<'a> {
    graph: &'a mut Graph,
    txn_id: u64,
}

impl<'a> TxnView<'a> {
    /// Wraps `graph`, scoping all access to transaction `txn_id`.
    #[must_use]
    pub const fn new(graph: &'a mut Graph, txn_id: u64) -> Self {
        Self { graph, txn_id }
    }
}

/// Read-only sibling of [`TxnView`] that binds a **shared** `&Graph` to a
/// `txn_id`, exposing the same snapshot reads as a [`GraphAccess`].
///
/// This is the lookup-phase adapter for the server's two-lock transactional
/// path. The A-vs-B contention measurement chose a read-lock lookup followed by
/// a write-lock apply for the mutation path; the transactional path preserves
/// that discipline, so its MATCH/MERGE lookup must run under a read lock — a
/// shared `&Graph`, which [`TxnView`] (holding `&mut Graph`) cannot provide.
/// `TxnReadView` fills exactly that slot: the engine's generic lookup functions
/// (`compile_match_bindings`, `merge_lookup`, `eval_unwind_and_match`) run over
/// it unchanged, seeing the txn's own uncommitted writes via the pending
/// overlay. It intentionally implements no writes — the apply phase runs later
/// over `&mut Graph` under the write lock.
///
/// Exposed (`pub`) so the server's transactional accessor can build the
/// lookup-phase view under its read lock, mirroring how it holds a plain
/// `&Graph` for auto-commit lookups.
pub struct TxnReadView<'a> {
    graph: &'a Graph,
    txn_id: u64,
}

impl<'a> TxnReadView<'a> {
    /// Wraps a shared `graph`, scoping all reads to transaction `txn_id`.
    #[must_use]
    pub const fn new(graph: &'a Graph, txn_id: u64) -> Self {
        Self { graph, txn_id }
    }
}

/// Implements the read half of [`GraphAccess`] for a type that exposes
/// `self.graph` (`&Graph`) and `self.txn_id` (`u64`), routing every read to the
/// matching `_in_txn` engine method. Shared verbatim by [`TxnView`] (which adds
/// the write half) and [`TxnReadView`] (read-only), so the snapshot-read logic
/// lives in one place.
macro_rules! txn_read_methods {
    () => {
        fn node_ids(&self) -> Vec<NodeId> {
            // Union the committed id set with this txn's pending overlay,
            // filtered by snapshot visibility, so the txn enumerates its own
            // uncommitted inserts (read-your-writes) — not just reach them by
            // id. `unwrap_or_default` is safe: the view is only built with a
            // txn_id the caller knows is active.
            self.graph.node_ids_in_txn(self.txn_id).unwrap_or_default()
        }

        fn nodes_by_label(&self, label: &str) -> Vec<NodeId> {
            self.graph
                .nodes_by_label_in_txn(self.txn_id, label)
                .unwrap_or_default()
        }

        fn node(&self, id: NodeId) -> Result<Node> {
            self.graph.node_in_txn(self.txn_id, id)
        }

        fn node_projected(&self, id: NodeId, _keys: &[&str]) -> Result<Node> {
            // No `_in_txn` projected read exists; return the full
            // snapshot-visible node. Projection is a decode optimization, not a
            // semantic filter, so returning all properties is correct (just not
            // minimal).
            self.graph.node_in_txn(self.txn_id, id)
        }

        fn node_exists(&self, id: NodeId) -> bool {
            self.graph.node_visible_in_txn(self.txn_id, id)
        }

        fn node_visible(&self, id: NodeId) -> bool {
            self.graph.node_visible_in_txn(self.txn_id, id)
        }

        fn node_count(&self) -> usize {
            self.node_ids().len()
        }

        fn edges_by_label(&self, label: &str) -> Vec<EdgeId> {
            // Edge visibility is resolved per-id via `edge_in_txn` at read time;
            // the label set is the committed superset, filtered here by snapshot.
            self.graph
                .edges_by_label(label)
                .into_iter()
                .filter(|&id| self.graph.edge_in_txn(self.txn_id, id).is_ok())
                .collect()
        }

        fn edge(&self, id: EdgeId) -> Result<Edge> {
            self.graph.edge_in_txn(self.txn_id, id)
        }

        fn edge_count(&self) -> usize {
            // The GQL compiler does not read `edge_count` through `GraphAccess`
            // (verified), so this is only a metadata convenience. There is no
            // public snapshot-aware edge enumerator, so report the committed
            // count; a snapshot-exact count is neither needed nor cheaply
            // available here.
            self.graph.edge_count()
        }

        fn outgoing_edges(&self, node: NodeId) -> Result<Vec<Edge>> {
            self.graph.outgoing_edges_in_txn(self.txn_id, node)
        }

        fn incoming_edges(&self, node: NodeId) -> Result<Vec<Edge>> {
            self.graph.incoming_edges_in_txn(self.txn_id, node)
        }

        fn outgoing_edges_by_label(&self, node: NodeId, label: &str) -> Result<Vec<Edge>> {
            self.graph
                .outgoing_edges_by_label_in_txn(self.txn_id, node, label)
        }

        fn incoming_edges_by_label(&self, node: NodeId, label: &str) -> Result<Vec<Edge>> {
            self.graph
                .incoming_edges_by_label_in_txn(self.txn_id, node, label)
        }
    };
}

impl GraphAccess for TxnReadView<'_> {
    txn_read_methods!();

    // Write half: unreachable for a read-only view. The engine's lookup phase
    // never calls these; the apply phase runs over `&mut Graph` under the write
    // lock, not over this adapter. Returning an error (rather than `panic!`)
    // keeps a misuse recoverable at the trait boundary.
    fn add_node(&mut self, _label: &str, _properties: Properties) -> Result<NodeId> {
        Err(read_only_write_error("add_node"))
    }

    fn update_node(&mut self, _id: NodeId, _node: &Node) -> Result<()> {
        Err(read_only_write_error("update_node"))
    }

    fn remove_node(&mut self, _id: NodeId) -> Result<Node> {
        Err(read_only_write_error("remove_node"))
    }

    fn add_edge(
        &mut self,
        _label: &str,
        _source: NodeId,
        _target: NodeId,
        _properties: Properties,
    ) -> Result<EdgeId> {
        Err(read_only_write_error("add_edge"))
    }

    fn update_edge(&mut self, _id: EdgeId, _edge: &Edge) -> Result<()> {
        Err(read_only_write_error("update_edge"))
    }

    fn remove_edge(&mut self, _id: EdgeId) -> Result<Edge> {
        Err(read_only_write_error("remove_edge"))
    }
}

/// Builds the error returned when a write is attempted through the read-only
/// [`TxnReadView`]. Centralised so the message shape is identical across the six
/// write methods.
fn read_only_write_error(method: &str) -> crate::Error {
    crate::Error::GqlCompileError(format!(
        "TxnReadView is read-only; {method} must run over a write-locked &mut Graph"
    ))
}

impl GraphAccess for TxnView<'_> {
    txn_read_methods!();

    // ── Node mutations ───────────────────────────────────────────────────────

    fn add_node(&mut self, label: &str, properties: Properties) -> Result<NodeId> {
        self.graph.add_node_in_txn(self.txn_id, label, properties)
    }

    fn update_node(&mut self, id: NodeId, node: &Node) -> Result<()> {
        self.graph.update_node_in_txn(self.txn_id, id, node)
    }

    fn remove_node(&mut self, id: NodeId) -> Result<Node> {
        // `remove_node_in_txn` returns `()`; `GraphAccess::remove_node` must
        // return the removed node, so read it (at the txn snapshot) first.
        let node = self.graph.node_in_txn(self.txn_id, id)?;
        self.graph.remove_node_in_txn(self.txn_id, id)?;
        Ok(node)
    }

    // ── Edge mutations ───────────────────────────────────────────────────────

    fn add_edge(
        &mut self,
        label: &str,
        source: NodeId,
        target: NodeId,
        properties: Properties,
    ) -> Result<EdgeId> {
        self.graph
            .add_edge_in_txn(self.txn_id, label, source, target, properties)
    }

    fn update_edge(&mut self, id: EdgeId, edge: &Edge) -> Result<()> {
        self.graph.update_edge_in_txn(self.txn_id, id, edge)
    }

    fn remove_edge(&mut self, id: EdgeId) -> Result<Edge> {
        let edge = self.graph.edge_in_txn(self.txn_id, id)?;
        self.graph.remove_edge_in_txn(self.txn_id, id)?;
        Ok(edge)
    }
}

#[cfg(test)]
mod tests {
    use crate::access::GraphAccess;
    use crate::gql::txn_view::TxnView;
    use crate::{Graph, Properties};

    use crate::edge::Edge;
    use crate::property::Property;

    #[test]
    fn txn_view_reads_txns_own_uncommitted_node() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(txn, "Person", Properties::new()).unwrap();

        // The view, bound to this txn, sees the txn's own pending node.
        let view = TxnView::new(&mut g, txn);
        assert_eq!(view.node(id).unwrap().label(), "Person");
    }

    // ── Cycle 1: read isolation ──────────────────────────────────────────────

    #[test]
    fn txn_view_isolation_own_write_visible_autocommit_not() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let id = g.add_node_in_txn(txn, "N", Properties::new()).unwrap();

        // Inside the txn: visible.
        {
            let view = TxnView::new(&mut g, txn);
            assert!(view.node(id).is_ok());
            assert!(view.node_exists(id));
            assert_eq!(view.node_ids(), vec![id]);
        }
        // Auto-commit reader (no commit yet): NOT visible.
        assert!(
            g.node(id).is_err(),
            "uncommitted txn write must be invisible to auto-commit"
        );
        assert!(!g.node_visible(id));
    }

    #[test]
    fn txn_view_sees_committed_base_and_hides_others_uncommitted() {
        let mut g = Graph::new();
        g.enable_mvcc();
        // Committed node visible to everyone.
        let committed = g.add_node("Base", Properties::new()).unwrap();
        // A separate txn writes an uncommitted node.
        let other = g.begin_txn().unwrap();
        let hidden = g
            .add_node_in_txn(other, "Hidden", Properties::new())
            .unwrap();
        // Our txn (started after) sees the committed base but not the other's pending node.
        let reader = g.begin_txn().unwrap();
        let view = TxnView::new(&mut g, reader);
        assert!(view.node(committed).is_ok());
        assert!(
            view.node(hidden).is_err(),
            "must not see another txn's uncommitted node"
        );
        g.rollback_txn(other).unwrap();
        g.rollback_txn(reader).unwrap();
    }

    // ── Cycle 2: writes through the adapter ──────────────────────────────────

    #[test]
    fn txn_view_add_node_via_trait_is_txn_scoped() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        let id = {
            let mut view = TxnView::new(&mut g, txn);
            let id = view.add_node("Person", Properties::new()).unwrap();
            assert!(view.node(id).is_ok()); // own write visible in-txn
            id
        };
        assert!(g.node(id).is_err()); // not auto-commit-visible
        g.commit_txn(txn).unwrap();
        assert!(g.node(id).is_ok()); // visible after commit
    }

    #[test]
    fn txn_view_update_and_remove_node_via_trait() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let id = g.add_node("Person", Properties::new()).unwrap();
        let txn = g.begin_txn().unwrap();
        {
            let mut view = TxnView::new(&mut g, txn);
            let mut n = view.node(id).unwrap();
            n.properties_mut()
                .insert("k".into(), Property::String("v".into()));
            view.update_node(id, &n).unwrap();
            assert_eq!(
                view.node(id).unwrap().properties().get("k"),
                Some(&Property::String("v".into()))
            );
            // remove returns the node that existed at the txn snapshot
            let removed = view.remove_node(id).unwrap();
            assert_eq!(removed.id().0, id.0);
            assert!(view.node(id).is_err());
        }
        // auto-commit still sees the original until commit
        assert!(g.node(id).is_ok());
    }

    #[test]
    fn txn_view_edge_writes_and_traversal_via_trait() {
        let mut g = Graph::new();
        g.enable_mvcc();
        let a = g.add_node("N", Properties::new()).unwrap();
        let b = g.add_node("N", Properties::new()).unwrap();
        let txn = g.begin_txn().unwrap();
        {
            let mut view = TxnView::new(&mut g, txn);
            let eid = view.add_edge("REL", a, b, Properties::new()).unwrap();
            assert!(view.edge(eid).is_ok());
            // traversal via the adapter sees the txn's own pending edge
            assert_eq!(view.outgoing_edges(a).unwrap().len(), 1);
            let removed: Edge = view.remove_edge(eid).unwrap();
            assert_eq!(removed.label(), "REL");
            assert_eq!(view.outgoing_edges(a).unwrap().len(), 0);
        }
        // no edge leaked to auto-commit
        assert_eq!(g.outgoing_edges(a).unwrap().len(), 0);
    }

    // ── Cycle 20: read-your-writes end-to-end through the parsed GQL path ─────
    //
    // This is the integration the Bolt handler will drive in Phase 5: a write
    // executed inside an open transaction, followed by a *parsed* `MATCH …
    // RETURN` read that must see that uncommitted write — via the exact same
    // engine entry points (`parse_statement` + `execute_bare_mutation` for the
    // write, `parse` + `execute_with_deadline` over a `TxnView` for the read),
    // not an ad-hoc `TxnView` method call. The read runs over the `TxnView`, so
    // `TxnView`'s overlay-backed `node_ids`/`nodes_by_label` (Cycle 19) are what
    // make the created node visible to the MATCH. Auto-commit stays blind until
    // COMMIT.
    #[test]
    fn begin_create_match_sees_own_write_within_txn() {
        use crate::gql;

        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();

        // Write phase: CREATE inside the txn, via the same parsed-mutation path
        // the server's bare-CREATE branch uses, carrying the open txn id.
        let create = match gql::parse_statement("CREATE (n:Persona) RETURN n").unwrap() {
            gql::GqlStatement::Mutation(m) => m,
            other => panic!("expected mutation, got {other:?}"),
        };
        let (_rows, stats) = crate::gql::mutation_exec::execute_bare_mutation(
            &mut g,
            &create,
            &std::collections::HashMap::new(),
            Some(txn),
        )
        .unwrap();
        assert_eq!(
            stats.nodes_created, 1,
            "CREATE must produce one pending node"
        );

        // Read phase: a parsed MATCH … RETURN executed over the TxnView must see
        // the txn's own uncommitted node (read-your-writes).
        let query = gql::parse("MATCH (n:Persona) RETURN n").unwrap();
        {
            let view = TxnView::new(&mut g, txn);
            let rows = gql::execute_with_deadline(&view, &query, 0, None)
                .expect("read over TxnView must not fail");
            assert_eq!(
                rows.len(),
                1,
                "MATCH in the same txn must see the node it just CREATEd"
            );
        }

        // Isolation: auto-commit readers see nothing until COMMIT.
        assert_eq!(
            g.nodes_by_label("Persona").len(),
            0,
            "uncommitted CREATE must be invisible to auto-commit before COMMIT"
        );
        g.commit_txn(txn).unwrap();
        assert_eq!(
            g.nodes_by_label("Persona").len(),
            1,
            "COMMIT must make the created node visible to auto-commit"
        );
    }

    // ── Cycle 22: read-only txn view over `&Graph` (two-lock lookup phase) ────
    //
    // The server's transactional path preserves the two-lock discipline chosen
    // by the A-vs-B contention measurement: the lookup phase must run under a
    // *read* lock (a shared `&Graph`), never a write lock, so a costly MATCH
    // inside a transaction does not stall concurrent auto-commit readers. That
    // rules out `TxnView`, which holds `&mut Graph`. `TxnReadView` binds a
    // shared `&Graph` to a `txn_id` and exposes the same snapshot reads
    // (read-your-writes over the txn's pending overlay) as a `GraphAccess`, so
    // the engine's generic lookup functions run over it unchanged.
    #[test]
    fn txn_read_view_over_shared_ref_sees_own_pending_write() {
        use crate::gql::txn_view::TxnReadView;

        let mut g = Graph::new();
        g.enable_mvcc();
        let committed = g.add_node("Base", Properties::new()).unwrap();
        let txn = g.begin_txn().unwrap();
        let pending = g
            .add_node_in_txn(txn, "Persona", Properties::new())
            .unwrap();

        // Build the read-only view over a *shared* borrow — the borrow a read
        // lock yields. This must compile (no `&mut`) and must enumerate the
        // txn's own pending node alongside the committed base.
        let shared: &Graph = &g;
        let view = TxnReadView::new(shared, txn);

        assert!(view.node(pending).is_ok(), "own pending node visible by id");
        assert!(view.node(committed).is_ok(), "committed base visible");

        let mut ids = view.node_ids();
        ids.sort_by_key(|n| n.0);
        let mut expected = vec![committed, pending];
        expected.sort_by_key(|n| n.0);
        assert_eq!(ids, expected, "enumeration unions committed + own pending");

        assert_eq!(
            view.nodes_by_label("Persona"),
            vec![pending],
            "label lookup sees own pending node"
        );
    }

    #[test]
    fn txn_read_view_drives_engine_lookup_without_write_lock() {
        use crate::gql;
        use crate::gql::txn_view::TxnReadView;

        let mut g = Graph::new();
        g.enable_mvcc();
        let txn = g.begin_txn().unwrap();
        g.add_node_in_txn(txn, "Persona", Properties::new())
            .unwrap();

        // The engine's generic MATCH compiler runs over the shared read-only
        // view and sees the txn's own uncommitted write — the exact lookup a
        // server read lock will perform in Phase 4.
        let shared: &Graph = &g;
        let view = TxnReadView::new(shared, txn);
        let query = gql::parse("MATCH (n:Persona) RETURN n").unwrap();
        let rows = gql::execute_with_deadline(&view, &query, 0, None)
            .expect("read over TxnReadView must not fail");
        assert_eq!(rows.len(), 1, "MATCH sees the txn's own pending node");
    }
}
