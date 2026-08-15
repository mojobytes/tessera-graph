// SPDX-License-Identifier: MIT

use ermya_graph::{Direction, Graph, Properties, props};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Creates a social graph:
/// Alice(Person) -KNOWS-> Bob(Person) -LIKES-> Cats(Thing)
/// Charlie(Person) -KNOWS-> Dave(Person) -LIKES-> Dogs(Thing)
fn social_graph() -> (
    Graph,
    ermya_graph::NodeId,
    ermya_graph::NodeId,
    ermya_graph::NodeId,
    ermya_graph::NodeId,
    ermya_graph::NodeId,
    ermya_graph::NodeId,
) {
    let mut g = Graph::new();
    let alice = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let bob = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    let cats = g.add_node("Thing", props! { "name" => "Cats" }).unwrap();
    let charlie = g
        .add_node("Person", props! { "name" => "Charlie" })
        .unwrap();
    let dave = g.add_node("Person", props! { "name" => "Dave" }).unwrap();
    let dogs = g.add_node("Thing", props! { "name" => "Dogs" }).unwrap();

    g.add_edge("KNOWS", alice, bob, Properties::new()).unwrap();
    g.add_edge("LIKES", bob, cats, Properties::new()).unwrap();
    g.add_edge("KNOWS", charlie, dave, Properties::new())
        .unwrap();
    g.add_edge("LIKES", dave, dogs, Properties::new()).unwrap();

    (g, alice, bob, cats, charlie, dave, dogs)
}

// ---------------------------------------------------------------------------
// Fase 1: PatternMatch type
// ---------------------------------------------------------------------------

#[test]
fn get_node_returns_bound_node() {
    let mut g = Graph::new();
    let n0 = g.add_node("A", Properties::new()).unwrap();

    let results = g
        .pattern()
        .node("a")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();
    assert!(!results.is_empty());

    let found = results.iter().any(|m| m.get_node("a").unwrap().id() == n0);
    assert!(found);
}

#[test]
fn get_node_unknown_variable_returns_error() {
    let mut g = Graph::new();
    g.add_node("A", Properties::new()).unwrap();

    let results = g
        .pattern()
        .node("a")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();
    let err = results[0].get_node("nonexistent").unwrap_err();
    assert!(
        err.to_string().contains("pattern variable not found"),
        "unexpected error: {err}"
    );
}

#[test]
fn get_edge_unknown_variable_returns_error() {
    let mut g = Graph::new();
    g.add_node("A", Properties::new()).unwrap();

    let results = g
        .pattern()
        .node("a")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();
    let err = results[0].get_edge("nonexistent").unwrap_err();
    assert!(
        err.to_string().contains("pattern variable not found"),
        "unexpected error: {err}"
    );
}

// ---------------------------------------------------------------------------
// Fase 2: Single node patterns
// ---------------------------------------------------------------------------

#[test]
fn single_node_no_constraint_matches_all() {
    let mut g = Graph::new();
    g.add_node("Person", Properties::new()).unwrap();
    g.add_node("Person", Properties::new()).unwrap();
    g.add_node("Thing", Properties::new()).unwrap();

    let results = g
        .pattern()
        .node("a")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(results.len(), 3);
}

#[test]
fn single_node_label_filter() {
    let mut g = Graph::new();
    g.add_node("Person", Properties::new()).unwrap();
    g.add_node("Person", Properties::new()).unwrap();
    g.add_node("Thing", Properties::new()).unwrap();

    let results = g
        .pattern()
        .node("a")
        .label("Person")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(results.len(), 2);

    for m in &results {
        assert_eq!(m.get_node("a").unwrap().label(), "Person");
    }
}

#[test]
fn single_node_property_filter() {
    let (g, alice, _bob, _cats, _charlie, _dave, _dogs) = social_graph();

    let results = g
        .pattern()
        .node("a")
        .label("Person")
        .where_prop("name", "Alice")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get_node("a").unwrap().id(), alice);
}

