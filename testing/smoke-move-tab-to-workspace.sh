#!/usr/bin/env bash
# testing/smoke-move-tab-to-workspace.sh
#
# KMUX-61 release-facing proof for the right-click "Move to Workspace" workflow.
#
# Purpose:
#   Prove the full user-visible path a real operator takes:
#     1. Two workspaces exist.
#     2. At least two tabs exist.
#     3. A named tab is moved via the right-click tab menu -> "Move to
#        Workspace" -> target.
#     4. The tab row leaves the source workspace and appears under the target.
#     5. Active workspace / active tab selection follows the intended target.
#
#   The script automates everything that CAN be scripted (isolated launch, tab
#   creation, BEFORE/AFTER state capture, and all assertions) and PROMPTS the
#   operator for the two GUI-only steps, because the running app's socket
#   protocol currently has NO action to create a workspace or move a tab between
#   workspaces (tracked as KMUX-79). Once that lands this script can be made
#   fully non-interactive.
#
# How to run (must be an installed / release-equivalent build for sign-off):
#   bash testing/smoke-move-tab-to-workspace.sh
#   TAAROF_APP_BIN=/path/to/taarof-app bash testing/smoke-move-tab-to-workspace.sh
#
# Release sign-off requirement (KMUX-61 AC5):
#   Run this against ~/.local/bin/taarof-app (or an explicitly supplied
#   release-equivalent binary) before closing the issue. The banner prints the
#   resolved binary and its sha256 so the operator can confirm what was tested.
#
# Exit status: 0 on PASS, non-zero on FAIL or setup error.

set -euo pipefail

# ---------------------------------------------------------------------------
# Resolve repo root, CLI, and app binary.
# ---------------------------------------------------------------------------
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

app_bin="${TAAROF_APP_BIN:-$HOME/.local/bin/taarof-app}"
cli_bin="${TAAROF_CLI:-$repo_root/taarof-cli/taarof}"

session="${TAAROF_SESSION:-move-smoke-$$}"
# Never let an inherited TAAROF_SESSION leak the operator's real session in.
export TAAROF_SESSION="$session"

log_path="${TAAROF_SMOKE_LOG:-${TMPDIR:-/tmp}/taarof-move-smoke-$session.log}"

fail() { echo "[move-smoke] FAIL: $*" >&2; exit 1; }
note() { echo "[move-smoke] $*"; }

command -v python3 >/dev/null 2>&1 || fail "python3 is required to parse query-state JSON"

[[ -x "$app_bin" ]] || fail "app binary not found or not executable: $app_bin (set TAAROF_APP_BIN)"
[[ -e "$cli_bin" ]] || fail "taarof CLI not found: $cli_bin (set TAAROF_CLI)"

taarof() { python3 "$cli_bin" "$@"; }

# ---------------------------------------------------------------------------
# Launch an isolated app instance. TAAROF_SESSION namespaces the registry,
# socket, and persisted session so the operator's real session is untouched.
# ---------------------------------------------------------------------------
if [[ "${TAAROF_SMOKE_USE_RUNNING:-0}" == "1" ]]; then
  note "TAAROF_SMOKE_USE_RUNNING=1: attaching to an already-running session '$session'"
  app_pid=""
else
  note "launching isolated taarof-app (session=$session); log: $log_path"
  TAAROF_SESSION="$session" "$app_bin" >"$log_path" 2>&1 &
  app_pid=$!
  cleanup() {
    if [[ -n "${app_pid:-}" ]] && kill -0 "$app_pid" 2>/dev/null; then
      note "stopping isolated app (pid $app_pid)"
      kill "$app_pid" 2>/dev/null || true
      wait "$app_pid" 2>/dev/null || true
    fi
  }
  trap cleanup EXIT
fi

