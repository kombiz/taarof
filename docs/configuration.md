# Taarof configuration reference

This reference describes every user-authored configuration surface in the
shipped desktop app and optional control gateway.

## File inventory

| File | Scope | Format | Reload behavior |
| --- | --- | --- | --- |
| `~/.config/taarof/config.toml` | App behavior, remote hosts, local APIs | TOML | Validated live reload for supported consumers; startup-owned services require a new process, except the installed update path is read by the already-installed watcher on each refresh |
| `~/.config/taarof/keybindings.toml` | Shortcuts and chords | TOML | Restart Taarof |
| `~/.config/ghostty/config` | Imported terminal font, colors, cursor, shell | Ghostty flat config | Restart Taarof |
| `<repo>/.taarof.config` | Per-project agent, mise, and dashboard hints | TOML | Re-read when project context is resolved |
| `~/.config/taarof/agent-signatures.json` | Global agent process signature override | JSON | Restart Taarof |
| `~/.config/taarof/gateway.toml` | Optional remote-control gateway | TOML | Restart the gateway |
| `~/.config/taarof/templates.json` | UI-managed saved templates | JSON | Read on use |
| `~/.config/taarof/views.json` | UI-managed saved dashboard views | JSON | Read on use |
| `~/.config/taarof/projects.json` | Registered repository launch targets | JSON | Read on use |

Session files and files under `$XDG_RUNTIME_DIR` are state or secrets, not
configuration. Do not hand-edit runtime registries, sockets, or bearer tokens.

`config.toml` is watched by its parent directory. After a short debounce,
Taarof reads and validates one complete replacement snapshot off the GTK
thread; invalid or partially written files leave the last known-good snapshot
in place. Appearance, sidebar layout/icons/compact mode, dock visibility, and
future configuration lookups such as tmux host resolution update from that
snapshot. Existing attached panes retain their current target until reconnect.

Some consumers are intentionally startup-owned: the HTTP listener and
`[http_control]` gate, history writer/storage, automatic `[tasks] enabled`
discovery, and settings read from `keybindings.toml`, `agent-signatures.json`,
or direct Ghostty edits. Restart Taarof after changing those surfaces. The
update watcher itself is installed during startup, but it reads
`[update].installed_binary_path` from the current live snapshot on every
refresh, so changing that path applies to the next refresh without restarting.
**Reload into update** also starts a new process, but is an operator-controlled
executable-update action rather than a config-reload button.

## Discover, generate, and validate

```bash
taarof config path
taarof config default             # print the generated starter file
taarof config default --write     # create it without overwriting
taarof config validate
```

Use `--force` with `config default --write` only after backing up the existing
file. The validator checks both `config.toml` and `keybindings.toml` with the
same parsers used by the app.

## `config.toml`

All sections and keys are optional. Missing values use the defaults below.
Underscore names are canonical; the parser accepts a few documented hyphenated
aliases, but generated files always use underscores.

### Fully annotated example

