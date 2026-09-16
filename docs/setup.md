# Taarof setup guide

This guide covers four supported setup shapes:

| Goal | Install Taarof where? | Configure what? |
| --- | --- | --- |
| Use the desktop app on this computer | The local graphical Linux host | App, CLI, shell integration, optional tmux |
| Use Taarof on another graphical workstation | Each graphical Linux host | Repeat the desktop installation on that host |
| Run shells and tmux sessions on remote servers | Only the desktop host needs Taarof | SSH, tmux, and shell integration on each remote |
| Control one desktop from another device | The owner desktop plus the optional gateway | Loopback APIs, gateway, private HTTPS, and device pairing |

Taarof is a Linux GTK application. macOS and Windows can be SSH clients or
remote-control clients, but they cannot run the shipped desktop app.

For a guided setup, invoke the repo-local `$setup-taarof` agent skill. It asks
for the desired topology, audits the local computer and named remotes without
changing them, presents a per-host plan, obtains approval for writes, and runs
the verification matrix after installation.

## 1. Choose an installation method

Use a release archive when you only want to run Taarof. Use a source checkout
when you want the latest development build or plan to contribute.

### Release archive

The release archive contains the desktop binary, Python CLI, desktop metadata,
icon, and web bundle. It requires Python 3.11 or newer but does not require Rust
or Node.js. It still requires the host's GTK4, libadwaita, GtkSourceView 5, and
VTE GTK4 runtime libraries; installing the corresponding packages from the
native package table below satisfies both release and source installs.

```bash
sha256sum -c taarof-linux-x86_64.tar.gz.sha256
tar -xzf taarof-linux-x86_64.tar.gz
./taarof-linux-x86_64/install.sh
```

The default prefix is `~/.local`. Ensure `~/.local/bin` is on `PATH`, then run:

```bash
taarof-app
```

### Source checkout

Install the native libraries first:

| Distribution | Native packages |
| --- | --- |
| Arch/Manjaro | `sudo pacman -S --needed base-devel git python gtk4 libadwaita gtksourceview5 vte4 sqlite` |
| Fedora 39+ | `sudo dnf install gcc gcc-c++ make pkgconf-pkg-config git python3 gtk4-devel libadwaita-devel gtksourceview5-devel vte291-gtk4-devel sqlite-devel` |
| Ubuntu 26.04+ | `sudo apt install build-essential pkg-config git curl python3 libgtk-4-dev libadwaita-1-dev libgtksourceview-5-dev libvte-2.91-gtk4-dev libsqlite3-dev` |

Python must be 3.11 or newer. You also need a recent Rust toolchain and Node.js
LTS. If the checkout is trusted by `mise`, the repository's standard path is:

```bash
mise trust mise.toml
mise run setup
mise run install
```

`mise run install` builds the current checkout in release mode, builds the web
client, records the build's source provenance, and installs the result. Re-run
it after pulling to install the latest version, then restart Taarof. Use
`mise run dev` to run a debug build from the checkout without installing it.

Without `mise`, run the underlying commands:

```bash
cargo build --release --manifest-path taarof-app/Cargo.toml
cargo build --release --manifest-path agent-launcher/Cargo.toml
npm --prefix taarof-web ci
npm --prefix taarof-web run build
bash packaging/linux/emit-artifact-provenance.sh taarof-app/target/release/taarof-app
bash packaging/linux/install-local.sh
```

The provenance step records which commit the binary was built from. It fails on
a tree without usable Git metadata, such as an unpacked source archive; the
install still works, and the app then reports its source identity as unknown.

The installer writes only under the selected prefix (default `~/.local`) and
does not start or enable the optional remote-control gateway.

### Contributor verification

After installing the native packages above, run `mise run ci` for the full
local gate. It installs locked web dependencies, builds the web bundle, checks
the optional performance harness, and runs native, CLI, and frontend tests.
Pull-request CI also compiles the harness and runs the CLI regression suite.

For standalone native tests in a fresh checkout, build the web assets first:
`runtime_smoke` exercises the web-asset resolver and requires `taarof-web/dist`.

