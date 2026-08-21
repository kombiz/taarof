# Release Test Checklist

Use this checklist before calling a build release-ready. It is intentionally
split into validated core flows, high-risk gaps, and platform expectations.

## Platform Expectations

- [ ] Test on Linux with GTK4 and libadwaita
- [ ] Test on a Wayland desktop session
- [ ] Test on an X11 desktop session
- [ ] Confirm required system libraries are installed:
  - `gtk4`
  - `libadwaita`
  - `vte4` / `libvte-2.91-gtk4`
- [ ] Confirm packaging/install validation passes with `bash packaging/linux/validate-local-install.sh`

Expected supported systems today:

- Linux only
- Primary target: GTK4 desktop environments on Wayland or X11
- Distro expectations documented in the README:
  - Arch / Manjaro
  - Fedora 39+
  - Ubuntu 26.04+

Not currently expected to work as a supported release target:

- macOS
- Windows

## Core Release Flows

- [ ] Launch the app from a clean shell session
- [ ] Confirm the first terminal tab appears and accepts input
- [ ] Create a new tab from the sidebar `+ New tab` button
- [ ] Create a new tab from the socket/API surface
- [ ] Rename a tab
- [ ] Split a pane vertically
- [ ] Split a pane horizontally
- [ ] Run a command in an existing pane through the socket API
- [ ] Send raw keys to a pane through the socket API
- [ ] Capture pane text through the socket API
- [ ] Close a split pane and confirm layout recovers correctly
- [ ] Open the dashboard tab
- [ ] Confirm `/health` returns `ok`
- [ ] Confirm `/` serves the browser client from the installed web bundle
- [ ] Confirm `/?token=...` bootstraps browser auth and removes the token from the visible URL
- [ ] Confirm the browser client does not keep token-bearing CacheStorage entries after bootstrap or logout
- [ ] Confirm authenticated `/api/v1/state` works
- [ ] Confirm unauthenticated `/api/v1/state` is rejected
- [ ] Confirm `/api/v1/sessions`, `/api/v1/workspaces`, `/api/v1/tabs`,
      `/api/v1/panes`, and `/api/v1/events` return valid payloads
- [ ] Confirm `/api/v1/events/ws` streams live events with token auth

## tmux and Session Flows

Automated coverage now exercises the headless socket contract for
`detach-pane`, `list-detached`, `query-state`, and `attach-session`, including
the detached-session round-trip state transition. It also verifies that a failed
runtime socket request cannot kill the bridge for later `list-tabs`,
`query-state`, or mutating socket actions. Automated coverage also
exercises the HTTP pane-attach websocket route so an idle tmux pane keeps its
initial ANSI snapshot on the first poll and a same-`pane_id` reattach retargets
the existing browser socket to the new tmux session/host. The checks below are
the remaining live tmux and sidebar/dashboard validation pass.

- [ ] Create a local tmux-backed tab
- [ ] Open the web pane attach view and confirm the initial ANSI/colors persist after the first poll interval
- [ ] For visual/container validation, run `docker compose -f testing/kasm/docker-compose.yml up --build`, then run `bash testing/kasm/start-taarof-visual-smoke.sh` inside the noVNC desktop.
- [ ] In the visual smoke, confirm duplicate VTE `pane_id = 0` tabs (`Workspace-A`, `Workspace-B`) render live, tab-scoped content in the web pane attach view.
- [ ] Confirm `tmux_session` appears in the pane snapshot
- [ ] Confirm dashboard session state reflects the tmux session
- [ ] With `close_behavior = "close"` (default), closing a tmux-backed tab shows the Kill / Detach / Cancel confirmation naming the session and host; Kill removes the session, Detach leaves it running and registers it in BACKGROUND, Cancel does nothing
- [ ] With `close_behavior = "detach"`, closing a tmux-backed tab shows no dialog and toasts that the session was detached and keeps running
- [ ] Enabling "tmux-backed workspace" toasts that new tabs/splits inherit tmux backing; disabling toasts they run as plain shells
- [ ] Creating a tmux tab over SSH (or when taarof itself is inside tmux) warns about nesting, and the tab row shows the `⚠ nested` tmux label
- [ ] Close the tmux-backed tab and confirm cleanup
- [ ] Detach a tmux-backed pane
- [ ] Confirm detached session appears in the BACKGROUND section
- [ ] Confirm detached session appears in `list-detached`
- [ ] Reattach the detached session
- [ ] Reattach a different tmux target into the same `pane_id` and confirm the browser follows it without a refresh
- [ ] Confirm detached session disappears from BACKGROUND and `list-detached`

## Remote and Host Flows

