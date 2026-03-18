#!/usr/bin/env bash
#
# Post-Edit Check — PostToolUse hook for Write/Edit/MultiEdit.
#
# Runs `cargo check` after Rust file edits to catch type errors and
# borrow issues early (instead of accumulating them).
#
# Exit codes:
#   0 — always (PostToolUse hooks are informational, not blocking)
# Output on stderr is shown to Claude as feedback.

set -uo pipefail

INPUT=$(cat)

# Extract file_path from tool input
FILE_PATH=$(echo "$INPUT" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    print(data.get('tool_input', {}).get('file_path', ''))
except Exception:
    print('')
" 2>/dev/null)

# Only check .rs and Cargo.toml files
case "$FILE_PATH" in
    *.rs|*/Cargo.toml)
        ;;
    *)
        exit 0
        ;;
esac

# Run cargo check (fast — no codegen, just type checking)
OUTPUT=$(cargo check --message-format=short 2>&1) || true
EXIT_CODE=$?

if [ $EXIT_CODE -ne 0 ]; then
    # Filter to only error lines (not warnings) for brevity
    ERRORS=$(echo "$OUTPUT" | grep -E "^error" | head -10)
    if [ -n "$ERRORS" ]; then
        cat >&2 <<EOF

=== POST-EDIT: cargo check found errors ===
$ERRORS
============================================
EOF
    fi
fi

exit 0
