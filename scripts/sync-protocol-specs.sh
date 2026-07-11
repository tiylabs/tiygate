#!/bin/sh
# Refresh or verify the official machine-readable API wire-schema snapshots.
#
# Usage:
#   scripts/sync-protocol-specs.sh          # refresh snapshots and lock file
#   scripts/sync-protocol-specs.sh --check  # report stale snapshots, no writes

set -eu

mode="sync"
if [ "${1:-}" = "--check" ]; then
    mode="check"
elif [ "$#" -ne 0 ]; then
    echo "usage: $0 [--check]" >&2
    exit 2
fi

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
spec_dir="$root/protocol-specs/api-wire"
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/tiygate-protocol-specs.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

fetch() {
    name=$1
    url=$2
    destination=$3
    temporary="$tmp_dir/$name"

    curl --fail --location --retry 3 --silent --show-error \
        --output "$temporary" "$url"

    # Google's Discovery endpoint does not promise object-key ordering. Canonical
    # JSON keeps its digest meaningful when the revision and semantic document
    # have not changed.
    if [ "$name" = "gemini-v1beta.discovery.json" ]; then
        jq --sort-keys . "$temporary" >"$temporary.canonical"
        mv "$temporary.canonical" "$temporary"
    fi

    incoming_sha=$(shasum -a 256 "$temporary" | awk '{print $1}')
    last_sha=$incoming_sha
    last_revision=""
    if [ "$name" = "gemini-v1beta.discovery.json" ]; then
        last_revision=$(jq -r '.revision // "unknown"' "$temporary")
    fi
    if [ -f "$destination" ]; then
        existing_sha=$(shasum -a 256 "$destination" | awk '{print $1}')
    else
        existing_sha=""
    fi

    if [ "$mode" = "check" ]; then
        if [ "$incoming_sha" != "$existing_sha" ]; then
            echo "stale: ${destination#$root/}" >&2
            stale=1
        fi
        return
    fi

    mkdir -p "$(dirname -- "$destination")"
    mv "$temporary" "$destination"
}

stale=0
last_sha=""
last_revision=""
fetch \
    "openai-openapi.yaml" \
    "https://raw.githubusercontent.com/openai/openai-openapi/main/openapi.yaml" \
    "$spec_dir/openai/openapi.yaml"
openai_sha=$last_sha
fetch \
    "gemini-v1beta.discovery.json" \
    "https://generativelanguage.googleapis.com/\$discovery/rest?version=v1beta" \
    "$spec_dir/gemini/v1beta.discovery.json"
gemini_sha=$last_sha
gemini_revision=$last_revision

if [ "$mode" = "check" ]; then
    lock_file="$spec_dir/lock.json"
    if [ ! -f "$lock_file" ]; then
        echo "stale: protocol-specs/api-wire/lock.json is missing" >&2
        stale=1
    else
        locked_openai_sha=$(jq -r '.resources["openai-openapi"].sha256 // empty' "$lock_file")
        locked_gemini_sha=$(jq -r '.resources["gemini-v1beta-discovery"].sha256 // empty' "$lock_file")
        locked_gemini_revision=$(jq -r '.resources["gemini-v1beta-discovery"].revision // empty' "$lock_file")
        if [ "$locked_openai_sha" != "$openai_sha" ]; then
            echo "stale: protocol-specs/api-wire/lock.json (OpenAI digest)" >&2
            stale=1
        fi
        if [ "$locked_gemini_sha" != "$gemini_sha" ] || [ "$locked_gemini_revision" != "$gemini_revision" ]; then
            echo "stale: protocol-specs/api-wire/lock.json (Gemini revision or digest)" >&2
            stale=1
        fi
    fi
    if [ "$stale" -ne 0 ]; then
        exit 1
    fi
    echo "protocol specification snapshots are current"
    exit 0
fi

generated_at=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
lock_tmp="$tmp_dir/lock.json"
cat >"$lock_tmp" <<EOF
{
  "generated_at": "$generated_at",
  "resources": {
    "openai-openapi": {
      "url": "https://raw.githubusercontent.com/openai/openai-openapi/main/openapi.yaml",
      "sha256": "$openai_sha"
    },
    "gemini-v1beta-discovery": {
      "url": "https://generativelanguage.googleapis.com/\$discovery/rest?version=v1beta",
      "revision": "$gemini_revision",
      "sha256": "$gemini_sha"
    }
  }
}
EOF
mv "$lock_tmp" "$spec_dir/lock.json"
echo "refreshed protocol specification snapshots"