```toml
# App chrome. Every color must be exactly six hexadecimal RGB digits.
[appearance]
base = "#14110c"           # Main app background.
mantle = "#100d09"         # Sidebar/deeper background.
crust = "#0b0906"          # Darkest background layer.
surface = "#241d13"        # Cards, rows, and ordinary raised surfaces.
surface_raised = "#34291a" # More prominent raised surface.
border = "#463726"         # Borders and separators.
text = "#efe7d6"           # Primary body text.
subtext = "#c3b9a7"        # Secondary body text.
muted = "#7c715e"          # Disabled and low-emphasis text.
title = "#f6f0e4"          # High-emphasis titles.
soft_text = "#d8cdb6"      # Softer high-legibility text.
warm_muted = "#a08d6f"     # Warm secondary status text.
accent = "#e0a43a"         # Selection and primary accent.
accent_hover = "#f0be5e"   # Accent hover state.
running = "#7bbf7a"        # Running agent state.
activity = "#6bb0b8"       # Recent activity state.
waiting = "#d9b34a"        # Waiting-for-input state.
ports = "#e0913a"          # Listening-port indicator.
error = "#d6796b"          # Error state.

# Sidebar placement, optional sidebar-only colors, and workspace icons.
[sidebar]
position = "left"          # "left" or "right"; default "left".
compact = false            # true starts with the 64 px icon-and-status rail.
background = "#100d09"     # Hex or CSS named color; falls back to mantle.
surface = "#241d13"        # Hex or CSS named color; falls back to surface.
accent = "#e0a43a"         # Hex or CSS named color; falls back to accent.
text = "#efe7d6"           # Hex or CSS named color; falls back to text.
muted = "#7c715e"          # Hex or CSS named color; falls back to muted.

[sidebar.workspace-icons]
default = "•"              # Fallback icon for every workspace.
worktree = "⑂"             # Fallback specifically for git worktrees.

[sidebar.workspace-icons.by-name]
infra = "🛠"               # Exact, case-insensitive workspace-name match.

[sidebar.workspace-icons.by-repo]
taarof = "⌘"               # Exact repo-directory basename match.

# Terminal actions owned by Taarof, not terminal rendering.
[terminal]
copy_recent_lines = 200     # Rows copied by Copy Recent Output; 1..10000.
clipboard_history_size = 50 # In-memory clipboard entries; 1..1000.

# Ctrl+Click handling for path:line[:column] terminal references.
[editor]
open_command = "zed {path}:{line}:{col}" # Executable template; no shell.
click_action = "peek"       # "peek" for in-app viewer or "editor".

# External URL launcher. Omit the override to use the system default browser.
[browser]
# open_command = "helium-browser {url}" # Whitespace-split argv; no shell.

# Project task discovery from mise configuration.
[mise]
include_global = false      # Also include ~/.config/mise/config.toml tasks.

# Automatic tmux inheritance and close behavior.
[tmux]
enabled = false             # Automatic workspace/split inheritance.
session_prefix = "taarof"  # Prefix for generated tmux session names.
close_behavior = "close"   # "close" kills after confirmation; "detach" survives.
session_style = "inherit"  # "plain" sets status off + mouse on, session-scoped.

# Optional executable used to detect an installed update.
[update]
# installed_binary_path = "/opt/taarof/bin/taarof-app"

# Optional loopback browser/API server.
[http]
enabled = false
port = 7800                 # Unsigned 16-bit TCP port; 7800 recommended.
bind_address = "127.0.0.1" # Keep on IPv4/IPv6 loopback.
unsafe_allow_non_loopback = false

# Optional sanitized observational history.
[history]
enabled = false
max_age_days = 30
max_records = 200000
max_bytes = 268435456       # 256 MiB soft cap.
maintenance_interval_minutes = 15
queue_capacity = 4096
record_events = true
record_diagnostics = true
record_work = true

# Optional write routes for the local browser.
[http_control]
enabled = false             # Effective only on a loopback HTTP bind.

# Optional right-hand Session/Agents/Tasks dock.
[dock]
visible = false

# Automatic .plan task panel and optional GitHub PR view.
[tasks]
enabled = false
pull_requests = false       # Requires an authenticated gh CLI for local repos.
default_view = "tasks"      # "tasks" or canonical "pull_requests".

# Restored non-tmux agent panes default to an opt-in resume action.
[session]
auto_resume_agents = false  # true relaunches the saved agent session immediately.

# External task/PR agent runner integration.
[loop_runner]
enabled = false
dev_command = "loop-runner dev-loop --repo {repo} --issue {issue}"
pr_command = "loop-runner pr-loop --repo {repo} --pr {pr} --loop {loop}"
review_loop = "pr-review-readonly"
merge_loop = "pr-review"

# Optional tuning only — connection candidates come from ~/.ssh/config.
# Repeat [hosts.<label>] for each target that needs the policy metadata below.
[hosts.control]
address = "build.example"     # Exact ssh destination or ~/.ssh/config alias.
max_sessions = 8           # Unsigned session-budget metadata.
warn_cpu_percent = 80      # Unsigned CPU warning threshold; use 0..100.
warn_memory_percent = 80   # Unsigned memory threshold; use 0..100.
idle_detach_minutes = 120  # Unsigned idle-policy metadata in minutes.
tmux_backed = true         # Host participates in tmux-backed inheritance.
```

