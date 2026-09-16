# taarof

`taarof` is a Linux-native terminal workspace app built with Rust, GTK4/libadwaita,
and VTE. The primary shipped surface in this repo is `taarof-app/`: tabbed
terminals, split panes, session restore, agent-aware activity indicators,
optional tmux-backed panes, and an optional local HTTP API with a paired
`taarof-web/` browser client.

The installed `taarof` command comes from `taarof-cli/taarof`, the Python
client for the desktop app. `examples/taarof` is a separate experimental
Zellij/SSH launcher with the same filename; do not install it over the CLI.
Its [WASM sidebar](wasm-sidebar/README.md) has its own build and is not part
of the desktop bundle.

![taarof desktop with agent-aware sidebar and Agents dock](docs/media/assets/hero.png)

![creating an agent worktree workspace, watching it work, and seeing when it needs input](docs/media/assets/agent-worktree.gif)

## Highlights

- Tabs and split panes with keyboard navigation
- Session restore for tabs, panes, and working directories
- Agent status indicators and activity text in the sidebar
- Optional tmux-backed panes plus detach/attach flows
- Ghostty theme import and OSC 7 cwd tracking, including SSH-aware path display
- Local Unix socket API and optional loopback-only HTTP API
- Saved dashboard views and workspace templates
- Responsive browser view that starts in observe mode and can unlock local control

## Product stance

Taarof is first and foremost a terminal with side tabs. It can observe and show
agent state for panes and tabs, but agent workflows, task trackers, dashboards,
and loop-runner integrations are optional surfaces rather than required ways to
use the app. Public-facing defaults should stay unopinionated: users should be
able to keep the UI minimal, hide advanced panels, and bind their own commands.

## Install

The supported install path in this checkout is a source build plus the local
Linux packaging script. The installed desktop binary is currently
`taarof-app`.

### System packages

| Distro | Packages |
| --- | --- |
| Arch / Manjaro | `sudo pacman -S gtk4 libadwaita gtksourceview5 vte4 sqlite` |
| Fedora 39+ | `sudo dnf install gtk4-devel libadwaita-devel gtksourceview5-devel vte291-gtk4-devel sqlite-devel` |
| Ubuntu 26.04+ | `sudo apt install libgtk-4-dev libadwaita-1-dev libgtksourceview-5-dev libvte-2.91-gtk4-dev libsqlite3-dev` |

You also need a recent Rust toolchain and Node.js:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Build and install locally

Start from a local checkout of this repo. With `mise`, `mise run install` runs
the build, provenance, and install steps below, but does not launch the app.
Without it:

```bash
cargo build --release --manifest-path taarof-app/Cargo.toml
cargo build --release --manifest-path agent-launcher/Cargo.toml
(cd taarof-web && npm ci && npm run build)
bash packaging/linux/emit-artifact-provenance.sh taarof-app/target/release/taarof-app
bash packaging/linux/install-local.sh
~/.local/bin/taarof-app
```

`packaging/linux/install-local.sh` installs the release binary, desktop entry,
metainfo, icon, and built web bundle under `~/.local`.
`emit-artifact-provenance.sh` records which commit the binary came from. It
fails on a tree without usable Git metadata, such as an unpacked source
archive; skip it there and the app reports its source identity as unknown.

### Install from a GitHub release tarball

Version tags publish a Linux x86_64 tarball and checksum as GitHub Release
assets. Download both files, verify the checksum, then run the bundled installer:

```bash
sha256sum -c taarof-linux-x86_64.tar.gz.sha256
tar -xzf taarof-linux-x86_64.tar.gz
./taarof-linux-x86_64/install.sh
~/.local/bin/taarof-app
```

The tarball installer installs the same desktop binary, CLI, desktop entry,
metainfo, icon, and web bundle under `~/.local`.
The shipped tarball bundles SQLite into `taarof-app`, so it does not require a
system SQLite runtime package for OpenCode history.

### Run from source during development

```bash
(cd taarof-web && npm ci && npm run build)
cargo run --manifest-path taarof-app/Cargo.toml
```

If `taarof-web/dist` is missing, the browser client will not load from a source
checkout until you build it.

Source and distro builds link the system SQLite library for OpenCode history.
Build with `--features bundled-sqlite` for a self-contained binary, or with
`--no-default-features` to omit the OpenCode history adapter.

Published AUR packages are not documented here yet. The GitHub release tarball
is the binary artifact intended to unblock that packaging path.

## Local Web Client

When `[http].enabled = true` is set in `~/.config/taarof/config.toml`, `taarof`
serves the browser client at `/` and mirrors the query API over local HTTP with
bearer-token auth. Browser control remains disabled unless the separate
`[http_control].enabled = true` gate is active on a loopback bind.

### Command-line tool

The local installer also installs `taarof` into `~/.local/bin/taarof`. It is a
thin Python 3.11+ client for the existing local HTTP and Unix socket APIs, so
it works with a running `taarof-app` without launching a second GUI process.

