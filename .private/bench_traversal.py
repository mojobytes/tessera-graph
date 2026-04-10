#!/usr/bin/env python3.12
"""Benchmark: traversal & shortestPath — TesseraGraph vs Memgraph.

Reads from a **precargado** production dataset (~200k nodes, ~200k edges).
Does NOT create data — load nodes.cypher + edges.cypher first.

Usage:
  # 1. Load dataset (one-time):
  docker cp nodes.cypher tessera-graphdb:/tmp/ && docker cp edges.cypher tessera-graphdb:/tmp/
  docker compose exec graphdb tessera-graph-cli -u admin --password 'Admin@.123' \
      --tls-skip-verify exec -l cypher /tmp/nodes.cypher
  docker compose exec graphdb tessera-graph-cli -u admin --password 'Admin@.123' \
      --tls-skip-verify exec -l cypher /tmp/edges.cypher

  cat nodes.cypher edges.cypher | docker compose exec -T memgraph \
      mgconsole --host 127.0.0.1 --port 7687 --use-ssl=true

  # 2. Run benchmark:
  python3.12 .private/bench_traversal.py
"""

import sys
import time
import statistics
import json
import importlib

# ── Monkey-patch: neo4j driver rejects non-Neo4j servers ───────────────────
_noop = lambda agent: None
for _mod_path in (
    "neo4j.io",
    "neo4j._sync.io",
    "neo4j._sync.io._bolt3",
    "neo4j._sync.io._bolt4",
    "neo4j._sync.io._bolt5",
    "neo4j._sync.io._common",
    "neo4j._async.io",
    "neo4j._async.io._bolt3",
    "neo4j._async.io._bolt4",
    "neo4j._async.io._bolt5",
    "neo4j._async.io._common",
):
    try:
        _m = importlib.import_module(_mod_path)
        if hasattr(_m, "check_supported_server_product"):
            _m.check_supported_server_product = _noop
    except ImportError:
        pass

from neo4j import GraphDatabase

# ── Configuration ────────────────────────────────────────────────────────────

TESSERA_URI = "bolt+ssc://localhost:7687"
TESSERA_AUTH = ("admin", "Admin@.123")

MEMGRAPH_URI = "bolt+ssc://localhost:7688"
MEMGRAPH_AUTH = None

ITERATIONS = 100
WARMUP = 5

# ── Helpers ──────────────────────────────────────────────────────────────────

def connect(uri, auth):
    return GraphDatabase.driver(uri, auth=auth)


def verify_dataset(driver, target_name):
    """Verify the production dataset is loaded. Returns (node_count, edge_count)."""
    with driver.session() as s:
        nc = s.run("MATCH (n) RETURN count(n) AS c").single()["c"]
        ec = s.run("MATCH ()-[r]->() RETURN count(r) AS c").single()["c"]
    return nc, ec


def pick_start_nodes(driver):
    """Pick representative start nodes for traversal benchmarks."""
    nodes = {}
    with driver.session() as s:
        # A Client node (top of hierarchy — deep traversal)
        r = s.run("MATCH (c:Client) RETURN c.id AS id LIMIT 1").single()
        if r:
            nodes["client"] = r["id"]

        # A Plant node (mid-level — medium traversal)
        r = s.run("MATCH (p:Plant) RETURN p.id AS id LIMIT 1").single()
        if r:
            nodes["plant"] = r["id"]

        # A Part node (low-level — shallow traversal)
        r = s.run("MATCH (p:Part) RETURN p.id AS id LIMIT 1").single()
        if r:
            nodes["part"] = r["id"]

        # Two distant nodes for shortest path
        r = s.run(
            "MATCH (a:Client) WITH a LIMIT 1 "
            "MATCH (b:Component) RETURN a.id AS aid, b.id AS bid LIMIT 1"
        ).single()
        if r:
            nodes["path_from"] = r["aid"]
            nodes["path_to"] = r["bid"]
    return nodes


def benchmark_query(driver, query, iterations, warmup=WARMUP):
    """Run a query `iterations` times and return latency samples (ns)."""
    # Warmup (discard)
    for _ in range(warmup):
        with driver.session() as s:
            list(s.run(query))

    latencies = []
    for _ in range(iterations):
        with driver.session() as s:
            t0 = time.perf_counter_ns()
            list(s.run(query))
            t1 = time.perf_counter_ns()
            latencies.append(t1 - t0)
    return latencies


