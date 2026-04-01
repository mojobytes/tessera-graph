#!/usr/bin/env python3
"""Convert Memgraph CSV export to tessera-import JSON format.

Input:  nodes.csv  — "labels","props" from mgconsole --output_format csv
        edges.csv  — "source_id","target_id","rel_type"
Output: graph.json — { "nodes": [...], "edges": [...] } for tessera-import
"""

import csv
import json
import re
import sys
from pathlib import Path


# ---------------------------------------------------------------------------
# Memgraph property-map parser
# ---------------------------------------------------------------------------
# Memgraph outputs props like:
#   {key1: "val", key2: true, key3: 5, key4: "str with \"escaped\" quotes"}
# This is NOT JSON — keys are unquoted, strings use "" (CSV-doubled) quotes.

def parse_memgraph_props(raw: str) -> dict:
    """Parse a Memgraph property map string into a Python dict."""
    raw = raw.strip()
    if not raw or raw == "{}":
        return {}

    # Strip outer braces
    if raw.startswith("{") and raw.endswith("}"):
        raw = raw[1:-1].strip()

    result = {}
    i = 0
    length = len(raw)

    while i < length:
        # Skip whitespace and commas
        while i < length and raw[i] in (" ", "\t", ",", "\n", "\r"):
            i += 1
        if i >= length:
            break

        # Parse key (unquoted identifier — may contain spaces? No, Memgraph keys don't)
        # Keys can contain letters, digits, underscores, and some special chars
        key_start = i
        while i < length and raw[i] != ":":
            i += 1
        key = raw[key_start:i].strip()
        if not key:
            break
        i += 1  # skip ':'

        # Skip whitespace after colon
        while i < length and raw[i] in (" ", "\t"):
            i += 1
        if i >= length:
            result[key] = None
            break

        # Parse value
        if raw[i] == '"':
            # String value — handle doubled quotes from CSV
            i += 1  # skip opening quote
            val_chars = []
            while i < length:
                if raw[i] == '"':
                    # Check for doubled quote (CSV escaping)
                    if i + 1 < length and raw[i + 1] == '"':
                        val_chars.append('"')
                        i += 2
                    else:
                        # End of string
                        i += 1
                        break
                elif raw[i] == '\\':
                    # Backslash escaping (for embedded JSON like specifications)
                    if i + 1 < length:
                        next_ch = raw[i + 1]
                        if next_ch == '"':
                            val_chars.append('"')
                            i += 2
                        elif next_ch == '\\':
                            val_chars.append('\\')
                            i += 2
                        elif next_ch == 'n':
                            val_chars.append('\n')
                            i += 2
                        elif next_ch == 't':
                            val_chars.append('\t')
                            i += 2
                        else:
                            val_chars.append(raw[i])
                            i += 1
                    else:
                        val_chars.append(raw[i])
                        i += 1
                else:
                    val_chars.append(raw[i])
                    i += 1
            result[key] = "".join(val_chars)
        elif raw[i:i+4] == "true":
            result[key] = True
            i += 4
        elif raw[i:i+5] == "false":
            result[key] = False
            i += 5
        elif raw[i:i+4] == "null" or raw[i:i+4] == "Null":
            result[key] = None
            i += 4
        elif raw[i] == '[':
            # Array value — find matching bracket
            depth = 1
            start = i
            i += 1
            while i < length and depth > 0:
                if raw[i] == '[':
                    depth += 1
                elif raw[i] == ']':
                    depth -= 1
                elif raw[i] == '"':
                    i += 1
                    while i < length:
                        if raw[i] == '"':
                            if i + 1 < length and raw[i+1] == '"':
                                i += 2
                            else:
                                break
                        else:
                            i += 1
                i += 1
            result[key] = raw[start:i]
        else:
            # Numeric value
            val_start = i
            while i < length and raw[i] not in (",", "}", " ", "\t"):
                i += 1
            val_str = raw[val_start:i].strip()
            try:
                if "." in val_str:
                    result[key] = float(val_str)
                else:
                    result[key] = int(val_str)
            except ValueError:
                result[key] = val_str

    return result


