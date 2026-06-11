#!/usr/bin/env bash
# Build the embedded single-binary: frontend first, then the Rust
# server with the `webui` feature. rust-embed reads webui/dist at
# compile time (release) so the frontend MUST be built first.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> Building WebUI (webui/dist)"
pushd webui >/dev/null
if [ ! -d node_modules ]; then
  npm install
fi
npm run build
popd >/dev/null

echo "==> Building tiygate-server (release, webui embedded)"
cargo build -p tiygate-server --release --features webui

echo "==> Done. Binary: target/release/tiygate"
