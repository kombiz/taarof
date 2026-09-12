# CLAUDE.md

Agent guide for `taarof`, a Linux-first terminal workspace. The shipped product
is `taarof-app/`, a Rust GTK4/libadwaita/VTE desktop app with tabs, split panes,
session restore, agent activity indicators, optional tmux-backed panes, a Unix
socket control API, and an optional loopback HTTP API plus web client.

`taarof-app/` is the source of truth for shipped behavior. On your workstation,
verify version claims against the live process instead of a binary merely found
on disk.

## Security and architecture boundaries

- The Unix socket is a privileged same-user control surface. Preserve runtime
  directory ownership and private permissions.
- The HTTP API is bearer-token observation by default. Mutating requests require
  `[http_control].enabled = true` and an actual loopback bind. Do not widen the
  bind or proxy the raw token or socket.
- UI surfaces activate typed `Action` values, not raw `win.*` or `term.*`
  strings.
- HTTP control actions translate to the matching socket message and reuse the
  socket handler; HTTP is not a second control implementation.
- Child processes must pass through the child-environment sanitizer and
  unreaped children must use the process-reaping seam.
- Keep GTK state serialized on the main thread and spawn expensive work behind
  a channel.

## Branch workflow

Create task branches from `origin/kmux` in dedicated worktrees and target pull
requests to `kmux`.
Promote reviewed work from `kmux` to `main` only with explicit authorization.
GitHub's default branch remains `main`.

## Gates

Run `mise run ci` for the full contributor gate. Clippy treats warnings as
errors. Never `docker compose run` for builds or tests: the image can write
root-owned files into a bind-mounted checkout. Use an unprivileged container
user and an isolated cargo target volume for container validation.

See `README.md`, `docs/configuration.md`, `docs/setup.md`, `RUNBOOK.md`, and
`TROUBLESHOOTING.md` for product and operator documentation.
