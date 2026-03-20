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

# Use Python for robust parsing: extract the command, detect the git
# operation, and — for push — extract the target remote (skipping flags).
# Output format: "ACTION|REMOTE" where ACTION is commit/push/none.
PARSED=$(echo "$INPUT" | python3 -c "
import json, sys, shlex

try:
    data = json.load(sys.stdin)
    cmd = data.get('tool_input', {}).get('command', '')
except Exception:
    print('none|')
    sys.exit(0)

# Tokenise the first simple command (before && or ;).
# This avoids matching 'git push' inside embedded strings.
for sep in [' && ', ' ; ', ';']:
    cmd = cmd.split(sep)[0]

try:
    tokens = shlex.split(cmd)
except ValueError:
    print('none|')
    sys.exit(0)

# Walk tokens looking for 'git' followed by 'commit' or 'push'.
i = 0
while i < len(tokens):
    if tokens[i] == 'git' and i + 1 < len(tokens):
        sub = tokens[i + 1]
        if sub == 'commit':
            print('commit|')
            sys.exit(0)
        if sub == 'push':
            # Extract remote: first positional arg after 'push' (skip flags).
            rest = tokens[i + 2:]
            remote = ''
            skip_next = False
            for t in rest:
                if skip_next:
                    skip_next = False
                    continue
                if t == '--':
                    break
                if t.startswith('-'):
                    # Flags with a value attached (--repo=X) — skip.
                    # Short flags that take a value: none for git push.
                    continue
                remote = t
                break
            print(f'push|{remote or \"origin\"}')
            sys.exit(0)
    i += 1

print('none|')
" 2>/dev/null || echo "none|")

ACTION="${PARSED%%|*}"
REMOTE="${PARSED##*|}"

# If not a git commit/push, allow
if [ "$ACTION" = "none" ]; then
    exit 0
fi

# --- Check 1: Block commits on protected branches ---
if [ "$ACTION" = "commit" ]; then
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
if [ "$ACTION" = "push" ]; then

    # Check 3: Block force push
    FORCE_CHECK=$(echo "$INPUT" | python3 -c "
import json, sys, shlex
try:
    data = json.load(sys.stdin)
    cmd = data.get('tool_input', {}).get('command', '')
    for sep in [' && ', ' ; ', ';']:
        cmd = cmd.split(sep)[0]
    tokens = shlex.split(cmd)
    i = tokens.index('push') if 'push' in tokens else -1
    rest = tokens[i+1:] if i >= 0 else []
    has_force = any(t in ('-f', '--force', '--force-with-lease') for t in rest)
    print('yes' if has_force else 'no')
except Exception:
    print('no')
" 2>/dev/null || echo "no")

    if [ "$FORCE_CHECK" = "yes" ]; then
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

    if ! git remote | grep -qx "$REMOTE"; then
        cat >&2 <<EOF

=== GIT SAFETY: REMOTE NOT FOUND ===
Remote '$REMOTE' does not exist.
Available remotes: $(git remote | tr '\n' ' ')
====================================
EOF
        exit 2
    fi
fi

exit 0
