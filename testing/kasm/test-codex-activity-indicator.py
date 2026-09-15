#!/usr/bin/env python3
"""Isolated real-GTK proof for Codex activity labels and state backgrounds.

Run this under ``dbus-run-session`` and ``xvfb-run``. The fixture uses only an
inert local process named ``codex`` and a synthetic current-format transcript.
"""

from __future__ import annotations

import argparse
from collections import Counter
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import tempfile
import time

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402
from PIL import ImageGrab  # noqa: E402


STATE_COLORS = {
    "working": (0x24, 0x36, 0x2B),
    "waiting_input": (0x3D, 0x34, 0x20),
    "done": (0x24, 0x38, 0x3C),
    "errored": (0x40, 0x2B, 0x2B),
    "idle": (0x30, 0x2D, 0x28),
}
STATE_TEXT_COLOR = (0xFF, 0xF8, 0xE8)
STATE_LABELS = {
    "working": "WORKING",
    "waiting_input": "WAITING",
    "done": "TURN ENDED",
    "errored": "ERRORED",
    "idle": "IDLE",
}


def eventually(check, label: str, timeout: float = 20.0):
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (AssertionError, OSError, subprocess.SubprocessError, json.JSONDecodeError) as error:
            last_error = error
        time.sleep(0.2)
    raise AssertionError(f"timed out waiting for {label}: {last_error}")


def walk(accessible):
    yield accessible
    for index in range(accessible.get_child_count()):
        child = accessible.get_child_at_index(index)
        if child is not None:
            yield from walk(child)


