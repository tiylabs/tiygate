"""Offline fault injection for protocol CI checks; no external tool installs."""

import hashlib
import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "check_protocol_specs", ROOT / "scripts" / "check-protocol-specs.py"
)
SNAPSHOTS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SNAPSHOTS)


class SnapshotChecks(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="tiygate-ci-snapshots-")
        self.addCleanup(self.scratch.cleanup)
        self.directory = Path(self.scratch.name)
        openai = b"openapi: 3.1.0\n"
        gemini = b'{"revision":"test-revision"}\n'
        for relative, content in [
            ("openai/openapi.yaml", openai),
            ("gemini/v1beta.discovery.json", gemini),
        ]:
            destination = self.directory / relative
            destination.parent.mkdir(parents=True)
            destination.write_bytes(content)
        self.lock = {
            "resources": {
                "openai-openapi": {
                    "url": "https://raw.githubusercontent.com/openai/openai-openapi/main/openapi.yaml",
                    "sha256": hashlib.sha256(openai).hexdigest(),
                },
                "gemini-v1beta-discovery": {
                    "url": "https://generativelanguage.googleapis.com/$discovery/rest?version=v1beta",
                    "sha256": hashlib.sha256(gemini).hexdigest(),
                    "revision": "test-revision",
                },
            }
        }
        self.write_lock()

    def write_lock(self):
        (self.directory / "lock.json").write_text(json.dumps(self.lock))

    def test_matching_snapshots_pass_without_writes(self):
        before = {p: p.read_bytes() for p in self.directory.rglob("*") if p.is_file()}
        SNAPSHOTS.check_snapshots(self.directory)
        self.assertEqual(before, {p: p.read_bytes() for p in before})

    def test_changed_snapshot_is_rejected_without_rewriting_lock(self):
        original_lock = (self.directory / "lock.json").read_bytes()
        (self.directory / "openai/openapi.yaml").write_bytes(b"changed snapshot")
        with self.assertRaisesRegex(ValueError, "SHA-256"):
            SNAPSHOTS.check_snapshots(self.directory)
        self.assertEqual(original_lock, (self.directory / "lock.json").read_bytes())

    def test_revision_mismatch_is_rejected(self):
        self.lock["resources"]["gemini-v1beta-discovery"]["revision"] = "wrong"
        self.write_lock()
        with self.assertRaisesRegex(ValueError, "revision"):
            SNAPSHOTS.check_snapshots(self.directory)

    def test_unofficial_source_is_rejected(self):
        self.lock["resources"]["openai-openapi"]["url"] = "https://example.invalid/schema"
        self.write_lock()
        with self.assertRaisesRegex(ValueError, "source URL"):
            SNAPSHOTS.check_snapshots(self.directory)

    def test_missing_resource_is_rejected(self):
        del self.lock["resources"]["openai-openapi"]
        self.write_lock()
        with self.assertRaisesRegex(ValueError, "inventory"):
            SNAPSHOTS.check_snapshots(self.directory)

    def test_missing_snapshot_is_rejected(self):
        (self.directory / "openai/openapi.yaml").unlink()
        with self.assertRaises(OSError):
            SNAPSHOTS.check_snapshots(self.directory)

    def test_malformed_lock_is_rejected(self):
        (self.directory / "lock.json").write_text("invalid JSON")
        with self.assertRaises(ValueError):
            SNAPSHOTS.check_snapshots(self.directory)


class LayeringChecks(unittest.TestCase):
    def run_check(self, failed_package="", injected_package="", dependency=""):
        with tempfile.TemporaryDirectory(prefix="tiygate-ci-layering-") as scratch:
            cargo = Path(scratch) / "cargo"
            cargo.write_text(
                '#!/bin/sh\n'
                'while [ "$#" -gt 0 ]; do\n'
                '  if [ "$1" = "-p" ]; then shift; package=$1; break; fi\n'
                '  shift\n'
                'done\n'
                'if [ "$package" = "$FAILED_PACKAGE" ]; then exit 37; fi\n'
                'printf "%s v0.1.0\\n" "$package"\n'
                'if [ "$package" = "$INJECTED_PACKAGE" ]; then\n'
                '  printf "%s v1.0.0\\n" "$INJECTED_DEPENDENCY"\n'
                'fi\n'
            )
            cargo.chmod(0o755)
            env = dict(
                os.environ,
                PATH=f"{scratch}:{os.environ['PATH']}",
                FAILED_PACKAGE=failed_package,
                INJECTED_PACKAGE=injected_package,
                INJECTED_DEPENDENCY=dependency,
            )
            return subprocess.run(
                ["bash", str(ROOT / "scripts" / "verify-deps.sh")],
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
            )

    def test_dependency_query_failure_never_passes(self):
        for package in [
            "tiygate-core",
            "tiygate-providers",
            "tiygate-provider-bedrock",
            "tiygate-desktop",
        ]:
            with self.subTest(package=package):
                result = self.run_check(failed_package=package)
                self.assertEqual(result.returncode, 37, result.stdout + result.stderr)
                self.assertNotIn("All dependency isolation checks passed", result.stdout)

    def test_forbidden_transitive_dependency_is_rejected(self):
        for package, dependency in [
            ("tiygate-core", "sqlx"),
            ("tiygate-providers", "aws-sdk-bedrockruntime"),
            ("tiygate-desktop", "tiygate-protocols"),
        ]:
            with self.subTest(package=package):
                result = self.run_check(injected_package=package, dependency=dependency)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("FAIL:", result.stdout)

    def test_clean_dependencies_pass(self):
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
