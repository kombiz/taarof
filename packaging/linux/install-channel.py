#!/usr/bin/env python3
"""Build a pinned public branch without switching or installing the caller's tree."""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile


REPOSITORY = "https://github.com/kombiz/taarof.git"
REMOTES = {REPOSITORY, "git@github.com:kombiz/taarof.git", "ssh://git@github.com/kombiz/taarof.git"}
APP_ID = "io.github.kombiz.taarof"


def run(*args: str, cwd: Path, env: dict[str, str] | None = None) -> None:
    subprocess.run(args, cwd=cwd, env=env, check=True)


def git(root: Path, *args: str) -> str:
    return subprocess.check_output(["git", *args], cwd=root, text=True).strip()


def build_environment(target: Path) -> dict[str, str]:
    env = os.environ.copy()
    # Never let a caller's artifact override install an unrelated binary.
    for key in list(env):
        if key.startswith(("TAAROF_INSTALL_", "TAAROF_BUILD_")) or key == "TAAROF_ARTIFACT_SIDECAR":
            del env[key]
    env["CARGO_TARGET_DIR"] = str(target)
    return env


def verify_artifact(binary: Path, revision: str) -> None:
    manifest = json.loads(Path(str(binary) + ".provenance.json").read_text())
    if (manifest.get("source_revision") != revision
            or manifest.get("source_dirty") is not False
            or manifest.get("binary_sha256") != hashlib.sha256(binary.read_bytes()).hexdigest()):
        raise RuntimeError("built artifact does not match the selected clean branch revision")


