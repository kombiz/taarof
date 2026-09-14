from __future__ import annotations

import importlib.machinery
import importlib.util
import io
import json
import os
import shlex
import stat
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


def load_cli_module():
    cli_path = Path(__file__).with_name("taarof")
    loader = importlib.machinery.SourceFileLoader("taarof_cli_under_test", str(cli_path))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    if spec is None:
        raise RuntimeError("could not load taarof CLI module spec")
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


class HttpBaseTests(unittest.TestCase):
    def test_default_http_base_matches_app_default_port(self) -> None:
        cli = load_cli_module()
        env = {key: value for key, value in os.environ.items() if key != "TAAROF_HTTP"}
        with patch.dict(os.environ, env, clear=True):
            self.assertEqual(cli._http_base(), "http://127.0.0.1:7800")

    def test_http_base_allows_environment_override(self) -> None:
        cli = load_cli_module()
        with patch.dict(os.environ, {"TAAROF_HTTP": "http://127.0.0.1:9999"}, clear=True):
            self.assertEqual(cli._http_base(), "http://127.0.0.1:9999")


class HttpAuthTests(unittest.TestCase):
    def test_http_get_uses_runtime_bearer_token(self) -> None:
        cli = load_cli_module()

        with tempfile.TemporaryDirectory() as tmp:
            runtime_dir = Path(tmp)
            (runtime_dir / "taarof-current.json").write_text(
                '{"pid":1234,"socket_path":"/tmp/taarof.sock"}',
                encoding="utf-8",
            )
            (runtime_dir / "taarof-http-1234.token").write_text("secret-token\n", encoding="utf-8")

            captured = {}

            class FakeResponse:
                def __enter__(self):
                    return self

                def __exit__(self, exc_type, exc, tb):
                    return False

                def read(self):
                    return b'{"ok":true}'

            def fake_urlopen(request, timeout):
                captured["authorization"] = request.get_header("Authorization")
                captured["url"] = request.full_url
                captured["timeout"] = timeout
                return FakeResponse()

            with (
                patch.dict(os.environ, {"XDG_RUNTIME_DIR": str(runtime_dir)}, clear=True),
                patch.object(cli.urllib.request, "urlopen", fake_urlopen),
            ):
                self.assertEqual(cli.http_get("/api/v1/state"), {"ok": True})

            self.assertEqual(captured["authorization"], "Bearer secret-token")
            self.assertEqual(captured["url"], "http://127.0.0.1:7800/api/v1/state")
            self.assertEqual(captured["timeout"], 5)

    def test_http_token_environment_override_wins(self) -> None:
        cli = load_cli_module()
        with patch.dict(os.environ, {"TAAROF_HTTP_TOKEN": "override-token"}, clear=True):
            self.assertEqual(cli._http_token(None), "override-token")

    def test_runtime_identity_uses_authenticated_http_route(self) -> None:
        cli = load_cli_module()
        calls = []

        def fake_http_get(path, session=None):
            calls.append((path, session))
            return {
                "ok": True,
                "data": {
                    "schema": "taarof.runtime-identity.v1",
                    "runtime_id": "018f0f1e-a2d3-4c55-8f7b-9023456789ab",
                    "session_name": "default",
                },
            }

        with (
            patch.object(cli, "http_get", fake_http_get),
            patch.object(cli, "_emit"),
        ):
            rc = cli.main(["runtime-identity"])

        self.assertEqual(rc, 0)
        self.assertEqual(calls, [("/api/v1/runtime-identity", None)])


class HistoryTests(unittest.TestCase):
    def test_query_history_builds_filtered_socket_request(self) -> None:
        cli = load_cli_module()
        captured = {}

        def fake_socket_send(message, session=None):
            captured["message"] = message
            captured["session"] = session
            return {"ok": True, "data": {"records": []}}

        with (
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli, "_emit"),
        ):
            rc = cli.main([
                "--session", "dev", "query-history", "--since-id", "41",
                "--limit", "25", "--record-type", "work", "--task", "EXAMPLE-133",
            ])

        self.assertEqual(rc, 0)
        self.assertEqual(captured, {
            "message": {
                "action": "query-history",
                "since_id": 41,
                "limit": 25,
                "record_type": "work",
                "task": "EXAMPLE-133",
            },
            "session": "dev",
        })

    def test_history_doctor_reports_live_writer_status(self) -> None:
        cli = load_cli_module()
        response = {"ok": True, "data": {"history": {
            "available": True,
            "state": "ok",
            "schema_version": 2,
            "lag_ms": 7,
            "queue_depth": 4,
            "last_durable_seq": {"event": 9, "work": 3, "id": 12},
            "dropped": 2,
            "storage": {
                "main_bytes": 8192, "wal_bytes": 0,
                "soft_max_bytes": 268435456, "record_count": 12,
                "oldest_record_ts_unix_ms": 1000,
                "newest_record_ts_unix_ms": 2000,
            },
            "maintenance": {
                "last_result": "ok", "last_run_at_unix_ms": 2000,
                "rows_removed_last": 3, "rows_removed_total": 8,
                "consecutive_failures": 0, "last_error": None,
            },
        }}}
        with patch.object(cli, "socket_send", return_value=response):
            result = cli.check_history({"history": {"enabled": True}}, None, None)
        self.assertEqual(result["status"], "ok")
        self.assertEqual(result["schema_version"], 2)
        self.assertEqual(result["last_durable_seq"], {
            "event": 9, "work": 3, "id": 12,
        })
        self.assertEqual(result["dropped"], 2)
        self.assertEqual(result["queue_depth"], 4)
        self.assertEqual(result["storage"]["main_bytes"], 8192)
        self.assertEqual(result["maintenance"]["rows_removed_last"], 3)

    def test_history_doctor_warns_on_maintenance_failure(self) -> None:
        cli = load_cli_module()
        response = {"ok": True, "data": {"history": {
            "available": True, "state": "ok", "schema_version": 2,
            "storage": {
                "main_bytes": 9000000, "wal_bytes": 0,
                "soft_max_bytes": 8388608,
            },
            "maintenance": {
                "last_result": "failed", "consecutive_failures": 2,
                "last_error": "checkpoint busy",
            },
        }}}
        with patch.object(cli, "socket_send", return_value=response):
            result = cli.check_history({"history": {"enabled": True}}, None, None)
        self.assertEqual(result["status"], "warn")
        self.assertIn("checkpoint busy", result["detail"])

    def test_history_doctor_does_not_mislabel_non_ok_state_as_size_overage(self) -> None:
        cli = load_cli_module()
        response = {"ok": True, "data": {"history": {
            "available": True, "state": "starting", "schema_version": 2,
            "storage": {
                "main_bytes": 0, "wal_bytes": 0, "soft_max_bytes": 0,
            },
            "maintenance": {
                "last_result": "never", "consecutive_failures": 0,
            },
        }}}
        with patch.object(cli, "socket_send", return_value=response):
            result = cli.check_history({"history": {"enabled": True}}, None, None)
        self.assertEqual(result["status"], "warn")
        self.assertIn("history state is starting", result["detail"])
        self.assertNotIn("soft cap", result["detail"])

    def test_history_doctor_errors_on_misconfiguration(self) -> None:
        cli = load_cli_module()
        response = {"ok": True, "data": {"history": {
            "available": False, "state": "misconfigured", "schema_version": 0,
            "reason": "history.max_records is 42; expected 1000..=10000000",
        }}}
        with patch.object(cli, "socket_send", return_value=response):
            result = cli.check_history({"history": {"enabled": True}}, None, None)
        self.assertEqual(result["status"], "error")
        self.assertIn("history.max_records is 42", result["detail"])

    def test_history_doctor_detects_misconfiguration_when_history_is_disabled(self) -> None:
        cli = load_cli_module()
        response = {"ok": True, "data": {"history": {
            "available": False, "enabled": False, "state": "misconfigured",
            "schema_version": 0,
            "reason": "history.max_records is 42; expected 1000..=10000000",
        }}}
        with patch.object(cli, "socket_send", return_value=response):
            result = cli.check_history({"history": {"enabled": False}}, None, None)
        self.assertEqual(result["status"], "error")
        self.assertIn("history.max_records is 42", result["detail"])

    def test_history_doctor_keeps_disabled_history_optional_without_a_runtime(self) -> None:
        cli = load_cli_module()
        with patch.object(cli, "socket_send", side_effect=OSError("not running")):
            result = cli.check_history({"history": {"enabled": False}}, None, None)
        self.assertEqual(result["status"], "skip")

    def test_history_doctor_errors_when_config_cannot_be_parsed(self) -> None:
        cli = load_cli_module()
        result = cli.check_history(None, "TOML parse error", None)
        self.assertEqual(result["status"], "error")
        self.assertIn("TOML parse error", result["detail"])


class SendKeysTests(unittest.TestCase):
    def test_send_keys_reads_payload_from_file(self) -> None:
        cli = load_cli_module()

        with tempfile.TemporaryDirectory() as tmp:
            payload_path = Path(tmp) / "paste.txt"
            payload = "line one\n" + ("x" * 300_000) + "\nline three"
            payload_path.write_text(payload, encoding="utf-8")
            captured = {}

            def fake_socket_send(message, session=None):
                captured["message"] = message
                captured["session"] = session
                return {"ok": True}

            with (
                patch.object(cli, "socket_send", fake_socket_send),
                patch.object(cli, "_emit"),
            ):
                rc = cli.main([
                    "send-keys",
                    "--pane",
                    "2",
                    "--keys-file",
                    str(payload_path),
                ])

            self.assertEqual(rc, 0)
            self.assertEqual(
                captured,
                {
                    "message": {
                        "action": "send-keys",
                        "pane": 2,
                        "keys": payload,
                    },
                    "session": None,
                },
            )

    def test_send_keys_reads_payload_from_stdin(self) -> None:
        cli = load_cli_module()
        captured = {}

        def fake_socket_send(message, session=None):
            captured["message"] = message
            captured["session"] = session
            return {"ok": True}

        payload = "paste from pipe\nnext line"
        with (
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli, "_emit"),
            patch.object(cli.sys, "stdin", io.StringIO(payload)),
        ):
            rc = cli.main(["send-keys", "--pane", "3", "--stdin"])

        self.assertEqual(rc, 0)
        self.assertEqual(
            captured,
            {
                "message": {
                    "action": "send-keys",
                    "pane": 3,
                    "keys": payload,
                },
                "session": None,
            },
        )


