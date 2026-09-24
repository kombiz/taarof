# Local Socket and HTTP API

taarof exposes a same-user local Unix socket with both privileged control
actions and a read-only query subset. Its opt-in HTTP server has a bearer-token
protected observation surface plus a deliberately narrow, mutating subset that
is available only when the listener is loopback-bound and the separate control
gate is enabled.

This API is the intended foundation for future HTTP/WebSocket access
(see issue #49 and the Phase 3 roadmap in
`docs/superpowers/plans/2026-03-29-tmux-hybrid-integration.md`).

## Trust model

The Unix socket is a privileged local control surface, not a sandboxed
RPC boundary.

- Any process running as the same user and able to connect to the socket
  can create tabs and panes, execute shell commands, inject raw
  keystrokes, and capture pane text.
- The runtime directory ownership and `0700`-style permissions are the
  primary access control. Protect `$XDG_RUNTIME_DIR` and do not share it
  across trust boundaries.
- If you need a lower-privilege integration surface, use the HTTP API described
  below. It is read-only by default; its narrow write surface requires a
  separate loopback-only control gate.

## Query endpoints vs imperative actions

The Unix socket supports two categories of messages:

| Category | Examples | Side effects |
|----------|----------|-------------|
| **Imperative actions** | `create-tab`, `switch-tab`, `run-in-pane`, `send-keys` | Mutate state |
| **Query endpoints** | `query-state`, `query-events`, `query-history`, `query-agent-sessions`, `list-tabs` | Read-only snapshots |

Query endpoints never modify workspace/control state and never emit events into
the event store. `query-events` records only retention-health metadata when it
observes a stale consumer cursor. The endpoints are safe to call at any
frequency from dashboards, scripts, or future HTTP bridges.

The installed `taarof` CLI is a thin client over these same surfaces. For
example, `taarof list-tabs` sends `{"action":"list-tabs"}` to the Unix socket,
and `taarof query-state` / `taarof query-events` map directly to the
read-only messages documented below.

## Endpoints

### query-state

Returns a versioned snapshot of the full taarof runtime state.

```json
{"action": "query-state"}
```

Response schema: `taarof.state.v1`

Top-level fields:

| Field | Type | Description |
|-------|------|-------------|
| `schema` | string | Always `"taarof.state.v1"` |
| `generated_at_unix_ms` | number | Snapshot timestamp |
| `session_name` | string \| null | taarof session identifier; `null` means the unnamed/default session |
| `active_workspace` | number | Active workspace ID |
| `active_tab` | number \| null | Active tab ID in active workspace |
| `capabilities` | object | Feature flags (`events`, `agent_jobs`, `history`, `update_watch`) |
| `health` | object | Live degraded-state summary for probes and event retention |
| `update` | object | Cached running/installed executable identity and advisory update state |
| `diagnostics` | object | Durable local diagnostics metadata and recent records |
| `workspaces` | array | All workspaces with nested tabs and panes |
| `active_ports` | array | All listening ports across tabs |
| `alerts` | array | Tabs currently needing attention |
| `recent_alerts` | array | Recent alert events from the event store |
| `saved_views` | array | Persisted dashboard saved views from `~/.config/taarof/views.json` |
| `saved_views_error` | string \| null | Load failure for saved views; null when the snapshot loaded successfully |
| `saved_templates` | array | Persisted tab/workspace templates from `~/.config/taarof/templates.json` |
| `saved_templates_error` | string \| null | Load failure for saved templates; null when the snapshot loaded successfully |
| `agent_jobs` | array | Active agent sessions across all tabs |
| `dashboard` | object | Dashboard sessions and host stats |
| `detached_sessions` | array | Detached tmux sessions |
| `events` | object | Event cursor metadata (`high_watermark`, `next_seq`, `stored`, `capacity`, `dropped`) |
| `history` | object | In-memory SQLite writer status: availability, enabled state, schema version, per-space durable sequence, queue depth, lag, drops, last commit, state, bounded reason, and the storage/maintenance snapshots below. No database query is performed. |
| `work_ledger` | object | Bounded session ledger records plus immutable observation provenance, current reconciliation overlays, persisted view preferences, and restore health |
| `work` | object | Complete `taarof.work-stream.v1` native Work projection for web Monitor parity: effective filter/palette, legend, chronological entries, overflow count, and reconciliation state |

`history.storage` is refreshed by the writer after maintenance and contains
`main_bytes`, `wal_bytes`, `soft_max_bytes`, `page_count`, `free_pages`,
`record_count`, `oldest_record_ts_unix_ms`, and
`newest_record_ts_unix_ms`. `history.maintenance` contains
`last_run_at_unix_ms`, `next_run_at_unix_ms`, `last_result` (`never`, `ok`,
`pending`, or `failed`), `rows_removed_last`, `rows_removed_total`,
`duration_ms`, `consecutive_failures`, and `last_error`. These are cached writer
metrics; serving `query-state` does not query SQLite.

The health component is degraded when history is `misconfigured`, unavailable,
or its latest maintenance result is `failed`. The component error identifies
the invalid setting or maintenance operation that needs attention.

Work records keep their observation-time `evidence_source`, `authority`, and
`verification` unchanged. The separate `reconciliation` object reports
`pending`, `verified`, `stale`, or `unverified`, the canonical source checked,
current safe value when known, origin availability, and a bounded reason. An
agent `finished` report therefore remains observational and cannot mark a task
done; `.plan/tasks.json` stays canonical. Likewise, retained PR facts are
reconciled by exact GitHub repository and PR number without rewriting history.

`work.restore` and `work_ledger.restore` make missing legacy history, rejected
partial records, corrupt files, and unavailable persistence explicit while
startup continues. Named-session application IDs, layouts, ledgers,
registries, diagnostics, and CLI lookup use a bounded 48-byte ASCII slug plus
the first 128 bits of SHA-256 over the trimmed session name's UTF-8 bytes.
Persisted layouts
also retain and validate that normalized name independently of the digest, so
names that sanitize alike cannot share bindings or work history. The former
32-bit FNV-derived named paths are not auto-migrated because colliding names
cannot be attributed safely; only older slug-only layout files whose raw name
was already path-safe are imported unambiguously. A named process's Unix socket
uses the fixed-size `taarof-s-<128-bit-digest>-<pid>.sock` basename and rejects
an overlong full runtime path before binding; clients discover it through the
digest-keyed registry.

The persisted ledger schema is `taarof.work-ledger.v3`. It migrates v1/v2
records and also retains bounded transcript-message, file-operation, and exact
PR observation watermarks. Clearing visible history preserves these watermarks,
including across restart, so the next poll cannot recreate deleted observations.

Dashboard tmux probes only target tmux hosts that are in use by a configured
remote host, a tmux-backed workspace/pane, or a detached session. Missing local
tmux is therefore not a degraded health condition when no active runtime feature
depends on it; failures for expected local or remote tmux targets still surface
as dashboard health degradation.

#### Update object

`update` has schema `taarof.update.v1`. `state` is `current`,
`update_pending`, or `unknown`; `reason` is null or one of
`running_executable_deleted`, `content_mismatch`, `build_id_mismatch`,
`installed_binary_missing`, `installed_unreadable`, `running_unreadable`,
`hash_failed`, and `unsupported_platform`.

`running` and `installed` each report the selected path, canonicalized path,
symlink/deleted flags, byte size, SHA-256, optional GNU build id, readability,
and a short fixed error code. `content_matches` is populated whenever both
hashes are available, including the deleted-but-byte-identical case.
`checked_at_unix_ms` is the completed worker timestamp. `restart_required` is
true only for `update_pending`, and `restart_policy` is always `operator`.
These fields contain no environment values, file contents, or arbitrary
filesystem search results.

`health.update` carries only `state`, `reason`, and `checked_at_unix_ms`.
Update status is informational: it is not included in degraded components, so
an installed update does not change `health.state` from `ok`.

#### Workspace object

| Field | Type | Description |
|-------|------|-------------|
| `id` | number | Workspace ID |
| `name` | string | Display name |
| `active_tab` | number | Active tab ID within workspace |
| `collapsed` | bool | Sidebar collapsed state |
| `repo_root` | string \| null | Git repository root |
| `branch_name` | string \| null | Current branch |
| `is_worktree` | bool | Whether this is a git worktree workspace |
| `run_status` | string | `"idle"`, `"running"`, or `"errored"` |
| `tmux_backed` | bool | Whether workspace uses tmux inheritance |
| `host_config_name` | string \| null | Remote host config name |
| `tab_count` | number | Number of tabs |
| `tabs` | array | Tab objects |

#### Tab object

