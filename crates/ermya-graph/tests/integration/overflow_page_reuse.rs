// SPDX-License-Identifier: MIT

//! Property-overflow pages must be reusable.
//!
//! Three measured defects motivate this file. An entity whose encoded
//! properties exceed the inline cap (38 bytes for a node) gets a whole
//! 4096-byte page to itself; updating it wrote a fresh chain and abandoned the
//! old one; and deleting it freed nothing at all. A benchmark over 2 000 nodes
//! updated 20 times each ended holding 164 MB of overflow pages for ~78 KB of
//! live data, and grew without bound from there.
//!
//! These tests are written against the observable consequence — the file stops
//! growing — rather than against the mechanism, so a different implementation
//! of reuse would still satisfy them.

use tempfile::TempDir;
use ermya_graph::{Graph, GraphConfig, Property, props};

const fn test_config() -> GraphConfig {
    GraphConfig {
        memory_limit_bytes: 1024 * 1024,
        create_if_missing: true,
        adj_cache_capacity: 1024,
        wal_enabled: true,
        ..GraphConfig::new()
    }
}

/// A value long enough to push a single-property node past the inline cap.
///
/// Measured, not guessed: a `name` property tips into overflow at 29
/// characters. 64 keeps it clearly on the overflow side without spanning more
/// than one page.
fn overflowing_value(seed: &str) -> String {
    format!("{seed}{}", "x".repeat(64))
}

#[test]
fn updating_an_overflowed_node_does_not_grow_the_file_without_bound() {
    let tmp = TempDir::new().unwrap();
    let mut g = Graph::open(tmp.path(), &test_config()).unwrap();

    let id = g
        .add_node("P", props! { "name" => overflowing_value("a").as_str() })
        .unwrap();
    let after_insert = g.overflow_page_count();
    assert!(
        after_insert > 0,
        "test is not exercising the overflow path at all"
    );

    // Same node, same property length, 50 times over. Live data is constant by
    // construction, so any growth here is pure waste.
    for i in 0..50 {
        let mut node = g.node(id).unwrap();
        node.properties_mut().insert(
            "name".into(),
            Property::String(overflowing_value(&format!("{i}"))),
        );
        g.update_node(id, &node).unwrap();
    }

    // Since overflowed properties are packed several entities to a page, a
    // rewrite reclaims the entity's own previous bytes within the page it
    // already occupies — no second page is needed at all.
    //
    // Before any of this work the same loop reached 51 pages and kept climbing.
    assert_eq!(
        g.overflow_page_count(),
        after_insert,
        "50 updates of one node must not grow the file at all"
    );

    // Reuse must not corrupt what it reuses.
    let final_value = g.node(id).unwrap();
    match final_value.properties().get("name") {
        Some(Property::String(s)) => {
            assert_eq!(*s, overflowing_value("49"), "last write must be readable");
        }
        other => panic!("expected the updated string, got {other:?}"),
    }
}

#[test]
fn overflow_growth_does_not_scale_with_the_number_of_updates() {
    // The sharpest statement of the defect: cost per update must be zero, not
    // merely small. Ten updates and a hundred updates have to leave the file
    // exactly the same size — before the fix the second run held 90 more pages
    // than the first.
    fn pages_after(updates: usize) -> u32 {
        let tmp = TempDir::new().unwrap();
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        let id = g
            .add_node("P", props! { "name" => overflowing_value("a").as_str() })
            .unwrap();
        for i in 0..updates {
            let mut node = g.node(id).unwrap();
            node.properties_mut().insert(
                "name".into(),
                Property::String(overflowing_value(&format!("{i}"))),
            );
            g.update_node(id, &node).unwrap();
        }
        g.overflow_page_count()
    }

    let few = pages_after(10);
    let many = pages_after(100);
    assert_eq!(
        few, many,
        "overflow footprint must not depend on how many times an entity was \
         rewritten (10 updates: {few} pages, 100 updates: {many} pages)"
    );
}