# ---------------------------------------------------------------------------
# Wait for the isolated app's control socket to answer query-state.
# ---------------------------------------------------------------------------
note "waiting for the isolated app control socket to come up"
ready=0
for _ in $(seq 1 120); do
  if taarof query-state --session "$session" >/dev/null 2>&1; then
    ready=1
    break
  fi
  if [[ -n "${app_pid:-}" ]] && ! kill -0 "$app_pid" 2>/dev/null; then
    echo "----- app log tail -----" >&2
    tail -60 "$log_path" >&2 2>/dev/null || true
    fail "taarof-app exited before its socket was ready (see $log_path)"
  fi
  sleep 0.25
done
[[ "$ready" == "1" ]] || fail "control socket for session '$session' never became ready"

# ---------------------------------------------------------------------------
# Print exactly which binary is under test (AC5): resolve the running pid's exe.
# ---------------------------------------------------------------------------
running_exe=""
if [[ -n "${app_pid:-}" ]]; then
  running_exe="$(readlink -f "/proc/$app_pid/exe" 2>/dev/null || true)"
fi
[[ -n "$running_exe" ]] || running_exe="$(readlink -f "$app_bin" 2>/dev/null || echo "$app_bin")"
running_sha="$(sha256sum "$running_exe" 2>/dev/null | awk '{print $1}' || echo "unknown")"

cat <<BANNER

============================================================
 KMUX-61 move-tab-to-workspace smoke
   session:      $session
   app binary:   $app_bin
   running exe:  $running_exe
   sha256:       $running_sha
   CLI:          $cli_bin
============================================================
Confirm the running exe above is the installed / release build before you
trust a PASS for release sign-off.

BANNER

# ---------------------------------------------------------------------------
# Create two named tabs in the (single) starting workspace so the move has a
# real, identifiable subject and the source keeps a tab after the move.
# ---------------------------------------------------------------------------
moving_tab_name="MOVE-ME-$session"
keep_tab_name="STAY-$session"

note "creating tab '$keep_tab_name' (stays in the source workspace)"
taarof create-tab --session "$session" --name "$keep_tab_name" >/dev/null \
  || fail "could not create keep tab"

note "creating tab '$moving_tab_name' (the tab you will move)"
taarof create-tab --session "$session" --name "$moving_tab_name" >/dev/null \
  || fail "could not create moving tab"

# ---------------------------------------------------------------------------
# Capture BEFORE state.
# ---------------------------------------------------------------------------
before_json="$(taarof query-state --session "$session")" \
  || fail "query-state (before) failed"

# assert_before: MUST see the moving tab, in a workspace; record its source ws.
source_ws_id="$(
  MOVING="$moving_tab_name" python3 - "$before_json" <<'PY'
import json, os, sys
state = json.loads(sys.argv[1])
moving = os.environ["MOVING"]
loc = None
for ws in state.get("workspaces", []):
    for tab in ws.get("tabs", []):
        if tab.get("name") == moving:
            loc = ws.get("id")
if loc is None:
    sys.exit("BEFORE: moving tab %r not found in any workspace" % moving)
print(loc)
PY
)" || fail "$source_ws_id"

note "BEFORE: '$moving_tab_name' is in source workspace id=$source_ws_id"

# ---------------------------------------------------------------------------
# GUI steps the socket protocol cannot yet perform (KMUX-79). Prompt operator.
# ---------------------------------------------------------------------------
cat <<PROMPT

------------------------------------------------------------
 OPERATOR ACTION REQUIRED (two GUI steps)
------------------------------------------------------------
In the taarof window for session '$session':

  1. Create a SECOND workspace (sidebar "+", or the workspace menu),
     so there are at least two workspaces.

  2. Right-click the tab named:  $moving_tab_name
     Choose:  Move to Workspace  ->  <the second workspace>

Leave the tab '$keep_tab_name' where it is (it proves the source
workspace is not simply emptied/deleted).

When both steps are done, come back here and press Enter.
------------------------------------------------------------
PROMPT
read -r -p "Press Enter after completing the move... " _

