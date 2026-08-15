// SPDX-License-Identifier: MIT

use ermya_graph::{Graph, GraphConfig, Properties, WalRecord, WalWriter, props};
use tempfile::TempDir;

const fn wal_config() -> GraphConfig {
    GraphConfig {
        memory_limit_bytes: 64 * 1024,
        create_if_missing: true,
        adj_cache_capacity: 1024,
        wal_enabled: true,
        ..GraphConfig::new()
    }
}

/// Simulates a crash by dropping the graph without flushing.
/// The WAL file should contain un-checkpointed records.
fn crash_graph(g: Graph) {
    // Explicitly drop without calling flush.
    drop(g);
}

#[test]
fn data_survives_simulated_crash_add_node() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    // Session 1: add a node, crash without flush.
    let node_id;
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        node_id = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
        // Crash: no flush
        crash_graph(g);
    }

    // Session 2: reopen — WAL recovery should restore the node.
    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(
            g.node_count(),
            1,
            "node should survive crash via WAL recovery"
        );
        let node = g.node(node_id).unwrap();
        assert_eq!(node.label(), "Person");
    }
}

#[test]
fn data_survives_simulated_crash_add_edge() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    let (nid_a, nid_b);
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        nid_a = g.add_node("A", Properties::new()).unwrap();
        nid_b = g.add_node("B", Properties::new()).unwrap();
        g.add_edge("KNOWS", nid_a, nid_b, Properties::new())
            .unwrap();
        crash_graph(g);
    }

    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
        let out = g.outgoing_edges(nid_a).unwrap();
        assert_eq!(out.len(), 1);
    }
}

#[test]
fn tombstone_survives_crash() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    let nid;
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        nid = g.add_node("X", Properties::new()).unwrap();
        g.flush().unwrap(); // persist the node first
    }

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.node_count(), 1);
        g.remove_node(nid).unwrap();
        crash_graph(g); // crash after remove but before flush
    }

    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(
            g.node_count(),
            0,
            "tombstone should survive crash via WAL recovery"
        );
    }
}

#[test]
fn clean_open_after_flush_skips_wal() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.add_node("A", Properties::new()).unwrap();
        g.flush().unwrap();
    }

    // WAL should be empty after flush — open should succeed normally.
    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.node_count(), 1);
    }
}

#[test]
fn partial_wal_record_at_end_is_ignored() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.add_node("A", Properties::new()).unwrap();
        g.add_node("B", Properties::new()).unwrap();
        crash_graph(g);
    }

    // Truncate the WAL to simulate an incomplete record at the end.
    let wal_path = tmp.path().join("wal.log");
    let data = std::fs::read(&wal_path).unwrap();
    if data.len() > 10 {
        // Remove a few bytes from the end to corrupt the last record.
        std::fs::write(&wal_path, &data[..data.len() - 5]).unwrap();
    }

    // Open should still work — partial record at end is ignored.
    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        // At least some data should be recovered (the first node).
        assert!(
            g.node_count() >= 1,
            "at least first node should survive partial WAL"
        );
    }
}

#[test]
fn multiple_crash_recovery_cycles_preserve_data() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    // Cycle 1: add node, flush.
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.add_node("Cycle1", Properties::new()).unwrap();
        g.flush().unwrap();
    }

    // Cycle 2: add node, crash.
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        g.add_node("Cycle2", Properties::new()).unwrap();
        crash_graph(g);
    }

    // Cycle 3: recover, add node, flush.
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.node_count(), 2, "both nodes should exist after recovery");
        g.add_node("Cycle3", Properties::new()).unwrap();
        g.flush().unwrap();
    }

    // Final verification.
    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(
            g.node_count(),
            3,
            "all three nodes should exist after multi-cycle"
        );
    }
}

#[test]
fn overflow_label_survives_crash() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    // A label long enough to overflow the inline 48-byte slot field.
    let long_label = "A".repeat(100);

    let node_id;
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        node_id = g.add_node(long_label.as_str(), Properties::new()).unwrap();
        crash_graph(g);
    }

    // Reopen — WAL recovery should replay string pages + node slot.
    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.node_count(), 1);
        let node = g.node(node_id).unwrap();
        assert_eq!(node.label(), long_label.as_str());
    }
}

#[test]
fn overflow_properties_survive_crash() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    // Properties large enough to overflow the inline slot.
    let big_value = "X".repeat(200);

    let node_id;
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        node_id = g
            .add_node("Node", props! { "data" => big_value.as_str() })
            .unwrap();
        crash_graph(g);
    }

    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.node_count(), 1);
        let node = g.node(node_id).unwrap();
        assert_eq!(node.label(), "Node");
        assert_eq!(
            node.properties().get("data").unwrap().to_string(),
            big_value
        );
    }
}