class GroupedCommandTests(unittest.TestCase):
    def test_tab_create_matches_flat_create_tab_payload(self) -> None:
        cli = load_cli_module()
        calls = []

        def fake_socket_send(message, session=None):
            calls.append(message)
            return {"ok": True}

        with (
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli, "_emit"),
        ):
            self.assertEqual(cli.main(["create-tab", "--name", "X"]), 0)
            self.assertEqual(cli.main(["tab", "create", "--name", "X"]), 0)

        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[0], {"action": "create-tab", "name": "X"})
        self.assertEqual(calls[0], calls[1])

    def test_pane_run_matches_flat_run_payload(self) -> None:
        cli = load_cli_module()
        calls = []

        def fake_socket_send(message, session=None):
            calls.append(message)
            return {"ok": True}

        with (
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli, "_emit"),
        ):
            self.assertEqual(
                cli.main(["run", "--pane", "1", "--command", "ls"]), 0)
            self.assertEqual(
                cli.main(["pane", "run", "--pane", "1", "--command", "ls"]), 0)

        self.assertEqual(calls[0], {"action": "run-in-pane", "pane": 1, "command": "ls"})
        self.assertEqual(calls[0], calls[1])

    def test_pane_split_matches_flat_payload_and_maps_idempotency_key(self) -> None:
        cli = load_cli_module()
        calls = []

        def fake_socket_send(message, session=None):
            calls.append(message)
            return {"ok": True, "workspace_id": 1, "tab_id": 2, "pane_id": 3}

        with (
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli, "_emit"),
        ):
            args = [
                "--tab", "current", "--direction", "horizontal",
                "--command", "cargo test", "--cwd", "/repo",
                "--idempotency-key", "build:42",
            ]
            self.assertEqual(cli.main(["split-pane", *args]), 0)
            self.assertEqual(cli.main(["pane", "split", *args]), 0)

        expected = {
            "action": "split-pane",
            "tab": "current",
            "direction": "horizontal",
            "command": "cargo test",
            "working_dir": "/repo",
            "idempotency_key": "build:42",
        }
        self.assertEqual(calls, [expected, expected])

    def test_split_pane_omits_unspecified_optional_fields(self) -> None:
        cli = load_cli_module()
        calls = []
        with (
            patch.object(cli, "socket_send", lambda message, session=None: calls.append(message) or {"ok": True}),
            patch.object(cli, "_emit"),
        ):
            self.assertEqual(cli.main(["split-pane"]), 0)
        self.assertEqual(calls, [{"action": "split-pane"}])

    def test_prompt_agent_submits_then_waits_with_returned_token(self) -> None:
        cli = load_cli_module()
        calls = []

        def fake_socket_send(message, session=None, timeout=10.0):
            calls.append((message, timeout))
            if message["action"] == "prompt-agent":
                return {"ok": True, "data": {"turn_token": "turn-1"}}
            return {"ok": True, "data": {"outcome": "completed"}}

        with patch.object(cli, "socket_send", fake_socket_send), patch.object(cli, "_emit"):
            self.assertEqual(cli.main([
                "prompt-agent", "--tab", "12", "--pane", "4",
                "--prompt", "run tests", "--timeout", "3",
                "--max-output-bytes", "2048",
            ]), 0)

        self.assertEqual(calls[0], ({
            "action": "prompt-agent", "tab": "12", "pane": 4, "prompt": "run tests",
        }, 10.0))
        self.assertEqual(calls[1], ({
            "action": "wait-agent-turn", "turn_token": "turn-1",
            "timeout_seconds": 3.0, "max_output_bytes": 2048,
        }, 8.0))

    def test_prompt_agent_no_wait_returns_before_wait_request(self) -> None:
        cli = load_cli_module()
        calls = []
        with (
            patch.object(cli, "socket_send", lambda message, session=None: calls.append(message) or {
                "ok": True, "data": {"turn_token": "turn-2"}}),
            patch.object(cli, "_emit"),
        ):
            self.assertEqual(cli.main([
                "prompt-agent", "--tab", "12", "--pane", "4",
                "--prompt", "hello", "--no-wait",
            ]), 0)
        self.assertEqual(calls, [{
            "action": "prompt-agent", "tab": "12", "pane": 4, "prompt": "hello",
        }])

    def test_cancel_agent_turn_maps_token(self) -> None:
        cli = load_cli_module()
        calls = []
        with (
            patch.object(cli, "socket_send", lambda message, session=None: calls.append(message) or {"ok": True}),
            patch.object(cli, "_emit"),
        ):
            self.assertEqual(cli.main(["cancel-agent-turn", "turn-3"]), 0)
        self.assertEqual(calls, [{"action": "cancel-agent-turn", "turn_token": "turn-3"}])

    def test_grouped_help_is_reachable(self) -> None:
        cli = load_cli_module()
        parser = cli.build_parser()
        # `tab --help` should exit(0) via argparse rather than error.
        with self.assertRaises(SystemExit) as ctx:
            parser.parse_args(["tab", "--help"])
        self.assertEqual(ctx.exception.code, 0)


class RenderTests(unittest.TestCase):
    LIST_TABS_RESP = {
        "ok": True,
        "data": {
            "active_workspace": 1,
            "workspaces": [
                {
                    "id": 1,
                    "name": "main",
                    "tabs": [
                        {
                            "tab_id": 10,
                            "name": "edit",
                            "agent_name": "claude",
                            "agents": [{"pane_id": 1, "agent_name": "claude"}],
                            "needs_attention": False,
                        },
                        {
                            "tab_id": 11,
                            "name": "logs",
                            "agent_name": None,
                            "agents": [],
                            "needs_attention": True,
                        },
                    ],
                }
            ],
        },
    }

    def test_render_list_tabs_table(self) -> None:
        cli = load_cli_module()
        rendered = cli.render_list_tabs(self.LIST_TABS_RESP)
        self.assertIsNotNone(rendered)
        lines = rendered.splitlines()
        self.assertEqual(lines[0].split(), ["id", "name", "workspace", "agents", "attn"])
        # Data rows carry the expected ids/names/agents/attention marker.
        self.assertIn("10", lines[2])
        self.assertIn("edit", lines[2])
        self.assertIn("claude", lines[2])
        self.assertTrue(lines[3].rstrip().endswith("!"))

    def test_render_list_tabs_empty(self) -> None:
        cli = load_cli_module()
        rendered = cli.render_list_tabs({"ok": True, "data": {"workspaces": []}})
        self.assertEqual(rendered, "no tabs")

    def test_query_state_render_exposes_runtime_probe_truth(self) -> None:
        cli = load_cli_module()
        payload = json.loads(json.dumps(self.LIST_TABS_RESP))
        payload["data"]["runtime_probe"] = {
            "state": "partial",
            "process": {"state": "stale", "age_ms": 4_000},
            "ports": {"state": "ok", "age_ms": 0},
        }

        rendered = cli.render_query_state(payload)

        self.assertIn("runtime probe: partial", rendered)
        self.assertIn("process=stale (4000ms)", rendered)
        self.assertIn("ports=ok (0ms)", rendered)

    def test_format_json_equals_compact_default(self) -> None:
        cli = load_cli_module()
        # --format json must produce byte-identical output to today's default
        # (compact separators, no trailing indent).
        expected = json.dumps(self.LIST_TABS_RESP, separators=(",", ":"))

        def fake_socket_send(message, session=None):
            return self.LIST_TABS_RESP

        for extra in ([], ["--format", "json"]):
            out = io.StringIO()
            with (
                patch.object(cli, "socket_send", fake_socket_send),
                patch.object(cli.sys, "stdout", out),
            ):
                rc = cli.main(["list-tabs"] + extra)
            self.assertEqual(rc, 0)
            self.assertEqual(out.getvalue(), expected + "\n")

    def test_format_table_renders_human_output(self) -> None:
        cli = load_cli_module()

        def fake_socket_send(message, session=None):
            return self.LIST_TABS_RESP

        out = io.StringIO()
        with (
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli.sys, "stdout", out),
        ):
            rc = cli.main(["list-tabs", "--format", "table"])
        self.assertEqual(rc, 0)
        text = out.getvalue()
        self.assertIn("workspace", text)
        self.assertIn("claude", text)
        # Not JSON.
        self.assertNotIn('{"ok"', text)


