#!/usr/bin/env bash
set -euo pipefail

repo_root="${TAAROF_REPO_ROOT:-/config/taarof}"
display="${DISPLAY:-:1}"
target_dir="${CARGO_TARGET_DIR:-/container-target}"
app="$target_dir/release/taarof-app"
cli="$repo_root/taarof-cli/taarof"
fixture_dir="$repo_root/testing/kasm/fixtures"

for executable in cargo jq xdotool timeout; do
    command -v "$executable" >/dev/null 2>&1 || {
        echo "restored-close: missing command: $executable" >&2
        exit 69
    }
done
test -x "$cli" || {
    echo "restored-close: missing CLI: $cli" >&2
    exit 69
}

cd "$repo_root"
DISPLAY="$display" cargo +stable test \
    --manifest-path taarof-app/Cargo.toml \
    restored_background_tab_close_reclaims_row_without_materializing \
    -- --ignored --nocapture
cargo +stable build --release --manifest-path taarof-app/Cargo.toml
test -x "$app" || {
    echo "restored-close: release build missing: $app" >&2
    exit 1
}

root="$(mktemp -d /tmp/taarof-restored-close.XXXXXX)"
home="$root/home"
runtime="$root/runtime"
app_pid=""

cleanup() {
    if [[ -n "$app_pid" ]] && kill -0 "$app_pid" 2>/dev/null; then
        kill -TERM "$app_pid" 2>/dev/null || true
        for _ in $(seq 1 30); do
            kill -0 "$app_pid" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$app_pid" 2>/dev/null || true
    fi
    rm -rf "$root"
}
trap cleanup EXIT

install -d -m 700 \
    "$home/.config/taarof" \
    "$home/.local/share/taarof" \
    "$home/.local/state" \
    "$runtime"
install -m 600 \
    "$fixture_dir/restored-background-config.toml" \
    "$home/.config/taarof/config.toml"
install -m 600 \
    "$fixture_dir/restored-background-session.json" \
    "$home/.local/share/taarof/session.json"

export DISPLAY="$display"
export HOME="$home"
export XDG_CONFIG_HOME="$home/.config"
export XDG_DATA_HOME="$home/.local/share"
export XDG_STATE_HOME="$home/.local/state"
export XDG_RUNTIME_DIR="$runtime"

"$app" >"$root/app.log" 2>&1 &
app_pid=$!

query_state() {
    timeout 3 "$cli" query-state
}

ready=0
for _ in $(seq 1 120); do
    if query_state 2>/dev/null | jq -e '
        .ok == true
        and .data.active_tab != null
        and ([.data.workspaces[].tabs[]] | length == 3)
    ' >/dev/null; then
        ready=1
        break
    fi
    kill -0 "$app_pid" 2>/dev/null || {
        echo "restored-close: app exited before readiness" >&2
        tail -100 "$root/app.log" >&2 || true
        exit 1
    }
    sleep 0.1
done
[[ "$ready" == 1 ]] || {
    echo "restored-close: app did not become ready" >&2
    tail -100 "$root/app.log" >&2 || true
    exit 1
}

before="$root/before.json"
query_state >"$before"
active_tab_id="$(jq -er '.data.active_tab' "$before")"
target_tab_id="$(jq -er '
    [.data.workspaces[].tabs[] | select(.name == "Restored-Target")][0].tab_id
' "$before")"
jq -e --argjson active "$active_tab_id" --argjson target "$target_tab_id" '
    .data.active_tab == $active
    and ([.data.workspaces[].tabs[] | select(.tab_id == $target)][0].panes[0].attach_kind
         == "unsupported")
' "$before" >/dev/null

# Let startup probes settle before taking leak/storm baselines.
sleep 2
fd_before="$(find "/proc/$app_pid/fd" -mindepth 1 -maxdepth 1 | wc -l)"
threads_before="$(find "/proc/$app_pid/task" -mindepth 1 -maxdepth 1 -type d | wc -l)"
wchar_before="$(awk '/^wchar:/ {print $2}' "/proc/$app_pid/io")"
ticks_before="$(awk '{print $14 + $15}' "/proc/$app_pid/stat")"

window_id="$(
    xdotool search --onlyvisible --name '^taarof$' 2>/dev/null \
        | tail -1
)"
[[ -n "$window_id" ]] || {
    echo "restored-close: visible taarof window not found" >&2
    exit 1
}
eval "$(xdotool getwindowgeometry --shell "$window_id")"

# The fixed regression fixture is the first workspace with Active as row one
# and Restored-Target as row two. The normal sidebar is 220px wide; this point
# lands on row two's close button in the Kasm 1024x768 desktop.
xdotool mousemove --sync "$((X + 205))" "$((Y + 263))"
sleep 0.2
xdotool click 1

after="$root/after.json"
query_state >"$after"
jq -e --argjson active "$active_tab_id" --argjson target "$target_tab_id" '
    .ok == true
    and .data.active_tab == $active
    and ([.data.workspaces[].tabs[] | select(.tab_id == $target)] | length == 0)
    and ([.data.workspaces[].tabs[] | select(.name == "Restored-Sibling")][0].panes[0].attach_kind
         == "unsupported")
' "$after" >/dev/null

# The query above proves GTK/main-context liveness immediately after the real
# click. One second later, CPU and write activity must have settled rather than
# continuing the incident's one-core / high-throughput feedback storm.
sleep 1
wchar_after="$(awk '/^wchar:/ {print $2}' "/proc/$app_pid/io")"
ticks_after="$(awk '{print $14 + $15}' "/proc/$app_pid/stat")"
fd_after="$(find "/proc/$app_pid/fd" -mindepth 1 -maxdepth 1 | wc -l)"
threads_after="$(find "/proc/$app_pid/task" -mindepth 1 -maxdepth 1 -type d | wc -l)"

wchar_delta=$((wchar_after - wchar_before))
tick_delta=$((ticks_after - ticks_before))
((wchar_delta < 1048576)) || {
    echo "restored-close: process writes did not settle ($wchar_delta bytes)" >&2
    exit 1
}
((tick_delta < 50)) || {
    echo "restored-close: CPU did not settle ($tick_delta ticks)" >&2
    exit 1
}
((fd_after <= fd_before + 2)) || {
    echo "restored-close: fd count grew ($fd_before -> $fd_after)" >&2
    exit 1
}
((threads_after <= threads_before + 2)) || {
    echo "restored-close: thread count grew ($threads_before -> $threads_after)" >&2
    exit 1
}

echo "restored background tab close: PASS"
echo "active_tab=$active_tab_id closed_tab=$target_tab_id"
echo "wchar_delta=$wchar_delta cpu_ticks=$tick_delta fds=$fd_before->$fd_after threads=$threads_before->$threads_after"