#[test]
fn many_overflowed_nodes_share_pages_instead_of_taking_one_each() {
    // The origin waste: a node whose properties exceed the inline cap used to
    // take a whole 4096-byte page, so 200 nodes cost 200 pages regardless of
    // how little each one stored.
    let tmp = TempDir::new().unwrap();
    let mut g = Graph::open(tmp.path(), &test_config()).unwrap();

    let n = 200_u32;
    for i in 0..n {
        g.add_node(
            "P",
            props! { "name" => overflowing_value(&format!("n{i}")).as_str() },
        )
        .unwrap();
    }

    let pages = g.overflow_page_count();
    assert!(
        pages < n / 4,
        "200 nodes of ~70 bytes must share pages, not take one each \
         (got {pages} pages for {n} nodes)"
    );
}

#[test]
fn packing_keeps_every_node_readable() {
    // Packing puts many entities' bytes in one page behind a directory; a
    // mistake there returns another entity's properties rather than an error.
    let tmp = TempDir::new().unwrap();
    let mut g = Graph::open(tmp.path(), &test_config()).unwrap();

    let mut ids = Vec::new();
    for i in 0..100_u32 {
        let v = overflowing_value(&format!("node{i}"));
        ids.push((g.add_node("P", props! { "name" => v.as_str() }).unwrap(), v));
    }

    for (id, expected) in &ids {
        match g.node(*id).unwrap().properties().get("name") {
            Some(Property::String(s)) => assert_eq!(s, expected),
            other => panic!("node {id:?} lost its properties: {other:?}"),
        }
    }
}

#[test]
fn packed_properties_survive_a_reopen() {
    let tmp = TempDir::new().unwrap();
    let mut ids = Vec::new();

    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        for i in 0..50_u32 {
            let v = overflowing_value(&format!("keep{i}"));
            ids.push((g.add_node("P", props! { "name" => v.as_str() }).unwrap(), v));
        }
        g.flush().unwrap();
    }

    let g = Graph::open(tmp.path(), &test_config()).unwrap();
    for (id, expected) in &ids {
        match g.node(*id).unwrap().properties().get("name") {
            Some(Property::String(s)) => assert_eq!(s, expected),
            other => panic!("node {id:?} lost its properties across reopen: {other:?}"),
        }
    }
}

#[test]
fn deleting_an_overflowed_node_returns_its_pages() {
    let tmp = TempDir::new().unwrap();
    let mut g = Graph::open(tmp.path(), &test_config()).unwrap();

    let id = g
        .add_node("P", props! { "name" => overflowing_value("a").as_str() })
        .unwrap();
    let after_first = g.overflow_page_count();

    g.remove_node(id).unwrap();

    // A fresh node of the same shape must land on the freed page rather than
    // extending the file.
    let reused = g
        .add_node("P", props! { "name" => overflowing_value("b").as_str() })
        .unwrap();
    assert_eq!(
        g.overflow_page_count(),
        after_first,
        "the deleted node's page must be handed back out, not left orphaned"
    );

    // And the recycled page must hold the new node's data, not the old one's.
    match g.node(reused).unwrap().properties().get("name") {
        Some(Property::String(s)) => assert_eq!(*s, overflowing_value("b")),
        other => panic!("expected the new node's value, got {other:?}"),
    }
}

#[test]
fn freed_pages_survive_a_reopen() {
    // Without persistence the free list is rebuilt as empty on restart, so
    // every page freed before the restart leaks permanently — the same bug
    // this change exists to fix, just showing up one boot later.
    let tmp = TempDir::new().unwrap();

    let after_first = {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        let id = g
            .add_node("P", props! { "name" => overflowing_value("a").as_str() })
            .unwrap();
        let count = g.overflow_page_count();
        g.remove_node(id).unwrap();
        g.flush().unwrap();
        count
    };

    let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
    let _ = g
        .add_node("P", props! { "name" => overflowing_value("c").as_str() })
        .unwrap();

    assert_eq!(
        g.overflow_page_count(),
        after_first,
        "a page freed before the restart must still be reusable after it"
    );
}