class DoctorTests(unittest.TestCase):
    def _run_doctor(self, cli, argv):
        out = io.StringIO()
        with patch.object(cli.sys, "stdout", out):
            rc = cli.main(argv)
        return rc, out.getvalue()

    def test_registry_missing_reports_missing_app(self) -> None:
        cli = load_cli_module()
        with tempfile.TemporaryDirectory() as tmp:
            env = {"XDG_RUNTIME_DIR": tmp}
            with patch.dict(os.environ, env, clear=True):
                result = cli.run_doctor_checks(None)
        self.assertFalse(result["ok"])
        reg = next(c for c in result["checks"] if c["name"] == "app.registry")
        self.assertEqual(reg["status"], "error")
        self.assertIn("missing app", reg["detail"])

    def test_dead_pid_reports_stale_registry(self) -> None:
        cli = load_cli_module()
        with tempfile.TemporaryDirectory() as tmp:
            runtime_dir = Path(tmp)
            sock = runtime_dir / "taarof-99999.sock"
            (runtime_dir / "taarof-current.json").write_text(
                json.dumps({"pid": 2, "socket_path": str(sock)}),
                encoding="utf-8",
            )
            env = {"XDG_RUNTIME_DIR": str(runtime_dir)}
            # Force pid liveness to False so the test is deterministic.
            with (
                patch.dict(os.environ, env, clear=True),
                patch.object(cli, "_pid_alive", lambda pid: False),
            ):
                result = cli.run_doctor_checks(None)
        reg = next(c for c in result["checks"] if c["name"] == "app.registry")
        self.assertEqual(reg["status"], "error")
        self.assertIn("stale registry", reg["detail"])
        self.assertFalse(result["ok"])

    def test_missing_token_reports_missing_token_and_marks_http(self) -> None:
        cli = load_cli_module()
        with tempfile.TemporaryDirectory() as tmp:
            runtime_dir = Path(tmp)
            (runtime_dir / "taarof-current.json").write_text(
                json.dumps({"pid": 4321, "socket_path": str(runtime_dir / "s.sock")}),
                encoding="utf-8",
            )
            facts = {"pid": 4321, "socket_path": None, "alive": True}
            # HTTP enabled config so the checks run (no token file exists).
            # Stub /health so the test does not depend on a local server.
            env = {"XDG_RUNTIME_DIR": str(runtime_dir)}

            class FakeResponse:
                def __enter__(self):
                    return self

                def __exit__(self, *a):
                    return False

                def read(self):
                    return b"{}"

            with (
                patch.dict(os.environ, env, clear=True),
                patch.object(cli.urllib.request, "urlopen",
                             lambda *a, **k: FakeResponse()),
            ):
                records = cli.check_http(
                    facts, {"http": {"enabled": True}}, None, None, timeout=0.2)
        by_name = {r["name"]: r for r in records}
        self.assertEqual(by_name["http.token"]["status"], "error")
        self.assertIn("missing token", by_name["http.token"]["detail"])
        # With no token, the authenticated state probe is skipped, not errored.
        self.assertEqual(by_name["http.state"]["status"], "skip")

    def test_http_disabled_skips_with_hint(self) -> None:
        cli = load_cli_module()
        with patch.dict(os.environ, {}, clear=True):
            records = cli.check_http({"pid": None}, {"http": {"enabled": False}}, None, None)
        health = next(r for r in records if r["name"] == "http.health")
        self.assertEqual(health["status"], "skip")
        self.assertIn("enabled = true", health["detail"])

    def test_json_output_shape(self) -> None:
        cli = load_cli_module()
        with tempfile.TemporaryDirectory() as tmp:
            env = {
                "XDG_RUNTIME_DIR": tmp,
                "TAAROF_SOCK": str(Path(tmp) / "nope.sock"),
            }
            with patch.dict(os.environ, env, clear=True):
                rc, out = self._run_doctor(cli, ["doctor", "--format", "json"])
        self.assertEqual(rc, 1)  # registry missing -> error -> exit 1
        payload = json.loads(out)
        self.assertIn("ok", payload)
        self.assertIn("checks", payload)
        self.assertIsInstance(payload["checks"], list)
        self.assertTrue(all({"name", "status", "detail"} <= set(c) for c in payload["checks"]))
        self.assertTrue(all(c["status"] in cli.DOCTOR_STATUSES for c in payload["checks"]))

    def test_table_output_renders(self) -> None:
        cli = load_cli_module()
        with tempfile.TemporaryDirectory() as tmp:
            env = {
                "XDG_RUNTIME_DIR": tmp,
                "TAAROF_SOCK": str(Path(tmp) / "nope.sock"),
            }
            with patch.dict(os.environ, env, clear=True):
                rc, out = self._run_doctor(cli, ["doctor", "--format", "table"])
        self.assertEqual(rc, 1)
        self.assertIn("status", out)
        self.assertIn("check", out)
        self.assertIn("app.registry", out)
        self.assertIn("overall:", out)
        # Not JSON.
        self.assertNotIn('{"name"', out)

    def test_hosts_not_probed_by_default(self) -> None:
        cli = load_cli_module()
        cfg = {"hosts": {"box": {"address": "user@box.example.com"}}}
        records = cli.check_hosts(cfg, None, probe=False)
        rec = next(r for r in records if r["name"] == "hosts.box")
        self.assertEqual(rec["status"], "skip")
        self.assertIn("--probe-hosts", rec["detail"])

    def test_update_check_maps_current_pending_unknown_and_missing(self) -> None:
        cli = load_cli_module()
        facts = {"alive": True, "socket_path": "/tmp/taarof.sock"}

        def response(state=None, include=True):
            update = {
                "state": state,
                "reason": "content_mismatch" if state == "update_pending" else None,
                "running": {"sha256": "a" * 64},
                "installed": {"sha256": "b" * 64, "path": "/opt/taarof-app"},
                "checked_at_unix_ms": 123,
            }
            return {"ok": True, "data": {"update": update} if include else {}}

        for state, expected in [
            ("current", "ok"),
            ("update_pending", "warn"),
            ("unknown", "skip"),
        ]:
            with patch.object(cli, "socket_send", lambda *a, _state=state, **k: response(_state)):
                record = cli.check_update(facts, None)
            self.assertEqual(record["status"], expected)
            self.assertEqual(record["state"], state)
            self.assertEqual(record["installed_path"], "/opt/taarof-app")
            self.assertIn("restart is operator-controlled", record["detail"])

        with patch.object(cli, "socket_send", lambda *a, **k: response(include=False)):
            self.assertEqual(cli.check_update(facts, None)["status"], "skip")

        malformed = response("unknown")
        malformed["data"]["update"].update({
            "state": {},
            "reason": [],
            "checked_at_unix_ms": True,
        })
        with patch.object(cli, "socket_send", lambda *a, **k: malformed):
            record = cli.check_update(facts, None)
        self.assertEqual(record["status"], "skip")
        self.assertEqual(record["state"], "unknown")
        self.assertIsNone(record["reason"])
        self.assertIsNone(record["checked_at_unix_ms"])

    def test_runtime_probe_doctor_distinguishes_absent_partial_failed_and_recovery(self) -> None:
        cli = load_cli_module()
        facts = {"alive": True, "socket_path": "/tmp/taarof.sock"}

        def response(state, process="unknown", ports="unknown", reason=None):
            return {
                "ok": True,
                "data": {
                    "runtime_probe": {
                        "state": state,
                        "process": {
                            "state": process,
                            "age_ms": 4_000 if process == "stale" else 0,
                            "error": reason,
                        },
                        "ports": {"state": ports, "age_ms": 0, "error": None},
                    }
                },
            }

        cases = [
            (response("absent"), "skip"),
            (response("partial", "stale", "ok", "process source unreadable"), "warn"),
            (response("failed", "error", "error", "runtime probe worker panicked"), "error"),
            (response("ok", "ok", "ok"), "ok"),
            ({"ok": True, "data": {}}, "skip"),
        ]
        for payload, expected in cases:
            with patch.object(cli, "socket_send", return_value=payload):
                record = cli.check_runtime_probe(facts, None)
            self.assertEqual(record["status"], expected)

        with patch.object(
            cli,
            "socket_send",
            return_value=response(
                "partial", "stale", "ok", "process source unreadable"
            ),
        ):
            record = cli.check_runtime_probe(facts, None)
        self.assertIn("process=stale", record["detail"])
        self.assertIn("ports=ok", record["detail"])
        self.assertIn("age=4000ms", record["detail"])

    def test_update_warning_does_not_fail_doctor_overall(self) -> None:
        cli = load_cli_module()
        ok = lambda name: cli._check(name, "ok", "ok")
        with (
            patch.object(cli, "check_app_registry", lambda session: (
                ok("app.registry"),
                {"alive": True, "socket_path": "/tmp/taarof.sock"},
            )),
            patch.object(cli, "check_socket", lambda facts, session: ok("socket.reachability")),
            patch.object(cli, "check_update", lambda facts, session: cli._check(
                "app.update", "warn", "restart is operator-controlled"
            )),
            patch.object(cli, "check_runtime_dir", lambda: ok("runtime.permissions")),
            patch.object(cli, "_load_config", lambda: ({}, None)),
            patch.object(cli, "check_http", lambda *args: []),
            patch.object(cli, "check_history", lambda *args: ok("history.storage")),
            patch.object(cli, "check_tmux", lambda: []),
            patch.object(cli, "check_hosts", lambda *args: []),
        ):
            result = cli.run_doctor_checks(None)
        self.assertTrue(result["ok"])
        update = next(c for c in result["checks"] if c["name"] == "app.update")
        self.assertEqual(update["status"], "warn")


class ConfigCommandTests(unittest.TestCase):
    def _run(self, cli, argv):
        """Run the CLI, capturing stdout/stderr and the SystemExit code.

        The config commands print directly and `raise SystemExit(code)`, so we
        catch that instead of relying on main()'s return value.
        """
        out, err = io.StringIO(), io.StringIO()
        code = 0
        extra_err = ""
        with patch.object(cli.sys, "stdout", out), patch.object(cli.sys, "stderr", err):
            try:
                cli.main(argv)
            except SystemExit as exc:
                if isinstance(exc.code, int):
                    code = exc.code
                elif exc.code is None:
                    code = 0
                else:
                    # A string exit message (Python prints it to stderr on real
                    # exit); surface it here so tests can assert on it.
                    code = 1
                    extra_err = str(exc.code)
        return code, out.getvalue(), err.getvalue() + extra_err

    # -- discovery order --------------------------------------------------

    def test_find_binary_prefers_env_override(self) -> None:
        cli = load_cli_module()
        with patch.dict(os.environ, {"TAAROF_APP_BIN": "/opt/custom/taarof-app"}, clear=True):
            self.assertEqual(cli._find_app_binary(), "/opt/custom/taarof-app")

    def test_find_binary_falls_back_to_path(self) -> None:
        cli = load_cli_module()
        env = {k: v for k, v in os.environ.items() if k != "TAAROF_APP_BIN"}
        with tempfile.TemporaryDirectory() as tmp:
            env["XDG_RUNTIME_DIR"] = tmp  # empty -> no registry -> no /proc discovery
            with (
                patch.dict(os.environ, env, clear=True),
                patch("shutil.which", lambda name: "/usr/bin/taarof-app"),
            ):
                self.assertEqual(cli._find_app_binary(), "/usr/bin/taarof-app")

    def test_find_binary_uses_running_app_exe(self) -> None:
        cli = load_cli_module()
        with tempfile.TemporaryDirectory() as tmp:
            runtime_dir = Path(tmp)
            exe = runtime_dir / "running-taarof-app"
            exe.write_text("#!/bin/sh\n", encoding="utf-8")
            (runtime_dir / "taarof-current.json").write_text(
                json.dumps({"pid": 4242, "socket_path": str(runtime_dir / "s.sock")}),
                encoding="utf-8",
            )
            env = {k: v for k, v in os.environ.items() if k != "TAAROF_APP_BIN"}
            env["XDG_RUNTIME_DIR"] = str(runtime_dir)
            with (
                patch.dict(os.environ, env, clear=True),
                patch.object(cli, "_pid_exe", lambda pid: str(exe)),
                patch("shutil.which", lambda name: "/usr/bin/taarof-app"),
            ):
                # /proc discovery wins over PATH.
                self.assertEqual(cli._find_app_binary(), str(exe))

    # -- binary-missing fallbacks ----------------------------------------

    def test_config_path_fallback_when_binary_missing(self) -> None:
        cli = load_cli_module()
        with patch.object(cli, "_find_app_binary", lambda: None):
            code, out, err = self._run(cli, ["config", "path"])
        self.assertEqual(code, 0)
        self.assertIn(".config/taarof/config.toml", out)
        self.assertIn(".config/taarof/keybindings.toml", out)
        self.assertIn("not found", err)

    def test_config_default_fallback_when_binary_missing(self) -> None:
        cli = load_cli_module()
        with patch.object(cli, "_find_app_binary", lambda: None):
            code, out, err = self._run(cli, ["config", "default"])
        self.assertEqual(code, 0)
        self.assertIn("fallback template", out)
        self.assertIn("not found", err)

    def test_config_validate_errors_when_binary_missing(self) -> None:
        cli = load_cli_module()
        with patch.object(cli, "_find_app_binary", lambda: None):
            code, out, err = self._run(cli, ["config", "validate"])
        self.assertEqual(code, 1)
        self.assertIn("taarof-app not found", err + out)

    # -- --write refuses to overwrite ------------------------------------

    def test_config_default_write_refuses_overwrite_without_force(self) -> None:
        cli = load_cli_module()
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            dest = home / ".config" / "taarof" / "config.toml"
            dest.parent.mkdir(parents=True)
            dest.write_text("existing = true\n", encoding="utf-8")
            with (
                patch.object(cli, "_find_app_binary", lambda: None),
                patch.object(cli, "_CONVENTIONAL_CONFIG_PATH", dest),
            ):
                code, out, err = self._run(cli, ["config", "default", "--write"])
            self.assertEqual(code, 1)
            self.assertIn("refusing to overwrite", err + out)
            # Original content untouched.
            self.assertEqual(dest.read_text(encoding="utf-8"), "existing = true\n")

    def test_config_default_write_overwrites_with_force(self) -> None:
        cli = load_cli_module()
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            dest = home / ".config" / "taarof" / "config.toml"
            dest.parent.mkdir(parents=True)
            dest.write_text("existing = true\n", encoding="utf-8")
            with (
                patch.object(cli, "_find_app_binary", lambda: None),
                patch.object(cli, "_CONVENTIONAL_CONFIG_PATH", dest),
            ):
                code, out, err = self._run(cli, ["config", "default", "--write", "--force"])
            self.assertEqual(code, 0)
            self.assertNotEqual(dest.read_text(encoding="utf-8"), "existing = true\n")
            self.assertIn("fallback template", dest.read_text(encoding="utf-8"))

    # -- validate passthrough exit code (fake executable) ----------------

    def _fake_app_bin(self, tmp: str, *, rc: int, stdout: str) -> str:
        script = Path(tmp) / "fake-taarof-app"
        script.write_text(
            "#!/bin/sh\n"
            f'printf %s "{stdout}"\n'
            f"exit {rc}\n",
            encoding="utf-8",
        )
        script.chmod(script.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)
        return str(script)

    def test_config_validate_mirrors_binary_exit_zero(self) -> None:
        cli = load_cli_module()
        with tempfile.TemporaryDirectory() as tmp:
            fake = self._fake_app_bin(tmp, rc=0, stdout="config: valid\\n")
            with patch.dict(os.environ, {"TAAROF_APP_BIN": fake}, clear=True):
                code, out, err = self._run(cli, ["config", "validate"])
        self.assertEqual(code, 0)
        self.assertIn("config: valid", out)

    def test_config_validate_mirrors_binary_exit_one(self) -> None:
        cli = load_cli_module()
        with tempfile.TemporaryDirectory() as tmp:
            fake = self._fake_app_bin(tmp, rc=1, stdout="config.toml: error: bad\\n")
            with patch.dict(os.environ, {"TAAROF_APP_BIN": fake}, clear=True):
                code, out, err = self._run(cli, ["config", "validate"])
        self.assertEqual(code, 1)
        self.assertIn("error: bad", out)

    def test_config_validate_passes_json_flag_to_binary(self) -> None:
        cli = load_cli_module()
        captured: dict = {}

        def fake_run(app_bin, args):
            captured["args"] = args

            class R:
                returncode = 0
                stdout = '{"ok":true,"findings":[]}'
                stderr = ""

            return R()

        with (
            patch.object(cli, "_find_app_binary", lambda: "/fake/taarof-app"),
            patch.object(cli, "_run_app_binary", fake_run),
        ):
            code, out, err = self._run(cli, ["config", "validate", "--format", "json"])
        self.assertEqual(code, 0)
        self.assertIn("--json", captured["args"])
        # Default (no --format) should NOT pass --json (human table default).
        with (
            patch.object(cli, "_find_app_binary", lambda: "/fake/taarof-app"),
            patch.object(cli, "_run_app_binary", fake_run),
        ):
            self._run(cli, ["config", "validate"])
        self.assertNotIn("--json", captured["args"])