```bash
npm --prefix taarof-web ci
npm --prefix taarof-web run build
cargo test --manifest-path taarof-app/Cargo.toml
python3 taarof-cli/test_taarof_cli.py
cargo check --manifest-path taarof-app/Cargo.toml --example performance_harness --features harness
```

These are headless checks. Use the Kasm desktop and its visual checklist in
`testing/kasm/README.md` for GTK/VTE interaction evidence.

## 2. Create and validate local configuration

The app works with built-in defaults when `config.toml` is absent. Generate the
commented starter file with the installed CLI:

```bash
taarof config path
taarof config default --write
taarof config validate
```

`taarof config default --write` refuses to overwrite an existing file. Back up
an existing setup before making changes:

```bash
cp -a ~/.config/taarof ~/.config/taarof.backup-$(date +%Y%m%d-%H%M%S)
```

Edit `~/.config/taarof/config.toml`, then validate again. Taarof validates and
reloads app configuration changes live. Appearance, sidebar behavior, future
actions, and SSH/tmux host entries update without disrupting attached panes.
Keybinding, imported Ghostty, HTTP-listener, and other startup-owned changes
still require a reload or restart. See [configuration.md](configuration.md) for
every supported key, type, default, and parameter.

## 3. Install shell integration locally

Taarof reads terminal escape sequences; it does not poll the shell for its
working directory. Install both integrations:

- OSC 7 reports the current host and directory.
- OSC 133 marks prompts for terminal interoperability. The helper also sends
  VTE's `OSC 666;vte.shell.precmd!` signal, which Taarof uses for exact
  copy-recent-output and prompt navigation.

Source builds and tarball/local installs deliver the same helpers. Installation
never edits your shell startup files. Select the source below:

```bash
# Installed package (replace ~/.local if you chose another install prefix):
shell_dir="$HOME/.local/share/taarof/shell"
# From a source checkout instead, copy the helpers to your own directory:
# mkdir -p "$HOME/.config/taarof/shell"
# cp taarof-app/resources/osc7.* examples/taarof-shell-integration.* "$HOME/.config/taarof/shell/"
# shell_dir="$HOME/.config/taarof/shell"
```

Add these two lines once to `~/.bashrc`, after checking for existing entries:

```bash
source "$HOME/.local/share/taarof/shell/osc7.bash"
source "$HOME/.local/share/taarof/shell/taarof-shell-integration.sh"
```

Adjust those paths to your selected directory. For Zsh use `osc7.zsh` in
`~/.zshrc`, followed by the same `.sh` marker helper. For Fish use native files
in `~/.config/fish/config.fish`:

```fish
source "$HOME/.local/share/taarof/shell/osc7.fish"
source "$HOME/.local/share/taarof/shell/taarof-shell-integration.fish"
```

Repeated sourcing preserves existing prompt hooks without duplicate registration.
Bash emits command-start C through PS0 (Bash 4.4+) without replacing a DEBUG
trap; Zsh and Fish use native preexec hooks. Prompt tracking in Taarof requires
the VTE signal in addition to the standard OSC133 markers. Open a new shell and verify
that changing into a directory with spaces or Unicode updates the pane's CWD.

Contributor verification requires Bash, Zsh and Fish installed:
`python3 testing/test_shell_integration.py` exercises real interactive shells and
copied remote payloads in private temporary homes. To verify an installed payload,
set `TAAROF_TEST_SHELL_DIR=/your/prefix/share/taarof/shell` for that command.

## 4. Optional local features

### Durable tmux-backed panes

Install `tmux`, then use this safe preset:

```toml
[tmux]
enabled = true
session_prefix = "taarof"
close_behavior = "detach"
```

`detach` leaves work running when a Taarof pane closes. The default, `close`,
kills the backing tmux session after a confirmation. See
[tmux-integration.md](tmux-integration.md) for the complete lifecycle.

### Local browser view

Keep the listener on loopback:

```toml
[http]
enabled = true
port = 7800
bind_address = "127.0.0.1"
unsafe_allow_non_loopback = false

[http_control]
enabled = false
```

Restart Taarof, then run `taarof doctor`. The token under
`$XDG_RUNTIME_DIR` is a runtime secret; do not copy it into config files, shell
history, issue comments, or documentation. Enable `[http_control]` only when the
local browser needs the narrow write surface.

