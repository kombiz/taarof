#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
fixture="$(mktemp -d)"

cleanup() {
    rm -rf "$fixture"
}
trap cleanup EXIT

mkdir -p \
    "$fixture/taarof-app/target/release" \
    "$fixture/taarof-app/target/build/fixture/out" \
    "$fixture/taarof-web/dist"
cp -a "$repo_root/packaging" "$fixture/packaging"
cp -a "$repo_root/taarof-cli" "$fixture/taarof-cli"
cp "$repo_root/LICENSE" "$fixture/LICENSE"
printf '<!doctype html>\n' >"$fixture/taarof-web/dist/index.html"

binary="$fixture/taarof-app/target/release/taarof-app"
stamp="$fixture/taarof-app/target/build/fixture/out/taarof-build-stamp.json"
printf '%s\n' 'fixture binary fixture-build-id' >"$binary"
chmod 755 "$binary"
printf '%s\n' \
    '{"schema":"taarof.build-stamp.v1","app_version":"0.0.0","build_id":"fixture-build-id","source_revision":"deadbeef","source_describe":"deadbeef","source_dirty":false}' \
    >"$stamp"
bash "$fixture/packaging/linux/emit-artifact-provenance.sh" "$binary"

# Keep the build ID/version valid but replace the bytes. A release bundle must
# reject this stale sidecar rather than attaching old provenance to new bytes.
printf '%s\n' 'replaced fixture binary fixture-build-id' >"$binary"
if bash "$fixture/packaging/linux/release-bundle.sh" "$fixture/dist" \
    >"$fixture/out" 2>"$fixture/err"; then
    echo "release bundle accepted a stale artifact provenance sidecar" >&2
    exit 1
fi
grep -F "artifact provenance sidecar does not match the exact binary build" \
    "$fixture/err" >/dev/null
test ! -e "$fixture/dist/taarof-linux-x86_64.tar.gz"

echo "release bundle rejects stale artifact provenance sidecars"