class AttachSessionHostTests(unittest.TestCase):
    def _capture(self, cli, argv):
        calls = []

        def fake_socket_send(message, session=None):
            calls.append(message)
            return {"ok": True}

        with (
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli, "_emit"),
        ):
            rc = cli.main(argv)
        return rc, calls

    def test_attach_session_omits_host_and_ssh_target_by_default(self) -> None:
        cli = load_cli_module()
        rc, calls = self._capture(cli, ["attach-session", "work"])
        self.assertEqual(rc, 0)
        self.assertEqual(calls, [{"action": "attach-session", "session_name": "work"}])
        # Explicitly: no null keys leaked.
        self.assertNotIn("host", calls[0])
        self.assertNotIn("ssh_target", calls[0])

    def test_attach_session_includes_host_and_ssh_target_when_given(self) -> None:
        cli = load_cli_module()
        rc, calls = self._capture(
            cli,
            ["attach-session", "work", "--host", "box", "--ssh-target", "me@box"],
        )
        self.assertEqual(rc, 0)
        self.assertEqual(
            calls,
            [{
                "action": "attach-session",
                "session_name": "work",
                "host": "box",
                "ssh_target": "me@box",
            }],
        )

    def test_tmux_attach_grouped_alias_shares_host_options(self) -> None:
        cli = load_cli_module()
        # The grouped `tmux attach` now builds a merged inventory first (three
        # read queries), then attaches. With no known session, a bare name +
        # --host falls through to the historical direct-attach fast path.
        rc, calls = self._capture(
            cli, ["tmux", "attach", "work", "--host", "box"])
        self.assertEqual(rc, 0)
        self.assertEqual(
            calls[-1],
            {"action": "attach-session", "session_name": "work", "host": "box"},
        )
        self.assertNotIn("ssh_target", calls[-1])


class AgentStatusStateTests(unittest.TestCase):
    def _capture(self, cli, argv):
        calls = []

        def fake_socket_send(message, session=None):
            calls.append(message)
            return {"ok": True}

        with (
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli, "_emit"),
        ):
            rc = cli.main(argv)
        return rc, calls

    def test_agent_status_accepts_waiting_input_wire_name(self) -> None:
        cli = load_cli_module()
        rc, calls = self._capture(
            cli, ["agent-status", "--state", "waiting-input"])
        self.assertEqual(rc, 0)
        self.assertEqual(calls[0]["action"], "agent-status")
        self.assertEqual(calls[0]["state"], "waiting-input")

    def test_agent_status_forwards_stable_pane_target(self) -> None:
        cli = load_cli_module()
        rc, calls = self._capture(
            cli,
            ["agent-status", "--tab", "current", "--pane", "7", "--state", "running"],
        )
        self.assertEqual(rc, 0)
        self.assertEqual(calls[0]["tab"], "current")
        self.assertEqual(calls[0]["pane"], 7)

    def test_agent_status_accepts_errored_wire_name(self) -> None:
        cli = load_cli_module()
        rc, calls = self._capture(
            cli, ["agent", "status", "--state", "errored"])
        self.assertEqual(rc, 0)
        self.assertEqual(calls[0]["state"], "errored")

    def test_agent_status_rejects_unknown_state(self) -> None:
        cli = load_cli_module()
        with self.assertRaises(SystemExit):
            cli.build_parser().parse_args(["agent-status", "--state", "nope"])


class AgentWorkspaceTimeoutTests(unittest.TestCase):
    def _capture(self, cli, argv):
        calls = []

        def fake_socket_send(message, session=None, timeout=10.0):
            calls.append((message, session, timeout))
            return {"ok": True}

        with (
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli, "_emit"),
        ):
            rc = cli.main(argv)
        return rc, calls

    def test_agent_workspace_default_timeout_is_sent_to_socket_and_server(self) -> None:
        cli = load_cli_module()
        rc, calls = self._capture(cli, ["agent-workspace", "--branch", "feature/default"])

        self.assertEqual(rc, 0)
        self.assertEqual(calls, [(
            {
                "action": "agent-workspace",
                "branch": "feature/default",
                "timeout_seconds": 120.0,
            },
            None,
            120.0,
        )])

    def test_agent_workspace_timeout_override_is_sent_to_socket_and_server(self) -> None:
        cli = load_cli_module()
        rc, calls = self._capture(
            cli,
            ["agent-workspace", "--branch", "feature/override", "--timeout", "7.5"],
        )

        self.assertEqual(rc, 0)
        self.assertEqual(calls, [(
            {
                "action": "agent-workspace",
                "branch": "feature/override",
                "timeout_seconds": 7.5,
            },
            None,
            7.5,
        )])


