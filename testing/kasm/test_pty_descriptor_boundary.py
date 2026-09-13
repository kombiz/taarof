#!/usr/bin/env python3
"""Real GTK/VTE PTY lifecycle check, only in a disposable non-root container.

Run under dbus-run-session in the Kasm image with --binary pointing at the
exact built executable. No HTTP listener, host state, or published port is used.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import tempfile
import time


def eventually(check, label, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.025)
    raise AssertionError(f"timed out: {label}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    if not Path("/.dockerenv").is_file() or os.getuid() == 0:
        parser.error("requires a disposable container running as the desktop user")
    binary = args.binary.resolve(strict=True)
    args.evidence.mkdir(parents=True, exist_ok=True)
    children = []
    with tempfile.TemporaryDirectory(prefix="taarof-pty-boundary-") as tmp:
        root = Path(tmp)
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("TAAROF_", "INFISICAL_"))}
        for key in ("HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
                    "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"):
            directory = root / key.lower()
            directory.mkdir(mode=0o700)
            env[key] = str(directory)
        env.update(DISPLAY=":97", GSK_RENDERER="cairo", SHELL="/bin/bash",
                   VTE_VERSION="parent-sentinel", TAAROF_SESSION="pty-boundary",
                   INFISICAL_TOKEN="inert-test-token", INFISICAL_SERVICE_TOKEN="inert-test-service")
        config = Path(env["XDG_CONFIG_HOME"])
        (config / "taarof").mkdir()
        (config / "taarof/config.toml").write_text(
            '[http]\nenabled = false\n[history]\nenabled = false\n'
            '[tasks]\nenabled = false\n[dock]\nvisible = false\n')
        (config / "ghostty").mkdir()
        (config / "ghostty/config").write_text('command = /bin/bash --noprofile --norc\n')
        with (args.evidence / "gtk.log").open("w") as log:
            try:
                display = subprocess.Popen(["Xvfb", ":97", "-screen", "0", "1280x800x24"],
                                           stdout=log, stderr=log)
                children.append(display)
                eventually(lambda: Path("/tmp/.X11-unix/X97").exists(), "X display")
                app = subprocess.Popen([str(binary)], env=env, stdout=log, stderr=log)
                children.append(app)

                def registry():
                    assert app.poll() is None, "app exited before readiness"
                    for record in Path(env["XDG_RUNTIME_DIR"]).glob("taarof-current*.json"):
                        data = json.loads(record.read_text())
                        if data["pid"] == app.pid:
                            return data["socket_path"]
                sock = eventually(registry, "app socket")

                def request(action, **kw):
                    with socket.socket(socket.AF_UNIX) as connection:
                        connection.settimeout(5)
                        connection.connect(sock)
                        connection.sendall(json.dumps(dict(action=action, **kw)).encode())
                        connection.shutdown(socket.SHUT_WR)
                        data = b""
                        while chunk := connection.recv(65536):
                            data += chunk
                    response = json.loads(data)
                    assert response.get("ok"), response.get("error", "socket request failed")
                    return response.get("data", response)

                def tabs():
                    return [t for w in request("list-tabs")["workspaces"] for t in w["tabs"]]

                def text(tab, pane):
                    return json.dumps(request("get-text", tab=str(tab), pane=pane, scrollback=100))

                def send(tab, pane, command):
                    request("send-keys", tab=str(tab), pane=pane, keys=command + "\n")

                initial = eventually(lambda: tabs(), "first tab")[0]
                tab = initial["tab_id"]
                pane = initial["panes"][0]["pane_id"]

                def verify_pane(tab, pane):
                    send(tab, pane, "printf 'BOUNDARY_PID_%s\\n' \"$$\"")
                    pid = int(eventually(lambda: (re.search(r"BOUNDARY_PID_(\d+)", text(tab, pane)) or [None, None])[1], "shell pid"))
                    send(tab, pane, 'test "$VTE_VERSION" = 8203 && test -z "${INFISICAL_TOKEN+x}${INFISICAL_SERVICE_TOKEN+x}" && printf "ENV_%s\\n" CLEAN')
                    eventually(lambda: "ENV_CLEAN" in text(tab, pane), "child environment")
                    own_tty = os.readlink(f"/proc/{pid}/fd/0")
                    for fd in Path(f"/proc/{pid}/fd").iterdir():
                        try:
                            target = os.readlink(fd)
                        except FileNotFoundError:
                            continue
                        if int(fd.name) > 2:
                            assert "ptmx" not in target, "pane inherited PTY master"
                            assert not target.startswith("/dev/pts/") or target == own_tty, "pane inherited sibling slave"
                    return pid

                verify_pane(tab, pane)
                request("create-tab", name="Boundary sibling")
                sibling = eventually(lambda: next((t for t in tabs() if t["tab_id"] != tab), None), "second tab")
                sibling_tab = sibling["tab_id"]
                sibling_pane = sibling["panes"][0]["pane_id"]
                second_pid = verify_pane(sibling_tab, sibling_pane)
                split = request("split-pane", tab=str(sibling_tab), direction="horizontal")
                split_pane = split["pane_id"]
                split_pid = verify_pane(sibling_tab, split_pane)

                # Real keyboard input passes through VTE's conduit and back into
                # its rendered text; socket get-text reads the native VTE widget.
                windows = subprocess.check_output(
                    ["xdotool", "search", "--onlyvisible", "--pid", str(app.pid)], env=env).splitlines()
                subprocess.run(["xdotool", "windowfocus", "--sync", windows[-1].decode()], env=env, check=True)
                subprocess.run(["xdotool", "type", "--clearmodifiers", "--delay", "1",
                                "printf 'KEYBOARD_%s\\n' OK"], env=env, check=True)
                subprocess.run(["xdotool", "key", "Return"], env=env, check=True)
                eventually(lambda: "KEYBOARD_OK" in text(sibling_tab, split_pane), "VTE keyboard round trip")
                from PIL import ImageGrab
                ImageGrab.grab(xdisplay=env["DISPLAY"]).save(args.evidence / "gtk.png")
                request("close-pane", tab=str(sibling_tab), pane=split_pane)
                eventually(lambda: not Path(f"/proc/{split_pid}").exists(), "split child reaped")
                send(sibling_tab, sibling_pane, "printf 'SURVIVOR_%s\\n' OK")
                eventually(lambda: "SURVIVOR_OK" in text(sibling_tab, sibling_pane), "sibling remains usable")
                request("close-tab", tab=str(sibling_tab))
                eventually(lambda: not Path(f"/proc/{second_pid}").exists(), "second tab child reaped")
                send(tab, pane, "printf 'ORIGINAL_%s\\n' OK")
                eventually(lambda: "ORIGINAL_OK" in text(tab, pane), "original remains usable")
                result = dict(binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                              process_sha256=hashlib.sha256(Path(f"/proc/{app.pid}/exe").read_bytes()).hexdigest(),
                              panes_verified=3, child_vte_version="8203", ambient_token_names_absent=True,
                              cross_pane_descriptors_absent=True, keyboard_round_trip=True,
                              split_and_tab_children_reaped=True, surviving_panes_usable=True)
                assert result["binary_sha256"] == result["process_sha256"]
                (args.evidence / "gtk-result.json").write_text(json.dumps(result, indent=2) + "\n")
                print(json.dumps(result, indent=2))
            finally:
                for child in reversed(children):
                    child.send_signal(signal.SIGTERM)
                    try:
                        child.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()


if __name__ == "__main__":
    main()