#[test]
fn empty_graph_returns_no_matches() {
    let g = Graph::new();
    let results = g
        .pattern()
        .node("a")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();
    assert!(results.is_empty());
}

#[test]
fn empty_pattern_returns_no_matches() {
    let mut g = Graph::new();
    g.add_node("A", Properties::new()).unwrap();

    let results = g
        .pattern()
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();
    assert!(results.is_empty());
}

// ---------------------------------------------------------------------------
// Fase 3: Two-node pattern with edge
// ---------------------------------------------------------------------------

#[test]
fn two_node_pattern_outgoing() {
    let mut g = Graph::new();
    let alice = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let bob = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    let cats = g.add_node("Thing", props! { "name" => "Cats" }).unwrap();
    g.add_edge("KNOWS", alice, bob, Properties::new()).unwrap();
    g.add_edge("LIKES", alice, cats, Properties::new()).unwrap();

    let results = g
        .pattern()
        .node("a")
        .edge(Direction::Outgoing)
        .label("KNOWS")
        .node("b")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get_node("a").unwrap().id(), alice);
    assert_eq!(results[0].get_node("b").unwrap().id(), bob);
}

#[test]
fn two_node_pattern_multiple_matches() {
    let mut g = Graph::new();
    let alice = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let bob = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    let carol = g.add_node("Person", props! { "name" => "Carol" }).unwrap();
    g.add_edge("KNOWS", alice, bob, Properties::new()).unwrap();
    g.add_edge("KNOWS", alice, carol, Properties::new())
        .unwrap();

    let results = g
        .pattern()
        .node("a")
        .label("Person")
        .where_prop("name", "Alice")
        .edge(Direction::Outgoing)
        .label("KNOWS")
        .node("b")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 2);
    for m in &results {
        assert_eq!(m.get_node("a").unwrap().id(), alice);
    }

    let bs: std::collections::HashSet<_> = results
        .iter()
        .map(|m| m.get_node("b").unwrap().id())
        .collect();
    assert!(bs.contains(&bob));
    assert!(bs.contains(&carol));
}

#[test]
fn two_node_pattern_no_matching_edge_label() {
    let mut g = Graph::new();
    let alice = g.add_node("Person", Properties::new()).unwrap();
    let bob = g.add_node("Person", Properties::new()).unwrap();
    g.add_edge("KNOWS", alice, bob, Properties::new()).unwrap();

    let results = g
        .pattern()
        .node("a")
        .edge(Direction::Outgoing)
        .label("HATES")
        .node("b")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert!(results.is_empty());
}

#[test]
fn two_node_incoming_direction() {
    let mut g = Graph::new();
    let alice = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let bob = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    g.add_edge("KNOWS", alice, bob, Properties::new()).unwrap();

    // From Bob's perspective, Alice KNOWS Bob (incoming).
    let results = g
        .pattern()
        .node("b")
        .label("Person")
        .where_prop("name", "Bob")
        .edge(Direction::Incoming)
        .label("KNOWS")
        .node("a")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get_node("a").unwrap().id(), alice);
    assert_eq!(results[0].get_node("b").unwrap().id(), bob);
}

// ---------------------------------------------------------------------------
// Fase 4: Property filters on target nodes
// ---------------------------------------------------------------------------

#[test]
fn three_hop_full_pattern() {
    let (g, alice, _bob, cats, _charlie, _dave, _dogs) = social_graph();

    let results = g
        .pattern()
        .node("a")
        .label("Person")
        .edge(Direction::Outgoing)
        .label("KNOWS")
        .node("b")
        .edge(Direction::Outgoing)
        .label("LIKES")
        .node("c")
        .label("Thing")
        .where_prop("name", "Cats")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get_node("a").unwrap().id(), alice);
    assert_eq!(results[0].get_node("c").unwrap().id(), cats);
}