| Field | Type | Description |
|-------|------|-------------|
| `tab_id` | number | Tab ID |
| `name` | string | Display name |
| `kind` | string | `"terminal"` or `"dashboard"` |
| `focused_pane` | number | Focused pane ID |
| `agent_running` | bool | Whether the tab has fresh running activity; process detection alone is not enough |
| `agent_name` | string \| null | Name of the primary reporting agent, falling back to the primary detected process when no pane has activity |
| `agent_session_id` | string \| null | Provider session identifier for that primary agent when taarof can determine it safely |
| `agent_pane_id` | number \| null | Stable pane ID for that primary agent |
| `agent_activity` | object \| null | Primary per-pane activity from socket, VTE termprop, or output scan |
| `agents` | array | Per-pane agent observations, including stable `pane_id`, disambiguated `instance_label`, identity, session, and activity |
| `needs_attention` | bool | Alert flag |
| `notification_msg` | string \| null | Notification message |
| `listening_ports` | array | Port numbers this tab is listening on |
| `panes` | array | Pane objects |

When present, `agent_activity.state` is one of `idle`, `running`,
`waiting-input`, `errored`, or `done`. The primary observation is selected from
per-pane activity (alerts before fresh running work before completion); if no
pane has activity, the tab-level identity falls back to the detected agent
process. Consumers that display multiple agents should use `agents` rather than
assigning the aggregate to the focused pane.

#### Pane object

| Field | Type | Description |
|-------|------|-------------|
| `pane_id` | number | Pane ID |
| `shell_running` | bool | Whether a shell process exists |
| `has_child_process` | bool \| null | Whether a local pane root has a local child process; `null` when `remote_shell` is true or `tmux_host` identifies a remote tmux target because local `/proc` cannot observe commands beyond SSH |
| `remote_shell` | bool | Whether an SSH session is detected |
| `cwd` | string \| null | Current working directory |
| `cwd_host` | string \| null | Remote hostname from OSC 7 |
| `tmux_session` | string \| null | Backing tmux session name |
| `tmux_host` | string \| null | SSH target for remote tmux |
| `attach_supported` | bool | Whether the browser client can attempt a live pane attach |
| `attach_kind` | string | `"tmux"` for supported tmux-backed panes, otherwise `"unsupported"` |
| `cols` | number \| null | Current visible terminal width when known |
| `rows` | number \| null | Current visible terminal height when known |
| `transcript` | object \| null | Live agent transcript summary when a transcript resolves (see below) |

`has_child_process` is deliberately local-only. The local runtime can inspect
`/proc` for a local pane, but an SSH client does not expose the remote shell's
process tree. Terminal output timing is not a reliable foreground-command
lifecycle signal, and remote tmux metadata is available only for tmux-backed
panes. `remote_shell` is best-effort, so an authoritative remote `tmux_host`
also forces `null` even when that probe flag is false. Remote panes therefore
use `null` rather than a misleading `false`. Consumers
must treat `null` as unknown, not idle or complete; agent activity and tmux
probe fields remain independent signals when available.

#### Pane transcript object

Present under a pane's `transcript` field once taarof resolves a live agent
transcript for that pane. All fields are derived on-disk from the agent's own
transcript, so they are ground truth rather than screen-scraped.

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | Provider session identifier the transcript was folded from |
| `message_count` | number | Total assistant messages folded so far |
| `updated_at_unix_ms` | number | Wall-clock of the last fold that changed anything |
| `last_message` | string \| null | Last assistant message text (raw markdown) |
| `files_touched` | array of strings | Every file the agent referenced via a tool (including reads), deduped, oldest-first |
| `recent_files` | array of objects | Files the agent created or edited, deduped by path keeping the latest operation, oldest-first (see below) |
| `recent_tool_calls` | array of objects | Recent tool calls as `{ tool, target }`, oldest-first |

Each `recent_files` entry is `{ "path": string, "op": "write" | "edit", "at_unix_ms": number }`,
where `op` is `write` for created files and `edit` for edited files.

#### Agent job object

| Field | Type | Description |
|-------|------|-------------|
| `workspace_id` | number | Workspace containing the agent |
| `workspace_name` | string | Workspace display name |
| `tab_id` | number | Tab containing the agent |
| `tab_name` | string | Tab display name |
| `pane_id` | number \| null | Pane currently running the agent when known |
| `agent_name` | string \| null | Agent identifier |
| `session_id` | string \| null | Provider session identifier when taarof can determine it safely |
| `activity` | object \| null | Activity with `state`, `text`, `source`, `origin` |

#### Detached session object

| Field | Type | Description |
|-------|------|-------------|
| `session_name` | string | Detached tmux session name |
| `host` | string | Human-readable host label |
| `workspace` | string | Workspace name that detached the session |
| `ssh_target` | string \| null | Remote SSH target for detached remote tmux sessions |
| `last_command` | string \| null | Last non-shell tmux command observed before detach |
| `finished` | bool | Whether taarof observed the detached session finish |
| `is_detached` | bool | Always `true` for entries in `detached_sessions` |

#### Saved view object

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Saved view display name |
| `preset` | string | Preset identifier such as `recent-alerts` |
| `limit` | number \| null | Stored per-view limit override |
| `effective_limit` | number | Runtime-applied limit after defaults/clamping |
| `requires_unimplemented_data` | bool | Whether the preset depends on data taarof does not capture yet |

#### Saved template object

`saved_templates` is a flat array of lightweight template summaries.

Tab template fields:

| Field | Type | Description |
|-------|------|-------------|
| `kind` | string | Always `"tab"` |
| `name` | string | Template display name |
| `tab_name` | string | Restored tab label |
| `cwd` | string \| null | Stored tab cwd |
| `discovery_cwd` | string \| null | Stored discovery cwd |
| `pane_count` | number | Number of persisted panes in the template |

Workspace template fields:

| Field | Type | Description |
|-------|------|-------------|
| `kind` | string | Always `"workspace"` |
| `name` | string | Template display name |
| `tab_count` | number | Number of tabs in the template |
| `active_tab_index` | number | Stored active tab index |
| `tab_names` | array | Ordered tab names restored by the template |

### query-events

Returns a paginated slice of the in-memory event feed.

```json
{"action": "query-events", "since_seq": 0, "limit": 100}
```

Response schema: `taarof.events.v1`

| Field | Type | Description |
|-------|------|-------------|
| `schema` | string | Always `"taarof.events.v1"` |
| `since_seq` | number \| null | Requested cursor |
| `limit` | number | Applied limit (clamped to 1-500) |
| `next_seq` | number | Resume cursor for the next page |
| `high_watermark` | number | Highest sequence in the store |
| `oldest_seq` | number \| null | Lowest retained sequence (null when store is empty) |
| `gap` | bool | Whether the requested cursor is missing retained history |
| `gap_from` | number \| null | First missing sequence, or null when there is no gap |
| `gap_to` | number \| null | Last missing sequence, or null when there is no gap |
| `resnapshot_required` | bool | Whether the consumer must refresh full state before applying deltas |
| `capacity` | number | Maximum records retained in the in-memory ring |
| `dropped` | number | Cumulative records evicted by ring rotation |
| `last_dropped_at_unix_ms` | number \| null | Most recent ring-eviction timestamp |
| `events` | array | Event records |

**Stale cursor detection:** A gap exists exactly when the caller supplied
`since_seq` and `since_seq + 1 < oldest_seq`. When `gap` and
`resnapshot_required` are true, refresh full state via `query-state`; the
inclusive missing range is `gap_from` through `gap_to`. A cursor immediately
before `oldest_seq` is contiguous and does not count as a gap.

Each event record:

| Field | Type | Description |
|-------|------|-------------|
| `seq` | number | Monotonic sequence number |
| `ts_unix_ms` | number | Timestamp |
| `event_type` | string | Event type identifier |
| `payload` | object | Event-specific data |

Work consumers bootstrap from `query-state.data.work`, remember the independent
EventStore cursor, then apply `work_recorded`, `work_reconciled`,
`work_preferences_changed`, and `work_ledger_cleared` deltas. More than 25
reconciliations from one emission are represented by one `work_reconciled_batch`
plus the first 25 genuine `work_reconciled` transitions. Consumers must
resnapshot because the remaining per-record deltas are omitted.
A work record's ledger-local `work_seq` is not an EventStore `seq`; never
interchange the two. Clear events contain only scope, stable pane origin when
applicable, and count, never deleted summaries or transcript data.

`work_reconciled_batch.payload` is bounded reconciliation metadata:

| Field | Type | Description |
|-------|------|-------------|
| `total` | number | Reconciliation transitions represented by the batch |
| `by_status` | object | Counts keyed by `pending`, `verified`, `stale`, or `unverified` |
| `work_seq_min` | number | Lowest affected ledger-local sequence |
| `work_seq_max` | number | Highest affected ledger-local sequence |
| `per_entity_emitted` | number | Number of following per-entity transitions (currently 25) |
| `per_entity_omitted` | number | Number of transitions represented only by the summary |
| `truncated` | bool | Always true; some per-entity payloads were intentionally omitted |
| `resnapshot_required` | bool | Always true; refresh `query-state.data.work` |

`work_recorded.payload` is a self-contained Work entry delta:

| Field | Type | Description |
|-------|------|-------------|
| `work_seq` | number | Ledger-local sequence for later reconciliation deltas |
| `record` | object | Immutable Work record and stable workspace/tab/pane origins |
| `marker` | string | Stable pane marker (`P1`-`P5` or `OVF`) |
| `color_slot` | number \| null | Effective five-pane color slot |
| `reconciliation` | object | Initial reconciliation state at emission time |
| `current_target` | object \| null | Current numeric workspace/tab/pane target plus all stable origins, resolved at emission time; clients must not focus using numeric IDs retained in `record.identity` |

For focus, resolve the record's stable tab and pane origins against the latest
`query-state.data.work.legend` and use that live legend row's current workspace,
tab, and pane IDs. A null or no-longer-matching target must fail closed.

**Pagination:** Use `next_seq` from the response as `since_seq` in the next call
to page forward. If a response reports a gap, resnapshot before applying its
events. When `events` is empty and `next_seq == high_watermark`, you are caught
up.

### query-history

Returns a keyset-paginated page from the optional sanitized SQLite history
store. The socket listener performs this read directly; it never crosses the
GTK bridge and, like other query endpoints, does not emit an event.

```json
{
  "action": "query-history",
  "since_id": 1200,
  "limit": 100,
  "record_type": "work",
  "from_ts": 1784500000000,
  "task": "EXAMPLE-133",
  "authority": "plan_canonical",
  "verification": "canonical_file",
  "severity": "warn",
  "text": "blocked",
  "order": "desc",
  "scan_budget": 20000
}
```

Response schema: `taarof.history.v1`. The envelope contains `since_id`, the
clamped `limit` (`1..500`), `next_id`, `has_more`, `scanned`,
`scan_exhausted`, `truncated`, echoed `filters`, and `records`. Supported
filters are `from_ts`, `to_ts`, `record_type`, `session`, `workspace`, `pane`,
`task`, `repository`, `authority`, `verification`, and `severity`. Severity is
one of `info`, `warn`, or `error`; event and Work rows have a null level and are
therefore excluded when severity is set.

`order` is `asc` (the backward-compatible default) or `desc`. Pagination
resolves `since_id` to its `(ts_unix_ms, id)` key and advances with a symmetric
`>` or `<` composite cursor in the requested order; it never uses `OFFSET` or a
full-result temporary sort. A cursor whose record has already been pruned fails
closed so clients can restart from a fresh page instead of receiving duplicates.

`text` performs a case-insensitive secondary match over the already-sanitized
`summary` and `subtype` columns only. It never searches `attrs`. The indexed
candidate scan is bounded by `scan_budget` (default `20000`, maximum `100000`)
per page. `scanned` reports candidates examined, `scan_exhausted` says the
budget stopped the page while candidates remain, and `next_id` resumes after
the last examined candidate even when no rows matched. `truncated` reports that
a requested limit or budget exceeded its supported maximum.

Queries are cancellable through SQLite's interrupt handle. Native filter
changes interrupt the superseded worker; aborting or disconnecting an HTTP
request interrupts its blocking query worker. A cancelled query returns the
bounded `history query cancelled` error and never changes storage.

History contains allowlisted metadata only. It never stores terminal content,
typed input, transcript bodies, environment values, bearer tokens, SSH secrets,
or raw credential-bearing argv. SQLite history is not a task or PR authority.

### query-agent-sessions

Returns a read-only catalog of live agent bindings plus recent historical
sessions discovered from local agent stores, and — when remote hosts are
configured or backing live tmux panes — from those hosts over SSH.

```json
{"action":"query-agent-sessions"}
```

Default response schema: `taarof.agent-sessions.v1` (unchanged).

Opt into the additive shared contract with
`{"action":"query-agent-sessions","schema":"agent.sessions.v2"}` or
`GET /api/v1/agent-sessions?schema=agent.sessions.v2`. Unknown schema names are
rejected (HTTP 400). Both versions use the same cached discovery and retain the
existing socket and HTTP trust boundaries.

V2 returns `schema`, `generated_at_unix_ms`, `providers`, `sessions`, and
`remote_hosts`. Each session has a structured `stable_ref` containing
`provider_id`, canonical `host_identity`, and the opaque `session_id`. Host DNS
case and a final dot normalize; callers must supply canonical host identity,
not a display alias. Taarof uses the local kernel hostname and its existing
configured SSH destination (preserving remote username case). SSH aliases are
not resolved: different configured destinations remain distinct. Renaming a
display label or moving cwd does not change
identity. Only exact stable refs deduplicate; active records sort first, then
newest update time. V1's historical cwd-based live hint is preserved for v1 but
does not become exact live authority in v2.

Render only the v2 `display` fields (`provider`, `session_id`, `host`, `title`,
`cwd`, optional `repo_root`), bounded to 256 Unicode scalar values with terminal
escapes, controls, line breaks, and bidi controls removed. Provider/host warnings,
errors, and live-binding labels receive the same treatment. `stable_ref` and
structured action arguments are machine data, not safe UI labels. V2 titles use
provider/session metadata, because legacy titles can contain prompt excerpts;
legacy v1 titles remain unchanged.

V2 also includes `state`, `source`, `confidence`, timestamps, `warnings`, an
optional exact `live_binding`, and `actions`. Each action carries `kind`,
`transport`, `program`, `argv`, `cwd`, optional structured `remote` destination,
and `confirmation`. Remote plans require the existing explicit SSH target;
a host display label alone never authorizes a remote action. Degraded remote
source warnings stay visible. Resume plans require confirmation; this task adds
no executor or attach implementation.

The `agent-session-core` path dependency owns the existing local parsers,
provider status, discovery cache, normalization, and planning. It has no GTK,
VTE, application-state, HTTP, or Unix-socket dependency. Its built-in registry
exposes versioned metadata and discovery/planning traits. Copilot retains its
existing history-unavailable status. The legacy `resume_command` is copy/display
metadata only: automatic restoration passes structured argv to the existing
spawn seam; manual resume encodes structured words into the already-running
idle shell without reading or parsing that metadata.

The following tables describe the default **v1** payload.

Top-level fields:

| Field | Type | Description |
|-------|------|-------------|
| `schema` | string | Always `"taarof.agent-sessions.v1"` |
| `generated_at_unix_ms` | number | Snapshot timestamp |
| `providers` | array | Per-provider discovery status |
| `sessions` | array | Recent normalized agent sessions |
| `remote_hosts` | array | Per-host remote discovery status. **Additive and omitted entirely** when no remote host is configured or live, so a local-only payload is unchanged from earlier releases |

#### Provider status object

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Provider identifier such as `claude`, `codex`, `pi`, `opencode`, `copilot` |
| `ok` | bool | Whether discovery completed without an internal scan failure |
| `history_available` | bool | Whether historical session scanning is supported on this host |
| `warning` | string \| null | Non-fatal availability note, such as Copilot history being unavailable |
| `error` | string \| null | Fatal scan error for this provider |
| `session_count` | number | Number of returned recent sessions for this provider |

#### Agent session object

| Field | Type | Description |
|-------|------|-------------|
| `agent` | string | Provider identifier |
| `session_id` | string | Provider session identifier |
| `title` | string | Human-facing title for the session |
| `cwd` | string | Session working directory |
| `host` | string \| null | **Additive.** Source host name for a session discovered over SSH. Absent — not `null`, the key is omitted — on every local record, so local payloads are unchanged |
| `repo_root` | string \| null | Repository root when known. Always absent for remote records: a remote `cwd` is never probed against the local filesystem |
| `started_at_unix_ms` | number \| null | Session start time when known |
| `updated_at_unix_ms` | number | Last observed update time |
| `status` | string | `"active"` when mapped to a live taarof tab, otherwise `"recent"` |
| `live_binding` | object \| null | Current taarof workspace/tab/pane target when the mapping is deterministic |
| `resume_command` | string \| null | Shell-ready command string to resume the session manually |
| `resume_unavailable_reason` | string \| null | Explanation when resume is not available |

#### Live binding object

| Field | Type | Description |
|-------|------|-------------|
| `agent` | string | Provider identifier |
| `session_id` | string \| null | Live provider session identifier when known |
| `cwd` | string \| null | Current live cwd used for fallback matching |
| `workspace_id` | number | taarof workspace ID |
| `workspace_name` | string | taarof workspace name |
| `tab_id` | number | taarof tab ID |
| `tab_name` | string | taarof tab name |
| `pane_id` | number | taarof pane ID running the agent |

#### Remote host status object

| Field | Type | Description |
|-------|------|-------------|
| `host` | string | Configured host name, or the ssh target when the host is only known from a live remote tmux pane |
| `ssh_target` | string | The ssh target exactly as configured |
| `ok` | bool | Whether the most recent probe of this host completed |
| `stale` | bool | Whether these records are **degraded** rather than merely cached — see "Freshness" below. A healthy host refreshed on schedule reports `false` |
| `error` | string \| null | Why the last probe failed, when it did |
| `session_count` | number | Remote sessions currently reported for this host |
| `dropped_lines` | number | Record lines that exist on the host but are not represented in `sessions` — over the per-line cap, or not valid JSON. Non-zero means this host is under-reported |
| `truncated_files` | number | Record files whose per-file byte budget trimmed the sample early. The session-bearing line always survives; what is trimmed are the extra lines sampled for a title |
| `warning` | string \| null | Plain-language summary of any under-reporting, absent when none |
| `observed_at_unix_ms` | number \| null | When these records were observed; absent until a probe succeeds |