Automated coverage now locks the serialized remote snapshot fields
(`cwd_host`, `remote_shell`, `tmux_host`) plus the remote host probe parser in
`cargo test`. The checks below remain the live SSH and remote tmux operator
pass.

- [ ] Open an SSH-backed tab
- [ ] Confirm `cwd_host` is populated for remote OSC 7 paths
- [ ] Confirm `remote_shell` flips to `true` when SSH is active
- [ ] Confirm `has_child_process` is `null` for an SSH pane before, during, and
      after a remote command (remote process state is intentionally unknown)
- [ ] Create a tmux-backed tab on a configured remote host
- [ ] Confirm `tmux_host` is populated in pane snapshots
- [ ] Confirm `has_child_process` remains `null` for the remote tmux pane even
      if best-effort `remote_shell` detection is false
- [ ] Confirm dashboard host/session data reflects the remote target
- [ ] Verify host-status polling and recovery behavior

## Workspace and Persistence Flows

Automated coverage now round-trips a realistic multi-workspace session through
disk state, including per-workspace active tab indexes, worktree metadata,
detached sessions, and the background-section collapse state. The
right-click "Move to Workspace" workflow additionally has a single named
regression anchor,
`right_click_move_tab_regression_moves_visible_row_between_workspaces`
(run `cargo test --manifest-path taarof-app/Cargo.toml right_click_move_tab_regression`),
plus the operator smoke `testing/smoke-move-tab-to-workspace.sh` for the live
GUI action. The checks below remain the live restart and UI validation pass.

- [ ] Create multiple workspaces
- [ ] Switch between workspaces
- [ ] Verify active tab is preserved per workspace
- [ ] Prove the right-click **Move to Workspace** workflow with `bash testing/smoke-move-tab-to-workspace.sh` (needs >=2 workspaces and >=2 tabs; confirm the moved tab leaves the source, lands under the target, and active workspace/tab follow it)
- [ ] Exercise worktree-backed workspace creation
- [ ] Close the last tab in a worktree workspace and confirm workspace removal behavior
- [ ] Restart the app and verify session restore
- [ ] Confirm restored tabs, panes, active tab, and workspace state are correct
- [ ] Verify detached session restore behavior if applicable

## Templates, Views, and Agent Flows

Automated coverage now validates persisted saved views/templates through
config round-trips and the read-only `query-state` surface. `cargo test` also
covers the headless `agent-workspace` state path. The two-distro release gate
runs credential-free Claude, Codex, Pi, and Kimi process/signature and explicit
activity-lifecycle scenarios. Real provider prompts remain opt-in through
`mise run release:e2e:live-agents` after either isolated LiteLLM configuration
or dedicated test-account authentication; they are never part of ordinary CI.
The checks below are the remaining
live-instance and manual UI/UX validation pass.

- [ ] Create and reopen a saved dashboard view through the palette/UI
- [ ] Create and reopen a workspace template through the palette/UI
- [ ] Run `bash testing/test-agent-workspace.sh` against a live taarof instance
- [ ] Review the script output and confirm it passed create, reuse, command, and cleanup checks
- [ ] Open a Claude Code tab and confirm the sidebar state transitions through `idle -> running -> waiting-input -> idle` within the normal refresh interval
- [ ] Run a Claude Code Teams teammate-mode session in a taarof pane and confirm the activity indicator shows teammate work as `running` and teammate completion as `done`
- [ ] Review `diagnostics*.jsonl` after the teammate-mode smoke and confirm the Claude hook handling is clean with no unknown-event or hook-error noise
- [ ] Open a tab with multiple agents (two same-kind, e.g. two Codex, plus one other) and confirm each pane shows a stable disambiguated label (`codex #1`, `codex #2`), the tab row tooltip lists every agent+pane+state, and a waiting-input on one agent raises a notification naming that specific agent and pane whose activation focuses the correct tab and pane; confirm repeated scans do not re-fire the same notification. The web Monitor shows the same per-pane agents distinctly.

## Input and UX Flows

Automated coverage now freezes the default release shortcut matrix and the
terminal-scoped pane action bindings. The checks below remain the manual
keyboard and UX validation pass.

