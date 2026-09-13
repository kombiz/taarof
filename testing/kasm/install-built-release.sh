#!/usr/bin/env bash
set -euo pipefail

repo_root="${1:?usage: install-built-release.sh REPO_ROOT PREFIX}"
prefix="${2:?usage: install-built-release.sh REPO_ROOT PREFIX}"

if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
    if [[ "$CARGO_TARGET_DIR" = /* ]]; then
        target_dir="$CARGO_TARGET_DIR"
    else
        target_dir="$repo_root/$CARGO_TARGET_DIR"
    fi
    agent_target_dir="$target_dir"
else
    target_dir="$repo_root/taarof-app/target"
    agent_target_dir="$repo_root/taarof-app/target"
fi

built_binary="$target_dir/release/taarof-app"
if [[ ! -f "$built_binary" || ! -x "$built_binary" ]]; then
    echo "missing executable release artifact: $built_binary" >&2
    exit 1
fi

TAAROF_INSTALL_AGENT_BINARY="$agent_target_dir/release/agent" \
TAAROF_INSTALL_BINARY="$built_binary" \
    bash "$repo_root/packaging/linux/install-local.sh" "$prefix"
