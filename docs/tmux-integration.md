# taarof tmux integration

`taarof` can run panes in two different ways:

- directly, by spawning your shell or command in VTE
- indirectly, by attaching that pane to a tmux session

The tmux path is useful when you want a pane to survive window closes, restart
cleanly on restore, or run on a remote host while `taarof` keeps the GTK tab and
split layout.

## Mental Model

The important detail is that `taarof` does not mirror tmux's own pane tree.

- `taarof` owns the GUI: workspaces, tabs, splits, focus, sidebar, and restore
- tmux owns long-lived process state for tmux-backed panes
- one tmux-backed `taarof` pane maps to one tmux session

That means a split inside a tmux-backed tab creates another tmux session for the
new `taarof` pane. It does not run `tmux split-window` inside a shared tmux
session.

## When tmux Is Used

There are three ways a pane ends up tmux-backed:

1. A pane is created explicitly through the sidebar `+ tmux tab` button, the
   command palette, the `Ctrl+Shift+T` shortcut, or the socket API with
   `create-tmux-tab`.
2. A saved session is restored and that pane already has tmux metadata.
3. A split is created from an already tmux-backed pane, or inside a tmux-backed
   workspace, while `[tmux].enabled = true` or the workspace's tmux mode is on.

The most direct user-facing entrypoints are now the sidebar `+ tmux tab`
button, the command palette entry, the `Ctrl+Shift+T` shortcut, and the socket
API. Workspace tmux mode is also exposed in the UI so users can opt a workspace
into tmux-backed new tabs and split inheritance.

If local tmux is unavailable and there are no remote candidates at all — no
`~/.ssh/config` aliases and no `[hosts.<name>]` entries — the GUI entrypoint
stays disabled and explains how to enable tmux-backed tabs.

`[tmux].enabled` matters for the automatic workspace and split inheritance path.
The explicit sidebar, palette, and `create-tmux-tab` socket entrypoints are all
intentional tmux requests, so they remain valid even when automatic tmux
inheritance is disabled.

## Config

`taarof` reads tmux settings from `~/.config/taarof/config.toml`.
Those settings are part of the same installed config snapshot as the Ghostty
theme import, so tmux-related actions do not reread disk mid-session.

Example:

```toml
[tmux]
enabled = true
session_prefix = "taarof"
close_behavior = "detach"

[hosts.devbox]
address = "you@devbox"
max_sessions = 12
warn_cpu_percent = 70
warn_memory_percent = 75
idle_detach_minutes = 60
tmux_backed = true
```

Relevant keys:

- `enabled`: turns on the automatic tmux path for tmux-backed workspaces and
  split inheritance
- `session_prefix`: prefix used in generated tmux session names
- `close_behavior`:
  - `close` is the default and kills the tmux session when the pane closes
  - `detach` leaves the tmux session running when the pane closes
