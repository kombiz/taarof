# RUNBOOK.md

Operational guide for `taarof`.

## Service overview

`taarof` is primarily a local desktop application (`taarof-app/`) with two optional local control surfaces:

| Surface | Default state | Purpose |
| --- | --- | --- |
| GTK desktop app | Enabled when launched | Main UI |
| Unix socket API | Enabled when the runtime dir is trusted | Same-user local control and query surface |
| HTTP API | Disabled until `[http].enabled = true` | Local web/API surface; write routes need `[http_control]` |

## Start, stop, restart

| Task | Command / action |
| --- | --- |
| Run in development | `cargo run --manifest-path taarof-app/Cargo.toml` |
| Build release binary | `cargo build --release --manifest-path taarof-app/Cargo.toml` |
| Launch built binary | `./taarof-app/target/release/taarof-app` |
| Install local desktop assets | `bash packaging/linux/install-local.sh` |
| Apply config changes | `config.toml` validates and reloads supported consumers live; restart only after changing startup-owned services or companion files |

## Verify the running local build

Locate the active runtime checkout from the live process rather than assuming a path.

From the repo root:

```bash
pid=$(jq -r .pid "$XDG_RUNTIME_DIR/taarof-current.json")
sock=$(jq -r .socket_path "$XDG_RUNTIME_DIR/taarof-current.json")

ps -p "$pid" -o pid,lstart,cmd
readlink -f "/proc/$pid/exe"
sha256sum "/proc/$pid/exe" ~/.local/bin/taarof-app taarof-app/target/release/taarof-app
TAAROF_SOCK="$sock" taarof query-state --pretty
```

The running version is the expected local build when the `/proc/<pid>/exe`, installed binary, and repo release binary hashes match.

### Update pending

The sidebar **Update** chip, `taarof doctor`, or `query-state.update` reports
`update_pending` when the running executable was deleted/replaced or differs
from the installed executable. This is advisory; health remains independent.

The update watcher itself is installed during startup, but it resolves
`[update].installed_binary_path` from the current live config snapshot on every
refresh. Changing that path therefore affects the next refresh without
restarting Taarof; changing the startup-owned HTTP/control, history, or task
discovery services still requires a new process.

Before restarting, review active panes, bound tasks, running agents, and session
restore readiness. Save or detach any work that cannot be reconstructed, then
restart Taarof manually at a convenient boundary. Taarof never restarts itself,
kills the running process, or closes panes in response to an installed update.

If the state is `unknown`, inspect its reason and fix the configured path or
permissions before treating the install as current.

## Health checks

### App and local state

| Check | Expected result |
| --- | --- |
| Session file exists | `~/.local/share/taarof/session.json` or `session-<name>.json` updates over time |
| Diagnostics log exists | `~/.local/state/taarof/diagnostics*.jsonl` appears after startup or failures |
| Socket registry exists | `$XDG_RUNTIME_DIR/taarof-current.json` while the app is running |

### HTTP API

Enable it in `~/.config/taarof/config.toml`:

```toml
[http]
enabled = true
port = 7800
bind_address = "127.0.0.1"
unsafe_allow_non_loopback = false

[http_control]
enabled = false
```

Then check:

```bash
curl http://127.0.0.1:7800/health
TOKEN=$(cat "$XDG_RUNTIME_DIR/taarof-http-<pid>.token")
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7800/api/v1/state
```

`/health` is intentionally unauthenticated. The rest of the HTTP API requires the bearer token.

To enable local browser writeback, keep `[http].bind_address` on loopback and
set `[http_control].enabled = true`. Control requests still require the bearer
token and are rejected on non-loopback HTTP binds. This config is read at app
startup, so restart `taarof-app` after changing it. Enabling control upgrades the
same per-process bearer token used for read routes; any local holder of that
token can execute the narrow write routes documented in `docs/local-query-api.md`.

### Remote control access

Do not expose taarof's raw HTTP port or Unix socket directly. For remote access,
keep taarof on owner-host loopback and use your own reverse proxy,
authentication layer, and narrow control gateway. Revoke the gateway's control
grant, or disable `[http_control]`, when remote control should stop.

## Runtime paths

| Surface | Path |
| --- | --- |
| App config | `~/.config/taarof/config.toml` |
| Session state | `~/.local/share/taarof/session.json` |
| Named session state | `~/.local/share/taarof/session-<name>.json` |
| Templates | `~/.config/taarof/templates.json` |
| Saved views | `~/.config/taarof/views.json` |
| Agent signatures | `~/.config/taarof/agent-signatures.json` |
| Socket registry | `$XDG_RUNTIME_DIR/taarof-current.json` |
| Socket | `$XDG_RUNTIME_DIR/taarof-<pid>.sock` |
| HTTP token | `$XDG_RUNTIME_DIR/taarof-http-<pid>.token` |
| Diagnostics log | `~/.local/state/taarof/diagnostics.jsonl` |
| Diagnostics archive | `~/.local/state/taarof/diagnostics.jsonl.1` |