#[test]
fn reuse_never_hands_out_a_page_that_is_still_live() {
    // The failure mode this guards against is the dangerous one: handing a
    // live page to a second entity silently overwrites the first entity's
    // properties, and nothing reports an error.
    let tmp = TempDir::new().unwrap();
    let mut g = Graph::open(tmp.path(), &test_config()).unwrap();

    let keep = g
        .add_node(
            "Keep",
            props! { "name" => overflowing_value("keep").as_str() },
        )
        .unwrap();
    let drop_me = g
        .add_node(
            "Drop",
            props! { "name" => overflowing_value("drop").as_str() },
        )
        .unwrap();

    g.remove_node(drop_me).unwrap();

    // Several new overflowing nodes, any of which could wrongly land on the
    // surviving node's page.
    for i in 0..10 {
        g.add_node(
            "New",
            props! { "name" => overflowing_value(&format!("new{i}")).as_str() },
        )
        .unwrap();
    }

    match g.node(keep).unwrap().properties().get("name") {
        Some(Property::String(s)) => assert_eq!(
            *s,
            overflowing_value("keep"),
            "a live node's properties were overwritten by page reuse"
        ),
        other => panic!("the surviving node lost its properties: {other:?}"),
    }
}

#[test]
fn shrinking_a_node_below_the_inline_cap_releases_its_chain() {
    // An entity that stops overflowing has no further use for its chain. If
    // this leaked, the "update" fix would only cover same-size rewrites.
    let tmp = TempDir::new().unwrap();
    let mut g = Graph::open(tmp.path(), &test_config()).unwrap();

    let id = g
        .add_node("P", props! { "name" => overflowing_value("a").as_str() })
        .unwrap();
    let with_overflow = g.overflow_page_count();

    let mut node = g.node(id).unwrap();
    node.properties_mut()
        .insert("name".into(), Property::String("tiny".into()));
    g.update_node(id, &node).unwrap();

    // Nothing overflows any more, so the chain must now be on the free list.
    assert_eq!(
        g.reusable_overflow_page_count(),
        with_overflow,
        "shrinking below the inline cap must release the chain"
    );

    // And a new overflowing node must land on it rather than extend the file.
    g.add_node("Q", props! { "name" => overflowing_value("q").as_str() })
        .unwrap();

    assert_eq!(
        g.overflow_page_count(),
        with_overflow,
        "the released page must be reused instead of growing the file"
    );
}

// ── Under explicit transactions ─────────────────────────────────────────
//
// A commit must NOT release anything: a reader whose snapshot predates it can
// still resolve the previous version. The release belongs to the vacuum, which
// only runs once no live snapshot can need that version. These pin the vacuum
// actually doing it — without them the transactional path silently keeps the
// very defect the auto-commit path had.

#[test]
fn vacuuming_a_transactional_delete_releases_its_pages() {
    let mut g = Graph::new();
    g.enable_mvcc();

    let insert = g.begin_txn().unwrap();
    let mut ids = Vec::new();
    for i in 0..60 {
        ids.push(
            g.add_node_in_txn(
                insert,
                "P",
                props! { "name" => overflowing_value(&format!("n{i}")).as_str() },
            )
            .unwrap(),
        );
    }
    g.commit_txn(insert).unwrap();
    g.vacuum_once().unwrap();
    let after_insert = g.overflow_page_count();
    assert!(after_insert > 0, "test is not exercising the overflow path");

    let delete = g.begin_txn().unwrap();
    for id in &ids {
        g.remove_node_in_txn(delete, *id).unwrap();
    }
    g.commit_txn(delete).unwrap();
    g.vacuum_once().unwrap();

    // Before the fix this was flatly zero: the vacuum released nothing, so a
    // transactional delete leaked its pages permanently.
    //
    // Not compared for exact equality with `after_insert` because the free list
    // spends one page on the directory that records the rest — that page is not
    // "reusable", it is in use keeping the accounting. What matters is that the
    // pages come back at all.
    let reusable = g.reusable_overflow_page_count();
    assert!(
        reusable > 0,
        "the vacuum must release the deleted nodes' pages (got {reusable} reusable \
         of {after_insert} held)"
    );

    // And they must actually be handed back out: new overflowing nodes have to
    // land on them rather than extend the file.
    let before_refill = g.overflow_page_count();
    let refill = g.begin_txn().unwrap();
    for i in 0..40 {
        g.add_node_in_txn(
            refill,
            "Q",
            props! { "name" => overflowing_value(&format!("r{i}")).as_str() },
        )
        .unwrap();
    }
    g.commit_txn(refill).unwrap();
    assert_eq!(
        g.overflow_page_count(),
        before_refill,
        "40 new overflowing nodes must reuse the released pages, not grow the file"
    );
}

