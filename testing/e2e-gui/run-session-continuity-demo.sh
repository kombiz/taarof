#!/usr/bin/env bash
# Run the exact supplied app/agent pair against owned synthetic continuity state.
set -euo pipefail

usage() {
  echo "usage: $0 --app ABSOLUTE_APP --agent ABSOLUTE_AGENT --evidence ABSOLUTE_DIR" >&2
}

app=
agent=
evidence=
while (($#)); do
  case "$1" in
    --app) app=${2-}; shift 2 ;;
    --agent) agent=${2-}; shift 2 ;;
    --evidence) evidence=${2-}; shift 2 ;;
    *) usage; exit 2 ;;
  esac
done

for path in "$app" "$agent"; do
  [[ $path = /* && -x $path ]] || { usage; exit 2; }
done
[[ $evidence = /* ]] || { usage; exit 2; }
command -v bwrap >/dev/null
command -v dbus-run-session >/dev/null
command -v xvfb-run >/dev/null
command -v xdotool >/dev/null

mkdir -p "$evidence"
[[ -z $(find "$evidence" -mindepth 1 -maxdepth 1 -print -quit) ]] || {
  echo "evidence directory must be empty" >&2
  exit 2
}
fixture_home=$(mktemp -d -t taarof-continuity-home.XXXXXX)
cleanup() { rm -rf -- "$fixture_home"; }
trap cleanup EXIT

# HOME deliberately remains the product's normal absolute spelling while the
# mount at that path is a new empty directory. The namespace has its own /tmp,
# network, PID, IPC, UTS, X server, D-Bus session, tmux socket, and synthetic
# provider history. No browser profile is created; the app's per-process HTTP
# token stays inside the disposable namespace and is handled in memory only.
exec bwrap \
  --die-with-parent --unshare-pid --unshare-net --unshare-ipc --unshare-uts \
  --proc /proc --dev /dev --tmpfs /tmp \
  --ro-bind /usr /usr --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib /lib64 \
  --ro-bind /etc /etc \
  --dir /home --bind "$fixture_home" /home/laddy \
  --dir /fixture --ro-bind "$app" /fixture/taarof-app --ro-bind "$agent" /fixture/agent \
  --ro-bind "$(dirname -- "$0")/session_continuity_demo.py" /fixture/session_continuity_demo.py \
  --bind "$evidence" /evidence \
  --setenv HOME /home/laddy --setenv PATH /usr/bin:/bin --setenv LANG C.UTF-8 \
  --setenv __EGL_VENDOR_LIBRARY_FILENAMES /usr/share/glvnd/egl_vendor.d/50_mesa.json \
  --setenv XDG_RUNTIME_DIR /tmp/runtime --unsetenv DISPLAY \
  /bin/sh -c 'mkdir -p /tmp/runtime /tmp/.X11-unix; chmod 700 /tmp/runtime; chmod 1777 /tmp/.X11-unix; exec xvfb-run -a --server-args="-screen 0 1280x900x24 -nolisten tcp" dbus-run-session -- /usr/bin/python3 /fixture/session_continuity_demo.py'