When `TAAROF_SESSION` is set, the session and diagnostics filenames are namespaced.

## Logs and diagnostics

Primary operator-visible signals:

| Signal | Where |
| --- | --- |
| App warnings / startup failures | stderr / terminal output |
| Structured diagnostics | `~/.local/state/taarof/diagnostics*.jsonl` |
| HTTP degraded status | `/health` and `query-state` health payloads |
| Packaging validation | `bash packaging/linux/validate-local-install.sh` |

### Inspect and shrink SQLite history

Run `taarof doctor` or inspect `history.storage` and `history.maintenance` in
`taarof query-state --pretty`. The important fields are main/WAL bytes,
oldest/newest record timestamps, pending writer depth, the last maintenance
result, rows removed, and consecutive failures. A `pending` result means the
bounded pass yielded and will continue. A `failed` result leaves live Taarof and
readable history running where possible; use `last_error` to check permissions,
free disk space, or a long-lived SQLite reader. Retries back off to at most one
hour, so repeated log messages should not form a tight loop.

To shrink the store, tighten `history.max_age_days`, `history.max_records`, or
`history.max_bytes` within the documented bounds, then restart Taarof for an
immediate pass or wait for `maintenance_interval_minutes`. Deletes happen in
age, count, then size order. WAL checkpointing is automatic, and free database
pages are reclaimed incrementally over later passes; a large file may therefore
take more than one cadence to reach its soft cap. Back up the SQLite file before
manual intervention, and never treat it as the source of truth for tasks,
sessions, Work ledgers, transcripts, repositories, or PRs.

## Attach an existing tmux session

Use this when a tmux session already exists (locally or on a configured remote
host) and you want it in a taarof pane without knowing the generated session
name or the socket API.

1. Open the command palette (`Ctrl+Shift+P` by default) and select
   **Attach Session…**.
2. A picker lists every candidate tmux session, de-duplicated by
   `(session name, host)`. Each row shows: the host (`local` or the ssh target),
   the session name, its state, and its current/last command when known. The
   list merges two sources:
   - sessions this taarof instance explicitly detached, and
   - sessions discovered by the dashboard prober (local plus any hosts declared
     under `[hosts]` in `~/.config/taarof/config.toml`).
   The same session name on two different hosts appears as two distinct rows —
   selection always attaches the exact host you picked, never name alone.
3. Selecting a row:
   - **`[open here]`** — the session is already attached in a tab of this
     taarof instance; taarof focuses/switches to that tab instead of spawning a
     duplicate pane.
   - **`[detached]` / `[attached]`** — taarof attaches the session in a new
     tab/pane via the normal attach flow.
4. If there are no candidate sessions, the picker shows a next-action message
   rather than a silent empty list: create one with a tmux-backed tab, or
   configure `[hosts]` for remote discovery. If tmux itself is unavailable, the
   picker says so.

Remote discovery reuses the dashboard prober; the attach picker does not run any
new ssh probing of its own.

## Backup

Back up these files before risky changes or local experiments:

- `~/.config/taarof/config.toml`
- `~/.config/taarof/templates.json`
- `~/.config/taarof/views.json`
- `~/.config/taarof/agent-signatures.json`
- `~/.local/share/taarof/session*.json`

## Rollback

### Runtime/config rollback

1. Quit `taarof`.
2. Restore the config/state files above from backup.
3. Relaunch the app.

### Source/build rollback

1. Check out the desired git revision or tag.
2. Rebuild with `cargo build --release --manifest-path taarof-app/Cargo.toml`.
3. Reinstall with `bash packaging/linux/install-local.sh` if you use the local desktop bundle.

## Troubleshooting map

| Problem area | Preferred reference |
| --- | --- |
| Known failure patterns | `TROUBLESHOOTING.md` |
| HTTP API and `/health` | `docs/local-query-api.md` |
| Remote-control gateway | External authenticated gateway (outside this tree) |
| Release and packaging flow | `docs/release.md` |
| tmux-backed pane behavior | `docs/tmux-integration.md` |

## Security notes

- Keep the HTTP API on loopback unless there is an explicit need for remote access.
- Non-loopback binds require `[http].unsafe_allow_non_loopback = true` and should be treated as unsafe.
- Remote control must go through a gateway/auth boundary; do not proxy the bearer token or Unix socket to a browser.
- Bearer token files are runtime secrets and should never be committed or copied into docs.
- The Unix socket is trusted only because the runtime dir is validated as user-owned and owner-only.
- The web client persists the HTTP API bearer token in the browser's `window.localStorage` under `taarof.web.token`; it survives browser restarts until explicitly cleared. This is acceptable under the documented loopback-only default, where the token never leaves the local machine.
- If you expose the HTTP API beyond loopback, treat that stored token as a long-lived credential: rotate or restrict it, prefer a session-scoped storage mode for such setups, and clear `taarof.web.token` from localStorage (or use a fresh private window) when access should stop.