### `[appearance]`

All values are strings containing `#RRGGBB`. A leading `#` is required by the
documented format. Omitted roles keep the built-in Majlis value shown in the
example. These colors style the GTK application; terminal-pane colors still
come from the Ghostty import.

The aliases `surface-raised`, `soft-text`, `warm-muted`, and `accent-hover` are
accepted for compatibility. Prefer the underscore names.

### `[sidebar]` and workspace icons

`position` accepts `left` or `right` case-insensitively. Invalid values fall
back to `left`. `compact` defaults to `false`; when true, names and secondary
metadata collapse into tooltips while workspace icons, tab numbers, and agent
status dots remain mouse and keyboard navigable. **Toggle Compact Sidebar** in
the command palette (keybindable as `toggle-sidebar-compact`) changes the mode
for the current run; live config reload reapplies `sidebar.compact`. Sidebar colors accept `#RGB`, `#RRGGBB`, `#RRGGBBAA`, or a
single CSS named-color word. They override `[appearance]` only in the sidebar.

Icon values are any non-empty string, typically one Unicode glyph or emoji.
Resolution order is:

1. `by-name` exact case-insensitive workspace name;
2. `by-repo` exact case-insensitive repository basename;
3. `worktree` for a git worktree;
4. `default`;
5. Taarof's derived initial.

### `[terminal]`

| Key | Type | Default | Accepted values | Effect |
| --- | --- | --- | --- | --- |
| `copy_recent_lines` | integer | `200` | `1..10000` | Maximum rows copied by the recent-output action. Invalid values fall back to 200. |
| `clipboard_history_size` | integer | `50` | `1..1000` | Maximum in-memory clipboard entries. Invalid values fall back to 50. |

Clipboard history is process memory, not a persisted secret store.

### `[editor]`

| Key | Type | Default | Accepted values | Effect |
| --- | --- | --- | --- | --- |
| `open_command` | string | `zed {path}:{line}:{col}` | Non-empty executable/argument template | Opens a local file in the external editor. |
| `click_action` | string | `peek` | `peek`, `editor` | Chooses the in-app read-only overlay or external editor. Unknown values become `peek`. |

Supported placeholders are `{path}`, `{line}`, and `{col}`. Taarof substitutes
them into individual arguments without passing the command through a shell.
Relative paths resolve against the pane's current OSC 7 directory. Remote-pane
paths are not sent to a local editor.

### `[browser]`

| Key | Type | Default | Accepted values | Effect |
| --- | --- | --- | --- | --- |
| `open_command` | string | absent | Non-empty whitespace-delimited command template | Opens terminal and task-panel URLs with the selected browser instead of the system GIO handler. |

Use `{url}` where the URL belongs. If the placeholder is absent, Taarof appends
the URL as the final argument. Taarof splits the template on whitespace and
launches it directly without a shell, so shell quoting and operators are not
supported. A blank value behaves like an omitted value and uses the system
default browser.

### `[mise]`

`include_global` is a Boolean. `false` discovers project and parent-directory
mise tasks only; `true` also includes the user's global mise config.

### `[tmux]`

