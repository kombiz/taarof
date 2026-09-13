#!/usr/bin/env bash
# Observe an explicit installation. All launcher traffic uses synthetic roots.
set -euo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
exec python3 "$script_dir/agent_launcher_smoke.py" "$@"
