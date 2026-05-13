#!/usr/bin/env python3
"""Convert tessera-import JSON to Cypher CREATE statements for Memgraph.

Generates two files:
- nodes.cypher — CREATE statements for all nodes
- edges.cypher — MATCH...CREATE statements for all edges
"""

import json
import sys
from pathlib import Path


def escape_cypher_string(s: str) -> str:
    """Escape a string for Cypher single-quoted literals."""
    return s.replace("\\", "\\\\").replace("'", "\\'")


def value_to_cypher(v) -> str:
    if v is None:
        return "null"
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    if isinstance(v, float):
        return str(v)
    if isinstance(v, str):
        return f"'{escape_cypher_string(v)}'"
    if isinstance(v, list):
        return f"'{escape_cypher_string(json.dumps(v))}'"
    return f"'{escape_cypher_string(str(v))}'"


def main():
    base = Path(__file__).parent.parent
    input_path = base / "graph.json"
    nodes_path = base / "nodes.cypher"
    edges_path = base / "edges.cypher"

    print("Loading graph.json...", file=sys.stderr)
    with open(input_path, "r", encoding="utf-8") as f:
        data = json.load(f)

    nodes = data["nodes"]
    edges = data["edges"]
    print(f"  {len(nodes)} nodes, {len(edges)} edges", file=sys.stderr)

    # Nodes
    print(f"Writing {nodes_path}...", file=sys.stderr)
    with open(nodes_path, "w", encoding="utf-8") as f:
        for node in nodes:
            label = node["label"]
            props = node.get("properties", {})
            if props:
                pairs = [f"`{k}`: {value_to_cypher(v)}" for k, v in props.items()]
                f.write(f"CREATE (:{label} {{{', '.join(pairs)}}});\n")
            else:
                f.write(f"CREATE (:{label});\n")

    # Edges
    print(f"Writing {edges_path}...", file=sys.stderr)
    with open(edges_path, "w", encoding="utf-8") as f:
        for edge in edges:
            src = edge["source"]
            tgt = edge["target"]
            rel = edge["label"]

            src_match_key = list(src["match"].keys())[0]
            src_match_val = value_to_cypher(src["match"][src_match_key])
            tgt_match_key = list(tgt["match"].keys())[0]
            tgt_match_val = value_to_cypher(tgt["match"][tgt_match_key])

            src_label = src.get("label", "")
            tgt_label = tgt.get("label", "")
            src_label_str = f":{src_label}" if src_label else ""
            tgt_label_str = f":{tgt_label}" if tgt_label else ""

            f.write(
                f"MATCH (a{src_label_str} {{{src_match_key}: {src_match_val}}}), "
                f"(b{tgt_label_str} {{{tgt_match_key}: {tgt_match_val}}}) "
                f"CREATE (a)-[:{rel}]->(b);\n"
            )

    nodes_mb = nodes_path.stat().st_size / (1024 * 1024)
    edges_mb = edges_path.stat().st_size / (1024 * 1024)
    print(f"Done: {nodes_mb:.1f} MB nodes, {edges_mb:.1f} MB edges", file=sys.stderr)


if __name__ == "__main__":
    main()