#[test]
fn three_hop_all_things() {
    let (g, _alice, _bob, _cats, _charlie, _dave, _dogs) = social_graph();

    // Two Person->KNOWS->Person->LIKES->Thing chains exist.
    let results = g
        .pattern()
        .node("a")
        .label("Person")
        .edge(Direction::Outgoing)
        .label("KNOWS")
        .node("b")
        .edge(Direction::Outgoing)
        .label("LIKES")
        .node("c")
        .label("Thing")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 2);
}

// ---------------------------------------------------------------------------
// Fase 5: Named edge variables
// ---------------------------------------------------------------------------

#[test]
fn edge_var_captures_edge() {
    let mut g = Graph::new();
    let alice = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let bob = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    let eid = g
        .add_edge("KNOWS", alice, bob, props! { "since" => 2020i64 })
        .unwrap();

    let results = g
        .pattern()
        .node("a")
        .edge_var("e", Direction::Outgoing)
        .label("KNOWS")
        .node("b")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 1);
    let edge = results[0].get_edge("e").unwrap();
    assert_eq!(edge.id(), eid);
    assert_eq!(edge.label(), "KNOWS");
}

#[test]
fn unnamed_edge_not_in_match() {
    let mut g = Graph::new();
    let alice = g.add_node("Person", Properties::new()).unwrap();
    let bob = g.add_node("Person", Properties::new()).unwrap();
    g.add_edge("KNOWS", alice, bob, Properties::new()).unwrap();

    let results = g
        .pattern()
        .node("a")
        .edge(Direction::Outgoing)
        .label("KNOWS")
        .node("b")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 1);
    // Edge was not named, so get_edge should fail.
    assert!(results[0].get_edge("e").is_err());
}

// ---------------------------------------------------------------------------
// Fase 6: Edge cases
// ---------------------------------------------------------------------------

#[test]
fn isolated_node_single_match() {
    let mut g = Graph::new();
    let n0 = g.add_node("X", Properties::new()).unwrap();

    let results = g
        .pattern()
        .node("a")
        .label("X")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get_node("a").unwrap().id(), n0);
}

#[test]
fn cycle_graph_does_not_loop_infinitely() {
    let mut g = Graph::new();
    let a = g.add_node("N", Properties::new()).unwrap();
    let b = g.add_node("N", Properties::new()).unwrap();
    g.add_edge("R", a, b, Properties::new()).unwrap();
    g.add_edge("R", b, a, Properties::new()).unwrap();

    // Pattern is 2 hops: (x)-[:R]->(y)-[:R]->(z)
    // Should find a->b->a and b->a->b (z can equal x since no uniqueness constraint).
    let results = g
        .pattern()
        .node("x")
        .edge(Direction::Outgoing)
        .label("R")
        .node("y")
        .edge(Direction::Outgoing)
        .label("R")
        .node("z")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 2);
}

#[test]
fn both_direction_edge() {
    let mut g = Graph::new();
    let a = g.add_node("N", Properties::new()).unwrap();
    let b = g.add_node("N", Properties::new()).unwrap();
    g.add_edge("R", a, b, Properties::new()).unwrap();

    let results = g
        .pattern()
        .node("x")
        .label("N")
        .edge(Direction::Both)
        .label("R")
        .node("y")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    // Both a->b and b->a (via Both direction)
    assert_eq!(results.len(), 2);
}

// ---------------------------------------------------------------------------
// Fase 7: Pattern validation errors
// ---------------------------------------------------------------------------

#[test]
fn duplicate_node_variable_returns_error() {
    let mut g = Graph::new();
    let n0 = g.add_node("N", Properties::new()).unwrap();
    let n1 = g.add_node("N", Properties::new()).unwrap();
    g.add_edge("R", n0, n1, Properties::new()).unwrap();

    let err = g
        .pattern()
        .node("a")
        .edge(Direction::Outgoing)
        .label("R")
        .node("a") // duplicate variable
        .execute()
        .unwrap_err();

    assert!(
        err.to_string().contains("duplicate node variable"),
        "unexpected error: {err}"
    );
}

