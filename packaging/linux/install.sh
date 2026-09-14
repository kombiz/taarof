#!/usr/bin/env bash
set -euo pipefail

desktop_id="io.github.kombiz.taarof"
prefix="${1:-$HOME/.local}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python_requirement_message="python3 >= 3.11 is required to install the taarof CLI"

binary_source_path="$script_dir/bin/taarof-app"
cli_source_path="$script_dir/bin/taarof"
desktop_source_path="$script_dir/share/applications/$desktop_id.desktop"
metainfo_source_path="$script_dir/share/metainfo/$desktop_id.metainfo.xml"
icon_source_path="$script_dir/share/icons/hicolor/scalable/apps/$desktop_id.svg"
web_source_dir="$script_dir/share/taarof/web"
manifest_source_path="$script_dir/share/taarof/install-manifest.json"

binary_install_path="$prefix/bin/taarof-app"
cli_install_path="$prefix/bin/taarof"
desktop_install_path="$prefix/share/applications/$desktop_id.desktop"
metainfo_install_path="$prefix/share/metainfo/$desktop_id.metainfo.xml"
icon_install_path="$prefix/share/icons/hicolor/scalable/apps/$desktop_id.svg"
web_install_dir="$prefix/share/taarof/web"
manifest_install_path="$prefix/share/taarof/install-manifest.json"

escape_sed_replacement() {
    printf '%s' "$1" | sed -e 's/[\\|&]/\\&/g'
}

require_cli_python() {
    if ! command -v python3 >/dev/null 2>&1; then
        echo "$python_requirement_message" >&2
        exit 1
    fi

    if ! python3 - <<'PY'
import sys

raise SystemExit(0 if sys.version_info >= (3, 11) else 1)
PY
    then
        echo "$python_requirement_message" >&2
        exit 1
    fi
}

require_file() {
    if [[ ! -f "$1" ]]; then
        echo "missing bundled file: $1" >&2
        exit 1
    fi
}

require_file "$binary_source_path"
require_file "$cli_source_path"
require_file "$script_dir/bin/agent"
require_file "$desktop_source_path"
require_file "$metainfo_source_path"
require_file "$icon_source_path"
require_file "$web_source_dir/index.html"
require_file "$manifest_source_path"
require_cli_python
require_file "$script_dir/share/taarof/bundle-manifest.json"
require_file "$script_dir/share/taarof/bundle-provenance.py"
python3 "$script_dir/share/taarof/bundle-provenance.py" verify \
    --manifest "$script_dir/share/taarof/bundle-manifest.json" \
    --app "$binary_source_path" --app-sidecar "$manifest_source_path" \
    --agent "$script_dir/bin/agent" --cli "$cli_source_path"

install -Dm755 "$binary_source_path" "$binary_install_path"
install -Dm755 "$cli_source_path" "$cli_install_path"
install -Dm755 "$script_dir/bin/agent" "$prefix/bin/agent"
install -d "$(dirname "$desktop_install_path")" "$(dirname "$metainfo_install_path")" "$web_install_dir"
sed \
    -e "s|^Exec=.*$|Exec=$(escape_sed_replacement "$binary_install_path")|" \
    -e "s|^TryExec=.*$|TryExec=$(escape_sed_replacement "$binary_install_path")|" \
    "$desktop_source_path" >"$desktop_install_path"
chmod 644 "$desktop_install_path"
install -Dm644 "$metainfo_source_path" "$metainfo_install_path"
install -Dm644 "$icon_source_path" "$icon_install_path"
rm -rf "$web_install_dir"
mkdir -p "$web_install_dir"
cp -a "$web_source_dir"/. "$web_install_dir"/

# Verify the sidecar against the bundled artifact before copying. A corrupted
# release must not gain a plausible provenance record at its destination.
source "$script_dir/share/taarof/source-provenance.sh"
if ! command -v taarof_copy_artifact_provenance >/dev/null 2>&1; then
    # The bundle carries a self-contained verifier copied by release-bundle.
    echo "bundle provenance verifier missing" >&2
    exit 1
fi
taarof_copy_artifact_provenance "$manifest_source_path" "$binary_source_path" "$manifest_install_path"
python3 "$script_dir/share/taarof/bundle-provenance.py" verify \
    --manifest "$script_dir/share/taarof/bundle-manifest.json" \
    --app "$binary_install_path" --app-sidecar "$manifest_install_path" \
    --agent "$prefix/bin/agent" --cli "$cli_install_path"
install -Dm644 "$script_dir/share/taarof/bundle-manifest.json" "$prefix/share/taarof/bundle-manifest.json"

# Explicit helper payload; never modify shell startup files.
for helper in osc7.bash osc7.zsh osc7.fish agent-status.bash agent-status.zsh taarof-shell-integration.sh taarof-shell-integration.fish; do
    install -Dm644 "$script_dir/share/taarof/shell/$helper" "$prefix/share/taarof/shell/$helper"
done

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$prefix/share/applications" || true
fi

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" || true
fi

echo "installed taarof desktop assets, CLI, and web bundle to $prefix"