def report(name, target, latencies):
    """Print stats and return dict."""
    if latencies is None:
        print(f"  {name:30s} [{target:12s}]: SKIPPED")
        return None

    latencies_ms = [ns / 1_000_000 for ns in latencies]
    mean_ms = statistics.mean(latencies_ms)
    s = sorted(latencies_ms)
    p50 = s[len(s) // 2]
    p95 = s[int(len(s) * 0.95)]
    p99 = s[int(len(s) * 0.99)]
    qps = 1000.0 / mean_ms if mean_ms > 0 else 0

    print(f"  {name:30s} [{target:12s}]: {qps:8.1f} qps | "
          f"mean={mean_ms:7.2f}ms  p50={p50:7.2f}ms  p95={p95:7.2f}ms  p99={p99:7.2f}ms")

    return {
        "scenario": name,
        "target": target,
        "qps": round(qps, 1),
        "mean_ms": round(mean_ms, 2),
        "p50_ms": round(p50, 2),
        "p95_ms": round(p95, 2),
        "p99_ms": round(p99, 2),
    }


# ── Benchmark Scenarios ─────────────────────────────────────────────────────

def run_benchmarks(driver, target_name, start_nodes):
    results = []

    # 1. Point lookup by indexed property
    nid = start_nodes.get("plant")
    if nid:
        q = f"MATCH (n:Plant {{id: '{nid}'}}) RETURN n"
        lats = benchmark_query(driver, q, ITERATIONS)
        r = report("point-lookup (Plant by id)", target_name, lats)
        if r: results.append(r)

    # 2. Shallow traversal: Plant → children (depth 1)
    if nid:
        q = f"MATCH (p:Plant {{id: '{nid}'}})-[:CONTAINS]->(c) RETURN count(c) AS cnt"
        lats = benchmark_query(driver, q, ITERATIONS)
        r = report("shallow-traversal (depth=1)", target_name, lats)
        if r: results.append(r)

    # 3. Medium traversal: Plant → all descendants (depth 3)
    if nid:
        q = f"MATCH (p:Plant {{id: '{nid}'}})-[:CONTAINS*1..3]->(c) RETURN count(c) AS cnt"
        lats = benchmark_query(driver, q, ITERATIONS)
        r = report("medium-traversal (depth=3)", target_name, lats)
        if r: results.append(r)

    # 4. Deep traversal: Client → all descendants (full depth)
    cid = start_nodes.get("client")
    if cid:
        q = f"MATCH (c:Client {{id: '{cid}'}})-[:CONTAINS*1..10]->(n) RETURN count(n) AS cnt"
        lats = benchmark_query(driver, q, ITERATIONS)
        r = report("deep-traversal (depth=10)", target_name, lats)
        if r: results.append(r)

    # 5. Aggregation: count nodes by label
    q = "MATCH (n:Component) RETURN count(n) AS cnt"
    lats = benchmark_query(driver, q, ITERATIONS)
    r = report("count-by-label (Component)", target_name, lats)
    if r: results.append(r)

    # 6. Shortest path: Client → Component
    pfrom = start_nodes.get("path_from")
    pto = start_nodes.get("path_to")
    if pfrom and pto:
        if target_name == "tessera":
            q = (f"MATCH (a:Client {{id: '{pfrom}'}}) "
                 f"MATCH (b:Component {{id: '{pto}'}}) "
                 f"RETURN shortestPath(a, b)")
        else:
            q = (f"MATCH (a:Client {{id: '{pfrom}'}}), (b:Component {{id: '{pto}'}}) "
                 f"RETURN shortestPath((a)-[*]->(b)) AS p")
        lats = benchmark_query(driver, q, ITERATIONS)
        r = report("shortest-path (Client→Component)", target_name, lats)
        if r: results.append(r)

    # 7. Pattern match: find all Parts under a specific Plant
    if nid:
        q = f"MATCH (p:Plant {{id: '{nid}'}})-[:CONTAINS*1..5]->(part:Part) RETURN count(part) AS cnt"
        lats = benchmark_query(driver, q, ITERATIONS)
        r = report("pattern-match (Plant→Parts)", target_name, lats)
        if r: results.append(r)

    return results


# ── Main ─────────────────────────────────────────────────────────────────────

def main():
    all_results = []

    for target_name, uri, auth in [
        ("tessera", TESSERA_URI, TESSERA_AUTH),
        ("memgraph", MEMGRAPH_URI, MEMGRAPH_AUTH),
    ]:
        print(f"\n{'='*72}")
        print(f"  Target: {target_name} ({uri})")
        print(f"{'='*72}")
        sys.stdout.flush()

        try:
            driver = connect(uri, auth)
        except Exception as e:
            print(f"  CONNECT FAILED: {e}")
            continue

        # Verify dataset
        nc, ec = verify_dataset(driver, target_name)
        print(f"  Dataset: {nc:,} nodes, {ec:,} edges")
        if nc < 1000:
            print(f"  ERROR: Dataset too small. Load nodes.cypher + edges.cypher first.")
            driver.close()
            continue
        sys.stdout.flush()

        # Pick start nodes
        start_nodes = pick_start_nodes(driver)
        print(f"  Start nodes: {json.dumps(start_nodes, indent=None)}")
        print(f"  Warmup: {WARMUP} iterations, Benchmark: {ITERATIONS} iterations")
        print()
        sys.stdout.flush()

        # Run benchmarks
        results = run_benchmarks(driver, target_name, start_nodes)
        all_results.extend(results)

        driver.close()

    # Summary
    print(f"\n{'='*72}")
    print("  RESULTS (JSON)")
    print(f"{'='*72}")
    print(json.dumps(all_results, indent=2))

    # Comparison table
    if len(all_results) > 1:
        print(f"\n{'='*72}")
        print("  COMPARISON: Tessera vs Memgraph")
        print(f"{'='*72}")
        tessera = {r["scenario"]: r for r in all_results if r["target"] == "tessera"}
        memgraph = {r["scenario"]: r for r in all_results if r["target"] == "memgraph"}
        for scenario in tessera:
            if scenario in memgraph:
                t = tessera[scenario]["mean_ms"]
                m = memgraph[scenario]["mean_ms"]
                ratio = m / t if t > 0 else float("inf")
                winner = "TESSERA" if t < m else "MEMGRAPH"
                print(f"  {scenario:30s}: T={t:7.2f}ms  M={m:7.2f}ms  "
                      f"ratio={ratio:.2f}x  → {winner}")


if __name__ == "__main__":
    main()
