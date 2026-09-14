#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="${1:-$PWD}"
BRANCH="test/agent-workspace-$(date +%s)"
MARKER_FILE=".taarof-agent-workspace-command-ran"
RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
REPO_ROOT="$(python3 - <<'PY' "$REPO_ROOT"
import os, sys
print(os.path.realpath(sys.argv[1]))
PY
)"

find_socket() {
    if [[ -n "${TAAROF_SOCK:-}" && -S "${TAAROF_SOCK}" ]]; then
        printf '%s\n' "$TAAROF_SOCK"
        return 0
    fi

    local reg
    reg=$(find "$RUNTIME_DIR" -maxdepth 1 -type f -name 'taarof-current*.json' | head -n1 || true)
    if [[ -n "$reg" ]]; then
        python3 - <<'PY' "$reg"
import json, sys
with open(sys.argv[1], 'r', encoding='utf-8') as fh:
    print(json.load(fh)['socket_path'])
PY
        return 0
    fi

    return 1
}

SOCK="$(find_socket || true)"
if [[ -z "$SOCK" || ! -S "$SOCK" ]]; then
    echo "FAIL: no taarof socket found. Set TAAROF_SOCK or start taarof first."
    exit 1
fi

send_json() {
    printf '%s\n' "$1" | socat -t10 - UNIX-CONNECT:"$SOCK"
}

build_payload() {
    python3 - <<'PY' "$@"
import json, sys
branch = sys.argv[1]
repo = sys.argv[2]
command = sys.argv[3] if len(sys.argv) > 3 else None
payload = {
    "action": "agent-workspace",
    "branch": branch,
    "repo": repo,
}
if command is not None:
    payload["command"] = command
print(json.dumps(payload))
PY
}

json_field() {
    python3 - <<'PY' "$1" "$2"
import json, sys
payload = json.loads(sys.argv[1])
value = payload
for part in sys.argv[2].split('.'):
    value = value[part]
print(value)
PY
}

assert_field_equals() {
    python3 - <<'PY' "$1" "$2" "$3"
import json, sys
payload = json.loads(sys.argv[1])
value = payload
for part in sys.argv[2].split('.'):
    value = value[part]
expected = json.loads(sys.argv[3])
assert value == expected, f"expected {sys.argv[2]} == {expected!r}, got {value!r}"
PY
}

assert_ok() {
    python3 - <<'PY' "$1"
import json, sys
payload = json.loads(sys.argv[1])
assert payload.get('ok') is True, payload
PY
}

assert_workspace_present() {
    python3 - <<'PY' "$1" "$2" "$3" "$4" "$5" "$6"
import json, sys
payload = json.loads(sys.argv[1])
worktree_path = sys.argv[2]
repo_root = sys.argv[3]
workspace_id = int(sys.argv[4])
tab_id = int(sys.argv[5])
expected_tab_count = int(sys.argv[6])
workspaces = payload["data"]["workspaces"]
matches = [ws for ws in workspaces if ws.get("working_tree_path") == worktree_path]
assert len(matches) == 1, f"expected exactly one workspace for {worktree_path}, got {len(matches)}"
ws = matches[0]
assert ws["id"] == workspace_id, f"expected workspace id {workspace_id}, got {ws['id']}"
assert ws["repo_root"] == repo_root, f"expected repo_root {repo_root!r}, got {ws['repo_root']!r}"
assert ws["is_worktree"] is True, f"expected worktree flag true, got {ws['is_worktree']!r}"
assert ws["tab_count"] == expected_tab_count, f"expected {expected_tab_count} tabs, got {ws['tab_count']}"
assert ws["active_tab"] == tab_id, f"expected active tab {tab_id}, got {ws['active_tab']}"
tab_ids = [tab["tab_id"] for tab in ws["tabs"]]
assert tab_id in tab_ids, f"expected tab {tab_id} in {tab_ids!r}"
PY
}

assert_workspace_absent() {
    python3 - <<'PY' "$1" "$2" "$3"
import json, sys
payload = json.loads(sys.argv[1])
worktree_path = sys.argv[2]
workspace_id = int(sys.argv[3])
for ws in payload["data"]["workspaces"]:
    assert ws["id"] != workspace_id, f"workspace {workspace_id} still present"
    assert ws.get("working_tree_path") != worktree_path, f"worktree workspace {worktree_path} still present"
PY
}

query_state() {
    send_json '{"action":"query-state"}'
}