# ---------------------------------------------------------------------------
# Capture AFTER state and assert the full workflow outcome.
# ---------------------------------------------------------------------------
after_json="$(taarof query-state --session "$session")" \
  || fail "query-state (after) failed"

echo
note "AFTER state (pretty):"
taarof query-state --session "$session" --pretty || true
echo

MOVING="$moving_tab_name" KEEP="$keep_tab_name" SOURCE_WS="$source_ws_id" \
python3 - "$after_json" <<'PY'
import json, os, sys

state = json.loads(sys.argv[1])
moving = os.environ["MOVING"]
keep = os.environ["KEEP"]
source_ws = int(os.environ["SOURCE_WS"])

workspaces = state.get("workspaces", [])
active_ws = state.get("active_workspace")
active_tab = state.get("active_tab")

def where(name):
    hits = []
    for ws in workspaces:
        for tab in ws.get("tabs", []):
            if tab.get("name") == name:
                hits.append((ws.get("id"), tab.get("tab_id"), ws))
    return hits

failures = []

# AC1: at least two workspaces exist after the move.
if len(workspaces) < 2:
    failures.append(
        "AC1: expected >= 2 workspaces after the move, found %d" % len(workspaces)
    )

moving_hits = where(moving)
keep_hits = where(keep)

if len(moving_hits) != 1:
    failures.append(
        "AC2/AC4: expected the moving tab %r exactly once, found %d placements"
        % (moving, len(moving_hits))
    )

target_ws_id = moving_tab_id = target_ws = None
if moving_hits:
    target_ws_id, moving_tab_id, target_ws = moving_hits[0]

    # AC4a: the moved tab must have LEFT the source workspace.
    if target_ws_id == source_ws:
        failures.append(
            "AC4: moving tab %r is still in the source workspace id=%s; the move "
            "did not relocate it" % (moving, source_ws)
        )

    # AC4b: the moved tab row must be UNDER the target workspace.
    target_tab_names = [t.get("name") for t in target_ws.get("tabs", [])]
    if moving not in target_tab_names:
        failures.append(
            "AC4: moving tab %r not found under target workspace id=%s tabs=%r"
            % (moving, target_ws_id, target_tab_names)
        )

# AC2: the source keeps its other tab (workspace was not emptied/deleted).
if not any(ws_id == source_ws for (ws_id, _tid, _ws) in keep_hits):
    failures.append(
        "AC2: keep tab %r is no longer in the source workspace id=%s; the source "
        "row set looks wrong" % (keep, source_ws)
    )

# AC3: active workspace and active tab must follow the move to the target.
if target_ws_id is not None:
    if active_ws != target_ws_id:
        failures.append(
            "AC3: active_workspace=%s does not match the target workspace id=%s"
            % (active_ws, target_ws_id)
        )
    if active_tab != moving_tab_id:
        failures.append(
            "AC3: active_tab=%s does not match the moved tab_id=%s"
            % (active_tab, moving_tab_id)
        )
    # The target workspace's own active_tab should also point at the moved tab.
    if target_ws is not None and target_ws.get("active_tab") != moving_tab_id:
        failures.append(
            "AC3: target workspace active_tab=%s does not match the moved tab_id=%s"
            % (target_ws.get("active_tab"), moving_tab_id)
        )

if failures:
    print("RESULT: FAIL")
    for f in failures:
        print("  - " + f)
    sys.exit(1)

print("RESULT: PASS")
print("  moved %r from workspace id=%s to workspace id=%s (tab_id=%s)"
      % (moving, source_ws, target_ws_id, moving_tab_id))
print("  active_workspace=%s active_tab=%s (both follow the target)"
      % (active_ws, active_tab))
print("  source workspace id=%s still holds %r" % (source_ws, keep))
PY
status=$?

echo
if [[ "$status" -eq 0 ]]; then
  echo "[move-smoke] PASS: right-click Move to Workspace workflow verified."
else
  echo "[move-smoke] FAIL: workflow assertions did not hold (see details above)." >&2
fi
exit "$status"
