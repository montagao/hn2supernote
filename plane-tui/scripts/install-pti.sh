#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${HOME}/.local/bin/pti"

mkdir -p "${HOME}/.local/bin"
cargo build --release --manifest-path "${ROOT}/Cargo.toml"
install -m 0755 "${ROOT}/target/release/plane-tui" "${DEST}"

echo "installed ${DEST}"