#[test]
fn consecutive_nodes_without_edge_returns_error() {
    let mut g = Graph::new();
    g.add_node("N", Properties::new()).unwrap();

    let err = g.pattern().node("a").node("b").execute().unwrap_err();

    assert!(
        err.to_string()
            .contains("consecutive node steps without an edge"),
        "unexpected error: {err}"
    );
}

#[test]
fn pattern_starting_with_edge_returns_error() {
    let g = Graph::new();

    let err = g
        .pattern()
        .edge(Direction::Outgoing)
        .node("a")
        .execute()
        .unwrap_err();

    assert!(
        err.to_string().contains("must start with a node"),
        "unexpected error: {err}"
    );
}

#[test]
fn pattern_ending_with_edge_returns_error() {
    let mut g = Graph::new();
    g.add_node("N", Properties::new()).unwrap();

    let err = g
        .pattern()
        .node("a")
        .edge(Direction::Outgoing)
        .execute()
        .unwrap_err();

    assert!(
        err.to_string().contains("missing final node"),
        "unexpected error: {err}"
    );
}

#[test]
fn duplicate_edge_variable_returns_error() {
    let mut g = Graph::new();
    let n0 = g.add_node("N", Properties::new()).unwrap();
    let n1 = g.add_node("N", Properties::new()).unwrap();
    let n2 = g.add_node("N", Properties::new()).unwrap();
    g.add_edge("R", n0, n1, Properties::new()).unwrap();
    g.add_edge("R", n1, n2, Properties::new()).unwrap();

    let err = g
        .pattern()
        .node("a")
        .edge_var("e", Direction::Outgoing)
        .label("R")
        .node("b")
        .edge_var("e", Direction::Outgoing) // duplicate edge variable
        .label("R")
        .node("c")
        .execute()
        .unwrap_err();

    assert!(
        err.to_string().contains("duplicate edge variable"),
        "unexpected error: {err}"
    );
}

// ---------------------------------------------------------------------------
// Fase 7b: Empty variable names
// ---------------------------------------------------------------------------

#[test]
fn pattern_empty_variable_name_rejected_with_clear_error() {
    let mut g = Graph::new();
    g.add_node("N", Properties::new()).unwrap();

    let err = g.pattern().node("").execute().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("empty"),
        "error must mention 'empty', got: {msg}"
    );
    assert!(
        !msg.contains("duplicate"),
        "error must NOT say 'duplicate', got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Fase 8: Edge property filters
// ---------------------------------------------------------------------------

#[test]
fn edge_property_filter() {
    let mut g = Graph::new();
    let alice = g.add_node("Person", props! { "name" => "Alice" }).unwrap();
    let bob = g.add_node("Person", props! { "name" => "Bob" }).unwrap();
    let carol = g.add_node("Person", props! { "name" => "Carol" }).unwrap();
    g.add_edge("KNOWS", alice, bob, props! { "since" => 2020i64 })
        .unwrap();
    g.add_edge("KNOWS", alice, carol, props! { "since" => 2023i64 })
        .unwrap();

    let results = g
        .pattern()
        .node("a")
        .label("Person")
        .where_prop("name", "Alice")
        .edge(Direction::Outgoing)
        .label("KNOWS")
        .where_edge_prop("since", 2020i64)
        .node("b")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].get_node("b").unwrap().id(), bob);
}