#### Remote discovery

Remote enumeration covers hosts taarof already has a reason to reach: hosts
configured with an `ssh_target`, plus hosts backing a live remote tmux pane or a
detached remote tmux session. Per host, per refresh window, taarof runs **one**
bounded, read-only command over `ssh -o BatchMode=yes -o ConnectTimeout=5`. It
lists the newest 20 record files per provider under the same
`~/.claude/projects`, `~/.codex/sessions`, `~/.pi/agent/sessions` and
`~/.kimi-code` roots the local scan uses, and returns only paths, mtimes and the
first few JSONL lines of each — never environment values, never whole
transcripts. Parsing happens locally with the same parsers the local scan uses.

Record lines are returned **whole or not at all**. A line over the per-line cap
is skipped and counted in `dropped_lines` rather than truncated, because a
truncated JSONL line is invalid JSON and would silently destroy the record
instead of reporting a loss. Remote discovery samples fewer lines per file than
the local scan does, so a remote session may fall back to a generated title
where the local one would have a real one; any such loss is counted in
`truncated_files` and described in `warning`.

#### Freshness, and the first query after startup

Remote results are served from a cache that a background refresh fills, so a
slow or unreachable host never delays or drops the local snapshot. Two separate
intervals govern this:

- A refresh round is kicked when the cached round is older than the **20s**
  refresh interval — the same window as the catalog TTL.
- Records stay `stale: false` for a **90s** freshness window.

The window is deliberately wider than the interval. A healthy host sits past the
refresh interval for most of its life, so equating the two would report every
host stale in steady state and `stale` would stop meaning anything. `stale` is
`true` only when the host is genuinely degraded: the last probe failed, no probe
has completed yet, or the last good round has aged past the freshness window
(several missed refreshes).

**Consequence at startup:** the first `query-agent-sessions` after taarof starts
returns **no remote sessions** — the first round has not landed yet. That is not
"this host has no agents": each configured host still appears in `remote_hosts`
with `stale: true`, `session_count: 0` and a `warning` saying it has not been
probed yet. Remote records appear from the next query after the first round
completes.

When a probe fails, the host's section keeps its last good records and reports
`ok: false`, `stale: true` and an `error`. Every local record is returned as
normal in all of these cases.

Remote `resume_command` values are host-qualified:

```
ssh -t <ssh target> 'cd <quoted cwd> && <provider resume>'
```

for example `ssh -t gpu-box.ts 'cd /srv/project && claude --resume abc123'`. The
inner command is escaped as a single shell word, so paths and session ids
containing spaces or quotes survive intact.

`query-agent-sessions` is read-only like the other query surfaces. It does not
run agent commands or resume sessions on behalf of callers; `resume_command` is
metadata for copy/paste workflows only. That holds for remote records too — the
only command taarof itself runs on a remote host is the read-only enumeration
above.

### Event retention and durable diagnostics

taarof now exposes two complementary retention layers:

- The event feed is still a rolling in-memory ring buffer. Its capacity is fixed at 512 records.
- Normal ring rotation evicts the oldest record and increments cumulative
  `dropped_total`; historical eviction alone is not degraded health.
- `query-state.data.health.events` always exposes cumulative dropped and cursor-gap
  counters, last-observed timestamps, drops in the 60-second rate window, and
  the active `abnormal_rate` / `active_cursor_loss` flags.
- Event retention is degraded only when a consumer gap was observed in the last
  120 seconds or at least one full ring capacity was dropped within 60 seconds.
  Health returns to `ok` automatically after those windows clear while cumulative
  counters remain queryable.
- `query-state.data.events.dropped` and `query-events.data.dropped` remain
  compatibility counters, and `last_dropped_at_unix_ms` records the most recent
  eviction.
- Durable operator diagnostics are written to a local JSONL log at
  `~/.local/state/taarof/diagnostics[-session].jsonl` when the platform exposes a state dir
  (falling back to the taarof data dir when needed).
- The durable log is rotated at 256 KiB with a single `.jsonl.1` archive. `query-state.data.diagnostics`
  reports the active log path, retention policy, recent records, and durable counters.

### list-tabs

Lightweight workspace/tab/pane listing without ports, alerts, events,
or dashboard data.

```json
{"action": "list-tabs"}
```

## Related tmux socket actions

The local Unix socket also exposes tmux-specific control helpers that are
useful alongside the read-only query surfaces.

### list-detached

Returns the detached tmux sessions that taarof is explicitly tracking.

```json
{"action":"list-detached"}
```

Each entry includes the same detached-session identity fields documented in
`query-state.data.detached_sessions`, including:

- `session_name`
- `host`
- `ssh_target`

`ssh_target` is the authoritative selector for remote tmux sessions. Local
sessions return `null`.

### attach-session

Reattaches a tracked detached tmux session as a new pane/tab.

Minimal request:

```json
{"action":"attach-session","session_name":"taarof--default--t7--0"}
```

If multiple detached sessions share the same `session_name` across different
hosts or SSH targets, taarof rejects the request as ambiguous instead of
guessing. In that case, call `list-detached` first and retry with either
`host` or `ssh_target`:

```json
{"action":"attach-session","session_name":"taarof--default--t7--0","ssh_target":"builder@ci-box"}
```

`host` is a human-facing label and `ssh_target` is the exact remote tmux target.
Prefer `ssh_target` when automating remote reattach flows.

The launcher can instead supply `expected_agent`, copied unchanged from a v2
Attach action's `attach` object. This guards an **already live pane**: Taarof
rechecks fresh process identity, full host/provider/session identity, workspace,
tab, pane, and tmux target before focusing that pane through its typed attach
handler. A disappeared, changed, or duplicate identity fails with a refresh
error. It never silently creates a replacement provider process. The top-level
`session_name` and `ssh_target` must match the guarded object; `host` is omitted.
Headless runtimes reject this desktop-only operation.

Socket v2 catalogs advertise Attach only for fresh exact per-pane identities
with a tmux backing. Cwd/recency hints are labelled inferred and never authorize
Attach. Local provider-history relaunch remains Resume. HTTP v2 stays an
observation surface and does not advertise these guarded socket Attach plans.

## HTTP API (opt-in)

The HTTP API is token-gated local automation, not a second implementation of
terminal control. The default listener is loopback-bound. On that listener,
the authenticated read routes expose state and observation; the explicitly
listed control routes add mutation only with `[http_control].enabled = true`.
An explicitly opted-in non-loopback listener remains observation-only and is an
unsafe diagnostics escape hatch, not a supported sharing or control deployment.

taarof can expose the same query API over HTTP when the `[http]` config
section is enabled. The HTTP server runs on a local port alongside the
GTK app.

### Configuration

In `~/.config/taarof/config.toml`:

```toml
[http]
enabled = true
port = 7800
bind_address = "127.0.0.1"          # default, localhost only
# Required for 0.0.0.0, LAN IPs, or any non-loopback bind:
unsafe_allow_non_loopback = false

[http_control]
enabled = false                     # explicit local write gate
```

### Authentication

Every protected REST request (except `/health`) requires a Bearer token in the
`Authorization` header. That header is also the default for WebSocket
handshakes. Browser WebSocket clients may instead use `?token=<token>` because
the browser WebSocket API cannot set arbitrary headers; it is a WebSocket-only
alternative, never a REST credential and never a bypass of loopback or control
gates. The browser asset fallback is not a data or control route. The token is
generated per-session and written to
`$XDG_RUNTIME_DIR/taarof-http-<pid>.token` (mode 0600).

```bash
TOKEN=$(cat "$XDG_RUNTIME_DIR/taarof-http-$TAAROF_PID.token")
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7800/api/v1/state
```