### Task panel and loop runner

Enable task discovery only if repositories on this host use `.plan/tasks.json`:

```toml
[tasks]
enabled = true
pull_requests = false
default_view = "tasks"
```

The loop-runner buttons additionally require a configured external runner and a
repo-local `.loop/loops.yaml`. Keep them disabled when no runner is installed.

### Registered projects and Coder

Focus a pane inside a GitHub-backed checkout and press `Ctrl+Alt+P` to register
it. Taarof resolves local or bounded remote Git identity, writes the
schema-versioned `~/.config/taarof/projects.json`, and adds a `Project:` entry
to the command palette. tmux-backed panes preserve their local or remote tmux
open mode. Registration stores repository/host/path metadata only—never GitHub
credentials, SSH keys, or raw probe output.

Coder projects use an installed and already-authenticated `coder` CLI. Add a
`coder` binding to the generated project record and set `preferred_open` to
`"coder"` when that repository should open in Coder. Taarof runs
`coder list --output json`, connects with
`coder ssh`, and only runs `coder create` when `create_if_missing` is explicitly
true and a template is configured. Authentication remains owned by Coder; do
not place its token in `projects.json`.

## 5. Verify the local installation

With the app running:

```bash
taarof config validate
taarof doctor
taarof list-tabs --pretty
bash packaging/linux/validate-local-install.sh  # source checkout only
```

Useful identity checks on the normal development workstation are:

```bash
pid=$(jq -r .pid "$XDG_RUNTIME_DIR/taarof-current.json")
readlink -f "/proc/$pid/exe"
sha256sum "/proc/$pid/exe" ~/.local/bin/taarof-app
```

Do not claim that a build is running merely because it exists on disk; the
`/proc/<pid>/exe` path and hash identify the live process.

## 6. Set up an SSH/tmux remote host

This mode keeps the GTK app on the desktop and runs selected panes on a Linux
server such as `build.example`, `agents.example`, or `development.example`.

### Remote prerequisites

On each remote:

1. Install and enable an SSH server according to that host's operating system.
2. Install `tmux`, `bash` or `zsh`, `procps`, and `git` when branch display is
   wanted.
3. Use SSH keys. Taarof background probes use `BatchMode=yes` and cannot answer
   password, passphrase, or host-key prompts.
4. Install the OSC 7 and OSC 133 shell files from your source or installed payload as in
   the local shell-integration section.

Copy the integration files without modifying the remote shell yet:

```bash
ssh HOST 'mkdir -p ~/.config/taarof'
scp "$shell_dir/osc7.bash" "$shell_dir/taarof-shell-integration.sh" \
    HOST:.config/taarof/
```

Then inspect the remote rc file, add the two idempotent `source` lines, and open
a new SSH session. For Zsh, copy `osc7.zsh` instead. For Fish, copy
`osc7.fish` and `taarof-shell-integration.fish` and use the Fish source lines.
These steps copy public helper files only; they do not install credentials.

### Local SSH configuration

Prefer a stable SSH alias or Tailscale hostname:

```sshconfig
Host build.example
    HostName build.example
    User YOUR_REMOTE_USER
    IdentityFile ~/.ssh/id_ed25519
    IdentitiesOnly yes
```

Verify non-interactive access before adding the host to Taarof:

```bash
ssh -o BatchMode=yes -o ConnectTimeout=5 build.example \
  'command -v tmux && test -n "$HOME" && printf REMOTE_OK'
```

### Taarof host entry (optional, but not for agent-session discovery)

Connecting needs no `[hosts.*]` entry. Every concrete `Host` alias in
`~/.ssh/config` is already offered by the tmux-tab dialog, the palette's
`Connect: <host>` entries, and the `create-tmux-tab` socket verb, so the SSH
configuration above is enough to open a remote tab.

A `[hosts.<label>]` entry is optional tuning — policy metadata, or a local name
that differs from the SSH target — with one exception worth knowing before you
skip it: **remote agent-session discovery does not read `~/.ssh/config`.** Its
host inventory is exactly the `[hosts.*]` entries that carry an `address`, plus
any host with a live taarof tmux session. So keep or add an entry for every
host whose agent sessions you want `taarof agent sessions` to discover; an
ssh_config alias alone will never be probed.