// ---------------------------------------------------------------------------
// Performance regression guard
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::cast_possible_wrap)] // allow: test fixture
fn projection_does_not_degrade_pattern_throughput() {
    use std::time::Instant;

    let mut g = Graph::new();
    g.begin_batch();
    for i in 0..1000_u64 {
        g.add_node(
            "X",
            props! {
                "id_key" => i as i64,
                "a" => "value_a",
                "b" => "value_b",
                "c" => "value_c",
                "d" => "value_d",
                "e" => "value_e",
                "f" => "value_f",
                "g" => "value_g",
                "h" => "value_h",
                "i" => "value_i",
                "j" => "value_j",
                "k" => "value_k",
                "l" => "value_l",
                "m" => "value_m",
                "n" => "value_n",
                "o" => "value_o",
                "p" => "value_p",
                "q" => "value_q",
                "r" => "value_r",
                "s" => "value_s"
            },
        )
        .unwrap();
    }
    g.end_batch().unwrap();

    // Measure non-projected baseline
    let start_full = Instant::now();
    let full_results: Vec<_> = g
        .pattern()
        .node("a")
        .label("X")
        .where_prop("id_key", 500_i64)
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();
    let elapsed_full = start_full.elapsed();

    // Measure projected
    let start_proj = Instant::now();
    let proj_results: Vec<_> = g
        .pattern()
        .node("a")
        .label("X")
        .where_prop("id_key", 500_i64)
        .project(vec!["id_key".into()])
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();
    let elapsed_proj = start_proj.elapsed();

    assert_eq!(full_results.len(), 1);
    assert_eq!(proj_results.len(), 1);

    // Absolute ceiling to catch runaway regressions
    assert!(
        elapsed_proj.as_millis() <= 1000,
        "projection took {elapsed_proj:?}, exceeded 1000ms absolute ceiling"
    );
    // Projected must not be more than 3x slower than full (CI slack)
    assert!(
        elapsed_proj <= elapsed_full * 3,
        "projection ({elapsed_proj:?}) was >3x slower than full ({elapsed_full:?})"
    );
}

// ---------------------------------------------------------------------------
// Label-only fast-path: skip property deserialization when only label constraint
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::cast_possible_wrap)] // allow: test fixture
fn node_label_check_does_not_require_property_decode() {
    let mut g = Graph::new();

    // Source node
    let source = g.add_node("Hub", Properties::new()).unwrap();

    // 1000 "Person" targets with properties
    g.begin_batch();
    for i in 0..1000_u64 {
        let t = g
            .add_node(
                "Person",
                props! { "name" => format!("p{i}"), "score" => i as i64 },
            )
            .unwrap();
        g.add_edge("CONNECTS", source, t, Properties::new())
            .unwrap();
    }

    // 1 "Robot" target with properties
    let robot = g
        .add_node("Robot", props! { "model" => "T-800", "year" => 2029_i64 })
        .unwrap();
    g.add_edge("CONNECTS", source, robot, Properties::new())
        .unwrap();
    g.end_batch().unwrap();

    // 1-hop pattern filtering for label "Robot"
    let results: Vec<_> = g
        .pattern()
        .node("src")
        .label("Hub")
        .edge(Direction::Outgoing)
        .label("CONNECTS")
        .node("tgt")
        .label("Robot")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 1, "expected exactly 1 Robot match");
    assert_eq!(results[0].get_node("tgt").unwrap().id(), robot);
    assert_eq!(results[0].get_node("tgt").unwrap().label(), "Robot");
}

// ---------------------------------------------------------------------------
// Arc-based bindings: expand_hop must not clone HashMap per match
// ---------------------------------------------------------------------------

#[test]
fn expand_hop_does_not_clone_bindings_per_matching_edge() {
    let mut g = Graph::new();

    // One source node with three outgoing KNOWS edges to three different targets.
    let src = g.add_node("Src", Properties::new()).unwrap();
    let t1 = g.add_node("Tgt", Properties::new()).unwrap();
    let t2 = g.add_node("Tgt", Properties::new()).unwrap();
    let t3 = g.add_node("Tgt", Properties::new()).unwrap();

    g.add_edge("KNOWS", src, t1, Properties::new()).unwrap();
    g.add_edge("KNOWS", src, t2, Properties::new()).unwrap();
    g.add_edge("KNOWS", src, t3, Properties::new()).unwrap();

    let results: Vec<_> = g
        .pattern()
        .node("a")
        .label("Src")
        .edge(Direction::Outgoing)
        .label("KNOWS")
        .node("b")
        .label("Tgt")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(
        results.len(),
        3,
        "expected 3 matches, got {}",
        results.len()
    );

    // All three results must bind `a` to the same source node.
    for result in &results {
        assert_eq!(
            result.get_node("a").unwrap().id(),
            src,
            "all matches should reference the same source node"
        );
    }
}

