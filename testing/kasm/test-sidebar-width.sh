#!/usr/bin/env bash
set -euo pipefail

repo_root="${TAAROF_REPO_ROOT:-/config/taarof}"
display="${DISPLAY:-:1}"

cd "$repo_root"
DISPLAY="$display" cargo +stable test \
    --manifest-path taarof-app/Cargo.toml \
    badge_rich_sidebar_stays_inside_requested_width \
    -- --ignored --nocapture
