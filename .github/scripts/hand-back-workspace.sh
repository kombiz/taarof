#!/usr/bin/env bash
# Hand the workspace back to the runner user at the end of a root container
# job (#101). Run it as the last step, with `if: always()`.
#
# Killing a step's `docker exec` client does not stop the processes it started
# inside the container. After a cancel, cargo and rustc keep writing, and files
# they create after the chown stay root-owned. So stop every process first
# (PID 1 is the runner's keep-alive and is never signalled), then chown.
set -euo pipefail

workspace="${1:-${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is not set}}"

# `kill -1` as root on a host would signal the whole machine.
if [[ ! -f /.dockerenv && ! -f /run/.containerenv ]]; then
  echo "::error::hand-back-workspace.sh only runs inside a job container." >&2
  exit 1
fi

# The runner creates the workspace's parent, so it carries the runner's uid.
owner="$(stat -c '%u:%g' "$workspace/..")"

# Sets `others` to the number of live processes other than PID 1 and this
# shell. Uses builtins only, so the count never includes a child of its own.
count_others() {
  local dir pid line state
  others=0
  for dir in /proc/[0-9]*; do
    pid="${dir#/proc/}"
    [[ "$pid" == 1 || "$pid" == "$$" ]] && continue
    read -r line 2>/dev/null <"$dir/stat" || continue
    state="${line##*) }"
    state="${state%% *}"
    # PID 1 does not reap, so killed orphans stay as zombies. They cannot write.
    [[ "$state" == Z ]] && continue
    others=$((others + 1))
  done
}

count_others
if ((others > 0)); then
  echo "Stopping ${others} leftover process(es) before the ownership hand-back."
  kill -TERM -1 2>/dev/null || true
  for ((i = 0; i < 20; i++)); do
    count_others
    ((others == 0)) && break
    sleep 0.5
  done
  if ((others > 0)); then
    echo "::warning::${others} process(es) ignored SIGTERM; sending SIGKILL."
    kill -KILL -1 2>/dev/null || true
    sleep 1
  fi
fi

# -h changes symlinks themselves and never follows them out of the workspace.
chown -R -h "$owner" "$workspace"

left="$(find "$workspace" ! -user "${owner%%:*}" -print -quit)"
if [[ -n "$left" ]]; then
  echo "::error::Files not owned by ${owner} remain after the hand-back, for example: ${left}" >&2
  exit 1
fi
echo "Workspace handed back to ${owner}."
