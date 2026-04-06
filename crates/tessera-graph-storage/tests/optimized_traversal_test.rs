//! TDD tests for optimized traversal.

use tessera_graph::gql::{self, GqlQuery, GqlStatement, GqlValue};
use tessera_graph::{Graph, Properties, Property};
use tessera_graph_storage::gql::{execute_query, needs_optimized_execution};

/// Helper: build a Properties map from key-value pairs.
fn props(pairs: &[(&str, &str)]) -> Properties {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), Property::String((*v).to_string())))
        .collect()
}

/// Helper: parse a GQL query string, supporting consecutive MATCH clauses.
fn parse_query(input: &str) -> GqlQuery {
    match gql::parse_statement(input).unwrap() {
        GqlStatement::Query(q) => q,
        _ => panic!("expected a query statement"),
    }
}

/// Helper: extract a single string column from results as a sorted Vec.
fn extract_column_sorted(results: &[std::collections::HashMap<String, GqlValue>], col: &str) -> Vec<String> {
    let mut values: Vec<String> = results
        .iter()
        .filter_map(|row| match row.get(col) {
            Some(GqlValue::Str(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    values.sort();
    values
}

// ── Cycle 1.1: Variable-hop detection ───────────────────────────────────────

#[test]
fn variable_hop_query_needs_optimized() {
    let query = parse_query("MATCH (a)-[*1..3]->(b) RETURN b");
    assert!(
        needs_optimized_execution(&query),
        "variable-hop pattern should trigger optimized execution"
    );
}

// ── Cycle 1.2: Negative and shortestPath cases ─────────────────────────────

#[test]
fn fixed_hop_query_does_not_need_optimized() {
    let query = parse_query("MATCH (a)-[r]->(b) RETURN b");
    assert!(
        !needs_optimized_execution(&query),
        "fixed-hop pattern should NOT trigger optimized execution"
    );
}

#[test]
fn empty_match_does_not_need_optimized() {
    let query = parse_query("MATCH (a) RETURN a");
    assert!(
        !needs_optimized_execution(&query),
        "node-only pattern should NOT trigger optimized execution"
    );
}

#[test]
fn shortest_path_query_needs_optimized() {
    let query = parse_query(
        "MATCH (a:Node {name:'A'}) MATCH (b:Node {name:'E'}) RETURN shortestPath(a, b)",
    );
    assert!(
        needs_optimized_execution(&query),
        "shortestPath function should trigger optimized execution"
    );
}

#[test]
fn variable_hop_with_where_delegates_to_mit_core() {
    let g = build_chain_graph();
    let query = parse_query(
        "MATCH (a:Node)-[*1..3]->(b:Node) WHERE a.name = 'A' RETURN b.name",
    );
    // WHERE triggers delegation to MIT core
    let enterprise_result = execute_query(&g, &query).unwrap();
    let mit_result = tessera_graph::gql::execute(&g, &query).unwrap();
    assert_eq!(enterprise_result, mit_result);

    // Verify non-trivially: query must find B, C, D
    let names = extract_column_sorted(&enterprise_result, "b.name");
    assert_eq!(
        names,
        vec!["B", "C", "D"],
        "WHERE delegation must return concrete results, not trivial empty"
    );
}

#[test]
fn regular_function_does_not_need_optimized() {
    let query = parse_query("MATCH (a:Node) RETURN id(a)");
    assert!(
        !needs_optimized_execution(&query),
        "id() function should NOT trigger optimized execution"
    );
}

// ── Cycle 2.1: Basic chain traversal matches MIT core ───────────────────────

/// Builds a chain graph: A -> B -> C -> D
fn build_chain_graph() -> Graph {
    let mut g = Graph::new();
    let a = g.add_node("Node", props(&[("name", "A")])).unwrap();
    let b = g.add_node("Node", props(&[("name", "B")])).unwrap();
    let c = g.add_node("Node", props(&[("name", "C")])).unwrap();
    let d = g.add_node("Node", props(&[("name", "D")])).unwrap();
    g.add_edge("CONNECTS", a, b, Properties::new()).unwrap();
    g.add_edge("CONNECTS", b, c, Properties::new()).unwrap();
    g.add_edge("CONNECTS", c, d, Properties::new()).unwrap();
    g
}

#[test]
fn variable_hop_results_match_mit_core() {
    let g = build_chain_graph();
    let query = parse_query(
        "MATCH (a:Node {name:'A'})-[*1..3]->(b:Node) RETURN b.name",
    );

    // Enterprise optimized path
    let enterprise_result = execute_query(&g, &query).unwrap();
    let enterprise_names = extract_column_sorted(&enterprise_result, "b.name");

    // MIT core path (reference)
    let mit_result = tessera_graph::gql::execute(&g, &query).unwrap();
    let mit_names = extract_column_sorted(&mit_result, "b.name");

    assert_eq!(
        enterprise_names, mit_names,
        "enterprise results must match MIT core (order-independent)"
    );
    // Should find B, C, D at depths 1, 2, 3
    assert_eq!(enterprise_names, vec!["B", "C", "D"]);
}

#[test]
fn fixed_hop_delegates_to_mit_core() {
    let g = build_chain_graph();
    let query = parse_query("MATCH (a:Node {name:'A'})-[r]->(b:Node) RETURN b.name");

    let enterprise_result = execute_query(&g, &query).unwrap();
    let mit_result = tessera_graph::gql::execute(&g, &query).unwrap();

    assert_eq!(enterprise_result, mit_result);
}

// ── Cycle 2.2: Edge cases ───────────────────────────────────────────────────

#[test]
fn variable_hop_min_zero_includes_start() {
    let g = build_chain_graph();
    let query = parse_query(
        "MATCH (a:Node {name:'A'})-[*0..1]->(b:Node) RETURN b.name",
    );
    let result = execute_query(&g, &query).unwrap();
    let names = extract_column_sorted(&result, "b.name");
    // min=0 includes start node A, plus depth-1 neighbor B
    assert_eq!(names, vec!["A", "B"]);
}

#[test]
fn variable_hop_cycle_no_duplicates() {
    let mut g = Graph::new();
    let a = g.add_node("Node", props(&[("name", "A")])).unwrap();
    let b = g.add_node("Node", props(&[("name", "B")])).unwrap();
    let c = g.add_node("Node", props(&[("name", "C")])).unwrap();
    // Cycle: A -> B -> C -> A
    g.add_edge("CONNECTS", a, b, Properties::new()).unwrap();
    g.add_edge("CONNECTS", b, c, Properties::new()).unwrap();
    g.add_edge("CONNECTS", c, a, Properties::new()).unwrap();

    let query = parse_query(
        "MATCH (a:Node {name:'A'})-[*1..10]->(b:Node) RETURN b.name",
    );
    let result = execute_query(&g, &query).unwrap();
    let names = extract_column_sorted(&result, "b.name");

    // BFS with visited set: B at depth 1, C at depth 2. No duplicates.
    assert_eq!(names, vec!["B", "C"]);
}

#[test]
fn variable_hop_depth_boundary() {
    let g = build_chain_graph(); // A -> B -> C -> D
    let query = parse_query(
        "MATCH (a:Node {name:'A'})-[*1..2]->(b:Node) RETURN b.name",
    );
    let result = execute_query(&g, &query).unwrap();
    let names = extract_column_sorted(&result, "b.name");
    // Depth 1: B, Depth 2: C. D is at depth 3, excluded.
    assert_eq!(names, vec!["B", "C"]);
}

#[test]
fn variable_hop_no_match_label() {
    let g = build_chain_graph();
    let query = parse_query(
        "MATCH (a:Node {name:'A'})-[*1..3]->(b:Other) RETURN b.name",
    );
    let result = execute_query(&g, &query).unwrap();
    assert!(result.is_empty(), "no nodes with label 'Other' should match");
}

// ── Cycle 2.3: Throughput guard ─────────────────────────────────────────────

/// Builds a tree graph with `branching_factor` children per node, `depth` levels.
fn build_tree_graph(branching_factor: usize, depth: usize) -> Graph {
    let mut g = Graph::new();
    let root = g.add_node("Node", props(&[("name", "root")])).unwrap();
    let mut current_level = vec![root];

    for d in 0..depth {
        let mut next_level = Vec::new();
        for &parent in &current_level {
            for c in 0..branching_factor {
                let name = format!("n_{d}_{c}");
                let child = g.add_node("Node", props(&[("name", &name)])).unwrap();
                g.add_edge("CHILD", parent, child, Properties::new()).unwrap();
                next_level.push(child);
            }
        }
        current_level = next_level;
    }
    g
}

#[test]
fn variable_hop_throughput_guard() {
    // Tree: branching=4, depth=4 → 1 + 4 + 16 + 64 + 256 = 341 nodes
    let g = build_tree_graph(4, 4);
    let query = parse_query("MATCH (a:Node {name:'root'})-[*1..4]->(b:Node) RETURN b.name");

    let iterations = 200;
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        let _ = execute_query(&g, &query).unwrap();
    }
    let elapsed = start.elapsed();
    let qps = iterations as f64 / elapsed.as_secs_f64();

    // Debug threshold: >= 200 qps (release target: >= 5000 qps)
    // Debug mode has significant overhead from bounds checking, no inlining, etc.
    assert!(
        qps >= 200.0,
        "throughput {qps:.0} qps is below 200 qps debug threshold (elapsed: {elapsed:.2?})"
    );

    eprintln!("variable-hop throughput: {qps:.0} qps ({iterations} queries in {elapsed:.2?})");

    // Verify correctness: tree branching=4, depth=4 → 4+16+64+256 = 340 reachable nodes
    let verification = execute_query(&g, &query).unwrap();
    assert_eq!(
        verification.len(),
        340,
        "tree branching=4 depth=4: expected 340 reachable nodes in *1..4"
    );
}

// ── Cycle 3.1: Bidirectional BFS shortest path ─────────────────────────────

/// Builds: A -> B -> C -> D -> E, plus shortcut A -> D
fn build_shortest_path_graph() -> Graph {
    let mut g = Graph::new();
    let a = g.add_node("Node", props(&[("name", "A")])).unwrap();
    let b = g.add_node("Node", props(&[("name", "B")])).unwrap();
    let c = g.add_node("Node", props(&[("name", "C")])).unwrap();
    let d = g.add_node("Node", props(&[("name", "D")])).unwrap();
    let e = g.add_node("Node", props(&[("name", "E")])).unwrap();
    g.add_edge("NEXT", a, b, Properties::new()).unwrap();
    g.add_edge("NEXT", b, c, Properties::new()).unwrap();
    g.add_edge("NEXT", c, d, Properties::new()).unwrap();
    g.add_edge("NEXT", d, e, Properties::new()).unwrap();
    g.add_edge("SHORTCUT", a, d, Properties::new()).unwrap(); // shortcut
    g
}

#[test]
fn shortest_path_matches_mit_core() {
    let g = build_shortest_path_graph();
    let query = parse_query(
        "MATCH (a:Node {name:'A'}) MATCH (b:Node {name:'E'}) RETURN shortestPath(a, b)",
    );

    let enterprise_result = execute_query(&g, &query).unwrap();
    let mit_result = tessera_graph::gql::execute(&g, &query).unwrap();

    assert_eq!(enterprise_result.len(), 1, "should produce exactly one row");
    assert_eq!(mit_result.len(), 1, "MIT should produce exactly one row");

    let col = enterprise_result[0].keys().next().unwrap().clone();
    let ent_path = match &enterprise_result[0][&col] {
        GqlValue::List(v) => v.clone(),
        v => panic!("expected List, got {v:?}"),
    };
    let mit_path = match mit_result[0].values().next().unwrap() {
        GqlValue::List(v) => v.clone(),
        v => panic!("expected List, got {v:?}"),
    };

    // Both must find optimal path A->D->E (3 nodes via shortcut)
    assert_eq!(ent_path.len(), 3, "enterprise: A->D->E = 3 nodes");
    assert_eq!(mit_path.len(), ent_path.len(), "MIT and enterprise must agree on length");

    // First node is A, last is E — verify endpoints match between enterprise and MIT
    assert_eq!(ent_path.first(), mit_path.first(), "first node must match");
    assert_eq!(ent_path.last(), mit_path.last(), "last node must match");
}

// ── Cycle 3.2: shortestPath edge cases ──────────────────────────────────────

#[test]
fn shortest_path_unreachable_returns_null() {
    let mut g = Graph::new();
    let a = g.add_node("Node", props(&[("name", "A")])).unwrap();
    let _b = g.add_node("Node", props(&[("name", "B")])).unwrap();
    // No edges — B unreachable from A
    let _ = a; // suppress unused warning

    let query = parse_query(
        "MATCH (a:Node {name:'A'}) MATCH (b:Node {name:'B'}) RETURN shortestPath(a, b)",
    );
    let result = execute_query(&g, &query).unwrap();
    assert_eq!(result.len(), 1);
    let row = &result[0];
    let val = row.values().next().unwrap();
    assert_eq!(*val, GqlValue::Null, "unreachable should return Null");
}

#[test]
fn shortest_path_same_node() {
    let mut g = Graph::new();
    let _a = g.add_node("Node", props(&[("name", "A")])).unwrap();

    let query = parse_query(
        "MATCH (a:Node {name:'A'}) MATCH (b:Node {name:'A'}) RETURN shortestPath(a, b)",
    );
    let result = execute_query(&g, &query).unwrap();
    assert_eq!(result.len(), 1);
    let val = result[0].values().next().unwrap();
    match val {
        GqlValue::List(path) => assert_eq!(path.len(), 1, "same-node path should have length 1"),
        _ => panic!("expected List, got {val:?}"),
    }
}

#[test]
fn shortest_path_direct_edge() {
    let mut g = Graph::new();
    let a = g.add_node("Node", props(&[("name", "A")])).unwrap();
    let b = g.add_node("Node", props(&[("name", "B")])).unwrap();
    g.add_edge("DIRECT", a, b, Properties::new()).unwrap();

    let query = parse_query(
        "MATCH (a:Node {name:'A'}) MATCH (b:Node {name:'B'}) RETURN shortestPath(a, b)",
    );
    let result = execute_query(&g, &query).unwrap();
    assert_eq!(result.len(), 1);
    let val = result[0].values().next().unwrap();
    match val {
        GqlValue::List(path) => assert_eq!(path.len(), 2, "direct edge path: [A, B]"),
        _ => panic!("expected List, got {val:?}"),
    }
}

#[test]
fn shortest_path_picks_minimum() {
    let g = build_shortest_path_graph(); // A->B->C->D->E + A->D
    let query = parse_query(
        "MATCH (a:Node {name:'A'}) MATCH (b:Node {name:'E'}) RETURN shortestPath(a, b)",
    );
    let result = execute_query(&g, &query).unwrap();
    assert_eq!(result.len(), 1);
    let val = result[0].values().next().unwrap();
    match val {
        GqlValue::List(path) => {
            // Shortest: A -> D -> E (length 3 nodes via shortcut)
            assert_eq!(path.len(), 3, "shortest path should be A->D->E (3 nodes)");
        }
        _ => panic!("expected List, got {val:?}"),
    }
}

// ── Cycle 3.3: shortestPath throughput guard ────────────────────────────────

/// Builds a grid graph with n×n nodes connected horizontally and vertically.
fn build_grid_graph(n: usize) -> Graph {
    let mut g = Graph::new();
    let mut ids = Vec::with_capacity(n * n);

    for i in 0..n {
        for j in 0..n {
            let name = format!("n_{i}_{j}");
            let id = g.add_node("Node", props(&[("name", &name)])).unwrap();
            ids.push(id);
        }
    }

    for i in 0..n {
        for j in 0..n {
            let idx = i * n + j;
            if j + 1 < n {
                g.add_edge("H", ids[idx], ids[idx + 1], Properties::new()).unwrap();
            }
            if i + 1 < n {
                g.add_edge("V", ids[idx], ids[idx + n], Properties::new()).unwrap();
            }
        }
    }
    g
}

#[test]
fn shortest_path_throughput_guard() {
    // 30×30 grid = 900 nodes (close to 1000 as per plan)
    let g = build_grid_graph(30);
    let query = parse_query(
        "MATCH (a:Node {name:'n_0_0'}) MATCH (b:Node {name:'n_29_29'}) RETURN shortestPath(a, b)",
    );

    // Compare enterprise vs MIT core throughput
    let iterations = 20;

    let start = std::time::Instant::now();
    for _ in 0..iterations {
        let _ = execute_query(&g, &query).unwrap();
    }
    let enterprise_elapsed = start.elapsed();
    let enterprise_qps = iterations as f64 / enterprise_elapsed.as_secs_f64();

    let start = std::time::Instant::now();
    for _ in 0..iterations {
        let _ = tessera_graph::gql::execute(&g, &query).unwrap();
    }
    let mit_elapsed = start.elapsed();
    let mit_qps = iterations as f64 / mit_elapsed.as_secs_f64();

    eprintln!(
        "shortestPath throughput: enterprise={enterprise_qps:.0} qps, mit={mit_qps:.0} qps, \
         ratio={:.2}x",
        enterprise_qps / mit_qps
    );

    // Debug threshold: enterprise >= mit * 0.9 (at least comparable)
    assert!(
        enterprise_qps >= mit_qps * 0.9,
        "enterprise ({enterprise_qps:.0} qps) should be at least 0.9x of MIT ({mit_qps:.0} qps)"
    );

    // Absolute floor: detect global degradation (not just relative)
    let min_absolute = if cfg!(debug_assertions) { 1.0 } else { 5.0 };
    assert!(
        enterprise_qps >= min_absolute,
        "shortestPath absolute throughput {enterprise_qps:.2} qps below floor {min_absolute:.1} qps"
    );
}

// ── Quality Ciclo 2: Expr::Var bare variable delegation ─────────────────────

#[test]
fn variable_hop_bare_var_return_delegates_to_mit_core() {
    let g = build_chain_graph(); // A -> B -> C -> D
    let query = parse_query("MATCH (a:Node {name:'A'})-[*1..1]->(b:Node) RETURN b");

    let mit_result = tessera_graph::gql::execute(&g, &query).unwrap();
    assert!(!mit_result.is_empty(), "MIT core must find results");

    let enterprise_result = execute_query(&g, &query).unwrap();
    assert_eq!(
        enterprise_result, mit_result,
        "bare Var RETURN must produce same result as MIT core"
    );
}

// ── Quality Ciclo 1: BFS bidireccional correctness ──────────────────────────

/// Two paths of equal length converging at same BFS layer.
/// A->B->C->E and A->D->C->E. shortestPath(A,E) = 4 nodes.
#[test]
fn bidirectional_bfs_two_paths_same_frontier_layer_picks_optimal() {
    let mut g = Graph::new();
    let a = g.add_node("Node", props(&[("name", "A")])).unwrap();
    let b = g.add_node("Node", props(&[("name", "B")])).unwrap();
    let c = g.add_node("Node", props(&[("name", "C")])).unwrap();
    let d = g.add_node("Node", props(&[("name", "D")])).unwrap();
    let e = g.add_node("Node", props(&[("name", "E")])).unwrap();
    g.add_edge("R", a, b, Properties::new()).unwrap();
    g.add_edge("R", b, c, Properties::new()).unwrap();
    g.add_edge("R", a, d, Properties::new()).unwrap();
    g.add_edge("R", d, c, Properties::new()).unwrap();
    g.add_edge("R", c, e, Properties::new()).unwrap();

    let query = parse_query(
        "MATCH (a:Node {name:'A'}) MATCH (b:Node {name:'E'}) RETURN shortestPath(a, b)",
    );
    let result = execute_query(&g, &query).unwrap();
    assert_eq!(result.len(), 1);
    let val = result[0].values().next().unwrap();
    match val {
        GqlValue::List(path) => {
            assert_eq!(
                path.len(),
                4,
                "optimal path A->_->C->E has 4 nodes, got {}",
                path.len()
            );
        }
        _ => panic!("expected List, got {val:?}"),
    }
}

/// Diamond: A->B->D and A->C->D. shortestPath(A,D) = 3 nodes.
#[test]
fn bidirectional_bfs_diamond_returns_length_3() {
    let mut g = Graph::new();
    let a = g.add_node("Node", props(&[("name", "A")])).unwrap();
    let b = g.add_node("Node", props(&[("name", "B")])).unwrap();
    let c = g.add_node("Node", props(&[("name", "C")])).unwrap();
    let d = g.add_node("Node", props(&[("name", "D")])).unwrap();
    g.add_edge("R", a, b, Properties::new()).unwrap();
    g.add_edge("R", a, c, Properties::new()).unwrap();
    g.add_edge("R", b, d, Properties::new()).unwrap();
    g.add_edge("R", c, d, Properties::new()).unwrap();

    let query = parse_query(
        "MATCH (a:Node {name:'A'}) MATCH (b:Node {name:'D'}) RETURN shortestPath(a, b)",
    );
    let result = execute_query(&g, &query).unwrap();
    assert_eq!(result.len(), 1);
    let val = result[0].values().next().unwrap();
    match val {
        GqlValue::List(path) => {
            assert_eq!(path.len(), 3, "diamond: optimal path has 3 nodes (A,_,D)");
        }
        _ => panic!("expected List, got {val:?}"),
    }
}
