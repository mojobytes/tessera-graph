#!/usr/bin/env bash
#
# Cross-Repo Write Guard — PreToolUse hook for the Enterprise repo.
# Blocks any Write/Edit/MultiEdit targeting the MIT (tessera-graph) repo.
#
# WHY: The MIT core (tessera-graph) provides basic GQL and graph primitives.
# Advanced GQL features (variable-length paths, shortestPath, WITH, OPTIONAL
# MATCH, CASE WHEN, etc.) are enterprise value and must be implemented in
# this repo — never in the MIT core. If the MIT core needs changes (e.g.,
# new AST types behind extended-gql), those must be done in a separate
# Claude session targeting the MIT repo directly, with explicit user approval.
#
# See docs/architecture/ROADMAP.md "MIT vs Enterprise Boundary" for the
# full boundary definition.
#
# Exit codes:
#   0 — allow
#   2 — block

set -euo pipefail

# Resolve the MIT repo path (sibling directory)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENTERPRISE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MIT_ROOT="$(cd "$ENTERPRISE_ROOT/../tessera-graph" 2>/dev/null && pwd)" || MIT_ROOT=""

# Read hook input from stdin
INPUT=$(cat)

# Extract file_path from the tool input JSON
FILE_PATH=$(echo "$INPUT" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    print(data.get('tool_input', {}).get('file_path', ''))
except Exception:
    print('')
" 2>/dev/null)

# If no file_path, allow (not a file write operation)
if [ -z "$FILE_PATH" ]; then
    exit 0
fi

# Resolve to absolute path
RESOLVED_PATH="$(cd "$(dirname "$FILE_PATH")" 2>/dev/null && pwd)/$(basename "$FILE_PATH")" 2>/dev/null || RESOLVED_PATH="$FILE_PATH"

# Block if path is under the MIT repo (append / to avoid prefix false positives:
# tessera-graph-enterprise starts with tessera-graph)
if [ -n "$MIT_ROOT" ] && [[ "$RESOLVED_PATH" == "$MIT_ROOT/"* ]]; then
    cat >&2 <<EOF

=== CROSS-REPO WRITE GUARD: BLOCKED ===
Target file: $FILE_PATH
This file belongs to the MIT repo (tessera-graph).
Enterprise repo hooks must not write to the MIT repo.
Use the MIT repo's Claude session for MIT changes.
========================================
EOF
    exit 2
fi

exit 0
