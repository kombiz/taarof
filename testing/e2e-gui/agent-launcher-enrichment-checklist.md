# Launcher enrichment attended checks (EXAMPLE-198 / EXAMPLE-200)

Record the source revision and hashes of the installed `agent` and live Taarof
executable before checking. Use the supported Kasm desktop workflow in
`testing/kasm/README.md`; a headless unit test is not visual acceptance.

- Run a provider with an explicit session identity inside a tmux-backed Taarof
  pane in a named test runtime. Query `agent --session NAME --json`; verify
  the exact provider/host/session reference, fresh workspace/tab/pane IDs,
  and separate Attach and Resume actions where history exists.
- Select Attach in the picker. Verify Taarof presents that exact pane, retains
  the same provider process, and accepts keyboard input. Repeat with another
  workspace active. Record the observed pane and process identity without
  recording prompt content or credentials.
- Stop the test process after displaying its row. Attach must fail with a
  refresh outcome; it must not launch a replacement or choose a neighbor.
- With an existing configured remote test host, observe initial refresh pending,
  then a healthy cached record. Resume must allocate a TTY at the exact SSH
  destination and recorded cwd. Use synthetic provider arguments containing
  spaces/quotes to check transport, never account credentials.
- Make that isolated remote fixture unreachable. Refresh must retain local
  sessions and show the remote error/staleness with no executable remote
  action. Restore the fixture and verify refresh recovers.
- Stop the named runtime. Local catalog and local Resume must still work;
  the missing-runtime diagnostic must be visible. An unknown named runtime
  must not silently select another running runtime.

Until these observations are recorded against installed artifacts, runtime
acceptance remains pending even when CI passes.