```bash
which taarof
taarof --help
taarof list-tabs
taarof agent-workspace --branch test --command 'echo hi'
taarof send-keys --pane 1 --keys-file /tmp/paste.txt
printf 'large paste payload' | taarof send-keys --pane 1 --stdin
```

`taarof list-tabs` returns the same JSON shape documented for the local socket
API. `agent-workspace` forwards directly to the app's existing worktree-aware
socket action, so it can create or reuse a worktree workspace from the CLI.
For large paste payloads, prefer `send-keys --keys-file` or `send-keys --stdin`
over inline `--keys` so the shell does not impose an argument-length limit.

### Local web client

With HTTP enabled, taarof serves the browser client at `/` from the built web
bundle. The client opens in observe mode; selected panes can only send input
after an explicit control unlock and a loopback-only `[http_control]` gate.
Open it with the runtime token in the URL:

```bash
http://127.0.0.1:<port>/?token=$(cat "$XDG_RUNTIME_DIR/taarof-http-<pid>.token")
```

**Token persistence note.** The web client saves the bearer token in your
browser's `window.localStorage` under the key `taarof.web.token`, so it survives
browser restarts until you clear it. Under the default loopback-only HTTP bind
the token never leaves your machine, so this is low risk and saves re-pasting the
token on every reload. If you ever expose the HTTP API beyond loopback, that
stored token becomes a long-lived, higher-value credential — rotate or restrict
it, prefer a session-scoped setup, and clear it when you are done. To clear it,
remove the `taarof.web.token` entry from the browser's localStorage (DevTools →
Application → Local Storage) or open the client in a fresh private window.

See [docs/local-query-api.md](docs/local-query-api.md) for the HTTP routes,
WebSocket event stream, token model, and runtime caveats.

## Automation Surface

`taarof` exposes a same-user Unix socket API for local automation plus a
matching HTTP surface when enabled. The versioned `query-state`,
`query-events`, and `list-tabs` APIs are documented in
[docs/local-query-api.md](docs/local-query-api.md).

The local runtime trust boundary matters: any process that can connect to the
socket can switch tabs, create panes, run commands, send keys, and capture pane
text. Runtime-dir ownership and loopback-only defaults are part of the security
model. See [SECURITY.md](SECURITY.md).

## Documentation

| Doc | Purpose |
| --- | --- |
| `README.md` | Human-facing overview, install, and feature summary |
| [docs/setup.md](docs/setup.md) | Complete local, SSH/tmux remote, workstation, and gateway setup |
| [docs/configuration.md](docs/configuration.md) | Every supported configuration file, key, default, and parameter |
| [RUNBOOK.md](RUNBOOK.md) | Operator tasks, restart, backup, and health checks |
| [TROUBLESHOOTING.md](TROUBLESHOOTING.md) | Known failure modes and fixes |
| [docs/local-query-api.md](docs/local-query-api.md) | Socket and HTTP API reference |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Contribution guide |
| [SECURITY.md](SECURITY.md) | Security reporting and trust model |
| [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) | Community expectations |

## Status

- Linux only
- Desktop binary name currently remains `taarof-app`
- Optional HTTP API defaults to loopback-only binding
- Browser/mobile access is observation-first; desktop usage remains the primary
  experience

Repository automation validates formatting, tests, desktop buildability, the
web bundle, and local Linux packaging on pull requests. See
[docs/release.md](docs/release.md) for the current release policy.

## License
This project is dual-licensed under either `MIT` or `Apache-2.0`, at your option. See `LICENSE-MIT` and `LICENSE-APACHE`.

## CWD Tracking Setup

Taarof reads OSC 7 directory reports and VTE `OSC 666` prompt signals from
Bash, Zsh and Fish. The helpers also emit standard OSC 133 shell markers. Installers provide helpers under `<prefix>/share/taarof/shell` without
editing your startup files. Source checkouts provide `taarof-app/resources/osc7.*`
and `examples/taarof-shell-integration.*`.

