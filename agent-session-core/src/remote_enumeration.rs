//! Bounded shell enumeration shared by the legacy SSH probe and stdio helper.
use crate::discovery::{
    build_resume_command, fallback_title, parse_claude_session, parse_codex_session,
    parse_pi_session, shell_escape, AgentSessionRecord,
};
use serde_json::Value;
use std::{cmp::Reverse, path::PathBuf};

/// Newest record files enumerated per provider on a remote host.
pub const REMOTE_MAX_FILES_PER_PROVIDER: usize = 20;
/// Lines read from each remote record file before any are selected.
pub const REMOTE_MAX_SCANNED_LINES: usize = 32;
/// Record lines kept from each remote file — enough for the session header the
/// parsers need plus an early user message for the title, not a transcript.
pub const REMOTE_MAX_SAMPLE_LINES: usize = 8;
/// Per-line character cap. Measured against real stores (2026-08-14): a codex
/// `session_meta` line — always line 1, and the only line carrying the id and
/// cwd — runs 18k–48k chars because it embeds the full instruction preamble;
/// claude first lines reach ~60k. An over-cap line is **dropped whole and
/// counted**, never truncated: a truncated JSON line is invalid JSON, so
/// truncation silently destroys the record instead of reporting a loss.
pub const REMOTE_MAX_LINE_CHARS: usize = 65536;
/// Byte budget per remote record file, applied only *after* the first kept
/// line. The first line is where every provider puts the session id and cwd, so
/// the budget must never be able to reject the one line that carries the
/// record — it trims the extra lines sampled for a title. A file stopped by
/// this budget is counted, not silently cut. Deliberately below
/// [`REMOTE_MAX_LINE_CHARS`]: a real codex header alone exceeds it, and must
/// still come through.
pub const REMOTE_MAX_BLOCK_BYTES: usize = 32768;
/// Sessions kept per remote host after parsing.
pub const REMOTE_MAX_SESSIONS_PER_HOST: usize = 60;
/// Hard cap on the bytes read back from one remote enumeration. Sized against
/// the per-block budget above so a normal round is never clipped by the reader.
pub const REMOTE_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Block framing for the remote enumeration stream. Content lines are prefixed
/// with `|` so no file can forge a header from its own contents.
pub const REMOTE_BLOCK_HEADER: &str = "##TAAROF-REMOTE-V1";
pub const REMOTE_BLOCK_END: &str = "##TAAROF-REMOTE-END";
/// Per-block loss report: `##TAAROF-REMOTE-STATS <dropped> <budget-stopped>`.
pub const REMOTE_BLOCK_STATS: &str = "##TAAROF-REMOTE-STATS";
pub const REMOTE_CONTENT_PREFIX: char = '|';

