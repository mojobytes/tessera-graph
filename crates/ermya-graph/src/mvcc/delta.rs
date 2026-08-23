// SPDX-License-Identifier: MIT

//! A `Delta`: one record's pending mutation held in memory until commit.
//!
//! Each delta carries two snapshots of the record it touches:
//!
//! - `prior` — the state *before* this delta, used for O(1) physical undo on
//!   rollback (Phase 4).
//! - `new` — the state *after* this delta, i.e. what a reader for whom the
//!   delta is visible must see. Carrying it lets reads resolve the visible
//!   value by walking the chain without re-executing the mutation.
//!
//! Both are needed: `prior` for rollback, `new` for visible reads. A delta
//! becomes visible to snapshot-`start_ts` readers once [`stamp_commit`] stamps
//! it with a `commit_ts`; before that only its own author sees it.
//!
//! [`stamp_commit`]: Delta::stamp_commit

use crate::edge::Edge;
use crate::node::Node;

/// A snapshot of a record's state, used both as a delta's `prior` (undo target)
/// and as its `new` (the version a visible read returns).
///
/// `Deleted` represents "the record does not exist" — as a `prior` it means the
/// delta is an insert (nothing existed before); as a `new` it means the delta
/// is a delete (nothing exists after).
#[derive(Debug, Clone, PartialEq)]
pub enum EntitySnapshot {
    /// The record is a node in this state.
    Node(Node),
    /// The record is an edge in this state.
    Edge(Edge),
    /// The record does not exist in this state.
    Deleted,
}

/// Fixed estimated cost of a `Deleted` snapshot: a tombstone holds no record
/// data, so it is charged a small constant (the delta slot overhead) that stays
/// below any real node/edge snapshot's estimate.
const DELETED_APPROX_SIZE: u64 = 16;

impl EntitySnapshot {
    /// Estimated in-memory bytes this snapshot occupies, used by the MVCC
    /// per-transaction memory cap to bound a transaction's uncommitted delta
    /// chain. It sums the fixed struct size and the heap owned by the label and
    /// each property (key length plus [`Property::approx_heap_size`]). `Deleted`
    /// owns no heap. The estimate is deliberately a lower bound (it ignores
    /// allocator slack and collection capacity), which is the correct bias for a
    /// defensive cap.
    #[must_use]
    pub fn approx_size(&self) -> u64 {
        let (base, label, props) = match self {
            Self::Node(n) => (size_of::<Node>(), n.label(), n.properties()),
            Self::Edge(e) => (size_of::<Edge>(), e.label(), e.properties()),
            // A tombstone retains no record data — only the delta slot that
            // holds it. Charge a small fixed cost, never more than a real
            // node/edge snapshot.
            Self::Deleted => return DELETED_APPROX_SIZE,
        };
        let props_bytes: usize = props
            .iter()
            .map(|(k, v)| k.len() + v.approx_heap_size())
            .sum();
        (base + label.len() + props_bytes) as u64
    }
}

/// The kind of mutation a [`Delta`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaOp {
    /// The record did not exist before this delta.
    Insert,
    /// The record existed and its contents changed.
    Update,
    /// The record existed and this delta removes it.
    Delete,
}

/// One pending mutation of a single record within a transaction.
///
/// See the module docs for why both `prior` and `new` are retained.
#[derive(Debug, Clone)]
pub struct Delta {
    txn_id: u64,
    /// Undo target, read by the Phase 4 rollback path. Held now so the delta is
    /// self-contained when rollback lands.
    prior: Option<EntitySnapshot>,
    new: Option<EntitySnapshot>,
    /// Mutation kind, read by the Phase 4 commit/rollback path.
    op: DeltaOp,
    commit_ts: Option<u64>,
}

impl Delta {
    /// Creates an uncommitted delta authored by `txn_id`.
    ///
    /// `prior` is the state this delta overwrites (`None` when the record had
    /// no committed base, i.e. a fresh insert); `new` is the state a visible
    /// read returns.
    pub const fn new(
        txn_id: u64,
        prior: Option<EntitySnapshot>,
        new: Option<EntitySnapshot>,
        op: DeltaOp,
    ) -> Self {
        Self {
            txn_id,
            prior,
            new,
            op,
            commit_ts: None,
        }
    }

    /// Returns the id of the transaction that authored this delta.
    pub const fn txn_id(&self) -> u64 {
        self.txn_id
    }

    /// Returns the state this delta overwrites (its undo target).
    ///
    /// Consumed by the Phase 4 rollback path, which restores `prior` to undo an
    /// aborted transaction's writes.
    pub const fn prior(&self) -> Option<&EntitySnapshot> {
        self.prior.as_ref()
    }

    /// Returns the state a reader sees when this delta is visible to it.
    pub const fn new_state(&self) -> Option<&EntitySnapshot> {
        self.new.as_ref()
    }

    /// Returns the kind of mutation this delta represents.
    ///
    /// Consumed by the Phase 4 commit/rollback path to decide how to materialize
    /// or undo each delta (insert vs update vs delete).
    pub const fn op(&self) -> DeltaOp {
        self.op
    }

    /// Returns the `commit_ts` once the authoring transaction has committed, or
    /// `None` while it is still in flight.
    pub const fn commit_ts(&self) -> Option<u64> {
        self.commit_ts
    }

