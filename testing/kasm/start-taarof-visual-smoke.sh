#!/usr/bin/env bash
set -euo pipefail

repo_root="${TAAROF_REPO_ROOT:-/config/taarof}"
prefix="${TAAROF_VISUAL_PREFIX:-/config/.local}"
session="${TAAROF_SESSION:-visual-smoke-$(date +%Y%m%d%H%M%S)}"
# The literal session name "default" means the unnamed default session: the app
# is launched without TAAROF_SESSION, so panes can use the CLI with no
# --session flag (used by media capture so on-camera commands match the docs).
if [[ "$session" == "default" ]]; then
  session=""
fi
http_port="${TAAROF_HTTP_PORT:-7800}"
log_path="${TAAROF_VISUAL_LOG:-}"
tasks_enabled="${TAAROF_VISUAL_TASKS_ENABLED:-false}"

case "$tasks_enabled" in
  true | false) ;;
  *)
    echo "[visual-smoke] TAAROF_VISUAL_TASKS_ENABLED must be true or false" >&2
    exit 2
    ;;
esac

export HOME="${HOME:-/config}"
export XDG_CONFIG_HOME="${XDG_CONFIG_HOME:-/config/.config}"
export XDG_DATA_HOME="${XDG_DATA_HOME:-/config/.local/share}"
export XDG_STATE_HOME="${XDG_STATE_HOME:-/config/.local/state}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-abc}"
export RUSTUP_HOME="${RUSTUP_HOME:-/usr/local/rustup}"
export CARGO_HOME="${CARGO_HOME:-/usr/local/cargo}"
base_path="${PATH:-/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin}"
export PATH="$prefix/bin:$CARGO_HOME/bin:/lsiopy/bin:/command:$base_path"
if [[ -n "$session" ]]; then
  export TAAROF_SESSION="$session"
else
  # Unnamed session: make sure an inherited TAAROF_SESSION (for example the
  # literal "default" marker) cannot leak into the app environment.
  unset TAAROF_SESSION
fi
export TAAROF_PANE_ATTACH_TRACE="${TAAROF_PANE_ATTACH_TRACE:-1}"

if [[ -z "$log_path" ]]; then
  log_path="$XDG_STATE_HOME/taarof/visual-smoke.log"
fi

# Stable, run-independent marker used by --stop and cleanup-on-start so a later
# invocation can always find and reap a previous smoke instance regardless of
# that instance's timestamped session name.
stable_pid_file="/tmp/taarof-visual-smoke.pid"
# Pattern that identifies any visual-smoke app instance via its environ.
smoke_session_prefix="visual-smoke-"
binary_install_path="$prefix/bin/taarof-app"

log() { echo "[visual-smoke] $*"; }

# Refuse to run anywhere but the approved testing/kasm container before doing
# anything destructive: stopping/killing a previous instance, launching an
# app that stops whatever else is on the HTTP port, or rewriting runtime pid
# and registry files. s6-overlay's container_environment plus the
# compose-declared TITLE and the fixed desktop user this image runs as are a
# fingerprint a real host or personal desktop will not match, so an
# accidental run there fails closed instead of killing a real taarof-app.
assert_kasm_container() {
  local env_dir="/run/s6/container_environment" title="" user
  [[ -f "$env_dir/TITLE" ]] && title="$(<"$env_dir/TITLE")"
  user="$(id -un 2>/dev/null || echo unknown)"
  if [[ ! -d "$env_dir" || "$title" != "taarof-testing" || "$user" != "abc" ]]; then
    echo "[visual-smoke] REFUSING: this script performs destructive process/session actions and only runs inside the approved testing/kasm container (expected $env_dir with TITLE=taarof-testing and user=abc; got container_env=$([[ -d "$env_dir" ]] && echo present || echo absent), TITLE=${title:-<unset>}, user=$user)." >&2
    exit 1
  fi
}

assert_kasm_container

# Process start time (field 22 of /proc/pid/stat, clock ticks since boot).
# Stable for the lifetime of a pid, so pairing it with the pid in the stable
# pid file detects OS pid reuse: a dead smoke app's pid can be reassigned by
# the kernel to an unrelated process before we next look at it, and that
# process must never be treated as "ours" just because the number matches.
proc_start_time() {
  local pid="$1" stat
  stat="$(cat "/proc/$pid/stat" 2>/dev/null)" || return 1
  # comm (field 2) is parenthesized and can itself contain ")"; strip through
  # the last ") " so field counting below is reliable regardless of comm.
  stat="${stat##*) }"
  awk '{print $20}' <<<"$stat"
}

