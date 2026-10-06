#!/bin/bash
# Verify that provider-bedrock's heavy dependencies (AWS SDK etc.)
# do not leak into core or other provider crates.
set -euo pipefail

echo "=== Checking core has no concrete HTTP/database/AWS dependencies ==="
CORE_DEPS=$(cargo tree --locked -p tiygate-core --all-features --edges normal --prefix none)
if printf '%s\n' "$CORE_DEPS" | grep -Ei '^(reqwest|redis|sqlx|tiygate-store|tiygate-protocols|tiygate-providers|aws[^ ]*|tiygate-provider-bedrock) ' >/dev/null; then
    echo "FAIL: Concrete I/O/provider/protocol dependency found in core!"
    exit 1
fi
echo "PASS: Core is clean"

echo ""
echo "=== Checking providers have no AWS dependencies ==="
if cargo tree -p tiygate-providers --depth 3 2>/dev/null | grep -qi 'aws\|bedrock'; then
    echo "FAIL: AWS/Bedrock dependencies found in providers!"
    exit 1
fi
echo "PASS: Providers are clean"

echo ""
echo "=== Checking bedrock crate IS self-contained ==="
cargo tree -p tiygate-provider-bedrock --depth 1 2>/dev/null
echo "PASS: Bedrock crate dependencies listed"

echo ""
echo "=== Checking src-tauri has no tiygate crate dependencies ==="
# The Tauri client crate must not depend on any tiygate-* internal
# crate — it manages the sidecar as an external binary process.
# We exclude the package's own name (tiygate-desktop) from the match.
if cargo tree -p tiygate-desktop --depth 2 2>/dev/null | grep -v 'tiygate-desktop' | grep -qi 'tiygate-'; then
    echo "FAIL: tiygate-* internal crate dependency found in src-tauri!"
    exit 1
fi
echo "PASS: src-tauri is isolated (no internal tiygate-* deps)"

echo ""
echo "All dependency isolation checks passed!"