class WorkReportingTests(unittest.TestCase):
    CONTEXT = {
        "TAAROF_WORK_SESSION": "default",
        "TAAROF_WORK_WORKSPACE_ORIGIN": "workspace-fixture",
        "TAAROF_WORK_TAB_ORIGIN": "tab-fixture",
        "TAAROF_WORK_PANE_ORIGIN": "pane-fixture",
        "TAAROF_WORK_TASK_ID": "EXAMPLE-112",
        "TAAROF_WORK_TASK_TITLE": "Explicit work reporting",
        "TAAROF_WORK_CHECKOUT_ROOT": "/repo",
        "TAAROF_WORK_BINDING_TOKEN": "ctx-11111111111111111111111111111111",
        "TAAROF_WORK_INSTRUCTIONS": "fixture instructions",
    }

    def test_named_session_registry_keys_match_app_and_do_not_collide(self) -> None:
        cli = load_cli_module()
        slash = cli._session_storage_key("dev/tools")
        space = cli._session_storage_key("dev tools")
        self.assertNotEqual(slash, space)
        self.assertRegex(slash, r"^dev-tools-[0-9a-f]{32}$")
        self.assertNotEqual(
            cli._session_storage_key("a?={$}%[)^,b"),
            cli._session_storage_key(r"a#\,|~^@:?{b"),
        )
        self.assertEqual(
            cli._session_storage_key("a?={$}%[)^,b"),
            "a-b-625bb59eb804ec5479e63735527b8d3b",
        )
        self.assertEqual(
            cli._session_storage_key(r"a#\,|~^@:?{b"),
            "a-b-586c365797efde941acc38aae9d0f2f9",
        )
        with patch.object(cli, "_runtime_dir", return_value=Path("/runtime")):
            self.assertEqual(
                cli._registry_path("dev/tools"),
                Path("/runtime") / f"taarof-current-{slash}.json",
            )

    def test_session_storage_key_trims_unicode_and_treats_whitespace_as_default(self) -> None:
        cli = load_cli_module()
        cat = cli._session_storage_key("🐈")
        self.assertEqual(cat, "f09f9088-65607de0a69ab2c3979891a88d8ce774")
        self.assertEqual(cat, cli._session_storage_key("  🐈  "))
        self.assertRegex(cat, r"^f09f9088-[0-9a-f]{32}$")
        self.assertEqual(cli._session_storage_key("   "), "default")
        with patch.object(cli, "_runtime_dir", return_value=Path("/runtime")):
            self.assertEqual(cli._registry_path("   "), Path("/runtime/taarof-current.json"))

    def test_very_long_ascii_and_unicode_session_keys_are_bounded_and_match_rust(self) -> None:
        cli = load_cli_module()
        cases = {
            "A" * 1000: (
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA-"
                "c2e686823489ced2017f6059b8b23931"
            ),
            "界" * 1000: (
                "e7958ce7958ce7958ce7958ce7958ce7958ce7958ce7958c-"
                "a9df9287ae828284ce75d277cad80a38"
            ),
        }
        for raw, expected in cases.items():
            key = cli._session_storage_key(raw)
            self.assertEqual(key, expected)
            self.assertLessEqual(len(key.encode()), 81)
            for component in (
                f"session-{key}.json",
                f"work-ledger-{key}.json",
                f"taarof-current-{key}.json",
                f"diagnostics-{key}.jsonl",
            ):
                self.assertTrue(component.isascii())
                self.assertLessEqual(len(component.encode()), 255)

    def test_agent_fixture_executes_documented_commands_verbatim(self) -> None:
        cli = load_cli_module()
        fixture = Path(__file__).parents[1] / "fixtures" / "agent-work-report.sh"
        if not fixture.exists():
            self.skipTest("private agent fixture is not part of the public export")
        commands = [
            line.strip() for line in fixture.read_text(encoding="utf-8").splitlines()
            if line.startswith("taarof work-report ")
        ]
        self.assertEqual(len(commands), 4)
        calls = []

        def fake_socket_send(message, session=None):
            calls.append((message, session))
            return {"ok": True}

        with (
            patch.dict(os.environ, self.CONTEXT, clear=True),
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli, "_emit"),
        ):
            for command in commands:
                self.assertEqual(cli.main(shlex.split(command)[1:]), 0)

        self.assertEqual(
            [call[0]["milestone"] for call in calls],
            ["started", "progress", "blocked", "finished"],
        )
        self.assertTrue(all(call[0]["action"] == "work-report" for call in calls))
        self.assertTrue(all(call[0]["task_id"] == "EXAMPLE-112" for call in calls))
        self.assertTrue(all("task_title" not in call[0] for call in calls))
        self.assertTrue(all(call[1] == "default" for call in calls))

    def test_work_report_explicit_session_must_match_context_before_socket(self) -> None:
        cli = load_cli_module()
        calls = []
        with (
            patch.dict(os.environ, {**self.CONTEXT, "TAAROF_WORK_SESSION": "dev"}, clear=True),
            patch.object(cli, "socket_send", lambda *args, **kwargs: calls.append((args, kwargs))),
        ):
            with self.assertRaisesRegex(SystemExit, "does not match"):
                cli.main(["--session", "other", "work-report", "started"])
        self.assertEqual(calls, [])

    def test_work_report_matching_explicit_session_routes_to_that_socket(self) -> None:
        cli = load_cli_module()
        calls = []

        def fake_socket_send(message, session=None):
            calls.append((message, session))
            return {"ok": True}

        with (
            patch.dict(os.environ, {**self.CONTEXT, "TAAROF_WORK_SESSION": "dev"}, clear=True),
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli, "_emit"),
        ):
            self.assertEqual(
                cli.main(["--session", "dev", "work-report", "progress"]), 0
            )
        self.assertEqual(calls[0][1], "dev")

    def test_work_report_stops_when_context_is_missing(self) -> None:
        cli = load_cli_module()
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(SystemExit) as raised:
                cli.main(["work-report", "started"])
        message = str(raised.exception)
        safe_workflow = (
            "SESSION='your-named-session'\n"
            'taarof --session "$SESSION" panes\n'
            'eval "$(taarof --session "$SESSION" work-context '
            '--tab <numeric-tab-id> --pane <numeric-pane-id> --shell)"'
        )
        self.assertIn("Stop and do not guess", message)
        self.assertIn(safe_workflow, message)
        self.assertNotIn("current", message)

    def test_manual_work_context_prints_shell_exports(self) -> None:
        cli = load_cli_module()
        response = {
            "ok": True,
            "data": {
                "session": "dev",
                "workspace_origin": "workspace-a",
                "tab_origin": "tab-a",
                "pane_origin": "pane-a",
                "task_id": "EXAMPLE-112",
                "task_title": "Agent's reporting contract",
                "checkout_root": "/repo/work tree",
                "binding_token": "ctx-11111111111111111111111111111111",
                "instructions": "Stop; don't guess.",
            },
        }
        sent = []
        out = io.StringIO()

        def fake_socket_send(message, session=None):
            sent.append((message, session))
            return response

        with (
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli.sys, "stdout", out),
        ):
            self.assertEqual(
                cli.main([
                    "--session", "dev", "work-context", "--tab", "17", "--pane", "3",
                    "--shell",
                ]),
                0,
            )
        self.assertEqual(
            sent,
            [({"action": "work-context", "pane": 3, "tab": "17"}, "dev")],
        )
        exports = out.getvalue()
        self.assertIn("export TAAROF_WORK_TASK_ID=EXAMPLE-112", exports)
        self.assertIn("TAAROF_WORK_TASK_TITLE='Agent'\"'\"'s reporting contract'", exports)
        self.assertNotIn("tab_id", exports)

    def test_work_context_requires_session_and_positive_numeric_ids(self) -> None:
        cli = load_cli_module()
        with self.assertRaisesRegex(SystemExit, "requires explicit --session"):
            cli.main(["work-context", "--tab", "17", "--pane", "3", "--shell"])
        for bad in ("current", "Shell", "0", "-1"):
            with self.assertRaises(SystemExit):
                cli.build_parser().parse_args([
                    "--session", "dev", "work-context", "--tab", bad, "--pane", "3",
                    "--shell",
                ])
        with self.assertRaises(SystemExit):
            cli.build_parser().parse_args([
                "--session", "dev", "work-context", "--tab", "17", "--pane", "0",
                "--shell",
            ])

    def test_failed_shell_context_is_inert_for_eval(self) -> None:
        cli = load_cli_module()
        out = io.StringIO()
        err = io.StringIO()
        with (
            patch.object(cli, "socket_send", return_value={"ok": False, "error": "wrong tab"}),
            patch.object(cli.sys, "stdout", out),
            patch.object(cli.sys, "stderr", err),
        ):
            rc = cli.main([
                "--session", "dev", "work-context", "--tab", "17", "--pane", "3",
                "--shell",
            ])
        self.assertEqual(rc, 1)
        self.assertEqual(out.getvalue(), "")
        self.assertIn("wrong tab", err.getvalue())

    def test_agent_fixture_commands_match_docs_and_agents_symlink(self) -> None:
        root = Path(__file__).parents[1]
        agents = root / "AGENTS.md"
        claude = root / "CLAUDE.md"
        skill = root / "agent-skills" / "legacy-app" / "SKILL.md"
        if not agents.exists() or not claude.exists() or not skill.exists():
            self.skipTest("private agent instructions are not part of the public export")
        self.assertTrue(agents.is_symlink())
        self.assertEqual(agents.resolve(), claude.resolve())
        self.assertEqual(agents.read_bytes(), claude.read_bytes())
        fixture_commands = [
            line for line in (root / "fixtures" / "agent-work-report.sh")
            .read_text(encoding="utf-8").splitlines()
            if line.startswith("taarof work-report ")
        ]
        # The instruction entrypoints link to progressively disclosed command
        # docs. Verify that route and the actual reference, rather than requiring
        # every entrypoint to duplicate the command bodies.
        reference = skill.parent / "reference" / "agent-integration.md"
        self.assertIn("agent-skills/legacy-app/SKILL.md", claude.read_text(encoding="utf-8"))
        self.assertIn("reference/agent-integration.md", skill.read_text(encoding="utf-8"))
        reference_commands = [
            shlex.split(line) for line in reference.read_text(encoding="utf-8").splitlines()
            if line.startswith("taarof work-report ")
        ]
        self.assertTrue(fixture_commands)
        for command in fixture_commands:
            self.assertIn(shlex.split(command), reference_commands)
        safe_discovery = 'taarof --session "$SESSION" list-tabs --pretty'
        safe_eval = (
            'eval "$(taarof --session "$SESSION" work-context '
            '--tab "$NUMERIC_TAB_ID" --pane "$NUMERIC_PANE_ID" --shell)"'
        )
        for document in (reference, root / "fixtures" / "agent-work-report.sh"):
            contents = document.read_text(encoding="utf-8")
            self.assertIn(safe_discovery, contents)
            self.assertIn(safe_eval, contents)
            self.assertNotIn("work-context --tab current", contents)


class WorkspaceLifecycleTests(unittest.TestCase):
    def _capture(self, cli, argv):
        calls = []

        def fake_socket_send(message, session=None):
            calls.append(message)
            return {"ok": True}

        with (
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli, "_emit"),
        ):
            rc = cli.main(argv)
        return rc, calls

    def test_workspace_create_without_name_omits_name(self) -> None:
        cli = load_cli_module()
        rc, calls = self._capture(cli, ["workspace", "create"])
        self.assertEqual(rc, 0)
        self.assertEqual(calls, [{"action": "create-workspace"}])

    def test_workspace_create_with_name(self) -> None:
        cli = load_cli_module()
        rc, calls = self._capture(cli, ["workspace", "create", "feature"])
        self.assertEqual(rc, 0)
        self.assertEqual(calls, [{"action": "create-workspace", "name": "feature"}])

    def test_workspace_switch_payload(self) -> None:
        cli = load_cli_module()
        rc, calls = self._capture(cli, ["workspace", "switch", "2"])
        self.assertEqual(rc, 0)
        self.assertEqual(calls, [{"action": "switch-workspace", "workspace": "2"}])

    def test_workspace_rename_payload(self) -> None:
        cli = load_cli_module()
        rc, calls = self._capture(cli, ["workspace", "rename", "feature", "renamed"])
        self.assertEqual(rc, 0)
        self.assertEqual(
            calls, [{"action": "rename-workspace", "workspace": "feature", "name": "renamed"}]
        )

    def test_workspace_switch_requires_target(self) -> None:
        cli = load_cli_module()
        with self.assertRaises(SystemExit):
            cli.build_parser().parse_args(["workspace", "switch"])

    def test_workspace_rename_requires_new_name(self) -> None:
        cli = load_cli_module()
        with self.assertRaises(SystemExit):
            cli.build_parser().parse_args(["workspace", "rename", "feature"])


