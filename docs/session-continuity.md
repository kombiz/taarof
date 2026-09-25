# Session continuity

Taarof exposes three different continuity actions. Each one requires its own
authority at the moment it runs:

- **Reattach live terminal** reconnects only to an exact current process
  target. Restored tmux panes require the saved tmux server/session identity and
  Taarof's saved random generation. A legacy layout without that generation is
  reported unavailable. Restore never recreates a missing session or silently
  attaches a same-name replacement. Metadata probes, send, resize, and cleanup
  also retain and revalidate that saved generation, so a refused replacement
  cannot become writable merely because it reused the saved name.
- **Resume agent conversation** launches the provider from an exact saved
  provider session identity. The native app delegates to the `agent` binary in
  the same installed channel and verifies that its source identity matches the
  running app before using structured, shell-free arguments.
- **Reopen workspace layout** restores desktop workspaces, tabs, splits, and
  working directories. A saved terminal display checkpoint is visual context;
  it is not evidence that a process or provider conversation is still live.

New and Resume support from a provider does not imply queue, steer, interrupt,
or rewind support. Those capabilities need their own declared actions.

## Lifecycle outcomes

| Event | Reattach live terminal | Resume agent conversation | Reopen workspace layout | Display checkpoint |
| --- | --- | --- | --- | --- |
| Browser viewer disconnect | The browser can reconnect its read-only viewer only while the exact current pane remains in the desktop snapshot. This does not attach a native process. | Copy-only in the browser; the browser never launches a provider. | Unavailable in the browser; the native app owns saved layout state. | The latest observed output may remain visible but does not prove liveness. |
| Desktop shutdown; tmux survives | Available after restart only when the saved tmux identity and owned generation still match. | Available when an exact provider session identity and matching installed launcher remain available. | Restores the saved arrangement. | Restores as visual context. |
| tmux session or server is lost | Unavailable with a concrete missing or replacement-target reason. Taarof does not create a new session during restore. | Independent of tmux; may still be available from provider history. | Restores the arrangement with an unavailable terminal placeholder. | May restore, still without liveness authority. |
| Machine restart | Usually unavailable because the tmux server generation is gone, even when a new server reuses the same name or numeric ID. | Available only if provider history and the exact provider identity survive. | Restores from readable saved state. | May restore, still without liveness authority. |

## Saved-state recovery

If the saved layout is corrupt, unreadable, or belongs to another session
identity, Taarof preserves the original bytes under a collision-safe
`session.json.recovery.*` name and shows that path. If preservation fails,
autosave and shutdown persistence remain blocked for that process so an empty
layout cannot replace the recovery source. A valid intentionally empty layout
loads normally.
