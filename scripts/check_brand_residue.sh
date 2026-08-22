#!/usr/bin/env bash
set -euo pipefail

if ! command -v rg >/dev/null 2>&1; then
    echo "error: rg is required" >&2
    exit 2
fi

matches="$({
    rg -n -i 'tessera' . \
        --glob '!.git/**' \
        --glob '!scripts/check_brand_residue.sh' \
        --glob '!CHANGELOG.md' \
        --glob '!docs/plans/**' \
        --glob '!docs/specs/**' \
        --glob '!docs/superpowers/**' || test $? -eq 1
})"

if [[ -n "$matches" ]]; then
    echo "$matches"
    exit 1
fi