def parse_labels(raw: str) -> list[str]:
    """Parse label list like '["AssetNode", "Client"]' into ["AssetNode", "Client"]."""
    raw = raw.strip()
    if raw.startswith("[") and raw.endswith("]"):
        # Remove brackets and split — handles CSV doubled quotes
        inner = raw[1:-1]
        labels = []
        for part in inner.split(","):
            label = part.strip().strip('"').strip()
            if label:
                labels.append(label)
        return labels
    return [raw.strip('"')]


def strip_triple_quotes(s: str) -> str:
    """Remove extra CSV quoting — '\"\"\"uuid\"\"\"' → 'uuid'."""
    # mgconsole CSV wraps strings in "" which CSV reader turns into "
    return s.strip('"')


def main():
    base = Path(__file__).parent.parent
    nodes_path = base / "nodes.csv"
    edges_path = base / "edges.csv"
    output_path = base / "graph.json"

    if not nodes_path.exists():
        print(f"nodes.csv not found at {nodes_path}", file=sys.stderr)
        sys.exit(1)
    if not edges_path.exists():
        print(f"edges.csv not found at {edges_path}", file=sys.stderr)
        sys.exit(1)

    # --- Parse nodes ---
    nodes = []
    errors = 0
    print("Parsing nodes...", file=sys.stderr)
    with open(nodes_path, "r", encoding="utf-8") as f:
        reader = csv.reader(f)
        header = next(reader)  # skip header
        for row_num, row in enumerate(reader, start=2):
            if len(row) < 2:
                errors += 1
                continue
            labels_raw, props_raw = row[0], row[1]
            try:
                labels = parse_labels(labels_raw)
                props = parse_memgraph_props(props_raw)
                # tessera-import uses single label — use the most specific one
                # (last non-AssetNode label, or first if all are AssetNode)
                specific_labels = [l for l in labels if l != "AssetNode"]
                label = specific_labels[-1] if specific_labels else labels[0]
                # Store all labels as a property for multi-label support
                if len(labels) > 1:
                    props["_labels"] = labels
                nodes.append({"label": label, "properties": props})
            except Exception as e:
                errors += 1
                if errors <= 10:
                    print(f"  Row {row_num}: {e}", file=sys.stderr)

    print(f"  Parsed {len(nodes)} nodes ({errors} errors)", file=sys.stderr)

    # --- Build id→label index for edge endpoints ---
    id_to_label: dict[str, str] = {}
    for node in nodes:
        node_id = node["properties"].get("id")
        if node_id:
            id_to_label[node_id] = node["label"]
    print(f"  Built id→label index: {len(id_to_label)} entries", file=sys.stderr)

    # --- Parse edges ---
    edges = []
    edge_errors = 0
    missing_labels = 0
    print("Parsing edges...", file=sys.stderr)
    with open(edges_path, "r", encoding="utf-8") as f:
        reader = csv.reader(f)
        header = next(reader)  # skip header
        for row_num, row in enumerate(reader, start=2):
            if len(row) < 3:
                edge_errors += 1
                continue
            source_id = strip_triple_quotes(row[0])
            target_id = strip_triple_quotes(row[1])
            rel_type = strip_triple_quotes(row[2])
            source_label = id_to_label.get(source_id)
            target_label = id_to_label.get(target_id)
            if not source_label or not target_label:
                missing_labels += 1
                if missing_labels <= 5:
                    print(f"  Row {row_num}: missing label for source={source_id} or target={target_id}", file=sys.stderr)
                continue
            edges.append({
                "source": {"label": source_label, "match": {"id": source_id}},
                "target": {"label": target_label, "match": {"id": target_id}},
                "label": rel_type,
                "properties": {},
            })

    print(f"  Parsed {len(edges)} edges ({edge_errors} errors, {missing_labels} missing labels)", file=sys.stderr)

    # --- Write output ---
    print(f"Writing {output_path}...", file=sys.stderr)
    with open(output_path, "w", encoding="utf-8") as f:
        json.dump({"nodes": nodes, "edges": edges}, f, ensure_ascii=False)

    size_mb = output_path.stat().st_size / (1024 * 1024)
    print(f"Done: {len(nodes)} nodes, {len(edges)} edges, {size_mb:.1f} MB", file=sys.stderr)


if __name__ == "__main__":
    main()