# Is $1 a live process that is a taarof-app launched under a visual-smoke session?
is_smoke_pid() {
  local pid="$1"
  [[ -n "$pid" ]] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  local exe cmd
  exe="$(readlink -f "/proc/$pid/exe" 2>/dev/null || true)"
  cmd="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)"
  case "$exe$cmd" in
    *taarof-app*) ;;
    *) return 1 ;;
  esac
  # The pid recorded in our stable pid file is ours even when the app runs an
  # unnamed session (media capture), where no TAAROF_SESSION marker exists —
  # but only when the recorded start time still matches the live process, so
  # a reused pid (original exited, kernel handed the number to something else
  # that also happens to be a taarof-app) is never mistaken for ours before a
  # destructive kill.
  if [[ -f "$stable_pid_file" ]]; then
    local file_pid file_start live_start
    read -r file_pid file_start <"$stable_pid_file" 2>/dev/null || true
    if [[ "$file_pid" == "$pid" ]]; then
      live_start="$(proc_start_time "$pid" || true)"
      if [[ -n "$file_start" && -n "$live_start" && "$file_start" == "$live_start" ]]; then
        return 0
      fi
    fi
  fi
  # Confirm it belongs to a visual-smoke session.
  if tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null \
      | grep -q "^TAAROF_SESSION=${smoke_session_prefix}"; then
    return 0
  fi
  return 1
}

# Resolve the pid of the running smoke app: stable pid file first, then a
# pgrep fallback over taarof-app processes whose environ marks them as smoke.
resolve_smoke_pid() {
  local pid=""
  if [[ -f "$stable_pid_file" ]]; then
    pid="$(awk '{print $1}' "$stable_pid_file" 2>/dev/null || true)"
    if is_smoke_pid "$pid"; then
      echo "$pid"
      return 0
    fi
  fi
  # Fallback: scan taarof-app processes and check their environ.
  local candidate
  for candidate in $(pgrep -x taarof-app 2>/dev/null || true) \
                    $(pgrep -f "taarof-app" 2>/dev/null || true); do
    if is_smoke_pid "$candidate"; then
      echo "$candidate"
      return 0
    fi
  done
  return 1
}

# Read the TAAROF_SESSION of a running pid (used to clean its registry file).
session_of_pid() {
  local pid="$1"
  tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null \
    | sed -n 's/^TAAROF_SESSION=//p' | head -1
}

# Remove the runtime registry file for a session whose pid is dead.
remove_stale_registry() {
  local sess="$1"
  local reg
  if [[ -n "$sess" ]]; then
    reg="$XDG_RUNTIME_DIR/taarof-current-${sess}.json"
  else
    reg="$XDG_RUNTIME_DIR/taarof-current.json"
  fi
  if [[ -f "$reg" ]]; then
    local reg_pid
    reg_pid="$(jq -r '.pid // empty' "$reg" 2>/dev/null || true)"
    if [[ -z "$reg_pid" ]] || ! kill -0 "$reg_pid" 2>/dev/null; then
      rm -f "$reg" && log "removed stale registry $reg"
    fi
  fi
}

# Stop any running smoke app: TERM, wait briefly, KILL if needed, then clean up
# pid file and any stale registry file. Returns 0 whether or not one was found.
stop_smoke() {
  local pid sess
  pid="$(resolve_smoke_pid || true)"
  if [[ -z "$pid" ]]; then
    log "no running visual-smoke app found"
    # Even with nothing running, drop a stale stable pid file.
    [[ -f "$stable_pid_file" ]] && rm -f "$stable_pid_file"
    return 0
  fi
  sess="$(session_of_pid "$pid" || true)"
  log "stopping visual-smoke app pid $pid (session ${sess:-unknown})"
  kill -TERM "$pid" 2>/dev/null || true
  local waited=0
  while kill -0 "$pid" 2>/dev/null; do
    sleep 0.25
    waited=$((waited + 1))
    if [[ "$waited" -ge 20 ]]; then
      log "pid $pid did not exit after TERM; sending KILL"
      kill -KILL "$pid" 2>/dev/null || true
      sleep 0.5
      break
    fi
  done
  if kill -0 "$pid" 2>/dev/null; then
    log "warning: pid $pid still alive after KILL" >&2
  else
    log "stopped pid $pid"
  fi
  rm -f "$stable_pid_file"
  remove_stale_registry "$sess"
  return 0
}