Add one block per such remote to local `~/.config/taarof/config.toml`:

```toml
[tmux]
enabled = true
close_behavior = "detach"

[hosts.control]
address = "build.example"
max_sessions = 8
warn_cpu_percent = 80
warn_memory_percent = 80
idle_detach_minutes = 120
tmux_backed = true
```

The table name (`control`) is Taarof's label. `address` is the exact argument
passed to `ssh`, so it may be an SSH alias, `user@host`, or a `.ts` hostname.
After restarting Taarof:

```bash
taarof config validate
taarof doctor --probe-hosts
```

Create a remote tmux-backed tab and confirm:

- the sidebar reports `build.example:~/path`, not a local path;
- `taarof tmux ls` identifies the correct host;
- closing with `close_behavior = "detach"` leaves the remote session in
  `ssh build.example tmux list-sessions`;
- reattachment selects the exact `(session, host)` pair.

Plain SSH panes restore best-effort. Use remote tmux when process continuity
across app restarts or network interruptions matters.

### Remote agent activity

Remote agents can report lifecycle state through pane-local OSC 666 sequences.
Keep tool-specific lifecycle hooks on the remote host and never copy the local
runtime socket, registry, or bearer token across machines.

## 7. Install the desktop app on a remote workstation

If the other machine is a graphical Linux workstation, connect to its desktop
session and repeat Sections 1 through 5 on that machine. Do not launch the GTK
app through an unattended SSH shell unless display/session environment is
already available. Each workstation owns separate files under its own
`~/.config/taarof`, `~/.local/share/taarof`, and `$XDG_RUNTIME_DIR`.

Use a named session when two independent Taarof instances run for the same Unix
user:

```bash
TAAROF_SESSION=work taarof-app
taarof --session work list-tabs
```

Always let Taarof derive namespaced state and registry paths; do not construct
the hashed filenames manually.

## 8. Remote-control boundary

The v0.1.x public release does not ship a remote-control gateway. Keep
the HTTP API on loopback and follow the root `SECURITY.md` guidance.

## 9. Upgrade and rollback

Before an upgrade, back up:

```text
~/.config/taarof/
~/.config/ghostty/config
~/.local/share/taarof/session*.json
```

Re-run the chosen installer, then use the **Update → Reload into update** action
or restart Taarof manually before repeating the validation and live-binary
checks. The reload action saves the pane layout, reconnects tmux panes, and
runs saved resume commands for detected non-tmux agents in the active tab that
is restored eagerly. Tabs restored lazily later follow
`[session] auto_resume_agents`; ordinary non-tmux shell processes do not
survive. For rollback, stop Taarof, restore the previous
binary and configuration, and relaunch. If the gateway is enabled, re-pin its
`runtime.instance_id` after every Taarof restart as described in the remote
terminal runbook.

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

The launcher requires an existing absolute `HOME` for local history. An absent
store is reported as unavailable; unreadable, malformed, or over-budget stores
are reported as degraded. JSONL samples admit at most 64 lines, 1 MiB per line,
and 16 MiB total; the Kimi index admits at most 10,000 records within the same
byte budget. Pi's session map is capped at 1 MiB. Valid files remain visible
when another file is corrupt, but exact resume refuses a degraded provider
until a fresh scan succeeds. An incomplete write can therefore temporarily
prevent resume; retry after the provider finishes writing.

### Rust workspace and reproducible artifacts

The desktop app, shared session core, and standalone launcher share the root
`Cargo.toml` workspace and `Cargo.lock`. Keep their dependency identity relative
to this common workspace: independent sibling crates acquire checkout-dependent
Cargo metadata even when Rust source paths are remapped. The gateway and WASM
experiment remain separate packages with their own lockfiles.

Run Cargo from the repository root. `.cargo/config.toml` keeps shipped artifacts
in `taarof-app/target`, including `release/agent`; `CARGO_TARGET_DIR` overrides
that directory for isolated/container builds. Installed names remain
`bin/taarof-app`, `bin/taarof`, and `bin/agent`. Packaging explicitly selects the
build directory and uses the root lockfile.

