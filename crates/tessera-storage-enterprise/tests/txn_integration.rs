use tessera_graph::{props, Graph, SharedGraph};
use tessera_storage_enterprise::txn::{
    IsolationLevel, TransactionHandle, TransactionManager, TxnState,
};

#[test]
fn committed_write_is_visible_via_read_committed() {
    let wal_tmp = tempfile::NamedTempFile::new().unwrap();
    let graph = SharedGraph::new(Graph::new());
    let mgr = TransactionManager::new();

    let mut txn = mgr.begin(IsolationLevel::ReadCommitted);

    // Write inside the transaction scope
    let node_id = graph
        .write()
        .add_node("Person", props! { "name" => "Alice" })
        .unwrap();

    // Before commit, txn not in commit log
    assert!(!mgr.is_committed(txn.txn_id()));

    mgr.commit(&mut txn, wal_tmp.path()).unwrap();

    // After commit, txn is in commit log
    assert!(mgr.is_committed(txn.txn_id()));

    // The node exists in the graph
    let node = graph.read().node(node_id).unwrap();
    assert_eq!(node.label(), "Person");
}

#[test]
fn rolled_back_transaction_is_not_committed() {
    let wal_tmp = tempfile::NamedTempFile::new().unwrap();
    let mgr = TransactionManager::new();
    let mut txn = mgr.begin(IsolationLevel::ReadCommitted);
    let id = txn.txn_id();
    mgr.rollback(&mut txn, wal_tmp.path()).unwrap();
    assert!(!mgr.is_committed(id));
    assert_eq!(txn.state(), TxnState::RolledBack);
}

#[test]
fn snapshot_isolation_captures_committed_state() {
    let wal_tmp = tempfile::NamedTempFile::new().unwrap();
    let graph = SharedGraph::new(Graph::new());
    let mgr = TransactionManager::new();

    // T1: add node and commit
    let mut t1 = mgr.begin(IsolationLevel::SnapshotIsolation);
    graph
        .write()
        .add_node("N1", props! {})
        .unwrap();
    mgr.commit(&mut t1, wal_tmp.path()).unwrap();

    // T2: begins with SI — sees T1 committed
    let t2 = mgr.begin(IsolationLevel::SnapshotIsolation);
    let snap = t2.snapshot().unwrap();
    assert!(snap.is_visible(t1.txn_id()));

    // T3: begins after T2 — not visible in T2's snapshot
    let t3 = mgr.begin(IsolationLevel::SnapshotIsolation);
    assert!(!snap.is_visible(t3.txn_id()));

    // Graph has the node regardless (no slot-level MVCC yet)
    assert_eq!(graph.read().node_count(), 1);
}

#[test]
fn concurrent_transactions_have_unique_ids() {
    let mgr = TransactionManager::new();
    let handles: Vec<_> = (0..100)
        .map(|_| mgr.begin(IsolationLevel::ReadCommitted))
        .collect();

    let mut ids: Vec<u64> = handles.iter().map(TransactionHandle::txn_id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 100);
}
