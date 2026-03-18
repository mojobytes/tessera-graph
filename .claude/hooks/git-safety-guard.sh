#!/usr/bin/env bash
#
# Git Safety Guard — PreToolUse hook for Bash commands.
#
# Catches common git mistakes:
# 1. git commit on protected branches (main, develop) — must use feature/* or fix/*
# 2. git push without a configured remote
# 3. git push --force (always dangerous)
#
# Exit codes:
#   0 — allow
#   2 — block

set -euo pipefail

INPUT=$(cat)

COMMAND=$(echo "$INPUT" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    print(data.get('tool_input', {}).get('command', ''))
except Exception:
    print('')
" 2>/dev/null)

# If not a git command, allow
if ! echo "$COMMAND" | grep -qE '\bgit\s+(commit|push)'; then
    exit 0
fi

# --- Check 1: Block commits on protected branches ---
if echo "$COMMAND" | grep -qE '\bgit\s+commit\b'; then
    CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")

    if [ "$CURRENT_BRANCH" = "main" ] || [ "$CURRENT_BRANCH" = "develop" ]; then
        cat >&2 <<EOF

=== GIT SAFETY: BLOCKED COMMIT ON PROTECTED BRANCH ===
Current branch: $CURRENT_BRANCH
Commits to 'main' and 'develop' are not allowed directly.
Create a feature branch first:
  git checkout -b feature/<name>
  git checkout -b fix/<name>
=======================================================
EOF
        exit 2
    fi
fi

# --- Check 2 & 3: git push safety ---
if echo "$COMMAND" | grep -qE '\bgit\s+push\b'; then

    # Check 3: Block force push
    if echo "$COMMAND" | grep -qE '\bgit\s+push\s+.*(-f|--force)\b'; then
        cat >&2 <<EOF

=== GIT SAFETY: BLOCKED FORCE PUSH ===
Force push is dangerous and can destroy remote history.
If absolutely necessary, do it manually outside Claude.
=======================================
EOF
        exit 2
    fi

    # Check 2: Verify remote exists
    REMOTES=$(git remote 2>/dev/null || echo "")
    if [ -z "$REMOTES" ]; then
        cat >&2 <<EOF

=== GIT SAFETY: NO REMOTE CONFIGURED ===
No git remote found. Cannot push.
Configure a remote first:
  git remote add origin <url>
=========================================
EOF
        exit 2
    fi

    # Extract target remote from command (default: origin)
    TARGET_REMOTE=$(echo "$COMMAND" | grep -oE '\bgit\s+push\s+(\S+)' | awk '{print $3}')
    TARGET_REMOTE="${TARGET_REMOTE:-origin}"

    if ! git remote | grep -qx "$TARGET_REMOTE"; then
        cat >&2 <<EOF

=== GIT SAFETY: REMOTE NOT FOUND ===
Remote '$TARGET_REMOTE' does not exist.
Available remotes: $(git remote | tr '\n' ' ')
====================================
EOF
        exit 2
    fi
fi

exit 0
