#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
cargo build --target wasm32-wasip1 --release
echo "Built: target/wasm32-wasip1/release/taarof-sidebar.wasm"
ls -lh target/wasm32-wasip1/release/taarof-sidebar.wasm
