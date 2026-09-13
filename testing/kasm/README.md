# Isolated native verification

Install Docker and Compose, then from the repository root:

```bash
docker compose -f testing/kasm/docker-compose.yml up --build -d
bash testing/kasm/container-run.sh 'bash testing/kasm/start-taarof-visual-smoke.sh'
```

The disposable desktop is available at http://127.0.0.1:6901/. Both published
ports bind to loopback. Never use `docker compose run` for builds: use the helper
above, which runs as the desktop user with a separate cargo target volume.
The visual fixture replaces only its container test session. Do not run it on
your regular desktop. Bearer tokens are not printed in logs or URLs.

In the desktop, check the two Workspace-A/Workspace-B tabs, live VTE output,
selection/copy, and tab-scoped browser attach. Authenticate the loopback web
client with the private runtime token file inside this disposable container;
never copy that value into screenshots, reports or shared commands.

The installed-helper probe uses its own application and private HOME/XDG and
checks real VTE CWD plus exact command-output copy (including prompt marks):

```bash
bash testing/kasm/container-run.sh 'python3 testing/kasm/test_shell_integration_native.py --prefix /config/.local --evidence /tmp/shell-evidence'
```

Focused native regressions:

```bash
bash testing/kasm/container-run.sh 'DISPLAY=:1 bash testing/kasm/test-restored-background-close.sh'
bash testing/kasm/container-run.sh 'DISPLAY=:1 bash testing/kasm/test-sidebar-width.sh'
```

The descriptor-boundary probe takes an explicitly built binary and an evidence
directory; see `python3 testing/kasm/test_pty_descriptor_boundary.py --help`.
All these checks require a real GTK display. Headless Rust tests do not replace
visual acceptance. The public core gate is `mise run ci`; shell checks need Bash,
Zsh and Fish installed. HTTP parity checks use `protocol/openapi.yaml` operations
marked `loopback-http`; other operations remain staged protocol documentation.

Stop the fixture and disposable desktop:

```bash
bash testing/kasm/container-run.sh 'bash testing/kasm/start-taarof-visual-smoke.sh --stop'
docker compose -f testing/kasm/docker-compose.yml down
```

## Unread PTY input

`test_pty_input_responsiveness.py` checks GTK heartbeat and state-query latency,
sibling keyboard input, pane close, and exact partial-delivery cancellation
while an inert raw PTY child does not read. Run it in a disposable nonroot Kasm
container with no published ports. Supply an already-built binary explicitly;
it records that binary and running-process hashes. Compile the test-only timer
library in the same container:

```bash
cc -shared -fPIC -Wall -Wextra $(pkg-config --cflags glib-2.0) \
  testing/kasm/gtk_input_timer_probe.c $(pkg-config --libs glib-2.0) \
  -o /tmp/gtk-input-timer.so
dbus-run-session -- python3 testing/kasm/test_pty_input_responsiveness.py \
  --binary /config/.local/bin/taarof-app \
  --timer-library /tmp/gtk-input-timer.so --evidence /tmp/input-evidence
```

The fixture starts its own display and private HOME/XDG application. Its HTTP
control binds only to container loopback and its generated bearer stays in
memory for request headers. Run native probes sequentially to avoid display
collisions. Headless tests alone do not establish these runtime results.

## Native output backpressure and stalled observers

In the same disposable nonroot, no-published-port container, compile the
output helper and run against an explicitly installed binary:

```bash
cc -shared -fPIC -Wno-deprecated-declarations \
  $(pkg-config --cflags vte-2.91-gtk4) testing/kasm/gtk_output_probe.c \
  -o /tmp/gtk-output-probe.so $(pkg-config --libs vte-2.91-gtk4) -ldl
python3 testing/kasm/test_pty_output_delivery.py \
  --binary /config/.local/bin/taarof-app \
  --probe-library /tmp/gtk-output-probe.so --evidence /tmp/output-evidence
```

The test-only read gate pauses consumption while preserving the live VTE PTY.
It verifies GTK liveness, exact delivery after resume, final output before EOF,
paused pane cleanup, a presentation read error, and stalled web observers.
Only inert fixture output is used; token handling stays in request headers.
The separate [subscriber memory probe](../../docs/terminal-checkpoints.md#subscriber-memory-accounting)
reports bounded subscriber copies and replay, with model growth explicitly
outside that bound. Record its executable hash separately from the app binary.