- [ ] Open and use the command palette
- [ ] Open **Keyboard Shortcuts** from the palette and confirm it lists every action with its current trigger, reflects a `keybindings.toml` override (flagged `(customized)`), and shows `unbound` for unbound actions
- [ ] Open a fresh/empty workspace (no tabs) and confirm the next-action hints appear with current keybindings, then vanish once a tab is created
- [ ] Exercise dashboard-related palette commands
- [ ] Exercise tmux-related palette commands
- [ ] Exercise detach/attach palette commands
- [ ] Verify double-click rename for tabs
- [ ] Verify sidebar workspace collapse/expand behavior
- [ ] Verify attention and alert indicators
- [ ] Verify selection mode toggle
- [ ] Verify plain drag visibly selects and copies terminal text inside mouse-reporting TUIs without printing mouse escape sequences
- [ ] Verify `Shift`+drag still uses VTE's native selection behavior
- [ ] Verify `Ctrl`/`Alt`+drag passes mouse input through to mouse-aware TUIs
- [ ] Verify plain drag also visibly selects and copies while persistent `Ctrl+Shift+S` selection mode is active
- [ ] Verify copy/paste shortcuts
- [ ] Verify `Ctrl+Shift+C` / `Ctrl+Shift+V` still work before and after a plain-drag selection gesture
- [ ] Verify pane focus shortcuts
- [ ] Verify `Ctrl+Click` still opens plain visible `http(s)` URLs after plain-drag selection use. For every OSC 8 hyperlink, confirm the first activation opens only a destination-disclosure menu, the menu shows the full target, `Escape`/click-away opens nothing, and selecting the disclosed target opens it exactly once. Repeat in the browser terminal (ordinary click) and with a desktop `file://` OSC 8 link, which must not preview or launch before confirmation.
- [ ] Verify leader-mode shortcuts if they are enabled for release

## Mobile Web Flows

- [ ] Open the browser client on a phone-sized viewport and confirm the navigator becomes a drawer
- [ ] Confirm workspace and tab switching remain usable on the small-screen layout
- [ ] Confirm the mobile view keeps one pane visible at a time and pane switching remains tappable
- [ ] Confirm the mobile view stays read-only by default
- [ ] Confirm the bearer-token bootstrap flow still works after clearing mobile browser storage
- [ ] Confirm older service-worker registrations are removed by the browser client

## Regression Checks

- [ ] Run `bash testing/smoke-move-tab-to-workspace.sh` against the installed
      `~/.local/bin/taarof-app` (or an explicitly supplied release-equivalent
      binary) and confirm it prints `PASS` for the right-click Move-to-Workspace
      workflow. The script isolates its own `TAAROF_SESSION`, prints the tested
      binary + sha256, and requires the operator to perform the two GUI steps
      (create a 2nd workspace, then right-click a named tab -> Move to Workspace
      -> target) because the socket protocol cannot yet script those (EXAMPLE-79).
      This must pass each release so the workflow stays proven, not tribal
      knowledge. Backed in CI by the named unit regression
      `right_click_move_tab_regression_moves_visible_row_between_workspaces`.
- [ ] Confirm pane process-state fields in `/api/v1/state` update immediately when child processes start and stop
- [ ] Confirm pane process-state fields in `/api/v1/panes` update immediately when SSH appears and disappears
- [ ] Force a probe into a degraded state and leave it there for multiple poll cycles
- [ ] Confirm `probe_failures` does not keep increasing for an unchanged degraded state
- [ ] Restore the degraded dependency and confirm exactly one recovery transition is recorded
- [ ] Verify health returns to `ok` after recovery

## URL Link Regression Checks

- [ ] Watch stderr during tab/pane creation and confirm no `vte_terminal_match_add_regex(...)` multiline warning appears
- [ ] Confirm wrapped plain `http(s)` URLs still show a pointer cursor and open with `Ctrl+Click` in the desktop app
- [ ] Confirm wrapped plain `http(s)` URLs are clickable in the browser client terminal surface

## Sign-off

- [ ] Dispatch `Release` with `workflow_dispatch` from the candidate branch,
      passing the lowercase full immutable candidate SHA as `commit`; confirm
      its pre-build validation requires `commit == GITHUB_SHA == HEAD`
- [ ] Record the exact candidate SHA, rehearsal run URL/conclusion, and
      `taarof-linux-x86_64` artifact metadata (name, size, expiry) on the
      candidate PR or release sign-off; metadata inspection must not require
      downloading the artifact
- [ ] Confirm the rehearsal uploaded the tarball, checksum, and
      `taarof-linux-x86_64.tar.gz.intoto.jsonl` provenance bundle together.
      If GitHub-native attestation is unavailable for this private repository,
      stop: do not tag or substitute a public provenance log without an
      explicit privacy/platform decision.
- [ ] Confirm the rehearsal created neither a tag nor a GitHub Release
- [ ] Treat the April 9, 2026 manual validation in `docs/release.md` as expired
      release evidence; it is historical coverage only
- [ ] Update `docs/release.md` with the latest validation date and scope
- [ ] Attach or reference screenshots/logs for the release candidate
- [ ] Record any unvalidated areas in the release notes