Follow [the shell setup instructions](docs/setup.md#3-install-shell-integration-locally)
for the two source lines for your shell and the remote-copy procedure. Helpers
encode spaces and Unicode in CWD reports and preserve existing prompt hooks.
Bash and Zsh additionally report git branches in the terminal title. Install the
helpers on remote hosts too so SSH panes can report their remote directories.

## How It Works (Not a Ghostty Plugin)

taarof-app **is a terminal emulator** — it doesn't run inside Ghostty, kitty, Alacritty, or any other terminal. It uses [VTE](https://wiki.gnome.org/Apps/Terminal/VTE) (the same rendering library behind GNOME Terminal, Tilix, and Terminator) as its terminal backend.

The Ghostty connection is **config-only**: taarof reads `~/.config/ghostty/config` at startup to import your theme, so your terminals look the same without duplicating config.
It installs that Ghostty-derived data together with `~/.config/taarof/config.toml`
as one process-wide config snapshot, and runtime actions use that snapshot
rather than rereading disk on hot paths.

| What | How |
|------|-----|
| Terminal rendering | VTE (libvte-2.91-gtk4) — same backend as GNOME Terminal |
| Window toolkit | GTK4 + libadwaita |
| Colors, font, cursor | Parsed from Ghostty config at startup |
| Without Ghostty installed | Falls back to taarof's warm Majlis terminal palette |
| Shell command | Respects Ghostty's `command =` setting, else uses `$SHELL` |
| Copy-on-select | Respects Ghostty's `copy-on-select = true` |
| Mouse-friendly selection in TUIs | Plain drag visibly selects and copies terminal text even when a TUI enables mouse reporting; `Shift+Drag` keeps VTE's native selection behavior; use a non-Shift modifier such as `Ctrl+Drag` when you want a mouse-aware TUI to receive the drag |

## Ghostty Parity and SSH Restore Boundaries

taarof intentionally imports **appearance and shell defaults** from Ghostty, but keeps
workspace, tab, pane, keybinding, and socket behaviors **taarof-native**.
That means Ghostty theme values map well, while Ghostty-only window-management or
binding semantics do not automatically carry over.

SSH restore is also **best effort** rather than exact replay:

- local panes restore into their saved working directory
- SSH panes restore by relaunching the saved SSH command
- when the saved remote path is known and the SSH command supports remote exec,
  taarof now replays a remote `cd <saved-path>` before starting the shell
- taarof does **not** promise durable reconnect across network interruptions or full
  remote runtime/session continuation; for exact continuity, use remote `tmux` or
  `zellij`

## Appearance and Sidebar Customization

taarof also reads `~/.config/taarof/config.toml` for app chrome and sidebar
preferences. `[appearance]` exposes the complete Majlis colour-role palette;
terminal panes remain controlled by the imported Ghostty theme. `[sidebar]`
keeps layout/icon settings and optional sidebar-only overrides for compatibility.
Taarof validates and reloads `config.toml` live; direct Ghostty edits and
startup-owned services still require a new process.

```toml
[appearance]
base = "#14110c"
mantle = "#100d09"
crust = "#0b0906"
surface = "#241d13"
surface_raised = "#34291a"
border = "#463726"
text = "#efe7d6"
subtext = "#c3b9a7"
muted = "#7c715e"
title = "#f6f0e4"
soft_text = "#d8cdb6"
warm_muted = "#a08d6f"
accent = "#e0a43a"
accent_hover = "#f0be5e"
running = "#7bbf7a"
activity = "#6bb0b8"
waiting = "#d9b34a"
ports = "#e0913a"
error = "#d6796b"

[sidebar]
position = "right"
background = "#101820"
surface = "#202938"
accent = "#f59e0b"
text = "#f8fafc"
muted = "#94a3b8"

[sidebar.workspace-icons]
worktree = "⑂"

[sidebar.workspace-icons.by-repo]
taarof = "⌘"

[sidebar.workspace-icons.by-name]
infra = "🛠"

[terminal]
copy_recent_lines = 200

[mise]
include_global = false

[tasks]
enabled = false
pull_requests = false
default_view = "tasks"

[loop_runner]
enabled = false
dev_command = "loop-runner dev-loop --repo {repo} --issue {issue}"
pr_command = "loop-runner pr-loop --repo {repo} --pr {pr} --loop {loop}"
review_loop = "pr-review-readonly"
merge_loop = "pr-review"

[editor]
open_command = "zed {path}:{line}:{col}"

[browser]
open_command = "helium-browser {url}"
```

- `[appearance]` values must be six-digit RGB hex colours; every role is optional and omitted roles keep the built-in Majlis value
- terminal-pane colours and fonts still come from `~/.config/ghostty/config`
- `sidebar.position` accepts `left` or `right`; `sidebar.compact = true` starts with a 64 px icon-and-status rail. Use **Toggle Compact Sidebar** in the command palette to switch at runtime without rewriting config.
- sidebar theme keys are optional, override `[appearance]` only inside the sidebar, and accept hex or named CSS colours
- icon rules match exact workspace names first, then repo basenames, then fall back to a derived initial
- `terminal.copy_recent_lines` sets how many terminal rows the in-app `Copy Recent Output` action copies from the active pane
- `terminal.copy_recent_lines` defaults to `200` and accepts values from `1` to `10000`
- `mise.include_global` defaults to `false`; set it to `true` to include tasks from `~/.config/mise/config.toml` alongside project and parent-directory tasks
- `dock.visible` defaults to `false`. Set it to `true` to show the right-hand dock at startup, or run **Toggle Right Dock** from the command palette (the runtime toggle does not rewrite config). The dock exposes **Session** and **Agents**. Agents is a live, observational board with one card per detected agent pane; each card shows provider identity, tab/workspace context, current activity, and a working/waiting/done/error state, and opens the exact originating pane when selected. It does not replace any external task or PR truth.
- `tasks.enabled` defaults to `false` and governs **automatic** task display: when `true`, tabs auto-discover their tasks on startup and you get the persistent right-hand **Tasks** panel plus the sidebar `.plan/tasks.json` tracking lists, mise quick-action buttons (Dev/Test/Build), and inspector task sections without asking. It is off by default so opening a pane in a repo that happens to contain a `.plan/` directory doesn't surface its task backlog. Changing it takes effect on app restart.
  - Even with `tasks.enabled = false`, the task UI is still available **on demand**: running **Discover Tasks** for a tab (Ctrl+Shift+D, the sidebar right-click menu, or the command palette) reveals that tab's tasks and mise actions in the sidebar/inspector. This reveal is per-tab and transient — it resets on restart, so a fresh launch always starts clean. (The persistent right-hand panel and auto-on-startup still require `tasks.enabled = true`.)
- `tasks.pull_requests` defaults to `false`; when enabled, the persistent right-hand panel gets a per-repository switch between `.plan/tasks.json` and branch PRs. The PR view runs `gh pr list --head <branch> --state all --json ...` from the active local checkout, so it requires GitHub CLI auth and only applies to local GitHub-backed branches.
- `tasks.default_view` accepts `"tasks"` or `"pull_requests"` and controls the initial right-hand panel view when `tasks.pull_requests = true`; switching views is remembered separately for each active repo branch during the app session.
- `[loop_runner]` connects the Tasks panel to an external "loop runner" CLI that can build a `.plan` task into a PR or review/merge a branch PR. The integration is **config-driven** — taarof never hardcodes a tool name — so it stays generic for the public export:
  - `loop_runner.enabled` defaults to `false`. The per-task **Build with loop runner** button and the per-PR **Review only** / **Review & merge** buttons only appear when this is `true` **and** the active local checkout contains a `.loop/loops.yaml`. Repos without that file simply show no loop-runner buttons.
  - `loop_runner.dev_command` (default `loop-runner dev-loop --repo {repo} --issue {issue}`) is the template used to build a task. `{repo}` is the active checkout root, `{issue}` the task id, and `{loop}` the loop name.
  - `loop_runner.pr_command` (default `loop-runner pr-loop --repo {repo} --pr {pr} --loop {loop}`) is the template used to review a PR. `{repo}` is the checkout root, `{pr}` the PR number, and `{loop}` the loop name.
  - `loop_runner.review_loop` (default `pr-review-readonly`) is the **non-merging** loop used by **Review only**, and `loop_runner.merge_loop` (default `pr-review`) is the merge-capable loop used by **Review & merge**. **Review only requires a non-merging loop on the loop-runner side** (a `loops.yaml` entry with merging disabled, or an equivalent flag); if that loop is missing, the run surfaces an error toast rather than merging.
  - Dispatch behavior: clicking a button first shows a confirmation dialog naming the task/PR and the exact command (running an agent spends time and money; merging is irreversible), then runs the substituted command in a **new taarof tab** so the run is visible live. taarof tees the runner's final JSON result to a file under `$XDG_RUNTIME_DIR`, and on completion shows a **result toast** — the PR URL on a successful build, or `merged` / `reviewed (posted, not merged)` / `changes requested (comment posted)` / `checks not green` for a PR review — and refreshes the panel so a newly opened PR appears.
  - The build button is enabled only for **ready** local tasks (not done, no unresolved dependencies); blocked tasks show it disabled with a tooltip naming the unmet blockers. The Tasks panel also renders a **"Ready now — N can run in parallel"** grouping above the ready tasks, computed from the `.plan` dependency graph. Remote-host task dispatch is out of scope.
- `editor.open_command` (default `"zed {path}:{line}:{col}"`) — command template run when you Ctrl+Click a `path:line[:col]` reference in a pane; placeholders `{path}`, `{line}`, and `{col}` are substituted into individual arguments (not passed through a shell); a trailing `:{col}` is dropped when the clicked reference has no column; relative paths resolve against the pane's current working directory (tracked via OSC 7)
  - the file must exist locally for the editor to launch; a clicked path that doesn't resolve to a real file does nothing
  - files on remote-host panes are not clickable, since a local editor can't open them
  - resolution uses the shell's *current* directory, so a relative path printed on an older line before a `cd` may resolve incorrectly
- `browser.open_command` (unset by default) — command template used to open external URLs (clicked terminal links, task-panel PR links); `{url}` is substituted into individual arguments (not passed through a shell), and a template without `{url}` gets the URL appended as the final argument; leave it unset to use the system default browser, and if the configured command fails to launch, taarof falls back to the system default

### Configuration lifecycle

Config lifecycle semantics distinguish a live snapshot from startup-owned
services:

- Taarof watches `config.toml`, validates a complete replacement snapshot off
  the GTK thread, and keeps the last known-good snapshot if an edit is invalid.
  Appearance, sidebar placement/compact mode/icons, dock visibility, and
  subsequent configuration lookups (including new tmux/SSH host use) update
  live. Existing attached panes retain their current target until reconnect.
- `keybindings.toml`, `agent-signatures.json`, and direct Ghostty-config edits
  are not watched. Restart Taarof after changing them.
- The HTTP listener and control gate, optional history store, and automatic
  `[tasks] enabled` startup discovery are created during startup. Change those
  settings by restarting Taarof. The update watcher is also installed at
  startup, but every refresh reads `[update].installed_binary_path` from the
  current live snapshot, so a path change affects the next refresh without a
  restart. **Reload into update**, when the update dialog is available, starts
  a new process through the normal session-save path.

See `docs/configuration.md` for the per-file lifecycle, including the
boundary between live reload and executable reload.

### Platform Compatibility

taarof-app runs on **Linux with GTK4** (Wayland or X11). It requires:
- `gtk4` — window toolkit
- `libadwaita` — adaptive UI / dark theme
- `vte4` (vte-2.91-gtk4) — terminal rendering
- `gtksourceview5` — syntax highlighting in the file peek overlay
- SQLite development headers for source/distro builds with OpenCode history
  enabled by default

These are available on most Linux distros. **Not currently available on macOS or Windows** (VTE is Linux-only).

| Distro | Install |
|--------|---------|
| Arch / Manjaro | `sudo pacman -S gtk4 libadwaita gtksourceview5 vte4 sqlite` |
| Fedora 39+ | `sudo dnf install gtk4-devel libadwaita-devel gtksourceview5-devel vte291-gtk4-devel sqlite-devel` |
| Ubuntu 26.04+ | `sudo apt install libgtk-4-dev libadwaita-1-dev libgtksourceview-5-dev libvte-2.91-gtk4-dev libsqlite3-dev` |
| Older Ubuntu/Debian | Unsupported by the v0.1.x binary; taarof requires VTE >= 0.78 |

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Ctrl+T` | New tab |
| `?` | Open keyboard-shortcut help when app chrome has focus; terminal/text inputs keep literal `?` |
| `Ctrl+Shift+C` | Copy selection |
| `Ctrl+Shift+V` | Paste clipboard |
| `Ctrl+Alt+J` | Jump to the next tab needing attention |
| `Ctrl+Alt+H` | Open searchable durable History |
| Unbound by default | Copy recent output using `terminal.copy_recent_lines` |
| Plain drag | Select and copy terminal text locally, including inside mouse-reporting TUIs |
| `Shift+Drag` | Use VTE's native terminal selection behavior |
| `Ctrl+Drag` / `Alt+Drag` | Pass mouse drag through to mouse-aware TUIs |
| `Ctrl+Shift+S` | Toggle selection mode for the focused pane |
| `Ctrl+Shift+H/J/K/L` | Focus pane left/down/up/right |
| Double-click tab | Rename tab |
| Double-click "session" | Rename session |
| `Ctrl+Click` | Open plain `http(s)` URL or OSC 8 hyperlink under cursor; or open a `path:line[:col]` file reference in your configured editor |

The native **History** overlay and the authenticated web client's **History**
tab search the optional durable SQLite store across restarts. Results are
sanitized observations, not current task or PR authority: every row shows its
source and observed time separately from an exact live target's checked time.
Missing or ambiguous pane, task, and PR identities remain readable but cannot
navigate.

### In-app keybinding help

For the full, always-current list, open the **command palette** (`Ctrl+Shift+P`)
and run **Keyboard Shortcuts**, or press `?` while ordinary app chrome has
focus. Terminal applications and text inputs retain literal `?`. The help shows
every action with its *currently
effective* trigger — defaults overlaid with your `keybindings.toml`
customizations, grouped by category. Rebound actions are flagged
`(customized)`, and unbound actions read `unbound`. The list is generated live
from your keybinding config, so it never goes stale. Press `Esc` (or pick *Back
to Commands*) to close it.

### First-run action hints

A fresh workspace with no tabs shows a few unobtrusive next-action hints — new
tab, new tmux-backed tab, split a pane, command palette, next workspace — each
paired with its current keybinding. These dim hints disappear as soon as the
workspace has a tab, and reflect any `keybindings.toml` overrides.

Shortcuts are configurable in `~/.config/taarof/keybindings.toml`. The recent-output
action is `copy-recent-output`. Leader mode is opt-in: bind `leader-mode` in
`[shortcuts]`, then map one-key follow-ups in `[chords.leader]`.

Example:

```toml
[shortcuts]
leader-mode = "<Control>b"

[chords.leader]
c = "new-tab"
w = "command-palette"
z = "toggle-pane-zoom"
"%" = "split-vertical"
"\"" = "split-horizontal"
```

A tmux-flavored starter preset is included at `examples/keybindings.tmux.toml`.
Leader bindings currently target built-in taarof actions only.

## Tab Indicators

| Indicator | Meaning |
|-----------|---------|
| Green dot (●) | AI agent running (Claude, Codex, Aider, etc.) |
| Cyan status line | Socket-driven activity text such as `editing main.rs` |
| Blue dot (●) + status line | Agent/tool just finished; inactive tabs still pulse for attention |
| No dot | No detected agent or recent activity state |

Detection combines `/proc` process-tree scans with fresh activity signals. A detected local agent process qualifies recent terminal output for the green running indicator, but process presence alone does not keep a tab running forever.
Tools can push higher-signal status text over the Unix socket or VTE termprops, and taarof falls back to debounced terminal-output scanning when no fresh explicit update exists. Waiting-for-input and error activity render as alerts instead of running dots.

## Session Persistence

Sessions auto-save every 60 seconds and on window close to `~/.local/share/taarof/session.json`. On next launch, tabs, pane layouts, working directories, and the active tab are restored.

## Saved Dashboard Views

The dashboard includes a **Saved Views** section for lightweight live views built
from taarof's current in-memory state and recent event feed. The first MVP ships
with built-in presets for:

- Agent Activity
- Workspace Health
- Listening Ports
- Recent Alerts
- Currently Flagged Panes
- Failing Test Runs

Use the command palette to:

- save a named view from one of the built-in presets
- open a saved view later
- delete a saved view

Saved views are stored locally in `~/.config/taarof/views.json` and render live
current data when reopened.

Launching the same desktop app again reuses the existing window instead of creating a second independent session owner. If you intentionally want a separate instance, set `TAAROF_SESSION` before launch; for example `TAAROF_SESSION=dev cargo run --manifest-path taarof-app/Cargo.toml` uses a collision-resistant `~/.local/share/taarof/session-dev-<128-bit-digest>.json` namespace and its own GTK application instance.

## tmux Integration

`taarof` can optionally back individual panes with tmux sessions. The important
mental model is that `taarof` still owns the GTK tabs and splits, while tmux owns
the long-lived command state for tmux-backed panes.

The current tmux entrypoints include the sidebar `+ tmux tab` button, the
command palette entry, the default `Ctrl+Shift+T` shortcut, the Unix socket API
(`create-tmux-tab`, `detach-pane`, `attach-session`, `open-dashboard`), and
saved session restore. Workspaces can also opt into tmux inheritance so new
plain tabs and splits use tmux automatically when `[tmux].enabled = true`.
If local tmux is missing and no remote tmux hosts are configured, the GUI
entrypoint stays disabled and explains how to enable tmux-backed tabs.
See [docs/tmux-integration.md](docs/tmux-integration.md) for the full model,
config, session naming scheme, remote-host behavior, and concrete examples.

## Automation / IPC

taarof exposes a Unix socket for external notifications. The live socket path is
per-process, and the current active instance is published to a stable runtime
registry file.

Default session:

- socket: `$XDG_RUNTIME_DIR/taarof-<pid>.sock`
- registry: `$XDG_RUNTIME_DIR/taarof-current.json`

Named session with `TAAROF_SESSION=dev`:

- socket: `$XDG_RUNTIME_DIR/taarof-s-<128-bit-digest>-<pid>.sock`
- registry: `$XDG_RUNTIME_DIR/taarof-current-dev-<128-bit-digest>.json`

Pass `--session dev` to the CLI so it derives the collision-resistant key; do
not construct these filenames manually.

If `XDG_RUNTIME_DIR` is not set, taarof falls back to `/run/user/<uid>`.

For safety, the automation socket is only started when the selected runtime
directory is owned by the current user and has private `0700`-style
permissions. If `XDG_RUNTIME_DIR` is set but fails validation, taarof disables
the socket server instead of falling back to another directory.

This socket is a privileged same-user local control surface. Any process that
can connect can switch tabs, create panes, run shell commands, send raw keys,
and capture pane text. The runtime directory permissions are the access
control; treat them as part of your local trust boundary.

Supported message shapes include notifications, activity updates, pane control,
a versioned local query API, and the F010 scripting API:

```json
{"action":"notify","tab":"Shell 1","message":"Build done"}
{"action":"agent-status","tab":"current","pane":1,"state":"running","text":"Editing terminal.rs","source":"claude"}
{"action":"agent-status","tab":"current","pane":1,"state":"done","text":"Done","source":"claude"}
{"action":"agent-status","tab":"current","pane":1,"state":"idle"}
{"action":"create-tab","name":"dev","working_dir":"/tmp/project"}
{"action":"switch-tab","tab":"dev"}
{"action":"rename-tab","tab":"dev","name":"server"}
{"action":"list-tabs"}
{"action":"query-state"}
{"action":"query-events","since_seq":0,"limit":100}
{"action":"open-pane","tab":"server","command":"npm run dev","direction":"horizontal"}
{"action":"run-in-pane","tab":"server","pane":1,"command":"rspec"}
{"action":"send-keys","tab":"server","pane":1,"keys":"\u0003"}
{"action":"get-text","tab":"server","pane":1,"scrollback":200}
{"action":"close-pane","tab":"server","pane":1}
{"action":"close-tab","tab":"server"}
```

`tab` may be a tab name, numeric tab ID, or `"current"`; when omitted,
`agent-status` targets the active tab. `pane` is the stable pane ID reported by
`query-state`. It may be omitted only when the target tab has exactly one
addressable pane; multi-pane tabs reject an unattributed update rather than
assigning it to the focused pane. The CLI exposes the same field as
`taarof agent-status --pane ID`.

Name-based tab targeting only works when that tab name is unique across all
workspaces. If multiple tabs share the same name, taarof returns an ambiguity
error instead of picking the first match. Automation clients should call
`list-tabs` and then use numeric `tabs[].tab_id` plus the per-tab
`pane_id` for mutating actions such as `switch-tab`, `open-pane`,
`run-in-pane`, `send-keys`, `get-text`, `close-pane`, and `close-tab`.

Response shape:

- `{"ok":true}` for simple success
- `{"ok":true,"tab_id":7}` for `create-tab`
- `{"ok":true,"pane_id":3}` for `open-pane`
- `{"ok":true,"data":{...}}` for `list-tabs`, `query-state`, `query-events`, and `get-text`
- `{"ok":false,"error":"..."}` on validation or routing errors

`list-tabs` returns workspace and tab structure plus pane metadata including
workspace `id`, tab `tab_id`, `shell_running`, `has_child_process`, `cwd`, and
nullable `cwd_host` for remote OSC 7 paths. `has_child_process` reports local
process-tree state and is `null` across SSH or remote-tmux boundaries, where
remote child state cannot be observed. Use the returned numeric
`tab_id`/`pane_id` values whenever a human-readable tab name may collide across
workspaces. `close-tab` is non-interactive for scripts: if the target is the
only tab in a worktree workspace, taarof closes the workspace but does not delete
the underlying worktree.

`query-state` is the new versioned read-only snapshot API. It returns
`schema: "taarof.state.v1"` plus workspaces, tabs, panes, live health state,
durable diagnostics metadata, active ports, current alerts, dashboard state,
detached sessions, API capability flags, and event cursor metadata.
`workspace-state` is accepted as a backwards-compatible alias.
`data.events.high_watermark` is the current tail sequence and can be used to
seed a later `query-events` call; `data.events.next_seq` is the next sequence
number that will be assigned locally. `data.health` now exposes degraded probe
and event-retention state, and `data.diagnostics` exposes the local JSONL
diagnostic log path plus recent failure records and counters.

`query-events` returns the in-memory recent event feed for the current taarof
process with `schema: "taarof.events.v1"`, `since_seq`, `limit`, `next_seq`,
`high_watermark`, `oldest_seq`, `capacity`, `dropped`, `last_dropped_at_unix_ms`,
and an `events` array. `next_seq` is the resume cursor based on the page actually
returned; `high_watermark` is the highest emitted sequence currently in the
local event store. `oldest_seq` is the lowest retained sequence — if your
`since_seq` is below it, events were lost to ring buffer rollover. The event
ring is fixed at 512 entries, and overflow metadata is now surfaced directly in
the API so dashboards can tell when they missed history.

When `[http].enabled = true` in `~/.config/taarof/config.toml`, taarof also
exposes the query API over a local HTTP server with token-based auth and
a WebSocket endpoint for live event streaming. `/health` now returns degraded
runtime status instead of only shallow process liveness. See
[docs/local-query-api.md](docs/local-query-api.md) for the full reference.
Non-loopback HTTP binds now require the explicit unsafe opt-in
`[http].unsafe_allow_non_loopback = true`.

## VTE Activity Protocol

taarof also listens for VTE termprops, which is the official terminal-side
protocol for phase 3 of F016. This uses `OSC 666`, not a custom `OSC 777`
subcommand.

Watched termprops:

- `vte.ext.taarof.agent.state`
- `vte.ext.taarof.agent.text`
- `vte.ext.taarof.agent.source`

Set `text` and `source` first, then `state` last:

```bash
printf '\e]666;vte.ext.taarof.agent.text=Editing terminal.rs\e\\'
printf '\e]666;vte.ext.taarof.agent.source=codex\e\\'
printf '\e]666;vte.ext.taarof.agent.state=running\e\\'
printf '\e]666;vte.ext.taarof.agent.text=Done\e\\'
printf '\e]666;vte.ext.taarof.agent.state=done\e\\'
```

Clear by setting `state=idle`, then resetting the other properties:

```bash
printf '\e]666;vte.ext.taarof.agent.state=idle\e\\'
printf '\e]666;vte.ext.taarof.agent.text\e\\'
printf '\e]666;vte.ext.taarof.agent.source\e\\'
```

Helper scripts are included at `taarof-app/resources/agent-status.bash` and
`taarof-app/resources/agent-status.zsh`.

## Agent lifecycle integration

Agent tools can report lifecycle state with OSC 666 using the helper scripts in
`taarof-app/resources/agent-status.bash` and `agent-status.zsh`. Keep any
tool-specific hook configuration in your own environment; it is not part of
the public source export.

## Agent Signature Config

Agent detection still works out of the box, but signatures can now be extended
without editing source code.

Default config path:

- `~/.config/taarof/agent-signatures.json`

Override path:

- `TAAROF_AGENT_SIGNATURES=/abs/path/to/agent-signatures.json`

Example:

```json
{
  "signatures": [
    { "name": "claude", "patterns": ["claude", "claude-code"] },
    { "name": "helper", "patterns": ["helper-agent", "helperd"] }
  ]
}
```

## Contributing

Contributions are welcome. Start with the
[contribution guide](CONTRIBUTING.md), and please follow the
[code of conduct](CODE_OF_CONDUCT.md) when participating.

## Architecture

| Module | Ownership |
| --- | --- |
| `main.rs`, `lib.rs` | Thin entry point plus GTK wiring, typed action registration, live-config watcher, and process bridges. |
| `runtime.rs`, `workspace.rs`, `pane.rs` | Main-thread `AppState`, workspace/tab models, and pane-tree mutations. |
| `terminal.rs`, `terminal/` | VTE lifecycle, PTY child spawning, restore, attach/detach, split handling, broadcast, and terminal signals. |
| `config.rs`, `keybindings.rs` | Validated live `config.toml` snapshot (including Ghostty import) and the bindable typed `Action` contract. |
| `socket.rs`, `socket/`, `http.rs`, `http/`, `api.rs` | Same-user socket protocol/registry, token-gated loopback HTTP bridge, and versioned state projections. |
| `session.rs`, `app_session.rs`, `history/`, `diagnostics.rs` | Coalescing JSON session persistence, optional observational SQLite history, and JSONL diagnostics. |
| `runtime_probe.rs`, `app_runtime.rs`, `agents/` | Off-GTK runtime probes, stale/degraded reconciliation, and agent process/transcript observation. |
| `task_launch.rs`, `mise/`, `task_panel.rs`, `sidebar/discovery.rs`, `palette.rs` | One safe task-launch plan shared by palette, sidebar, task panel, and task actions. |
| `git.rs`, `tmux.rs`, `host.rs` | Off-GTK Git/worktree operations, tmux control, and remote-host probes. |

The module table above is the source tour for this tree; the fuller
data-flow material lives in the private development history.

### Standalone agent launcher

The installer also ships `agent`. It discovers local provider history without a
running Taarof process. When available, the same-user private Unix socket adds
fresh live identities and cached remote history. `agent --session NAME --json`
selects a named runtime (or use `TAAROF_SESSION` / `TAAROF_SOCK`). A missing or
unresponsive runtime reports a diagnostic and preserves local results.
`agent --attach '<stable-ref JSON>'` focuses an exact live tmux-backed pane;
`--resume` starts a provider from history. Remote Resume uses the configured SSH
destination, quoted structured program/argv/cwd, and a TTY. Stale or failed
remote records remain visible with their actions disabled.
`agent --json` returns the `agent.sessions.v2` catalog;
`agent providers` and `agent doctor` return versioned provider inventories and
per-provider history diagnostics without starting provider processes.

Start a provider with `agent --new codex`. To resume exactly, pass the JSON
`stable_ref` object from the catalog as one quoted argument to `agent --resume`,
or use an exact session ID that is unique across the catalog. Ambiguous IDs,
unavailable executables and missing working directories fail without launching.
The provider replaces the launcher in the terminal and retains its ordinary
signals and exit status. Model, approval, sandbox and account settings come from
the provider's own configuration.

Run bare `agent` for the keyboard picker. It shows New providers, then sessions
from the current repository and its linked worktrees, then other sessions.
Each group is ordered by your last sent message, newest first, with local times.
Ctrl+S cycles repository-first, global time order and Active/Recent groups.
Type to search; use arrows and Enter
to select, Tab to change host scope, Ctrl+P to cycle providers, Ctrl+N to focus
New, Ctrl+R to refresh, `?` for help and Esc to cancel. Ctrl+F appears only for
declared fork actions. `agent --new` and `agent --resume` open the corresponding
selection view. The picker requires a terminal; explicit targets and `--json`
remain scriptable. See [setup](docs/setup.md#keyboard-session-picker) for details.