class WorkspaceManifestTests(unittest.TestCase):
    @staticmethod
    def _state_for_pane(pane: dict) -> dict:
        return {
            "ok": True,
            "data": {
                "workspaces": [{
                    "id": 1,
                    "name": "main",
                    "tabs": [{"tab_id": 10, "name": "shell", "panes": [pane]}],
                }],
            },
        }

    def _export_then_dry_run_apply(self, cli, state: dict) -> tuple[str, dict]:
        with tempfile.TemporaryDirectory() as tmp:
            manifest_path = Path(tmp) / "workspace.toml"
            with (
                patch.object(cli, "socket_send", return_value=state),
                patch.object(cli, "_emit"),
            ):
                self.assertEqual(
                    cli.main(["workspace", "export", "--out", str(manifest_path)]),
                    0,
                )
            manifest = manifest_path.read_text(encoding="utf-8")

            with patch.object(cli, "_emit") as emit:
                self.assertEqual(
                    cli.main(["workspace", "apply", str(manifest_path), "--dry-run"]),
                    0,
                )
            return manifest, emit.call_args.args[0]

    def test_local_osc7_tab_round_trip_stays_local(self) -> None:
        cli = load_cli_module()
        state = self._state_for_pane({
            "pane_id": 0,
            "cwd": "/work/local-repo",
            "cwd_host": "devbox",
            "remote_shell": False,
        })

        manifest, report = self._export_then_dry_run_apply(cli, state)

        self.assertNotIn("remote =", manifest)
        self.assertEqual(
            report["workspaces"][0]["tabs"][0]["payload"],
            {
                "action": "create-tab",
                "name": "shell",
                "working_dir": "/work/local-repo",
            },
        )

    def test_remote_tab_round_trip_preserves_ssh_target(self) -> None:
        cli = load_cli_module()
        state = self._state_for_pane({
            "pane_id": 0,
            "cwd": "/srv/remote-repo",
            "cwd_host": "remote.example",
            "remote_shell": True,
        })

        manifest, report = self._export_then_dry_run_apply(cli, state)

        self.assertIn('remote = "remote.example"', manifest)
        payload = report["workspaces"][0]["tabs"][0]["payload"]
        self.assertNotIn("working_dir", payload)
        self.assertTrue(payload["command"].startswith("ssh -t remote.example "))
        self.assertIn("cd /srv/remote-repo", payload["command"])

    def test_apply_warns_but_preserves_ssh_to_local_hostname(self) -> None:
        cli = load_cli_module()
        with tempfile.TemporaryDirectory() as tmp:
            manifest_path = Path(tmp) / "legacy.toml"
            manifest_path.write_text(
                """[[workspace]]
name = "main"

[[workspace.tab]]
name = "shell"
remote = "devbox"
cwd = "/work/local-repo"
""",
                encoding="utf-8",
            )
            with (
                patch.object(cli.socket, "gethostname", return_value="devbox"),
                patch.object(cli.socket, "getfqdn", return_value="devbox.example"),
                patch.object(cli, "_emit") as emit,
            ):
                self.assertEqual(
                    cli.main(["workspace", "apply", str(manifest_path), "--dry-run"]),
                    0,
                )

        tab_report = emit.call_args.args[0]["workspaces"][0]["tabs"][0]
        self.assertEqual(
            tab_report["payload"],
            {
                "action": "create-tab",
                "name": "shell",
                "command": "ssh -t devbox 'cd /work/local-repo && exec $SHELL -l'",
            },
        )
        self.assertIn("intentional SSH-to-self", tab_report["warning"])
        self.assertIn("not shareable", tab_report["warning"])

    def test_local_hostname_warning_matches_short_name_to_fqdn(self) -> None:
        cli = load_cli_module()
        with (
            patch.object(cli.socket, "gethostname", return_value="devbox.example"),
            patch.object(cli.socket, "getfqdn", return_value="devbox.example"),
        ):
            self.assertTrue(cli._is_local_hostname("devbox"))
            self.assertTrue(cli._is_local_hostname("DEVBOX.EXAMPLE."))

    def test_remote_without_exportable_target_fails_instead_of_becoming_local(self) -> None:
        cli = load_cli_module()
        state = self._state_for_pane({
            "pane_id": 0,
            "cwd": "/srv/remote-repo",
            "cwd_host": None,
            "remote_shell": True,
            "tmux_host": None,
        })

        with (
            patch.object(cli, "socket_send", return_value=state),
            patch.object(cli, "_emit"),
            self.assertRaisesRegex(SystemExit, "no cwd_host or tmux_host SSH target"),
        ):
            cli.main(["workspace", "export"])

    def test_remote_tmux_prefers_connection_target_over_local_cwd_host(self) -> None:
        cli = load_cli_module()
        state = self._state_for_pane({
            "pane_id": 0,
            "cwd": "/srv/remote-repo",
            "cwd_host": "devbox",
            "remote_shell": True,
            "tmux_host": "builder.example",
            "tmux_session": "remote-session",
        })

        with (
            patch.object(cli.socket, "gethostname", return_value="devbox"),
            patch.object(cli.socket, "getfqdn", return_value="devbox.example"),
        ):
            manifest, report = self._export_then_dry_run_apply(cli, state)

        self.assertIn('remote = "devbox"', manifest)
        self.assertIn('tmux_host = "builder.example"', manifest)
        tab_report = report["workspaces"][0]["tabs"][0]
        self.assertEqual(
            shlex.split(tab_report["payload"]["command"]),
            ["ssh", "-t", "builder.example", "tmux attach -t remote-session"],
        )
        self.assertIn("using tmux_host", tab_report["warning"])
        self.assertNotIn("recreating tab locally", tab_report["warning"])

    def test_remote_apply_quotes_cwd_and_tmux_session_for_remote_shell(self) -> None:
        cli = load_cli_module()
        cwd = "/srv/repo with spaces; touch /tmp/cwd-injected"
        session = "session name; touch /tmp/session-injected"
        with tempfile.TemporaryDirectory() as tmp:
            manifest_path = Path(tmp) / "quoted.toml"
            manifest_path.write_text(
                f'''[[workspace]]
name = "main"

[[workspace.tab]]
name = "tmux"
remote = "remote.example"
tmux_session = "{session}"
cwd = "{cwd}"

[[workspace.tab]]
name = "plain"
remote = "remote.example"
cwd = "{cwd}"
''',
                encoding="utf-8",
            )
            with patch.object(cli, "_emit") as emit:
                self.assertEqual(
                    cli.main(["workspace", "apply", str(manifest_path), "--dry-run"]),
                    0,
                )

        report = emit.call_args.args[0]["workspaces"][0]
        self.assertEqual(
            shlex.split(report["tabs"][0]["payload"]["command"]),
            [
                "ssh",
                "-t",
                "remote.example",
                f"tmux attach -t {shlex.quote(session)}",
            ],
        )
        self.assertEqual(
            shlex.split(report["tabs"][1]["payload"]["command"]),
            [
                "ssh",
                "-t",
                "remote.example",
                f"cd {shlex.quote(cwd)} && exec $SHELL -l",
            ],
        )
        self.assertEqual(
            shlex.split(report["tmux"][0]["command"]),
            [
                "tmux", "has-session", "-t", session, "2>/dev/null", "||",
                "tmux", "new-session", "-d", "-s", session, "-c", cwd,
            ],
        )


class AgentLsRenderTests(unittest.TestCase):
    MULTI_AGENT_STATE = {
        "ok": True,
        "data": {
            "workspaces": [
                {
                    "id": 1,
                    "name": "main",
                    "tabs": [
                        {
                            "tab_id": 10,
                            "name": "dev",
                            "agents": [
                                {
                                    "pane_id": 1,
                                    "agent_name": "claude",
                                    "session_id": "sess-a",
                                    "activity": {"state": "running", "text": "building"},
                                },
                                {
                                    "pane_id": 2,
                                    "agent_name": "codex",
                                    "session_id": "sess-b",
                                    "activity": {"state": "waiting-input", "text": "prompt"},
                                },
                            ],
                        },
                    ],
                }
            ],
        },
    }

    FALLBACK_STATE = {
        "ok": True,
        "data": {
            "workspaces": [
                {
                    "id": 1,
                    "name": "main",
                    "tabs": [
                        {
                            "tab_id": 10,
                            "name": "legacy",
                            # No `agents` key at all (older app).
                            "agent_name": "claude",
                            "agent_pane_id": 3,
                            "agent_session_id": "sess-legacy",
                            "agent_activity": {"state": "running", "text": "old"},
                        },
                    ],
                }
            ],
        },
    }

    def test_multi_agent_tab_renders_one_row_per_agent(self) -> None:
        cli = load_cli_module()
        rendered = cli.render_agent_ls(self.MULTI_AGENT_STATE)
        self.assertIsNotNone(rendered)
        lines = rendered.splitlines()
        self.assertEqual(
            lines[0].split(),
            ["agent", "workspace", "tab", "pane", "session_id", "state", "activity"],
        )
        # Two agents -> two data rows after header + separator.
        data_rows = lines[2:]
        self.assertEqual(len(data_rows), 2)
        self.assertIn("claude", data_rows[0])
        self.assertIn("running", data_rows[0])
        self.assertIn("codex", data_rows[1])
        self.assertIn("waiting-input", data_rows[1])

    def test_fallback_renders_from_tab_level_fields(self) -> None:
        cli = load_cli_module()
        rendered = cli.render_agent_ls(self.FALLBACK_STATE)
        self.assertIsNotNone(rendered)
        lines = rendered.splitlines()
        data_rows = lines[2:]
        self.assertEqual(len(data_rows), 1)
        self.assertIn("claude", data_rows[0])
        self.assertIn("sess-legacy", data_rows[0])
        self.assertIn("running", data_rows[0])

    def test_empty_state_reports_no_live_agents(self) -> None:
        cli = load_cli_module()
        rendered = cli.render_agent_ls({"ok": True, "data": {"workspaces": []}})
        self.assertEqual(rendered, "no live agents")


class AgentSessionsTests(unittest.TestCase):
    def test_agent_sessions_sends_query_agent_sessions_payload(self) -> None:
        cli = load_cli_module()
        calls = []

        def fake_socket_send(message, session=None):
            calls.append(message)
            return {"ok": True, "data": {"providers": [], "sessions": []}}

        with (
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli, "_emit"),
        ):
            rc = cli.main(["agent", "sessions"])
        self.assertEqual(rc, 0)
        self.assertEqual(calls, [{"action": "query-agent-sessions"}])

    def test_agent_sessions_renders_snapshot_table(self) -> None:
        cli = load_cli_module()
        snapshot = {
            "ok": True,
            "data": {
                "schema": "agent-sessions/1",
                "generated_at_unix_ms": 1,
                "providers": [
                    {"name": "claude", "ok": True, "history_available": True,
                     "session_count": 2},
                ],
                "sessions": [
                    {
                        "agent": "claude",
                        "session_id": "abc",
                        "status": "idle",
                        "cwd": "/tmp/user/repo",
                        "live_binding": {"tab_id": 10, "tab_name": "dev"},
                    },
                ],
            },
        }
        rendered = cli.render_agent_sessions(snapshot)
        self.assertIsNotNone(rendered)
        self.assertIn("claude", rendered)
        self.assertIn("abc", rendered)
        self.assertIn("dev", rendered)  # live binding tab name
        self.assertIn("providers:", rendered)

    def test_agent_sessions_renders_remote_hosts_and_host_qualified_resume(self) -> None:
        cli = load_cli_module()
        snapshot = {
            "ok": True,
            "data": {
                "schema": "taarof.agent-sessions.v1",
                "generated_at_unix_ms": 1,
                "providers": [],
                "remote_hosts": [
                    {"host": "gpu-box", "ssh_target": "me@gpu-box.ts", "ok": True,
                     "stale": False, "session_count": 1, "dropped_lines": 0,
                     "truncated_files": 0, "warning": "sample truncated"},
                ],
                "sessions": [
                    {
                        "agent": "claude", "session_id": "local-1", "title": "t",
                        "cwd": "/tmp/user/repo", "updated_at_unix_ms": 1,
                        "status": "idle",
                        "resume_command": "claude --resume local-1",
                    },
                    {
                        "agent": "claude", "session_id": "remote-1", "title": "t",
                        "cwd": "/tmp/user/other", "updated_at_unix_ms": 1,
                        "status": "idle", "host": "gpu-box",
                        "resume_command": "ssh -t me@gpu-box.ts claude --resume remote-1",
                    },
                ],
            },
        }
        rendered = cli.render_agent_sessions(snapshot)
        self.assertIsNotNone(rendered)
        self.assertIn("remote hosts:", rendered)
        self.assertIn("gpu-box", rendered)
        self.assertIn("sample truncated", rendered)
        # Sessions table carries a host column and the verbatim resume command,
        # so the remote one stays visibly host-qualified.
        self.assertIn("host", rendered)
        self.assertIn("ssh -t me@gpu-box.ts claude --resume remote-1", rendered)
        self.assertIn("claude --resume local-1", rendered)

    def test_agent_sessions_omits_remote_host_table_when_absent(self) -> None:
        cli = load_cli_module()
        rendered = cli.render_agent_sessions({
            "ok": True,
            "data": {"schema": "taarof.agent-sessions.v1", "providers": [],
                     "sessions": []},
        })
        self.assertIsNotNone(rendered)
        self.assertNotIn("remote hosts:", rendered)

    def test_agent_sessions_renderer_declines_unexpected_shape(self) -> None:
        cli = load_cli_module()
        # No `sessions` key -> return None so main() falls back to JSON.
        self.assertIsNone(cli.render_agent_sessions({"ok": True, "data": {"foo": 1}}))