The public-export validator compares both executables, provenance, and entire
release bundles across independent exports. Failed validation retains its
scratch directory and reports its path for inspection.

### Complete bundle source identity

`agent --build-info` prints compile-time `agent.build.v1` identity without
reading provider history or contacting Taarof. The launcher and desktop share
the same build-provenance implementation; installed filenames alone never
establish matching source.

A package includes `share/taarof/bundle-manifest.json` with schema
`taarof.bundle.v1`, the compiled `source_revision`, explicit `source_dirty`,
`app_build_id`, `agent_build_id`, and SHA-256 values for `taarof-app`, `taarof`,
and `agent`. Generation verifies the desktop sidecar against its executable,
checks the launcher's own embedded identity, and compares CLI bytes with the
tracked blob at that exact source revision for clean builds. Dirty builds remain
packageable and installable with `source_dirty: true`; their hashes establish
artifact integrity, but they are unreviewed and do not prove exact committed
source. Desktop and launcher must agree on source, dirty state, profile and epoch.
The installer checks the manifest
before copying programs and verifies the installed bytes before copying it.

Release acceptance and the installed smoke require a clean committed checkout. A local
development install can still run when complete provenance is unavailable,
but reports that limitation and removes any previous complete bundle manifest.
The existing desktop `install-manifest.json` remains available separately.
### Keyboard session picker

Bare `agent` opens a local-first picker. New lists executable providers with a
declared new-session capability. Sessions from the current repository and its
linked worktrees come first, then other sessions, newest-message first within
each group. Ctrl+S also offers global time order and Active/Recent groups,
where Active requires an exact live binding and Recent
lists saved history. A stopped Taarof process does not prevent
local New and Recent use. `agent --new` opens only New rows; `agent --resume`
opens session rows.

| Key | Behavior |
| --- | --- |
| Type / Backspace | Fuzzy search provider, title, repository, directory, host and session name |
| Up / Down | Move selection |
| Enter | Run the declared default: New, or Attach when available for an exact live row, otherwise Resume |
| Tab | Cycle local, all hosts and active-only scope |
| Ctrl+N | Clear filters and focus New rows |
| Ctrl+P | Cycle provider filter, including all providers |
| Ctrl+R | Refresh while preserving exact selection |
| Ctrl+S | Cycle repository-first, global last-message order and Active/Recent groups |
| Ctrl+F | Fork only when the selected row declares that capability |
| ? | Toggle keyboard help |
| Esc / Ctrl+C | Exit without launching |

Attach uses the existing live process; Resume launches the provider using saved
history; New starts a new session; Fork creates a separate session from history.
Unsupported actions have no key action. Preview content is bounded metadata,
action meaning, confidence and degraded-source warnings. It excludes transcript
bodies and execution arguments. Labels cannot emit terminal control sequences.

A disappeared selection stays unselected until you choose again. Before launch,
the picker obtains a fresh catalog and requires the exact identity, capability
and structured plan to match. A changed or unavailable action stays in the picker
with an explanation. Terminal modes are restored before handing over to the
provider and when cancellation or execution fails.

The picker displays **Last sent**: the timestamp of your latest message to the
agent, in local time with its UTC offset. New actions remain at the top.
Changing the sort does not change the selected session.

Repository matching uses the Git common directory, so sessions in linked
worktrees and other branches of this checkout belong together. Separate clones
and nested repositories remain separate. A remote path is never matched against
a local repository merely because the text looks the same. Outside a Git
repository, the default falls back to global time order. Missing or unreadable
repository metadata stays in the other/unknown group; discovery uses bounded
local metadata reads without spawning Git per row.

Local Claude, Codex, and Pi history supplies message timestamps. Discovery reads
at most the final 1 MiB of each selected history file, independently of the title
sample. Assistant output, tool results, and file modification times do not count
as messages sent by you. If the newest message has no valid timestamp, the tail
is incomplete, or no user message is found within that budget, the time is
`unknown`. Kimi, OpenCode, remote history, and external adapters currently also
show `unknown`. Unknown times sort after known times, with history-update time
used only to order ties. Refresh with Ctrl+R to pick up newly sent messages.
