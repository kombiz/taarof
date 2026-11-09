# Remote helper (#96)

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

The helper emits replacement snapshots rather than deltas. Metrics change on
every read, so they ride heartbeats; a snapshot never repeats only because
load or memory moved. `metrics` fields are `load` (one-minute load), `mem`
(`total_bytes`, `used_bytes`), `nproc`, and `tmux_session_count`. Memory uses `MemTotal - MemAvailable`, as current
`free` does. Divide load by nproc and multiply by 100 for the app's CPU
load percentage. Unavailable metrics are `null`; a failed tmux query is
not reported as zero sessions. Load and memory come from `/proc`. `nproc`
and `tmux list-sessions` each have a one-second deadline.

Session records use the same file set and parsers as the app's SSH shell
probe. The helper runs on the host and sends only parsed records, so it reads
each file in-process with the **local** reader, not the shell probe's wire
budgets. Remote titles therefore match what the host's own discovery shows.

- Newest 20 files per Claude, Codex and Pi provider; last 20 Kimi index lines.
- Read up to 64 lines per record file, 1 MiB per line, 16 MiB per file.
  A large line after the header ends the sample; the record is kept.
- An invalid JSON line is skipped and counted, like the shell sampler, so a
  valid header still yields its session. An oversized line before any record
  is dropped and counted.
- Keep at most 60 parsed sessions.
- Cache each file's sample by device, inode, length, mtime and ctime. An
  unchanged file is not re-read. Session stores are append-only, so an append
  past a full 64-line (or byte-limited) sample reuses it with the new mtime,
  but only while a hash of the file's first 64 KiB is unchanged: an in-place
  rewrite on the same inode changes the header, so it is re-read. A shorter
  file is re-read, so a new session picks up its title.
- A failed read is not cached; the next poll retries it. Files that
  disappear leave the cache.

The SSH shell probe (the fallback) keeps its bounded sampler: 32 lines scanned,
8 kept, 65,536 characters per line, a 32,768-byte budget after the first line,
and 8 MiB per enumeration. Its loss counts still appear while a host runs on
the fallback.

`stats` reports `dropped_lines`, `unparsable_lines`, `truncated_files`,
`blocks_without_session`, and (from helper 0.3.0) `stores_incomplete`.
`stores_incomplete` counts session stores whose directory walk failed in whole
or in part (an unreadable store or subtree); readable parts still yield their
sessions, and a nonzero count warns. Older helpers omit the field and a reader
treats it as zero. From the helper, `dropped_lines` counts oversized
lines before a record, `unparsable_lines` counts invalid JSON lines, and
`truncated_files` and `output_truncated` stay zero (shell-sampler losses).
`blocks_without_session` counts files that yield no session, including files
that could not be read; stores also hold non-session JSONL, so this count
alone does not warn. `enumeration_failed` reports a failed, cancelled or
timed-out sample; that event has no session records. The sample has the same
10-second deadline as the app's SSH probe (`REMOTE_SCAN_TIMEOUT`), checked
before and after each store walk and file read. One blocked read (for example
a hung network mount) is not interruptible; the deadline applies between
reads, and shutdown waits for that read. Stats describe this sample, not lifetime totals.
Enumeration is read-only, but like the shell probe it may include an early
prompt as a session title. Treat the stream as private user metadata. Do not
log it indiscriminately. Resume strings are metadata, not permission to
execute; consumers must revalidate attachments.

The helper polls every two seconds by default. It compares parsed sessions and
stats rather than serializing raw record files into the output. Probe time
adds to the poll interval. Heartbeats continue during probes after the
initial snapshot. Each poll still walks the store directories to find the
newest files (the walk stops at 100,000 entries; fleet stores held at most
about 13,000 on 2026-11-09). Ordering uses millisecond mtimes, so files with
equal mtimes can order differently from the shell sampler's `%T@` sort.

## Build and host ownership

