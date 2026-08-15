// SPDX-License-Identifier: MIT

//! Read visibility: resolve the version of a record a given reader may see.

use super::chain::DeltaChainHead;
use super::delta::{Delta, EntitySnapshot};

/// Resolves the version of a record visible to a reader, given the committed
/// base (what is on the page today) and the record's delta chain.
///
/// The chain is walked newest-first (the order [`DeltaChainHead`] maintains).
/// The first delta [visible](delta_visible_to) to the reader determines the
/// result — its `new_state`, i.e. the record *after* that mutation. If no delta
/// is visible, the reader sees `committed_base`.
///
/// Own writes win: a reader that is itself the author sees its uncommitted
/// deltas. Otherwise a delta is visible only once committed with a `commit_ts`
/// strictly before the reader's `start_ts` — the snapshot-isolation rule,
/// matching [`crate::mvcc::clock::TxnClock`]'s "sees everything committed
/// strictly before that value". A committed version returned as
/// `Some(EntitySnapshot::Deleted)` means the record was deleted and the caller
/// must treat it as absent.
pub fn apply_deltas_for_read(
    committed_base: Option<EntitySnapshot>,
    chain: &DeltaChainHead,
    reader_start_ts: u64,
    reader_txn_id: Option<u64>,
) -> Option<EntitySnapshot> {
    for delta in chain.iter() {
        if delta_visible_to(delta, reader_start_ts, reader_txn_id) {
            return delta.new_state().cloned();
        }
    }
    committed_base
}

/// Returns `true` when `delta` is visible to a reader with `reader_start_ts`
/// and (optionally) `reader_txn_id`.
///
/// Authorship is checked first: a reader always sees its own writes, committed
/// or not. Otherwise the delta must be committed with a `commit_ts` strictly
/// before the reader's snapshot.
///
/// Strict `<` (not `<=`) matches the clock contract. The `commit_ts` of one
/// transaction can never equal another's `start_ts`: [`TxnClock::next`] issues
/// each value exactly once, so distinct transactions never share a timestamp.
/// The only `commit_ts == reader_start_ts` case would be a reader against its
/// own commit, which the authorship check above already resolves. `<` and `<=`
/// are therefore equivalent under the current clock, and `<` is chosen because
/// it states the invariant the docs promise rather than relying on uniqueness.
///
/// [`TxnClock::next`]: crate::mvcc::clock::TxnClock::next
fn delta_visible_to(delta: &Delta, reader_start_ts: u64, reader_txn_id: Option<u64>) -> bool {
    if reader_txn_id == Some(delta.txn_id()) {
        return true;
    }
    delta.commit_ts().is_some_and(|c| c < reader_start_ts)
}

#[cfg(test)]
mod tests {
    use super::apply_deltas_for_read;
    use crate::error::NodeId;
    use crate::mvcc::chain::DeltaChainHead;
    use crate::mvcc::delta::{Delta, DeltaOp, EntitySnapshot};
    use crate::node::Node;
    use crate::property::Properties;

    fn node_snapshot(id: u64, label: &str) -> EntitySnapshot {
        EntitySnapshot::Node(Node::new(NodeId(id), label, Properties::new()))
    }

    #[test]
    fn visibility_reader_before_any_commit_sees_only_committed_base() {
        let committed_base = Some(node_snapshot(1, "Person"));
        let mut chain = DeltaChainHead::new();
        let mut delta = Delta::new(
            42,
            Some(node_snapshot(1, "Person")),
            Some(node_snapshot(1, "PersonV2")),
            DeltaOp::Update,
        );
        delta.stamp_commit(10);
        chain.push(delta);

        // start_ts=5 is before the commit at 10 → base only.
        let visible = apply_deltas_for_read(committed_base.clone(), &chain, 5, None);
        assert_eq!(visible, committed_base);
    }

    #[test]
    fn visibility_reader_after_commit_sees_delta() {
        let committed_base = Some(node_snapshot(1, "Person"));
        let after = node_snapshot(1, "PersonV2");
        let mut chain = DeltaChainHead::new();
        let mut delta = Delta::new(
            42,
            Some(node_snapshot(1, "Person")),
            Some(after.clone()),
            DeltaOp::Update,
        );
        delta.stamp_commit(10);
        chain.push(delta);

        // start_ts=15 is after the commit → delta's new state.
        let visible = apply_deltas_for_read(committed_base, &chain, 15, None);
        assert_eq!(visible, Some(after));
    }

    #[test]
    fn visibility_writer_sees_own_uncommitted_delta() {
        let base = node_snapshot(1, "Person");
        let after = node_snapshot(1, "PersonV2");
        let mut chain = DeltaChainHead::new();
        chain.push(Delta::new(
            7,
            Some(base.clone()),
            Some(after.clone()),
            DeltaOp::Update,
        ));
        let visible = apply_deltas_for_read(Some(base), &chain, 100, Some(7));
        assert_eq!(visible, Some(after));
    }

