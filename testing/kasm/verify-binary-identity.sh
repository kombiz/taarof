#!/usr/bin/env bash
set -euo pipefail

built_binary="${1:?usage: verify-binary-identity.sh BUILT INSTALLED [RUNNING_PID]}"
installed_binary="${2:?usage: verify-binary-identity.sh BUILT INSTALLED [RUNNING_PID]}"
running_pid="${3:-}"

hash_binary() {
    local path="$1"
    if [[ ! -f "$path" ]]; then
        echo "binary is not a readable regular file: $path" >&2
        exit 1
    fi
    sha256sum "$path" | awk '{print $1}'
}

built_hash="$(hash_binary "$built_binary")"
installed_hash="$(hash_binary "$installed_binary")"

printf 'built sha256:     %s  %s\n' "$built_hash" "$built_binary"
printf 'installed sha256: %s  %s\n' "$installed_hash" "$installed_binary"

if [[ "$installed_hash" != "$built_hash" ]]; then
    echo "installed binary hash mismatch: built=$built_hash installed=$installed_hash" >&2
    exit 1
fi

if [[ -n "$running_pid" ]]; then
    running_binary="/proc/$running_pid/exe"
    running_path="$(readlink -f "$running_binary" 2>/dev/null || true)"
    if [[ -z "$running_path" ]]; then
        echo "running binary is unavailable for pid $running_pid" >&2
        exit 1
    fi
    running_hash="$(hash_binary "$running_binary")"
    printf 'running sha256:   %s  %s\n' "$running_hash" "$running_path"
    if [[ "$running_hash" != "$built_hash" ]]; then
        kill -TERM "$running_pid" 2>/dev/null || true
        echo "running binary hash mismatch: built=$built_hash running=$running_hash" >&2
        exit 1
    fi
fi
