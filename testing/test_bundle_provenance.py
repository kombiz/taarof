#!/usr/bin/env python3
"""Synthetic artifact/source fixtures; never execute a real provider or desktop."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

HELPER = Path(__file__).resolve().parents[1] / "packaging/linux/bundle-provenance.py"


class BundleProvenanceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.cli = self.root / "taarof-cli/taarof"
        self.cli.parent.mkdir()
        self.cli.write_text("#!/bin/sh\nprintf 'synthetic CLI\\n'\n")
        for args in (("init", "-q"), ("add", "taarof-cli/taarof"),
                     ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                      "commit", "-qm", "synthetic source")):
            subprocess.run(["git", "-C", str(self.root), *args], check=True, capture_output=True)
        revision = subprocess.check_output(["git", "-C", str(self.root), "rev-parse", "HEAD"], text=True).strip()
        self.app = self.root / "taarof-app"
        self.app.write_text("synthetic app fixture-build-id\n")
        self.sidecar = self.root / "app.json"
        self.identity = dict(source_revision=revision, source_dirty=False,
                             build_id="fixture-build-id", profile="release", built_at_unix="1704067200")
        self.sidecar.write_text(json.dumps(dict(self.identity, schema="taarof.artifact.v1",
                                               binary_sha256=self.hash(self.app))))
        self.agent = self.root / "agent"
        self.write_agent()
        self.gateway = self.root / "taarof-control-gateway"
        self.write_gateway()
        self.manifest = self.root / "bundle.json"

    @staticmethod
    def hash(path):
        return hashlib.sha256(path.read_bytes()).hexdigest()

    def write_agent(self, **changes):
        info = dict(self.identity, schema="agent.build.v1", version="0.1.0")
        info.update(changes)
        # JSON output is static, synthetic, and contains the embedded build token.
        self.agent.write_text("#!/usr/bin/python3\nprint(" + repr(json.dumps(info)) + ")\n")
        self.agent.chmod(0o755)

    def write_gateway(self, **changes):
        info = dict(self.identity, schema="taarof.gateway-build.v1", version="0.1.0")
        info.update(changes)
        self.gateway.write_text("#!/usr/bin/python3\nprint(" + repr(json.dumps(info)) + ")\n")
        self.gateway.chmod(0o755)

    def invoke(self, operation):
        args = [sys.executable, str(HELPER), operation, "--app", str(self.app),
                "--app-sidecar", str(self.sidecar), "--agent", str(self.agent), "--cli", str(self.cli),
                "--gateway", str(self.gateway)]
        args += (["--source-root", str(self.root), "--output", str(self.manifest)]
                 if operation == "create" else ["--manifest", str(self.manifest)])
        return subprocess.run(args, capture_output=True, text=True)

    def test_binds_all_installed_artifacts_to_the_compiled_revision(self):
        result = self.invoke("create")
        self.assertEqual(result.returncode, 0, result.stderr)
        manifest = json.loads(self.manifest.read_text())
        self.assertEqual(manifest["source_revision"], self.identity["source_revision"])
        self.assertFalse(manifest["source_dirty"])
        self.assertEqual(manifest["artifacts"], {
            "taarof-app": {"sha256": self.hash(self.app)},
            "taarof": {"sha256": self.hash(self.cli)},
            "agent": {"sha256": self.hash(self.agent)},
            "taarof-control-gateway": {"sha256": self.hash(self.gateway)},
        })
        self.assertEqual(manifest["gateway_build_id"], self.identity["build_id"])
        self.assertEqual(self.invoke("verify").returncode, 0)

    def test_stale_or_dirty_launcher_cannot_borrow_application_identity(self):
        for change in (dict(source_revision="0" * 40), dict(source_dirty=True), dict(profile="debug")):
            with self.subTest(change=change):
                self.write_agent(**change)
                self.assertNotEqual(self.invoke("create").returncode, 0)
                self.assertFalse(self.manifest.exists())

    def test_dirty_bundle_remains_installable_without_claiming_clean_source(self):
        self.identity["source_dirty"] = True
        self.sidecar.write_text(json.dumps(dict(self.identity, schema="taarof.artifact.v1",
                                               binary_sha256=self.hash(self.app))))
        self.write_agent()
        self.write_gateway()
        self.cli.write_text("uncommitted synthetic CLI change\n")
        result = self.invoke("create")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIs(json.loads(self.manifest.read_text())["source_dirty"], True)
        self.assertEqual(self.invoke("verify").returncode, 0)
        self.write_agent(source_dirty=False)
        self.assertNotEqual(self.invoke("create").returncode, 0)

    def test_unknown_executable_is_rejected_without_running_it(self):
        sentinel = self.root / "unexpected-execution"
        self.agent.write_text("#!/bin/sh\ntouch " + str(sentinel) + "\n")
        result = self.invoke("create")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(sentinel.exists())

    def test_cli_must_match_the_exact_source_blob(self):
        self.cli.write_text("changed synthetic CLI\n")
        result = self.invoke("create")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("CLI bytes differ", result.stderr)
        self.assertFalse(self.manifest.exists())

    def test_each_replaced_artifact_fails_verification(self):
        self.assertEqual(self.invoke("create").returncode, 0)
        for path in (self.app, self.agent, self.cli, self.gateway):
            with self.subTest(path=path.name):
                original = path.read_bytes()
                path.write_bytes(original + b"changed\n")
                self.assertNotEqual(self.invoke("verify").returncode, 0)
                path.write_bytes(original)

    def test_forged_manifest_cannot_change_the_build_identity(self):
        self.assertEqual(self.invoke("create").returncode, 0)
        original = json.loads(self.manifest.read_text())
        for field, value in (("source_revision", "0" * 40), ("source_dirty", True),
                             ("app_build_id", "other"), ("agent_build_id", "other"),
                             ("gateway_build_id", "other")):
            with self.subTest(field=field):
                self.manifest.write_text(json.dumps(dict(original, **{field: value})))
                self.assertNotEqual(self.invoke("verify").returncode, 0)

    def test_stale_gateway_cannot_borrow_application_identity(self):
        for change in (dict(source_revision="0" * 40), dict(source_dirty=True), dict(profile="debug")):
            with self.subTest(change=change):
                self.write_gateway(**change)
                self.assertNotEqual(self.invoke("create").returncode, 0)
                self.assertFalse(self.manifest.exists())


if __name__ == "__main__":
    unittest.main()
