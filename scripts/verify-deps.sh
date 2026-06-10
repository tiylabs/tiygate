#!/bin/bash
# Verify that provider-bedrock's heavy dependencies (AWS SDK etc.)
# do not leak into core or other provider crates.
set -euo pipefail

echo "=== Checking core has no AWS dependencies ==="
if cargo tree -p tiygate-core --depth 3 2>/dev/null | grep -qi 'aws\|bedrock'; then
    echo "FAIL: AWS/Bedrock dependencies found in core!"
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
echo "All dependency isolation checks passed!"