# Handle --stop before doing any expensive work.
if [[ "${1:-}" == "--stop" ]]; then
  stop_smoke
  exit 0
fi

# Re-exec fully detached when not attached to an interactive terminal (e.g. run
# via `docker exec ... start-taarof-visual-smoke.sh`). setsid + stdio to the log
# + stdin from /dev/null means the app survives the exec session ending or
# timing out. The interactive-desktop path (a tty) keeps the original
# foreground/operator-facing behavior.
if [[ "${TAAROF_VISUAL_DETACHED:-0}" != "1" && ! -t 0 ]]; then
  mkdir -p "$(dirname "$log_path")"
  log "no interactive terminal detected; relaunching detached (log: $log_path)"
  log "tail the log with: tail -f $log_path"
  TAAROF_VISUAL_DETACHED=1 \
  TAAROF_SESSION="${session:-default}" \
  TAAROF_HTTP_PORT="$http_port" \
  TAAROF_VISUAL_LOG="$log_path" \
    setsid bash "$0" "$@" </dev/null >>"$log_path" 2>&1 &
  disown || true
  log "detached pid: $!"
  exit 0
fi

mkdir -p "$XDG_CONFIG_HOME/taarof" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_RUNTIME_DIR" "$(dirname "$log_path")"
chmod 700 "$XDG_RUNTIME_DIR" || true

# Cleanup on start: ensure at most one smoke instance. Stop any previous one
# (same logic as --stop) so we never leave the binary held (avoids
# 'cp: Text file busy' at install time) or produce ambiguous multi-instance
# state.
log "cleanup: stopping any previous visual-smoke instance"
stop_smoke

cat >"$XDG_CONFIG_HOME/taarof/config.toml" <<EOF
[http]
enabled = true
port = $http_port
bind_address = "127.0.0.1"
unsafe_allow_non_loopback = false

[tasks]
enabled = $tasks_enabled
pull_requests = $tasks_enabled
default_view = "tasks"
EOF

# Optional extra TOML appended verbatim (escape hatch for smoke/capture runs
# that need non-default config, e.g. TAAROF_VISUAL_EXTRA_CONFIG=$'[tmux]\nenabled = true').
# Note: media capture deliberately does NOT set [dock] visible=true here —
# see KMUX-157 (sidebar layout breaks when badge-rich rows render).
if [[ -n "${TAAROF_VISUAL_EXTRA_CONFIG:-}" ]]; then
  printf '%s\n' "$TAAROF_VISUAL_EXTRA_CONFIG" >>"$XDG_CONFIG_HOME/taarof/config.toml"
fi

cd "$repo_root"

log "building release binary"
cargo build --release --manifest-path taarof-app/Cargo.toml
cargo build --release --manifest-path agent-launcher/Cargo.toml
built_binary="$(
  cargo metadata --manifest-path taarof-app/Cargo.toml --format-version 1 --no-deps \
    | jq -er '.target_directory + "/release/taarof-app"'
)"
if [[ ! -f "$built_binary" || ! -x "$built_binary" ]]; then
  echo "[visual-smoke] cargo did not produce an executable release artifact at $built_binary" >&2
  exit 1
fi

log "building web client"
(cd taarof-web && npm ci && npm run build)

# Guard installs: make absolutely sure nothing holds the target binary before
# install-local.sh copies over it (defense in depth beyond cleanup-on-start).
if is_smoke_pid "$(resolve_smoke_pid || true)"; then
  log "a smoke app still holds $binary_install_path; stopping it before install"
  stop_smoke
fi

log "installing to $prefix"
bash testing/kasm/install-built-release.sh "$repo_root" "$prefix"

log "verifying built and installed binary identity"
bash testing/kasm/verify-binary-identity.sh "$built_binary" "$binary_install_path"

