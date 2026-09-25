#!/usr/bin/env python3
"""Exercise real Git branch selection and launchers with a synthetic build toolchain."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("install_channel", ROOT / "packaging/linux/install-channel.py")
channel = importlib.util.module_from_spec(spec)
spec.loader.exec_module(channel)


class ChannelTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="taarof channel '")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.remote = self.base / "remote"
        self.remote.mkdir()
        self.git(self.remote, "init", "-q", "-b", "main")
        self.put(".gitignore", "target/\nnode_modules/\ndist/\n")
        self.put("taarof-app/Cargo.toml", "# fixture\n")
        self.put("taarof-web/package.json", "{}\n")
        self.put("channel", "main")
        self.put("packaging/linux/emit-artifact-provenance.sh", '''#!/bin/sh
python3 - "$1" <<'INNER'
import hashlib, json, pathlib, subprocess, sys
p=pathlib.Path(sys.argv[1])
revision=subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip()
p.with_name(p.name+'.provenance.json').write_text(json.dumps(dict(source_revision=revision, source_dirty=False, binary_sha256=hashlib.sha256(p.read_bytes()).hexdigest())))
INNER
''')
        self.put("packaging/linux/install-local.sh", '''#!/bin/sh
set -eu
mkdir -p "$1/bin" "$1/share/taarof/web" "$1/share/icons/hicolor/scalable/apps"
cp "$TAAROF_INSTALL_BINARY" "$1/bin/taarof-app"
cp "$TAAROF_INSTALL_BINARY" "$1/bin/taarof"
cp "$TAAROF_INSTALL_PROVENANCE" "$1/share/taarof/install-manifest.json"
printf '<svg/>' > "$1/share/icons/hicolor/scalable/apps/io.github.kombiz.taarof.svg"
''')
        self.commit()
        self.main_sha = self.git(self.remote, "rev-parse", "HEAD")
        self.git(self.remote, "checkout", "-qb", "kmux")
        self.put("channel", "kmux")
        self.commit()
        self.kmux_sha = self.git(self.remote, "rev-parse", "HEAD")
        self.repo = self.base / "checkout"
        subprocess.run(["git", "clone", "-q", str(self.remote), str(self.repo)], check=True)
        # Caller edits must never be selected for installation or discarded.
        (self.repo / "channel").write_text("uncommitted task work")
        self.prefix = self.base / "install prefix"
        self.cache = self.base / "build cache"
        self.tools = self.base / "tools"
        self.tools.mkdir()
        channel.write_executable(self.tools / "npm", "#!/bin/sh\nexit 0\n")
        channel.write_executable(self.tools / "cargo", '''#!/usr/bin/env python3
import os, pathlib
p=pathlib.Path(os.environ['CARGO_TARGET_DIR'])/'release/taarof-app'
p.parent.mkdir(parents=True, exist_ok=True)
label=pathlib.Path('channel').read_text()
p.write_text('#!/usr/bin/env python3\\nimport json,os,sys\\nprint(json.dumps(dict(channel='+repr(label)+', session=os.environ.get("TAAROF_SESSION"), installed=os.environ.get("TAAROF_INSTALLED_BINARY"), agent=os.environ.get("TAAROF_AGENT_BINARY"), sock=os.environ.get("TAAROF_SOCK"), argv=sys.argv[1:])))\\n')
p.chmod(0o755)
''')
        self.environment = patch.dict(os.environ, {
            "PATH": str(self.tools) + os.pathsep + os.environ["PATH"],
            "TAAROF_INSTALL_BINARY": "/must-not-use",
            "TAAROF_ARTIFACT_SIDECAR": "/must-not-write",
        })
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def put(self, name, text):
        p = self.remote / name
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)

    def git(self, cwd, *args):
        return subprocess.check_output(["git", *args], cwd=cwd, text=True, stderr=subprocess.PIPE).strip()

    def commit(self):
        self.git(self.remote, "add", ".")
        self.git(self.remote, "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "fixture")

    def install(self, name):
        with patch.object(channel, "REMOTES", {str(self.remote)}), patch.object(channel.shutil, "which", return_value=None):
            channel.install(self.repo, name, self.prefix, self.cache)

    def launch(self, name, *args):
        env = dict(os.environ, TAAROF_SESSION="inherited-wrong-target", TAAROF_SOCK="/wrong.sock")
        return json.loads(subprocess.check_output([str(self.prefix / "bin" / name), *args], env=env, text=True))

    def test_both_channels_pin_remote_source_and_preserve_checkout_and_each_other(self):
        before = self.git(self.repo, "status", "--porcelain")
        self.install("main")
        main_binary = (self.prefix / "lib/taarof/main/bin/taarof-app").read_bytes()
        self.install("kmux")
        self.assertEqual((self.prefix / "lib/taarof/main/bin/taarof-app").read_bytes(), main_binary)
        self.assertEqual(self.git(self.repo, "status", "--porcelain"), before)
        self.assertEqual(self.git(self.repo, "branch", "--show-current"), "kmux")
        self.assertEqual(self.launch("taarof-app")["channel"], "main")
        self.assertIsNone(self.launch("taarof-app")["session"])
        dev = self.launch("taarof-app-kmux", "argument with spaces")
        self.assertEqual(dev["channel"], "kmux")
        self.assertEqual(dev["session"], "kmux")
        self.assertIsNone(dev["sock"])
        self.assertEqual(dev["installed"], str(self.prefix / "lib/taarof/kmux/bin/taarof-app"))
        self.assertEqual(dev["agent"], str(self.prefix / "lib/taarof/kmux/bin/agent"))
        self.assertEqual(dev["argv"], ["argument with spaces"])
        self.assertEqual(self.launch("taarof-kmux", "state")["argv"], ["--session", "kmux", "state"])
        desktops = list((self.prefix / "share/applications").glob("*.desktop"))
        self.assertEqual(len(desktops), 2)
        self.assertTrue(any("Taarof Development (kmux)" in p.read_text() for p in desktops))
        self.assertEqual(self.git(self.repo, "worktree", "list", "--porcelain").count("worktree "), 1)
        for name, sha in (("main", self.main_sha), ("kmux", self.kmux_sha)):
            manifest = json.loads((self.prefix / f"lib/taarof/{name}/share/taarof/install-manifest.json").read_text())
            self.assertEqual(manifest["source_revision"], sha)

    def test_private_remote_is_rejected_before_fetch(self):
        with self.assertRaisesRegex(RuntimeError, "public"):
            channel.install(self.repo, "kmux", self.prefix, self.cache)
        self.assertFalse(self.prefix.exists())

    def test_failed_build_preserves_existing_install(self):
        self.install("main")
        before = (self.prefix / "bin/taarof-app").read_bytes()
        channel.write_executable(self.tools / "cargo", "#!/bin/sh\nexit 42\n")
        with self.assertRaises(subprocess.CalledProcessError):
            self.install("main")
        self.assertEqual((self.prefix / "bin/taarof-app").read_bytes(), before)
        self.assertEqual(self.git(self.repo, "worktree", "list", "--porcelain").count("worktree "), 1)

    def test_wrong_provenance_is_rejected(self):
        binary = self.base / "binary"
        binary.write_bytes(b"fixture")
        Path(str(binary) + ".provenance.json").write_text(json.dumps(dict(source_revision="wrong", source_dirty=False)))
        with self.assertRaisesRegex(RuntimeError, "selected clean branch"):
            channel.verify_artifact(binary, self.main_sha)


if __name__ == "__main__":
    unittest.main()
