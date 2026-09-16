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
`kmux` is GitHub's default branch, so clones and new pull requests start there.
`main` carries only promoted work.

## Gates

Run `mise run ci` for the full contributor gate. Clippy treats warnings as
errors. Never `docker compose run` for builds or tests: the image can write
root-owned files into a bind-mounted checkout. Use an unprivileged container
user and an isolated cargo target volume for container validation.

See `README.md`, `docs/configuration.md`, `docs/setup.md`, `RUNBOOK.md`, and
`TROUBLESHOOTING.md` for product and operator documentation.

<!-- graft:start -->
## Graft — repo context graph

This repo is indexed in `graft/`: small linked markdown nodes that explain each
system and carry exact file:line spans, kept in sync with the code through git.

For ANY task here — understanding how something works, finding where code lives,
or scoping a change — get context from the graph before grepping or opening
source files. Re-ask freely (it's cheap) and reuse literal identifiers you
already have (symbol, error string, file name) as the query. New to this repo?
Run `graft map` first — a token-budgeted orientation (dir clusters, hubs,
hotspots), no LLM, no key.

- Run `graft ask "<your question>" --source` → ranked nodes with the relevant
  code spans inlined (each hit's ≤8-line crux by default; `--full` for whole
  definitions when the crux isn't enough). Match the tool to the task shape:
  for understanding or editing, the top node IS the answer — cite its
  `covers:` file:line spans and edit straight from `--source`. For
  exhaustive tasks ("every occurrence / every caller of this pattern"), ranked
  results are top-N, not complete — run `graft grep "<literal>"` instead
  (exhaustive over indexed files, grouped by enclosing symbol), falling back
  to raw `grep -rn` only for unindexed files.
- `graft skeleton <file>` → every definition's signature + span, ~10× cheaper
  than reading the file; use it to skim an API surface.
- `graft callers <symbol>` gives precomputed, exact edges — who calls this.
  Add `--direction out` for what it calls, or `--depth N` to walk
  transitively for the full blast radius. For structural questions, skip
  ranking and use this directly.
- Or browse: `graft/INDEX.md` lists every node; follow the links.
- Monorepos and folders of multiple repos rank fairly across sub-projects —
  hits carry `[scope/]` labels naming which one they're from. Narrow with
  `graft ask "<task>" --in <scope>/` once you know where you're working.

If a returned span is truncated ("+N more lines"), open the file at that exact
range before finalizing. Only open source files when a node genuinely lacks a
needed detail, and then at the exact file:line the node points to — never
re-read whole files.

After big code changes, refresh the graph with `graft build` (deterministic,
no API key, $0).
<!-- graft:end -->
