#!/usr/bin/env python3
"""Benchmark: traversal & shortestPath — TesseraGraph vs Memgraph.

Uses the neo4j Python driver (Bolt protocol) against both servers.
Dataset is created via bulk CREATE statements, then queries are timed.
"""

import time
import statistics
import json
import neo4j
import neo4j.io
from neo4j import GraphDatabase

# Monkey-patch: neo4j driver rejects non-Neo4j servers. TesseraGraph
# reports "TesseraGraph/x.y.z" which triggers UnsupportedServerProduct.
neo4j.io.check_supported_server_product = lambda agent: None

# ── Configuration ────────────────────────────────────────────────────────────

TESSERA_URI = "bolt://localhost:7687"
TESSERA_AUTH = ("admin", "Admin@.123")

MEMGRAPH_URI = "bolt://localhost:7688"
MEMGRAPH_AUTH = None  # Memgraph has no auth by default

NODES = 1000
ITERATIONS = 100

# ── Helpers ──────────────────────────────────────────────────────────────────

def connect(uri, auth):
    """Connect with TLS, self-signed certs accepted."""
    return GraphDatabase.driver(uri, auth=auth, encrypted=True, trust=neo4j.TRUST_ALL_CERTIFICATES)


def create_dataset(driver, nodes, label="Node"):
    """Create a chain graph: n0 -> n1 -> ... -> n_{nodes-1}."""
    with driver.session() as s:
        # Batch create nodes (chunks of 100)
        for batch_start in range(0, nodes, 100):
            batch_end = min(batch_start + 100, nodes)
            statements = "; ".join(
                f"CREATE (:{label} {{name: 'n{i}', idx: {i}}})"
                for i in range(batch_start, batch_end)
            )
            # Memgraph supports multi-statement, TesseraGraph may not.
            # Use individual statements for compatibility.
            for i in range(batch_start, batch_end):
                s.run(f"CREATE (:{label} {{name: 'n{i}', idx: {i}}})").consume()

        # Create edges: n0->n1->...->n_{nodes-1}
        for i in range(nodes - 1):
            s.run(
                f"MATCH (a:Node {{idx: {i}}}), (b:Node {{idx: {i+1}}}) "
                "CREATE (a)-[:NEXT]->(b)"
            ).consume()

        # Add shortcuts for interesting pathfinding: every 100 nodes
        for i in range(0, nodes - 100, 100):
            s.run(
                f"MATCH (a:Node {{idx: {i}}}), (b:Node {{idx: {i+100}}}) "
                "CREATE (a)-[:SHORTCUT]->(b)"
            ).consume()


def count_nodes(driver):
    """Verify dataset loaded correctly."""
    with driver.session() as s:
        result = s.run("MATCH (n:Node) RETURN count(n) AS c").single()
        return result["c"]


def benchmark_traversal(driver, iterations, depth=5):
    """BFS traversal from node 0, depth hops."""
    query = (
        f"MATCH (a:Node {{idx: 0}})-[*1..{depth}]->(b:Node) "
        "RETURN count(b) AS cnt"
    )
    latencies = []
    for _ in range(iterations):
        with driver.session() as s:
            t0 = time.perf_counter_ns()
            result = s.run(query).single()
            t1 = time.perf_counter_ns()
            latencies.append(t1 - t0)
    return latencies


def benchmark_shortest_path(driver, iterations):
    """Shortest path from node 0 to node 999."""
    query = (
        "MATCH (a:Node {idx: 0}), (b:Node {idx: 999}) "
        "RETURN shortestPath((a)-[*]->(b)) AS p"
    )
    # TesseraGraph uses GQL syntax for shortestPath
    query_tessera = (
        "MATCH (a:Node {idx: 0}) "
        "MATCH (b:Node {idx: 999}) "
        "RETURN shortestPath(a, b)"
    )
    return query, query_tessera


def run_shortest_path(driver, query, iterations):
    """Run shortest path benchmark."""
    latencies = []
    for _ in range(iterations):
        with driver.session() as s:
            t0 = time.perf_counter_ns()
            try:
                result = s.run(query).single()
            except Exception:
                # Query syntax may differ — skip gracefully
                return None
            t1 = time.perf_counter_ns()
            latencies.append(t1 - t0)
    return latencies


def report(name, target, latencies):
    """Print stats for a benchmark run."""
    if latencies is None:
        print(f"  {name:20s} [{target:12s}]: SKIPPED (query not supported)")
        return None

    latencies_ms = [ns / 1_000_000 for ns in latencies]
    mean_ms = statistics.mean(latencies_ms)
    p50 = sorted(latencies_ms)[len(latencies_ms) // 2]
    p95 = sorted(latencies_ms)[int(len(latencies_ms) * 0.95)]
    p99 = sorted(latencies_ms)[int(len(latencies_ms) * 0.99)]
    qps = 1000.0 / mean_ms if mean_ms > 0 else 0

    print(f"  {name:20s} [{target:12s}]: {qps:8.1f} qps | "
          f"mean={mean_ms:7.1f}ms  p50={p50:7.1f}ms  p95={p95:7.1f}ms  p99={p99:7.1f}ms")

    return {
        "scenario": name,
        "target": target,
        "qps": round(qps, 1),
        "mean_ms": round(mean_ms, 1),
        "p50_ms": round(p50, 1),
        "p95_ms": round(p95, 1),
        "p99_ms": round(p99, 1),
    }


# ── Main ─────────────────────────────────────────────────────────────────────

def main():
    results = []

    for target_name, uri, auth in [
        ("tessera", TESSERA_URI, TESSERA_AUTH),
        ("memgraph", MEMGRAPH_URI, MEMGRAPH_AUTH),
    ]:
        print(f"\n{'='*60}")
        print(f"  Target: {target_name} ({uri})")
        print(f"{'='*60}")

        try:
            driver = connect(uri, auth)
        except Exception as e:
            print(f"  CONNECT FAILED: {e}")
            continue

        # Dataset setup
        print(f"  Creating dataset: {NODES} nodes + chain edges + shortcuts...")
        t0 = time.time()
        try:
            create_dataset(driver, NODES)
        except Exception as e:
            print(f"  DATASET CREATION FAILED: {e}")
            driver.close()
            continue
        elapsed = time.time() - t0
        n = count_nodes(driver)
        print(f"  Dataset ready: {n} nodes ({elapsed:.1f}s)")

        # Traversal
        print(f"  Running traversal ({ITERATIONS} iterations, depth=5)...")
        lats = benchmark_traversal(driver, ITERATIONS, depth=5)
        r = report("traversal", target_name, lats)
        if r:
            results.append(r)

        # Shortest path
        query_cypher, query_gql = benchmark_shortest_path(driver, ITERATIONS)
        query = query_gql if target_name == "tessera" else query_cypher
        print(f"  Running shortestPath ({ITERATIONS} iterations)...")
        lats = run_shortest_path(driver, query, ITERATIONS)
        r = report("shortestPath", target_name, lats)
        if r:
            results.append(r)

        # Cleanup
        with driver.session() as s:
            s.run("MATCH (n) DETACH DELETE n").consume()
        driver.close()

    # Summary
    print(f"\n{'='*60}")
    print("  SUMMARY")
    print(f"{'='*60}")
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