### Endpoints

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| GET | `/health` | No | Runtime health summary with degraded-state metadata |
| GET | `/api/v1/runtime-identity` | Yes | Current HTTP-router runtime ID and session name (`taarof.runtime-identity.v1`) |
| GET | `/api/v1/state` | Yes | Full state snapshot (`taarof.state.v1`) |
| GET | `/api/v1/agent-sessions` | Yes | Recent agent-session catalog (`taarof.agent-sessions.v1`) |
| GET | `/api/v1/sessions` | Yes | Session metadata, dashboard state, detached sessions |
| GET | `/api/v1/workspaces` | Yes | Workspace projection matching the state snapshot workspace array |
| GET | `/api/v1/tabs` | Yes | Flattened tab projection with `workspace_id`/`workspace_name` |
| GET | `/api/v1/panes` | Yes | Flattened pane projection with `workspace_id`/`tab_id` |
| GET | `/api/v1/file-preview?tab=T&pane=P&path=PATH&line=N&col=C` | Yes | Read-only bounded preview for a local pane file reference |
| GET | `/api/v1/file-stat?tab=T&pane=P&path=PATH` | Yes | Metadata for a local pane file reference without reading its contents |
| GET | `/api/v1/tabs/{tab_id}/panes/{pane_id}/attach` | Yes | WebSocket: pane snapshot plus live updates for a pane scoped to its tab |
| GET | `/api/v1/tabs/{tab_id}/panes/{pane_id}/control/ws` | Yes + control gate | WebSocket: local-only bidirectional browser control stream for a pane |
| GET | `/api/v1/tabs/{tab_id}/panes/{pane_id}/pty/ws` | Yes + control gate | WebSocket: broker-owned raw PTY frames with signed input and resize |
| GET | `/api/v1/panes/{pane_id}/attach` | Yes | Legacy WebSocket: pane snapshot plus live updates for callers that only know pane id |
| GET | `/api/v1/events?since_seq=N&limit=M` | Yes | Event feed page (`taarof.events.v1`) |
| GET | `/api/v1/history?since_id=N&limit=M&record_type=work&order=desc&text=blocked` | Yes | Indexed, bounded sanitized history page (`taarof.history.v1`); accepts all socket history filters |
| GET | `/api/v1/events/ws` | Yes | WebSocket: live event stream |
| POST | `/api/v1/control/send-keys` | Yes + control gate | Send raw bytes to a pane |
| POST | `/api/v1/control/run-in-pane` | Yes + control gate | Run a shell command in a pane |
| POST | `/api/v1/control/switch-tab` | Yes + control gate | Focus a tab |
| POST | `/api/v1/control/create-tab` | Yes + control gate | Create a terminal tab |
| POST | `/api/v1/control/split-pane` | Yes + control gate | Split a tab and optionally run a command in the new pane |

The raw PTY WebSocket reconstructs a bounded terminal checkpoint; see
[checkpoint coverage and renderer checks](terminal-checkpoints.md) for supported
semantics and known limits.

The read-only REST endpoints return the same JSON shapes as the Unix socket
`query-state`, `query-events`, `query-history`, and `query-agent-sessions` messages, wrapped in
`{"ok": true, "data": ...}`.

`protocol/openapi.yaml` records this exact shipped set as operations marked
`x-taarof-surface: loopback-http`. Its unmarked operations are the separate,
staged remote-protocol design and are not routes served by this local HTTP
listener.

`/api/v1/runtime-identity` is the authenticated source for the current
HTTP-router identity. The gateway must pin this exact `runtime_id` after every
Taarof restart. The installed CLI reads the process-scoped bearer token without
printing it:

```bash
taarof runtime-identity --pretty
```

### Runtime and source build identity

`data.identity` (`taarof.identity.v1`) is published additively on
`/api/v1/runtime-identity`, on `query-state`/`api/v1/state`, and in
`$XDG_RUNTIME_DIR/taarof-current.json`. `schema` and `runtime_id` on the
runtime-identity route are unchanged, so an existing gateway pin keeps working.

It answers two **independent** questions, and conflating them is the bug the
block exists to prevent:

| Field | Question | Values |
|---|---|---|
| `binary_state` | Is the running binary the installed one? | `current`, `update_pending`, `unknown` |
| `running_matches_installed` | Did both binaries hash equal? | `true`, `false`, `null` (could not hash both) |
| `source_state` | Was the running binary built from *this* checkout? | `matches`, `differs`, `unknown` |

Two binaries hashing equal proves they are the same binary. It proves nothing
about whether either was built from the source in front of you — the exact
situation that produced a confidently wrong `current` while both binaries
predated the fix under test.

`source_state` is therefore only `matches` when it can be **proved**: the build
recorded a revision, the checkout reports the same revision, and both trees are
known-clean. A missing revision, an unreadable checkout, or a dirty tree yields
`unknown`. Nothing is ever rounded up. Installed provenance is a separate
SHA-bound `taarof.artifact.v1` sidecar: a foreign schema, changed binary,
version/build-id mismatch, or a tampered manifest clears every provenance field
and reports an error instead of retaining a partial claim.

Supporting fields: `build` (revision, describe, dirty, root, profile and build
time stamped in at compile time by `taarof-app/build.rs`), `source` (what the
checkout says now), `installed_provenance` (read back from
`<prefix>/share/taarof/install-manifest.json`, emitted after linking and copied
only after its SHA matches the installed artifact), and
the full `running`/`installed` binary identities.

Identity is computed on the update-watch blocking worker — hashing and `git`
discovery never run on the GTK thread — and is cached with the same 60s TTL, so
every surface reads one completed verdict rather than probing independently.
Before the first probe completes, every axis reads `unknown`.

The CLI reports the same block from whichever surface is available, preferring
the registry so it works without a socket round-trip:

```bash
taarof --version --pretty     # or: taarof version --format table
taarof doctor                 # the `app.identity` check
```

`/api/v1/state` is the full `taarof.state.v1` snapshot. `/api/v1/workspaces`,
`/api/v1/tabs`, and `/api/v1/panes` are direct read-only projections over the
same runtime workspace/tab/pane ingredients: workspaces keep the nested
workspace shape, tabs flatten those nested tab objects and add workspace
identity, and panes flatten pane objects and add workspace/tab identity.

`/api/v1/sessions` returns `data.session_name` as `string | null`, matching the
top-level `query-state.session_name` contract. `null` means the unnamed/default
runtime session and is not an error.

`/api/v1/file-preview` resolves `path` against the live `cwd` for the requested
`tab`/`pane`, canonicalizes both the pane cwd and target file, and returns a
bounded UTF-8 text preview. It rejects remote panes, paths outside the pane cwd,
missing files, non-regular files, binary or non-UTF-8 content, and oversized
files. The endpoint is read-only but still requires the per-process bearer token.

### Local control endpoints

The mutating subset is the five `POST /api/v1/control/*` routes plus the
bidirectional `/control/ws` and `/pty/ws` WebSockets in the endpoint table.
HTTP control is disabled unless `[http_control].enabled = true`. Even when
enabled, taarof only activates that subset when the HTTP listener is bound to a
loopback address. A non-loopback HTTP bind remains read-only and returns
`403 Forbidden` for control requests.

All control routes require the per-process bearer token. Requests without a
valid bearer token return `401 Unauthorized` before the control gate is checked.
When `[http_control].enabled = true`, that same token is elevated from
observation to command execution for this app process. Any local client that
holds the token can call the control routes while the app is running. The
browser "Unlock control" button prevents accidental input from that browser; it
is not a server-side per-browser grant or principal boundary. Changing
`[http_control].enabled` requires restarting `taarof-app`.

Request bodies:

```json
{"tab":"current","pane":1,"keys":"\u0003"}
```

```json
{"tab":"server","pane":1,"command":"cargo test"}
```

```json
{"tab":"server"}
```

```json
{"name":"Build","working_dir":"/tmp/user/project","command":"cargo test"}
```

```json
{"tab":"server","direction":"horizontal","command":"cargo test","working_dir":"/tmp/user/project","idempotency_key":"build-split-42"}
```

Successful requests return `{"ok": true, "data": ...}` where `data` is the
typed socket command response: `send-keys`, `run-in-pane`, and `switch-tab`
return `{"ok":true}`; `create-tab` also returns `tab_id`; and `split-pane`
also returns the exact `workspace_id`, `tab_id`, and `pane_id`. A bounded
`idempotency_key` (1–128 ASCII letters, digits, `-`, `_`, `.`, or `:`) makes
retries return the original target; reuse for different inputs is rejected.
Records expire after ten minutes, are capped at 256 per running process, and
store only a request fingerprint and target IDs—not commands or environment.
The OpenAPI operation schema is authoritative for each
route. Accepted actions emit an `http_control_action`
event containing only audit metadata such as action name, requested tab target,
pane ID, and response tab/pane IDs. The event payload does not include typed
keys, commands, pane contents, bearer tokens, socket paths, or raw process IDs.

Automated end-to-end coverage in `taarof-app/tests/runtime_smoke.rs` currently
exercises `/health`, `/api/v1/state`, `/api/v1/sessions`, `/api/v1/workspaces`,
`/api/v1/tabs`, `/api/v1/panes`, `/api/v1/events`, and `/api/v1/events/ws`
against a live Axum server with real auth tokens. The pane-attach websocket,
`/api/v1/agent-sessions`, and `/api/v1/file-preview` routes have additional
route coverage in `taarof-app/src/http.rs`.
The HTTP control routes and browser control WebSocket are covered by
`http_control_` tests for disabled mode, auth failure, loopback-only enablement,
bridge dispatch, runtime error response bodies, input-frame forwarding, and
resize-frame handling.

### Prompting one agent turn over the Unix socket

The privileged same-user socket exposes a two-phase agent-turn operation. It
is intentionally not an HTTP route. First submit a prompt to an exact tab and
pane:

```json
{"action":"prompt-agent","tab":"12","pane":4,"prompt":"Run the focused tests."}
```

