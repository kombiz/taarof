#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
out_dir="${1:-$repo_root/dist/release}"
desktop_id="io.github.kombiz.taarof"
artifact_name="taarof-linux-x86_64"
bundle_root="$out_dir/$artifact_name"
tarball_path="$out_dir/$artifact_name.tar.gz"
checksum_path="$tarball_path.sha256"

binary_path="$repo_root/taarof-app/target/release/taarof-app"
artifact_sidecar_path="$binary_path.provenance.json"
cli_source_path="$repo_root/taarof-cli/taarof"
web_dist_path="$repo_root/taarof-web/dist"

# shellcheck source=packaging/linux/source-provenance.sh
. "$script_dir/source-provenance.sh"

require_file() {
    if [[ ! -f "$1" ]]; then
        echo "missing required file: $1" >&2
        exit 1
    fi
}

require_file "$binary_path"
if [[ ! -f "$artifact_sidecar_path" ]]; then
    TAAROF_ARTIFACT_SIDECAR="$artifact_sidecar_path" \
        bash "$script_dir/emit-artifact-provenance.sh" "$binary_path"
else
    # A sidecar left beside a rebuilt binary is not provenance for that binary.
    # Re-emit it to a private temporary file from the build stamp (which also
    # proves the stamp build ID is embedded in this executable), then require
    # exact equality before the existing sidecar can enter a release tarball.
    expected_sidecar_path="$(mktemp)"
    cleanup_expected_sidecar() {
        rm -f "$expected_sidecar_path"
    }
    trap cleanup_expected_sidecar EXIT
    TAAROF_ARTIFACT_SIDECAR="$expected_sidecar_path" \
        bash "$script_dir/emit-artifact-provenance.sh" "$binary_path"
    if ! cmp -s "$artifact_sidecar_path" "$expected_sidecar_path"; then
        echo "artifact provenance sidecar does not match the exact binary build: $artifact_sidecar_path" >&2
        exit 1
    fi
    rm -f "$expected_sidecar_path"
    trap - EXIT
fi
require_file "$artifact_sidecar_path"
require_file "$cli_source_path"
require_file "$web_dist_path/index.html"
require_file "$script_dir/install.sh"
require_file "$script_dir/source-provenance.sh"
require_file "$script_dir/emit-artifact-provenance.sh"
require_file "$script_dir/$desktop_id.desktop"
require_file "$script_dir/$desktop_id.metainfo.xml"
require_file "$script_dir/icons/hicolor/scalable/apps/$desktop_id.svg"
require_file "$repo_root/LICENSE"

rm -rf "$bundle_root" "$tarball_path" "$checksum_path"
mkdir -p \
    "$bundle_root/bin" \
    "$bundle_root/share/applications" \
    "$bundle_root/share/metainfo" \
    "$bundle_root/share/icons/hicolor/scalable/apps" \
    "$bundle_root/share/taarof/web"

install -Dm755 "$binary_path" "$bundle_root/bin/taarof-app"
install -Dm644 "$artifact_sidecar_path" "$bundle_root/share/taarof/install-manifest.json"
install -Dm644 "$script_dir/source-provenance.sh" "$bundle_root/share/taarof/source-provenance.sh"
install -Dm755 "$cli_source_path" "$bundle_root/bin/taarof"
install -Dm755 "$script_dir/install.sh" "$bundle_root/install.sh"
install -Dm644 "$script_dir/$desktop_id.desktop" "$bundle_root/share/applications/$desktop_id.desktop"
install -Dm644 "$script_dir/$desktop_id.metainfo.xml" "$bundle_root/share/metainfo/$desktop_id.metainfo.xml"
install -Dm644 \
    "$script_dir/icons/hicolor/scalable/apps/$desktop_id.svg" \
    "$bundle_root/share/icons/hicolor/scalable/apps/$desktop_id.svg"
install -Dm644 "$repo_root/LICENSE" "$bundle_root/LICENSE"

if [[ -f "$repo_root/LICENSE-MIT" ]]; then
    install -Dm644 "$repo_root/LICENSE-MIT" "$bundle_root/LICENSE-MIT"
fi
if [[ -f "$repo_root/LICENSE-APACHE" ]]; then
    install -Dm644 "$repo_root/LICENSE-APACHE" "$bundle_root/LICENSE-APACHE"
fi

cp -a "$web_dist_path"/. "$bundle_root/share/taarof/web"/

if git -C "$repo_root" rev-parse --verify HEAD >/dev/null 2>&1; then
    source_date_epoch="${SOURCE_DATE_EPOCH:-$(git -C "$repo_root" log -1 --format=%ct)}"
else
    # Deterministic fallback when building from an exported source tree.
    source_date_epoch="${SOURCE_DATE_EPOCH:-1704067200}"
fi

# The already SHA-bound artifact sidecar is copied verbatim; installation only
# moves it after re-verifying the matching binary.

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
echo "$checksum_path"
