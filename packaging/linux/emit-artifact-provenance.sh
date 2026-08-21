#!/usr/bin/env bash
set -euo pipefail

# Emit a SHA-bound sidecar beside a linked artifact. The build stamp originates
# in build.rs; rejecting a stamp token not embedded in the executable prevents
# a checkout's newer Git state from being attached to an older binary.
binary="${1:?usage: emit-artifact-provenance.sh BINARY [STAMP]}"
sidecar="${TAAROF_ARTIFACT_SIDECAR:-$binary.provenance.json}"
stamp="${2:-}"

stamp_is_embedded_in_binary() {
    python3 - "$1" "$binary" <<'PY'
import json
import sys

try:
    stamp = json.load(open(sys.argv[1], encoding="utf-8"))
    build_id = stamp.get("build_id")
    binary = open(sys.argv[2], "rb").read()
except (OSError, ValueError):
    raise SystemExit(1)
raise SystemExit(0 if isinstance(build_id, str) and build_id.encode() in binary else 1)
PY
}

if [[ -z "$stamp" ]]; then
    target_dir="$(dirname "$(dirname "$binary")")"
    while IFS= read -r candidate; do
        if stamp_is_embedded_in_binary "$candidate"; then
            stamp="$candidate"
            break
        fi
    done < <(find "$target_dir" -path '*/out/taarof-build-stamp.json' -type f -print 2>/dev/null | sort -r)
fi
[[ -x "$binary" && -f "$stamp" ]] || { echo "missing binary or build stamp" >&2; exit 1; }

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$script_dir/source-provenance.sh"
sha="$(taarof_sha256 "$binary")"
[[ -n "$sha" ]] || { echo "could not hash $binary" >&2; exit 1; }

python3 - "$stamp" "$binary" "$sha" "$sidecar" <<'PY'
import json
import sys

stamp_path, binary_path, sha, output = sys.argv[1:]
try:
    stamp = json.load(open(stamp_path, encoding="utf-8"))
except (OSError, ValueError) as err:
    raise SystemExit(f"invalid build stamp: {err}")
required = ("app_version", "build_id", "source_revision", "source_describe", "source_dirty")
missing = [key for key in required if stamp.get(key) in (None, "")]
if stamp.get("schema") != "taarof.build-stamp.v1" or missing:
    raise SystemExit(
        "build stamp has unknown provenance "
        f"(schema={stamp.get('schema')!r}, missing={missing}): {stamp_path}"
    )
with open(binary_path, "rb") as handle:
    if stamp["build_id"].encode() not in handle.read():
        raise SystemExit("build stamp is not embedded in this binary")
artifact = {key: stamp.get(key) for key in (
    "app_version", "build_id", "source_revision", "source_describe",
    "source_dirty", "source_root", "profile", "built_at_unix",
)}
artifact.update(schema="taarof.artifact.v1", binary_sha256=sha)
with open(output, "w", encoding="utf-8") as handle:
    json.dump(artifact, handle, sort_keys=True, separators=(",", ":"))
    handle.write("\n")
PY
