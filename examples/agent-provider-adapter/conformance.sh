#!/usr/bin/env bash
# Run from any directory against an explicitly enabled synthetic provider.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
if [[ $# != 1 ]]; then
  echo 'Usage: conformance.sh <enabled-provider-id>' >&2
  exit 2
fi
cargo test --manifest-path "$root/agent-launcher/Cargo.toml" external_adapter
cargo run --quiet --manifest-path "$root/agent-launcher/Cargo.toml" -- conformance "$1"