#[test]
fn corrupt_middle_wal_record_does_not_lose_subsequent_data() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    // Session 1: add 3 nodes, flush first to establish pages, then add more and crash.
    let (nid_a, nid_c);
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        nid_a = g.add_node("First", Properties::new()).unwrap();
        g.flush().unwrap(); // node A is safely on disk + WAL truncated
        let _nid_b = g.add_node("Second", Properties::new()).unwrap();
        nid_c = g.add_node("Third", Properties::new()).unwrap();
        crash_graph(g); // nodes B and C are only in the WAL
    }

    // Corrupt the FIRST WAL record (node B's WriteNode). Node C's WriteNode follows.
    let wal_path = tmp.path().join("wal.log");
    {
        let mut data = std::fs::read(&wal_path).unwrap();
        assert!(data.len() > 10, "WAL should contain records for B and C");
        // The first record's length is in the first 4 bytes (LE u32).
        let first_record_len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
        let total_first_record = 4 + first_record_len;
        // Zero out the entire first record (not just flip the CRC) because
        // zeroing prevents false-positive decodes: a zeroed length field (0)
        // makes the decoder reject it immediately (minimum record is 13 bytes),
        // whereas a CRC-flip still leaves a plausible header that the decoder
        // must walk through. Unit tests in wal/reader.rs cover the CRC-flip
        // path; this integration test covers the length-zero / truncated path.
        for byte in &mut data[0..total_first_record] {
            *byte = 0;
        }
        std::fs::write(&wal_path, &data).unwrap();
    }

    // Session 2: reopen — forward-scanning should skip corrupt record B
    // and recover node C. Node A was already flushed and is safe.
    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        // Node A was flushed before crash — always survives.
        assert!(g.node_exists(nid_a), "node A was flushed, must survive");
        // Node C should be recovered from the WAL despite B's record being corrupt.
        assert!(
            g.node_exists(nid_c),
            "node C should survive via forward-scanning"
        );
        assert!(g.node_count() >= 2, "at least nodes A and C should exist");
        let node_c = g.node(nid_c).unwrap();
        assert_eq!(node_c.label(), "Third");
    }
}

#[test]
fn tombstone_replay_does_not_shrink_slot_count() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    // Add two nodes (slot 0 and slot 1 on same page), remove the first, crash.
    // WAL replay: WriteNode(slot=0), WriteNode(slot=1), TombstoneNode(id=1).
    // Bug: replay_tombstone used count_live_slots which could shrink slot_count,
    // causing rebuild_indexes to miss the live node at slot 1.
    let (nid_a, nid_b);
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        nid_a = g.add_node("A", Properties::new()).unwrap();
        nid_b = g.add_node("B", Properties::new()).unwrap();
        g.remove_node(nid_a).unwrap();
        crash_graph(g);
    }
    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert!(!g.node_exists(nid_a), "nid_a was removed");
        assert!(g.node_exists(nid_b), "nid_b must survive tombstone replay");
        assert_eq!(g.node(nid_b).unwrap().label(), "B");
    }
}

#[test]
fn checkpoint_mid_wal_does_not_discard_subsequent_records() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    // Session 1: add node A, flush (pages written + WAL truncated), add node B, crash.
    let (nid_a, nid_b);
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        nid_a = g.add_node("Before", Properties::new()).unwrap();
        g.flush().unwrap();
        nid_b = g.add_node("After", Properties::new()).unwrap();
        crash_graph(g);
    }

    // After crash, the WAL contains only node B's records (flush truncated it).
    // Prepend a Checkpoint record to simulate a partial flush: the Checkpoint
    // was written but the WAL was not truncated before the crash.
    let wal_path = tmp.path().join("wal.log");
    {
        let existing_wal = std::fs::read(&wal_path).unwrap();
        // Write a standalone Checkpoint to a temp file to get its bytes.
        let tmp_wal = tmp.path().join("wal_tmp.log");
        {
            let mut w = WalWriter::open(&tmp_wal).unwrap();
            w.append(WalRecord::Checkpoint { lsn: 0 }).unwrap();
            w.sync().unwrap();
        }
        let mut combined = std::fs::read(&tmp_wal).unwrap();
        combined.extend_from_slice(&existing_wal);
        std::fs::write(&wal_path, &combined).unwrap();
        let _ = std::fs::remove_file(&tmp_wal);
    }

    // Session 2: reopen. Bug: `break` on Checkpoint discards node B's records.
    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert!(g.node_exists(nid_a), "nid_a was flushed to disk pages");
        assert!(
            g.node_exists(nid_b),
            "nid_b must survive post-Checkpoint WAL records"
        );
        assert_eq!(g.node(nid_b).unwrap().label(), "After");
    }
}

#[test]
fn wal_recovery_lazy_adj_pages_survive_crash() {
    let tmp = TempDir::new().unwrap();
    let config = wal_config();

    // Session 1: add_node produces no adj pages; add_edge lazily allocates them.
    // Crash without flush — both node slots and adj pages must be in the WAL.
    let (nid_a, nid_b, eid);
    {
        let mut g = Graph::open(tmp.path(), &config).unwrap();
        nid_a = g.add_node("Source", Properties::new()).unwrap();
        nid_b = g.add_node("Target", Properties::new()).unwrap();
        eid = g.add_edge("LINK", nid_a, nid_b, Properties::new()).unwrap();
        crash_graph(g);
    }

    // Session 2: reopen — WAL recovery must restore nodes AND adjacency pages.
    {
        let g = Graph::open(tmp.path(), &config).unwrap();
        assert_eq!(g.node_count(), 2, "both nodes must survive crash via WAL");
        assert_eq!(g.edge_count(), 1, "edge must survive crash via WAL");

        let out = g.outgoing_edges(nid_a).unwrap();
        assert_eq!(
            out.len(),
            1,
            "source node must have 1 outgoing edge after recovery"
        );
        assert_eq!(out[0].id(), eid, "recovered outgoing edge id must match");

        let inc = g.incoming_edges(nid_b).unwrap();
        assert_eq!(
            inc.len(),
            1,
            "target node must have 1 incoming edge after recovery"
        );
        assert_eq!(inc[0].id(), eid, "recovered incoming edge id must match");
    }
}