For local work, build on a headless Linux host with
`cargo build -p taarof-remote --release`. This selects only the helper and its
non-GTK dependencies. The workspace's default member remains the desktop app.

The helper has its own release line, separate from the desktop app's `v*`
releases. Push an annotated tag `taarof-remote-v<version>`, where `<version>`
is the `taarof-remote/Cargo.toml` version; `.github/workflows/release-remote.yml`
refuses a tag that does not match. The workflow builds the helper static for
`x86_64-unknown-linux-musl`, so it runs on every fleet host whatever its glibc.
It publishes a pre-release that never becomes "Latest", with these assets:

- `taarof-remote-x86_64-unknown-linux-musl.tar.gz`: one directory holding
  `taarof-remote` and the licenses
- `….tar.gz.sha256` and `….tar.gz.intoto.jsonl` (build provenance)

Rehearse without publishing, on the exact tip of a branch:
`gh workflow run release-remote.yml --ref <branch> -f commit=<that branch's tip SHA>`.
The workflow refuses any other commit, so the rehearsal's attestation always
describes the commit it built.
`packaging/linux/remote-helper-bundle.sh` builds the asset and refuses a
dynamically linked binary. `testing/test-remote-helper-bundle.sh` checks its
layout, checksum and reproducibility.

Mise owns installation on each host. The dotfiles dev-host tier pins it:

```toml
"github:kombiz/taarof" = { version = "<version>", version_prefix = "taarof-remote-v", asset_pattern = "taarof-remote-x86_64-unknown-linux-musl.tar.gz", os = ["linux"] }
```

Do not hand-copy a binary from the desktop, use a curl installer, or add a
second tool owner. Non-interactive SSH may lack mise's shims on PATH; the app's
remote command adds `~/.local/share/mise/shims` for that case.

## App consumer (slice 2)

taarof-app keeps one `ssh -T -o BatchMode=yes -o ConnectTimeout=5`
connection per remote host that agent-session discovery already probes. The
remote command adds `~/.local/share/mise/shims` to PATH when non-interactive
SSH does not find the helper. Each host has its own supervisor thread; a slow
or dead host never holds up GTK or another host.

- **Identity.** Records are stamped from the configured host name and SSH
  target. `hello.host` is ignored. A streamed record never carries a live
  pane binding.
- **Freshness.** A stream is fresh for 25 seconds after its last event (two
  missed default heartbeats plus slack). A fresh stream replaces the shell
  probe for that host's sessions and for the workspace host status. Otherwise
  the host falls back to the shell probe, and the last streamed records age
  through the usual 90-second window.
- **Stalls.** No event for 35 seconds ends the connection. Reconnects back off
  from 2 seconds, doubling to at most 60 seconds.
- **Fallback.** Exit 127 (helper missing), another `protocol_version`, or three
  failed connections in a row switch the host to the shell probe for 10
  minutes. A connection resets the failure count only when it ends inside an
  unbroken 60-second run of good samples; a failed sample restarts that run. `RemoteHostStatus.warning` says why. The stream is then tried
  again, and its first good snapshot clears the note.
- **Bad input.** A line over 1 MiB, a line that is not an event, a stream that
  does not start with `hello`, or a second `hello` ends the connection and
  counts as a failure.
- **Failed samples.** A snapshot with `enumeration_failed` keeps the stream's
  last good records and their observation time, even across reconnects. While
  the latest sample has failed, the app does not use the stream for sessions:
  the shell probe runs, and the newest successful records from either source
  stay in the cache, as for any failed host. Heartbeat metrics still feed host
  status.
- **Teardown.** Both pipe readers are cancellable and joined, so a descendant
  of ssh that keeps a pipe open cannot leave threads behind.

Heartbeat metrics come from the helper's last finished sample. During a slow
sample they can lag `observed_at_unix_ms` by up to one poll plus the 10-second
sampler deadline. That is fine for the load and memory gauges they feed.

Remote process-tree observation, performance baselines, hook push, delta
events, and installing the helper through mise remain outside this work.