cleanup() {
    if [[ -n "${WORKTREE_PATH:-}" && -d "$WORKTREE_PATH" ]]; then
        git -C "$REPO_ROOT" worktree remove --force "$WORKTREE_PATH" >/dev/null 2>&1 || true
    fi
    git -C "$REPO_ROOT" branch -D "$BRANCH" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "==> Using socket: $SOCK"
echo "==> Repo: $REPO_ROOT"
echo "==> Branch: $BRANCH"

echo "==> Test 1: create agent workspace"
RESP=$(send_json "$(build_payload "$BRANCH" "$REPO_ROOT")")
echo "$RESP"
assert_ok "$RESP"
assert_field_equals "$RESP" data.created_worktree true
assert_field_equals "$RESP" data.ran_command false
assert_field_equals "$RESP" data.repo_root "\"$REPO_ROOT\""
WORKTREE_PATH="$(json_field "$RESP" data.worktree_path)"
WORKSPACE_ID="$(json_field "$RESP" data.workspace_id)"
TAB_ID="$(json_field "$RESP" data.tab_id)"

[[ -d "$WORKTREE_PATH" ]] || { echo "FAIL: worktree path not created: $WORKTREE_PATH"; exit 1; }
[[ -n "$WORKSPACE_ID" && -n "$TAB_ID" ]] || { echo "FAIL: missing workspace/tab ids"; exit 1; }

echo "PASS: created worktree at $WORKTREE_PATH (workspace=$WORKSPACE_ID tab=$TAB_ID)"

STATE1="$(query_state)"
echo "$STATE1"
assert_ok "$STATE1"
assert_workspace_present "$STATE1" "$WORKTREE_PATH" "$REPO_ROOT" "$WORKSPACE_ID" "$TAB_ID" 1
echo "PASS: query-state reports the worktree-backed workspace"

echo "==> Test 2: reuse same branch and run a command"
CMD="touch $MARKER_FILE"
RESP2=$(send_json "$(build_payload "$BRANCH" "$REPO_ROOT" "$CMD")")
echo "$RESP2"
assert_ok "$RESP2"
assert_field_equals "$RESP2" data.created_worktree false
assert_field_equals "$RESP2" data.ran_command true
WORKTREE_PATH_2="$(json_field "$RESP2" data.worktree_path)"
WORKSPACE_ID_2="$(json_field "$RESP2" data.workspace_id)"
TAB_ID_2="$(json_field "$RESP2" data.tab_id)"
[[ "$WORKTREE_PATH_2" == "$WORKTREE_PATH" ]] || {
    echo "FAIL: expected reused worktree path, got $WORKTREE_PATH_2"
    exit 1
}
[[ "$WORKSPACE_ID_2" == "$WORKSPACE_ID" ]] || {
    echo "FAIL: expected reused workspace id, got $WORKSPACE_ID_2"
    exit 1
}
[[ "$TAB_ID_2" == "$TAB_ID" ]] || {
    echo "FAIL: expected reused tab id, got $TAB_ID_2"
    exit 1
}

for _ in {1..20}; do
    if [[ -f "$WORKTREE_PATH/$MARKER_FILE" ]]; then
        break
    fi
    sleep 0.25
done

[[ -f "$WORKTREE_PATH/$MARKER_FILE" ]] || {
    echo "FAIL: command marker file not found in worktree"
    exit 1
}

echo "PASS: reused existing worktree and command executed"

STATE2="$(query_state)"
echo "$STATE2"
assert_ok "$STATE2"
assert_workspace_present "$STATE2" "$WORKTREE_PATH" "$REPO_ROOT" "$WORKSPACE_ID" "$TAB_ID" 1
echo "PASS: query-state still reports exactly one reused agent workspace"

echo "==> Test 3: close the worktree tab and confirm workspace cleanup"
RESP3=$(send_json "{\"action\":\"close-tab\",\"tab\":\"$TAB_ID\"}")
echo "$RESP3"
assert_ok "$RESP3"

for _ in {1..20}; do
    STATE3="$(query_state)"
    if python3 - <<'PY' "$STATE3" "$WORKTREE_PATH" "$WORKSPACE_ID"
import json, sys
payload = json.loads(sys.argv[1])
worktree_path = sys.argv[2]
workspace_id = int(sys.argv[3])
for ws in payload["data"]["workspaces"]:
    if ws["id"] == workspace_id or ws.get("working_tree_path") == worktree_path:
        raise SystemExit(1)
raise SystemExit(0)
PY
    then
        break
    fi
    sleep 0.25
done

assert_workspace_absent "$STATE3" "$WORKTREE_PATH" "$WORKSPACE_ID"
[[ -d "$WORKTREE_PATH" ]] || {
    echo "FAIL: closing the worktree workspace unexpectedly removed the worktree directory"
    exit 1
}

echo "PASS: closing the agent workspace removes it from taarof state without deleting the worktree"
echo "All agent-workspace checks passed."