if curl -fsS "http://127.0.0.1:$http_port/health" >/dev/null 2>&1; then
  echo "[visual-smoke] HTTP port $http_port is already serving; stop the existing app or set TAAROF_HTTP_PORT" >&2
  exit 1
fi

log "launching taarof-app; log: $log_path"
# Launch fully detached: setsid + stdin from /dev/null so the app is never a
# child of (and never dies with) the exec/shell session that started it.
app_env=()
if [[ -n "$session" ]]; then
  app_env+=(TAAROF_SESSION="$session")
else
  app_env+=(-u TAAROF_SESSION)
fi
app_env+=(TAAROF_PANE_ATTACH_TRACE="$TAAROF_PANE_ATTACH_TRACE")
setsid env "${app_env[@]}" \
  "$prefix/bin/taarof-app" </dev/null >>"$log_path" 2>&1 &
app_pid=$!
disown || true

# Write the pid files atomically (temp + mv). The stable pid file is the
# cross-run handle used by --stop and cleanup-on-start; it pairs the pid with
# its process start time so a later reader can detect pid reuse (see
# is_smoke_pid) before treating some other process as the smoke app.
app_start_time="$(proc_start_time "$app_pid" || true)"
pid_tmp="$(mktemp "${stable_pid_file}.XXXXXX")"
printf '%s %s\n' "$app_pid" "$app_start_time" >"$pid_tmp"
mv -f "$pid_tmp" "$stable_pid_file"
# Also keep a per-session pid file for informational/backward-compatible use.
session_pid_file="/tmp/taarof-${session:-default}.pid"
pid_tmp2="$(mktemp "${session_pid_file}.XXXXXX")"
echo "$app_pid" >"$pid_tmp2"
mv -f "$pid_tmp2" "$session_pid_file"

for _ in $(seq 1 120); do
  if curl -fsS "http://127.0.0.1:$http_port/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$app_pid" 2>/dev/null; then
    echo "[visual-smoke] taarof-app exited early" >&2
    tail -120 "$log_path" >&2 || true
    exit 1
  fi
  sleep 0.25
done

if ! curl -fsS "http://127.0.0.1:$http_port/health" >/dev/null 2>&1; then
  echo "[visual-smoke] HTTP health did not become ready" >&2
  tail -120 "$log_path" >&2 || true
  exit 1
fi

log "verifying running binary identity"
if ! bash testing/kasm/verify-binary-identity.sh \
    "$built_binary" "$binary_install_path" "$app_pid"; then
  echo "[visual-smoke] refusing readiness: running binary does not match the fresh build" >&2
  stop_smoke
  exit 1
fi

token_path="$(ls -t "$XDG_RUNTIME_DIR"/taarof-http-*.token 2>/dev/null | head -1 || true)"
if [[ -z "$token_path" ]]; then
  echo "[visual-smoke] no HTTP token found under $XDG_RUNTIME_DIR" >&2
  exit 1
fi
token="$(cat "$token_path")"
# Keep the bearer value out of argv and generated logs/URLs.
curl_authenticated() {
  printf 'header = "Authorization: Bearer %s"\n' "$token" | curl --config - "$@"
}

require_tab_id() {
  local label="$1"
  local response="$2"
  local tab_id

  if ! tab_id="$(jq -er '.tab_id // empty' <<<"$response")"; then
    echo "[visual-smoke] create-tab for $label did not return tab_id: $response" >&2
    exit 1
  fi

  echo "$tab_id"
}

issue_135_agent="/tmp/taarof-issue-135-codex"
cat >"$issue_135_agent" <<'AGENT'
#!/usr/bin/env bash
set -euo pipefail
exec -a codex bash -c 'while true; do sleep 60; done' &
codex_child_pid=$!
trap 'kill "$codex_child_pid" 2>/dev/null || true' EXIT

while true; do
  printf 'Bash: cargo test --manifest-path taarof-app/Cargo.toml\n'
  sleep 1
done
AGENT
chmod +x "$issue_135_agent"