Prompts are single-line UTF-8 text; newline, carriage-return, and NUL bytes are
rejected so a payload cannot submit early or add a second Enter. On successful
delivery, the response immediately includes a `turn_token`, the
post-submission event `boundary_seq`, and the pane's `pre_state`. Prompt text is
sent through the existing `send-keys` path but is never copied into events,
diagnostics, or the in-process turn registry.

For tmux-backed panes, the existing `send-keys` adapter necessarily places the
text in the short-lived local `tmux` process argv, so same-user `/proc` access
remains inside this privileged local control surface's trust boundary. Then
wait using the token:

```json
{"action":"wait-agent-turn","turn_token":"…","timeout_seconds":120,"scrollback":1000,"max_output_bytes":65536}
```

The wait observes only `agent_activity_changed` events after its boundary for
that exact tab and pane; it does not poll terminal text. A matching `done`,
`waiting-input`, or `errored` transition completes the wait. `idle` completes
only after a post-boundary `running` transition. The response contains both
transition evidence and a bounded UTF-8 tail captured through the existing
logical-line `get-text` path, including original/returned byte counts and
explicit truncation metadata. Missing provider attribution is reported as
`degraded-generic` evidence rather than inferred.

When the exact pane is bound to a Claude Code or Codex structured transcript,
`evidence.native_turn` may additionally contain `provider`, `id`, and
`observed_at_unix_ms`. Claude uses the most recent non-meta user record UUID
that opens the turn; Codex uses the rollout `turn_id` when present. This
identity is returned
only when it belongs to the same detected provider process/session, differs
from the identity seen before the prompt, and was observed inside the
post-prompt window. An ID first exposed on a completion record is accepted;
data that is still missing or delayed when the generic transition completes,
malformed, stale, unknown-provider, or session-mismatched leaves `native_turn`
as `null`. It never changes the provider-neutral completion outcome.
Provider/process detection must already identify the exact pane when the prompt
is submitted; a binding that appears only afterward intentionally falls back to
the generic token for that turn.

Typed outcomes include `completed`, `waiting-input`, `errored`, `timeout`,
`cancelled`, `pane-exited`, and `event-ring-overflow`. A pending token can be
cancelled with `{"action":"cancel-agent-turn","turn_token":"…"}`. Tokens are
process-local, expire after 15 minutes, and permit only one active waiter.

The CLI combines both phases by default:

```bash
taarof prompt-agent --tab 12 --pane 4 --prompt 'Run the focused tests.'
```

Use `--no-wait` to receive the token immediately, followed by
`taarof wait-agent-turn TOKEN` or `taarof cancel-agent-turn TOKEN`.

The same Axum service also serves the browser client at `/`. For local startup,
open:

```bash
http://127.0.0.1:<port>/?token=$(cat "$XDG_RUNTIME_DIR/taarof-http-<pid>.token")
```

The frontend stores that token in local storage, removes it from the visible
URL, purges any old token-bearing CacheStorage entries, and returns to the token
prompt if the API rejects it with `401`. The web client does not register a
service worker; older service-worker registrations are unregistered on load.

Browser asset lookup prefers `TAAROF_WEB_DIST_DIR` when that directory contains a
valid build. After that, source-checkout runs prefer the repo-local
`taarof-web/dist` bundle and then the user data bundle under
`~/.local/share/taarof/web`. Installed binaries prefer the bundle next to the
binary under `share/taarof/web`, then the user data bundle, and finally the
repo-local `taarof-web/dist` path as a last-resort developer fallback.

If none of those locations contains a valid bundle, `/` returns a generic
`503` page that says `web bundle missing`. The attempted paths are recorded
in the diagnostics log for operator debugging instead of being reflected to
unauthenticated clients.

### WebSocket event stream

Connect to `/api/v1/events/ws` with the taarof bearer token. `Authorization`
is the default; `?token=<token>` is the browser-only WebSocket alternative.

- Non-browser clients may send `Authorization: Bearer <token>`.
- Browser clients should use `?token=<token>` on the WebSocket URL because
  the browser `WebSocket` API cannot attach arbitrary HTTP headers.

The server pushes each new event as a JSON message:

```json
{"seq": 42, "ts_unix_ms": 1712534400000, "event_type": "alert_raised", "payload": {...}}
```

If the WebSocket receiver falls behind, the server sends
`{"event_type":"_lagged","skipped":N,"resnapshot_required":true}`. Re-page
`query-events` from the last seen EventStore sequence to determine whether ring
retention also lost history and, if so, learn the exact missing range. The lag
frame itself requires a `query-state` refresh before applying further deltas,
even when the retained event page reports no ring gap.

The browser client subscribes to this stream and debounces state refreshes after
event messages, so workspace/tab/pane metadata does not rely only on manual
refresh. After a disconnect it reconnects with capped exponential backoff and
jitter, pages `/api/v1/events` from the last applied sequence, and deduplicates
events seen by both replay and the live socket. Every reconnect also refreshes a
full state and agent-session snapshot, including when the socket is otherwise
silent. A retention gap, `_lagged` frame, lower event high watermark, or changed
`/api/v1/runtime-identity` value discards the old cursor and requires a new full
snapshot. The header reports connected, recovering, or disconnected state and
keeps the last verified time visible while displayed data is stale.

### WebSocket pane attach

Connect to `/api/v1/tabs/{tab_id}/panes/{pane_id}/attach` with the same bearer token rules as
the event stream. The older `/api/v1/panes/{pane_id}/attach` route remains for
legacy callers, but browser clients should use the tab-scoped route because pane
ids are only unique inside a tab/session restore path:

- Non-browser clients may send `Authorization: Bearer <token>`.
- Browser clients should use `?token=<token>` on the WebSocket URL because
  the browser `WebSocket` API cannot attach arbitrary HTTP headers.

Attach support is read-only:

- Tmux-backed panes use repeated `tmux capture-pane` snapshots.
- VTE-backed panes use the GTK bridge to capture the selected live terminal.
- The transport sends one base64 `snapshot` frame followed by debounced UTF-8
  `replace` frames when the captured visible content changes.
- No input/write path is exposed yet.

For backend tracing, set `TAAROF_PANE_ATTACH_TRACE=1` before starting taarof.
That logs VTE capture dimensions/previews and WebSocket frame encoding/payload
metadata without enabling any remote exposure.

Initial snapshot frame:

```json
{"type":"snapshot","pane_id":7,"cols":120,"rows":40,"encoding":"base64","payload":"Li4u"}
```

- `payload` is the current visible tmux pane capture, base64-encoded.
- `cols` and `rows` reflect the current tmux pane size.

Fallback live update frame:

```json
{"type":"replace","pane_id":7,"cols":120,"rows":40,"encoding":"utf8","payload":"visible screen text"}
```

- `payload` replaces the visible screen contents for the pane.
- Frames are only sent when the captured text or pane dimensions change.

If the attach target disappears for several consecutive polls, or if tmux
capture fails terminally, taarof sends an error frame and then closes the socket:

```json
{"type":"error","pane_id":7,"error":"command exited with status ..."}
```

### Broker PTY output delivery

Each broker pane admits at most four native receivers and sixteen raw WebSocket
observers. Native queues retain at most 4 MiB and 512 entries each (at most 8 KiB
per entry), including initial replay. A full native queue backpressures that
pane's child without holding the broker state lock. Native bytes stay ordered;
queries, resize and other panes remain usable. A native attachment is registered
before the product starts reading its child. A later native attachment whose
initial history was evicted fails explicitly instead of receiving a suffix.

Raw WebSocket observers retain coalesced notifications, then read at most 64 KiB
and eight replay frames per batch. Falling behind the existing 4 MiB replay
window triggers an explicit checkpoint reset. Every socket send has a two-second
deadline; a stalled send disconnects and releases its observer slot. More than
sixteen observers receives `observer_limit`. A checkpoint that exceeds the
conservative 256 KiB reconstruction budget fails with `checkpoint_limit` before
copying or serializing the model. Resize can make a later checkpoint exceed
that budget. Native output and replay continue unchanged.

Natural child exit drains queued output and waits for VTE's consumed-output EOF
before automatic pane cleanup. The direct child is reaped while that drain is
pending; descendants retaining the PTY can delay EOF. Explicit pane close cancels
pending delivery immediately and reaps off the GTK thread. A reader or conduit
failure is reported and terminates the failed attachment instead of waiting for
an EOF that can no longer arrive. Admitted web input outcomes still drain after
final output before the connection closes.

