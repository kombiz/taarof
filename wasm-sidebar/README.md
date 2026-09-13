# Experimental Zellij sidebar

This Rust/WASM plugin belongs to the Zellij experiment. The shipped desktop
app is `../taarof-app/`; its installed Python CLI is `../taarof-cli/taarof`.
Neither depends on this plugin. The separate `../examples/taarof` Bash launcher
loads it from `$XDG_CONFIG_HOME/zellij/plugins/taarof-sidebar.wasm` (default
`~/.config/zellij/plugins/taarof-sidebar.wasm`). Do not install that launcher
as a replacement for the desktop CLI.

CI still compiles this experiment. Build support is not evidence of active
users or a desktop runtime dependency. Retention is pending a consumer review;
no archival or deletion has been approved.

## Build and optional installation

From this directory, with Rust and the `wasm32-wasip1` target installed:

```bash
./build.sh
```

`./install.sh` explicitly copies the resulting plugin to
`~/.config/zellij/plugins/taarof-sidebar.wasm`. It does not honor
`XDG_CONFIG_HOME`; for a custom config root, copy the artifact to the launcher's
configured path yourself. Desktop installation does not run this script.

The plugin uses `zellij-tile` and requests command execution and Zellij state
permissions. Its configuration defaults refer to the older `agent-terminal`
agent/project/alert files (`src/state.rs`). Those are separate from the GTK
app's configuration and session store. This document labels the existing
behavior; it does not certify a live Zellij session.