claude_subagent_demo="/tmp/taarof-claude-subagent-demo"
cat >"$claude_subagent_demo" <<'AGENT'
#!/usr/bin/env bash
set -euo pipefail
cwd="$PWD"
mangled="${cwd//\//-}"
mangled="${mangled//./-}"
project_dir="${HOME}/.claude/projects/${mangled}"
transcript="${project_dir}/visual-subagents.jsonl"
mkdir -p "$project_dir"
timestamp="$(date -u +'%Y-%m-%dT%H:%M:%S.000Z')"
printf '%s\n' "{\"type\":\"assistant\",\"timestamp\":\"${timestamp}\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"visual-child-a\",\"name\":\"Agent\",\"input\":{\"name\":\"researcher\",\"description\":\"Inspect transcript reconciliation\"}},{\"type\":\"tool_use\",\"id\":\"visual-child-b\",\"name\":\"Task\",\"input\":{\"name\":\"researcher\",\"description\":\"Review compact hierarchy\"}}]}}" >"$transcript"
exec -a claude bash -c 'while true; do sleep 60; done' &
claude_child_pid=$!
trap 'kill "$claude_child_pid" 2>/dev/null || true' EXIT
while true; do sleep 60; done
AGENT
chmod +x "$claude_subagent_demo"

echo "[visual-smoke] creating duplicate-pane-id VTE tabs"
echo "[visual-smoke] creating process-backed agent demo tab"
issue_135_response="$("$prefix/bin/taarof" create-tab --session "$session" --name Agent-Demo \
  --command "$issue_135_agent")"
echo "$issue_135_response"
issue_135_tab_id="$(require_tab_id Agent-Demo "$issue_135_response")"

claude_subagent_response="$("$prefix/bin/taarof" create-tab --session "$session" --name Claude-Subagents \
  --cwd "$repo_root" --command "$claude_subagent_demo")"
echo "$claude_subagent_response"
claude_subagent_tab_id="$(require_tab_id Claude-Subagents "$claude_subagent_response")"

boiler_response="$("$prefix/bin/taarof" create-tab --session "$session" --name Workspace-A \
  --command "printf 'VISUAL_SMOKE_WORKSPACE_A_INITIAL\\n'; exec bash -i")"
echo "$boiler_response"
boiler_tab_id="$(require_tab_id Workspace-A "$boiler_response")"

sample_response="$("$prefix/bin/taarof" create-tab --session "$session" --name Workspace-B \
  --command "printf 'VISUAL_SMOKE_WORKSPACE_B_INITIAL\\n'; exec bash -i")"
echo "$sample_response"
sample_tab_id="$(require_tab_id Workspace-B "$sample_response")"

multi_agent_response="$("$prefix/bin/taarof" create-tab --session "$session" --name Multi-Agent \
  --command "exec bash -i")"
echo "$multi_agent_response"
multi_agent_tab_id="$(require_tab_id Multi-Agent "$multi_agent_response")"

multi_pane_1="$("$prefix/bin/taarof" open-pane --session "$session" --tab "$multi_agent_tab_id" \
  --command "exec bash -i" --direction vertical | jq -er '.pane_id')"
multi_pane_2="$("$prefix/bin/taarof" open-pane --session "$session" --tab "$multi_agent_tab_id" \
  --command "exec bash -i" --direction horizontal | jq -er '.pane_id')"
scratch_pane="$("$prefix/bin/taarof" open-pane --session "$session" --tab "$multi_agent_tab_id" \
  --command "exec bash -i" --direction vertical | jq -er '.pane_id')"

"$prefix/bin/taarof" agent-status --session "$session" --tab "$multi_agent_tab_id" --pane 0 \
  --state waiting-input --text "review requested" --source codex
"$prefix/bin/taarof" agent-status --session "$session" --tab "$multi_agent_tab_id" --pane "$multi_pane_1" \
  --state waiting-input --text "needs approval" --source codex
"$prefix/bin/taarof" agent-status --session "$session" --tab "$multi_agent_tab_id" --pane "$multi_pane_2" \
  --state errored --text "tests failed" --source claude
"$prefix/bin/taarof" agent-status --session "$session" --tab "$multi_agent_tab_id" --pane "$scratch_pane" \
  --state done --text "scratch complete" --source pi
"$prefix/bin/taarof" close-pane --session "$session" --tab "$multi_agent_tab_id" --pane "$scratch_pane"

sleep 1