These are per-pane subscriber limits, not a bound on total application RSS or
terminal-model retention. See [checkpoint memory limits](terminal-checkpoints.md#subscriber-memory-accounting)
for copies, metadata and the separate pathological combining-text limitation.

### Broker PTY input completion

The broker-owned `/api/v1/tabs/{tab_id}/panes/{pane_id}/pty/ws` route requires
bearer authentication, the loopback bind, and `[http_control].enabled = true`.
It starts with a checkpoint and uses the terminal-frame protocol. GTK validates
pane identity, epoch, grant and deadline, then admits input without waiting for
the child. One FIFO writer per pane handles native VTE-encoded input and web
input. It holds at most 16 queued jobs plus one active job, each at most 65,536
bytes; each WebSocket also permits at most 16 pending input completions.
Saturation is rejected explicitly. Native input applies backpressure on its
relay thread, preserving VTE encoding and keeping GTK responsive.

An input `ack` means all bytes were accepted by PTY writes, not that the child
consumed or acted on them. Input acknowledgements and errors carry an optional
`input_result` object; resize acknowledgements and output cursors keep their
existing meaning. For example:

```json
{"input_seq":"1","requested_bytes":65536,"written_bytes":11776,"status":"deadline_expired"}
```

The decimal `input_seq` is echoed when supplied. Terminal statuses are
`delivered`, `cancelled`, `deadline_expired`, `closed`, `write_failed`, or
`rejected` (never admitted). Partial delivery is reported explicitly. The signed
deadline bounds admission and delivery, with a maximum five-second server wait.
A terminal outcome cancels the unwritten remainder; those bytes cannot be sent
later when the child resumes reading. Disconnect cancels pending input, and pane
closure stops its writer. Already accepted bytes cannot be recalled. A timeout
before GTK admission releases the pending slot and prevents delayed dispatch.

### WebSocket pane control

Connect to `/api/v1/tabs/{tab_id}/panes/{pane_id}/control/ws` with the same
browser token convention as the attach stream. This route is not a remote
sharing primitive: it requires the bearer token, `[http_control].enabled = true`,
and the same loopback-only control gate as the POST control endpoints.

The browser client opens this socket only after the local operator clicks
`Unlock control`. Locked panes continue to use the read-only attach socket. If
the control socket closes, is rejected, or reports an error, the browser returns
to observe-only snapshot attach.

The first server frame is a full terminal `snapshot`, followed by backend
terminal update frames. Append-only terminal output is sent as a `delta` frame:

```json
{"type":"delta","pane_id":7,"cols":120,"rows":40,"encoding":"utf8","payload":"new output"}
```

When the captured output is rewritten, truncated, or resized, the server sends a
full `replace` frame using the same shape as the read-only attach socket. Client
input is sent as JSON text frames:

```json
{"type":"input","encoding":"utf8","payload":"abc\r"}
```

The payload is forwarded through the same raw `send-keys` runtime path used by
`POST /api/v1/control/send-keys`. Audit events record the action, tab target,
pane ID, and response metadata, but never record typed contents, paste contents,
or bearer tokens.

Resize requests are also JSON text frames:

```json
{"type":"resize","cols":120,"rows":40}
```

The server validates bounded positive dimensions and dispatches them through
the control bridge. VTE-backed panes call the VTE terminal sizing API; tmux-backed
panes call `tmux resize-pane` against the pane's backing session. Supported
backend implementations return:

```json
{"type":"resize","pane_id":7,"cols":120,"rows":40,"supported":true}
```

Unsupported or failed resize acknowledgements are explicit so the browser can
keep control live without pretending the backend pane changed size.

```json
{"type":"resize","pane_id":7,"cols":120,"rows":40,"supported":false,"message":"tmux resize-pane failed: missing pane"}
```

### Staged collaborative session sharing design

Issue #53 is staged as a design contract on top of the existing loopback HTTP
surface, which is read-only by default but has a separately gated local control
subset. It is not a reason to expose the current local server to a network. A
future sharing service must translate an opaque grant into the same snapshot,
event, and pane-attach primitives documented above.

#### Addressing model

The stable internal resource address is a tuple, not a display-name URL:

```text
{ session_name, workspace_id, tab_id?, pane_id? }
```

- `session_name` selects the isolated taarof runtime namespace.
- `workspace_id`, `tab_id`, and `pane_id` come from `taarof.state.v1` and are
  the authoritative selectors for a running session.
- Workspace and tab names remain labels only. They may be included in link
  previews, but they are not identity because users can rename them and duplicate
  labels can exist.
- A workspace-scoped share omits both `tab_id` and `pane_id`; the viewer can
  inspect tabs and panes inside the granted workspace. A tab-scoped share fixes
  `tab_id`; a pane-scoped share fixes both `tab_id` and `pane_id`.

The user-facing collaboration URL should be opaque and capability-based:

```text
https://<share-service>/s/<share_id>#workspace=<workspace_id>&tab=<tab_id>&pane=<pane_id>
```

`share_id` resolves server-side to the grant, scope, expiry, and policy. The URL
must not embed the local bearer token, Unix socket path, raw process id, cwd, or
SSH target. A custom `taarof://share/<share_id>` deep link may be added later as
a launcher convenience, but the HTTPS form is the canonical browser address once
remote sharing exists.

#### Read-only collaboration first

The first collaboration mode is `observe` only:

- Allowed: filtered `query-state`, filtered `query-events`, and read-only pane
  attach streams for panes inside the grant scope.
- Disallowed: `send-keys`, `run-in-pane`, tab/workspace creation, focus changes,
  detach/attach mutations, broadcast input, config writes, and any Unix socket
  imperative action.
- The browser UI must keep terminal input disabled and describe the viewer as an
  observer. A later `control` mode requires a new grant type, explicit host-side
  approval, visible audit events, and a separate revocation path.

#### Presence, privacy, and revocation

Presence records are scoped to a share grant and should be ephemeral:

| Field | Purpose |
|-------|---------|
| `viewer_id` | Random per-grant id; stable only for the lifetime of the connection or browser storage grant |
| `display_name` | Optional viewer-supplied label; never trusted as identity |
| `address` | Current `{workspace_id, tab_id?, pane_id?}` being viewed |
| `mode` | `observe` initially; future `control` only after separate authorization |
| `connected_at_unix_ms` / `last_seen_unix_ms` | Presence freshness and stale cleanup |

Privacy defaults:

- Only expose resources inside the grant scope.
- Do not expose environment variables, runtime token paths, Unix socket paths, or
  host-local filesystem paths beyond fields already intentionally present in the
  read-only API.
- Treat cwd, command names, tab labels, and pane contents as sensitive workspace
  data. The owner must see the grant scope before creating a share.
- Non-loopback collaboration requires TLS and a service/auth boundary; do not use
  `[http].unsafe_allow_non_loopback = true` as a sharing mechanism.

Revocation requirements:

- Every share has an opaque id, creation time, expiry/TTL, scope, mode, and
  revoked flag.
- Revocation invalidates new HTTP requests and closes matching WebSocket/event
  streams as soon as practical.
- Token rotation or app shutdown invalidates all grants tied to that runtime.
- Grant audit events should record create, viewer join/leave, expiry, and revoke
  without logging pane contents or bearer tokens.

#### Trust boundaries

- The same-user Unix socket remains privileged and must never be proxied directly
  to collaborators.
- The current local HTTP bearer token remains a runtime-local bootstrap secret;
  it is not a share token and should not be sent to teammates.
- A future share service may hold a local bearer token on behalf of the owner,
  but collaborators only receive scoped share grants.
- Relay/service failures should fail closed: no state, no pane stream, and no
  fallback to unscoped local API access.

### Staged HTTPS remote attachment design

Issue #54 extends the staged collaboration contract with an authenticated HTTPS
attachment boundary for non-loopback observation. This is not a replacement for
the local HTTP server and does not make `[http].unsafe_allow_non_loopback = true`
safe for remote use. The local HTTP API remains a same-user, runtime-local
bootstrap surface; remote attachment must be served by an HTTPS listener or
service boundary that terminates TLS, validates scoped remote tokens, and
forwards only the read-only primitives documented above.

#### EXAMPLE-13 staging status

EXAMPLE-13 is a design/staging slice, not an implementation toggle. This section is
the review target for authenticated HTTPS remote observation: it defines the
attachment boundary, the remote token lifecycle, and the relationship between
local-only access and future remote grants. Until a later implementation passes
the review gates below, the supported HTTP surface remains loopback-first local
access only, and unsafe non-loopback HTTP remains diagnostics-only.

#### Architecture

The remote observation path has three layers:

1. **Owner runtime:** the existing taarof process, Unix socket, and local HTTP
   API. It owns the authoritative workspace/tab/pane state and pane capture
   streams.
2. **Remote attachment boundary:** a future HTTPS service or in-process HTTPS
   listener that presents the browser API, validates remote tokens, filters
   responses by grant scope, and closes streams when grants expire or are
   revoked.
3. **Remote viewer:** a browser using HTTPS and WebSocket connections to observe
   state, events, and pane output. Viewers never receive the local bearer token,
   Unix socket path, process id, or SSH target.

The HTTPS boundary may run in-process later, but it must stay configured and
audited separately from `[http]`. A future config should use a dedicated section,
for example:

```toml
[https_remote]
enabled = false
bind_address = "127.0.0.1"
port = 7843
cert_path = "/path/to/fullchain.pem"
key_path = "/path/to/privkey.pem"
token_ttl_seconds = 3600
```

Non-loopback binds require HTTPS, explicit opt-in, and a remote token issuer. A
plain HTTP non-loopback bind must remain an unsafe diagnostics escape hatch, not
the remote attachment mechanism.

The boundary responsibilities are intentionally split so future implementation
does not blur local bootstrap access with remote observation:

| Component | Owns | Must not do |
|-----------|------|-------------|
| Owner runtime | authoritative state, pane capture, local bearer token | expose the Unix socket or local bearer token to remote viewers |
| HTTPS boundary | TLS termination, remote grant validation, filtering, audit | accept unscoped local bearer tokens as remote credentials |
| Remote viewer | read-only rendering of granted resources | request write/control actions or infer hidden resources from filtered responses |

#### Remote token lifecycle

Remote tokens are capability grants, not session bootstrap tokens. Each issued
token should have server-side grant metadata:

| Field | Purpose |
|-------|---------|
| `grant_id` | Opaque identifier used for lookup, audit, and revocation |
| `issued_at_unix_ms` | Creation time for audit and TTL enforcement |
| `expires_at_unix_ms` | Hard expiry; expired grants fail closed |
| `scope` | `{ session_name, workspace_id, tab_id?, pane_id? }` |
| `mode` | `observe` initially; future modes require separate grants |
| `audience` | Remote attachment surface, distinct from local HTTP |
| `revoked` | Server-side flag checked by requests and streams |

Issuance should happen only through an owner-approved action, such as a CLI or
owner UI flow that displays the exact scope and expiry before creating the
grant. Tokens should be short-lived by default, opaque to clients, and stored
hashed at rest when persisted. The local per-process HTTP bearer token remains
unscoped and is therefore unsuitable for remote sharing.

Expiry and revocation rules:

- Every remote request checks token presence, expiry, audience, mode, and scope
  before reading owner-runtime state.
- WebSocket event and pane-attach streams re-check expiry/revocation and close
  as soon as practical when a grant is no longer valid.
- Revocation is server-side and immediate for new requests; active streams must
  fail closed rather than falling back to local bearer-token access.
- App shutdown, token-store loss, or owner-runtime token rotation invalidates all
  remote grants tied to that runtime.
- Audit records should include grant create, viewer connect/disconnect, expiry,
  and revoke events, but never pane contents, bearer tokens, or token hashes.

A concrete issuance flow should preserve owner intent at each step:

1. The owner selects a workspace, tab, or pane in the local UI or CLI.
2. The issuer displays the exact scope, mode, expiry, and remote audience before
   minting the grant.
3. The issuer stores only server-side metadata plus a hashed token verifier when
   persistence is needed.
4. The viewer receives an opaque HTTPS URL or token that cannot be converted into
   the local bearer token.
5. Revocation flips server-side grant state, rejects new requests, and closes
   matching event or pane streams without falling back to local HTTP auth.

#### Endpoint exposure

Remote HTTPS should expose the same observation primitives, filtered by grant
scope:

- `GET /api/v1/state`
- `GET /api/v1/events`
- `GET /api/v1/events/ws`
- `GET /api/v1/tabs/{tab_id}/panes/{pane_id}/attach`

Workspace-scoped grants may inspect tabs and panes in that workspace. Tab-scoped
grants are limited to one tab. Pane-scoped grants may only receive metadata and
frames for that pane. Mutating Unix socket actions and any future write/control
HTTP routes are out of scope for `observe` grants.

#### Local-only vs remote attachment

| Surface | Bind | Token | Intended use |
|---------|------|-------|--------------|
| Unix socket | Same-user runtime dir | Filesystem permissions | Privileged local control |
| Local HTTP | Loopback by default | Per-process bearer token | Local browser and same-user automation |
| Unsafe HTTP non-loopback | Explicit unsafe opt-in | Same local bearer token | Diagnostics only; not sharing |
| HTTPS remote attachment | Dedicated HTTPS boundary | Scoped expiring remote grant | Read-only remote observation |

The relationship is intentionally one-way: remote attachment may translate a
valid scoped grant into local read-only API calls, but local HTTP must not accept
remote grants directly unless it is running behind the dedicated HTTPS boundary
and enforcing the same scope, expiry, revocation, and audit rules.

### Staged authenticated control-mode design

The browser writeback/control surface is a separate trust boundary from both
local observe mode and remote read-only attachment.

#### EXAMPLE-29 through EXAMPLE-32 status

EXAMPLE-29 defined the reviewable contract for authenticated browser control.
EXAMPLE-30 added the loopback-only write API behind `[http_control].enabled = true`.
EXAMPLE-31 added the local browser unlock/writeback UX. EXAMPLE-32 documents the
a private deployment Authentik+Caddy gateway pattern for remote personal access; it does not
turn unsafe non-loopback HTTP into a supported control path.

#### Principles

- **Local-first:** write/control stays loopback-only by default, even after it
  exists.
- **Explicit operator enablement:** no hidden auto-upgrade from observe to
  control; the owner must enable the local control gate and deliberately unlock
  browser input.
- **Separate credentials:** the per-process local bearer token is not the same
  thing as a remote scoped control grant.
- **Audited actions:** accepted control actions should emit audit events without
  storing pane contents or bearer secrets.
- **Homelab exposure through a gateway, not raw non-loopback HTTP:** the
  supported remote pattern is an HTTPS control boundary fronted by Authentik and
  Caddy, not `[http].unsafe_allow_non_loopback = true`.

#### Control capabilities

The first control slice should stay narrow and operator-oriented:

- `send-keys`
- `run-in-pane`
- `switch-tab`
- `create-tab`

Out of scope for the first slice:

- multi-user collaboration
- sharing raw local bearer tokens
- direct Unix socket proxying
- unaudited background automation via the browser
- treating unsafe non-loopback HTTP as a supported control deployment

#### Local-only control surface

The local control API uses the separate `[http_control].enabled = true` config
gate and remains loopback-only. The browser UI presents two visible modes:

- `observe` — default, read-only
- `control` — explicit browser unlock, visibly active, lockable again locally

The local app has no per-browser expiry or revocation principal. Local control
fails closed when the app started with `[http_control].enabled = false` or when
HTTP is not loopback-bound. Remote gateway grants may add expiry and revocation,
but those checks happen before forwarding approved actions to taarof.

#### Homelab operator pattern

For a private deployment-style personal homelab use, the preferred remote control shape is:

1. taarof stays bound to loopback on the owner host.
2. A loopback-bound control gateway on the owner host (or a tightly-coupled
   companion service) holds the local bearer token and speaks to taarof locally.
3. `gateway.example` Caddy fronts that gateway through an Authentik `forward_auth`
   route and a Tailscale/SSH tunnel.
4. The browser talks only to the HTTPS gateway and never receives the raw local
   bearer token or Unix socket path.

The operator-facing deployment shape is documented outside this public tree.

#### Control-mode review gates

Before any remote gateway implementation marks control mode as supported beyond
local loopback use, review must prove:

- loopback-only local control remains the default
- remote control requires a dedicated HTTPS boundary with separate auth/grants
- observe and control grants are distinct and revocable
- accepted control actions emit audit records
- Authentik/Caddy integration does not devolve into proxying raw local secrets

#### Design review gates

Before any implementation marks remote attachment as supported, review must prove
these properties are true:

- The operator has explicitly enabled `[https_remote]`; `[http]` loopback
  behavior and bearer-token bootstrap semantics remain unchanged.
- Non-loopback remote observation fails closed when TLS config, token storage,
  owner-runtime connectivity, or grant validation is unavailable.
- Every remote response and stream is filtered by `{ session_name, workspace_id,
  tab_id?, pane_id? }` before leaving the HTTPS boundary.
- Revocation and expiry are enforced for both one-shot REST requests and
  long-lived WebSocket/event streams.
- Audit records identify grant lifecycle and viewer connection events without
  storing pane contents, bearer tokens, token hashes, local paths, process ids, or
  Unix socket paths.

### Security notes

- The server binds to `127.0.0.1` by default.
- Any non-loopback bind requires
  `[http].unsafe_allow_non_loopback = true`. taarof refuses `0.0.0.0`,
  LAN, or public binds without that explicit opt-in.
- Even with that opt-in, treat non-loopback HTTP binds as unsafe. Any
  client that can reach the port and obtain the bearer token can read
  taarof state and event data.
- Token files are created with mode 0600 and removed on clean shutdown.
- The HTTP API is read-only by default. Mutating control routes require
  `[http_control].enabled = true` and are never active on non-loopback binds.

## Future extensions

The HTTP API now exposes state, event streaming, pane observation, and a narrow
loopback-only control surface for the local web client. Issue #53 is staged
above as a read-only collaboration contract; remote collaboration and remote
control still require a service/auth boundary, richer transport semantics than
the `replace` fallback, and HTTPS remote attachment.

The unsafe non-loopback HTTP escape hatch remains diagnostics-only. Do not use
it as a supported control deployment path.

Exact launcher Attach currently requires a local native executable with a
verifiable provider signature and canonical resume argv whose ID exists in
provider history. Additional flags, resume names, and prefixes cannot authorize
Attach. Interpreter
wrappers, title-only detection, and remote process IDs remain observation-only;
remote cached provider history can still offer Resume after a healthy refresh.
