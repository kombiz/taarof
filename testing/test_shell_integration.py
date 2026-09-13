#!/usr/bin/env python3
"""Real interactive-shell OSC7/133 checks, using only private inert fixtures.

Optional TAAROF_TEST_SHELL_DIR selects an installed share/taarof/shell payload.
Every shell must be installed; missing dependencies fail rather than fake a pass.
"""

import os
import fcntl
from pathlib import Path
import pty
import select
import shlex
import shutil
import subprocess
import tempfile
import termios
import time
import unittest
from urllib.parse import unquote
import re

ROOT = Path(__file__).resolve().parents[1]


def read_until(fd, predicate):
    output = b""
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if select.select([fd], [], [], 0.1)[0]:
            try:
                output += os.read(fd, 65536)
            except OSError as error:
                raise AssertionError(
                    f"shell exited before markers: {output!r}"
                ) from error
            if predicate(output):
                return output
    raise AssertionError(f"shell did not emit expected inert markers: {output!r}")


class ShellIntegration(unittest.TestCase):
    def check_shell(self, shell, remote_copy):
        executable = shutil.which(shell)
        self.assertIsNotNone(
            executable, f"install {shell} for shell integration verification"
        )
        with tempfile.TemporaryDirectory(prefix="taarof-shell-test-") as temporary:
            home = Path(temporary)
            cwd = home / "inert space café"
            cwd.mkdir()
            payload = home / "remote-copy" if remote_copy else home / "shell"
            payload.mkdir()
            installed = os.environ.get("TAAROF_TEST_SHELL_DIR")
            names = [
                f"osc7.{shell}",
                "taarof-shell-integration.fish"
                if shell == "fish"
                else "taarof-shell-integration.sh",
            ]
            for name in names:
                source = (
                    Path(installed) / name
                    if installed
                    else ROOT
                    / (
                        "taarof-app/resources"
                        if name.startswith("osc7")
                        else "examples"
                    )
                    / name
                )
                shutil.copyfile(source, payload / name)
            # New HOME and minimal environment: no owner rc files or credentials.
            env = {
                "HOME": str(home),
                "PATH": os.environ["PATH"],
                "TERM": "dumb",
                "LANG": "C.UTF-8",
                "USER": "inert",
            }
            master, slave = pty.openpty()
            attrs = termios.tcgetattr(slave)
            attrs[3] &= ~termios.ECHO
            termios.tcsetattr(slave, termios.TCSANOW, attrs)
            args = {
                "bash": ["--noprofile", "--norc", "-i"],
                "zsh": ["-f", "-i"],
                "fish": ["--no-config", "-i"],
            }[shell]
            process = subprocess.Popen(
                [executable, *args],
                stdin=slave,
                stdout=slave,
                stderr=slave,
                env=env,
                cwd=home,
                start_new_session=True,
                preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0),
            )
            os.close(slave)
            try:
                if shell == "bash":
                    setup = "PS0=PRESERVED_PS0; PROMPT_COMMAND=('printf PRESERVED_HOOK'); trap ':' DEBUG"
                elif shell == "zsh":
                    setup = "autoload -Uz add-zsh-hook; inert_hook() { printf PRESERVED_HOOK; }; add-zsh-hook precmd inert_hook"
                else:
                    setup = "function inert_hook --on-event fish_prompt; printf PRESERVED_HOOK; end"
                commands = [setup]
                for _ in range(2):
                    commands += [
                        "source " + shlex.quote(str(payload / name)) for name in names
                    ]
                commands += ["printf READY_MARKER"]
                os.write(master, ("; ".join(commands) + "\n").encode())
                read_until(
                    master,
                    lambda data: (
                        b"READY_MARKER" in data
                        and b"]133;A" in data.split(b"READY_MARKER", 1)[1]
                    ),
                )
                os.write(
                    master,
                    ("cd " + shlex.quote(str(cwd)) + "; printf CWD_MARKER\n").encode(),
                )
                output = read_until(
                    master,
                    lambda data: (
                        b"CWD_MARKER" in data
                        and b"]133;A" in data.split(b"CWD_MARKER", 1)[1]
                        and b"PRESERVED_HOOK" in data.split(b"CWD_MARKER", 1)[1]
                    ),
                )
                # Wait for the entire prompt rather than observing a partial write.
                time.sleep(0.05)
                while select.select([master], [], [], 0)[0]:
                    output += os.read(master, 65536)
                prompt = output.split(b"CWD_MARKER", 1)[1]
                paths = re.findall(rb"\x1b\]7;file://[^/]*(.*?)(?:\x1b\\|\x07)", prompt)
                self.assertEqual(
                    len(paths), 1, "repeated sourcing must not duplicate CWD hooks"
                )
                self.assertEqual(unquote(paths[0].decode()), str(cwd))
                self.assertIn(b"%20", paths[0])
                self.assertIn(b"%C3%A9", paths[0].upper())
                self.assertEqual(prompt.count(b"]133;A"), 1)
                self.assertEqual(prompt.count(b"]666;vte.shell.precmd!"), 1)
                self.assertEqual(prompt.count(b"PRESERVED_HOOK"), 1)
                if shell == "bash":
                    self.assertIn(b"PRESERVED_PS0", output)
                    self.assertIn(b"]133;C", output)
                    os.write(master, b"trap -p DEBUG; printf TRAP_CHECK_DONE\\n\n")
                    trap = read_until(master, lambda data: b"TRAP_CHECK_DONE" in data)
                    self.assertIn(b"trap -- ':' DEBUG", trap)
            finally:
                process.terminate()
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                os.close(master)

    def test_source_and_remote_copy_real_shells(self):
        for shell in ("bash", "zsh", "fish"):
            for remote_copy in (False, True):
                with self.subTest(shell=shell, remote_copy=remote_copy):
                    self.check_shell(shell, remote_copy)


if __name__ == "__main__":
    unittest.main()
