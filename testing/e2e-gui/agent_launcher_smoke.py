#!/usr/bin/env python3
"""Installed launcher proof without real accounts, transcripts, or runtime control."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import time


class SmokeFailure(Exception):
    pass


def require(condition, category):
    if not condition:
        raise SmokeFailure(category)


def digest(path):
    with open(path, "rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def read_json(path):
    with open(path, "rb") as handle:
        data = handle.read(1024 * 1024 + 1)
    require(len(data) <= 1024 * 1024, "manifest_size_limit")
    return json.loads(data)


def identity(prefix, package, source, archive=None):
    relative = Path("share/taarof/bundle-manifest.json")
    manifest = read_json(package / relative)
    require(manifest == read_json(prefix / relative), "installed_bundle_manifest_mismatch")
    require(manifest.get("schema") == "taarof.bundle.v1", "unsupported_bundle_manifest")
    require(manifest.get("source_revision") == source, "bundle_source_mismatch")
    require(manifest.get("source_dirty") is False, "bundle_source_not_clean")
    artifacts = {}
    for name in ("agent", "taarof", "taarof-app"):
        installed = prefix / "bin" / name
        packaged = package / "bin" / name
        require(installed.is_file() and os.access(installed, os.X_OK), "installed_artifact_unavailable")
        actual = digest(installed)
        require(actual == digest(packaged), "installed_package_hash_mismatch")
        require(manifest["artifacts"][name]["sha256"] == actual, "bundle_artifact_hash_mismatch")
        artifacts[name] = {"installed_path": str(installed.resolve()), "sha256": actual}
    app_manifest = read_json(package / "share/taarof/install-manifest.json")
    require(app_manifest == read_json(prefix / "share/taarof/install-manifest.json"), "installed_app_manifest_mismatch")
    require(app_manifest.get("schema") == "taarof.artifact.v1" and app_manifest.get("source_dirty") is False, "app_source_not_clean_or_supported")
    require(app_manifest.get("source_revision") == source, "app_source_mismatch")
    require(app_manifest.get("binary_sha256") == artifacts["taarof-app"]["sha256"], "app_sidecar_hash_mismatch")
    require(app_manifest.get("build_id") == manifest.get("app_build_id"), "app_build_id_mismatch")
    require(bool(manifest.get("app_build_id")) and bool(manifest.get("agent_build_id")), "missing_build_identity")
    package_identity = {
        "directory": str(package.resolve()),
        "bundle_manifest_sha256": digest(package / relative),
        "source_revision": source,
        "app_build_id": manifest["app_build_id"],
        "agent_build_id": manifest["agent_build_id"],
    }
    if archive:
        with tarfile.open(archive, "r:gz") as handle:
            members = handle.getmembers()
            require(len(members) < 20000, "archive_member_limit")
            for name, artifact in artifacts.items():
                matches = [m for m in members if m.name.endswith("/bin/" + name)]
                require(len(matches) == 1 and matches[0].isfile(), "archive_artifact_ambiguous")
                with handle.extractfile(matches[0]) as stream:
                    require(hashlib.file_digest(stream, "sha256").hexdigest() == artifact["sha256"], "archive_artifact_hash_mismatch")
        package_identity["archive_sha256"] = digest(archive)
    return {"status": "pass", "package": package_identity, "installed": artifacts}


def runtime_identity(pid, expected_hash):
    if pid is None:
        return {"status": "not_observed", "reason": "no_explicit_runtime_pid"}
    root = Path("/proc") / str(pid)
    require(root.stat().st_uid == os.getuid(), "runtime_not_owned_by_current_user")
    # The start-time field distinguishes PID reuse; no cmdline or environment read.
    before = (root / "stat").read_text().rsplit(")", 1)[1].split()[19]
    executable = root / "exe"
    target = os.readlink(executable)
    require(Path(target.removesuffix(" (deleted)")).name == "taarof-app", "runtime_is_not_taarof_app")
    inode_before = executable.stat()
    actual = digest(executable)
    inode_after = executable.stat()
    require((inode_before.st_dev, inode_before.st_ino) == (inode_after.st_dev, inode_after.st_ino), "runtime_executable_changed")
    after = (root / "stat").read_text().rsplit(")", 1)[1].split()[19]
    require(before == after and os.readlink(executable) == target, "runtime_changed_during_observation")
    return {"status": "observed", "pid": pid, "start_time_ticks": before,
            "executable": target, "sha256": actual,
            "matches_installed_app": actual == expected_hash,
            "behavior": "not_tested"}


def run_command(agent, args, env, cwd):
    # Files cap in-memory capture; fixed commands and synthetic fixtures only.
    with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as error:
        child = subprocess.Popen([str(agent), *args], env=env, cwd=cwd,
                                 stdin=subprocess.DEVNULL, stdout=output,
                                 stderr=error, start_new_session=True)
        deadline = time.monotonic() + 20
        try:
            while child.poll() is None:
                require(time.monotonic() < deadline, "installed_command_deadline")
                require(os.fstat(output.fileno()).st_size <= 1024 * 1024 and os.fstat(error.fileno()).st_size <= 1024 * 1024, "installed_command_output_limit")
                time.sleep(0.01)
            output.seek(0)
            error.seek(0)
            out, err = output.read(1024 * 1024 + 1), error.read(1024 * 1024 + 1)
            require(len(out) <= 1024 * 1024 and len(err) <= 1024 * 1024, "installed_command_output_limit")
            return child.returncode, out, err
        finally:
            import signal
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()


ADAPTER = r'''
import json, os, sys, time
r=json.loads(sys.stdin.readline())
assert sys.stdin.read()==''
assert not set(os.environ)-set(['HOME','PATH','LANG','LC_CTYPE','XDG_CONFIG_HOME','XDG_DATA_HOME','XDG_STATE_HOME'])
identity=sys.argv[1]
if identity=='slow-fixture': time.sleep(10)
result={'protocol':1}
id="opaque 'quotes' and spaces"
cwd=os.path.join(os.environ['HOME'],"project 'quotes' and spaces")
if r['operation']=='metadata': result.update(id=identity,display_name=identity,capabilities=['new','resume'])
elif r['operation']=='probe': result.update(available=True)
elif r['operation']=='discover': result.update(sessions=[{'session_id':id,'title':'\x1b]0;HOSTILE_TITLE\x07\n\u202e'+'z'*10000,'cwd':cwd,'updated_at_unix_ms':1}])
elif r['operation'] in ('plan-new','plan-resume'):
    assert r['cwd']==cwd
    if r['operation']=='plan-resume': assert r['session_id']==id
    result.update(program=sys.executable,argv=[os.path.join(os.environ['HOME'],'fixture-launch.py'),r['operation'],r.get('session_id','')],cwd=cwd)
else: raise SystemExit(1)
print(json.dumps(result))
'''


def automated_smoke(agent):
    with tempfile.TemporaryDirectory(prefix="agent-installed-smoke-") as temporary:
        home = Path(temporary)
        cwd = home / "project 'quotes' and spaces"
        cwd.mkdir()
        runtime = home / "runtime"
        runtime.mkdir(mode=0o700)
        providers = home / ".config/agent/providers.d"
        providers.mkdir(parents=True)
        adapter = home / "fixture-adapter.py"
        adapter.write_text(ADAPTER)
        (home / "fixture-launch.py").write_text(
            "import os,sys\nassert os.path.basename(os.getcwd()) == \"project 'quotes' and spaces\"\n"
            "assert sys.argv[1] in ('plan-new','plan-resume')\n"
            "assert sys.argv[2] == (\"opaque 'quotes' and spaces\" if sys.argv[1]=='plan-resume' else '')\n"
            "print('SYNTHETIC_EXEC_OK')\n")
        env = {"HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config"),
               "XDG_RUNTIME_DIR": str(runtime), "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8",
               "SMOKE_ENV_SENTINEL": "SYNTHETIC_ENV_MUST_NOT_APPEAR"}
        # No Taarof discovery files or sockets can exist in this fresh root.
        for provider in ("synthetic-fixture", "slow-fixture"):
            (providers / (provider + ".toml")).write_text(
                "schema = 1\nid = " + json.dumps(provider) + "\ndisplay_name = " + json.dumps(provider)
                + "\nenabled = true\ncapabilities = ['new', 'resume']\ncommand = "
                + json.dumps([sys.executable, str(adapter), provider]) + "\n")
        # Private native transcript-like fixture must never leak through the v2 projection.
        history = home / ".codex/sessions"
        history.mkdir(parents=True)
        (history / "one.jsonl").write_text(json.dumps({"type": "session_meta", "payload": {"id": "native-fixture", "cwd": str(cwd)}}) + "\n" + json.dumps({"type": "response_item", "payload": {"role": "user", "content": [{"type": "input_text", "text": "SYNTHETIC_PROMPT_MUST_NOT_APPEAR"}]}}) + "\n")
        outputs = []
        def run(args):
            code, out, err = run_command(agent, args, env, cwd)
            outputs.extend((out, err))
            require(code == 0, "installed_command_failed")
            return out
        catalog = json.loads(run(["--json"]))
        require(catalog.get("schema") == "agent.sessions.v2", "catalog_schema_mismatch")
        rows = catalog["sessions"]
        require(any(row["stable_ref"]["provider_id"] == "synthetic-fixture" for row in rows), "synthetic_provider_absent")
        require(any(row["stable_ref"]["provider_id"] == "codex" for row in rows), "local_history_lost")
        require(any(status["name"] == "slow-fixture" and not status["ok"] for status in catalog["providers"]), "timeout_not_isolated")
        for row in rows:
            for value in row["display"].values():
                if isinstance(value, str):
                    require(len(value) <= 256 and not any(ord(c) < 32 or c in '\u202e\u2066' for c in value), "hostile_display_not_inert")
        run(["doctor"])
        for args in (["--new", "synthetic-fixture"], ["--resume", "opaque 'quotes' and spaces"]):
            require(run(args).strip() == b"SYNTHETIC_EXEC_OK", "structured_execution_mismatch")
        manifest = providers / "synthetic-fixture.toml"
        manifest.write_text(manifest.read_text().replace("enabled = true", "enabled = false"))
        disabled = json.loads(run(["--json"]))
        require(not any(row["stable_ref"]["provider_id"] == "synthetic-fixture" for row in disabled["sessions"]), "disabled_provider_remained")
        for data in outputs:
            require(not any(marker in data for marker in (b"SYNTHETIC_PROMPT_MUST_NOT_APPEAR", b"SYNTHETIC_ENV_MUST_NOT_APPEAR", b"HOSTILE_TITLE")), "private_fixture_leaked")
    return {name: "pass" for name in (
        "hostile_display_inert", "quoted_cwd_and_session_execution", "catalog_diagnostic_privacy",
        "external_timeout_isolation", "disabled_manifest_removal", "isolated_no_taarof_local_fallback")}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", type=Path, required=True, help="explicit installed prefix; never installed by this script")
    parser.add_argument("--package-dir", type=Path, required=True, help="extracted supported release bundle")
    parser.add_argument("--package-archive", type=Path)
    parser.add_argument("--source-sha", required=True, help="expected immutable reviewed source commit")
    parser.add_argument("--receipt", type=Path, required=True, help="new JSON receipt path; existing files are not overwritten")
    parser.add_argument("--runtime-pid", type=int, help="optional own-user taarof-app PID; observe identity only")
    args = parser.parse_args()
    require(re.fullmatch(r"[0-9a-f]{40}", args.source_sha), "source_sha_must_be_full_commit")
    require(args.runtime_pid is None or args.runtime_pid > 1, "invalid_runtime_pid")
    receipt = {"schema": "agent.release-smoke.v1", "expected_source_revision": args.source_sha,
               "observed_at_unix": int(time.time()), "automated_status": "failed",
               "identity": {"status": "not_checked"},
               "running_taarof": {"status": "not_observed"},
               "review": {"status": "pending", "source_revision": args.source_sha},
               "release_status": "pending_attended_proof", "attended": {
                   name: "pending" for name in ("primary_provider_new_resume", "live_tmux_attach",
                   "remote_degradation", "operator_taarof_stop_local_fallback_restart")},
               "gates": {name: "not_recorded" for name in ("ci", "plan_validation", "package_validation", "public_export", "headless_gui")}}
    # Reserve before any subprocess; never overwrite an operator's prior receipt.
    with args.receipt.open("x") as output:
        status = 0
        try:
            receipt["identity"] = identity(args.prefix.resolve(), args.package_dir.resolve(), args.source_sha, args.package_archive)
            with tempfile.TemporaryDirectory(prefix="agent-identity-smoke-") as home:
                code, out, _ = run_command((args.prefix / "bin/agent").resolve(), ["--build-info"], {"HOME": home, "XDG_RUNTIME_DIR": home, "PATH": "/usr/bin:/bin"}, home)
                require(code == 0, "agent_build_identity_unavailable")
                build = json.loads(out)
                require(build.get("schema") == "agent.build.v1" and build.get("source_revision") == args.source_sha and build.get("source_dirty") is False and build.get("build_id") == receipt["identity"]["package"]["agent_build_id"], "agent_build_identity_mismatch")
                receipt["identity"]["installed"]["agent"]["source_revision"] = build["source_revision"]
                receipt["identity"]["installed"]["agent"]["build_id"] = build["build_id"]
            receipt["running_taarof"] = runtime_identity(args.runtime_pid, receipt["identity"]["installed"]["taarof-app"]["sha256"])
            receipt["automated_checks"] = automated_smoke((args.prefix / "bin/agent").resolve())
            receipt["automated_status"] = "pass"
        except (SmokeFailure, OSError, ValueError, KeyError, TypeError, tarfile.TarError) as error:
            # Never copy arbitrary subprocess output, file contents, argv, or env.
            receipt["failure_category"] = str(error) if isinstance(error, SmokeFailure) else "artifact_or_protocol_unavailable"
            status = 1
        json.dump(receipt, output, indent=2, sort_keys=True)
        output.write("\n")
    print("Automated smoke " + ("passed; attended release proof remains pending." if status == 0 else "failed; see receipt category."))
    return status


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (SmokeFailure, OSError) as error:
        print("agent smoke: " + (str(error) if isinstance(error, SmokeFailure) else "receipt unavailable"), file=sys.stderr)
        raise SystemExit(2)
