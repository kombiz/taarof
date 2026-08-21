# TROUBLESHOOTING.md — taarof Known Failure Patterns

> **This is the first doc any agent should consult before making changes.**
> Start with the current Taarof sections below. Ghostty, Zellij, and Textual
> material is retained only as clearly labeled historical compatibility context.

---

## Before You Rewrite Anything

**Stop. Read this section. Every time.**

1. **Check this doc first.** The problem you're seeing is probably listed below
   with a known fix. Rewriting config from scratch when a one-line change would
   fix it is the #1 cause of agent-introduced regressions.

2. **Verify the actual error.** Read the exact error message. Copy it. Search
   this doc for keywords from it. Don't assume you know the cause — the same
   symptom (e.g., "pane won't open") has at least four different root causes
   listed below.

3. **Make minimal targeted fixes.** Change one thing at a time. If you change
   several current Taarof settings or modules at once, you won't know which
   change fixed (or broke) things. Diff your changes
   before applying — if the diff is more than ~10 lines for a bugfix, you're
   probably doing too much.

4. **Test after each change.** Run the specific command that was failing.
   Confirm it works. Then move on. Don't batch five fixes and hope they all
   land cleanly.

**If the fix isn't in this doc**, add it after you solve the problem. Future
agents (and future you) will thank you.

---

## Historical compatibility: Ghostty, Zellij, and Textual

The following three sections preserve troubleshooting notes for the pre-Taarof stack and optional theme import. They do not describe the shipped Taarof runtime: Taarof owns its GTK window, tabs, panes, keybindings, session state, and sidebar. Use them only when maintaining archived legacy material or a separate Ghostty/Zellij/Textual installation.

### Ghostty Issues

Ghostty is a GPU-accelerated terminal emulator. It handles rendering, window
management, tabs, splits, and keybindings. It does **not** handle sessions,
workspaces, layouts, or agent logic — that's Zellij's job.

**Config location:** `~/.config/ghostty/config` (flat `key = value` format)

