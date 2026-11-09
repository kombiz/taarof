//! Bounded shell enumeration shared by the legacy SSH probe and stdio helper.
use crate::discovery::{
    build_resume_command, fallback_title, parse_claude_session, parse_codex_session,
    parse_pi_session, shell_escape, AgentSessionRecord,
};
use serde_json::Value;
use std::{cmp::Reverse, path::PathBuf, time::Duration};

/// Wall-clock deadline for one remote enumeration. The app's SSH probe and the
/// stdio helper share it, so a host that works with one works with the other.
pub const REMOTE_SCAN_TIMEOUT: Duration = Duration::from_secs(10);

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
    let (mut sessions, stats) = parse_remote_enumeration_records(output);
    stamp_remote_records(&mut sessions, host, ssh_target);
    (sessions, stats)
}

/// Stamp parsed remote records with the consumer's own host label and SSH
/// target: the host identity and a resume command that SSHes to that target.
pub fn stamp_remote_records(sessions: &mut [AgentSessionRecord], host: &str, ssh_target: &str) {
    for session in sessions {
        session.host = Some(host.to_string());
        session.resume_command = Some(remote_resume_command(
            ssh_target,
            &session.agent,
            &session.cwd,
            &session.session_id,
        ));
    }
}

/// Parse one host's enumeration output without stamping: `host` and
/// `resume_command` stay `None`, because only the consumer knows which SSH
/// target reached this host. The stdio helper sends records in this form.
pub fn parse_remote_enumeration_records(
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
            session.host = None;
            session.resume_command = None;
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

/// Bytes read from the end of the kimi index. The shell sampler keeps its last
/// [`REMOTE_MAX_FILES_PER_PROVIDER`] lines; this bounds the read that finds them.
const LOCAL_KIMI_TAIL_BYTES: u64 = 1024 * 1024;

/// JSONL session stores the helper reads, as `(provider, path under HOME,
/// parser)`. Same set and same parsers as [`remote_enumeration_script`].
type SessionParser = fn(&std::path::Path, u64, &[Value]) -> Option<AgentSessionRecord>;
const LOCAL_JSONL_STORES: [(&str, &str, SessionParser); 3] = [
    ("claude", ".claude/projects", parse_claude_session),
    ("codex", ".codex/sessions", parse_codex_session),
    ("pi", ".pi/agent/sessions", parse_pi_session),
];

/// One file's outcome, reused while its mtime and length are unchanged.
#[derive(Clone)]
struct CachedFile {
    modified_at_unix_ms: u64,
    len: u64,
    records: Vec<AgentSessionRecord>,
    unparsable_lines: usize,
}

/// Enumeration for a helper that runs **on** the host.
///
/// The shell sampler's limits exist for raw lines crossing SSH. A helper sends
/// only parsed records, so it reads each file with the local reader instead
/// (64 lines, 1 MiB per line, 16 MiB per file): remote titles match what the
/// host's own discovery would show. The file set, parsers, ordering and caps
/// match [`remote_enumeration_script`]. Results are cached by path, mtime and
/// length, so a poll re-reads only files that changed.
#[derive(Default)]
pub struct LocalEnumerator {
    cache: std::collections::HashMap<PathBuf, CachedFile>,
}

impl LocalEnumerator {
    /// Enumerate `home`. `check` runs before every file read; an error from it
    /// (cancellation) or a passed `deadline` fails the whole sample, so no
    /// partial record set is ever published.
    pub fn enumerate(
        &mut self,
        home: &std::path::Path,
        deadline: Duration,
        mut check: impl FnMut() -> std::io::Result<()>,
    ) -> std::io::Result<(Vec<AgentSessionRecord>, RemoteParseStats)> {
        let started = std::time::Instant::now();
        let mut check = || -> std::io::Result<()> {
            check()?;
            if started.elapsed() > deadline {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            Ok(())
        };
        let mut seen = std::collections::HashSet::new();
        let mut sessions = Vec::new();
        let mut stats = RemoteParseStats::default();
        for (_, dir, parser) in LOCAL_JSONL_STORES {
            let root = home.join(dir);
            if !root.is_dir() {
                continue;
            }
            // Like the shell's `find 2>/dev/null`: an unreadable store is skipped.
            let Ok(scan) = crate::discovery::collect_jsonl_candidates(&root) else {
                continue;
            };
            for candidate in scan.files.into_iter().take(REMOTE_MAX_FILES_PER_PROVIDER) {
                check()?;
                let path = candidate.path.clone();
                let modified = candidate.modified_at_unix_ms;
                let file = self.cached(&path, modified, |len| {
                    let (records, cacheable) =
                        match crate::discovery::read_jsonl_sample_lines(&path) {
                            Ok(sample) => (
                                parser(&path, modified, &sample.values)
                                    .into_iter()
                                    .collect(),
                                !sample.incomplete_append,
                            ),
                            // A header that is unreadable, malformed or over
                            // 1 MiB yields no session; it is counted below.
                            Err(_) => (Vec::new(), true),
                        };
                    let file = CachedFile {
                        modified_at_unix_ms: modified,
                        len,
                        records,
                        unparsable_lines: 0,
                    };
                    (file, cacheable)
                });
                seen.insert(path);
                account(&file, &mut sessions, &mut stats);
            }
        }
        let index = home.join(".kimi-code/session_index.jsonl");
        if index.is_file() {
            check()?;
            let modified = fs_modified_unix_ms(&index);
            let file = self.cached(&index, modified, |len| {
                let (lines, unparsable_lines) = read_kimi_tail(&index, len);
                let file = CachedFile {
                    modified_at_unix_ms: modified,
                    len,
                    records: parse_remote_kimi_index(modified, &lines),
                    unparsable_lines,
                };
                (file, true)
            });
            seen.insert(index);
            account(&file, &mut sessions, &mut stats);
        }
        self.cache.retain(|path, _| seen.contains(path));
        for session in &mut sessions {
            // A remote cwd must never be probed against the consumer's disk.
            session.repo_root = None;
            session.host = None;
            session.resume_command = None;
        }
        sessions.sort_by_key(|session| Reverse(session.updated_at_unix_ms));
        sessions.truncate(REMOTE_MAX_SESSIONS_PER_HOST);
        Ok((sessions, stats))
    }

    /// The cached outcome for `path`, or a fresh read when its mtime or length
    /// changed. `read` gets the current length and says whether to cache.
    fn cached(
        &mut self,
        path: &std::path::Path,
        modified_at_unix_ms: u64,
        read: impl FnOnce(u64) -> (CachedFile, bool),
    ) -> CachedFile {
        let len = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        if let Some(file) = self.cache.get(path) {
            if file.modified_at_unix_ms == modified_at_unix_ms && file.len == len {
                return file.clone();
            }
        }
        let (file, cacheable) = read(len);
        if cacheable {
            self.cache.insert(path.to_path_buf(), file.clone());
        } else {
            self.cache.remove(path);
        }
        file
    }
}

fn account(
    file: &CachedFile,
    sessions: &mut Vec<AgentSessionRecord>,
    stats: &mut RemoteParseStats,
) {
    stats.unparsable_lines += file.unparsable_lines;
    if file.records.is_empty() {
        stats.blocks_without_session += 1;
    }
    sessions.extend(file.records.iter().cloned());
}

fn fs_modified_unix_ms(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// The last [`REMOTE_MAX_FILES_PER_PROVIDER`] lines of the kimi index, parsed,
/// plus how many of them were not valid JSON.
fn read_kimi_tail(path: &std::path::Path, len: u64) -> (Vec<Value>, usize) {
    use std::io::{Read, Seek, SeekFrom};
    let start = len.saturating_sub(LOCAL_KIMI_TAIL_BYTES);
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        file.seek(SeekFrom::Start(start))?;
        file.take(LOCAL_KIMI_TAIL_BYTES).read_to_end(&mut bytes)
    });
    if read.is_err() {
        return (Vec::new(), 0);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        // The first line of a mid-file read is a fragment.
        lines.remove(0);
    }
    let lines: Vec<&str> = lines
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail = &lines[lines.len().saturating_sub(REMOTE_MAX_FILES_PER_PROVIDER)..];
    let mut values = Vec::new();
    let mut unparsable = 0;
    for line in tail {
        match serde_json::from_str::<Value>(line) {
            Ok(value) => values.push(value),
            Err(_) => unparsable += 1,
        }
    }
    (values, unparsable)
}

#[cfg(test)]
mod local_enumerator_tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn home(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "local-enum-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(home: &Path, rel: &str, content: &str) {
        let path = home.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// A real codex layout: a large `session_meta`, then instruction messages,
    /// then the first real user message, after the shell's 32 KiB budget.
    fn codex_file(id: &str) -> String {
        let pad = |n: usize| "p".repeat(n);
        let mut lines = vec![serde_json::json!({
            "type": "session_meta",
            "payload": {"id": id, "cwd": "/synthetic/project", "instructions": pad(40_000)},
        })];
        for _ in 0..3 {
            lines.push(serde_json::json!({
                "type": "response_item",
                "payload": {"type": "message", "role": "developer",
                    "content": [{"type": "input_text", "text": pad(15_000)}]},
            }));
        }
        lines.push(serde_json::json!({
            "type": "response_item",
            "payload": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "fix the flaky stream test"}]},
        }));
        lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>()
    }

    fn run(
        enumerator: &mut LocalEnumerator,
        home: &Path,
    ) -> (Vec<AgentSessionRecord>, RemoteParseStats) {
        enumerator
            .enumerate(home, REMOTE_SCAN_TIMEOUT, || Ok(()))
            .unwrap()
    }

    #[test]
    fn codex_title_survives_where_the_shell_budget_loses_it() {
        let home = home("codex-title");
        write(
            &home,
            ".codex/sessions/2026/11/09/a.jsonl",
            &codex_file("codex-a"),
        );
        let (sessions, stats) = run(&mut LocalEnumerator::default(), &home);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "fix the flaky stream test");
        assert_eq!(stats, RemoteParseStats::default());
        // The same file through the SSH shell sampler: budget-stopped, no title.
        let shell = std::process::Command::new("sh")
            .args(["-c", &remote_enumeration_script()])
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        let (shell_sessions, shell_stats) =
            parse_remote_enumeration_records(&String::from_utf8_lossy(&shell.stdout));
        assert_eq!(shell_stats.truncated_files, 1);
        assert_eq!(shell_sessions[0].title, "codex codex-a");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn records_are_unstamped_and_capped_like_the_shell() {
        let home = home("unstamped");
        for index in 0..25 {
            write(
                &home,
                &format!(".claude/projects/p/{index}.jsonl"),
                &format!("{{\"sessionId\":\"c{index}\",\"cwd\":\"/synthetic\"}}\n"),
            );
        }
        let (sessions, stats) = run(&mut LocalEnumerator::default(), &home);
        // Newest 20 files per provider, like `head -n 20` after the mtime sort.
        assert_eq!(sessions.len(), REMOTE_MAX_FILES_PER_PROVIDER);
        assert!(sessions
            .iter()
            .all(|s| s.host.is_none() && s.resume_command.is_none() && s.repo_root.is_none()));
        assert_eq!(stats, RemoteParseStats::default());
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn changed_and_removed_files_are_re_read_not_served_from_cache() {
        let home = home("cache");
        let rel = ".claude/projects/p/a.jsonl";
        write(
            &home,
            rel,
            "{\"sessionId\":\"first\",\"cwd\":\"/synthetic\"}\n",
        );
        let mut enumerator = LocalEnumerator::default();
        assert_eq!(run(&mut enumerator, &home).0[0].session_id, "first");
        // Different length: the cache entry is stale even with an equal mtime.
        write(
            &home,
            rel,
            "{\"sessionId\":\"second-id\",\"cwd\":\"/synthetic\"}\n",
        );
        assert_eq!(run(&mut enumerator, &home).0[0].session_id, "second-id");
        fs::remove_file(home.join(rel)).unwrap();
        assert!(run(&mut enumerator, &home).0.is_empty());
        assert!(enumerator.cache.is_empty(), "removed files leave the cache");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_file_without_a_session_is_counted() {
        let home = home("no-session");
        write(
            &home,
            ".claude/projects/p/journal.jsonl",
            "{\"kind\":\"journal\"}\n",
        );
        write(&home, ".codex/sessions/bad.jsonl", "not json\n");
        let (sessions, stats) = run(&mut LocalEnumerator::default(), &home);
        assert!(sessions.is_empty());
        assert_eq!(stats.blocks_without_session, 2);
        // Like the shell path: stores hold non-session JSONL, so a file with no
        // session alone is counted but does not warn.
        assert_eq!(stats.warning(), None);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn cancellation_and_deadline_fail_the_whole_sample() {
        let home = home("cancel");
        write(
            &home,
            ".claude/projects/p/a.jsonl",
            "{\"sessionId\":\"a\",\"cwd\":\"/s\"}\n",
        );
        let mut enumerator = LocalEnumerator::default();
        let cancelled = enumerator.enumerate(&home, REMOTE_SCAN_TIMEOUT, || {
            Err(std::io::ErrorKind::BrokenPipe.into())
        });
        assert_eq!(
            cancelled.unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        let late = enumerator.enumerate(&home, Duration::ZERO, || {
            std::thread::sleep(Duration::from_millis(1));
            Ok(())
        });
        assert_eq!(late.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn kimi_index_tail_matches_the_shell_and_counts_bad_lines() {
        let home = home("kimi");
        let mut index = String::new();
        for n in 0..30 {
            index.push_str(&format!("{{\"sessionId\":\"k{n}\",\"workDir\":\"/w\"}}\n"));
        }
        index.push_str("{broken\n");
        write(&home, ".kimi-code/session_index.jsonl", &index);
        let (sessions, stats) = run(&mut LocalEnumerator::default(), &home);
        // The last 20 lines: k11..k29 plus the broken one.
        assert_eq!(sessions.len(), 19);
        assert!(sessions.iter().any(|s| s.session_id == "k29"));
        assert!(!sessions.iter().any(|s| s.session_id == "k10"));
        assert_eq!(stats.unparsable_lines, 1);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn kimi_tail_skips_the_fragment_of_a_mid_file_read() {
        let home = home("kimi-big");
        let filler = format!("{{\"pad\":\"{}\"}}\n", "z".repeat(200_000));
        let mut index = filler.repeat(6);
        index.push_str("{\"sessionId\":\"last\",\"workDir\":\"/w\"}\n");
        write(&home, ".kimi-code/session_index.jsonl", &index);
        let (sessions, stats) = run(&mut LocalEnumerator::default(), &home);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "last");
        assert_eq!(
            stats.unparsable_lines, 0,
            "the leading fragment is not a record"
        );
        fs::remove_dir_all(home).unwrap();
    }
}
