#!/usr/bin/env python3
"""Synthetic release-receipt regressions; no provider accounts or live runtime."""
import importlib.util
import io
import json
from pathlib import Path
import shutil
import tarfile
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "agent_launcher_smoke", Path(__file__).parent / "e2e-gui/agent_launcher_smoke.py")
SMOKE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SMOKE)


class InstalledIdentityTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.package = self.root / "package"
        self.prefix = self.root / "installed"
        self.source = "1" * 40
        (self.package / "bin").mkdir(parents=True)
        share = self.package / "share/taarof"
        share.mkdir(parents=True)
        artifacts = {}
        for name in ("agent", "taarof", "taarof-app"):
            path = self.package / "bin" / name
            path.write_text("synthetic " + name)
            path.chmod(0o755)
            artifacts[name] = {"sha256": SMOKE.digest(path)}
        manifest = dict(schema="taarof.bundle.v1", source_revision=self.source,
                        source_dirty=False, app_build_id="app", agent_build_id="agent",
                        artifacts=artifacts)
        (share / "bundle-manifest.json").write_text(json.dumps(manifest))
        (share / "install-manifest.json").write_text(json.dumps(dict(
            schema="taarof.artifact.v1", source_revision=self.source, source_dirty=False,
            build_id="app", binary_sha256=artifacts["taarof-app"]["sha256"])))
        shutil.copytree(self.package, self.prefix)

    def archive(self, prefix="taarof-linux-x86_64", extra=None):
        archive = self.root / "package.tar.gz"
        with tarfile.open(archive, "w:gz") as handle:
            for name in ("agent", "taarof", "taarof-app"):
                data = (self.package / "bin" / name).read_bytes()
                member = tarfile.TarInfo(prefix + "/bin/" + name)
                member.size = len(data)
                handle.addfile(member, io.BytesIO(data))
            if extra:
                member = tarfile.TarInfo(extra)
                member.size = 1
                handle.addfile(member, io.BytesIO(b"x"))
        return archive

    def test_supported_archive_and_install_are_bound_without_claiming_runtime(self):
        result = SMOKE.identity(self.prefix, self.package, self.source, self.archive())
        self.assertEqual(result["status"], "pass")
        self.assertEqual(SMOKE.runtime_identity(None, "unused")["status"], "not_observed")

    def test_archive_paths_cannot_traverse_or_borrow_another_bundle_root(self):
        for prefix in ("../taarof-linux-x86_64", "/taarof-linux-x86_64",
                       "other", "taarof-linux-x86_64/../taarof-linux-x86_64"):
            with self.subTest(prefix=prefix), self.assertRaises(SMOKE.SmokeFailure):
                SMOKE.identity(self.prefix, self.package, self.source, self.archive(prefix))

    def test_non_command_archive_members_cannot_escape_the_bundle(self):
        for extra in ("../escape", "/absolute", "taarof-linux-x86_64/../../escape"):
            with self.subTest(extra=extra), self.assertRaises(SMOKE.SmokeFailure):
                SMOKE.identity(self.prefix, self.package, self.source,
                               self.archive(extra=extra))

    def test_replaced_installed_commands_fail_even_with_unchanged_manifests(self):
        for name in ("agent", "taarof", "taarof-app"):
            path = self.prefix / "bin" / name
            original = path.read_bytes()
            path.write_bytes(original + b"different")
            with self.subTest(name=name), self.assertRaises(SMOKE.SmokeFailure):
                SMOKE.identity(self.prefix, self.package, self.source)
            path.write_bytes(original)

    def test_duplicate_and_linked_commands_are_rejected(self):
        for kind in ("duplicate", "symlink", "hardlink"):
            archive = self.archive()
            # Re-create the archive, preserving its otherwise valid command bytes.
            with tarfile.open(archive, "r:gz") as handle:
                entries = [(member, handle.extractfile(member).read()) for member in handle]
            with tarfile.open(archive, "w:gz") as handle:
                for member, data in entries:
                    handle.addfile(member, io.BytesIO(data))
                member = tarfile.TarInfo("taarof-linux-x86_64/bin/agent")
                if kind != "duplicate":
                    member.type = tarfile.SYMTYPE if kind == "symlink" else tarfile.LNKTYPE
                    member.linkname = "../../outside"
                handle.addfile(member)
            with self.subTest(kind=kind), self.assertRaises(SMOKE.SmokeFailure):
                SMOKE.identity(self.prefix, self.package, self.source, archive)


if __name__ == "__main__":
    unittest.main()