| Symptom | Cause | Fix |
|---------|-------|-----|
| Config syntax errors after upgrade | Ghostty is pre-1.0; syntax changes between releases | Check the [changelog](https://github.com/ghostty-org/ghostty/releases) when upgrading. Re-read docs for any changed key names. |
| Font rendering looks off | GPU renderer handles subpixel hinting differently than CPU renderers | Adjust `font-thicken = true` or tweak `font-feature-settings`. Test with `font-family = monospace` to isolate the issue. |
| Wayland vs X11 behavior differs | Some keybinds and IME input methods behave differently under Wayland | Test under `WAYLAND_DISPLAY` — if the variable is set, you're on Wayland. Some X11-era keybind assumptions won't hold. |
| Gray screen on Hyprland | Background opacity value set too low, or wrong async backend | Keep `background-opacity = 1` and set `gtk-single-instance = false`. Use `async-backend = epoll` (see slow rendering fix below). |
| Keybind not working | Conflict with Hyprland or Zellij — one of them captures the key first | Check all three config layers: Hyprland (`~/.config/hypr/hyprland.conf`), Ghostty (`~/.config/ghostty/config`), Zellij (`~/.config/zellij/config.kdl`). Hyprland wins over Ghostty, Ghostty wins over Zellij. |
| Slow rendering on Hyprland | Default async backend is suboptimal for Hyprland's Wayland compositor | Add `async-backend = epoll` to `~/.config/ghostty/config`. This is the single biggest perf fix for Ghostty on Hyprland. |

**Recommended baseline config:**
```ini
font-family = JetBrainsMono Nerd Font
font-size = 13
window-padding-x = 6
window-padding-y = 6
background-opacity = 1
async-backend = epoll
window-inherit-working-directory = false
tab-inherit-working-directory = true
split-inherit-working-directory = true
bell = true
```

---

### Zellij Issues

Zellij handles persistent sessions, pane layouts, tabs, and WASM plugins.
It uses KDL format for configuration.

**Config locations:**
- Main config: `~/.config/zellij/config.kdl`
- Layouts: `~/.config/zellij/layouts/<name>.kdl`
- Plugins: `~/.config/zellij/plugins/`

| Symptom | Cause | Fix |
|---------|-------|-----|
| `Error parsing KDL` | Missing braces, unquoted strings, bad indentation | Validate KDL syntax — check that every `{` has a matching `}`. Use a KDL linter or count braces manually. Common: forgetting `}` after a `keybinds` block. |
| Layout not found | Wrong path or name doesn't match | File must be at `~/.config/zellij/layouts/<name>.kdl`. Launch with `zellij --layout <name>` (name only, no path, no `.kdl`). |
| Pane command not launching | Using `edit` node instead of `command` node | Use `command "bash"` with `args "-lc" "your command"` inside a `pane` node. The `edit` node opens a file in `$EDITOR`, it doesn't run arbitrary commands. |
| Plugin path error | WASM file missing or wrong path | Verify the `.wasm` file exists in `~/.config/zellij/plugins/`. Use `location "file:~/.config/zellij/plugins/foo.wasm"` with the `file:` prefix. |
| Keybind conflict with Ghostty | Ghostty captures the key before Zellij sees it | Use `clear-defaults = true` in the Zellij keybinds block to start clean, then define only what you need. Check Ghostty config for overlapping `keybind` entries. |
| Session not persisting after close | `on_force_close` set to `quit` | Set `on_force_close "detach"` in `config.kdl`. This detaches the session instead of killing it when you close the terminal. |
| KDL v1 vs v2 confusion | Using `#true` instead of `true` | **Zellij uses KDL v1.** Booleans are `true`/`false` without the `#` prefix. KDL v2 uses `#true`/`#false` — don't use that syntax. |
| Can't attach to session | Session name typo or session already exited | Run `zellij list-sessions` to see active sessions. Names are case-sensitive. If the session crashed, it won't appear in the list. |

**Quick validation:**
```bash
# Test layout loading
zellij --layout ~/.config/zellij/layouts/dev.kdl

# Load layout as new tab from inside Zellij
zellij action new-tab --layout ~/.config/zellij/layouts/dev.kdl

# List active sessions
zellij list-sessions
```

---

### Textual Sidebar Issues

The sidebar is a Textual (Python TUI) app running in a fixed left pane in
Zellij. It displays sessions, projects, agents, alerts, and shortcuts.

**Key constraint:** The sidebar reads state from files and Zellij — it does
not manage sessions itself.

| Symptom | Cause | Fix |
|---------|-------|-----|
| Textual version mismatch / API errors | API changes between Textual versions | Pin the version in requirements: `textual>=0.40,<1.0`. Check the [Textual changelog](https://github.com/Textualize/textual/blob/main/CHANGELOG.md) after upgrading. |
| Pane too small / rendering garbled | Pane width below minimum (28 columns) | Set minimum pane width in the Zellij layout: `pane size=28 { ... }`. The sidebar needs at least 28 columns to render properly. |
| Process crashes silently | Zellij doesn't auto-restart crashed pane commands | Wrap in a restart loop: `while true; do python -m agent_terminal.sidebar; sleep 2; done` |
| Alerts not updating in sidebar | Wrong file path — sidebar and alert script disagree | Both must use the same path: `~/.local/share/agent-terminal/alerts.log`. Check the `LOG_FILE` variable in `agent-alert` and the path in `sidebar.py`. |
| `ImportError` on textual or rich | Packages not installed in the right Python environment | Run: `python -m pip install --user textual rich`. Verify with `python -c "import textual; print(textual.__version__)"`. |

---

## Task Panel Issues

| Symptom | Cause | Fix |
|---------|-------|-----|
| Discover Tasks shows taarof/legacy-app tasks in unrelated local tabs | A target repo without mise config fell back to taarof's ambient process CWD, which could persist as the tab's `discovery_cwd` | Keep explicit tab discovery scoped to the focused pane's CWD; only move upward to a real mise-configured ancestor |
| Discover Tasks switches to the PRs view | The configured or remembered Pull Requests mode remained active after discovery | Treat explicit task discovery as a request to select the Tasks view for the active context |

---

## Notification Issues

Current Taarof notifications are sent with `gio::Notification` through the GTK
application when a non-active tab transitions busy → idle while the window is
unfocused. Activating the notification focuses its recorded pane. The optional,
throttled attention sound is a separate `canberra-gtk-play` invocation; the
diagnostics journal records its first launch failure. Taarof does not call
`notify-send`, create `alerts.log`, or ship an `agent-alert` helper.

| Symptom | Cause | Fix |
|---------|-------|-----|
| No desktop popup for a background tab becoming ready | The app was focused, the tab was active, no busy → idle transition occurred, or the desktop notification service rejected delivery | Reproduce with an unfocused window and a non-active agent tab. Then verify the user-session notification service and inspect Taarof's diagnostics journal; `gio::Notification` is the application path. |
| No audible attention sound | `canberra-gtk-play` is unavailable or its 10-second cooldown is active | Install the distribution package that provides `canberra-gtk-play` (`libcanberra` on Arch), then check `~/.local/state/taarof/diagnostics.jsonl` for the recorded launch failure. Silent desktop notifications remain valid. |
| Notification opens the wrong place | The app did not receive the notification activation or the pane no longer exists | Confirm the live tab/pane state with `taarof query-state --pretty`; notification actions target the recorded tab and pane through the app-scoped `focus-pane` action. |

### Historical pre-Taarof `agent-alert` reference

> **Historical / non-primary.** The following `notify-send` / `alerts.log` /
> terminal-bell helper belonged to the Ghostty/Zellij/Textual agent-terminal
> stack. It is retained for maintaining that separate legacy installation and
> does not describe the shipped Taarof runtime.

**Reference implementation for historical `agent-alert`:**
```bash
#!/usr/bin/env bash
set -euo pipefail

TITLE="${1:-Agent Alert}"
MESSAGE="${2:-Agent is waiting for your input.}"
LOG_DIR="${HOME}/.local/share/agent-terminal"
LOG_FILE="${LOG_DIR}/alerts.log"
TIMESTAMP="$(date '+%Y-%m-%d %H:%M:%S')"

# Layer 1: desktop notification
if command -v notify-send &>/dev/null; then
    notify-send "$TITLE" "$MESSAGE" --urgency=normal --icon=dialog-information 2>/dev/null || true
fi

# Layer 2: persistent log
mkdir -p "$LOG_DIR"
echo "[${TIMESTAMP}] [AGENT-ALERT] ${TITLE} | ${MESSAGE}" >> "$LOG_FILE"

# Layer 3: terminal bell
printf '\a'
exit 0
```

---

## Wayland / Omarchy Issues

taarof runs on Omarchy (Arch Linux + Hyprland). This is a Wayland-first
environment. Many X11-era tools and assumptions do not apply.

| Symptom | Cause | Fix |
|---------|-------|-----|
| Clipboard operations fail | Using X11 tools (`xclip`, `xsel`) on Wayland | Use `wl-copy` / `wl-paste` instead. Install with `sudo pacman -S wl-clipboard` if missing. |
| Screenshots broken | Using X11 tools (`scrot`, `import`) | Use `grim` (capture) + `slurp` (region select): `grim -g "$(slurp)" screenshot.png` |
| Hyprland config breaks after upgrade | Syntax changes between Hyprland versions | Check the [Hyprland migration notes](https://wiki.hyprland.org/Configuring/Migration-Changes/) after every upgrade. |
| AUR package breaks after major update | Shared library version mismatch (`.so` files) | Rebuild with `yay -S <package>`. Check AUR comments for known issues before rebuilding. |
| `WAYLAND_DISPLAY` is empty | Not running inside a Wayland session | Ensure you're running inside Hyprland. If SSH'd in, Wayland env vars won't be set — this is expected. |
| Ghostty won't start after AUR update | Binary needs rebuild against new libs | Rebuild: `yay -S ghostty`. If that fails, check AUR comments for patches. |
| Closing a restored background tab freezes GTK and floods the Wayland socket | The direct close callback unparents its own tab row during GTK signal dispatch, while row-owned controllers or per-row sources can retain stale widget state | Defer user-initiated close to the next GLib main-loop turn, keep permanent row callbacks weak, and cancel the row handle's sources before unparenting. Run `testing/kasm/test-restored-background-close.sh` through `container-run.sh` to verify socket, CPU, I/O, fd, and thread stability. |

**Wayland tool equivalents:**
| X11 Tool | Wayland Replacement | Package |
|----------|-------------------|---------|
| `xclip` / `xsel` | `wl-copy` / `wl-paste` | `wl-clipboard` |
| `scrot` / `import` | `grim` + `slurp` | `grim`, `slurp` |
| `dunst` (X11 notifications) | `mako` | `mako` |
| `xdotool` | `wtype` / `ydotool` | `wtype` / `ydotool` |

**Environment validation:**
```bash
[ -n "$WAYLAND_DISPLAY" ] && echo "Wayland: ACTIVE ($WAYLAND_DISPLAY)" || echo "Wayland: NOT DETECTED"
hyprctl version 2>/dev/null && echo "Hyprland: RUNNING" || echo "Hyprland: NOT RUNNING"
command -v wl-copy &>/dev/null && echo "wl-clipboard: OK" || echo "wl-clipboard: MISSING"
command -v notify-send &>/dev/null && echo "libnotify: OK" || echo "libnotify: MISSING"
command -v mako &>/dev/null && echo "mako: OK" || echo "mako: MISSING"
```

---

## Historical: Agent Launch Issues

> **Historical / non-primary.** This covers the pre-`taarof` Zellij "Agent
> Harness" (`agent-run` + `agents.json`), not the shipped runtime — see
> `agent-skills/legacy/agent-launch-skill.md` and `CLAUDE.md` → "Secrets". Do **not**
> store provider credentials in `agents.json`; in the private deployment homelab secrets
> live in Infisical and are injected at launch (`infisical run -- <agent>`).
> The `${VAR}` references below assume the value is already exported into the
> environment at runtime, never written into the committed config.

Agents are launched via `agent-run` which reads `agents.json`, resolves
the binary, and opens a Zellij pane with the right environment.

**Config location:** `~/.config/agent-terminal/agents.json`

| Symptom | Cause | Fix |
|---------|-------|-----|
| `agents.json not found` | Config directory doesn't exist | `mkdir -p ~/.config/agent-terminal` and create the config file. See reference format below. |
| Binary not in PATH | Agent tool not installed or bin directory not on `$PATH` | Install the tool, then verify with `which <binary>`. If installed but not found, add its directory to `$PATH` in your shell profile. |
| Zellij pane not opening | Not inside a Zellij session | `agent-run` detects the `ZELLIJ` env var. If it's not set, you're not inside Zellij. Start a session first: `zellij` or `zellij attach <name>`. |
| Pane opens and closes immediately | Missing API key, missing binary, or command error | Test the command directly first: run `claude --version` or `codex --help` in a normal terminal. Check that required env vars (like `ANTHROPIC_API_KEY`) are set. |
| Conflicting env vars between agents | Env vars leaking between panes | Export env vars inside the pane subprocess, not the calling shell: `zellij action new-pane --cwd "$CWD" -- env KEY=VALUE bash -c "$full_cmd"` |
| TUI agent shows garbled output | TTY not allocated properly | Ensure `zellij action new-pane` isn't piping stdin. Test the agent binary directly to confirm it works in a normal TTY. |

**Reference agents.json:**
```json
{
  "agents": [
    {
      "name": "claude",
      "command": "claude",
      "args": [],
      "env": { "ANTHROPIC_API_KEY": "${ANTHROPIC_API_KEY}" },
      "description": "Anthropic Claude Code"
    },
    {
      "name": "codex",
      "command": "codex",
      "args": ["--full-auto"],
      "env": { "OPENAI_API_KEY": "${OPENAI_API_KEY}" },
      "description": "OpenAI Codex CLI"
    }
  ]
}
```

---

## Historical: Zellij Layout Issues

Zellij layouts are KDL files that define pane arrangement, commands, and
plugin placement.

**Layout directory:** `~/.config/zellij/layouts/`

| Symptom | Cause | Fix |
|---------|-------|-----|
| Sidebar doesn't appear | `sidebar.py` path wrong in layout `command` node | Check that the `command` path in the layout matches the actual location of the sidebar script. Use absolute paths or ensure the module is importable. |
| Quoted strings in KDL break parsing | Nested quotes not properly escaped | KDL uses `"` for strings. If you need literal quotes inside, use `\"`. For complex args, break them into separate `args` entries. |
| Layout loads wrong content | Layout name collision — multiple files with similar names | Check `~/.config/zellij/layouts/` for duplicates. Zellij resolves by name (without `.kdl` extension). |
| Pane starts in wrong directory | `cwd` not set or inherited incorrectly | Set `cwd "/path/to/dir"` explicitly on the `pane` node. Don't rely on inheritance for critical panes. |
| Layout pane sizes don't add up | Percentages or fixed sizes exceed available space | Use `size` as a percentage (e.g., `size="30%"`) or fixed columns (e.g., `size=28`). Ensure sizes leave room for remaining panes. One pane should be unsized to absorb remaining space. |

**KDL v1 syntax quick reference (what Zellij uses):**
```kdl
// Booleans: no # prefix
pane borderless=true

// Strings: quoted
command "bash"
args "-lc" "echo hello"

// Children: curly braces
pane {
    command "python"
    args "-m" "agent_terminal.sidebar"
}

// Comments
// single line
/* multi line */
```

**⚠️ KDL v2 syntax (do NOT use with Zellij):**
```kdl
// WRONG for Zellij:
pane borderless=#true    // v2 boolean syntax
```

---

## Taarof Issues

### HTTP tmux control returns 504 but the pane changes later

Tmux-backed `send-keys` and `resize-pane` can legitimately run until the tmux
worker deadline, so they must not inherit the shorter read/query bridge timeout.
Keep their HTTP wait derived from `TMUX_CONTROL_DEADLINE` plus the bounded
main-context callback margin. The HTTP request guard must atomically choose one
outcome: cancel a mutation that has not started, or let a claimed mutation
return its exact result. Never return 504 after the mutation claims apply;
callers could retry and duplicate the control operation. The focused regressions
are `http_tmux_control_waits_past_the_former_five_second_bridge_limit`,
`http_control_actual_timeout_cancels_late_apply`, and
`http_control_does_not_timeout_after_mutation_claims_apply`.

### Kasm build or runtime cannot resolve Arch mirrors or crates.io

If host DNS succeeds but a bridge-network container cannot resolve names, check
the container's `/etc/resolv.conf`. Some Docker daemon configurations point
containers at the bridge gateway (for example `172.17.0.1`) even though no DNS
resolver listens there. Do not restart Docker and disrupt unrelated workloads
just to run the visual harness. The repo-owned Kasm Compose file builds through
the host network and supplies explicit runtime resolvers; use
`testing/kasm/container-run.sh` for non-visual commands and the documented
Compose desktop for GTK/VTE checks.

### Remote tmux pane probe reports invalid metadata

Tmux 3.7b can replace ASCII control-character delimiters in pane metadata when
the probe crosses a remote shell boundary. New probes use the printable
`__TAAROF_PANE_INFO_V1__` delimiter and retain parser compatibility only with
explicit legacy unit-separator encodings. Mixed, mangled, and ordinary
pipe-delimited metadata fail closed; a `|` inside a path remains valid when the
payload uses a supported delimiter. If the warning persists, compare a manual
remote `tmux display-message -p` probe with `taarof list-tabs --pretty`; verify
SSH access and the target session before changing parser behavior.

### Remote Claude or Codex is active but no agent appears

Taarof deliberately cannot inspect a process after it crosses an SSH boundary.
Do not forward or copy the desktop host's privileged Unix socket as a workaround.
Run `audit-host.sh --ssh HOST`; a missing `integration.remote-agent-emitter`
means that the remote provider has no pane-local status hook. Review the
installer dry run, obtain the remote host owner's approval, then use the
approved installer. The supported signal is OSC 666 written to the remote
process's `/dev/tty`, which SSH delivers to that pane only. Do not install the
GTK app remotely or enable non-loopback HTTP for this purpose.

### Installed build is newer but the running app is stale

After `mise run install`, Linux can keep the old executable inode mapped while
the installed path points to new content. The sidebar then shows **Update** and
`taarof doctor` reports `app.update` as a warning with reason
`running_executable_deleted` or `content_mismatch`.

Review running agents, bound tasks, and session-restore readiness, then restart
Taarof manually. Do not kill panes just to clear the warning. If the status is
`unknown`, inspect `taarof query-state --pretty` for a missing/unreadable path or
hash failure and verify `[update].installed_binary_path` when configured.

### Ctrl-click opens the wrong document path or treats a directory as a file

The desktop VTE matcher needs a dedicated, boundary-aware `~/` path alternative.
Without it, `~/notes.md` can be truncated to `/notes.md` and dotfiles such as
`~/.bashrc` can be missed. Keep sentence-punctuation trimming downstream, and
gate document-match hover, peek, and editor activation on `Path::is_file()`
rather than `exists()` so directories such as `path/.` remain inert while
symlinks to regular files continue to work. OSC-8 URI handling is a separate
surface and is unchanged by this fix.

### Multi-pane tab shows only one agent child row

If multiple local agents remain visible but the left rail and `query-state`
report only one, inspect every `/proc/<wrapper-pid>/task/*/children` file. Linux
attributes a child to the thread that spawned it; reading only the process
leader's `children` file misses agents launched by multithreaded wrappers such
as `infisical`. Agent process-tree discovery must union and deduplicate children
from every task before building the shared per-pane projection.

## Quick Diagnostic Commands

When the shipped Taarof app is not behaving as expected, start with its live
state and configuration:

```bash
taarof query-state --pretty
taarof doctor
taarof config validate
```

### Historical pre-Taarof tool inventory

Use the following only to diagnose a separate legacy Ghostty/Zellij/Textual
installation; these commands do not report Taarof runtime state.

```bash
# System state
echo "Wayland: ${WAYLAND_DISPLAY:-NOT SET}"
echo "Zellij: ${ZELLIJ:-NOT SET}"
echo "Shell: $SHELL"
echo "Term: $TERM"

# Tool availability
for cmd in ghostty zellij python notify-send wl-copy mako grim; do
    command -v "$cmd" &>/dev/null && echo "$cmd: $(which $cmd)" || echo "$cmd: NOT FOUND"
done

# Zellij state
zellij list-sessions 2>/dev/null || echo "Zellij not running or not in PATH"

# Config files exist
for f in ~/.config/ghostty/config ~/.config/zellij/config.kdl ~/.config/agent-terminal/agents.json; do
    [ -f "$f" ] && echo "$f: EXISTS" || echo "$f: MISSING"
done

# Notification test
notify-send "Test" "If you see this, notifications work" 2>/dev/null || echo "notify-send failed"
```

---

## Still Stuck?

1. **Drive the running app** with `agent-skills/legacy-app/SKILL.md` — live state
   from the socket/HTTP API usually beats guessing.
2. **Check the reference docs** in `reference-docs/` — full API/config
   references for Ghostty, Zellij, KDL, Textual, and Arch packages. These
   cover the pre-`taarof` stack; see `agent-skills/legacy/README.md` for what
   still applies.
3. **Add your fix here** once you solve it. This doc only works if we keep it
   current.
