#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
prefix="$(mktemp -d)"
desktop_id="io.github.kombiz.taarof"
release_out_dir=""
bundle_extract_dir=""
bundle_prefix=""
runtime_dir=""
fake_python_dir=""
fake_prefix=""

cleanup() {
    for path in \
        "$prefix" \
        "$release_out_dir" \
        "$bundle_extract_dir" \
        "$bundle_prefix" \
        "$runtime_dir" \
        "$fake_python_dir" \
        "$fake_prefix"
    do
        if [[ -n "$path" ]]; then
            rm -rf "$path"
        fi
    done
}
trap cleanup EXIT

bash "$repo_root/testing/test-release-bundle-provenance.sh"
python3 "$repo_root/testing/test_bundle_provenance.py"

manifest_license="$(
    sed -n 's/^license[[:space:]]*=[[:space:]]*"\([^"]*\)"/\1/p' \
        "$repo_root/taarof-app/Cargo.toml" \
        | head -n 1
)"

binary_path="$prefix/bin/taarof-app"
cli_path="$prefix/bin/taarof"
desktop_path="$prefix/share/applications/$desktop_id.desktop"
metainfo_path="$prefix/share/metainfo/$desktop_id.metainfo.xml"
icon_path="$prefix/share/icons/hicolor/scalable/apps/$desktop_id.svg"
web_index_path="$prefix/share/taarof/web/index.html"
release_out_dir="$(mktemp -d)"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo_root/taarof-app/target}"
cargo build --locked --release --manifest-path "$repo_root/agent-launcher/Cargo.toml"
cargo build --locked --release --features bundled-sqlite --manifest-path "$repo_root/taarof-app/Cargo.toml"
cargo build --locked --release --manifest-path "$repo_root/taarof-control-gateway/Cargo.toml"
bash "$script_dir/emit-artifact-provenance.sh" "${CARGO_TARGET_DIR:-$repo_root/taarof-app/target}/release/taarof-app"
(cd "$repo_root/taarof-web" && npm ci --include=dev && npm run build)
bash "$script_dir/install-local.sh" "$prefix"

test -x "$binary_path"
test -x "$cli_path"
test -x "$prefix/bin/agent"
test -x "$prefix/bin/taarof-control-gateway"
test -f "$prefix/share/systemd/user/taarof-control-gateway.service"
test -f "$prefix/share/taarof/caddy/taarof-control.caddy"
test -f "$prefix/share/licenses/taarof-control-gateway/LICENSE"
cmp "$repo_root/taarof-control-gateway/LICENSE" \
    "$prefix/share/licenses/taarof-control-gateway/LICENSE"
grep -Fx "ExecStart=$prefix/bin/taarof-control-gateway" \
    "$prefix/share/systemd/user/taarof-control-gateway.service" >/dev/null
for argument in --version --build-info --help providers; do
    HOME="$prefix" "$prefix/bin/agent" "$argument" >/dev/null
done
test -f "$desktop_path"
test -f "$metainfo_path"
test -f "$icon_path"
test -f "$web_index_path"
test -f "$prefix/share/taarof/install-manifest.json"
test -f "$prefix/share/taarof/bundle-manifest.json"
for helper in osc7.bash osc7.zsh osc7.fish agent-status.bash agent-status.zsh taarof-shell-integration.sh taarof-shell-integration.fish; do
    test -f "$prefix/share/taarof/shell/$helper"
done

grep -Fx "Exec=$binary_path" "$desktop_path" >/dev/null
grep -Fx "TryExec=$binary_path" "$desktop_path" >/dev/null
grep -F "<project_license>${manifest_license}</project_license>" "$metainfo_path" >/dev/null

release_bundle_output="$(bash "$script_dir/release-bundle.sh" "$release_out_dir")"
release_tarball_path="$(printf '%s\n' "$release_bundle_output" | head -n 1)"
release_checksum_path="$(printf '%s\n' "$release_bundle_output" | tail -n 1)"
test -f "$release_tarball_path"
test -f "$release_checksum_path"
(cd "$release_out_dir" && sha256sum -c "$(basename "$release_checksum_path")")

bundle_extract_dir="$(mktemp -d)"
bundle_prefix="$(mktemp -d)"
tar -xzf "$release_tarball_path" -C "$bundle_extract_dir"
bash "$bundle_extract_dir/taarof-linux-x86_64/install.sh" "$bundle_prefix"
test -x "$bundle_prefix/bin/taarof-app"
test -x "$bundle_prefix/bin/taarof"
test -x "$bundle_prefix/bin/agent"
HOME="$bundle_prefix" "$bundle_prefix/bin/agent" providers >/dev/null
cmp "$prefix/share/taarof/bundle-manifest.json" "$bundle_prefix/share/taarof/bundle-manifest.json"
test -f "$bundle_prefix/share/applications/$desktop_id.desktop"
test -f "$bundle_prefix/share/metainfo/$desktop_id.metainfo.xml"
test -f "$bundle_prefix/share/icons/hicolor/scalable/apps/$desktop_id.svg"
test -f "$bundle_prefix/share/taarof/web/index.html"
test -f "$bundle_prefix/share/taarof/install-manifest.json"
for helper in osc7.bash osc7.zsh osc7.fish agent-status.bash agent-status.zsh taarof-shell-integration.sh taarof-shell-integration.fish; do
    cmp "$prefix/share/taarof/shell/$helper" "$bundle_prefix/share/taarof/shell/$helper"
