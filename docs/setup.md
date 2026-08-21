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

Without `mise`, run the underlying commands:

```bash
cargo build --release --manifest-path taarof-app/Cargo.toml
npm --prefix taarof-web ci
npm --prefix taarof-web run build
bash packaging/linux/install-local.sh
```

The installer writes only under the selected prefix (default `~/.local`) and
does not start or enable the optional remote-control gateway.

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
- OSC 133 marks prompts so copy-recent-output and prompt navigation are exact.

For Bash:

```bash
mkdir -p ~/.config/taarof
install -m 0644 taarof-app/resources/osc7.bash ~/.config/taarof/osc7.bash
install -m 0644 examples/taarof-shell-integration.sh ~/.config/taarof/taarof-shell-integration.sh
printf '\n%s\n' \
  'source "$HOME/.config/taarof/osc7.bash"' \
  'source "$HOME/.config/taarof/taarof-shell-integration.sh"' >> ~/.bashrc
```

For Zsh, replace `osc7.bash` and `.bashrc` with `osc7.zsh` and `.zshrc`.
Before appending, check that each `source` line is not already present. Fish
needs native functions; the example at the top of
`examples/taarof-shell-integration.sh` contains the supported Fish equivalent.

Open a new shell and verify that changing directory updates the pane's CWD.

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
4. Install the OSC 7 and OSC 133 shell files from the checkout exactly as in
   the local shell-integration section.

Copy the integration files without modifying the remote shell yet:

```bash
ssh HOST 'mkdir -p ~/.config/taarof'
scp taarof-app/resources/osc7.bash \
    examples/taarof-shell-integration.sh \
    HOST:.config/taarof/
```

Then inspect the remote rc file, add the two idempotent `source` lines, and open
a new SSH session. For Zsh, copy `osc7.zsh` instead.

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