def write_executable(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    # Replace the directory entry so a running launcher is not truncated.
    fd, temporary = tempfile.mkstemp(prefix=f".{path.name}-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as handle:
            handle.write(text)
        os.chmod(temporary, 0o755)
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def launcher(payload: Path, channel: str, program: str) -> str:
    session = "export TAAROF_SESSION=kmux\n" if channel == "kmux" else "unset TAAROF_SESSION\n"
    # Drop inherited control targets so the channel CLI cannot address another app.
    reset = "unset TAAROF_SOCK TAAROF_HTTP TAAROF_HTTP_TOKEN\n"
    app = shlex.quote(str(payload / "bin/taarof-app"))
    agent = shlex.quote(str(payload / "bin/agent"))
    web = shlex.quote(str(payload / "share/taarof/web"))
    command = shlex.quote(str(payload / "bin" / program))
    session_arg = " --session kmux" if channel == "kmux" and program == "taarof" else ""
    return ("#!/bin/sh\n" + session + reset
            + f"export TAAROF_APP_BIN={app}\nexport TAAROF_INSTALLED_BINARY={app}\nexport TAAROF_AGENT_BINARY={agent}\nexport TAAROF_WEB_DIST_DIR={web}\n"
            + f'exec {command}{session_arg} "$@"\n')


def desktop_exec(path: Path) -> str:
    # Desktop Entry Exec quoting differs from shell quoting; % introduces field codes.
    escaped = str(path).replace("\\", "\\\\").replace('"', '\\"').replace("`", "\\`").replace("$", "\\$").replace("%", "%%")
    return f'"{escaped}"'


def publish_launchers(prefix: Path, payload: Path, channel: str) -> None:
    suffix = "-kmux" if channel == "kmux" else ""
    for program in ("taarof-app", "taarof", "agent"):
        if (payload / "bin" / program).is_file():
            write_executable(prefix / "bin" / (program + suffix), launcher(payload, channel, program))
    app_id = APP_ID
    if channel == "kmux":
        app_id += ".kmux_" + hashlib.sha256(b"kmux").hexdigest()[:32]
    name = "Taarof Development (kmux)" if channel == "kmux" else "Taarof"
    executable = prefix / "bin" / ("taarof-app" + suffix)
    desktop = prefix / "share/applications" / (app_id + ".desktop")
    desktop.parent.mkdir(parents=True, exist_ok=True)
    desktop.write_text(
        "[Desktop Entry]\nVersion=1.0\nType=Application\n"
        f"Name={name}\nExec={desktop_exec(executable)}\n"
        f"Icon={app_id}\nTerminal=false\nCategories=System;TerminalEmulator;GTK;\n"
        f"StartupNotify=true\nStartupWMClass={app_id}\n"
    )
    icon = prefix / "share/icons/hicolor/scalable/apps" / (app_id + ".svg")
    icon.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(payload / "share/icons/hicolor/scalable/apps" / (APP_ID + ".svg"), icon)
    if shutil.which("update-desktop-database"):
        run("update-desktop-database", str(desktop.parent), cwd=prefix)


def install(root: Path, channel: str, prefix: Path, cache: Path) -> None:
    if git(root, "remote", "get-url", "origin") not in REMOTES:
        raise RuntimeError(f"origin must be the public {REPOSITORY}")
    cache.mkdir(parents=True, exist_ok=True)
    # One build/install per channel and shared Git store; different channels may build together.
    with (cache / f"{channel}.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        remote_ref = f"refs/remotes/origin/{channel}"
        run("git", "fetch", "origin", f"refs/heads/{channel}:{remote_ref}", cwd=root)
        revision = git(root, "rev-parse", remote_ref + "^{commit}")
        print(f"Installing {REPOSITORY} {channel} at {revision}", flush=True)
        scratch = Path(tempfile.mkdtemp(prefix=f"{channel}-", dir=cache))
        source = scratch / "source"
        run("git", "worktree", "add", "--detach", str(source), revision, cwd=root)
        try:
            env = build_environment(cache / f"target-{channel}")
            run("npm", "--prefix", "taarof-web", "ci", cwd=source, env=env)
            run("npm", "--prefix", "taarof-web", "run", "build", cwd=source, env=env)
            for crate in ("taarof-app", "agent-launcher"):
                if (source / crate / "Cargo.toml").exists():
                    run("cargo", "build", "--locked", "--release", "--manifest-path", f"{crate}/Cargo.toml", cwd=source, env=env)
            binary = Path(env["CARGO_TARGET_DIR"]) / "release/taarof-app"
            run("bash", "packaging/linux/emit-artifact-provenance.sh", str(binary), cwd=source, env=env)
            verify_artifact(binary, revision)
            if git(source, "status", "--porcelain", "--untracked-files=all"):
                raise RuntimeError("build modified the source worktree; preserving it for inspection")
            payload = prefix / "lib/taarof" / channel
            # The current production branch predates CARGO_TARGET_DIR-aware packaging.
            env["TAAROF_INSTALL_BINARY"] = str(binary)
            env["TAAROF_INSTALL_PROVENANCE"] = str(binary) + ".provenance.json"
            env["TAAROF_INSTALL_AGENT_BINARY"] = str(binary.parent / "agent")
            run("bash", "packaging/linux/install-local.sh", str(payload), cwd=source, env=env)
            publish_launchers(prefix, payload, channel)
            print(f"Installed {channel}: {prefix / 'bin' / ('taarof-app-kmux' if channel == 'kmux' else 'taarof-app')}")
            print("The running app was not restarted. Launch the installed channel when ready.")
        finally:
            # Do not force-remove unexpected source changes, even after a failed build.
            if not git(source, "status", "--porcelain", "--untracked-files=all"):
                run("git", "worktree", "remove", str(source), cwd=root)
                scratch.rmdir()
            else:
                print(f"Preserved modified build worktree: {source}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("channel", nargs="?", choices=("main", "kmux"), default="main")
    parser.add_argument("--prefix", type=Path, default=Path.home() / ".local")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    # Outside the repository and separate from the user's working trees.
    cache = Path(os.environ.get("XDG_CACHE_HOME", str(Path.home() / ".cache"))) / "taarof-install"
    install(root, args.channel, args.prefix.expanduser().resolve(), cache.resolve())


if __name__ == "__main__":
    main()