pub fn remote_enumeration_script() -> String {
    format!(
        r#"set -u
sample() {{
head -n {scanned} "$1" 2>/dev/null | awk -v maxlen={chars} -v maxlines={lines} -v maxbytes={block} '
length($0) > maxlen {{ dropped++; next }}
{{
  if (kept > 0 && bytes + length($0) > maxbytes) {{ stopped = 1; exit }}
  print "{prefix}" $0
  bytes += length($0)
  kept++
  if (kept >= maxlines) exit
}}
END {{ printf "{stats} %d %d\n", dropped+0, stopped+0 }}'
}}
for spec in claude:.claude/projects codex:.codex/sessions pi:.pi/agent/sessions; do
provider=${{spec%%:*}}
dir=$HOME/${{spec#*:}}
[ -d "$dir" ] || continue
find "$dir" -type f -name '*.jsonl' -printf '%T@ %p\n' 2>/dev/null | sort -rn | head -n {files} | while IFS=' ' read -r mtime path; do
printf '{header} %s %s %s\n' "$provider" "$mtime" "$path"
sample "$path"
printf '{footer}\n'
done
done
kimi_index=$HOME/.kimi-code/session_index.jsonl
if [ -f "$kimi_index" ]; then
kimi_mtime=$(find "$kimi_index" -maxdepth 0 -printf '%T@\n' 2>/dev/null)
printf '{header} %s %s %s\n' kimi "${{kimi_mtime:-0}}" "$kimi_index"
tail -n {files} "$kimi_index" 2>/dev/null | awk -v maxlen={chars} -v maxlines={files} -v maxbytes={block} '
length($0) > maxlen {{ dropped++; next }}
{{
  if (kept > 0 && bytes + length($0) > maxbytes) {{ stopped = 1; exit }}
  print "{prefix}" $0
  bytes += length($0)
  kept++
  if (kept >= maxlines) exit
}}
END {{ printf "{stats} %d %d\n", dropped+0, stopped+0 }}'
printf '{footer}\n'
fi
"#,
        files = REMOTE_MAX_FILES_PER_PROVIDER,
        scanned = REMOTE_MAX_SCANNED_LINES,
        lines = REMOTE_MAX_SAMPLE_LINES,
        chars = REMOTE_MAX_LINE_CHARS,
        block = REMOTE_MAX_BLOCK_BYTES,
        header = REMOTE_BLOCK_HEADER,
        footer = REMOTE_BLOCK_END,
        stats = REMOTE_BLOCK_STATS,
        prefix = REMOTE_CONTENT_PREFIX,
    )
}

pub fn remote_resume_command(
    ssh_target: &str,
    provider: &str,
    cwd: &str,
    session_id: &str,
) -> String {
    format!(
        "ssh -t {} {}",
        shell_escape(ssh_target),
        shell_escape(&build_resume_command(provider, cwd, session_id))
    )
}

struct RemoteBlock {
    provider: String,
    path: PathBuf,
    modified_at_unix_ms: u64,
    lines: Vec<Value>,
}

/// What one host's enumeration lost on the way here. Every count is a record
/// line that existed on the host and is not represented in the parsed results,
/// so a caller can tell "this host has no sessions" from "we could not read
/// them".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteParseStats {
    /// Lines the host skipped as over the per-line cap.
    pub dropped_lines: usize,
    /// Lines that arrived but were not valid JSON.
    pub unparsable_lines: usize,
    /// Files whose per-block byte budget stopped the sample early.
    pub truncated_files: usize,
    /// Blocks that yielded no session record at all.
    pub blocks_without_session: usize,
}

impl RemoteParseStats {
    pub fn lost_lines(&self) -> usize {
        self.dropped_lines + self.unparsable_lines
    }

    /// A short, honest summary, or None when nothing was lost.
    pub fn warning(&self) -> Option<String> {
        if self.lost_lines() == 0 && self.truncated_files == 0 {
            return None;
        }
        let mut parts = Vec::new();
        if self.dropped_lines > 0 {
            parts.push(format!(
                "{} record line(s) exceeded the {REMOTE_MAX_LINE_CHARS}-char cap",
                self.dropped_lines
            ));
        }
        if self.unparsable_lines > 0 {
            parts.push(format!(
                "{} line(s) were not valid JSON",
                self.unparsable_lines
            ));
        }
        if self.truncated_files > 0 {
            parts.push(format!(
                "{} file(s) hit the {REMOTE_MAX_BLOCK_BYTES}-byte per-file budget",
                self.truncated_files
            ));
        }
        if self.blocks_without_session > 0 {
            parts.push(format!(
                "{} file(s) yielded no session",
                self.blocks_without_session
            ));
        }
        Some(format!(
            "This host is under-reported: {}.",
            parts.join("; ")
        ))
    }
}

/// Split the framed enumeration stream back into per-file blocks. Only exact
/// header lines frame a block and only `|`-prefixed lines carry content, so a
/// record file cannot forge a block of its own.
fn split_remote_blocks(output: &str) -> (Vec<RemoteBlock>, RemoteParseStats) {
    let mut blocks = Vec::new();
    let mut stats = RemoteParseStats::default();
    let mut current: Option<RemoteBlock> = None;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{REMOTE_BLOCK_STATS} ")) {
            // The host's own loss report for the block it just emitted.
            let mut counts = rest.split_whitespace();
            stats.dropped_lines += counts
                .next()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or_default();
            if counts
                .next()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or_default()
                > 0
            {
                stats.truncated_files += 1;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix(&format!("{REMOTE_BLOCK_HEADER} ")) {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            let mut parts = rest.splitn(3, ' ');
            let (Some(provider), Some(mtime), Some(path)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            current = Some(RemoteBlock {
                provider: provider.to_string(),
                path: PathBuf::from(path),
                modified_at_unix_ms: mtime
                    .parse::<f64>()
                    .ok()
                    .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                    .map(|seconds| (seconds * 1000.0) as u64)
                    .unwrap_or_default(),
                lines: Vec::new(),
            });
            continue;
        }
        if line == REMOTE_BLOCK_END {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            continue;
        }
        let Some(block) = current.as_mut() else {
            continue;
        };
        let Some(content) = line.strip_prefix(REMOTE_CONTENT_PREFIX) else {
            continue;
        };
        match serde_json::from_str::<Value>(content) {
            Ok(value) => block.lines.push(value),
            Err(_) => stats.unparsable_lines += 1,
        }
    }
    if let Some(block) = current.take() {
        blocks.push(block);
    }
    (blocks, stats)
}

/// Parse one host's enumeration output with the same parsers the local scan
/// uses, then stamp every record with its source host.
pub fn parse_remote_enumeration_output(
    host: &str,
    ssh_target: &str,
    output: &str,
) -> (Vec<AgentSessionRecord>, RemoteParseStats) {
    let output = if output.len() > REMOTE_MAX_OUTPUT_BYTES {
        let end = output
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= REMOTE_MAX_OUTPUT_BYTES)
            .last()
            .unwrap_or(0);
        &output[..end]
    } else {
        output
    };

    let mut sessions = Vec::new();
    let (blocks, mut stats) = split_remote_blocks(output);
    for block in blocks {
        let parsed = match block.provider.as_str() {
            "claude" => parse_claude_session(&block.path, block.modified_at_unix_ms, &block.lines)
                .into_iter()
                .collect::<Vec<_>>(),
            "codex" => parse_codex_session(&block.path, block.modified_at_unix_ms, &block.lines)
                .into_iter()
                .collect(),
            "pi" => parse_pi_session(&block.path, block.modified_at_unix_ms, &block.lines)
                .into_iter()
                .collect(),
            "kimi" => parse_remote_kimi_index(block.modified_at_unix_ms, &block.lines),
            _ => Vec::new(),
        };
        if parsed.is_empty() {
            stats.blocks_without_session += 1;
        }
        for mut session in parsed {
            // A remote cwd must never be probed against the local filesystem.
            session.repo_root = None;
            session.host = Some(host.to_string());
            session.resume_command = Some(remote_resume_command(
                ssh_target,
                &session.agent,
                &session.cwd,
                &session.session_id,
            ));
            sessions.push(session);
        }
    }
    sessions.sort_by_key(|session| Reverse(session.updated_at_unix_ms));
    sessions.truncate(REMOTE_MAX_SESSIONS_PER_HOST);
    (sessions, stats)
}

fn parse_remote_kimi_index(modified_at_unix_ms: u64, lines: &[Value]) -> Vec<AgentSessionRecord> {
    lines
        .iter()
        .filter_map(|entry| {
            let session_id = entry.get("sessionId")?.as_str()?.to_string();
            let cwd = entry.get("workDir")?.as_str()?.to_string();
            Some(AgentSessionRecord {
                agent: "kimi".to_string(),
                session_id: session_id.clone(),
                title: fallback_title("", "kimi", &session_id),
                cwd: cwd.clone(),
                host: None,
                repo_root: None,
                started_at_unix_ms: None,
                updated_at_unix_ms: modified_at_unix_ms,
                last_user_message_at_unix_ms: None,
                status: "recent".to_string(),
                live_binding: None,
                resume_command: Some(build_resume_command("kimi", &cwd, &session_id)),
                resume_unavailable_reason: None,
            })
        })
        .collect()
}
