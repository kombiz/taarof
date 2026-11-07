#!/usr/bin/env bash
# Contract for packaging/linux/remote-helper-bundle.sh: the taarof-remote
# release asset is static, laid out for mise's github backend, checksummed,
# and byte-reproducible.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT

target="x86_64-unknown-linux-musl"
artifact="taarof-remote-$target"
release_dir="$fixture/target/$target/release"
mkdir -p "$release_dir"
export CARGO_TARGET_DIR="$fixture/target"
export SOURCE_DATE_EPOCH=1704067200

# A dynamically linked binary is refused.
cp "$(type -P env)" "$release_dir/taarof-remote"
if bash "$repo_root/packaging/linux/remote-helper-bundle.sh" "$fixture/dist" \
    >/dev/null 2>"$fixture/err"; then
    echo "remote helper bundle accepted a dynamically linked binary" >&2
    exit 1
fi
grep -F "dynamically linked" "$fixture/err" >/dev/null
test ! -e "$fixture/dist/$artifact.tar.gz"

# A file readelf cannot inspect is refused, not packaged.
printf '%s\n' 'not an ELF file' >"$release_dir/taarof-remote"
chmod 755 "$release_dir/taarof-remote"
if bash "$repo_root/packaging/linux/remote-helper-bundle.sh" "$fixture/dist" \
    >/dev/null 2>"$fixture/err"; then
    echo "remote helper bundle accepted a file it could not inspect" >&2
    exit 1
fi
grep -F "could not read ELF program headers" "$fixture/err" >/dev/null
test ! -e "$fixture/dist/$artifact.tar.gz"

# The dynamic loader has no program interpreter of its own, so it stands in
# for a static binary without needing the musl target here.
loader="$(readelf -l "$(type -P env)" | sed -n 's/.*program interpreter: \(.*\)]/\1/p')"
cp -L "$loader" "$release_dir/taarof-remote"
chmod 755 "$release_dir/taarof-remote"

bash "$repo_root/packaging/linux/remote-helper-bundle.sh" "$fixture/dist" >/dev/null
(cd "$fixture/dist" && sha256sum -c "$artifact.tar.gz.sha256" >/dev/null)
listing="$(tar -tzf "$fixture/dist/$artifact.tar.gz" | sort)"
expected="$(printf '%s\n' "$artifact/" "$artifact/LICENSE" "$artifact/LICENSE-APACHE" \
    "$artifact/LICENSE-MIT" "$artifact/taarof-remote" | sort)"
if [[ "$listing" != "$expected" ]]; then
    printf 'unexpected asset layout:\n%s\n' "$listing" >&2
    exit 1
fi
tar -tvzf "$fixture/dist/$artifact.tar.gz" "$artifact/taarof-remote" | grep -q '^-rwxr-xr-x 0/0'

# Rebuilding the same input yields the same bytes.
first="$(cut -d' ' -f1 "$fixture/dist/$artifact.tar.gz.sha256")"
bash "$repo_root/packaging/linux/remote-helper-bundle.sh" "$fixture/dist" >/dev/null
test "$first" = "$(cut -d' ' -f1 "$fixture/dist/$artifact.tar.gz.sha256")"

echo "remote helper bundle contract ok"