    /// Stamps this delta with the transaction's `commit_ts`, making it visible
    /// to readers whose `start_ts` is strictly after `commit_ts`.
    ///
    /// Called by the Phase 4 commit path once per delta of the committing
    /// transaction.
    pub const fn stamp_commit(&mut self, commit_ts: u64) {
        self.commit_ts = Some(commit_ts);
    }

    /// Whether the vacuum may materialize this delta to its page and free it,
    /// given the `watermark` = the oldest live transaction's `start_ts`
    /// ([`crate::mvcc::TxnRegistry::oldest_active_start_ts`]).
    ///
    /// A delta is safe to vacuum only when it is committed AND no live reader
    /// can still need the delta chain to observe its snapshot:
    ///
    /// - An uncommitted delta (`commit_ts` is `None`) is NEVER safe — its
    ///   author might still roll back, and nobody but the author sees it.
    /// - With no live transactions (`watermark` is `None`), any committed delta
    ///   is safe: no held snapshot could need an older version.
    /// - With a live transaction whose `start_ts` is `w`, only deltas with
    ///   `commit_ts < w` are safe. A delta with `commit_ts == w` MUST be kept:
    ///   a reader with `start_ts == w` sees everything committed strictly
    ///   before `w`, so that version is invisible to it and materializing it to
    ///   the page would corrupt that reader's snapshot.
    pub const fn safe_to_vacuum(&self, watermark: Option<u64>) -> bool {
        match self.commit_ts {
            None => false,
            Some(commit_ts) => match watermark {
                None => true,
                Some(w) => commit_ts < w,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Delta, DeltaOp, EntitySnapshot};
    use crate::error::NodeId;
    use crate::node::Node;
    use crate::property::Properties;

    fn make_node(id: u64, label: &str) -> Node {
        Node::new(NodeId(id), label, Properties::new())
    }

    #[test]
    fn delta_holds_prior_state_and_txn_id() {
        let prior = Some(EntitySnapshot::Node(make_node(1, "Person")));
        let new = Some(EntitySnapshot::Node(make_node(1, "Person")));
        let delta = Delta::new(7, prior.clone(), new, DeltaOp::Update);
        assert_eq!(delta.txn_id(), 7);
        assert_eq!(delta.prior(), prior.as_ref());
        assert!(delta.commit_ts().is_none());
    }

    #[test]
    fn delta_carries_both_prior_and_new_state() {
        let prior = Some(EntitySnapshot::Deleted);
        let new = Some(EntitySnapshot::Node(make_node(2, "City")));
        let delta = Delta::new(3, prior.clone(), new.clone(), DeltaOp::Insert);
        assert_eq!(delta.prior(), prior.as_ref());
        assert_eq!(delta.new_state(), new.as_ref());
    }

    #[test]
    fn approx_size_grows_with_label_and_properties() {
        use crate::property::Property;
        let small = EntitySnapshot::Node(make_node(1, "A"));
        let mut props = Properties::new();
        props.insert("name".into(), Property::String("a-long-value".into()));
        let big = EntitySnapshot::Node(Node::new(NodeId(1), "A-longer-label", props));
        assert!(
            big.approx_size() > small.approx_size(),
            "a longer label and extra properties must raise the estimate"
        );
    }

    #[test]
    fn approx_size_deleted_is_small_and_fixed() {
        // `Deleted` owns no heap: its estimate is just the enum's own size and
        // never varies.
        assert_eq!(
            EntitySnapshot::Deleted.approx_size(),
            EntitySnapshot::Deleted.approx_size()
        );
        assert!(
            EntitySnapshot::Deleted.approx_size()
                < EntitySnapshot::Node(make_node(1, "X")).approx_size()
        );
    }

    #[test]
    fn delta_stamp_commit_sets_commit_ts() {
        let mut delta = Delta::new(1, None, Some(EntitySnapshot::Deleted), DeltaOp::Insert);
        delta.stamp_commit(42);
        assert_eq!(delta.commit_ts(), Some(42));
    }

    #[test]
    fn delta_op_is_preserved() {
        let delta = Delta::new(1, None, None, DeltaOp::Delete);
        assert_eq!(delta.op(), DeltaOp::Delete);
    }

    fn committed(commit_ts: u64) -> Delta {
        let mut d = Delta::new(1, None, Some(EntitySnapshot::Deleted), DeltaOp::Insert);
        d.stamp_commit(commit_ts);
        d
    }

    #[test]
    fn uncommitted_delta_is_never_safe_to_vacuum() {
        let d = Delta::new(1, None, None, DeltaOp::Insert);
        assert!(!d.safe_to_vacuum(None));
        assert!(!d.safe_to_vacuum(Some(100)));
    }

    #[test]
    fn committed_delta_is_safe_when_no_live_txn() {
        assert!(committed(5).safe_to_vacuum(None));
    }

    #[test]
    fn committed_delta_before_watermark_is_safe() {
        assert!(committed(5).safe_to_vacuum(Some(6)));
    }

    #[test]
    fn committed_delta_equal_to_watermark_is_not_safe() {
        // A reader with start_ts == watermark sees everything committed strictly
        // before it, so commit_ts == watermark must stay in the chain.
        assert!(!committed(6).safe_to_vacuum(Some(6)));
    }

    #[test]
    fn committed_delta_after_watermark_is_not_safe() {
        assert!(!committed(7).safe_to_vacuum(Some(6)));
    }
}