if ! curl_authenticated -fsS \
  "http://127.0.0.1:$http_port/api/v1/state" \
  | jq -e --argjson tab_id "$multi_agent_tab_id" --argjson pane_1 "$multi_pane_1" --argjson pane_2 "$multi_pane_2" '
    [.data.workspaces[].tabs[] | select(.tab_id == $tab_id)][0] as $tab
    | ($tab.panes | map(.pane_id) | sort) == ([0, $pane_1, $pane_2] | sort)
      and ($tab.agents | map(.pane_id) | sort) == ([0, $pane_1, $pane_2] | sort)
      and ($tab.agents | map(.agent_name)) == ["codex", "codex", "claude"]
  ' >/dev/null; then
  echo "[visual-smoke] Multi-Agent pane projection or close cleanup is incorrect" >&2
  exit 1
fi

echo "[visual-smoke] Multi-Agent real split projection is backend-ready"

sleep 1
"$prefix/bin/taarof" run --session "$session" --tab "$boiler_tab_id" --pane 0 \
  --command "printf 'VISUAL_SMOKE_WORKSPACE_A_LIVE_%s\\n' \"\$(date +%s)\""
"$prefix/bin/taarof" run --session "$session" --tab "$sample_tab_id" --pane 0 \
  --command "printf 'VISUAL_SMOKE_WORKSPACE_B_LIVE_%s\\n' \"\$(date +%s)\""

echo "[visual-smoke] waiting for Agent-Demo to report output-scan running activity"
issue_135_ready=0
for _ in $(seq 1 40); do
  if curl_authenticated -fsS \
    "http://127.0.0.1:$http_port/api/v1/state" \
    | jq -e --argjson issue_135_tab_id "$issue_135_tab_id" '
      .data.workspaces[].tabs[]
      | select(.tab_id == $issue_135_tab_id)
      | .agent_running == true
        and .agent_name == "codex"
        and .agent_activity.state == "running"
        and .agent_activity.origin == "output-scan"
    ' >/dev/null; then
    echo "[visual-smoke] Agent-Demo is backend-ready"
    issue_135_ready=1
    break
  fi
  sleep 0.5
done

if [[ "$issue_135_ready" != "1" ]]; then
  echo "[visual-smoke] Agent-Demo did not become backend-ready" >&2
  curl_authenticated -fsS \
    "http://127.0.0.1:$http_port/api/v1/state" \
    | jq --argjson issue_135_tab_id "$issue_135_tab_id" '.data.workspaces[].tabs[] | select(.tab_id == $issue_135_tab_id) | {tab_id, name, agent_running, agent_name, agent_pane_id, agent_activity}' >&2 || true
  tail -120 "$log_path" >&2 || true
  exit 1
fi

curl_authenticated -fsS \
  "http://127.0.0.1:$http_port/api/v1/state" \
  | jq --argjson issue_135_tab_id "$issue_135_tab_id" '.data.workspaces[].tabs[] | select(.tab_id == $issue_135_tab_id) | {tab_id, name, agent_running, agent_name, agent_pane_id, agent_activity}'

cat <<EOF

[visual-smoke] ready
  noVNC desktop: open http://127.0.0.1:6901/ from the host
  taarof web:    http://127.0.0.1:$http_port/ (use the private runtime token file to authenticate)
  app pid:       $app_pid
  pid file:      $stable_pid_file
  log:           $log_path

Manual visual checks:
  1. In the noVNC desktop, confirm the GTK taarof window is visible.
  2. Open the taarof web URL in the container browser.
  3. Select Agent-Demo in the GTK sidebar.
  4. Confirm it shows a green running indicator and a running status line.
  5. Confirm the pane is continuously printing "Bash: cargo test --manifest-path taarof-app/Cargo.toml".
  6. Confirm Workspace-A and Workspace-B still show their VISUAL_SMOKE_* markers.
  7. Select Workspace-A and Workspace-B in the sidebar.
  8. Confirm each pane shows its own VISUAL_SMOKE_* marker and live updates.
  9. Confirm Workspace-A content never appears in Workspace-B and vice versa.
 10. Select Multi-Agent and confirm its left-rail tab is expanded with codex #1,
     codex #2, and claude real-pane children; the closed scratch pane is absent.
 11. Click each child and confirm exact split-pane focus, then compare labels,
     states, and activity with the right-hand Agents mode.

Stop later with:
  bash testing/kasm/start-taarof-visual-smoke.sh --stop
  (or: kill $app_pid)
EOF
