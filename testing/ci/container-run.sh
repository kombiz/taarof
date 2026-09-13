#!/usr/bin/env bash
# Run the repository gate in a purpose-built GTK/VTE image as the host user.
set -euo pipefail

if [[ $# -eq 0 ]]; then
    echo "usage: $0 <command...>" >&2
    exit 64
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
image="taarof-ci:local"

resolve_ipv4() {
    local host="$1"
    local address
    address="$(getent ahostsv4 "$host" | awk 'NR == 1 { print $1 }')"
    if [[ ! "$address" =~ ^[0-9]+(\.[0-9]+){3}$ ]]; then
        echo "could not resolve an IPv4 address for $host" >&2
        exit 69
    fi
    printf '%s' "$address"
}

# The builder daemon's bridge resolver is intentionally not trusted. Inject
# only the two mirrors present in archlinux:base instead of exposing build
# steps to the host network.
fastly_ip="$(resolve_ipv4 fastly.mirror.pkgbuild.com)"
geo_ip="$(resolve_ipv4 geo.mirror.pkgbuild.com)"
docker build \
    --build-arg CI_UID="$(id -u)" \
    --build-arg CI_GID="$(id -g)" \
    --add-host "fastly.mirror.pkgbuild.com:$fastly_ip" \
    --add-host "geo.mirror.pkgbuild.com:$geo_ip" \
    --file "$repo_root/testing/ci/Dockerfile" --tag "$image" \
    "$repo_root/testing/ci"

exec docker run --rm \
    --user "$(id -u):$(id -g)" \
    --dns 1.1.1.1 \
    --dns 8.8.8.8 \
    --env HOME=/tmp/taarof-ci-home \
    --env CARGO_HOME=/tmp/taarof-ci-cargo \
    --env CARGO_TARGET_DIR=/tmp/taarof-ci-target \
    --volume "$repo_root:/repo" \
    --workdir /repo \
    "$image" \
    bash -c 'mkdir -p "$HOME" "$CARGO_HOME" "$CARGO_TARGET_DIR"; exec "$@"' _ "$@"
