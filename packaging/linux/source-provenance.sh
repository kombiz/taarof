#!/usr/bin/env bash
# Artifact provenance helpers. Source claims are accepted only from a sidecar
# whose SHA binds it to the exact binary being installed; installers never
# inspect their ambient checkout to manufacture provenance.

taarof_sha256() {
    [[ -f "$1" ]] || return 0
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}';
    elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'; fi
}

# taarof_verify_artifact_sidecar <sidecar> <binary>
# Prints nothing and returns non-zero for absent, foreign, malformed, or
# replacement sidecars. Python is already required by the installed CLI.
taarof_verify_artifact_sidecar() {
    local sidecar="$1" binary="$2" actual
    [[ -f "$sidecar" ]] || return 1
    actual="$(taarof_sha256 "$binary")"
    [[ -n "$actual" ]] || return 1
    python3 - "$sidecar" "$actual" <<'PY'
import json
import re
import sys

try:
    value = json.load(open(sys.argv[1], encoding="utf-8"))
except (OSError, ValueError):
    raise SystemExit(1)
sha = value.get("binary_sha256") if isinstance(value, dict) else None
valid = isinstance(sha, str) and re.fullmatch(r"[0-9a-fA-F]{64}", sha)
required = ("app_version", "build_id", "source_revision", "source_describe", "source_dirty")
raise SystemExit(0 if value.get("schema") == "taarof.artifact.v1" and valid
                 and sha == sys.argv[2] and all(value.get(key) not in (None, "") for key in required)
                 else 1)
PY
}

# Copy only a verified artifact-sidecar. A failure is deliberately non-fatal to
# the install: the binary is still usable, but runtime identity becomes unknown.
taarof_copy_artifact_provenance() {
    local sidecar="$1" binary="$2" destination="$3"
    if ! taarof_verify_artifact_sidecar "$sidecar" "$binary"; then
        echo "note: no valid artifact provenance sidecar for $binary; runtime source identity will be unknown" >&2
        return 1
    fi
    install -Dm644 "$sidecar" "$destination"
}
