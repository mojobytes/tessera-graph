#!/usr/bin/env python3
"""Convert tessera-import JSON to GQL CREATE statements.

Input:  graph.json — { "nodes": [...], "edges": [...] }
Output: nodes.gql  — CREATE statements for nodes
        edges.gql  — CREATE statements for edges (MATCH+CREATE pairs)

Edges are in a separate file because all nodes must exist before edges.
"""

import json
import sys
from pathlib import Path


def escape_gql_string(s: str) -> str:
    """Escape a string for GQL single-quoted literals."""
    return s.replace("\\", "\\\\").replace("'", "\\'")


def property_to_gql(value) -> str:
    """Convert a JSON value to GQL literal."""
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return str(value)
    if isinstance(value, str):
        return f"'{escape_gql_string(value)}'"
    if isinstance(value, list):
        # Store arrays as JSON string
        return f"'{escape_gql_string(json.dumps(value))}'"
    # Fallback
    return f"'{escape_gql_string(str(value))}'"


def node_to_create(node: dict) -> str:
    """Convert a node dict to a CREATE statement."""
    label = node["label"]
    props = node.get("properties", {})
    if not props:
        return f"CREATE (:{label});"

    prop_pairs = []
    for k, v in props.items():
        prop_pairs.append(f"{k}: {property_to_gql(v)}")
    props_str = ", ".join(prop_pairs)
    return f"CREATE (:{label} {{{props_str}}});"


def main():
    base = Path(__file__).parent.parent
    input_path = base / "graph.json"
    nodes_path = base / "nodes.gql"
    edges_path = base / "edges.gql"

    if not input_path.exists():
        print(f"graph.json not found at {input_path}", file=sys.stderr)
        sys.exit(1)

    print("Loading graph.json...", file=sys.stderr)
    with open(input_path, "r", encoding="utf-8") as f:
        data = json.load(f)

    nodes = data["nodes"]
    edges = data["edges"]
    print(f"  {len(nodes)} nodes, {len(edges)} edges", file=sys.stderr)

    # Write nodes
    print(f"Writing {nodes_path}...", file=sys.stderr)
    with open(nodes_path, "w", encoding="utf-8") as f:
        for node in nodes:
            f.write(node_to_create(node))
            f.write("\n")

    nodes_mb = nodes_path.stat().st_size / (1024 * 1024)
    print(f"  {nodes_mb:.1f} MB", file=sys.stderr)

    # Write edges — each edge needs MATCH source, MATCH target, CREATE relationship
    # But tessera-graph GQL parser doesn't support MATCH+CREATE in one statement.
    # Use CREATE with inline relationship syntax if available, otherwise skip.
    # Actually, tessera-graph CREATE syntax for edges is not standard Cypher.
    # The GQL parser supports: MATCH (a) MATCH (b) WHERE ... CREATE (a)-[:REL]->(b)
    # Let's check what the parser can handle...
    #
    # For now, generate MATCH+CREATE pairs:
    print(f"Writing {edges_path}...", file=sys.stderr)
    with open(edges_path, "w", encoding="utf-8") as f:
        for edge in edges:
            src_id = edge["source"]["match"]["id"]
            tgt_id = edge["target"]["match"]["id"]
            rel = edge["label"]
            f.write(
                f"MATCH (a {{id: '{escape_gql_string(src_id)}'}}) "
                f"MATCH (b {{id: '{escape_gql_string(tgt_id)}'}}) "
                f"CREATE (a)-[:{rel}]->(b);\n"
            )

    edges_mb = edges_path.stat().st_size / (1024 * 1024)
    print(f"  {edges_mb:.1f} MB", file=sys.stderr)
    print("Done.", file=sys.stderr)


if __name__ == "__main__":
    main()