#[test]
fn a_commit_does_not_release_pages_a_live_reader_may_still_need() {
    // The other half of the contract: releasing at commit time would pull
    // pages out from under a transaction that started earlier.
    let mut g = Graph::new();
    g.enable_mvcc();

    let setup = g.begin_txn().unwrap();
    let id = g
        .add_node_in_txn(
            setup,
            "P",
            props! { "name" => overflowing_value("a").as_str() },
        )
        .unwrap();
    g.commit_txn(setup).unwrap();
    g.vacuum_once().unwrap();

    // A reader that must keep seeing the original value.
    let reader = g.begin_txn().unwrap();

    let writer = g.begin_txn().unwrap();
    let mut node = g.node_in_txn(writer, id).unwrap();
    node.properties_mut()
        .insert("name".into(), Property::String(overflowing_value("b")));
    g.update_node_in_txn(writer, id, &node).unwrap();
    g.commit_txn(writer).unwrap();
    // Vacuum cannot reclaim past the reader's snapshot.
    g.vacuum_once().unwrap();

    match g.node_in_txn(reader, id).unwrap().properties().get("name") {
        Some(Property::String(s)) => assert_eq!(
            *s,
            overflowing_value("a"),
            "the older snapshot must still resolve its own version"
        ),
        other => panic!("the reader lost the node's properties: {other:?}"),
    }
}

#[test]
fn freed_pages_survive_a_crash_before_flush() {
    // Which pages are free lives in the metadata page, and that page is only
    // written by `flush`. Without a journal record carrying that state, a
    // release not followed by a flush did not survive a crash: the page came
    // back neither live nor reusable — leaked, i.e. the very defect the free
    // list exists to remove, reappearing on the recovery path.
    let tmp = TempDir::new().unwrap();
    let v = overflowing_value("a");

    {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        let id = g.add_node("P", props! { "name" => v.as_str() }).unwrap();
        g.flush().unwrap(); // the node is durable
        g.remove_node(id).unwrap(); // freed, but deliberately NOT flushed
        assert_eq!(
            g.reusable_overflow_page_count(),
            1,
            "before the crash the page is recorded as reusable"
        );
    }

    // Reopening replays the journal — this is the crash-recovery path.
    let mut g = Graph::open(tmp.path(), &test_config()).unwrap();

    assert_eq!(g.node_count(), 0, "the delete itself must survive");
    assert_eq!(
        g.reusable_overflow_page_count(),
        1,
        "the page freed before the crash must still be reusable after recovery"
    );

    // And it must actually be handed back out rather than merely counted.
    let before = g.overflow_page_count();
    g.add_node("P", props! { "name" => overflowing_value("b").as_str() })
        .unwrap();
    assert_eq!(
        g.overflow_page_count(),
        before,
        "the recovered free page must be reused instead of growing the file"
    );
}

#[test]
fn a_crash_mid_sequence_recovers_the_latest_free_list_state() {
    // Several releases and reuses in a row, then a crash. Each journal record
    // carries the whole state rather than a delta, so replaying them in order
    // must land on the state as of the crash — not an intermediate one, and
    // never a half-applied list.
    let tmp = TempDir::new().unwrap();

    let (expected_pages, expected_reusable) = {
        let mut g = Graph::open(tmp.path(), &test_config()).unwrap();
        let mut ids = Vec::new();
        for i in 0..80 {
            ids.push(
                g.add_node(
                    "P",
                    props! { "name" => overflowing_value(&format!("n{i}")).as_str() },
                )
                .unwrap(),
            );
        }
        g.flush().unwrap();

        // Churn: delete half, add some back, delete more. No flush after this.
        for id in ids.iter().step_by(2) {
            g.remove_node(*id).unwrap();
        }
        for i in 0..20 {
            g.add_node(
                "P",
                props! { "name" => overflowing_value(&format!("m{i}")).as_str() },
            )
            .unwrap();
        }
        for id in ids.iter().skip(1).step_by(4) {
            g.remove_node(*id).unwrap();
        }
        (g.overflow_page_count(), g.reusable_overflow_page_count())
    };

    let g = Graph::open(tmp.path(), &test_config()).unwrap();
    assert_eq!(
        (g.overflow_page_count(), g.reusable_overflow_page_count()),
        (expected_pages, expected_reusable),
        "recovery must reproduce the free-list state as of the crash"
    );
}