- `session_style`:
  - `inherit` is the default and sets no tmux options at all
  - `plain` sets `status off` and `mouse on` on the sessions taarof creates
    (see [Plain session style](#plain-session-style))

## Where remote targets come from

`~/.ssh/config` is the canonical source of SSH connection candidates. Every
concrete `Host` alias in it — including aliases pulled in through `Include`
directives — is offered by the sidebar `+ tmux tab` dialog, by the palette's
`Connect: <host>` entries, and accepted as the `host` field of a
`create-tmux-tab` socket request. Wildcard patterns (`Host *`, `Host web?`),
negated patterns (`Host !secret`), and `Match` blocks describe sets rather than
destinations, so they are skipped.

Interpretation stops at the alias. The connection argv is `ssh <alias>`, so ssh
itself resolves `HostName`, `User`, `Port`, and `ProxyJump` — Taarof never
reinterprets them.

`[hosts.<name>]` entries in `config.toml` are **optional tuning** layered on top:
they carry `max_sessions`, `idle_detach_minutes`, the warning thresholds, and
`tmux_backed`. A `[hosts.<name>]` entry wins on name collision with an
`~/.ssh/config` alias, and its `address` may point somewhere other than the
label. You do not need a `[hosts.<name>]` entry to connect to a host.

The dialog also accepts a free-text `user@host` target for a one-off destination
that is in neither source. Taarof does not write it back to `config.toml`.

The `host` you pass over the socket API refers to an `~/.ssh/config` alias or a
`[hosts.<name>]` label, not directly to an SSH command. An unknown name is
rejected with `unknown host: <name>`.

Taarof validates and reloads `config.toml` changes live. Added, removed, or
edited `[hosts.<name>]` entries apply to new tmux tabs, socket requests, and
subsequent probes or reconnects. Existing attached panes keep their current SSH
target until reconnect so an edit cannot silently disrupt active work.

## Recommended tmux-backed Workspace Preset

If you want a workspace whose tabs and splits are tmux-backed by default and
survive closing, use this preset. It sets `detach` so an accidental close never
kills a long-running job, and pins a remote host as tmux-backed so new tabs and
splits on it inherit tmux.

```toml
[tmux]
enabled = true            # turn on automatic tmux inheritance
session_prefix = "taarof"
close_behavior = "detach" # closing a pane leaves the session running

[hosts.devbox]
address = "you@devbox"    # ssh target; omit for a local-only host entry
max_sessions = 12
idle_detach_minutes = 60
tmux_backed = true        # new tabs/splits on devbox inherit tmux backing
```

Notes:

- With `close_behavior = "detach"`, closing a tmux-backed pane/tab leaves the
  session running and taarof shows a toast confirming it survives. Reattach it
  later from the command palette or the dashboard.
- With the default `close_behavior = "close"`, closing a tmux-backed pane/tab
  first asks **Kill / Detach / Cancel** (see the section below) so you never
  destroy a session by reflex.
- `tmux_backed = true` on a host only matters while `[tmux].enabled = true`; the
  toggle in the workspace context menu explains this at the moment you flip it.

## Plain Session Style

A tmux-backed taarof pane is a tmux client, so by default it shows tmux's status
bar and inherits tmux's stock defaults. taarof owns the tab strip already, so
that status bar is usually a second, redundant one.

`[tmux].session_style` decides whether taarof touches any tmux option:

```toml
[tmux]
session_style = "plain"   # default: "inherit"
```

- `inherit` (default): taarof sets **no** tmux options. The generated command is
  exactly `tmux new-session -As <name> [-c <cwd>]`, byte for byte what taarof has
  always run.
- `plain`: taarof appends session-scoped `set-option` commands to that same
  command, so one child does create-and-style:

  ```
  tmux new-session -As <name> -c <cwd> \; \
    set-option -t <name> status off \; \
    set-option -t <name> mouse on
  ```

What `plain` deliberately does not do:

- It never uses `-g`. Every option is scoped to the one session with
  `-t <session>`, so your global tmux options are untouched.
- It never writes to, reads, or reloads `~/.tmux.conf`.
- It only applies to sessions taarof itself creates (the
  `{prefix}--{workspace}--t{tab}--{pane}` sessions). Attaching to a session you
  or another tool created — from the dashboard, the palette's reattach entry, or
  a foreign session name — never sets an option.

The same argv is used for remote hosts: the `;` separators are quoted for the
remote shell, so the remote `tmux` receives them as command separators rather
than the SSH login shell treating them as shell operators. Nothing has to be
installed or configured on the remote host for `plain` to work.

`plain` needs tmux 2.1 or newer, where `mouse` became a single option — that is
the tmux running the session, so for a remote pane it is the remote host's tmux
version. On anything older, keep `inherit`.

### The tmux.conf Recipe

Some settings cannot be applied by taarof after the fact and must live in
`~/.tmux.conf` (on **each** host that runs tmux — the remote host's config is
what governs a remote pane, not your laptop's):

```tmux
# Scrollback. This one MUST live here: history-limit is read when a pane is
# created, and the first pane of a session is created by the same command that
# creates the session — so no option set afterwards can retroactively grow it.
set -g history-limit 100000

# Mouse scroll/select/resize. taarof's session_style = "plain" also sets this
# per session; keeping it here makes it true for tmux sessions taarof did not
# create.
set -g mouse on

# New windows and splits open in the current pane's directory rather than the
# directory the session started in.
bind c new-window -c "#{pane_current_path}"
bind '"' split-window -c "#{pane_current_path}"
bind % split-window -h -c "#{pane_current_path}"

# Do not swallow the Escape key; without this, Esc-heavy TUIs (vim, and most
# agent CLIs) feel laggy or drop the keypress.
set -sg escape-time 0

# Colour and italics support that matches what taarof's VTE panes advertise.
set -g default-terminal "tmux-256color"

# Let applications inside tmux see focus in/out, which keeps editors and agent
# CLIs from redrawing stale state when you switch taarof panes.
set -g focus-events on
```

Which parts must live in `tmux.conf`, and why:

| Setting | Must be in tmux.conf? | Why |
| --- | --- | --- |
| `history-limit` | **Yes** | Read at pane creation. taarof's first pane is created by the very command that creates the session, so a later `set-option` only affects panes created after it — it cannot grow the scrollback you already have. This is why taarof does not set it for you. |
| `escape-time` | **Yes** | A server option (`-s`), not session-scoped; taarof sets nothing globally by design. |
| `default-terminal` | **Yes** | Consulted when the session's server/pane starts, before a session-scoped option could apply. |
| `focus-events` | Recommended | Session-scoped, but wanted for every session, not just taarof's. |
| `mouse` | Optional | taarof's `plain` style already sets it per session; put it here to get it in your own tmux sessions too. |
| `status off` | Optional | Only wanted inside taarof, where the tab strip already shows the same information. Setting it globally hides your status bar in a normal terminal too — which is exactly why taarof scopes it to its own sessions instead. |
| `-c "#{pane_current_path}"` binds | **Yes** | They are key bindings, not options; taarof never rebinds keys inside your tmux. |

For a remote host, put the same file on that host. A remote taarof pane runs
`ssh <target> tmux …`, so the remote tmux server reads the remote
`~/.tmux.conf`; the local one is irrelevant to it.

## Close vs Detach in the UI

taarof makes the consequence of closing a tmux-backed pane or tab explicit at
the moment you act, so the destructive vs safe distinction is never a surprise.

- **`close_behavior = "close"` (default, destructive):** closing a tmux-backed
  tab opens a modal dialog stating, for example, *"This kills tmux session
  `<name>` on `<host>`."* with three choices:
  - **Kill** — runs `tmux kill-session`; the session and its processes are gone.
  - **Detach** — leaves the session running in the background and closes the tab;
    taarof registers it so you can reattach later. This is the non-destructive
    escape hatch.
  - **Cancel** — nothing happens.
- **`close_behavior = "detach"` (safe):** no dialog. The tab closes immediately
  and taarof shows a toast: *"Detached tmux session `<name>`; it keeps running in
  the background."*

The confirmation only applies to user-initiated closes (the tab close button and
the tab context menu). Programmatic closes (socket API, session teardown) honor
the configured `close_behavior` directly without prompting.

## Workspace tmux Mode and Inheritance

Enabling **tmux-backed workspace** (workspace context menu) means new tabs and
new splits created in that workspace are tmux-backed automatically, as long as
`[tmux].enabled = true`. When you flip the toggle, taarof shows a one-line toast
explaining the inheritance so the mode is never silently on:

- enabling: *"tmux-backed workspace: new tabs and splits here run inside tmux
  sessions that survive detach"*
- disabling: *"tmux backing off: new tabs and splits here run as plain shells"*

Existing tmux-backed panes keep their own sessions regardless of the toggle.

## Nested tmux and SSH Warnings

Running a tmux-backed pane *inside* another tmux, or over SSH into a shell that
is itself already in tmux, produces a confusing nested experience: your prefix
key only reaches the outer tmux, and status lines stack. taarof warns about this
in two places:

- **On create:** if taarof itself was launched from inside tmux (`$TMUX` was set
  at startup), or the new tmux tab targets a remote host over SSH, a toast warns
  that the session may nest and suggests prefixing keys twice or detaching the
  outer session.
- **On the tab row:** a nested tmux-backed pane's tmux label shows a `⚠ nested`
  marker with a tooltip explaining the situation.

The warning is advisory only — nesting still works, and sometimes it is exactly
what you want.

## Session Naming

Generated session names use this format:

```text
{prefix}--{workspace}--t{tab_id}--{pane_id}
```

Example:

```text
taarof--default--t7--0
```

`taarof` sanitizes dots, colons, and spaces to `_`, and truncates long names to
128 characters.

The naming scheme is intentionally based on workspace, tab ID, and pane ID
rather than the visible tab title, so renaming a tab does not rename the tmux
session behind it.

## Local vs Remote

Local tmux-backed panes run tmux directly:

```bash
tmux new-session -As <session-name> [-c <cwd>]
```

Remote tmux-backed panes run the same command through SSH:

```bash
ssh -t <configured-host> tmux new-session -As <session-name> [-c <cwd>]
```

For background operations such as polling metadata or killing a session, `taarof`
uses non-interactive SSH with `BatchMode=yes` and a short connect timeout so the
UI does not hang waiting for a password prompt.

## What Happens on Create, Split, Close, and Restore

### Create

When `taarof` creates a tmux-backed pane, it launches `tmux new-session -As`.
That means:

- if the session already exists, `taarof` attaches to it
- if the session does not exist, tmux creates it first

### Split

Splitting a tmux-backed pane creates a new `taarof` pane and a new tmux session
for that pane. If the original pane was attached to a remote host and the
workspace does not already pin a host, the split inherits that remote target.

### Close

On close, behavior depends on `close_behavior`:

- `close`: `taarof` runs `tmux kill-session -t <name>`
- `detach`: `taarof` just drops the terminal client and leaves the tmux session
  alive

One subtlety: explicit detaching and plain close are not the same thing.

- `detach-pane` records the session in `taarof`'s detached/background state so it
  shows up cleanly for reattach flows
- closing a pane with `close_behavior = "detach"` leaves tmux alive, but it does
  not register that session as an explicit detached entry first

### Restore

Saved panes keep the tmux session name and host target in
`~/.local/share/taarof/session.json`.

On restart, `taarof` restores the pane tree, then reuses the same
`tmux new-session -As` flow to reconnect to each saved session. In practice,
that means restore is tolerant of both cases:

- the tmux session is still alive and gets reattached
- the tmux session is gone and gets recreated

## Detach, Reattach, and Dashboard

The tmux-specific management flow is built around four socket actions:

- `detach-pane`
- `attach-session`
- `list-detached`
- `open-dashboard`

For inspection and automation, there is also `dashboard-state`.

`detach-pane` closes the VTE pane without killing the tmux session and stores a
detached-session record in app state.

`attach-session` opens a new tab that runs:

```bash
tmux attach-session -t <session-name>
```

That reattach flow is meant to get you back into the session quickly. It is not
the same as recreating the original managed pane tree, so you should think of it
as "open a client on that tmux session again" rather than "rebuild the exact
original `taarof` pane object."

Detached-session identity is `(host/target, session_name)`, not just
`session_name`. If two detached sessions share the same name on different
hosts, `attach-session` intentionally rejects a bare `session_name` request as
ambiguous. Use `list-detached` first and pass `ssh_target` (preferred for
automation) or `host` to select the exact session to reattach.

The dashboard aggregates tmux sessions across local and configured remote hosts,
plus the detached sessions that `taarof` is explicitly tracking.

## Socket Examples

The active socket path is published in the runtime registry file:

- default session: `$XDG_RUNTIME_DIR/taarof-current.json`
- named session: `$XDG_RUNTIME_DIR/taarof-current-<session>.json`

If `XDG_RUNTIME_DIR` is unset, taarof falls back to `/run/user/$(id -u)`. If
`XDG_RUNTIME_DIR` is set but fails ownership/permission validation, taarof
disables the socket server instead of falling back.

Example helper:

```bash
TAAROF_REG="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/taarof-current.json"
TAAROF_SOCK="$(jq -r '.socket_path' "$TAAROF_REG")"
```

Create a local tmux-backed tab:

```bash
echo '{"action":"create-tmux-tab","name":"dev","working_dir":"/tmp/user/project"}' \
  | socat - UNIX-CONNECT:"$TAAROF_SOCK"
```

Create a remote tmux-backed tab on a configured host:

```bash
echo '{"action":"create-tmux-tab","name":"api","host":"devbox","working_dir":"/srv/api"}' \
  | socat - UNIX-CONNECT:"$TAAROF_SOCK"
```

Detach a pane:

```bash
echo '{"action":"detach-pane","tab":"dev","pane":0}' \
  | socat - UNIX-CONNECT:"$TAAROF_SOCK"
```

Open the dashboard:

```bash
echo '{"action":"open-dashboard"}' | socat - UNIX-CONNECT:"$TAAROF_SOCK"
```

Reattach a detached session:

```bash
echo '{"action":"attach-session","session_name":"taarof--default--t7--0"}' \
  | socat - UNIX-CONNECT:"$TAAROF_SOCK"
```

List detached sessions to discover remote identity fields:

```bash
echo '{"action":"list-detached"}' | socat - UNIX-CONNECT:"$TAAROF_SOCK"
```

Disambiguate a remote same-name session with `ssh_target`:

```bash
echo '{"action":"attach-session","session_name":"taarof--default--t7--0","ssh_target":"builder@ci-box"}' \
  | socat - UNIX-CONNECT:"$TAAROF_SOCK"
```

If you are working with multi-pane tabs, use `list-tabs` first to discover pane
IDs.

## User-Facing Gotchas

- A `taarof` split is not a tmux split. Each tmux-backed pane gets its own tmux
  session.
- Renaming a tab does not rename its tmux session.
- `close_behavior = "detach"` keeps the tmux session alive, but explicit
  `detach-pane` is the cleaner path if you want `taarof` to track that session in
  its detached/background UI.
- Remote tmux panes depend on a configured host entry and working SSH auth.
- You can create tmux-backed tabs directly from the sidebar button, the command
  palette, the `Ctrl+Shift+T` shortcut, or the socket API.
- Workspace tmux mode affects future plain tabs and split inheritance. Existing
  tmux-backed panes keep their own tmux sessions either way.