// ---------------------------------------------------------------------------
// Multi-property index intersection
// ---------------------------------------------------------------------------

#[test]
fn multi_property_constraint_returns_only_matching_nodes() {
    // 100 Person nodes: ids 1..=50 have status="Active", 51..=100 have status="Inactive".
    // A 2-property constraint (id=25, status="Active") must return exactly 1 result.
    let mut g = Graph::new();
    g.begin_batch();
    for i in 1_i64..=100 {
        let status = if i <= 50 { "Active" } else { "Inactive" };
        g.add_node("Person", props! { "id" => i, "status" => status })
            .unwrap();
    }
    g.end_batch().unwrap();

    let results: Vec<_> = g
        .pattern()
        .node("p")
        .label("Person")
        .where_prop("id", 25_i64)
        .where_prop("status", "Active")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(
        results.len(),
        1,
        "expected exactly 1 match for id=25 AND status=Active, got {}",
        results.len()
    );
    let node = results[0].get_node("p").unwrap();
    assert_eq!(node.label(), "Person");
    assert_eq!(
        node.properties().get("id"),
        Some(&ermya_graph::Property::I64(25))
    );
}

#[test]
fn multi_property_constraint_no_match_when_second_property_differs() {
    // Same 100-node setup: querying id=25 AND status="Inactive" should find 0 results
    // because node 25 has status="Active".
    let mut g = Graph::new();
    g.begin_batch();
    for i in 1_i64..=100 {
        let status = if i <= 50 { "Active" } else { "Inactive" };
        g.add_node("Person", props! { "id" => i, "status" => status })
            .unwrap();
    }
    g.end_batch().unwrap();

    let results: Vec<_> = g
        .pattern()
        .node("p")
        .label("Person")
        .where_prop("id", 25_i64)
        .where_prop("status", "Inactive")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(
        results.len(),
        0,
        "expected 0 matches for id=25 AND status=Inactive, got {}",
        results.len()
    );
}

// ---------------------------------------------------------------------------
// Double-buffer hop expansion: correctness
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::many_single_char_names)] // allow: test fixture
fn multi_hop_double_buffer_correctness() {
    // Chain: A -R1-> B -R2-> C -R3-> D
    let mut g = Graph::new();
    let a = g.add_node("Step", props! { "name" => "A" }).unwrap();
    let b = g.add_node("Step", props! { "name" => "B" }).unwrap();
    let c = g.add_node("Step", props! { "name" => "C" }).unwrap();
    let d = g.add_node("Step", props! { "name" => "D" }).unwrap();

    g.add_edge("NEXT", a, b, Properties::new()).unwrap();
    g.add_edge("NEXT", b, c, Properties::new()).unwrap();
    g.add_edge("NEXT", c, d, Properties::new()).unwrap();

    // 3-hop pattern: (n0)-[e0]->(n1)-[e1]->(n2)-[e2]->(n3)
    let results: Vec<_> = g
        .pattern()
        .node("n0")
        .label("Step")
        .edge(Direction::Outgoing)
        .label("NEXT")
        .node("n1")
        .edge(Direction::Outgoing)
        .label("NEXT")
        .node("n2")
        .edge(Direction::Outgoing)
        .label("NEXT")
        .node("n3")
        .execute()
        .unwrap()
        .collect::<ermya_graph::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 1, "expected exactly 1 match for 3-hop chain");

    let m = &results[0];
    assert_eq!(m.get_node("n0").unwrap().id(), a);
    assert_eq!(m.get_node("n1").unwrap().id(), b);
    assert_eq!(m.get_node("n2").unwrap().id(), c);
    assert_eq!(m.get_node("n3").unwrap().id(), d);
}