| Key | Type | Default | Accepted values | Effect |
| --- | --- | --- | --- | --- |
| `enabled` | Boolean | `false` | `true`, `false` | Enables automatic backing for opted-in workspaces and inherited splits. Explicit “new tmux tab” remains available when tmux exists. |
| `session_prefix` | string | `taarof` | Any string | Prefix for `{prefix}--{workspace}--t{tab}--{pane}`; unsafe characters are sanitized and names are bounded. |
| `close_behavior` | string | `close` | `close`, `detach` | Kill the backing session after confirmation, or leave it running. |
| `session_style` | string | `inherit` | `inherit`, `plain` | `inherit` sets no tmux options at all. `plain` applies `status off` and `mouse on` to the sessions taarof creates, scoped to those sessions (`set-option -t`, never `-g`), so global options, `~/.tmux.conf`, and sessions taarof did not create are untouched. |

For work that must survive window closes, use `detach`.

`session_style` cannot set `history-limit`: tmux reads it when a pane is
created, and taarof's first pane is created by the same command that creates the
session. Scrollback depth therefore belongs in `~/.tmux.conf` on the host
running tmux — see the recipe in
[`tmux-integration.md`](tmux-integration.md#the-tmuxconf-recipe).

### `[update]`

| Key | Type | Default | Accepted values | Effect |
| --- | --- | --- | --- | --- |
| `installed_binary_path` | string | unset | Non-empty absolute path | Overrides the installed executable compared with `/proc/self/exe`. Invalid relative or empty values are ignored with a warning. |

When the override is unset, Taarof checks `TAAROF_INSTALLED_BINARY` (an
absolute development/test override), then `~/.local/bin/taarof-app`,
`/usr/local/bin/taarof-app`, and `/usr/bin/taarof-app` in that order. Symlinks
are canonicalized, while both the configured link and resolved target are
reported by `query-state`. The watcher is installed during startup, but each
refresh obtains this override from the current live `config.toml` snapshot;
changing `installed_binary_path` affects the next refresh without a restart.

### `[session]`

| Key | Type | Default | Accepted values | Effect |
| --- | --- | --- | --- | --- |
| `auto_resume_agents` | Boolean | `false` | `true`, `false` | When false, restored Claude, Codex, Pi, or Kimi panes start a shell and expose a resume action. When true, Taarof runs the saved agent resume command immediately. tmux-backed panes always reattach tmux and ignore this setting. |

### `[http]` and `[http_control]`

| Key | Type | Default | Accepted values | Effect |
| --- | --- | --- | --- | --- |
| `http.enabled` | Boolean | `false` | `true`, `false` | Starts the browser and HTTP API server. |
| `http.port` | integer | `7800` | `0..65535`; use an ordinary unprivileged port | TCP listen port. |
| `http.bind_address` | string | `127.0.0.1` | IP address string | Listen address. Loopback is the supported safe default. |
| `http.unsafe_allow_non_loopback` | Boolean | `false` | `true`, `false` | Explicitly bypasses the non-loopback refusal. Do not use for normal remote access. |
| `http_control.enabled` | Boolean | `false` | `true`, `false` | Enables authenticated write routes only while HTTP is loopback-bound. |

`/health` is unauthenticated. Other routes require a per-process bearer token
created under `$XDG_RUNTIME_DIR`. Never persist that token in a configuration
file. Use the gateway for remote access.

### `[history]`

`[history]` enables an optional SQLite store for allowlisted event, diagnostic,
and Work metadata. It is observational only: JSON remains canonical for
sessions, configuration, tasks, PRs, views, templates, and diagnostics.

| Key | Type | Default | Bounds | Effect |
| --- | --- | --- | --- | --- |
| `enabled` | Boolean | `false` | `true`, `false` | Starts the history writer and creates the session-aware database under the Taarof state directory. |
| `max_age_days` | integer | `30` | `1..=3650`; `0` is unbounded when another limit is set | Deletes records older than the configured age. |
| `max_records` | integer | `200000` | `1000..=10000000`; `0` is unbounded | Retains at most this many newest records. |
| `max_bytes` | integer | `268435456` (256 MiB) | 8 MiB..=64 GiB; `0` is unbounded | Soft limit for the main database plus WAL. Size pruning retains at least 1,000 rows (or the smaller count limit). |
| `maintenance_interval_minutes` | integer | `15` | `1..=1440` | Cadence for retention, WAL checkpointing, and incremental page reclamation. |
| `queue_capacity` | integer | `4096` | `64..=1048576` | Bounds the non-blocking producer queue. |
| `record_events` | Boolean | `true` | `true`, `false` | Records only explicitly allowlisted event types and scalar fields. |
| `record_diagnostics` | Boolean | `true` | `true`, `false` | Records scrubbed messages and category-specific scalar details. |
| `record_work` | Boolean | `true` | `true`, `false` | Records sanitized Work identity, authority, verification, task status, and bounded PR metadata. |

The database is `history.sqlite3` for the default session or
`history-<session-key>.sqlite3` for a named session. The database directory is
mode `0700`; the database and WAL sidecars are mode `0600`. Storage failures
degrade history without blocking GTK, pane I/O, or the live event ring.

When enabled, open the native History overlay with `Ctrl+Alt+H` (action key
`history-view`) or select **History** in the authenticated web client. Text
search is bounded and covers only sanitized summaries and subtypes; result
exports use the same sanitized record projection.

Maintenance runs only on the history writer thread, in bounded chunks, with
deterministic precedence: age, then record count, then the byte soft cap. It
checkpoints the WAL and incrementally reclaims free pages after deletes. A pass
that reaches its two-second budget reports `pending` and continues on a later
tick. Failures retain readable history where possible and retry with exponential
backoff capped at one hour.

History configuration fails loudly. Enabling history requires at least one
record class and at least one nonzero retention bound. TOML parse errors,
out-of-range values, and contradictory settings produce a configuration
diagnostic and a `misconfigured` history state; they are never silently clamped
or treated as a normally disabled history store.

### `[dock]`

| Key | Type | Default | Accepted values | Effect |
| --- | --- | --- | --- | --- |
| `visible` | Boolean | `false` | `true`, `false` | Shows the right-hand Session/Agents/Tasks dock at startup. |

Use the keybindable `toggle-dock` action or **Toggle Right Dock** in the command
palette to change visibility for the current run. The runtime toggle does not
rewrite `config.toml`; a live config reload reapplies `dock.visible`.

### `[tasks]`

| Key | Type | Default | Accepted values | Effect |
| --- | --- | --- | --- | --- |
| `enabled` | Boolean | `false` | `true`, `false` | Automatically discovers `.plan/tasks.json` and shows the persistent task surfaces. Manual Discover Tasks still works when false. |
| `pull_requests` | Boolean | `false` | `true`, `false` | Adds a GitHub PR view for local branches using `gh pr list`. |
| `default_view` | string | `tasks` | `tasks`, `pull_requests` | Initial persistent panel view. Compatibility aliases include `pull-requests` and `prs`. |

GitHub and Linear are mirrors in the private deployment setup; `.plan/tasks.json` remains
canonical task truth.

### `[loop_runner]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | Boolean | `false` | Shows runner actions when the active repo also has `.loop/loops.yaml`. |
| `dev_command` | string | `loop-runner dev-loop --repo {repo} --issue {issue}` | Task implementation command template. |
| `pr_command` | string | `loop-runner pr-loop --repo {repo} --pr {pr} --loop {loop}` | PR review command template. |
| `review_loop` | string | `pr-review-readonly` | Non-merging loop selected by Review only. |
| `merge_loop` | string | `pr-review` | Merge-capable loop selected by Review & merge. |

Blank command or loop strings fall back to defaults. Template placeholders are
`{repo}`, `{issue}`, `{pr}`, and `{loop}`. Configure the named review loop as
non-merging in the external runner; Taarof cannot infer that safety property.

### `[hosts.<label>]`

SSH connection candidates come from `~/.ssh/config`: every concrete `Host` alias
there (including ones reached through `Include`) is offered by the tmux-tab
dialog, the palette's `Connect: <host>` entries, and the `create-tmux-tab`
socket verb. Wildcard patterns, negated patterns, and `Match` blocks are
skipped, and the connection argv stays `ssh <alias>` so ssh resolves
`HostName`/`User`/`Port`/`ProxyJump` itself.

`[hosts.<label>]` is therefore **optional tuning**, not the host list. Add one
only to attach the policy metadata below to a destination, or to give a
destination a local name that differs from its SSH target. A `[hosts.<label>]`
entry wins on name collision with an `~/.ssh/config` alias. Taarof never writes
host entries back to `config.toml`.

One exception to "optional": remote agent-session discovery's host inventory is
the `[hosts.*]` entries that carry an `address`, plus hosts with a live taarof
tmux session — deliberately **not** `~/.ssh/config` aliases. A host whose agent
sessions should be discovered needs an entry here.

The label is a local Taarof identifier. Repeat the table with a unique label for
each remote.

| Key | Type | Default | Accepted values | Effect |
| --- | --- | --- | --- | --- |
| `address` | string | absent | Non-empty SSH target | Passed to `ssh`; an absent address makes a local-only host entry. |
| `max_sessions` | integer | `8` | Unsigned 32-bit integer | Session-budget policy metadata. |
| `warn_cpu_percent` | integer | `80` | Unsigned integer; operationally use `0..100` | CPU warning policy metadata. |
| `warn_memory_percent` | integer | `80` | Unsigned integer; operationally use `0..100` | Memory warning policy metadata. |
| `idle_detach_minutes` | integer | `120` | Unsigned integer | Idle-detach policy metadata. |
| `tmux_backed` | Boolean | `true` | `true`, `false` | Marks this host for tmux-backed tab/split inheritance. |

The current shipped runtime loads the numeric policy values but does not
automatically kill or reject sessions solely because a numeric limit is
crossed. Treat them as host policy metadata, not a resource-enforcement
boundary. Background probes require non-interactive SSH.

### Remote agent activity

Remote agents may emit pane-local OSC 666 lifecycle state through their
controlling terminal. Tool-specific hook configuration stays on the remote
host and must never receive the local runtime socket, registry, or bearer token.

## `keybindings.toml`

The file is created with defaults on first app start. `[shortcuts]` maps an
action name to a GTK accelerator string. Modifiers include `<Control>`,
`<Shift>`, `<Alt>`, `<Super>`, `<Meta>`, and `<Primary>`. Set a value to `""`,
`"none"`, or `"disabled"` to unbind it.

```toml
[shortcuts]
new-tab = "<Control>t"
leader-mode = "<Control>b"
copy-recent-output = "<Control><Shift>o"
shortcut-help = "question"  # Set to "none" to unbind `?`.

[chords.quick-action]
d = "dev"   # Single key -> mise task name.
t = "test"

[chords.leader]
c = "new-tab"            # Single key -> built-in action name.
"%" = "split-vertical"
"\"" = "split-horizontal"
```

Every bindable action and its default:

| Action key | Default | Operation |
| --- | --- | --- |
| `new-tab` | `<Control>t` | Create a plain tab. |
| `new-tmux-tab` | `<Control><Shift>t` | Create a tmux-backed tab. |
| `previous-tab` | `<Control><Shift>Tab` | Switch to the previous tab. |
| `search-toggle` | `<Control><Shift>f` | Toggle terminal search. |
| `jump-attention` | `<Control><Alt>j` | Jump to the next tab needing attention. |
| `command-palette` | `<Control><Shift>p` | Open the command palette. |
| `shortcut-help` | `question` (`?`) | Open effective shortcut help from app chrome. VTE and text inputs consume literal `?` first. |
| `register-project` | `<Control><Alt>p` | Register the focused pane's repository and launch context. |
| `workspace-inspector` | `<Control><Shift>i` | Open the workspace inspector. |
| `leader-mode` | unbound | Wait for a `[chords.leader]` follow-up. |
| `quick-action` | `<Control>F5` | Open the mise quick-action chord. |
| `toggle-broadcast-input` | `<Control><Shift>b` | Toggle input broadcast. |
| `copy` | `<Control><Shift>c` | Copy the selection. |
| `copy-recent-output` | `<Control><Shift>o` | Copy the current prompt's output or configured recent rows. |
| `copy-last-message` | `<Control><Shift>m` | Copy the last detected agent message. |
| `paste` | `<Control><Shift>v` | Paste clipboard text. |
| `toggle-selection-mode` | `<Control><Shift>s` | Toggle keyboard selection mode. |
| `split-vertical` | `<Control><Shift>backslash` | Split left/right. |
| `split-horizontal` | `<Control><Shift>minus` | Split top/bottom. |
| `close-pane` | `<Control><Shift>w` | Close the focused pane. |
| `toggle-pane-zoom` | `<Control><Shift>z` | Zoom/unzoom the focused pane. |
| `focus-pane-left` | `<Control><Shift>h` | Focus left. |
| `focus-pane-right` | `<Control><Shift>l` | Focus right. |
| `focus-pane-up` | `<Control><Shift>k` | Focus up. |
| `focus-pane-down` | `<Control><Shift>j` | Focus down. |
| `jump-previous-prompt` | `<Control><Shift>Up` | Jump to the previous OSC 133 prompt. |
| `jump-next-prompt` | `<Control><Shift>Down` | Jump to the next OSC 133 prompt. |
| `new-workspace` | unbound | Create a workspace. |
| `previous-workspace` | `<Control><Alt>Tab` | Return to the previous workspace. |
| `next-workspace` | `<Control>Page_Down` | Cycle forward. |
| `prev-workspace` | `<Control>Page_Up` | Cycle backward. |
| `discover-tab` | `<Control><Shift>d` | Discover tasks for the current tab. |
| `send-to-pane` | `<Control><Shift>e` | Send content to another pane. |
| `clipboard-history` | `<Control><Shift>y` | Open clipboard history. |
| `recent-files` | `<Control><Shift>r` | Open recent files. |
| `peek-file` | `<Control><Shift>space` | Open file peek. |
| `history-view` | `<Control><Alt>h` | Open searchable durable History. |

`[chords.quick-action]` keys must each be one character and values are arbitrary
mise task names. Its defaults are `d=dev`, `t=test`, `b=build`, `l=lint`, and
`s=setup`. `[chords.leader]` keys must also be one character; values must be one
of the action keys above except `leader-mode` and `quick-action`.

## Imported Ghostty configuration

Taarof reads `~/.config/ghostty/config` and recursively follows `config-file`
imports. This is a compatibility subset, not the complete Ghostty schema.

```text
config-file = ?"~/.config/omarchy/current/theme/ghostty.conf" # Optional import.
font-family = "JetBrainsMono Nerd Font" # Terminal font family.
font-size = 12                           # Floating-point point size.
background = #14110c                     # Terminal background.
foreground = #efe7d6                     # Terminal foreground.
cursor-color = #efe7d6                   # Cursor color.
selection-background = #463726           # Selection background.
selection-foreground = #efe7d6           # Selection foreground.
palette = 0=#34291a                       # Palette index 0..15; repeat per index.
cursor-style = block                      # VTE cursor shape string.
cursor-style-blink = false                # Boolean cursor blink.
copy-on-select = true                     # Automatically copy a selection.
command = /bin/bash                       # Shell/command for new plain panes.
window-padding-x = 0                      # Horizontal pane padding in pixels.
window-padding-y = 0                      # Vertical pane padding in pixels.
```

Unsupported Ghostty keys are ignored by Taarof, even though Ghostty itself may
use them. Taarof has built-in defaults when the file or an optional import is
missing. App-chrome colors belong in `[appearance]`; only terminal rendering is
imported here.

## Per-project `.taarof.config`

Place this TOML file at or below the git root. Taarof searches upward from the
pane/workspace directory but does not cross the git root.

```toml
[mise]
pinned_tasks = ["lint", "test"] # Task names shown first in quick actions.

[workspace]
pinned_dashboard_views = [      # Dashboard preset identifiers shown first.
  "agent-activity",
  "workspace-health",
  "listening-ports",
  "recent-alerts",
  "currently-flagged-panes",
  "failing-test-runs",
]

[agents]
signatures = [
  { name = "helper", patterns = ["helper-agent", "helperd"] },
]
```

`pinned_tasks` is a list of non-empty mise task-name strings. Dashboard values
must be one of the six identifiers shown. Project signatures append to the
global catalogue. `name` is the UI label; `patterns` are process-name or command
line markers. If patterns are omitted, the name becomes the sole pattern.

## Global `agent-signatures.json`

```json
{
  "signatures": [
    { "name": "helper", "patterns": ["helper-agent", "helperd"] }
  ]
}
```

The top-level `signatures` value is an array. Every item has a non-empty `name`
and an optional array of string `patterns`; an empty pattern list falls back to
the name. A valid, non-empty file replaces the built-in global catalogue; use
per-project `.taarof.config` signatures when the new entries should append to
the catalogue instead. Set `TAAROF_AGENT_SIGNATURES` to an absolute alternate
path before starting Taarof when the default path should not be used.

## Remote-control boundary

The v0.1.x public release does not ship gateway configuration. Keep the
HTTP API on loopback and follow the root `SECURITY.md` guidance.

## UI-managed JSON stores

`templates.json` and `views.json` are schema-versioned stores written by the
app. Prefer the Taarof UI instead of hand-editing them. Their stable envelopes
are:

```json
{ "version": 1, "templates": [] }
```

```json
{ "version": 1, "views": [] }
```

Registered projects use a separate stable envelope:

```json
{ "schema": "taarof.projects.v1", "projects": [] }
```

Press `Ctrl+Alt+P` (or invoke the bindable `register-project` action) in a pane
to save its canonical repository, checkout root, branch, host identity, and
preferred regular/tmux launch mode. Registered projects appear as `Project:`
entries in the command palette; matching `Forget project:` entries remove them.
Local, SSH, local-tmux, and remote-tmux panes are classified from live pane
metadata; ambiguous or unavailable repository identity fails without writing
partial data. SSH records may retain sanitized routing options (port, login,
and jump host), but identity-file and credential-related options are dropped.
Use an SSH config alias for advanced or remote-tmux connections.

A project may optionally contain a `coder` object with `workspace_name`,
`repo_path`, optional `template`, and `create_if_missing`; set
`preferred_open` to `"coder"` to activate that adapter. Coder launches use
the external `coder` CLI. `create_if_missing` is the explicit guard for running
`coder create`; when false, a missing workspace is reported instead. Never put
Coder tokens, SSH keys, passwords, remote URLs containing credentials, or other
secrets in `projects.json`.

A view record has `name`, a `preset` from the six project preset identifiers,
and an optional positive `limit`. Template records contain complete saved tab
or workspace state and should be treated as generated data.

## Configuration safety checklist

- Back up `~/.config/taarof` before overwriting or migrating it.
- Run `taarof config validate` after every TOML/keybinding edit.
- `config.toml` changes use validated live reload. Appearance, sidebar settings, future app actions, and the SSH/tmux host catalogue update immediately; existing attached tmux panes keep their current target until reconnect. A failed validation preserves the last known-good snapshot.
- Restart Taarof after keybinding, agent-signature, or direct Ghostty changes, and after changing startup-owned app services such as HTTP listeners/control, history storage, or automatic task discovery. The update watcher is startup-installed, but `update.installed_binary_path` is live-read on every refresh.
- Keep HTTP and the gateway on loopback.
- Never store runtime bearer tokens in any configuration file.
- Verify SSH remotes with `BatchMode=yes` before relying on background probes.
- Verify the live binary and API after installation; a successful build alone
  is not runtime proof.
