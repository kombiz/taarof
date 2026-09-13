#!/usr/bin/env python3
"""Bind the three shipped programs to verified compile-time provenance, including explicitly dirty builds."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys


class ProvenanceError(Exception):
    pass


def read_json(path):
    value = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ProvenanceError("provenance must be a JSON object")
    return value


def digest(path):
    with Path(path).open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def require(condition, message):
    if not condition:
        raise ProvenanceError(message)


def app_identity(args):
    app = read_json(args.app_sidecar)
    require(app.get("schema") == "taarof.artifact.v1", "application sidecar schema mismatch")
    require(app.get("binary_sha256") == digest(args.app), "application sidecar hash mismatch")
    require(re.fullmatch(r"[0-9a-f]{40}", str(app.get("source_revision", ""))),
            "application source revision is unavailable")
    require(type(app.get("source_dirty")) is bool, "application source dirty state is unavailable")
    build_id = app.get("build_id")
    require(isinstance(build_id, str) and bool(build_id), "application build ID is unavailable")
    require(build_id.encode() in Path(args.app).read_bytes(), "application build ID is not embedded")
    return app


def agent_identity(args, app):
    binary = Path(args.agent).read_bytes()
    require(app["build_id"].encode() in binary,
            "launcher embedded build identity is unavailable")
    result = subprocess.run([str(Path(args.agent).resolve()), "--build-info"],
                            env={}, capture_output=True, timeout=10, check=False)
    require(result.returncode == 0, "launcher build identity is unavailable")
    require(len(result.stdout) <= 16_384 and len(result.stderr) <= 16_384,
            "launcher build identity exceeds its bound")
    agent = json.loads(result.stdout)
    require(isinstance(agent, dict) and agent.get("schema") == "agent.build.v1",
            "launcher build identity schema mismatch")
    require(agent.get("source_revision") == app["source_revision"]
            and agent.get("source_dirty") is app["source_dirty"], "launcher/application source mismatch")
    require(agent.get("profile") == app.get("profile") == "release",
            "bundle requires release-profile artifacts")
    require(isinstance(app.get("built_at_unix"), str) and app["built_at_unix"].isdigit()
            and agent.get("built_at_unix") == app["built_at_unix"],
            "launcher/application build epoch mismatch")
    build_id = agent.get("build_id")
    require(isinstance(build_id, str) and bool(build_id), "launcher build ID is unavailable")
    require(build_id.encode() in Path(args.agent).read_bytes(), "launcher build ID is not embedded")
    return agent


def artifact_hashes(args):
    return {name: {"sha256": digest(path)} for name, path in
            (("taarof-app", args.app), ("taarof", args.cli), ("agent", args.agent))}


def create(args):
    app = app_identity(args)
    agent = agent_identity(args, app)
    # Dirty bundles remain installable but cannot claim exact committed CLI source.
    if not app["source_dirty"]:
        result = subprocess.run(
            ["git", "-C", args.source_root, "show", f"{app['source_revision']}:taarof-cli/taarof"],
            capture_output=True, check=False,
        )
        require(result.returncode == 0, "CLI source blob is unavailable at the compiled revision")
        require(hashlib.sha256(result.stdout).hexdigest() == digest(args.cli),
                "CLI bytes differ from the compiled source revision")
    manifest = {
        "schema": "taarof.bundle.v1",
        "source_revision": app["source_revision"],
        "source_dirty": app["source_dirty"],
        "app_build_id": app["build_id"],
        "agent_build_id": agent["build_id"],
        "artifacts": artifact_hashes(args),
    }
    Path(args.output).write_text(json.dumps(manifest, sort_keys=True, separators=(",", ":")) + "\n",
                                 encoding="utf-8")


def verify(args):
    manifest = read_json(args.manifest)
    app = app_identity(args)
    require(manifest.get("schema") == "taarof.bundle.v1", "bundle manifest schema mismatch")
    require(manifest.get("source_revision") == app["source_revision"]
            and manifest.get("source_dirty") is app["source_dirty"], "bundle/application source mismatch")
    require(manifest.get("app_build_id") == app["build_id"], "bundle/application build mismatch")
    require(manifest.get("artifacts") == artifact_hashes(args), "bundle artifact hash mismatch")
    agent = agent_identity(args, app)
    require(manifest.get("agent_build_id") == agent["build_id"], "bundle/launcher build mismatch")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("create", "verify"))
    for field in ("app", "app-sidecar", "agent", "cli"):
        parser.add_argument("--" + field, required=True)
    parser.add_argument("--source-root")
    parser.add_argument("--output")
    parser.add_argument("--manifest")
    args = parser.parse_args()
    if args.operation == "create":
        require(args.source_root and args.output, "create requires source root and output")
        create(args)
    else:
        require(args.manifest, "verify requires a manifest")
        verify(args)


if __name__ == "__main__":
    try:
        main()
    except (ProvenanceError, OSError, ValueError, subprocess.SubprocessError) as error:
        # Never print artifact content, subprocess output, or an environment value.
        detail = str(error) if isinstance(error, ProvenanceError) else "provenance input unavailable"
        print("bundle provenance: " + detail, file=sys.stderr)
        sys.exit(1)
