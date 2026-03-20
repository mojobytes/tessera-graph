use tessera_graph::{Graph, SharedGraph, props};
use tessera_storage_enterprise::txn::{
    IsolationLevel, TransactionHandle, TransactionManager, TxnState,
};

#[test]
fn committed_write_is_visible_via_read_committed() {
    let wal_tmp = tempfile::NamedTempFile::new().unwrap();
    let graph = SharedGraph::new(Graph::new());
    let mgr = TransactionManager::open(wal_tmp.path()).unwrap();

    let mut txn = mgr.begin(IsolationLevel::ReadCommitted).unwrap();

    // Write inside the transaction scope
    let node_id = graph
        .write()
        .add_node("Person", props! { "name" => "Alice" })
        .unwrap();

    // Before commit, txn not in commit log
    assert!(!mgr.is_committed(txn.txn_id()).unwrap());

    mgr.commit(&mut txn).unwrap();

    // After commit, txn is in commit log
    assert!(mgr.is_committed(txn.txn_id()).unwrap());

    // The node exists in the graph
    let node = graph.read().node(node_id).unwrap();
    assert_eq!(node.label(), "Person");
}

#[test]
fn rolled_back_transaction_is_not_committed() {
    let wal_tmp = tempfile::NamedTempFile::new().unwrap();
    let mgr = TransactionManager::open(wal_tmp.path()).unwrap();
    let mut txn = mgr.begin(IsolationLevel::ReadCommitted).unwrap();
    let id = txn.txn_id();
    mgr.rollback(&mut txn).unwrap();
    assert!(!mgr.is_committed(id).unwrap());
    assert_eq!(txn.state(), TxnState::RolledBack);
}

#[test]
fn snapshot_isolation_captures_committed_state() {
    let wal_tmp = tempfile::NamedTempFile::new().unwrap();
    let graph = SharedGraph::new(Graph::new());
    let mgr = TransactionManager::open(wal_tmp.path()).unwrap();

    // T1: add node and commit
    let mut t1 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
    graph.write().add_node("N1", props! {}).unwrap();
    mgr.commit(&mut t1).unwrap();

    // T2: begins with SI — sees T1 committed
    let t2 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
    let snap = t2.snapshot().unwrap();
    assert!(snap.is_visible(t1.txn_id()));

    // T3: begins after T2 — not visible in T2's snapshot
    let t3 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
    assert!(!snap.is_visible(t3.txn_id()));

    // Graph has the node regardless (no slot-level MVCC yet)
    assert_eq!(graph.read().node_count(), 1);
}

#[test]
fn concurrent_transactions_have_unique_ids() {
    let wal_tmp = tempfile::NamedTempFile::new().unwrap();
    let mgr = TransactionManager::open(wal_tmp.path()).unwrap();
    let handles: Vec<_> = (0..100)
        .map(|_| mgr.begin(IsolationLevel::ReadCommitted).unwrap())
        .collect();

    let mut ids: Vec<u64> = handles.iter().map(TransactionHandle::txn_id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 100);
}

#[test]
fn sixteen_threads_concurrent_begin_commit() {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::thread;

    const THREAD_COUNT: usize = 16;
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mgr = Arc::new(TransactionManager::open(tmp.path()).unwrap());

    // collect() is intentional: all threads must be spawned before joining
    // to achieve actual concurrency. Clippy suggests merging, but that
    // would make execution sequential.
    #[allow(clippy::needless_collect)]
    let threads: Vec<_> = (0..THREAD_COUNT)
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
    let ids: Vec<u64> = threads.into_iter().map(|h| h.join().unwrap()).collect();

    // All IDs must be unique.
    let unique: HashSet<u64> = ids.iter().copied().collect();
    assert_eq!(unique.len(), THREAD_COUNT, "duplicate txn IDs detected");

    // All must be committed.
    for id in &ids {
        assert!(mgr.is_committed(*id).unwrap(), "txn {id} not in commit log");
    }

    // Committed set count must match exactly.
    assert_eq!(
        mgr.committed_count().unwrap(),
        THREAD_COUNT,
        "committed set has wrong size"
    );
}

#[test]
fn sixteen_threads_concurrent_begin_rollback() {
    use std::sync::Arc;
    use std::thread;

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

    for h in handles {
        h.join().unwrap();
    }

    // None were committed.
    assert_eq!(mgr.committed_count().unwrap(), 0);
}
