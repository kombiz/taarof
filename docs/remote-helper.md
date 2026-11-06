# Remote helper (slice 1 of #96)

`taarof-remote` is a small Linux binary. It has no GTK, libadwaita or VTE
dependency. It observes the current user's host and session metadata over SSH
stdio. It has no daemon, socket, network listener, or control messages.

```sh
ssh -T -o BatchMode=yes host.ts taarof-remote stream
# Optional intervals, in whole seconds (1..3600):
ssh -T -o BatchMode=yes host.ts taarof-remote stream --poll-seconds 2 --heartbeat-seconds 10
```

Keep SSH stdin open for the stream's lifetime. The helper exits successfully
on stdin EOF, a closed stdout pipe, SIGPIPE, SIGTERM, or SIGINT. It discards
input bytes; they cannot request an action. A stalled reader cannot block
shutdown. Child probes run in private process groups. They lose ambient
Infisical tokens and the internal reload marker. Timeouts and shutdown kill
and reap probe children, including shell pipelines.

Each stdout line is one JSON event. Diagnostics go to stderr.
The shared serde contract is `agent_session_core::remote_stream`.

| Event | Fields and meaning |
| --- | --- |
| `hello` | First line. `protocol_version: 1`, `helper_version`, `host` (kernel hostname, for display only). |
| `snapshot` | Full replacement: `sessions`, `stats`. Sent first, then only when sessions or stats change. |
| `heartbeat` | `observed_at_unix_ms`, `metrics`. Sent right after the first snapshot, then every 10 seconds by default. |

Rules for readers:

- **Line size.** No line is longer than 1 MiB (`MAX_EVENT_LINE_BYTES`), newline
  included. A snapshot that would be longer drops its oldest sessions and
  counts them in `stats.sessions_omitted`.
- **Unknown input.** Ignore unknown fields. Skip a line whose `type` you do not
  know; the Rust contract decodes it as `Event::Unknown`.
- **Identity.** Session records are unstamped: `host` and `resume_command` are
  always absent. The consumer stamps both from its own configured host and SSH
  target (`stamp_remote_records`). The `hello` hostname is never identity.

Slice 1 emits replacement snapshots rather than deltas. Metrics change on
every read, so they ride heartbeats; a snapshot never repeats only because
load or memory moved. `metrics` fields are `load` (one-minute load), `mem`
(`total_bytes`, `used_bytes`), `nproc`, and `tmux_session_count`. Memory uses `MemTotal - MemAvailable`, as current
`free` does. Divide load by nproc and multiply by 100 for the app's CPU
load percentage. Unavailable metrics are `null`; a failed tmux query is
not reported as zero sessions. Load and memory come from `/proc`. `nproc`
and `tmux list-sessions` each have a one-second deadline.

Session records use the exact shell sampler and parsers shared with the app's
existing remote probe. This keeps provider paths, sorting and limits unchanged:

- Newest 20 files per Claude, Codex and Pi provider; last 20 Kimi index lines.
- Scan the first 32 lines; keep up to 8 per record file.
- Drop whole lines above 65,536 characters. Never cut JSON lines.
- A 32,768-byte budget trims later lines. The first accepted line survives.
- Read at most 8 MiB per enumeration; keep at most 60 parsed sessions.

`stats` reports `dropped_lines`, `unparsable_lines`, `truncated_files`, and
`blocks_without_session`. `output_truncated` reports the aggregate reader
cap. `enumeration_failed` reports a failed or timed-out sampler; that event
has no session records. The sampler has the same 10-second deadline as the
app's SSH probe (`REMOTE_SCAN_TIMEOUT`). Stats describe
this sample, not lifetime totals. Enumeration is read-only, but like the shell
probe it may include an early prompt as a session title. Treat the stream as
private user metadata. Do not log it indiscriminately. Resume strings are
metadata, not permission to execute; consumers must revalidate attachments.

The helper polls every two seconds by default. It compares parsed sessions and
stats rather than serializing raw record files into the output. Probe time
adds to the poll interval. Heartbeats continue during child probes after the
initial snapshot. This first slice still invokes the shared shell sampler on each poll;
it makes no traffic or CPU speedup claim.

## Build and host ownership

Build on a headless Linux host with `cargo build -p taarof-remote --release`.
This selects only the helper and its non-GTK dependencies. The workspace's
default member remains the desktop app.

Mise owns installation on each host. Register the helper in that host's managed
mise tool configuration, with an approved source/version, and install through
mise. Do not hand-copy a binary from the desktop, use a curl installer, or add
a second tool owner. This slice does not change fleet tool pins or install it.

The GTK consumer, one persistent SSH connection per host, reconnect/backoff,
stale-host handling, and fallback to the existing shell probe are slice 2.
The app still uses its current remote probe. Remote process-tree observation,
performance baselines, hook push, and delta events are outside this slice.