def contrast_ratio(foreground, background) -> float:
    def luminance(color) -> float:
        channels = []
        for value in color:
            channel = value / 255
            channels.append(
                channel / 12.92
                if channel <= 0.04045
                else ((channel + 0.055) / 1.055) ** 2.4
            )
        return 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2]

    lighter, darker = sorted(
        (luminance(foreground), luminance(background)), reverse=True
    )
    return (lighter + 0.05) / (darker + 0.05)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    args.binary = args.binary.resolve()
    args.cli = args.cli.resolve()
    args.evidence.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="taarof-codex-indicator-") as temp:
        root = Path(temp)
        home = root / "home"
        config = root / "config"
        state = root / "state"
        runtime = root / "runtime"
        custom_codex_home = root / "pane-codex-home"
        project_root = root / "projects"
        for path in (home, config / "taarof", state, runtime, custom_codex_home, project_root):
            path.mkdir(parents=True, exist_ok=True)
        runtime.chmod(0o700)
        (config / "taarof/config.toml").write_text(
            "[dock]\nvisible = true\n[tasks]\nenabled = false\n",
            encoding="utf-8",
        )
        (config / "ghostty").mkdir()
        (config / "ghostty/config").write_text(
            "command = /bin/bash --noprofile --norc\n", encoding="utf-8"
        )
        fake_codex = root / "codex"
        fake_codex.write_text(
            "#!/bin/sh\nwhile :; do sleep 60; done\n", encoding="utf-8"
        )
        fake_codex.chmod(0o755)

        session = f"kmux-215-ui-{os.getpid()}"
        env = os.environ.copy()
        env.update(
            HOME=str(home),
            XDG_CONFIG_HOME=str(config),
            XDG_STATE_HOME=str(state),
            XDG_RUNTIME_DIR=str(runtime),
            TAAROF_SESSION=session,
            GTK_A11Y="atspi",
        )
        env.pop("CODEX_HOME", None)
        env.pop("NO_AT_BRIDGE", None)

        app = subprocess.Popen(
            [str(args.binary)],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            start_new_session=True,
            text=True,
        )

        def cli(*arguments: str):
            completed = subprocess.run(
                [str(args.cli), "--session", session, *arguments],
                env=env,
                capture_output=True,
                text=True,
            )
            if completed.returncode != 0:
                raise AssertionError(
                    f"taarof CLI failed ({completed.returncode}): {completed.stderr.strip()}"
                )
            return json.loads(completed.stdout)

        try:
            eventually(lambda: cli("query-state"), "private socket readiness")
            tabs = {}
            for state_name in STATE_LABELS:
                project = project_root / state_name
                project.mkdir()
                command = " ".join(
                    [
                        "env",
                        f"CODEX_HOME={shlex.quote(str(custom_codex_home))}",
                        shlex.quote(str(fake_codex)),
                    ]
                )
                response = cli(
                    "create-tab",
                    "--name",
                    f"Codex {STATE_LABELS[state_name]}",
                    "--cwd",
                    str(project),
                    "--command",
                    command,
                )
                tabs[state_name] = int(response["tab_id"])

            working_project = project_root / "working"
            transcript_dir = custom_codex_home / "sessions/2026/09/15"
            transcript_dir.mkdir(parents=True)
            timestamp = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
            records = [
                {
                    "type": "session_meta",
                    "timestamp": timestamp,
                    "payload": {
                        "id": "kmux-215-ui-working",
                        "cwd": str(working_project),
                        "timestamp": timestamp,
                    },
                },
                {
                    "type": "event_msg",
                    "timestamp": timestamp,
                    "payload": {"type": "task_started", "turn_id": "kmux-215-ui-turn"},
                },
            ]
            (transcript_dir / "rollout-kmux-215-ui.jsonl").write_text(
                "".join(json.dumps(record) + "\n" for record in records), encoding="utf-8"
            )

            def observed_states():
                snapshot = cli("query-state")["data"]
                observed = {}
                for workspace in snapshot["workspaces"]:
                    for tab in workspace["tabs"]:
                        if tab["tab_id"] in tabs.values() and tab.get("agents"):
                            observed[tab["tab_id"]] = tab["agents"][0]["state"]
                return observed

            eventually(
                lambda: len(observed_states()) == len(tabs), "five detected Codex panes"
            )
            for state_name in ("waiting_input", "done", "errored"):
                cli(
                    "agent-status",
                    "--tab",
                    str(tabs[state_name]),
                    "--pane",
                    "0",
                    "--state",
                    state_name.replace("_", "-"),
                    "--text",
                    STATE_LABELS[state_name],
                    "--source",
                    "codex",
                )

            expected = {tabs[name]: name for name in STATE_LABELS}

            def expected_states():
                current = observed_states()
                if all(
                    current.get(tab_id) == state_name
                    for tab_id, state_name in expected.items()
                ):
                    return current
                return None

            states = eventually(
                expected_states,
                "canonical lifecycle transitions",
            )

            desktop = Atspi.get_desktop(0)
            agents_button = eventually(
                lambda: next(
                    (
                        node
                        for node in walk(desktop)
                        if (node.get_name() or "").strip() == "Agents"
                    ),
                    None,
                ),
                "Agents mode accessibility node",
            )
            action = agents_button.get_action_iface()
            assert action is not None and action.get_n_actions() > 0
            assert action.do_action(0)

            def accessible_labels():
                return sorted(
                    {
                        (node.get_name() or "").strip()
                        for node in walk(Atspi.get_desktop(0))
                        if (node.get_name() or "").strip()
                    }
                )

            def expected_labels():
                names = accessible_labels()
                if all(
                    any(label in name for name in names)
                    for label in STATE_LABELS.values()
                ):
                    return names
                return None

            labels = eventually(
                expected_labels,
                "non-color activity labels in the GTK accessibility tree",
            )
            time.sleep(0.5)
            screenshot = ImageGrab.grab(xdisplay=env["DISPLAY"])
            screenshot_path = args.evidence / "codex-activity-states.png"
            screenshot.save(screenshot_path)
            rgb_image = screenshot.convert("RGB")
            pixel_data = getattr(
                rgb_image, "get_flattened_data", rgb_image.getdata
            )()
            pixels = Counter(pixel_data)
            color_counts = {
                state_name: pixels[color] for state_name, color in STATE_COLORS.items()
            }
            assert all(count >= 100 for count in color_counts.values()), color_counts
            contrast_ratios = {
                state_name: round(contrast_ratio(STATE_TEXT_COLOR, color), 2)
                for state_name, color in STATE_COLORS.items()
            }
            assert all(ratio >= 4.5 for ratio in contrast_ratios.values()), contrast_ratios

            result = {
                "binary": str(args.binary),
                "binary_sha256": subprocess.check_output(
                    ["sha256sum", str(args.binary)], text=True
                ).split()[0],
                "app_pid": app.pid,
                "states": {str(tab_id): state_name for tab_id, state_name in states.items()},
                "labels": [
                    name
                    for name in labels
                    if any(label in name for label in STATE_LABELS.values())
                ],
                "background_rgb": {
                    state_name: list(color) for state_name, color in STATE_COLORS.items()
                },
                "background_pixel_counts": color_counts,
                "text_contrast_ratios": contrast_ratios,
                "parent_codex_home_unset": True,
                "pane_codex_home": str(custom_codex_home),
            }
            (args.evidence / "result.json").write_text(
                json.dumps(result, indent=2) + "\n", encoding="utf-8"
            )
            print(json.dumps(result, indent=2))
        finally:
            try:
                os.killpg(app.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                app.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(app.pid, signal.SIGKILL)
                app.wait(timeout=5)
            if app.returncode not in (0, -signal.SIGTERM):
                stderr = app.stderr.read() if app.stderr is not None else ""
                print(stderr[-4000:])


if __name__ == "__main__":
    main()
