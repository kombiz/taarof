#!/usr/bin/env bash
# Package the static taarof-remote helper as a release asset that mise's
# github backend can install: one top-level directory holding the binary and
# its licenses, plus a sha256 sidecar. See docs/remote-helper.md.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
out_dir="${1:-$repo_root/dist/release}"
target="x86_64-unknown-linux-musl"
artifact_name="taarof-remote-$target"
binary_path="${CARGO_TARGET_DIR:-$repo_root/taarof-app/target}/$target/release/taarof-remote"
bundle_root="$out_dir/$artifact_name"
tarball_path="$out_dir/$artifact_name.tar.gz"

if [[ ! -x "$binary_path" ]]; then
    echo "missing helper binary: $binary_path" >&2
    exit 1
fi
# The asset must run on every fleet host whatever its glibc. A dynamically
# linked binary names a program interpreter; refuse one. A failed inspection
# is a failure, not a pass.
if ! program_headers="$(LC_ALL=C readelf -lW "$binary_path")"; then
    echo "could not read ELF program headers of $binary_path" >&2
    exit 1
fi
if grep -q 'program interpreter' <<<"$program_headers"; then
    echo "taarof-remote is dynamically linked; build it for $target" >&2
    exit 1
fi

rm -rf "$bundle_root"
install -Dm755 "$binary_path" "$bundle_root/taarof-remote"
for license in LICENSE LICENSE-APACHE LICENSE-MIT; do
    install -Dm644 "$repo_root/$license" "$bundle_root/$license"
done

if git -C "$repo_root" rev-parse --verify HEAD >/dev/null 2>&1; then
    source_date_epoch="${SOURCE_DATE_EPOCH:-$(git -C "$repo_root" log -1 --format=%ct)}"
else
    source_date_epoch="${SOURCE_DATE_EPOCH:-1704067200}"
fi

tar \
    --sort=name \
    --mtime="@$source_date_epoch" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -czf "$tarball_path" \
    -C "$out_dir" \
    "$artifact_name"

(cd "$out_dir" && sha256sum "$artifact_name.tar.gz" >"$artifact_name.tar.gz.sha256")

echo "$tarball_path"
echo "$tarball_path.sha256"
