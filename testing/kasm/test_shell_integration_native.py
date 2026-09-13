#!/usr/bin/env python3
"""Verify installed shell helpers through real VTE in a disposable container.

The probe owns a private HOME/XDG, application process and container clipboard.
It never reads an owner's runtime or launches an external agent.
"""

import argparse
import hashlib, json, os, pathlib, shlex, subprocess, tempfile, time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--prefix", type=pathlib.Path, required=True)
parser.add_argument("--evidence", type=pathlib.Path, required=True)
args = parser.parse_args()
if not pathlib.Path("/.dockerenv").is_file() or os.getuid() == 0:
    parser.error("requires a disposable non-root container")
prefix = args.prefix.resolve()
evidence = args.evidence.resolve()
evidence.mkdir(parents=True, exist_ok=True)
app = prefix / "bin/taarof-app"
cli = prefix / "bin/taarof"
with tempfile.TemporaryDirectory(prefix="kmux212-native-") as temporary:
    root = pathlib.Path(temporary)
    env = {k: os.environ[k] for k in ["PATH"]}
    for key in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
    ]:
        p = root / key
        p.mkdir(mode=0o700)
        env[key] = str(p)
    env.update(DISPLAY=":1", GDK_BACKEND="x11", SHELL="/bin/bash", LANG="C.UTF-8")
    cfg = pathlib.Path(env["XDG_CONFIG_HOME"]) / "taarof"
    cfg.mkdir()
    (cfg / "config.toml").write_text("[http]\nenabled=false\n[tasks]\nenabled=false\n")
    output = open(evidence / "app.log", "w")
    process = subprocess.Popen([str(app)], env=env, stdout=output, stderr=output)

    def call(*args):
        return json.loads(
            subprocess.check_output(
                [str(cli), "--format", "json", *map(str, args)],
                env=env,
                timeout=3,
                stderr=subprocess.DEVNULL,
            )
        )

    def wait(check):
        end = time.monotonic() + 10
        while time.monotonic() < end:
            try:
                result = check()
                if result:
                    return result
            except (OSError, subprocess.SubprocessError, KeyError):
                pass
            time.sleep(0.1)
        raise AssertionError("bounded native shell check failed")

    def tabs():
        return [t for w in call("query-state")["data"]["workspaces"] for t in w["tabs"]]

    def send(text):
        call("send-keys", "--tab", tab, "--pane", 0, "--keys", text + "\n")

    try:
        wait(lambda: tabs())
        assert (
            hashlib.sha256(app.read_bytes()).digest()
            == hashlib.sha256(
                pathlib.Path(f"/proc/{process.pid}/exe").read_bytes()
            ).digest()
        )
        tab = call(
            "create-tab",
            "--name",
            "Inert integration",
            "--command",
            "bash --noprofile --norc -i",
        )["tab_id"]
        call("switch-tab", tab)
        shell = prefix / "share/taarof/shell"
        send(
            "source "
            + shlex.quote(str(shell / "osc7.bash"))
            + "; source "
            + shlex.quote(str(shell / "taarof-shell-integration.sh"))
        )
        cwd = root / "inert space café"
        cwd.mkdir()
        send("cd " + shlex.quote(str(cwd)))
        wait(
            lambda: any(
                t["tab_id"] == tab and t["panes"][0]["cwd"] == str(cwd) for t in tabs()
            )
        )
        send("printf 'KMUX212_PROMPT_SENTINEL\\n'")
        time.sleep(0.6)
        window = subprocess.check_output(
            ["xdotool", "search", "--onlyvisible", "--pid", str(process.pid)],
            env=env,
            text=True,
        ).splitlines()[-1]
        subprocess.run(
            ["xdotool", "windowactivate", "--sync", window], env=env, check=True
        )
        subprocess.run(
            ["xdotool", "mousemove", "400", "300", "click", "1"], env=env, check=True
        )
        subprocess.run(
            ["xdotool", "key", "--clearmodifiers", "ctrl+shift+o"], env=env, check=True
        )
        time.sleep(0.3)
        copied = subprocess.check_output(
            ["xclip", "-selection", "clipboard", "-o"], env=env, text=True, timeout=3
        )
        assert copied.strip() == "KMUX212_PROMPT_SENTINEL", repr(copied)
        subprocess.run(
            ["scrot", "-o", str(evidence / "native-shell.png")], env=env, check=True
        )
        (evidence / "result.json").write_text(
            json.dumps(
                {
                    "binary_sha256": hashlib.sha256(app.read_bytes()).hexdigest(),
                    "process_identity": True,
                    "real_vte_cwd_unicode": True,
                    "native_prompt_mark_copy_exact": True,
                },
                indent=2,
            )
            + "\n"
        )
        print(
            "PASS exact installed binary, real VTE Unicode CWD and native prompt-mark copy"
        )
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        output.close()
