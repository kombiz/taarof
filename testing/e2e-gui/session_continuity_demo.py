#!/usr/bin/env python3
"""Synthetic native proof for layout reopen, provider resume, and exact tmux reattach."""
import hashlib
import base64
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import time

HOME = Path("/home/laddy")
EVIDENCE = Path("/evidence")
APP = Path("/fixture/taarof-app")
AGENT = Path("/fixture/agent")
SESSION = "taarof--continuity-demo--t1--0"
NONCE_A = "11111111111111111111111111111111"
NONCE_B = "22222222222222222222222222222222"
HTTP_PORT = 17828
TMUX_MARKER = "REATTACH LIVE TERMINAL: exact synthetic generation accepted"


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


def query_native_state():
    registry = json.loads((Path("/tmp/runtime") / "taarof-current.json").read_text())
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.connect(registry["socket_path"])
        client.sendall(b'{"action":"query-state"}')
        client.shutdown(socket.SHUT_WR)
        response = bytearray()
        while chunk := client.recv(65536):
            response.extend(chunk)
    return json.loads(response)["data"]


def replacement_unavailable_state():
    try:
        state = query_native_state()
    except (FileNotFoundError, ConnectionError, json.JSONDecodeError, KeyError, OSError):
        return False
    return any(
        pane.get("tmux_session") == SESSION
        and pane.get("shell_running") is False
        and pane.get("attach_supported") is False
        and pane.get("attach_unavailable_reason")
        == "Reattach unavailable: the exact saved tmux target no longer exists."
        for workspace in state.get("workspaces", [])
        for tab in workspace.get("tabs", [])
        for pane in tab.get("panes", [])
    )


def failed_resume_retained_state():
    try:
        state = query_native_state()
    except (FileNotFoundError, ConnectionError, json.JSONDecodeError, KeyError, OSError):
        return False
    return any(
        pane.get("shell_running") is False
        and pane.get("attach_supported") is False
        and str(pane.get("attach_unavailable_reason", "")).startswith(
            "Resume agent conversation failed (exit status 2)"
        )
        for workspace in state.get("workspaces", [])
        for tab in workspace.get("tabs", [])
        for pane in tab.get("panes", [])
    )


def exact_tmux_viewer_ready():
    try:
        state = query_native_state()
    except (FileNotFoundError, ConnectionError, json.JSONDecodeError, KeyError, OSError):
        return False
    return any(
        pane.get("tmux_session") == SESSION
        and pane.get("attach_supported") is True
        and pane.get("attach_kind") == "tmux"
        for workspace in state.get("workspaces", [])
        for tab in workspace.get("tabs", [])
        for pane in tab.get("panes", [])
    )


def tmux_pane_coordinates():
    state = query_native_state()
    for workspace in state.get("workspaces", []):
        for tab in workspace.get("tabs", []):
            for pane in tab.get("panes", []):
                if pane.get("tmux_session") == SESSION:
                    return tab["tab_id"], pane["pane_id"]
    raise RuntimeError("synthetic tmux pane was not projected")


def http_ready(process):
    token_file = Path("/tmp/runtime") / f"taarof-http-{process.pid}.token"
    if not token_file.is_file():
        return False
    try:
        with socket.create_connection(("127.0.0.1", HTTP_PORT), timeout=0.2):
            return True
    except OSError:
        return False


