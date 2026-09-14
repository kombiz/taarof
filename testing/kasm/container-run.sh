#!/usr/bin/env bash
# Run a command inside the visual-test container as the desktop user, not root.
#
# Why this exists:
#
# 1. `docker compose run` executes as root, because the linuxserver base image has
#    no USER directive (s6 init needs root, then drops to `abc` itself). Anything a
#    root-run cargo/npm writes into the /config/taarof bind mount lands in the host
#    tree owned by root:root, later breaking host builds: cargo cannot overwrite the
#    release binary, `npm ci` fails EACCES on unlink, vite cannot empty dist/.
#    s6 remaps `abc` to PUID/PGID (1000:1000), so dropping to it yields host-owned files.
#
# 2. s6-overlay strips the environment from the container command and re-exposes it
#    under /run/s6/container_environment. Compose `environment:` entries are therefore
#    invisible to a plain `bash -lc`, so we re-import the ones we need by hand.
#
# 3. CARGO_TARGET_DIR and CARGO_HOME must live outside the bind mount and be writable
#    by `abc`. The image ships CARGO_HOME=/usr/local/cargo owned by root, which a
#    dropped-privilege cargo cannot write (registry cache). Both are redirected into
#    the named volume, which also persists the build cache across runs.
#
# Usage: bash testing/kasm/container-run.sh 'cargo +stable test --manifest-path taarof-app/Cargo.toml'
set -euo pipefail

if [[ $# -eq 0 ]]; then
    echo "usage: $0 <command...>" >&2
    exit 64
fi

compose_file="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/docker-compose.yml"

exec docker compose -f "$compose_file" run --rm taarof-desktop bash -lc '
    set -euo pipefail

    # s6-overlay hides the container environment from this command; re-import it.
    env_dir=/run/s6/container_environment
    if [[ -f "$env_dir/CARGO_TARGET_DIR" ]]; then
        target="$(cat "$env_dir/CARGO_TARGET_DIR")"
    else
        target=/container-target
    fi
    cargo_home="$target/cargo-home"

    # The named volume is created root-owned; cargo runs as abc and must write both.
    install -d -o abc -g abc "$target" "$cargo_home"

    cd /config/taarof
    exec s6-setuidgid abc env \
        CARGO_TARGET_DIR="$target" \
        CARGO_HOME="$cargo_home" \
        RUSTUP_HOME="${RUSTUP_HOME:-/usr/local/rustup}" \
        bash -lc "$1"
' _ "$*"
