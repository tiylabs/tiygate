#!/usr/bin/env python3
"""Verify committed API wire snapshots against their lock without network I/O."""

import hashlib
import json
import sys
from pathlib import Path


RESOURCES = {
    "openai-openapi": (
        "openai/openapi.yaml",
        "https://raw.githubusercontent.com/openai/openai-openapi/main/openapi.yaml",
    ),
    "gemini-v1beta-discovery": (
        "gemini/v1beta.discovery.json",
        "https://generativelanguage.googleapis.com/$discovery/rest?version=v1beta",
    ),
}


def check_snapshots(spec_dir: Path) -> None:
    lock = json.loads((spec_dir / "lock.json").read_text(encoding="utf-8"))
    if not isinstance(lock, dict) or not isinstance(lock.get("resources"), dict):
        raise ValueError("lock.json must contain a resources object")
    resources = lock["resources"]
    if set(resources) != set(RESOURCES):
        raise ValueError("lock.json resource inventory differs from the snapshot inventory")

    for resource_id, (relative_path, url) in RESOURCES.items():
        entry = resources[resource_id]
        if not isinstance(entry, dict) or entry.get("url") != url:
            raise ValueError(f"{resource_id}: unexpected or missing official source URL")
        snapshot = (spec_dir / relative_path).read_bytes()
        if not snapshot:
            raise ValueError(f"{relative_path}: empty snapshot")
        digest = hashlib.sha256(snapshot).hexdigest()
        if entry.get("sha256") != digest:
            raise ValueError(f"{relative_path}: SHA-256 differs from lock.json")
        if resource_id == "gemini-v1beta-discovery":
            document = json.loads(snapshot)
            revision = document.get("revision") if isinstance(document, dict) else None
            if not isinstance(revision, str) or not revision:
                raise ValueError(f"{relative_path}: missing Discovery revision")
            if entry.get("revision") != revision:
                raise ValueError(f"{relative_path}: revision differs from lock.json")
        print(f"verified: {relative_path} ({digest})")


def main() -> int:
    spec_dir = Path(__file__).resolve().parent.parent / "protocol-specs" / "api-wire"
    try:
        check_snapshots(spec_dir)
    except (OSError, ValueError) as error:
        print(f"protocol snapshot check failed: {error}", file=sys.stderr)
        return 1
    print("committed protocol snapshots match their lock (offline)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