    #[test]
    fn visibility_other_txn_does_not_see_uncommitted_delta() {
        let base = node_snapshot(1, "Person");
        let after = node_snapshot(1, "PersonV2");
        let mut chain = DeltaChainHead::new();
        chain.push(Delta::new(
            7,
            Some(base.clone()),
            Some(after),
            DeltaOp::Update,
        ));
        // Reader is a different txn; delta is uncommitted → base only.
        let visible = apply_deltas_for_read(Some(base.clone()), &chain, 100, Some(999));
        assert_eq!(visible, Some(base));
    }

    #[test]
    fn visibility_committed_delete_hides_record() {
        let committed_base = Some(node_snapshot(1, "Person"));
        let mut chain = DeltaChainHead::new();
        let mut delta = Delta::new(
            5,
            Some(node_snapshot(1, "Person")),
            Some(EntitySnapshot::Deleted),
            DeltaOp::Delete,
        );
        delta.stamp_commit(10);
        chain.push(delta);
        let visible = apply_deltas_for_read(committed_base, &chain, 20, None);
        assert_eq!(visible, Some(EntitySnapshot::Deleted));
    }

    #[test]
    fn visibility_empty_chain_returns_base() {
        let base = Some(node_snapshot(1, "Person"));
        let chain = DeltaChainHead::new();
        assert_eq!(apply_deltas_for_read(base.clone(), &chain, 50, None), base);
    }

    // A commit exactly at the reader's start_ts is NOT visible (strict `<`):
    // the reader's snapshot predates that commit.
    #[test]
    fn visibility_commit_ts_equal_to_start_ts_is_not_visible() {
        let base = Some(node_snapshot(1, "Person"));
        let after = node_snapshot(1, "PersonV2");
        let mut chain = DeltaChainHead::new();
        let mut delta = Delta::new(
            9,
            Some(node_snapshot(1, "Person")),
            Some(after),
            DeltaOp::Update,
        );
        delta.stamp_commit(10);
        chain.push(delta);
        // start_ts == commit_ts == 10 → not yet visible, base wins.
        assert_eq!(apply_deltas_for_read(base.clone(), &chain, 10, None), base);
    }

    // Multi-delta chain: the walk is newest-first and returns the first VISIBLE
    // delta, skipping a newer one whose commit the reader's snapshot predates.
    #[test]
    fn visibility_multi_delta_returns_newest_visible_skipping_invisible() {
        let base = Some(node_snapshot(1, "V0"));
        let mut chain = DeltaChainHead::new();
        // Older committed delta, visible to start_ts=25.
        let mut older = Delta::new(
            1,
            base.clone(),
            Some(node_snapshot(1, "V1")),
            DeltaOp::Update,
        );
        older.stamp_commit(10);
        chain.push(older);
        // Newer committed delta, committed at 30 — NOT visible to start_ts=25.
        let mut newer = Delta::new(
            2,
            Some(node_snapshot(1, "V1")),
            Some(node_snapshot(1, "V2")),
            DeltaOp::Update,
        );
        newer.stamp_commit(30);
        chain.push(newer);

        // Reader at 25 skips V2 (commit 30) and sees V1 (commit 10).
        assert_eq!(
            apply_deltas_for_read(base.clone(), &chain, 25, None),
            Some(node_snapshot(1, "V1"))
        );
        // Reader at 35 sees the newest, V2.
        assert_eq!(
            apply_deltas_for_read(base, &chain, 35, None),
            Some(node_snapshot(1, "V2"))
        );
    }

    // Delete then re-insert on the same key: newest-first walk must return the
    // re-inserted record, not the earlier delete.
    #[test]
    fn visibility_delete_then_insert_returns_reinserted_record() {
        let base = Some(node_snapshot(1, "Person"));
        let mut chain = DeltaChainHead::new();
        let mut del = Delta::new(
            1,
            Some(node_snapshot(1, "Person")),
            Some(EntitySnapshot::Deleted),
            DeltaOp::Delete,
        );
        del.stamp_commit(10);
        chain.push(del);
        let reinserted = node_snapshot(1, "PersonAgain");
        let mut ins = Delta::new(
            2,
            Some(EntitySnapshot::Deleted),
            Some(reinserted.clone()),
            DeltaOp::Insert,
        );
        ins.stamp_commit(20);
        chain.push(ins);

        assert_eq!(
            apply_deltas_for_read(base, &chain, 30, None),
            Some(reinserted)
        );
    }
}