def websocket_checkpoint(process, tab_id, pane_id):
    # The bearer token exists only in the fixture runtime and this local
    # variable. It is never copied into evidence, argv, a profile, or logs.
    token = (Path("/tmp/runtime") / f"taarof-http-{process.pid}.token").read_text().strip()
    key = base64.b64encode(os.urandom(16)).decode()
    request = (
        f"GET /api/v1/tabs/{tab_id}/panes/{pane_id}/attach?token={token} HTTP/1.1\r\n"
        f"Host: 127.0.0.1:{HTTP_PORT}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    ).encode()
    with socket.create_connection(("127.0.0.1", HTTP_PORT), timeout=5) as client:
        client.sendall(request)
        response = bytearray()
        while b"\r\n\r\n" not in response:
            chunk = client.recv(4096)
            if not chunk:
                raise RuntimeError("websocket handshake closed without headers")
            response.extend(chunk)
        headers, buffered = bytes(response).split(b"\r\n\r\n", 1)
        status = int(headers.split(b"\r\n", 1)[0].split()[1])
        if status != 101:
            return status, None
        frame = bytearray(buffered)
        while len(frame) < 2:
            chunk = client.recv(4096)
            if not chunk:
                raise RuntimeError("websocket closed before the first frame")
            frame.extend(chunk)
        length = frame[1] & 0x7f
        offset = 2
        if length == 126:
            while len(frame) < 4:
                frame.extend(client.recv(4096))
            length = int.from_bytes(frame[2:4], "big")
            offset = 4
        elif length == 127:
            while len(frame) < 10:
                frame.extend(client.recv(4096))
            length = int.from_bytes(frame[2:10], "big")
            offset = 10
        while len(frame) < offset + length:
            chunk = client.recv(4096)
            if not chunk:
                raise RuntimeError("websocket closed during the first frame")
            frame.extend(chunk)
        return status, json.loads(bytes(frame[offset:offset + length]))


def snapshot_text(frame):
    if frame is None or frame.get("type") != "snapshot":
        return ""
    return base64.b64decode(frame.get("payload", "")).decode(errors="replace")


def websocket_checkpoint_until_marker(process, tab_id, pane_id):
    deadline = time.monotonic() + 5
    last_status, last_frame, last_text = None, None, ""
    while time.monotonic() < deadline:
        last_status, last_frame = websocket_checkpoint(process, tab_id, pane_id)
        if last_status != 101:
            break
        last_text = snapshot_text(last_frame)
        if TMUX_MARKER in " ".join(last_text.split()):
            return last_status, last_frame, last_text, True
        time.sleep(0.1)
    return last_status, last_frame, last_text, False


def write_provider_fixture():
    config = HOME / ".config"
    providers = config / "agent/providers.d"
    providers.mkdir(parents=True, exist_ok=True)
    (config / "taarof").mkdir(parents=True, exist_ok=True)
    (config / "taarof/config.toml").write_text(
        "[session]\nauto_resume_agents = true\n"
        f"[http]\nenabled = true\nport = {HTTP_PORT}\nbind_address = '127.0.0.1'\n"
        "[http_control]\nenabled = false\n"
    )
    adapter = HOME / "adapter.py"
    adapter.write_text("""import json,sys
r=json.loads(sys.stdin.readline()); op=r['operation']; out={'protocol':1}
if op=='metadata': out.update(id='continuity-fixture',display_name='Continuity Fixture',capabilities=['new','resume'])
elif op=='probe': out.update(available=True)
elif op=='discover': out.update(sessions=[{'session_id':'synthetic-session-28','title':'Synthetic continuity demo','cwd':'/home/laddy/demo','updated_at_unix_ms':2},{'session_id':'synthetic-failure-28','title':'Synthetic failed resume','cwd':'/home/laddy/demo','updated_at_unix_ms':1}])
elif op=='plan-resume':
 assert r['session_id'] in ('synthetic-session-28','synthetic-failure-28')
 script='/home/laddy/provider_resume.py' if r['session_id']=='synthetic-session-28' else '/home/laddy/provider_resume_fail.py'
 out.update(program='/usr/bin/python3',argv=[script],cwd='/home/laddy/demo')
else: raise SystemExit(2)
print(json.dumps(out))
""")
    (HOME / "provider_resume.py").write_text(
        "from pathlib import Path\nimport time\nmarker=Path('/home/laddy/resume-observed')\nmarker.write_text('exact synthetic provider identity')\nprint('RESUME AGENT CONVERSATION: synthetic provider identity accepted', flush=True)\nprint('DISPLAY CHECKPOINT: visual context only; it does not prove liveness', flush=True)\ntime.sleep(300)\n"
    )
    (HOME / "provider_resume_fail.py").write_text(
        "print('RESUME AGENT CONVERSATION FAILED: synthetic provider exit 2 retained for diagnosis', flush=True)\n"
        "raise SystemExit(2)\n"
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
    leaf_agent_failure = {
        "type": "leaf", "work_origin": "pane-agent-failure", "cwd": "/home/laddy/demo",
        "agent_session": {"agent_name": "continuity-fixture",
                          "session_id": "synthetic-failure-28",
                          "host_identity": os.uname().nodename.lower().rstrip("."),
                          "source": "argv"},
    }
    return {
        "version": 2,
        "workspaces": [{
            "id": 1, "work_origin": "workspace-demo", "name": "Continuity demo",
            "tabs": [{"name": "Three explicit actions", "work_origin": "tab-demo",
                      "cwd": "/home/laddy/demo",
                      "panes": {"type": "split", "direction": "horizontal", "ratio": 0.5,
                                "first": leaf_agent,
                                "second": {"type": "split", "direction": "vertical", "ratio": 0.5,
                                           "first": leaf_tmux, "second": leaf_agent_failure}}}],
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
        "while :; do printf '\\033[H\\033[2JREATTACH LIVE TERMINAL: exact synthetic generation accepted\\n'; sleep 0.25; done")
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
    first_http_ready = wait_until(lambda: http_ready(first))
    resume_observed = wait_until(lambda: (HOME / "resume-observed").exists())
    failed_resume_retained_observed = wait_until(failed_resume_retained_state)
    exact_attach_observed = wait_until(lambda: tmux_clients() == 1)
    exact_viewer_ready = wait_until(exact_tmux_viewer_ready)
    original_tab_id, original_pane_id = tmux_pane_coordinates()
    (original_http_status, original_http_frame, original_http_text,
     exact_http_capture_observed) = websocket_checkpoint_until_marker(
        first, original_tab_id, original_pane_id
    )
    (EVIDENCE / "original-http-snapshot.txt").write_text(original_http_text)
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
    second_http_ready = wait_until(lambda: http_ready(second))
    replacement_unavailable_observed = wait_until(replacement_unavailable_state)
    replacement_tab_id, replacement_pane_id = tmux_pane_coordinates()
    replacement_http_status, replacement_http_frame = websocket_checkpoint(
        second, replacement_tab_id, replacement_pane_id
    )
    replacement_http_refused = (
        replacement_http_status == 409 and replacement_http_frame is None
    )
    # The state transition runs on GTK's main thread. Give the next frame one
    # bounded interval to paint the already-retained terminal checkpoint.
    time.sleep(0.2)
    screenshot("replacement-generation.png", env)
    replacement_attachment_result, replacement_attached_count = tmux_attachment_check()
    replacement_rejected = (
        replacement_attachment_result.returncode == 0
        and replacement_attached_count == 0
        and replacement["continuity_id"] != original["continuity_id"]
    )
    stop_app(second)
    post_cleanup_attachment_result, post_cleanup_attached_count = tmux_attachment_check()
    replacement_survived_app_cleanup = (
        post_cleanup_attachment_result.returncode == 0
        and post_cleanup_attached_count == 0
        and tmux_identity() == replacement
    )
    replacement_rejected = replacement_rejected and replacement_survived_app_cleanup
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
        "http_enabled": True, "http_control_enabled": False, "browser_profile_created": False,
        "first_http_ready": first_http_ready, "second_http_ready": second_http_ready,
        "original_http_attach_status": original_http_status,
        "exact_http_capture_observed": exact_http_capture_observed,
        "exact_tmux_viewer_ready": exact_viewer_ready,
        "replacement_http_attach_status": replacement_http_status,
        "replacement_http_refused": replacement_http_refused,
        "original_tmux_identity": original, "replacement_tmux_identity": replacement,
        "resume_agent_conversation_observed": resume_observed,
        "failed_resume_retained_observed": failed_resume_retained_observed,
        "reattach_live_terminal_observed": exact_attach_observed,
        "original_tmux_attached_count": original_attached_count,
        "replacement_tmux_attached_count": replacement_attached_count,
        "replacement_post_app_cleanup_attached_count": post_cleanup_attached_count,
        "replacement_survived_app_cleanup": replacement_survived_app_cleanup,
        "replacement_generation_rejected": replacement_rejected,
        "replacement_unavailable_state_observed": replacement_unavailable_observed,
        "tmux_survived_desktop_shutdown": tmux_survived_desktop,
        "tmux_loss_observed": tmux_loss_observed,
        "reopen_workspace_layout_observation": "screenshots captured; owner review required",
        "replacement_rejection_visual_observation": (
            "screenshot captured after native unavailable state; owner review required"
        ),
        "owner_attended_status": "pending",
        "command_exits": {
            "app_build_info": app_build_result.returncode,
            "agent_build_info": agent_build_result.returncode,
            "original_tmux_attachment_query": original_attachment_result.returncode,
            "replacement_tmux_attachment_query": replacement_attachment_result.returncode,
            "replacement_post_app_cleanup_attachment_query": (
                post_cleanup_attachment_result.returncode
            ),
            "tmux_has_session_after_desktop_shutdown": survived_result.returncode,
            "tmux_has_session_after_server_loss": lost_result.returncode,
        },
    }
    receipt["automated_status"] = "pass" if all([
        resume_observed, exact_attach_observed, replacement_rejected,
        tmux_survived_desktop, tmux_loss_observed, parent_exe_missing_after_exit,
        receipt["embedded_channel_match"], first_window_mapped, second_window_mapped,
        replacement_unavailable_observed, failed_resume_retained_observed,
        first_http_ready, second_http_ready, exact_viewer_ready, exact_http_capture_observed,
        replacement_http_refused,
    ]) else "fail"
    (EVIDENCE / "receipt.json").write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n")
    print("Continuity demo automated checks complete; owner screenshot observation remains pending.")
    return 0 if receipt["automated_status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
