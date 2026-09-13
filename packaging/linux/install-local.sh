#!/usr/bin/env bash
set -euo pipefail

desktop_id="io.github.kombiz.taarof"
prefix="${1:-$HOME/.local}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
default_binary_path="${CARGO_TARGET_DIR:-$repo_root/taarof-app/target}/release/taarof-app"
binary_path="${TAAROF_INSTALL_BINARY:-$default_binary_path}"
binary_install_path="$prefix/bin/taarof-app"
cli_source_path="$repo_root/taarof-cli/taarof"
cli_install_path="$prefix/bin/taarof"
agent_binary_path="${TAAROF_INSTALL_AGENT_BINARY:-${CARGO_TARGET_DIR:-$repo_root/taarof-app/target}/release/agent}"
desktop_install_path="$prefix/share/applications/$desktop_id.desktop"
metainfo_install_path="$prefix/share/metainfo/$desktop_id.metainfo.xml"
web_dist_path="$repo_root/taarof-web/dist"
web_install_dir="$prefix/share/taarof/web"
install_manifest_path="$prefix/share/taarof/install-manifest.json"
default_sidecar_path="$default_binary_path.provenance.json"
artifact_sidecar_path="${TAAROF_INSTALL_PROVENANCE:-${TAAROF_INSTALL_BINARY:+$binary_path.provenance.json}}"
artifact_sidecar_path="${artifact_sidecar_path:-$default_sidecar_path}"

# shellcheck source=packaging/linux/source-provenance.sh
. "$script_dir/source-provenance.sh"

manifest_license="$(
    sed -n 's/^license[[:space:]]*=[[:space:]]*"\([^"]*\)"/\1/p' \
        "$repo_root/taarof-app/Cargo.toml" \
        | head -n 1
)"
project_license="${TAAROF_PROJECT_LICENSE:-${manifest_license:-NOASSERTION}}"

escape_sed_replacement() {
    printf '%s' "$1" | sed -e 's/[\\|&]/\\&/g'
}

require_cli_python() {
    if ! command -v python3 >/dev/null 2>&1; then
        echo "python3 >= 3.11 is required to install the taarof CLI" >&2
        exit 1
    fi

    if ! python3 - <<'PY'
import sys

raise SystemExit(0 if sys.version_info >= (3, 11) else 1)
PY
    then
        echo "python3 >= 3.11 is required to install the taarof CLI" >&2
        exit 1
    fi
}

if [[ ! -f "$binary_path" || ! -x "$binary_path" ]]; then
    echo "missing $binary_path" >&2
    if [[ -n "${TAAROF_INSTALL_BINARY:-}" ]]; then
        echo "TAAROF_INSTALL_BINARY must name an executable regular file" >&2
    else
        echo "build it first with: cargo build --release --manifest-path \"$repo_root/taarof-app/Cargo.toml\"" >&2
    fi
    exit 1
fi

if [[ ! -f "$cli_source_path" ]]; then
    echo "missing $cli_source_path" >&2
    exit 1
fi

if [[ ! -f "$web_dist_path/index.html" ]]; then
    echo "missing built web assets under $web_dist_path" >&2
    echo "build them first with: (cd \"$repo_root/taarof-web\" && npm install && npm run build)" >&2
    exit 1
fi

if [[ ! -f "$agent_binary_path" || ! -x "$agent_binary_path" ]]; then
    echo "missing $agent_binary_path; build with cargo build --release --manifest-path agent-launcher/Cargo.toml" >&2
    exit 1
fi

require_cli_python

install -Dm755 "$agent_binary_path" "$prefix/bin/agent"
install -Dm755 "$binary_path" "$binary_install_path"
install -Dm755 "$cli_source_path" "$cli_install_path"
install -d "$(dirname "$desktop_install_path")" "$(dirname "$metainfo_install_path")" "$web_install_dir"
sed \
    -e "s|^Exec=.*$|Exec=$(escape_sed_replacement "$binary_install_path")|" \
    -e "s|^TryExec=.*$|TryExec=$(escape_sed_replacement "$binary_install_path")|" \
    "$script_dir/$desktop_id.desktop" >"$desktop_install_path"
chmod 644 "$desktop_install_path"
sed \
    -e "s|<project_license>.*</project_license>|<project_license>$(escape_sed_replacement "$project_license")</project_license>|" \
    "$script_dir/$desktop_id.metainfo.xml" >"$metainfo_install_path"
chmod 644 "$metainfo_install_path"
install -Dm644 \
    "$script_dir/icons/hicolor/scalable/apps/$desktop_id.svg" \
    "$prefix/share/icons/hicolor/scalable/apps/$desktop_id.svg"
rm -rf "$web_install_dir"
mkdir -p "$web_install_dir"
cp -a "$web_dist_path"/. "$web_install_dir"/

# Ship helpers without editing any user's startup files.
for helper in osc7.bash osc7.zsh osc7.fish agent-status.bash agent-status.zsh; do
    install -Dm644 "$repo_root/taarof-app/resources/$helper" "$prefix/share/taarof/shell/$helper"
done
for helper in taarof-shell-integration.sh taarof-shell-integration.fish; do
    install -Dm644 "$repo_root/examples/$helper" "$prefix/share/taarof/shell/$helper"
done


# The sidecar was emitted after linking and binds its source claim to the exact
# artifact by SHA. Never reconstruct it from this checkout: that would label an
# arbitrary TAAROF_INSTALL_BINARY with unrelated ambient Git state.
taarof_copy_artifact_provenance "$artifact_sidecar_path" "$binary_path" "$install_manifest_path" || true

# Development installs remain usable when source proof is unavailable, but an
# old complete manifest must never describe replacement artifact bytes.
bundle_manifest_path="$prefix/share/taarof/bundle-manifest.json"
rm -f "$bundle_manifest_path"
if ! python3 "$script_dir/bundle-provenance.py" create \
    --source-root "$repo_root" --app "$binary_install_path" \
    --app-sidecar "$install_manifest_path" --agent "$prefix/bin/agent" \
    --cli "$cli_install_path" --output "$bundle_manifest_path"; then
    echo "note: complete bundle provenance unavailable; this installation is not source-verified" >&2
fi

if [[ "$project_license" == "NOASSERTION" ]]; then
    echo "warning: metainfo project_license is still NOASSERTION; set TAAROF_PROJECT_LICENSE to an SPDX identifier if you need to override the packaged license metadata" >&2
fi

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$prefix/share/applications" || true
fi

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" || true
fi

echo "installed taarof desktop assets, CLI, and web bundle to $prefix"
