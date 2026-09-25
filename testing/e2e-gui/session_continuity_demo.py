#!/usr/bin/env python3
"""Synthetic native proof for layout reopen, provider resume, and exact tmux reattach."""
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import time

HOME = Path("/home/laddy")
EVIDENCE = Path("/evidence")
APP = Path("/fixture/taarof-app")
AGENT = Path("/fixture/agent")
SESSION = "taarof--continuity-demo--t1--0"
NONCE_A = "11111111111111111111111111111111"
NONCE_B = "22222222222222222222222222222222"


def run(*args, check=True, capture=True, env=None):
    return subprocess.run(args, check=check, text=True,
                          stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
                          stderr=subprocess.PIPE if capture else subprocess.DEVNULL,
                          env=env)


def sha256(path):
    with path.open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def tmux_identity():
    value = run("tmux", "display-message", "-t", SESSION, "-p",
                "#{session_id}|#{session_created}|#{@taarof-continuity-id}").stdout.strip()
    session_id, created, nonce = value.split("|", 2)
    return {"session_id": session_id, "session_created": int(created), "continuity_id": nonce}


def tmux_attachment_check():
    # list-clients exits 1 when no clients exist, which is ambiguous with a
    # failed query. session_attached is a target-specific status query: exit 0
    # proves the saved-name replacement still exists while the value proves
    # whether the app attached a client to it.
    result = run("tmux", "display-message", "-t", SESSION, "-p",
                 "#{session_attached}", check=False)
    count = int(result.stdout.strip()) if result.returncode == 0 else None
    return result, count


def tmux_clients():
    result, count = tmux_attachment_check()
    return count if result.returncode == 0 else None


def write_provider_fixture():
    config = HOME / ".config"
    providers = config / "agent/providers.d"
    providers.mkdir(parents=True, exist_ok=True)
    (config / "taarof").mkdir(parents=True, exist_ok=True)
    (config / "taarof/config.toml").write_text(
        "[session]\nauto_resume_agents = true\n[http]\nenabled = false\n"
    )
    adapter = HOME / "adapter.py"
    adapter.write_text("""import json,sys
r=json.loads(sys.stdin.readline()); op=r['operation']; out={'protocol':1}
if op=='metadata': out.update(id='continuity-fixture',display_name='Continuity Fixture',capabilities=['new','resume'])
elif op=='probe': out.update(available=True)
elif op=='discover': out.update(sessions=[{'session_id':'synthetic-session-28','title':'Synthetic continuity demo','cwd':'/home/laddy/demo','updated_at_unix_ms':1}])
elif op=='plan-resume':
 assert r['session_id']=='synthetic-session-28'
 out.update(program='/usr/bin/python3',argv=['/home/laddy/provider_resume.py'],cwd='/home/laddy/demo')
else: raise SystemExit(2)
print(json.dumps(out))
""")
    (HOME / "provider_resume.py").write_text(
        "from pathlib import Path\nimport time\nmarker=Path('/home/laddy/resume-observed')\nfirst=not marker.exists()\nmarker.write_text('exact synthetic provider identity')\nprint('RESUME AGENT CONVERSATION: synthetic provider identity accepted', flush=True)\nprint('DISPLAY CHECKPOINT: visual context only; it does not prove liveness', flush=True)\nif first: time.sleep(300)\n"
    )
    (providers / "continuity-fixture.toml").write_text(
        "schema = 1\nid = 'continuity-fixture'\ndisplay_name = 'Continuity Fixture'\n"
        "enabled = true\ncapabilities = ['new', 'resume']\n"
        "command = ['/usr/bin/python3', '/home/laddy/adapter.py']\n"
    )
    (HOME / "demo").mkdir()


def saved_layout(identity):
    leaf_agent = {
        "type": "leaf", "work_origin": "pane-agent", "cwd": "/home/laddy/demo",
        "agent_session": {"agent_name": "continuity-fixture",
                          "session_id": "synthetic-session-28",
                          "host_identity": os.uname().nodename.lower().rstrip("."),
                          "source": "argv"},
    }
    leaf_tmux = {
        "type": "leaf", "work_origin": "pane-tmux", "cwd": "/home/laddy/demo",
        "tmux_session": SESSION, "tmux_identity": identity,
    }
    return {
        "version": 2,
        "workspaces": [{
            "id": 1, "work_origin": "workspace-demo", "name": "Continuity demo",
            "tabs": [{"name": "Three explicit actions", "work_origin": "tab-demo",
                      "cwd": "/home/laddy/demo",
                      "panes": {"type": "split", "direction": "horizontal", "ratio": 0.5,
                                "first": leaf_agent, "second": leaf_tmux}}],
            "active_tab_index": 0,
        }],
        "active_workspace_index": 0, "window_width": 1180, "window_height": 760,
    }


def app_environment():
    return {
        "HOME": str(HOME), "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8",
        "DISPLAY": os.environ["DISPLAY"], "DBUS_SESSION_BUS_ADDRESS": os.environ["DBUS_SESSION_BUS_ADDRESS"],
        "XDG_CONFIG_HOME": str(HOME / ".config"), "XDG_DATA_HOME": str(HOME / ".local/share"),
        "XDG_STATE_HOME": str(HOME / ".local/state"), "XDG_RUNTIME_DIR": "/tmp/runtime",
        "TMUX_TMPDIR": "/tmp/tmux", "TAAROF_AGENT_BINARY": str(AGENT),
        "TAAROF_INSTALLED_BINARY": str(APP),
    }


def start_app(env):
    return subprocess.Popen([str(APP)], env=env, cwd=HOME / "demo",
                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                            stderr=(EVIDENCE / "native.stderr").open("ab"), start_new_session=True)