class TmuxInventoryTests(unittest.TestCase):
    ATTACHED_STATE = {
        "ok": True,
        "data": {
            "workspaces": [
                {
                    "id": 1,
                    "name": "main",
                    "tabs": [
                        {
                            "tab_id": 10,
                            "name": "edit",
                            "panes": [
                                {
                                    "pane_id": 0,
                                    "tmux_session": "taarof-edit-1",
                                    "tmux_host": None,
                                    "cwd": "/tmp/user/repo",
                                    "tmux_probe": {
                                        "current_command": "vim",
                                        "checked_at_unix_ms": 1_000,
                                    },
                                },
                            ],
                        },
                        {
                            "tab_id": 11,
                            "name": "build",
                            "panes": [
                                {
                                    "pane_id": 0,
                                    "tmux_session": "taarof-build-2",
                                    "tmux_host": "me@box",
                                    "cwd": "/srv/app",
                                    "tmux_probe": {"current_command": "cargo"},
                                },
                            ],
                        },
                    ],
                }
            ],
        },
    }

    DETACHED = {
        "ok": True,
        "data": [
            {
                "session_name": "taarof-old-9",
                "host": "me@box",
                "workspace": "main",
                "ssh_target": "me@box",
                "finished": False,
                "last_command": "htop",
                "is_detached": True,
            },
        ],
    }

    DASHBOARD = {
        "ok": True,
        "data": {
            "hosts": [],
            "sessions": [
                {
                    "name": "taarof-dash-3",
                    "host": "",
                    "status": "detached",
                    "command": "top",
                    "started": 500,
                    "is_detached": True,
                },
            ],
        },
    }

    def test_merge_three_sources_dedup_by_session_and_host(self) -> None:
        cli = load_cli_module()
        inv = cli.build_tmux_inventory(
            self.ATTACHED_STATE, self.DETACHED, self.DASHBOARD)
        by_key = {(r["session"], r["host"]): r for r in inv}
        # One row per distinct (session, host): 2 attached + 1 detached + 1 dash.
        self.assertEqual(len(inv), 4)
        edit = by_key[("taarof-edit-1", "")]
        self.assertEqual(edit["state"], "attached")
        self.assertEqual(edit["workspace"], "main")
        self.assertEqual(edit["tab"], "edit")
        self.assertEqual(edit["command"], "vim")
        self.assertEqual(edit["cwd"], "/tmp/user/repo")
        build = by_key[("taarof-build-2", "me@box")]
        self.assertEqual(build["state"], "attached")
        self.assertEqual(build["host"], "me@box")
        old = by_key[("taarof-old-9", "me@box")]
        self.assertEqual(old["state"], "detached")
        self.assertEqual(old["command"], "htop")
        dash = by_key[("taarof-dash-3", "")]
        self.assertEqual(dash["state"], "detached")
        self.assertEqual(dash["command"], "top")

    def test_dedup_merges_attached_and_detached_same_identity(self) -> None:
        cli = load_cli_module()
        # Same (session, host) appears attached AND detached -> single row,
        # attached wins for state, detached fills missing workspace.
        attached = {
            "ok": True,
            "data": {
                "workspaces": [{
                    "id": 1, "name": "",
                    "tabs": [{
                        "tab_id": 1, "name": "t",
                        "panes": [{"pane_id": 0, "tmux_session": "s1",
                                   "tmux_host": None}],
                    }],
                }],
            },
        }
        detached = {"ok": True, "data": [
            {"session_name": "s1", "host": None, "workspace": "ws-from-detached",
             "last_command": "bash", "is_detached": True}]}
        inv = cli.build_tmux_inventory(attached, detached, None)
        self.assertEqual(len(inv), 1)
        row = inv[0]
        self.assertEqual(row["state"], "attached")
        self.assertEqual(row["tab"], "t")
        # workspace was empty on attached side, filled from detached.
        self.assertEqual(row["workspace"], "ws-from-detached")
        self.assertEqual(row["command"], "bash")

    def test_dashboard_localhost_merges_with_attached_null_host(self) -> None:
        cli = load_cli_module()
        # query-state reports local panes with tmux_host null; dashboard-state
        # labels the same session host "localhost". Both are THIS machine and
        # must collapse to ONE row (canonical host ""), keeping the attached
        # state and tab context. (Regression: the session showed up twice.)
        attached = {
            "ok": True,
            "data": {
                "workspaces": [{
                    "id": 1, "name": "taarof",
                    "tabs": [{
                        "tab_id": 8, "name": "TmuxTest",
                        "panes": [{"pane_id": 0,
                                   "tmux_session": "taarof--taarof--t8--0",
                                   "tmux_host": None,
                                   "tmux_probe": {"current_command": "bash"}}],
                    }],
                }],
            },
        }
        dashboard = {"ok": True, "data": {"hosts": [], "sessions": [
            {"name": "taarof--taarof--t8--0", "host": "localhost",
             "status": "Running", "command": "bash", "is_detached": False},
        ]}}
        inv = cli.build_tmux_inventory(attached, None, dashboard)
        self.assertEqual(len(inv), 1)
        row = inv[0]
        self.assertEqual(row["host"], "")
        self.assertEqual(row["state"], "attached")
        self.assertEqual(row["tab"], "TmuxTest")
        self.assertEqual(row["tab_id"], 8)

    def test_local_host_spellings_normalize_but_remote_stays_distinct(self) -> None:
        cli = load_cli_module()
        self.assertEqual(cli._tmux_host_label(None), "")
        self.assertEqual(cli._tmux_host_label(""), "")
        self.assertEqual(cli._tmux_host_label("localhost"), "")
        self.assertEqual(cli._tmux_host_label("LOCALHOST"), "")
        self.assertEqual(cli._tmux_host_label("127.0.0.1"), "")
        self.assertEqual(cli._tmux_host_label("::1"), "")
        self.assertEqual(cli._tmux_host_label("local"), "")
        self.assertEqual(cli._tmux_host_label("me@box"), "me@box")
        self.assertEqual(cli._tmux_host_label(" me@box "), "me@box")

    def test_empty_sources_render_no_sessions(self) -> None:
        cli = load_cli_module()
        inv = cli.build_tmux_inventory(None, None, None)
        self.assertEqual(inv, [])
        self.assertEqual(cli.render_tmux_ls(inv), "no tmux sessions")

    def test_render_ls_has_all_columns_and_omits_unknown_age(self) -> None:
        cli = load_cli_module()
        inv = cli.build_tmux_inventory(self.ATTACHED_STATE, None, None)
        # now_ms fixed so age is deterministic; edit has a probe ts, build none.
        rendered = cli.render_tmux_ls(inv, now_ms=1_000)
        lines = rendered.splitlines()
        self.assertEqual(lines[0].split(), cli.TMUX_LS_COLUMNS)
        self.assertIn("taarof-edit-1", rendered)
        self.assertIn("taarof-build-2", rendered)


class TmuxSelectorTests(unittest.TestCase):
    INV = [
        {"session": "sess-a", "host": "", "ssh_target": None,
         "workspace": "main", "tab": "edit", "pane": 0, "state": "attached",
         "command": "vim", "cwd": "/a", "updated_ms": None},
        {"session": "sess-b", "host": "me@box", "ssh_target": "me@box",
         "workspace": "main", "tab": "build", "pane": 1, "state": "attached",
         "command": "cargo", "cwd": "/b", "updated_ms": None},
        {"session": "sess-c", "host": "", "ssh_target": None,
         "workspace": "other", "tab": "logs", "pane": 0, "state": "detached",
         "command": "tail", "cwd": "/c", "updated_ms": None},
    ]

    def _cli(self):
        return load_cli_module()

    def test_exact_session_name_resolves(self) -> None:
        cli = self._cli()
        match, cands = cli.resolve_tmux_selector(self.INV, positional="sess-b")
        self.assertIsNotNone(match)
        self.assertEqual(match["session"], "sess-b")
        self.assertEqual(len(cands), 1)

    def test_workspace_and_tab_resolves(self) -> None:
        cli = self._cli()
        match, _ = cli.resolve_tmux_selector(
            self.INV, workspace="main", tab="build")
        self.assertIsNotNone(match)
        self.assertEqual(match["session"], "sess-b")

    def test_tab_and_pane_resolves(self) -> None:
        cli = self._cli()
        match, _ = cli.resolve_tmux_selector(self.INV, tab="edit", pane="0")
        self.assertIsNotNone(match)
        self.assertEqual(match["session"], "sess-a")

    def test_unique_tab_name_positional_resolves(self) -> None:
        cli = self._cli()
        match, _ = cli.resolve_tmux_selector(self.INV, positional="logs")
        self.assertIsNotNone(match)
        self.assertEqual(match["session"], "sess-c")

    def test_workspace_tab_positional_form_resolves(self) -> None:
        cli = self._cli()
        match, _ = cli.resolve_tmux_selector(self.INV, positional="other:logs")
        self.assertIsNotNone(match)
        self.assertEqual(match["session"], "sess-c")

    def test_no_match_returns_empty(self) -> None:
        cli = self._cli()
        match, cands = cli.resolve_tmux_selector(self.INV, positional="nope")
        self.assertIsNone(match)
        self.assertEqual(cands, [])

    def test_same_name_two_hosts_is_ambiguous(self) -> None:
        cli = self._cli()
        inv = [
            {"session": "shared", "host": "", "ssh_target": None,
             "workspace": "w", "tab": "t1", "pane": 0, "state": "attached",
             "command": None, "cwd": None, "updated_ms": None},
            {"session": "shared", "host": "me@box", "ssh_target": "me@box",
             "workspace": "w", "tab": "t2", "pane": 0, "state": "detached",
             "command": None, "cwd": None, "updated_ms": None},
        ]
        match, cands = cli.resolve_tmux_selector(inv, positional="shared")
        self.assertIsNone(match)
        self.assertEqual(len(cands), 2)

    def test_host_narrows_ambiguous_name_to_one(self) -> None:
        cli = self._cli()
        inv = [
            {"session": "shared", "host": "", "ssh_target": None,
             "workspace": "w", "tab": "t1", "pane": 0, "state": "attached",
             "command": None, "cwd": None, "updated_ms": None},
            {"session": "shared", "host": "me@box", "ssh_target": "me@box",
             "workspace": "w", "tab": "t2", "pane": 0, "state": "detached",
             "command": None, "cwd": None, "updated_ms": None},
        ]
        match, cands = cli.resolve_tmux_selector(
            inv, positional="shared", host="me@box")
        self.assertIsNotNone(match)
        self.assertEqual(match["host"], "me@box")
        self.assertEqual(len(cands), 1)

    def test_two_tabs_same_name_is_ambiguous(self) -> None:
        cli = self._cli()
        inv = [
            {"session": "s1", "host": "", "ssh_target": None,
             "workspace": "w", "tab": "dup", "pane": 0, "state": "attached",
             "command": None, "cwd": None, "updated_ms": None},
            {"session": "s2", "host": "", "ssh_target": None,
             "workspace": "w", "tab": "dup", "pane": 1, "state": "attached",
             "command": None, "cwd": None, "updated_ms": None},
        ]
        match, cands = cli.resolve_tmux_selector(inv, positional="dup")
        self.assertIsNone(match)
        self.assertEqual(len(cands), 2)


