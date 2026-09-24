#!/usr/bin/env python3
"""Focused provenance and failure-diagnostic tests for the performance recorder."""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "record_performance_baseline",
    ROOT / "testing/record-performance-baseline.py",
)
baseline = importlib.util.module_from_spec(spec)
spec.loader.exec_module(baseline)


class PerformanceBaselineTests(unittest.TestCase):
    def test_source_identity_comes_from_the_checked_out_repository(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            (repo / "fixture").write_text("clean\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(repo), "add", "fixture"], check=True)
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(repo),
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "-qm",
                    "fixture",
                ],
                check=True,
            )
            expected = subprocess.check_output(
                ["git", "-C", str(repo), "rev-parse", "HEAD"],
                text=True,
            ).strip()

            self.assertEqual(baseline.source_identity(repo), (expected, False))
            (repo / "fixture").write_text("dirty\n", encoding="utf-8")
            self.assertEqual(baseline.source_identity(repo), (expected, True))

    def test_git_failure_reports_bounded_stderr_without_stdout_or_arguments(self):
        with tempfile.TemporaryDirectory() as directory:
            tools = Path(directory)
            git = tools / "git"
            git.write_text(
                "#!/bin/sh\n"
                "printf 'captured stdout must stay private\\n'\n"
                "printf 'fatal: detected dubious ownership\\nat synthetic checkout\\n' >&2\n"
                "exit 128\n",
                encoding="utf-8",
            )
            git.chmod(0o755)
            environment = {"PATH": str(tools) + os.pathsep + os.environ["PATH"]}

            with patch.dict(os.environ, environment), self.assertRaises(RuntimeError) as raised:
                baseline.command_output("git", "rev-parse", "HEAD", cwd=tools)

            message = str(raised.exception)
            self.assertEqual(
                message,
                "git exited with status 128: fatal: detected dubious ownership at synthetic checkout",
            )
            self.assertNotIn("captured stdout", message)
            self.assertNotIn("rev-parse", message)


if __name__ == "__main__":
    unittest.main()