def stop_app(process):
    if process.poll() is None:
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def screenshot(name, env):
    run("import", "-display", env["DISPLAY"], "-window", "root", str(EVIDENCE / name),
        check=False, env=env)


def wait_until(predicate, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.1)
    return False


def taarof_window_mapped(env):
    result = run("xdotool", "search", "--onlyvisible", "--name", "taarof", check=False, env=env)
    return result.returncode == 0 and bool(result.stdout.strip())


def main():
    (HOME / ".local/share/taarof").mkdir(parents=True)
    Path("/tmp/runtime").mkdir(mode=0o700, exist_ok=True)
    Path("/tmp/tmux").mkdir(mode=0o700, exist_ok=True)
    os.environ["TMUX_TMPDIR"] = "/tmp/tmux"
    write_provider_fixture()
    run("tmux", "new-session", "-d", "-s", SESSION, "-c", str(HOME / "demo"),
        "printf 'REATTACH LIVE TERMINAL: exact synthetic generation accepted\\n'; sleep 300")
    run("tmux", "set-option", "-t", SESSION, "@taarof-continuity-id", NONCE_A)
    original = tmux_identity()
    state = HOME / ".local/share/taarof/session.json"
    pristine = json.dumps(saved_layout(original), indent=2) + "\n"
    state.write_text(pristine)
    env = app_environment()
    app_build_result = run(str(APP), "--build-info", env=env)
    agent_build_result = run(str(AGENT), "--build-info", env=env)
    app_build = json.loads(app_build_result.stdout)
    agent_build = json.loads(agent_build_result.stdout)

    first = start_app(env)
    first_window_mapped = wait_until(lambda: taarof_window_mapped(env))
    resume_observed = wait_until(lambda: (HOME / "resume-observed").exists())
    exact_attach_observed = wait_until(lambda: tmux_clients() == 1)
    original_attachment_result, original_attached_count = tmux_attachment_check()
    time.sleep(1)
    screenshot("original-generation.png", env)
    first_exe = os.readlink(f"/proc/{first.pid}/exe") if first.poll() is None else None
    stop_app(first)
    parent_exe_missing_after_exit = not Path(f"/proc/{first.pid}/exe").exists()
    survived_result = run("tmux", "has-session", "-t", SESSION, check=False)
    tmux_survived_desktop = survived_result.returncode == 0

    state.write_text(pristine)
    run("tmux", "kill-server")
    run("tmux", "new-session", "-d", "-s", SESSION, "-c", str(HOME / "demo"),
        "printf 'REPLACEMENT MUST NOT ATTACH\\n'; sleep 300")
    run("tmux", "set-option", "-t", SESSION, "@taarof-continuity-id", NONCE_B)
    replacement = tmux_identity()
    second = start_app(env)
    second_window_mapped = wait_until(lambda: taarof_window_mapped(env))
    time.sleep(1)
    replacement_attachment_result, replacement_attached_count = tmux_attachment_check()
    replacement_rejected = (
        replacement_attachment_result.returncode == 0
        and replacement_attached_count == 0
        and replacement["continuity_id"] != original["continuity_id"]
    )
    screenshot("replacement-generation.png", env)
    stop_app(second)
    run("tmux", "kill-server", check=False)
    lost_result = run("tmux", "has-session", "-t", SESSION, check=False)
    tmux_loss_observed = lost_result.returncode != 0

    receipt = {
        "schema": "taarof.session-continuity-demo.v1",
        "app_sha256": sha256(APP), "agent_sha256": sha256(AGENT),
        "app_embedded_identity": app_build,
        "agent_embedded_identity": agent_build,
        "agent_source_revision": agent_build.get("source_revision"),
        "agent_source_dirty": agent_build.get("source_dirty"),
        "embedded_channel_match": (
            app_build.get("build", {}).get("source_revision") == agent_build.get("source_revision")
            and app_build.get("build", {}).get("source_dirty") == agent_build.get("source_dirty")
        ),
        "running_app_executable": first_exe,
        "parent_executable_missing_after_exit": parent_exe_missing_after_exit,
        "first_window_mapped_before_screenshot": first_window_mapped,
        "replacement_window_mapped_before_screenshot": second_window_mapped,
        "home_inside_fixture": str(HOME), "synthetic_history_only": True,
        "http_enabled": False, "browser_profile_created": False,
        "original_tmux_identity": original, "replacement_tmux_identity": replacement,
        "resume_agent_conversation_observed": resume_observed,
        "reattach_live_terminal_observed": exact_attach_observed,
        "original_tmux_attached_count": original_attached_count,
        "replacement_tmux_attached_count": replacement_attached_count,
        "replacement_generation_rejected": replacement_rejected,
        "tmux_survived_desktop_shutdown": tmux_survived_desktop,
        "tmux_loss_observed": tmux_loss_observed,
        "reopen_workspace_layout_observation": "screenshots captured; owner review required",
        "owner_attended_status": "pending",
        "command_exits": {
            "app_build_info": app_build_result.returncode,
            "agent_build_info": agent_build_result.returncode,
            "original_tmux_attachment_query": original_attachment_result.returncode,
            "replacement_tmux_attachment_query": replacement_attachment_result.returncode,
            "tmux_has_session_after_desktop_shutdown": survived_result.returncode,
            "tmux_has_session_after_server_loss": lost_result.returncode,
        },
    }
    receipt["automated_status"] = "pass" if all([
        resume_observed, exact_attach_observed, replacement_rejected,
        tmux_survived_desktop, tmux_loss_observed, parent_exe_missing_after_exit,
        receipt["embedded_channel_match"], first_window_mapped, second_window_mapped,
    ]) else "fail"
    (EVIDENCE / "receipt.json").write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n")
    print("Continuity demo automated checks complete; owner screenshot observation remains pending.")
    return 0 if receipt["automated_status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
