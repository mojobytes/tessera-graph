#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --exclude ermya-graph-python --features plain-tcp --locked
cargo audit
cargo package -p ermya-graph --locked

release_venv="$(mktemp -d)/venv"
python3 -m venv "${release_venv}"
"${release_venv}/bin/pip" install 'maturin>=1.7,<2.0' pytest
VIRTUAL_ENV="${release_venv}" "${release_venv}/bin/maturin" develop --locked \
  --manifest-path crates/ermya-graph-python/Cargo.toml
"${release_venv}/bin/python" -m pytest crates/ermya-graph-python/tests -q