class TmuxAttachCommandTests(unittest.TestCase):
    def _attach(self, cli, inventory, argv):
        calls = []

        def fake_socket_send(message, session=None):
            calls.append(message)
            return {"ok": True}

        with (
            patch.object(cli, "_gather_tmux_inventory", lambda session: inventory),
            patch.object(cli, "socket_send", fake_socket_send),
            patch.object(cli, "_emit"),
        ):
            rc = cli.main(argv)
        return rc, calls

    def test_resolved_detached_forwards_host_in_attach_payload(self) -> None:
        cli = load_cli_module()
        inventory = [
            {"session": "sess-b", "host": "me@box", "ssh_target": "me@box",
             "workspace": "main", "tab": "build", "tab_id": None, "pane": 1,
             "state": "detached", "command": "cargo", "cwd": "/b",
             "updated_ms": None},
        ]
        rc, calls = self._attach(cli, inventory, ["tmux", "attach", "build"])
        self.assertEqual(rc, 0)
        self.assertEqual(
            calls[-1],
            {
                "action": "attach-session",
                "session_name": "sess-b",
                "host": "me@box",
                "ssh_target": "me@box",
            },
        )

    def test_resolved_attached_sends_switch_tab_with_tab_id(self) -> None:
        cli = load_cli_module()
        # attach-session is detached-only app-side; an already-attached session
        # must be focused via switch-tab instead.
        inventory = [
            {"session": "taarof--taarof--t8--0", "host": "", "ssh_target": None,
             "workspace": "taarof", "tab": "TmuxTest", "tab_id": 8, "pane": 0,
             "state": "attached", "command": "bash", "cwd": "/repo",
             "updated_ms": None},
        ]
        rc, calls = self._attach(cli, inventory, ["tmux", "attach", "TmuxTest"])
        self.assertEqual(rc, 0)
        self.assertEqual(calls[-1], {"action": "switch-tab", "tab": "8"})
        # No attach-session was sent at all.
        self.assertNotIn(
            "attach-session", [c.get("action") for c in calls])

    def test_ambiguous_selector_exits_1_with_candidates(self) -> None:
        cli = load_cli_module()
        inventory = [
            {"session": "shared", "host": "", "ssh_target": None,
             "workspace": "w", "tab": "t1", "pane": 0, "state": "attached",
             "command": None, "cwd": None, "updated_ms": None},
            {"session": "shared", "host": "me@box", "ssh_target": "me@box",
             "workspace": "w", "tab": "t2", "pane": 0, "state": "detached",
             "command": None, "cwd": None, "updated_ms": None},
        ]
        err = io.StringIO()
        code = 0
        with (
            patch.object(cli, "_gather_tmux_inventory", lambda session: inventory),
            patch.object(cli.sys, "stderr", err),
        ):
            try:
                cli.main(["tmux", "attach", "shared"])
            except SystemExit as exc:
                code = exc.code if isinstance(exc.code, int) else 1
        self.assertEqual(code, 1)
        self.assertIn("ambiguous", err.getvalue())
        self.assertIn("me@box", err.getvalue())

    def test_no_match_with_workspace_tab_exits_1(self) -> None:
        cli = load_cli_module()
        err = io.StringIO()
        code = 0
        with (
            patch.object(cli, "_gather_tmux_inventory", lambda session: []),
            patch.object(cli.sys, "stderr", err),
        ):
            try:
                cli.main(["tmux", "attach", "--workspace", "w", "--tab", "nope"])
            except SystemExit as exc:
                code = exc.code if isinstance(exc.code, int) else 1
        self.assertEqual(code, 1)
        self.assertIn("no tmux session matches", err.getvalue())


class RuntimeIdentityTests(unittest.TestCase):
    """EXAMPLE-164: binary equality must never be read as source freshness.

    The motivating incident: the installed and freshly built binaries hashed
    identically, so every surface said `current`, while both predated the source
    fix under test. Identity therefore carries two independent answers —
    "is the running binary the installed one" and "was it built from this
    source" — and the second one is `unknown` unless it can be *proved*.
    """

    #: An app that can prove nothing about its own provenance, running the
    #: byte-identical binary that is installed on disk.
    UNPROVABLE_SOURCE = {
        "schema": "taarof.identity.v1",
        "app_version": "0.1.0",
        "build": {
            "source_revision": None,
            "source_describe": None,
            "source_dirty": None,
            "profile": "release",
        },
        "source": {"root": "/src/legacy-app", "revision": "f" * 40, "dirty": False},
        "running": {"path": "/tmp/user/.local/bin/taarof-app", "sha256": "a" * 64},
        "installed": {"path": "/tmp/user/.local/bin/taarof-app", "sha256": "a" * 64},
        "running_matches_installed": True,
        # A stale or over-eager producer claiming a match it cannot support.
        # The CLI must not propagate that claim.
        "source_state": "matches",
        "binary_state": "current",
    }

    #: Fully provable identity: build revision recorded, checkout agrees.
    PROVEN = {
        "schema": "taarof.identity.v1",
        "app_version": "0.1.0",
        "build": {
            "source_revision": "c" * 40,
            "source_describe": "v0.1.0-2-gccccccc",
            "source_dirty": False,
            "profile": "release",
        },
        "source": {
            "root": "/src/legacy-app",
            "revision": "c" * 40,
            "describe": "v0.1.0-2-gccccccc",
            "dirty": False,
        },
        "installed_provenance": {"source_revision": "c" * 40},
        "running": {"path": "/tmp/user/.local/bin/taarof-app", "sha256": "a" * 64},
        "installed": {"path": "/tmp/user/.local/bin/taarof-app", "sha256": "a" * 64},
        "running_matches_installed": True,
        "source_state": "matches",
        "binary_state": "current",
    }

    def test_same_binary_with_unknown_source_is_not_current(self) -> None:
        cli = load_cli_module()
        normalized = cli.normalize_identity(self.UNPROVABLE_SOURCE)

        # Running-equals-installed still holds and is reported truthfully.
        self.assertIs(normalized["running_matches_installed"], True)
        self.assertEqual(normalized["binary_state"], "current")

        # ...but that says nothing about the source, so source freshness is
        # unknown. This is the whole point of the task: the two answers are
        # independent, and the missing one is never upgraded to a match.
        self.assertEqual(normalized["source_state"], "unknown")
        self.assertNotEqual(normalized["source_state"], "matches")
        self.assertIsNone(normalized["source_revision"])

        # Doctor must not present an unprovable build as a healthy one.
        facts = {"alive": True, "socket_path": "/tmp/taarof.sock"}
        response = {"ok": True, "data": {"identity": self.UNPROVABLE_SOURCE}}
        with patch.object(cli, "socket_send", lambda *a, **k: response):
            record = cli.check_identity(facts, None)
        self.assertNotEqual(record["status"], "ok")
        self.assertEqual(record["status"], "skip")
        self.assertEqual(record["source_state"], "unknown")
        self.assertIn("source", record["detail"])
        self.assertIn("unknown", record["detail"])

        # `--version` reads the registry without a round-trip; same verdict.
        with tempfile.TemporaryDirectory() as tmp:
            runtime_dir = Path(tmp)
            (runtime_dir / "taarof-current.json").write_text(
                json.dumps({
                    "pid": 4321,
                    "socket_path": str(runtime_dir / "s.sock"),
                    "identity": self.UNPROVABLE_SOURCE,
                }),
                encoding="utf-8",
            )
            with patch.dict(os.environ, {"XDG_RUNTIME_DIR": str(runtime_dir)}, clear=True):
                report = cli.build_version_report(None)
        self.assertEqual(report["identity"]["source_state"], "unknown")
        self.assertIs(report["identity"]["running_matches_installed"], True)

        # A proven identity, by contrast, is allowed to say `matches`.
        proven = cli.normalize_identity(self.PROVEN)
        self.assertEqual(proven["source_state"], "matches")
        self.assertEqual(proven["source_revision"], "c" * 40)

    def test_runtime_identity_surfaces_agree(self) -> None:
        """Registry, query-state, HTTP runtime-identity, --version and Doctor
        must all report one identity, not five approximations of one."""
        cli = load_cli_module()
        expected = cli.normalize_identity(self.PROVEN)

        # The normalizer is the single definition of the shape every surface
        # speaks, so an incomplete surface cannot quietly grow its own dialect.
        self.assertEqual(expected["schema"], cli.IDENTITY_SCHEMA)
        self.assertEqual(expected["app_version"], "0.1.0")

        registry = {
            "pid": 4321,
            "socket_path": "/tmp/taarof.sock",
            "identity": self.PROVEN,
        }
        query_state = {"ok": True, "data": {"identity": self.PROVEN}}
        runtime_identity = {
            "ok": True,
            "data": {
                "schema": "taarof.runtime-identity.v1",
                "runtime_id": "018f0f1e-a2d3-4c55-8f7b-9023456789ab",
                "session_name": "default",
                "identity": self.PROVEN,
            },
        }

        self.assertEqual(cli.identity_from_registry(registry), expected)
        self.assertEqual(cli.identity_from_query_state(query_state), expected)
        self.assertEqual(cli.identity_from_runtime_identity(runtime_identity), expected)

        # Doctor's identity check carries the same normalized block.
        facts = {"alive": True, "socket_path": "/tmp/taarof.sock"}
        with patch.object(cli, "socket_send", lambda *a, **k: query_state):
            record = cli.check_identity(facts, None)
        self.assertEqual(record["identity"], expected)
        self.assertEqual(record["status"], "ok")

        # `taarof --version` reports the same identity, end to end.
        with tempfile.TemporaryDirectory() as tmp:
            runtime_dir = Path(tmp)
            (runtime_dir / "taarof-current.json").write_text(
                json.dumps(registry), encoding="utf-8"
            )
            out = io.StringIO()
            with (
                patch.dict(os.environ, {"XDG_RUNTIME_DIR": str(runtime_dir)}, clear=True),
                patch.object(cli.sys, "stdout", out),
            ):
                rc = cli.main(["--version"])
        self.assertEqual(rc, 0)
        printed = json.loads(out.getvalue())
        self.assertEqual(printed["identity"], expected)
        self.assertEqual(printed["cli_version"], cli.CLI_VERSION)

        # An app with no identity block at all degrades to unknown on every
        # surface rather than inventing agreement.
        blank = cli.normalize_identity(None)
        self.assertEqual(blank["source_state"], "unknown")
        self.assertEqual(blank["binary_state"], "unknown")
        self.assertIsNone(blank["running_matches_installed"])
        self.assertEqual(cli.identity_from_registry({"pid": 1}), blank)
        self.assertEqual(cli.identity_from_query_state({"ok": True, "data": {}}), blank)
        self.assertEqual(cli.identity_from_runtime_identity({"ok": True}), blank)

    def test_normalizer_requires_clean_equal_revisions_and_valid_equal_hashes(self) -> None:
        cli = load_cli_module()
        forged = json.loads(json.dumps(self.PROVEN))
        forged["source"]["revision"] = "d" * 40
        forged["source_state"] = "matches"
        forged["running"]["sha256"] = "not-a-hash"
        forged["installed"]["sha256"] = "a" * 64
        forged["running_matches_installed"] = True
        forged["binary_state"] = "current"
        normalized = cli.normalize_identity(forged)
        self.assertEqual(normalized["source_state"], "unknown")
        self.assertEqual(normalized["binary_state"], "unknown")
        self.assertIsNone(normalized["running_matches_installed"])

    def test_doctor_source_identity_does_not_depend_on_update_state(self) -> None:
        cli = load_cli_module()
        source_matches_but_update_pending = json.loads(json.dumps(self.PROVEN))
        source_matches_but_update_pending["running"]["sha256"] = "b" * 64
        source_matches_but_update_pending["running_matches_installed"] = False
        source_matches_but_update_pending["binary_state"] = "update_pending"
        facts = {"alive": True, "socket_path": "/tmp/taarof.sock"}
        with patch.object(cli, "socket_send", return_value={
            "ok": True, "data": {"identity": source_matches_but_update_pending},
        }):
            record = cli.check_identity(facts, None)
        self.assertEqual(record["status"], "ok")
        self.assertEqual(record["source_state"], "matches")

    def test_cli_version_tracks_the_app_manifest(self) -> None:
        """The CLI ships beside the app; a drifting version string would make
        every 'surfaces agree' claim above vacuous."""
        cli = load_cli_module()
        manifest = (Path(__file__).parents[1] / "taarof-app" / "Cargo.toml").read_text(
            encoding="utf-8"
        )
        declared = next(
            line.split("=", 1)[1].strip().strip('"')
            for line in manifest.splitlines()
            if line.startswith("version = ")
        )
        self.assertEqual(cli.CLI_VERSION, declared)


if __name__ == "__main__":
    unittest.main()