done
grep -Fx "Exec=$bundle_prefix/bin/taarof-app" \
    "$bundle_prefix/share/applications/$desktop_id.desktop" >/dev/null
grep -Fx "TryExec=$bundle_prefix/bin/taarof-app" \
    "$bundle_prefix/share/applications/$desktop_id.desktop" >/dev/null

resolved_cli="$(PATH="$prefix/bin:$PATH" command -v taarof)"
test "$resolved_cli" = "$cli_path"

help_output="$("$cli_path" --help)"
printf '%s\n' "$help_output" | grep -F "list-tabs" >/dev/null
printf '%s\n' "$help_output" | grep -F "agent-workspace" >/dev/null

runtime_dir="$(mktemp -d)"
sock_path="$runtime_dir/taarof-test.sock"
registry_path="$runtime_dir/taarof-current.json"

python3 - "$sock_path" "$registry_path" <<'PY' &
import json
import os
import socket
import sys

sock_path, registry_path = sys.argv[1:3]
if os.path.exists(sock_path):
    os.unlink(sock_path)

with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
    server.bind(sock_path)
    server.listen(1)
    with open(registry_path, "w", encoding="utf-8") as handle:
        json.dump({"socket_path": sock_path}, handle)
    conn, _ = server.accept()
    with conn:
        chunks = []
        while True:
            buf = conn.recv(65536)
            if not buf:
                break
            chunks.append(buf)
        payload = json.loads(b"".join(chunks).decode().splitlines()[-1])
        assert payload["action"] == "list-tabs", payload
        response = {"ok": True, "data": {"workspaces": []}}
        conn.sendall((json.dumps(response) + "\n").encode())
PY
server_pid=$!

for _ in $(seq 1 50); do
    if [[ -S "$sock_path" && -f "$registry_path" ]]; then
        break
    fi
    sleep 0.1
done

list_tabs_output="$(XDG_RUNTIME_DIR="$runtime_dir" "$cli_path" list-tabs)"
wait "$server_pid"
printf '%s\n' "$list_tabs_output" | grep -F '{"ok":true,"data":{"workspaces":[]}}' >/dev/null

python3 - "$sock_path" "$registry_path" <<'PY' &
import json
import os
import socket
import sys

sock_path, registry_path = sys.argv[1:3]
if os.path.exists(sock_path):
    os.unlink(sock_path)

with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
    server.bind(sock_path)
    server.listen(2)
    with open(registry_path, "w", encoding="utf-8") as handle:
        json.dump({"socket_path": sock_path}, handle)
    for expected_timeout in (120.0, 7.5):
        conn, _ = server.accept()
        with conn:
            chunks = []
            while True:
                buf = conn.recv(65536)
                if not buf:
                    break
                chunks.append(buf)
            payload = json.loads(b"".join(chunks).decode().splitlines()[-1])
            assert payload == {
                "action": "agent-workspace",
                "branch": "test",
                "command": "echo hi",
                # The CLI pins the server-side operation lifetime to its own
                # synchronous wait so an abandoned request cannot open a workspace.
                "timeout_seconds": expected_timeout,
            }, payload
            response = {
                "ok": True,
                "tab_id": 7,
                "data": {
                    "workspace_id": 3,
                    "worktree_path": "/tmp/taarof-test",
                    "branch": "test",
                },
            }
            conn.sendall((json.dumps(response) + "\n").encode())
PY
server_pid=$!

for _ in $(seq 1 50); do
    if [[ -S "$sock_path" && -f "$registry_path" ]]; then
        break
    fi
    sleep 0.1
done

agent_workspace_output="$(
    XDG_RUNTIME_DIR="$runtime_dir" \
        "$cli_path" agent-workspace --branch test --command 'echo hi'
)"
agent_workspace_override_output="$(
    XDG_RUNTIME_DIR="$runtime_dir" \
        "$cli_path" agent-workspace --branch test --command 'echo hi' --timeout 7.5
)"
wait "$server_pid"
printf '%s\n' "$agent_workspace_output" | grep -F '"tab_id":7' >/dev/null
printf '%s\n' "$agent_workspace_output" | grep -F '"worktree_path":"/tmp/taarof-test"' >/dev/null
printf '%s\n' "$agent_workspace_override_output" | grep -F '"tab_id":7' >/dev/null

fake_python_dir="$(mktemp -d)"
fake_prefix="$(mktemp -d)"
fake_error="$fake_python_dir/install.err"

cat >"$fake_python_dir/python3" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
chmod 755 "$fake_python_dir/python3"

if PATH="$fake_python_dir:$PATH" bash "$script_dir/install-local.sh" "$fake_prefix" \
    > /dev/null 2>"$fake_error"; then
    echo "expected install-local.sh to reject an unusable python3" >&2
    exit 1
fi
grep -F "python3 >= 3.11 is required to install the taarof CLI" "$fake_error" >/dev/null

if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$desktop_path"
fi

if command -v appstreamcli >/dev/null 2>&1; then
    appstreamcli validate --no-net "$metainfo_path"
fi

echo "packaging validation passed"
