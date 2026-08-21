#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

WASM="target/wasm32-wasip1/release/taarof-sidebar.wasm"
DEST="${HOME}/.config/zellij/plugins/taarof-sidebar.wasm"

if [[ ! -f "$WASM" ]]; then
  echo "Build first: ./build.sh"
  exit 1
fi

mkdir -p "$(dirname "$DEST")"
cp "$WASM" "$DEST"
echo "Installed: $DEST ($(du -h "$DEST" | cut -f1))"
