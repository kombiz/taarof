//! Live per-pane agent transcript bridge.
//!
//! Agent CLIs write complete structured transcripts to disk as the
//! conversation happens (Claude Code: `~/.claude/projects/<mangled-cwd>/
//! <sessionId>.jsonl`, with every message and tool call). taarof already
//! uses pane/process metadata when available and already parses these same files
//! for the resume feature ([`crate::agent_sessions`]); this module connects them
//! *live*, including new sessions whose CLI arguments do not expose an id.
//!
//! The design mirrors three existing patterns:
//! - the injectable-root, `serde_json::Value`-driven JSONL adapters from
//!   `agent_sessions.rs`;
//! - the off-main-thread `gio::spawn_blocking` probe flow with a single-flight
//!   guard from `app_runtime::update_agent_indicators`;
//! - the `StateSnapshotIngredients` serialization path in `api.rs`.
//!
//! Separation of concerns:
//! - [`TranscriptTailer`] — a generic append-only byte reader. It holds a byte
//!   offset plus a partial-line buffer and never re-reads consumed bytes.
//! - [`TranscriptAdapter`] — a per-agent trait: resolve a transcript path from
//!   `(session_id, cwd)` and fold complete JSONL lines into a
//!   [`TranscriptState`].
//! - [`ClaudeTranscriptAdapter`] — the real Claude Code adapter.
//! - provider adapters for Claude Code, Codex, Pi Agent, and Kimi native JSONL stores;
//! - [`TranscriptTracker`] — owns the tailers so byte offsets persist across
//!   ticks; its `sync_and_poll` performs ALL disk IO and is meant to run inside
//!   the blocking worker.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::agents::{AgentLifecycle, HeadlessAgentEvidence, PaneTurn, TurnMarker};
use crate::AppState;

/// Cap on retained tool calls / touched files per pane, drained from the front
/// on overflow so a long-lived pane cannot grow these vectors without bound.
const MAX_TOOL_CALLS: usize = 20;
const MAX_FILES: usize = 20;
const MAX_CHILD_AGENTS: usize = 32;
const MAX_SESSION_START_SKEW_MS: u64 = 5 * 60 * 1_000;
const MAX_PROCESS_ENVIRONMENT_BYTES: u64 = 2 * 1024 * 1024;
const TRANSCRIPT_DISCOVERY_CACHE_TTL: Duration = Duration::from_secs(2);

/// `(tab_id, pane_id)` — the same key shape used across the runtime probe.
pub(crate) type PaneTranscriptKey = (u32, u32);

/// A single tool invocation seen in an assistant turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToolCall {
    /// Tool name, e.g. `Read`, `Edit`, `Bash`.
    pub tool: String,
    /// Primary argument: a file path, a shell command, or a search pattern.
    pub target: Option<String>,
}

/// Whether a file was created (`Write`) or edited (`Edit`/`MultiEdit`/
/// `NotebookEdit`) by the agent. Reads are deliberately excluded — the
/// recent-files feed is about *what the agent made*, not what it looked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileOp {
    Created,
    Edited,
}

impl FileOp {
    /// Human-readable verb for UI labels, e.g. "wrote report.md".
    pub fn verb(self) -> &'static str {
        match self {
            FileOp::Created => "wrote",
            FileOp::Edited => "edited",
        }
    }

    /// Stable lowercase token for the API/event wire shape.
    pub fn wire(self) -> &'static str {
        match self {
            FileOp::Created => "write",
            FileOp::Edited => "edit",
        }
    }
}

/// One file the agent created or edited, with the operation and when it was
/// last touched. The recent-files feed keeps one entry per path (newest op).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TouchedFile {
    pub path: String,
    pub op: FileOp,
    pub at_unix_ms: u64,
}

/// Provider-native identity for the currently folded turn. This is additive
/// correlation evidence: the generic pane/event boundary remains authoritative.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub(crate) struct ProviderNativeTurnId {
    pub provider: String,
    pub id: String,
    pub observed_at_unix_ms: u64,
}

/// Rolling ground-truth summary of one pane's agent conversation, folded from
/// the on-disk transcript.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TranscriptState {
    /// Normalised agent kind, e.g. `claude`.
    pub agent: String,
    /// Session id this state was folded from.
    pub session_id: String,
    /// Last assistant message text (raw markdown), if any.
    pub last_message: Option<String>,
    /// Distinct files the agent touched, most-recent-last, capped.
    pub files_touched: Vec<String>,
    /// Files the agent created or edited (not read), deduped by path keeping the
    /// latest operation, most-recent-last, capped. Drives the recent-files feed.
    pub recent_files: Vec<TouchedFile>,
    /// Recent tool calls, most-recent-last, capped.
    pub recent_tool_calls: Vec<ToolCall>,
    /// Total assistant messages folded so far.
    pub message_count: u64,
    /// Wall-clock of the last fold that changed anything.
    pub updated_at_unix_ms: u64,
    /// Where the agent sits in its current turn, stamped with the transcript
    /// record's own timestamp. This is the trustworthy native evidence the
    /// canonical state machine uses; see [`crate::agents::lifecycle`].
    pub turn: PaneTurn,
    /// Native turn identity when the bound provider exposes one. Cleared at a
    /// new turn boundary if that boundary does not carry a valid identity.
    pub native_turn_id: Option<ProviderNativeTurnId>,
    /// Headless child agents folded from provider-native structured evidence.
    /// They inherit this transcript's real pane only during shared projection.
    pub child_agents: Vec<HeadlessAgentEvidence>,
}

fn parent_agent_id(state: &TranscriptState, provider: &str) -> String {
    format!("{provider}:{}", state.session_id)
}

fn child_label(block: &Value) -> String {
    block
        .pointer("/input/name")
        .or_else(|| block.pointer("/input/description"))
        .or_else(|| block.pointer("/input/subagent_type"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("subagent")
        .chars()
        .take(80)
        .collect()
}

fn child_activity(block: &Value) -> String {
    block
        .pointer("/input/description")
        .or_else(|| block.pointer("/input/prompt"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("working")
        .chars()
        .take(160)
        .collect()
}

fn upsert_child_agent(state: &mut TranscriptState, child: HeadlessAgentEvidence) {
    if let Some(existing) = state
        .child_agents
        .iter_mut()
        .find(|existing| existing.stable_id == child.stable_id)
    {
        *existing = child;
        return;
    }
    push_capped(&mut state.child_agents, child, MAX_CHILD_AGENTS);
}

fn complete_child_agent(
    state: &mut TranscriptState,
    provider: &str,
    native_id: &str,
    errored: bool,
    at_unix_ms: u64,
) {
    let stable_id = format!("{provider}:{native_id}");
    let Some(child) = state
        .child_agents
        .iter_mut()
        .find(|child| child.stable_id == stable_id)
    else {
        return;
    };
    child.state = if errored {
        AgentLifecycle::Errored
    } else {
        AgentLifecycle::Done
    };
    child.activity = if errored { "errored" } else { "completed" }.to_string();
    child.updated_at_unix_ms = at_unix_ms;
}

fn fold_claude_child_agents(value: &Value, state: &mut TranscriptState, now: u64) {
    let at = record_timestamp_unix_ms(value).unwrap_or(now);
    let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use")
                if matches!(
                    block.get("name").and_then(Value::as_str),
                    Some("Task" | "Agent")
                ) =>
            {
                let Some(native_id) = block.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let stable_id = format!("claude:{native_id}");
                upsert_child_agent(
                    state,
                    HeadlessAgentEvidence {
                        stable_id,
                        parent_id: parent_agent_id(state, "claude"),
                        provider: "claude".to_string(),
                        label: child_label(block),
                        state: AgentLifecycle::Working,
                        activity: child_activity(block),
                        updated_at_unix_ms: at,
                    },
                );
            }
            Some("tool_result") => {
                let Some(native_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                    continue;
                };
                let errored = block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                complete_child_agent(state, "claude", native_id, errored, at);
            }
            _ => {}
        }
    }
}

fn codex_child_message(value: &Value) -> Option<(&str, &str)> {
    let text = value
        .pointer("/payload/content/0/text")
        .and_then(Value::as_str)?;
    let message_type = text
        .lines()
        .find_map(|line| line.strip_prefix("Message Type: "))?;
    let task_name = text
        .lines()
        .find_map(|line| line.strip_prefix("Task name: "))?;
    Some((message_type.trim(), task_name.trim()))
}

fn fold_codex_child_agents(value: &Value, state: &mut TranscriptState, now: u64) {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return;
    }
    let at = record_timestamp_unix_ms(value).unwrap_or(now);
    match value.pointer("/payload/type").and_then(Value::as_str) {
        Some("function_call")
            if value.pointer("/payload/name").and_then(Value::as_str) == Some("spawn_agent") =>
        {
            let Some(call_id) = value.pointer("/payload/call_id").and_then(Value::as_str) else {
                return;
            };
            let Some(arguments) = value
                .pointer("/payload/arguments")
                .and_then(Value::as_str)
                .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
            else {
                return;
            };
            let Some(task_name) = arguments.get("task_name").and_then(Value::as_str) else {
                return;
            };
            let activity = arguments
                .get("message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|message| !message.is_empty())
                .unwrap_or("working")
                .chars()
                .take(160)
                .collect();
            upsert_child_agent(
                state,
                HeadlessAgentEvidence {
                    stable_id: format!("codex:{call_id}"),
                    parent_id: parent_agent_id(state, "codex"),
                    provider: "codex".to_string(),
                    label: task_name.chars().take(80).collect(),
                    state: AgentLifecycle::Working,
                    activity,
                    updated_at_unix_ms: at,
                },
            );
        }
        Some("function_call_output") => {
            let Some(call_id) = value.pointer("/payload/call_id").and_then(Value::as_str) else {
                return;
            };
            let pending_id = format!("codex:{call_id}");
            let pending_index = state
                .child_agents
                .iter()
                .position(|child| child.stable_id == pending_id);
            let task_name = value
                .pointer("/payload/output")
                .and_then(Value::as_str)
                .and_then(|output| serde_json::from_str::<Value>(output).ok())
                .and_then(|output| {
                    output
                        .get("task_name")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                });
            let Some(task_name) = task_name else {
                if let Some(pending) = pending_index {
                    state.child_agents.remove(pending);
                }
                return;
            };
            let canonical_id = format!("codex:{task_name}");
            let canonical_index = state
                .child_agents
                .iter()
                .position(|child| child.stable_id == canonical_id);
            match (canonical_index, pending_index) {
                (Some(canonical), Some(pending)) => {
                    state.child_agents[canonical].updated_at_unix_ms = at;
                    state.child_agents.remove(pending);
                }
                (None, Some(pending)) => {
                    state.child_agents[pending].stable_id = canonical_id;
                    state.child_agents[pending].updated_at_unix_ms = at;
                }
                _ => {}
            }
        }
        Some("agent_message") => {
            let Some((message_type, task_name)) = codex_child_message(value) else {
                return;
            };
            match message_type {
                "FINAL_ANSWER" => complete_child_agent(state, "codex", task_name, false, at),
                "MESSAGE" => {
                    let stable_id = format!("codex:{task_name}");
                    if let Some(child) = state
                        .child_agents
                        .iter_mut()
                        .find(|child| child.stable_id == stable_id)
                    {
                        child.activity = "updated".to_string();
                        child.updated_at_unix_ms = at;
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Lossless change set produced while folding one transcript poll. Rolling UI
/// state remains capped; these file operations are consumed exactly once by
/// the work-ledger projection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TranscriptDelta {
    items: Vec<TranscriptDeltaItem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TranscriptDeltaItem {
    AssistantCompleted,
    FileOperation(TouchedFile),
}

impl TranscriptDelta {
    #[cfg(test)]
    fn assistant_messages(&self) -> usize {
        self.items
            .iter()
            .filter(|item| matches!(item, TranscriptDeltaItem::AssistantCompleted))
            .count()
    }

    #[cfg(test)]
    fn file_events(&self) -> Vec<&TouchedFile> {
        self.items
            .iter()
            .filter_map(|item| match item {
                TranscriptDeltaItem::FileOperation(file) => Some(file),
                TranscriptDeltaItem::AssistantCompleted => None,
            })
            .collect()
    }
}

/// A live pane that should have its transcript tailed. The runtime session id is
/// optional because fresh Claude, Codex, and Pi processes generally do not put
/// their generated id in argv. Pane identity and cwd remain available for
/// agent-native discovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaneTranscriptBinding {
    pub key: PaneTranscriptKey,
    pub agent: String,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// Pane shell/root pid. Resolution walks its live process tree off-thread
    /// to find the supported agent process and correlate its start time.
    pub shell_pid: Option<i32>,
    /// Injectable start time for deterministic fixture tests. Production
    /// bindings leave this empty and derive it from `shell_pid` under `/proc`.
    pub process_started_at_unix_ms: Option<u64>,
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn push_capped<T>(items: &mut Vec<T>, item: T, cap: usize) {
    items.push(item);
    if items.len() > cap {
        let excess = items.len() - cap;
        items.drain(0..excess);
    }
}

/// Classify a tool name into a [`FileOp`] for the recent-files feed. Only
/// create/edit tools qualify; reads and everything else return `None`.
fn file_op_for_tool(tool: &str) -> Option<FileOp> {
    match tool {
        "Write" => Some(FileOp::Created),
        "Edit" | "MultiEdit" | "NotebookEdit" => Some(FileOp::Edited),
        _ => None,
    }
}

/// Extract file evidence only from an allowlisted native create/edit tool with
/// a dedicated path field. Shell commands, prose, patch bodies, and arbitrary
/// argument strings are intentionally never interpreted as file operations.
fn structured_file_event(tool: &str, input: Option<&Value>, at: u64) -> Option<TouchedFile> {
    let op = match tool {
        "Write" | "write" | "write_file" | "create_file" => FileOp::Created,
        "Edit" | "MultiEdit" | "NotebookEdit" | "edit" | "edit_file" => FileOp::Edited,
        _ => return None,
    };
    let input = input?.as_object()?;
    let path = input
        .get("file_path")
        .or_else(|| input.get("path"))?
        .as_str()?
        .trim();
    if path.is_empty() || path.contains(['\0', '\n', '\r']) {
        return None;
    }
    Some(TouchedFile {
        path: path.to_string(),
        op,
        at_unix_ms: at,
    })
}

/// Record a created/edited file in the recent-files feed: dedupe by path
/// (dropping any earlier entry for the same path) so the newest operation wins
/// and moves to the end, then cap the list from the front on overflow.
fn push_recent_file(files: &mut Vec<TouchedFile>, path: String, op: FileOp, at: u64) {
    files.retain(|f| f.path != path);
    files.push(TouchedFile {
        path,
        op,
        at_unix_ms: at,
    });
    if files.len() > MAX_FILES {
        let excess = files.len() - MAX_FILES;
        files.drain(0..excess);
    }
}

/// Resolve a transcript-recorded file path against a pane cwd. Absolute paths
/// are returned verbatim; a relative path is joined onto `cwd` when known, and
/// left relative otherwise. This is the pure core of the recent-files picker's
/// existence check (the disk IO itself happens at the call site).
pub(crate) fn resolve_touched_file_path(path: &str, cwd: Option<&str>) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else if let Some(cwd) = cwd {
        Path::new(cwd).join(p)
    } else {
        p.to_path_buf()
    }
}

/// Append-only byte reader over a single transcript file.
///
/// The tailer remembers how many bytes it has already consumed (`offset`) and
/// buffers a trailing unterminated line (`partial`) so a JSONL record split
/// across two reads is never lost or double-counted. Each [`poll_new_lines`]
/// reads only the bytes appended since the previous poll — it never re-reads
/// the whole file. Byte-based splitting avoids slicing a multi-byte UTF-8
/// sequence at a read boundary.
///
/// [`poll_new_lines`]: TranscriptTailer::poll_new_lines
pub(crate) struct TranscriptTailer {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    reset_during_poll: bool,
    file_identity: Option<(u64, u64)>,
}

impl TranscriptTailer {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
            reset_during_poll: false,
            file_identity: None,
        }
    }

    /// Read bytes appended since the last poll and return the newly completed
    /// lines (trailing `\r`/`\n` trimmed, empty lines dropped). A file that
    /// shrank below the current offset or was replaced with another inode is
    /// treated as rotated/truncated and read again from the start.
    pub(crate) fn poll_new_lines(&mut self) -> std::io::Result<Vec<String>> {
        self.reset_during_poll = false;
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };

        let metadata = file.metadata()?;
        let len = metadata.len();
        let identity = transcript_file_identity(&metadata);
        let replaced = self
            .file_identity
            .zip(identity)
            .is_some_and(|(previous, current)| previous != current);
        self.file_identity = identity;
        if replaced || len < self.offset {
            // The file was truncated or rotated under us: restart from the top.
            self.offset = 0;
            self.partial.clear();
            self.reset_during_poll = true;
        }
        if len == self.offset {
            return Ok(Vec::new());
        }

        file.seek(SeekFrom::Start(self.offset))?;
        let to_read = len - self.offset;
        let mut buf = Vec::with_capacity(to_read as usize);
        let read = file.take(to_read).read_to_end(&mut buf)?;
        self.offset += read as u64;
        self.partial.extend_from_slice(&buf);

        let mut lines = Vec::new();
        let mut consumed = 0usize;
        while let Some(pos) = self.partial[consumed..]
            .iter()
            .position(|&byte| byte == b'\n')
        {
            let end = consumed + pos;
            let raw = &self.partial[consumed..end];
            let text = String::from_utf8_lossy(raw);
            let trimmed = text.trim_end_matches('\r');
            if !trimmed.is_empty() {
                lines.push(trimmed.to_string());
            }
            consumed = end + 1;
        }
        if consumed > 0 {
            self.partial.drain(0..consumed);
        }

        Ok(lines)
    }

    fn take_reset_during_poll(&mut self) -> bool {
        std::mem::take(&mut self.reset_during_poll)
    }
}

#[cfg(unix)]
fn transcript_file_identity(metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn transcript_file_identity(_metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    // Length-based truncation detection remains the safe portable fallback.
    None
}

/// Per-agent transcript adapter: resolve a file path and fold JSONL lines into
/// a [`TranscriptState`].
pub(crate) trait TranscriptAdapter: Send + Sync {
    /// Normalised agent kind this adapter handles, e.g. `claude`.
    fn agent_kind(&self) -> &'static str;

    /// Resolve the transcript file for a session. Returns `None` when the file
    /// cannot be located, so unresolvable panes degrade silently.
    fn resolve_path(
        &self,
        session_id: Option<&str>,
        cwd: Option<&str>,
        process_started_at_unix_ms: Option<u64>,
        provider_sessions_root: Option<&Path>,
    ) -> Option<ResolvedTranscript>;

    /// Fold complete JSONL lines into `state`, returning typed changes seen in
    /// this exact batch. The returned file list is intentionally not capped.
    fn ingest_lines(&self, lines: &[String], state: &mut TranscriptState) -> TranscriptDelta;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedTranscript {
    path: PathBuf,
    session_id: String,
}

#[derive(Clone, Debug)]
struct NativeTranscriptCandidate {
    path: PathBuf,
    session_id: String,
    cwd: String,
    started_at_unix_ms: Option<u64>,
}

#[derive(Default)]
struct NativeCandidateCache {
    inner: Mutex<HashMap<PathBuf, CachedNativeCandidates>>,
}

struct CachedNativeCandidates {
    observed_at: Instant,
    candidates: HashMap<PathBuf, NativeTranscriptCandidate>,
}

impl NativeCandidateCache {
    fn get_or_scan(
        &self,
        root: &Path,
        parse: fn(PathBuf) -> Option<NativeTranscriptCandidate>,
    ) -> Vec<NativeTranscriptCandidate> {
        let mut caches = self
            .inner
            .lock()
            .expect("native transcript candidate cache lock poisoned");
        if let Some(cached) = caches
            .get(root)
            .filter(|cached| cached.observed_at.elapsed() < TRANSCRIPT_DISCOVERY_CACHE_TTL)
        {
            return cached.candidates.values().cloned().collect();
        }

        // Refresh the path index but parse only files not seen in an earlier
        // scan. This keeps an unresolved pane from reparsing large native
        // stores every two seconds while still discovering newly-created files.
        let paths = jsonl_paths_recursive(root);
        let live_paths: HashSet<PathBuf> = paths.iter().cloned().collect();
        let cache = caches
            .entry(root.to_path_buf())
            .or_insert_with(|| CachedNativeCandidates {
                observed_at: Instant::now(),
                candidates: HashMap::new(),
            });
        cache.candidates.retain(|path, _| live_paths.contains(path));
        for path in paths {
            if cache.candidates.contains_key(&path) {
                continue;
            }
            if let Some(candidate) = parse(path.clone()) {
                cache.candidates.insert(path, candidate);
            }
        }
        cache.observed_at = Instant::now();
        cache.candidates.values().cloned().collect()
    }
}

fn system_time_unix_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

fn file_created_unix_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .created()
        .ok()
        .and_then(system_time_unix_ms)
}

fn parse_rfc3339_unix_ms(value: &str) -> Option<u64> {
    let value = value.strip_suffix('Z')?;
    let (date, time) = value.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second_part = time_parts.next()?;
    let (second, millis) = match second_part.split_once('.') {
        Some((second, fraction)) => {
            let mut millis = fraction.chars().take(3).collect::<String>();
            while millis.len() < 3 {
                millis.push('0');
            }
            (second.parse::<i64>().ok()?, millis.parse::<u64>().ok()?)
        }
        None => (second_part.parse::<i64>().ok()?, 0),
    };

    // Howard Hinnant's civil-date conversion, yielding days since 1970-01-01.
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour.checked_mul(3_600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?;
    u64::try_from(seconds)
        .ok()?
        .checked_mul(1_000)?
        .checked_add(millis)
}

/// The timestamp a native record carries for itself. Claude, Codex, and Pi all
/// stamp every record; Codex's `session_meta` keeps its timestamp one level
/// down. Using the record's own clock — never the fold clock — is what stops a
/// replayed transcript from looking like live work.
fn record_timestamp_unix_ms(value: &Value) -> Option<u64> {
    value
        .get("timestamp")
        .or_else(|| value.pointer("/payload/timestamp"))
        .and_then(|timestamp| {
            timestamp
                .as_str()
                .and_then(parse_rfc3339_unix_ms)
                .or_else(|| timestamp.as_u64())
        })
        .or_else(|| value.get("time").and_then(Value::as_u64))
}

/// Fold one record's turn marker into `state`. Records are appended in order,
/// so the last marker in a batch is the current phase. `fold_at_unix_ms` is
/// only a fallback for fixtures and providers that omit a timestamp.
fn apply_turn_marker(
    state: &mut TranscriptState,
    value: &Value,
    marker: Option<TurnMarker>,
    fold_at_unix_ms: u64,
) {
    let Some(marker) = marker else {
        return;
    };
    let at = record_timestamp_unix_ms(value).unwrap_or(fold_at_unix_ms);
    state.turn = PaneTurn::new(marker.phase(), at);
}

fn bounded_native_id(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control))
        .then(|| value.to_string())
}

fn apply_native_turn_id(
    state: &mut TranscriptState,
    value: &Value,
    marker: Option<TurnMarker>,
    provider: &str,
    fold_at_unix_ms: u64,
) {
    let Some(marker) = marker else {
        return;
    };
    if marker == TurnMarker::Started
        && !matches!(state.turn.phase, crate::agents::TurnPhase::Active)
    {
        // Never let a previous turn's native identity bleed into a new turn
        // whose provider record is missing or malformed.
        state.native_turn_id = None;
    }
    let candidate = match provider {
        "claude" if marker == TurnMarker::Started => {
            bounded_native_id(value.get("uuid").and_then(Value::as_str))
        }
        "codex" => bounded_native_id(value.pointer("/payload/turn_id").and_then(Value::as_str)),
        _ => None,
    };
    if let Some(id) = candidate {
        state.native_turn_id = Some(ProviderNativeTurnId {
            provider: provider.to_string(),
            id,
            observed_at_unix_ms: record_timestamp_unix_ms(value).unwrap_or(fold_at_unix_ms),
        });
    }
}

/// Claude Code: `assistant` records carry a `stop_reason`. Only a genuine
/// end-of-response closes the turn — `tool_use`, an explicit `null` (the
/// record is still streaming), and a missing field all leave it open. `user`
/// records are either a fresh prompt or a tool result feeding the open turn.
fn claude_turn_marker(value: &Value) -> Option<TurnMarker> {
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        // A subagent's turn is not this pane's turn.
        return None;
    }
    match value.get("type").and_then(Value::as_str)? {
        "user" => {
            if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            let is_tool_result = value.get("toolUseResult").is_some()
                || value
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| {
                        blocks.iter().any(|block| {
                            block.get("type").and_then(Value::as_str) == Some("tool_result")
                        })
                    });
            Some(if is_tool_result {
                TurnMarker::Progress
            } else {
                TurnMarker::Started
            })
        }
        "assistant" => {
            if value.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) {
                return Some(TurnMarker::Errored);
            }
            match value
                .pointer("/message/stop_reason")
                .and_then(Value::as_str)
            {
                Some("end_turn" | "stop_sequence" | "max_tokens" | "refusal") => {
                    Some(TurnMarker::Completed)
                }
                _ => Some(TurnMarker::Progress),
            }
        }
        _ => None,
    }
}

/// Codex: the rollout stream states its own turn boundaries via `event_msg`.
/// The allowlists matter — `thread_settings_applied` is written *after*
/// `task_complete`, so a catch-all "any event means progress" rule would
/// reopen every finished turn.
fn codex_turn_marker(value: &Value) -> Option<TurnMarker> {
    match value.get("type").and_then(Value::as_str)? {
        "event_msg" => match value.pointer("/payload/type").and_then(Value::as_str)? {
            "task_started" | "user_message" => Some(TurnMarker::Started),
            "task_complete" | "turn_aborted" | "shutdown_complete" => Some(TurnMarker::Completed),
            "error" | "stream_error" => Some(TurnMarker::Errored),
            "agent_message"
            | "agent_message_delta"
            | "agent_reasoning"
            | "agent_reasoning_delta"
            | "agent_reasoning_raw_content"
            | "agent_reasoning_raw_content_delta"
            | "agent_reasoning_section_break"
            | "token_count"
            | "context_compacted"
            | "exec_command_begin"
            | "exec_command_end"
            | "exec_command_output_delta"
            | "patch_apply_begin"
            | "patch_apply_end"
            | "mcp_tool_call_begin"
            | "mcp_tool_call_end"
            | "web_search_begin"
            | "web_search_end"
            | "sub_agent_activity" => Some(TurnMarker::Progress),
            _ => None,
        },
        "response_item" => match value.pointer("/payload/type").and_then(Value::as_str)? {
            "message" if value.pointer("/payload/role").and_then(Value::as_str) == Some("user") => {
                Some(TurnMarker::Started)
            }
            "message"
            | "reasoning"
            | "function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"
            | "agent_message"
            | "local_shell_call"
            | "web_search_call"
            | "tool_search_call"
            | "tool_search_call_output" => Some(TurnMarker::Progress),
            _ => None,
        },
        _ => None,
    }
}

/// Pi Agent: assistant records carry a `stopReason`. `toolUse` keeps the turn
/// open, `stop`/`aborted` hand control back, `error` fails the turn. A record
/// written before the stop reason is known counts as progress, never as a
/// completed response.
fn pi_turn_marker(value: &Value) -> Option<TurnMarker> {
    if value.get("type").and_then(Value::as_str)? != "message" {
        return None;
    }
    match value.pointer("/message/role").and_then(Value::as_str)? {
        "user" => Some(TurnMarker::Started),
        "toolResult" => Some(TurnMarker::Progress),
        "assistant" => Some(
            match value.pointer("/message/stopReason").and_then(Value::as_str) {
                Some("stop" | "aborted" | "endTurn" | "maxTokens") => TurnMarker::Completed,
                Some("error") => TurnMarker::Errored,
                _ => TurnMarker::Progress,
            },
        ),
        _ => None,
    }
}

/// Kimi Code's wire log brackets each model step. A prompt or step start opens
/// work; a final `step.end` hands control back. Tool and streamed-content
/// events keep the current step active. Because records are folded in order, a
/// step.end followed immediately by the next step.begin resolves to Active in
/// the same batch.
fn kimi_turn_marker(value: &Value) -> Option<TurnMarker> {
    match value.get("type").and_then(Value::as_str)? {
        "turn.prompt" | "turn.steer" | "llm.request" => Some(TurnMarker::Started),
        "turn.cancel" => Some(TurnMarker::Completed),
        "context.append_loop_event" => {
            match value.pointer("/event/type").and_then(Value::as_str)? {
                "step.begin" | "content.part" | "tool.call" | "tool.result" => {
                    Some(TurnMarker::Progress)
                }
                "step.end" => Some(TurnMarker::Completed),
                _ => None,
            }
        }
        _ => None,
    }
}

fn first_native_timestamp(path: &Path) -> Option<u64> {
    let reader = BufReader::new(File::open(path).ok()?);
    reader
        .lines()
        .take(64)
        .filter_map(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .find_map(|value| {
            record_timestamp_unix_ms(&value)
                .or_else(|| value.get("created_at").and_then(Value::as_u64))
        })
}

fn jsonl_paths_recursive(root: &Path) -> Vec<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut paths = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            {
                paths.push(path);
            }
        }
    }
    paths
}

fn select_native_candidate(
    candidates: Vec<NativeTranscriptCandidate>,
    session_id: Option<&str>,
    cwd: Option<&str>,
    process_started_at_unix_ms: Option<u64>,
) -> Option<ResolvedTranscript> {
    let mut matching: Vec<NativeTranscriptCandidate> = if let Some(session_id) = session_id {
        candidates
            .into_iter()
            .filter(|candidate| candidate.session_id == session_id)
            .collect()
    } else {
        let cwd = cwd?;
        candidates
            .into_iter()
            .filter(|candidate| candidate.cwd == cwd)
            .collect()
    };

    if session_id.is_some() && matching.len() == 1 {
        let candidate = matching.pop()?;
        return Some(ResolvedTranscript {
            path: candidate.path,
            session_id: candidate.session_id,
        });
    }
    if session_id.is_some() || matching.is_empty() {
        return None;
    }

    // Multiple native sessions can share a cwd. Correlate the actual agent
    // process start with transcript creation; never guess by newest cwd alone.
    let process_start = process_started_at_unix_ms?;
    let mut ranked: Vec<(u64, NativeTranscriptCandidate)> = matching
        .into_iter()
        .filter_map(|candidate| {
            let started = candidate.started_at_unix_ms?;
            Some((started.abs_diff(process_start), candidate))
        })
        .filter(|(distance, _)| *distance <= MAX_SESSION_START_SKEW_MS)
        .collect();
    ranked.sort_by_key(|(distance, _)| *distance);
    let (best_distance, best) = ranked.first()?.clone();
    if ranked
        .get(1)
        .is_some_and(|(next_distance, _)| *next_distance == best_distance)
    {
        return None;
    }
    Some(ResolvedTranscript {
        path: best.path,
        session_id: best.session_id,
    })
}

fn command_basename(value: &str) -> String {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .to_ascii_lowercase()
}

fn process_matches_agent(pid: i32, agent: &str) -> bool {
    let aliases: &[&str] = match agent {
        "claude" => &["claude"],
        "codex" => &["codex"],
        "pi" => &["pi", "pii"],
        "kimi" => &["kimi", "kimi-code"],
        _ => return false,
    };
    crate::agents::get_process_comm(pid)
        .into_iter()
        .chain(
            crate::agents::get_process_cmdline(pid)
                .into_iter()
                .flatten()
                .take(2),
        )
        .map(|value| command_basename(&value))
        .any(|candidate| aliases.contains(&candidate.as_str()))
}

fn find_agent_process_pid(shell_pid: i32, agent: &str) -> Option<i32> {
    let mut queue = VecDeque::from([shell_pid]);
    let mut seen = HashSet::new();
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        if process_matches_agent(pid, agent) {
            return Some(pid);
        }
        queue.extend(crate::agents::get_child_pids(pid));
    }
    None
}

fn process_start_unix_ms(pid: i32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    // `after_comm` begins at field 3 (state); process start ticks are field 22.
    let start_ticks: u64 = after_comm.split_whitespace().nth(19)?.parse().ok()?;
    let boot_seconds: u64 = fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .parse()
        .ok()?;
    // SAFETY: sysconf is a read-only libc query with no pointer arguments.
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        return None;
    }
    Some(
        boot_seconds
            .saturating_mul(1_000)
            .saturating_add(start_ticks.saturating_mul(1_000) / ticks_per_second as u64),
    )
}

fn binding_process_start(binding: &PaneTranscriptBinding) -> Option<u64> {
    binding.process_started_at_unix_ms.or_else(|| {
        let shell_pid = binding.shell_pid?;
        let agent_pid = find_agent_process_pid(shell_pid, &binding.agent)?;
        process_start_unix_ms(agent_pid)
    })
}

fn binding_agent_cwd(binding: &PaneTranscriptBinding) -> Option<String> {
    binding.cwd.clone().or_else(|| {
        let shell_pid = binding.shell_pid?;
        let agent_pid = find_agent_process_pid(shell_pid, &binding.agent)?;
        fs::read_link(format!("/proc/{agent_pid}/cwd"))
            .ok()?
            .to_str()
            .map(str::to_owned)
    })
}

/// Read one named value from a live same-user process without copying or
/// logging the rest of its environment. Linux bounds the environment at exec,
/// but keep an explicit cap so a procfs read can never grow without limit.
fn process_environment_value(pid: i32, name: &str) -> Option<OsString> {
    if name.is_empty() || name.as_bytes().contains(&b'=') || name.as_bytes().contains(&0) {
        return None;
    }
    let mut reader = BufReader::new(File::open(format!("/proc/{pid}/environ")).ok()?);
    let mut bytes_read = 0_u64;
    let mut prefix = name.as_bytes().to_vec();
    prefix.push(b'=');
    loop {
        let remaining = MAX_PROCESS_ENVIRONMENT_BYTES.checked_sub(bytes_read)?;
        let mut entry = Vec::new();
        let read = (&mut reader)
            .take(remaining + 1)
            .read_until(0, &mut entry)
            .ok()?;
        if read == 0 {
            return None;
        }
        bytes_read += read as u64;
        if bytes_read > MAX_PROCESS_ENVIRONMENT_BYTES {
            return None;
        }
        if entry.last() == Some(&0) {
            entry.pop();
        }
        if let Some(value) = entry
            .strip_prefix(prefix.as_slice())
            .filter(|value| !value.is_empty())
        {
            return Some(OsString::from_vec(value.to_vec()));
        }
    }
}

fn binding_codex_sessions_root(binding: &PaneTranscriptBinding) -> Option<PathBuf> {
    if binding.agent != "codex" {
        return None;
    }
    let shell_pid = binding.shell_pid?;
    let agent_pid = find_agent_process_pid(shell_pid, &binding.agent)?;
    let mut codex_home = PathBuf::from(process_environment_value(agent_pid, "CODEX_HOME")?);
    if codex_home.is_relative() {
        codex_home = fs::read_link(format!("/proc/{agent_pid}/cwd"))
            .ok()?
            .join(codex_home);
    }
    codex_sessions_root(Some(codex_home), None)
}

/// Map a cwd to Claude Code's project-directory name. Claude replaces every `/`
/// and `.` with `-` (verified on-disk against
/// `~/.claude/projects/-home-developer-...`).
fn mangle_claude_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|ch| if ch == '/' || ch == '.' { '-' } else { ch })
        .collect()
}

/// Claude Code adapter over `~/.claude/projects/<mangled-cwd>/<sessionId>.jsonl`.
pub(crate) struct ClaudeTranscriptAdapter {
    projects_root: Option<PathBuf>,
}

impl Default for ClaudeTranscriptAdapter {
    fn default() -> Self {
        Self {
            projects_root: dirs::home_dir().map(|home| home.join(".claude/projects")),
        }
    }
}

impl ClaudeTranscriptAdapter {
    #[cfg(test)]
    pub(crate) fn with_projects_root(root: PathBuf) -> Self {
        Self {
            projects_root: Some(root),
        }
    }
}

impl TranscriptAdapter for ClaudeTranscriptAdapter {
    fn agent_kind(&self) -> &'static str {
        "claude"
    }

    fn resolve_path(
        &self,
        session_id: Option<&str>,
        cwd: Option<&str>,
        process_started_at_unix_ms: Option<u64>,
        _provider_sessions_root: Option<&Path>,
    ) -> Option<ResolvedTranscript> {
        let root = self.projects_root.as_ref()?;
        if !root.exists() {
            return None;
        }

        // Fresh Claude processes do not expose their generated session id in
        // argv. The cwd maps directly to Claude's project store; correlate its
        // top-level transcripts (never subagent transcripts) with process start.
        if session_id.is_none() {
            let cwd = cwd?;
            let project_dir = root.join(mangle_claude_cwd(cwd));
            let candidates = fs::read_dir(project_dir)
                .ok()?
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    if !path.is_file()
                        || path.extension().and_then(|ext| ext.to_str()) != Some("jsonl")
                    {
                        return None;
                    }
                    Some(NativeTranscriptCandidate {
                        session_id: path.file_stem()?.to_str()?.to_string(),
                        started_at_unix_ms: first_native_timestamp(&path)
                            .or_else(|| file_created_unix_ms(&path)),
                        path,
                        cwd: cwd.to_string(),
                    })
                })
                .collect();
            return select_native_candidate(
                candidates,
                None,
                Some(cwd),
                process_started_at_unix_ms,
            );
        }

        let session_id = session_id?;
        let filename = format!("{session_id}.jsonl");

        // Fast path: derive the project dir from the pane cwd.
        if let Some(cwd) = cwd {
            let candidate = root.join(mangle_claude_cwd(cwd)).join(&filename);
            if candidate.exists() {
                return Some(ResolvedTranscript {
                    path: candidate,
                    session_id: session_id.to_string(),
                });
            }
        }

        // Fallback: scan one level of project subdirs for the session file. This
        // is robust even if the mangling guess above is wrong for an odd cwd.
        let entries = fs::read_dir(root).ok()?;
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let candidate = entry.path().join(&filename);
            if candidate.is_file() {
                return Some(ResolvedTranscript {
                    path: candidate,
                    session_id: session_id.to_string(),
                });
            }
        }

        None
    }

    fn ingest_lines(&self, lines: &[String], state: &mut TranscriptState) -> TranscriptDelta {
        let now = now_unix_ms();
        let mut count = 0usize;
        let mut items = Vec::new();
        for line in lines {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let marker = claude_turn_marker(&value);
            fold_claude_child_agents(&value, state, now);
            apply_native_turn_id(state, &value, marker, "claude", now);
            apply_turn_marker(state, &value, marker, now);
            if value.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            if value.pointer("/message/role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            count += 1;

            let mut text_parts: Vec<String> = Vec::new();
            if let Some(Value::Array(blocks)) = value.pointer("/message/content") {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    text_parts.push(text.to_string());
                                }
                            }
                        }
                        Some("tool_use") => {
                            let tool = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let input = block.get("input");
                            let file_path = input
                                .and_then(|input| input.get("file_path"))
                                .and_then(Value::as_str);
                            let target = file_path
                                .or_else(|| {
                                    input
                                        .and_then(|input| input.get("command"))
                                        .and_then(Value::as_str)
                                })
                                .or_else(|| {
                                    input
                                        .and_then(|input| input.get("pattern"))
                                        .and_then(Value::as_str)
                                })
                                .map(str::to_string);
                            // Classify the file op before `tool` is moved into
                            // the ToolCall below.
                            let file_op = file_op_for_tool(&tool);
                            if !tool.is_empty() || target.is_some() {
                                push_capped(
                                    &mut state.recent_tool_calls,
                                    ToolCall { tool, target },
                                    MAX_TOOL_CALLS,
                                );
                            }
                            if let Some(path) = file_path {
                                let path = path.to_string();
                                if !state.files_touched.contains(&path) {
                                    push_capped(&mut state.files_touched, path.clone(), MAX_FILES);
                                }
                                if let Some(op) = file_op {
                                    items.push(TranscriptDeltaItem::FileOperation(TouchedFile {
                                        path: path.clone(),
                                        op,
                                        at_unix_ms: now,
                                    }));
                                    push_recent_file(&mut state.recent_files, path, op, now);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            let joined = text_parts.concat();
            if !joined.is_empty() {
                state.last_message = Some(joined);
            }
            items.push(TranscriptDeltaItem::AssistantCompleted);
        }

        if count > 0 {
            state.message_count += count as u64;
            state.updated_at_unix_ms = now;
        }
        TranscriptDelta { items }
    }
}

fn first_native_record(path: &Path, record_type: &str) -> Option<Value> {
    let reader = BufReader::new(File::open(path).ok()?);
    reader
        .lines()
        .take(64)
        .filter_map(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .find(|value| value.get("type").and_then(Value::as_str) == Some(record_type))
}

fn codex_candidate(path: PathBuf) -> Option<NativeTranscriptCandidate> {
    let meta = first_native_record(&path, "session_meta")?;
    Some(NativeTranscriptCandidate {
        session_id: meta.pointer("/payload/id")?.as_str()?.to_string(),
        cwd: meta.pointer("/payload/cwd")?.as_str()?.to_string(),
        started_at_unix_ms: meta
            .pointer("/payload/timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_unix_ms)
            .or_else(|| file_created_unix_ms(&path)),
        path,
    })
}

fn pi_candidate(path: PathBuf) -> Option<NativeTranscriptCandidate> {
    let session = first_native_record(&path, "session")?;
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            path.file_stem()?
                .to_str()?
                .rsplit_once('_')
                .map(|(_, id)| id.to_string())
        })?;
    Some(NativeTranscriptCandidate {
        session_id,
        cwd: session.get("cwd")?.as_str()?.to_string(),
        started_at_unix_ms: session
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_unix_ms)
            .or_else(|| file_created_unix_ms(&path)),
        path,
    })
}

fn kimi_candidate(path: PathBuf) -> Option<NativeTranscriptCandidate> {
    if path.file_name().and_then(|name| name.to_str()) != Some("wire.jsonl") {
        return None;
    }
    let session_dir = path.parent()?.parent()?.parent()?;
    let state: Value =
        serde_json::from_str(&fs::read_to_string(session_dir.join("state.json")).ok()?).ok()?;
    let session_id = session_dir.file_name()?.to_str()?.to_string();
    Some(NativeTranscriptCandidate {
        session_id,
        cwd: state.get("workDir")?.as_str()?.to_string(),
        started_at_unix_ms: state
            .get("createdAt")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_unix_ms)
            .or_else(|| first_native_timestamp(&path))
            .or_else(|| file_created_unix_ms(&path)),
        path,
    })
}

fn mangle_pi_cwd(cwd: &str) -> String {
    let encoded = cwd
        .trim_matches('/')
        .chars()
        .map(|ch| if ch == '/' { '-' } else { ch })
        .collect::<String>();
    format!("--{encoded}--")
}

// Inject environment paths so discovery can be regression-tested without
// changing the process-wide environment during parallel tests.
fn codex_sessions_root(codex_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    codex_home
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| home.map(|home| home.join(".codex")))
        .map(|home| home.join("sessions"))
}

/// Codex adapter over `$CODEX_HOME/sessions` (default `~/.codex/sessions`).
pub(crate) struct CodexTranscriptAdapter {
    sessions_root: Option<PathBuf>,
    candidates: NativeCandidateCache,
}

impl Default for CodexTranscriptAdapter {
    fn default() -> Self {
        Self {
            sessions_root: codex_sessions_root(
                std::env::var_os("CODEX_HOME").map(PathBuf::from),
                dirs::home_dir(),
            ),
            candidates: NativeCandidateCache::default(),
        }
    }
}

impl CodexTranscriptAdapter {
    #[cfg(test)]
    fn with_sessions_root(root: PathBuf) -> Self {
        Self {
            sessions_root: Some(root),
            candidates: NativeCandidateCache::default(),
        }
    }
}

impl TranscriptAdapter for CodexTranscriptAdapter {
    fn agent_kind(&self) -> &'static str {
        "codex"
    }

    fn resolve_path(
        &self,
        session_id: Option<&str>,
        cwd: Option<&str>,
        process_started_at_unix_ms: Option<u64>,
        provider_sessions_root: Option<&Path>,
    ) -> Option<ResolvedTranscript> {
        // The live Codex process is authoritative for its own CODEX_HOME. A
        // desktop-launched Taarof often has no CODEX_HOME even though a pane
        // launcher sets one, which previously left a real turn stuck at IDLE.
        let root = provider_sessions_root.or(self.sessions_root.as_deref())?;
        if !root.is_dir() {
            return None;
        }
        let candidates = self.candidates.get_or_scan(root, codex_candidate);
        select_native_candidate(candidates, session_id, cwd, process_started_at_unix_ms)
    }

    fn ingest_lines(&self, lines: &[String], state: &mut TranscriptState) -> TranscriptDelta {
        let now = now_unix_ms();
        let mut count = 0usize;
        let mut items = Vec::new();
        for line in lines {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let marker = codex_turn_marker(&value);
            fold_codex_child_agents(&value, state, now);
            apply_native_turn_id(state, &value, marker, "codex", now);
            apply_turn_marker(state, &value, marker, now);
            if value.get("type").and_then(Value::as_str) != Some("response_item") {
                continue;
            }
            match value.pointer("/payload/type").and_then(Value::as_str) {
                Some("message")
                    if value.pointer("/payload/role").and_then(Value::as_str)
                        == Some("assistant") =>
                {
                    count += 1;
                    let text = value
                        .pointer("/payload/content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|block| {
                            matches!(
                                block.get("type").and_then(Value::as_str),
                                Some("output_text") | Some("text")
                            )
                        })
                        .filter_map(|block| block.get("text").and_then(Value::as_str))
                        .collect::<String>();
                    if !text.is_empty() {
                        state.last_message = Some(text);
                    }
                    items.push(TranscriptDeltaItem::AssistantCompleted);
                }
                Some("function_call") => {
                    let tool = value
                        .pointer("/payload/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let arguments = value
                        .pointer("/payload/arguments")
                        .and_then(Value::as_str)
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
                    if let Some(event) = structured_file_event(tool, arguments.as_ref(), now) {
                        push_recent_file(
                            &mut state.recent_files,
                            event.path.clone(),
                            event.op,
                            event.at_unix_ms,
                        );
                        if !state.files_touched.contains(&event.path) {
                            push_capped(&mut state.files_touched, event.path.clone(), MAX_FILES);
                        }
                        items.push(TranscriptDeltaItem::FileOperation(event));
                    }
                }
                _ => {}
            }
        }
        if count > 0 {
            state.message_count += count as u64;
        }
        if !items.is_empty() {
            state.updated_at_unix_ms = now;
        }
        TranscriptDelta { items }
    }
}

/// Pi Agent adapter over `~/.pi/agent/sessions/**/*.jsonl`.
pub(crate) struct PiTranscriptAdapter {
    sessions_root: Option<PathBuf>,
    candidates: NativeCandidateCache,
}

impl Default for PiTranscriptAdapter {
    fn default() -> Self {
        Self {
            sessions_root: dirs::home_dir().map(|home| home.join(".pi/agent/sessions")),
            candidates: NativeCandidateCache::default(),
        }
    }
}

impl PiTranscriptAdapter {
    #[cfg(test)]
    fn with_sessions_root(root: PathBuf) -> Self {
        Self {
            sessions_root: Some(root),
            candidates: NativeCandidateCache::default(),
        }
    }
}

impl TranscriptAdapter for PiTranscriptAdapter {
    fn agent_kind(&self) -> &'static str {
        "pi"
    }

    fn resolve_path(
        &self,
        session_id: Option<&str>,
        cwd: Option<&str>,
        process_started_at_unix_ms: Option<u64>,
        _provider_sessions_root: Option<&Path>,
    ) -> Option<ResolvedTranscript> {
        let root = self.sessions_root.as_deref()?;
        if !root.is_dir() {
            return None;
        }
        let scoped_root = cwd
            .map(|cwd| root.join(mangle_pi_cwd(cwd)))
            .filter(|path| path.is_dir())
            .unwrap_or_else(|| root.to_path_buf());
        let candidates = self.candidates.get_or_scan(&scoped_root, pi_candidate);
        select_native_candidate(candidates, session_id, cwd, process_started_at_unix_ms)
    }

    fn ingest_lines(&self, lines: &[String], state: &mut TranscriptState) -> TranscriptDelta {
        let now = now_unix_ms();
        let mut count = 0usize;
        let mut items = Vec::new();
        for line in lines {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            apply_turn_marker(state, &value, pi_turn_marker(&value), now);
            if value.get("type").and_then(Value::as_str) != Some("message")
                || value.pointer("/message/role").and_then(Value::as_str) != Some("assistant")
            {
                continue;
            }
            count += 1;
            let blocks = value.pointer("/message/content").and_then(Value::as_array);
            let text = blocks
                .into_iter()
                .flatten()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<String>();
            if !text.is_empty() {
                state.last_message = Some(text);
            }
            for block in blocks.into_iter().flatten() {
                if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                    continue;
                }
                let tool = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(event) = structured_file_event(tool, block.get("arguments"), now) {
                    push_recent_file(
                        &mut state.recent_files,
                        event.path.clone(),
                        event.op,
                        event.at_unix_ms,
                    );
                    if !state.files_touched.contains(&event.path) {
                        push_capped(&mut state.files_touched, event.path.clone(), MAX_FILES);
                    }
                    items.push(TranscriptDeltaItem::FileOperation(event));
                }
            }
            items.push(TranscriptDeltaItem::AssistantCompleted);
        }
        if count > 0 {
            state.message_count += count as u64;
            state.updated_at_unix_ms = now;
        }
        TranscriptDelta { items }
    }
}

/// Kimi Code adapter over `~/.kimi-code/sessions/*/session_*/agents/main/wire.jsonl`.
pub(crate) struct KimiTranscriptAdapter {
    sessions_root: Option<PathBuf>,
    candidates: NativeCandidateCache,
}

impl Default for KimiTranscriptAdapter {
    fn default() -> Self {
        Self {
            sessions_root: dirs::home_dir().map(|home| home.join(".kimi-code/sessions")),
            candidates: NativeCandidateCache::default(),
        }
    }
}

impl KimiTranscriptAdapter {
    #[cfg(test)]
    fn with_sessions_root(root: PathBuf) -> Self {
        Self {
            sessions_root: Some(root),
            candidates: NativeCandidateCache::default(),
        }
    }
}

impl TranscriptAdapter for KimiTranscriptAdapter {
    fn agent_kind(&self) -> &'static str {
        "kimi"
    }

    fn resolve_path(
        &self,
        session_id: Option<&str>,
        cwd: Option<&str>,
        process_started_at_unix_ms: Option<u64>,
        _provider_sessions_root: Option<&Path>,
    ) -> Option<ResolvedTranscript> {
        let root = self.sessions_root.as_deref()?;
        if !root.is_dir() {
            return None;
        }
        let candidates = self.candidates.get_or_scan(root, kimi_candidate);
        select_native_candidate(candidates, session_id, cwd, process_started_at_unix_ms)
    }

    fn ingest_lines(&self, lines: &[String], state: &mut TranscriptState) -> TranscriptDelta {
        let now = now_unix_ms();
        let mut completed = 0usize;
        for line in lines {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let marker = kimi_turn_marker(&value);
            apply_turn_marker(state, &value, marker, now);
            if marker == Some(TurnMarker::Completed)
                && value.get("type").and_then(Value::as_str) == Some("context.append_loop_event")
            {
                completed += 1;
            }
            if value.pointer("/event/type").and_then(Value::as_str) == Some("content.part")
                && value.pointer("/event/part/type").and_then(Value::as_str) == Some("text")
            {
                if let Some(text) = value.pointer("/event/part/text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        state.last_message = Some(text.to_string());
                    }
                }
            }
        }
        if completed > 0 {
            state.message_count = state.message_count.saturating_add(completed as u64);
            state.updated_at_unix_ms = now;
        }
        TranscriptDelta::default()
    }
}

/// One tracked pane: its adapter kind, the session it is bound to, the owning
/// tailer (so byte offsets survive across ticks), and the folded state.
struct PaneTail {
    agent: String,
    session_id: String,
    process_started_at_unix_ms: Option<u64>,
    source_path: PathBuf,
    tailer: TranscriptTailer,
    state: TranscriptState,
    message_event_ordinal: u64,
    file_event_ordinal: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TranscriptWorkEvent {
    AssistantCompleted {
        ordinal: u64,
    },
    FileOperation {
        path: String,
        op: FileOp,
        at_unix_ms: u64,
        ordinal: u64,
    },
}

/// A per-pane transcript change produced by one poll.
pub(crate) struct TranscriptTick {
    pub key: PaneTranscriptKey,
    pub state: TranscriptState,
    pub new_messages: usize,
    pub work_events: Vec<TranscriptWorkEvent>,
    /// True only when a native transcript was newly resolved. AppState must
    /// cache the folded state but project no work from this tick.
    pub baseline: bool,
}

/// The result of one [`TranscriptTracker::sync_and_poll`]: changed panes plus
/// panes that are no longer bound (or whose session changed) and were dropped.
pub(crate) struct TranscriptPollResult {
    pub ticks: Vec<TranscriptTick>,
    pub removed: Vec<PaneTranscriptKey>,
}

impl TranscriptPollResult {
    /// Results collected before a tab close must not repopulate its mirror or
    /// emit activity for that deleted tab when the worker reaches GTK again.
    pub(crate) fn retain_tabs(&mut self, live: &HashSet<u32>) {
        self.ticks.retain(|tick| live.contains(&tick.key.0));
    }
}

/// Owns per-pane tailers and the set of adapters. All disk IO happens in
/// [`sync_and_poll`], which is meant to run inside a blocking worker.
///
/// [`sync_and_poll`]: TranscriptTracker::sync_and_poll
pub(crate) struct TranscriptTracker {
    adapters: Vec<Box<dyn TranscriptAdapter>>,
    inner: Mutex<HashMap<PaneTranscriptKey, PaneTail>>,
}

impl TranscriptTracker {
    /// Never wait on worker IO from GTK. A busy tracker requires another poll;
    /// an empty mirror alone does not prove the worker has released its tails.
    pub(crate) fn has_tracked_panes(&self) -> bool {
        self.inner
            .try_lock()
            .map(|inner| !inner.is_empty())
            .unwrap_or(true)
    }

    pub(crate) fn with_default_adapters() -> Self {
        Self::with_adapters(vec![
            Box::new(ClaudeTranscriptAdapter::default()),
            Box::new(CodexTranscriptAdapter::default()),
            Box::new(PiTranscriptAdapter::default()),
            Box::new(KimiTranscriptAdapter::default()),
        ])
    }

    fn with_adapters(adapters: Vec<Box<dyn TranscriptAdapter>>) -> Self {
        Self {
            adapters,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Reconcile tracked panes against `bindings` and read any appended
    /// transcript bytes. Runs entirely off the GTK main thread. Change-gated:
    /// a pane only appears in `ticks` when its folded state actually moved, so
    /// idle ticks are cheap.
    pub(crate) fn sync_and_poll(&self, bindings: &[PaneTranscriptBinding]) -> TranscriptPollResult {
        let binding_process_starts: HashMap<PaneTranscriptKey, Option<u64>> = bindings
            .iter()
            .map(|binding| (binding.key, binding_process_start(binding)))
            .collect();
        let binding_cwds: HashMap<PaneTranscriptKey, Option<String>> = bindings
            .iter()
            .map(|binding| (binding.key, binding_agent_cwd(binding)))
            .collect();
        let binding_provider_roots: HashMap<PaneTranscriptKey, Option<PathBuf>> = bindings
            .iter()
            .map(|binding| (binding.key, binding_codex_sessions_root(binding)))
            .collect();
        let mut inner = self.inner.lock().expect("transcript tracker lock poisoned");
        let mut ticks = Vec::new();
        let mut removed = Vec::new();

        // Drop panes that are no longer bound, or whose bound session changed.
        let binding_by_key: HashMap<PaneTranscriptKey, &PaneTranscriptBinding> = bindings
            .iter()
            .map(|binding| (binding.key, binding))
            .collect();
        let existing_keys: Vec<PaneTranscriptKey> = inner.keys().copied().collect();
        for key in existing_keys {
            let keep = binding_by_key.get(&key).is_some_and(|binding| {
                inner.get(&key).is_some_and(|tail| {
                    tail.agent == binding.agent
                        && binding
                            .session_id
                            .as_ref()
                            .is_none_or(|session_id| tail.session_id == *session_id)
                        && (binding.session_id.is_some()
                            || tail.process_started_at_unix_ms
                                == binding_process_starts.get(&key).copied().flatten())
                })
            });
            if !keep {
                inner.remove(&key);
                removed.push(key);
            }
        }

        let mut used_paths: HashSet<PathBuf> = inner
            .values()
            .map(|tail| tail.source_path.clone())
            .collect();

        for binding in bindings {
            // Resolve the adapter first (borrows `self.adapters`, disjoint from
            // the `inner` map guard) to keep the borrows clean.
            let Some(adapter) = self
                .adapters
                .iter()
                .find(|adapter| adapter.agent_kind() == binding.agent)
            else {
                continue;
            };

            let mut baseline = false;
            let tail = match inner.entry(binding.key) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let Some(resolved) = adapter.resolve_path(
                        binding.session_id.as_deref(),
                        binding_cwds
                            .get(&binding.key)
                            .and_then(|cwd| cwd.as_deref()),
                        binding_process_starts.get(&binding.key).copied().flatten(),
                        binding_provider_roots
                            .get(&binding.key)
                            .and_then(|root| root.as_deref()),
                    ) else {
                        // Unresolvable session: insert nothing, degrade silently.
                        continue;
                    };
                    if used_paths.contains(&resolved.path) {
                        // Never bind one native transcript to two live panes.
                        // Ambiguity must remain unavailable and use the visible
                        // recent-output fallback instead of cross-session data.
                        continue;
                    }
                    used_paths.insert(resolved.path.clone());
                    baseline = true;
                    entry.insert(PaneTail {
                        agent: binding.agent.clone(),
                        session_id: resolved.session_id.clone(),
                        process_started_at_unix_ms: binding_process_starts
                            .get(&binding.key)
                            .copied()
                            .flatten(),
                        source_path: resolved.path.clone(),
                        tailer: TranscriptTailer::new(resolved.path),
                        state: TranscriptState {
                            agent: binding.agent.clone(),
                            session_id: resolved.session_id,
                            ..Default::default()
                        },
                        message_event_ordinal: 0,
                        file_event_ordinal: 0,
                    })
                }
            };
            let lines = match tail.tailer.poll_new_lines() {
                Ok(lines) => lines,
                Err(_) => continue,
            };
            if tail.tailer.take_reset_during_poll() {
                // A truncate/rotation may contain replayed history. Consume it
                // as the new byte baseline but do not fold or emit it.
                continue;
            }
            if lines.is_empty() && !baseline {
                continue;
            }
            let snapshot = tail.state.clone();
            let delta = adapter.ingest_lines(&lines, &mut tail.state);
            let mut work_events = Vec::with_capacity(delta.items.len());
            for item in delta.items {
                match item {
                    TranscriptDeltaItem::AssistantCompleted => {
                        tail.message_event_ordinal = tail.message_event_ordinal.saturating_add(1);
                        if !baseline {
                            work_events.push(TranscriptWorkEvent::AssistantCompleted {
                                ordinal: tail.message_event_ordinal,
                            });
                        }
                    }
                    TranscriptDeltaItem::FileOperation(event) => {
                        tail.file_event_ordinal = tail.file_event_ordinal.saturating_add(1);
                        if !baseline {
                            work_events.push(TranscriptWorkEvent::FileOperation {
                                path: event.path,
                                op: event.op,
                                at_unix_ms: event.at_unix_ms,
                                ordinal: tail.file_event_ordinal,
                            });
                        }
                    }
                }
            }
            let new_messages = work_events
                .iter()
                .filter(|event| matches!(event, TranscriptWorkEvent::AssistantCompleted { .. }))
                .count();
            if baseline || tail.state != snapshot || new_messages > 0 || !work_events.is_empty() {
                ticks.push(TranscriptTick {
                    key: binding.key,
                    state: tail.state.clone(),
                    new_messages,
                    work_events,
                    baseline,
                });
            }
        }

        TranscriptPollResult { ticks, removed }
    }
}

/// Build transcript bindings from the current runtime probe: every local pane
/// with a running agent name. A runtime session id improves lookup but is not
/// required; adapters can resolve the agent-native store from the pane cwd.
/// Mirrors the per-pane iteration in `agent_sessions::build_live_agent_bindings`.
pub(crate) fn collect_transcript_bindings(state: &AppState) -> Vec<PaneTranscriptBinding> {
    let Some(snapshot) = state.runtime_probe.as_ref() else {
        return Vec::new();
    };

    let mut bindings = Vec::new();
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            for leaf in tab.panes.leaves() {
                if leaf.shell_pid.is_none() {
                    continue;
                }
                let Some(status) = snapshot
                    .pane_agents
                    .get(&(tab.id, leaf.pane_id))
                    .filter(|status| status.running)
                else {
                    continue;
                };
                let Some(agent) = status.agent_name.clone() else {
                    continue;
                };
                bindings.push(PaneTranscriptBinding {
                    key: (tab.id, leaf.pane_id),
                    agent,
                    session_id: status.session_id.clone(),
                    cwd: leaf.local_cwd(),
                    shell_pid: leaf.shell_pid,
                    process_started_at_unix_ms: None,
                });
            }
        }
    }
    bindings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::TurnPhase;
    use std::fs;
    use std::io::Write;

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "taarof-transcript-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    fn assistant_line(text: &str, tool: &str, file_path: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "sessionId": "sess-1",
            "cwd": "/a/b",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": text },
                    { "type": "tool_use", "name": tool, "input": { "file_path": file_path } }
                ]
            }
        })
        .to_string()
    }

    #[test]
    fn pi_cwd_mangling_preserves_dots() {
        assert_eq!(
            mangle_pi_cwd("/tmp/user/.config/tool/skills/example"),
            "--tmp-user-.config-tool-skills-example--"
        );
    }

    #[test]
    fn test_transcript_tail_reads_only_appended_lines() {
        let dir = unique_temp_dir("tail");
        let path = dir.join("session.jsonl");
        let first = assistant_line("First", "Read", "/a/b/one.rs");
        let second = assistant_line("Second", "Read", "/a/b/two.rs");
        fs::write(&path, format!("{first}\n{second}\n")).expect("fixture should write");

        let mut tailer = TranscriptTailer::new(path.clone());

        // First poll returns both pre-existing lines.
        let lines = tailer.poll_new_lines().expect("first poll should succeed");
        assert_eq!(lines.len(), 2, "first poll returns the two existing lines");

        // Append one more line; the next poll must return ONLY the appended line.
        let third = assistant_line("Third", "Edit", "/a/b/three.rs");
        {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("append handle should open");
            writeln!(file, "{third}").expect("append should write");
        }
        let lines = tailer.poll_new_lines().expect("second poll should succeed");
        assert_eq!(lines, vec![third.clone()], "only the appended line is read");

        // No change: a further poll returns nothing (offset unchanged).
        let lines = tailer.poll_new_lines().expect("third poll should succeed");
        assert!(lines.is_empty(), "no new bytes yields no lines");

        // Partial line: write bytes without a terminating newline, then the
        // remainder. Only the completed line surfaces, and only once.
        let fourth = assistant_line("Fourth", "Read", "/a/b/four.rs");
        let (head, tail) = fourth.split_at(fourth.len() / 2);
        {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("append handle should open");
            write!(file, "{head}").expect("partial write should succeed");
        }
        let lines = tailer
            .poll_new_lines()
            .expect("partial poll should succeed");
        assert!(
            lines.is_empty(),
            "an unterminated line is buffered, not emitted"
        );
        {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("append handle should open");
            writeln!(file, "{tail}").expect("remainder write should succeed");
        }
        let lines = tailer
            .poll_new_lines()
            .expect("completion poll should succeed");
        assert_eq!(
            lines,
            vec![fourth],
            "buffered partial completes into one line"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn test_claude_adapter_extracts_last_assistant_message_and_tool_paths() {
        let adapter = ClaudeTranscriptAdapter::default();
        let lines = vec![
            assistant_line("Working", "Read", "/a/b/main.rs"),
            assistant_line("Done.", "Edit", "/a/b/sidebar.rs"),
        ];

        let mut state = TranscriptState::default();
        let new_messages = adapter.ingest_lines(&lines, &mut state);

        assert_eq!(
            new_messages.assistant_messages(),
            2,
            "both assistant messages counted"
        );
        assert_eq!(state.last_message.as_deref(), Some("Done."));
        assert_eq!(state.message_count, 2);
        assert!(
            state.files_touched.contains(&"/a/b/main.rs".to_string()),
            "first tool path recorded"
        );
        assert!(
            state.files_touched.contains(&"/a/b/sidebar.rs".to_string()),
            "second tool path recorded"
        );
        assert_eq!(state.recent_tool_calls.len(), 2);
        assert_eq!(state.recent_tool_calls[0].tool, "Read");
        assert_eq!(
            state.recent_tool_calls[1].target.as_deref(),
            Some("/a/b/sidebar.rs")
        );
    }

    #[test]
    fn test_claude_adapter_ignores_non_assistant_lines() {
        let adapter = ClaudeTranscriptAdapter::default();
        let user_line = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": "hello" }
        })
        .to_string();
        let junk_line = "not json at all".to_string();

        let mut state = TranscriptState::default();
        let new_messages = adapter.ingest_lines(&[user_line, junk_line], &mut state);

        assert_eq!(new_messages.assistant_messages(), 0);
        assert!(state.last_message.is_none());
        assert_eq!(state.message_count, 0);
    }

    #[test]
    fn claude_preserves_source_text_blocks_without_trimming() {
        let adapter = ClaudeTranscriptAdapter::default();
        let line = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "  leading"},
                    {"type": "text", "text": "\ntrailing  "}
                ]
            }
        })
        .to_string();
        let mut state = TranscriptState::default();

        adapter.ingest_lines(&[line], &mut state);

        assert_eq!(state.last_message.as_deref(), Some("  leading\ntrailing  "));
    }

    #[test]
    fn native_structured_file_operations_are_typed_for_all_supported_adapters() {
        let claude = ClaudeTranscriptAdapter::default();
        let mut claude_state = TranscriptState::default();
        let claude_delta = claude.ingest_lines(
            &[assistant_line("done", "Edit", "/repo/src/claude.rs")],
            &mut claude_state,
        );
        assert_eq!(claude_delta.file_events().len(), 1);
        assert_eq!(claude_delta.file_events()[0].op, FileOp::Edited);

        let codex = CodexTranscriptAdapter::default();
        let mut codex_state = TranscriptState::default();
        let codex_delta = codex.ingest_lines(
            &[serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "write_file",
                    "arguments": serde_json::json!({
                        "path": "/repo/src/codex.rs",
                        "content": "HOSTILE_SECRET_SENTINEL"
                    }).to_string()
                }
            })
            .to_string()],
            &mut codex_state,
        );
        assert_eq!(codex_delta.file_events().len(), 1);
        assert_eq!(codex_delta.file_events()[0].op, FileOp::Created);
        assert_eq!(codex_delta.file_events()[0].path, "/repo/src/codex.rs");

        let pi = PiTranscriptAdapter::default();
        let mut pi_state = TranscriptState::default();
        let pi_delta = pi.ingest_lines(
            &[serde_json::json!({
                "type": "message",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "toolCall",
                        "name": "edit",
                        "arguments": {"path": "/repo/src/pi.rs", "oldText": "x", "newText": "y"}
                    }]
                }
            })
            .to_string()],
            &mut pi_state,
        );
        assert_eq!(pi_delta.file_events().len(), 1);
        assert_eq!(pi_delta.file_events()[0].op, FileOp::Edited);
        assert_eq!(pi_delta.file_events()[0].path, "/repo/src/pi.rs");
    }

    #[test]
    fn codex_file_only_function_call_refreshes_transcript_timestamp() {
        let adapter = CodexTranscriptAdapter::default();
        let mut state = TranscriptState {
            updated_at_unix_ms: 1,
            ..TranscriptState::default()
        };
        let delta = adapter.ingest_lines(
            &[serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "edit_file",
                    "arguments": serde_json::json!({"path": "/repo/src/file.rs"}).to_string()
                }
            })
            .to_string()],
            &mut state,
        );

        assert_eq!(delta.file_events().len(), 1);
        assert_eq!(state.message_count, 0);
        assert!(state.updated_at_unix_ms > 1);
    }

    #[test]
    fn native_delta_stream_preserves_file_and_message_interleaving() {
        fn codex_file(path: &str) -> String {
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "edit_file",
                    "arguments": serde_json::json!({"path": path}).to_string()
                }
            })
            .to_string()
        }
        fn codex_message(text: &str) -> String {
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text}]
                }
            })
            .to_string()
        }
        fn kinds(delta: &TranscriptDelta) -> Vec<&'static str> {
            delta
                .items
                .iter()
                .map(|item| match item {
                    TranscriptDeltaItem::AssistantCompleted => "message",
                    TranscriptDeltaItem::FileOperation(_) => "file",
                })
                .collect()
        }

        let adapter = CodexTranscriptAdapter::default();
        let mut state = TranscriptState::default();
        let final_message = adapter.ingest_lines(
            &[codex_file("/repo/a.rs"), codex_message("done")],
            &mut state,
        );
        assert_eq!(kinds(&final_message), vec!["file", "message"]);

        let interleaved = adapter.ingest_lines(
            &[
                codex_message("one"),
                codex_file("/repo/b.rs"),
                codex_message("two"),
                codex_file("/repo/c.rs"),
            ],
            &mut state,
        );
        assert_eq!(
            kinds(&interleaved),
            vec!["message", "file", "message", "file"]
        );
    }

    #[test]
    fn prose_shell_and_unstructured_patch_payloads_never_become_file_evidence() {
        let codex = CodexTranscriptAdapter::default();
        let mut state = TranscriptState::default();
        let lines = [
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "edited /repo/prose-secret.rs"}]
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"sed -i x /repo/shell-secret.rs\"}"
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "apply_patch",
                    "arguments": "*** Update File: /repo/patch-secret.rs"
                }
            })
            .to_string(),
        ];
        let delta = codex.ingest_lines(&lines, &mut state);
        assert!(delta.file_events().is_empty());
        assert!(state.recent_files.is_empty());
    }

    #[test]
    fn test_unresolvable_session_degrades_without_error() {
        // An empty projects root cannot resolve any session.
        let root = unique_temp_dir("unresolvable");
        let adapter = ClaudeTranscriptAdapter::with_projects_root(root.clone());
        assert!(
            adapter
                .resolve_path(Some("bogus-session"), Some("/tmp/x"), None, None)
                .is_none(),
            "no file exists for a bogus session"
        );

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(adapter)]);
        let result = tracker.sync_and_poll(&[PaneTranscriptBinding {
            key: (1, 2),
            agent: "claude".to_string(),
            session_id: Some("bogus-session".to_string()),
            cwd: Some("/tmp/x".to_string()),
            shell_pid: None,
            process_started_at_unix_ms: None,
        }]);

        assert!(
            result.ticks.is_empty(),
            "an unresolvable pane produces no ticks and does not panic"
        );
        assert!(result.removed.is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn claude_copy_resolves_without_runtime_session_id() {
        let root = unique_temp_dir("claude-no-runtime-session");
        let cwd = "/work/no-session";
        let project_dir = root.join(mangle_claude_cwd(cwd));
        fs::create_dir_all(&project_dir).expect("project dir should be created");
        let path = project_dir.join("native-session.jsonl");
        fs::write(
            &path,
            format!(
                "{}\n",
                assistant_line(
                    "Pristine **Claude** markdown",
                    "Read",
                    "/work/no-session/lib.rs"
                )
            ),
        )
        .expect("fixture should write");

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            ClaudeTranscriptAdapter::with_projects_root(root.clone()),
        )]);
        let result = tracker.sync_and_poll(&[PaneTranscriptBinding {
            key: (7, 8),
            agent: "claude".to_string(),
            session_id: None,
            cwd: Some(cwd.to_string()),
            shell_pid: None,
            process_started_at_unix_ms: file_created_unix_ms(&path),
        }]);

        assert_eq!(result.ticks.len(), 1);
        assert_eq!(
            result.ticks[0].state.last_message.as_deref(),
            Some("Pristine **Claude** markdown")
        );
        assert_eq!(result.ticks[0].state.session_id, "native-session");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn codex_copy_extracts_latest_assistant_markdown() {
        let root = unique_temp_dir("codex-no-runtime-session");
        let session_dir = root.join("2026/07/14");
        fs::create_dir_all(&session_dir).expect("session dir should be created");
        let path = session_dir.join("rollout-2026-07-14T12-00-00-native-codex.jsonl");
        let pristine = "  ## Codex result\n\n- exact markdown\n  ";
        let meta = serde_json::json!({
            "type": "session_meta",
            "payload": {"id": "native-codex", "cwd": "/work/codex"}
        });
        let reply = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": pristine}]
            }
        });
        fs::write(&path, format!("{meta}\n{reply}\n")).expect("fixture should write");

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            CodexTranscriptAdapter::with_sessions_root(root.clone()),
        )]);
        let result = tracker.sync_and_poll(&[PaneTranscriptBinding {
            key: (9, 1),
            agent: "codex".to_string(),
            session_id: None,
            cwd: Some("/work/codex".to_string()),
            shell_pid: None,
            process_started_at_unix_ms: file_created_unix_ms(&path),
        }]);

        assert_eq!(result.ticks.len(), 1);
        assert_eq!(result.ticks[0].state.session_id, "native-codex");
        assert_eq!(
            result.ticks[0].state.last_message.as_deref(),
            Some(pristine)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pi_copy_extracts_appended_assistant_markdown() {
        let root = unique_temp_dir("pi-no-runtime-session");
        let session_dir = root.join("--work-pi--");
        fs::create_dir_all(&session_dir).expect("session dir should be created");
        let path = session_dir.join("2026-07-14T12-00-00-000Z_native-pi.jsonl");
        let session = serde_json::json!({
            "type": "session",
            "id": "native-pi",
            "timestamp": "2026-07-14T12:00:00.000Z",
            "cwd": "/work/pi"
        });
        let first = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "First Pi reply"}]
            }
        });
        fs::write(&path, format!("{session}\n{first}\n")).expect("fixture should write");

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            PiTranscriptAdapter::with_sessions_root(root.clone()),
        )]);
        let binding = PaneTranscriptBinding {
            key: (10, 2),
            agent: "pi".to_string(),
            session_id: None,
            cwd: Some("/work/pi".to_string()),
            shell_pid: None,
            process_started_at_unix_ms: parse_rfc3339_unix_ms("2026-07-14T12:00:00.000Z"),
        };
        let first_poll = tracker.sync_and_poll(std::slice::from_ref(&binding));
        assert_eq!(
            first_poll.ticks[0].state.last_message.as_deref(),
            Some("First Pi reply")
        );

        let pristine = "## Final Pi reply\n\n`exact source`\n";
        let appended = serde_json::json!({
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{"type": "thinking", "thinking": "hidden"}, {"type": "text", "text": pristine}]
            }
        });
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append handle should open");
        writeln!(file, "{appended}").expect("append should write");

        let second_poll = tracker.sync_and_poll(std::slice::from_ref(&binding));
        assert_eq!(second_poll.ticks.len(), 1);
        assert_eq!(second_poll.ticks[0].new_messages, 1);
        assert_eq!(
            second_poll.ticks[0].state.last_message.as_deref(),
            Some(pristine)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn two_pi_panes_sharing_a_cwd_bind_to_their_own_transcripts() {
        let root = unique_temp_dir("pi-shared-cwd");
        let cwd = "/work/shared";
        let session_dir = root.join(mangle_pi_cwd(cwd));
        fs::create_dir_all(&session_dir).expect("session dir should be created");

        for (id, timestamp, text) in [
            ("pi-left", "2026-07-14T12:00:00.000Z", "left pane reply"),
            ("pi-right", "2026-07-14T12:30:00.000Z", "right pane reply"),
        ] {
            let session = serde_json::json!({
                "type": "session",
                "id": id,
                "timestamp": timestamp,
                "cwd": cwd
            });
            let reply = serde_json::json!({
                "type": "message",
                "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
            });
            fs::write(
                session_dir.join(format!("{timestamp}_{id}.jsonl").replace(':', "-")),
                format!("{session}\n{reply}\n"),
            )
            .expect("fixture should write");
        }

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            PiTranscriptAdapter::with_sessions_root(root.clone()),
        )]);
        // Both panes report the same cwd; only the Pi process start times differ.
        let result = tracker.sync_and_poll(&[
            PaneTranscriptBinding {
                key: (13, 1),
                agent: "pi".to_string(),
                session_id: None,
                cwd: Some(cwd.to_string()),
                shell_pid: None,
                process_started_at_unix_ms: parse_rfc3339_unix_ms("2026-07-14T12:00:00.000Z"),
            },
            PaneTranscriptBinding {
                key: (13, 2),
                agent: "pi".to_string(),
                session_id: None,
                cwd: Some(cwd.to_string()),
                shell_pid: None,
                process_started_at_unix_ms: parse_rfc3339_unix_ms("2026-07-14T12:30:00.000Z"),
            },
        ]);

        let messages: HashMap<PaneTranscriptKey, &str> = result
            .ticks
            .iter()
            .filter_map(|tick| Some((tick.key, tick.state.last_message.as_deref()?)))
            .collect();
        assert_eq!(messages[&(13, 1)], "left pane reply");
        assert_eq!(messages[&(13, 2)], "right pane reply");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn transcript_resolution_stays_scoped_to_the_focused_pane() {
        let root = unique_temp_dir("codex-pane-isolation");
        let session_dir = root.join("2026/07/14");
        fs::create_dir_all(&session_dir).expect("session dir should be created");

        for (id, timestamp, text) in [
            ("codex-early", "2026-07-14T12:00:00.000Z", "early pane"),
            ("codex-late", "2026-07-14T12:30:00.000Z", "late pane"),
        ] {
            let meta = serde_json::json!({
                "type": "session_meta",
                "payload": {"id": id, "cwd": "/work/shared", "timestamp": timestamp}
            });
            let reply = serde_json::json!({
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}
            });
            fs::write(
                session_dir.join(format!("{id}.jsonl")),
                format!("{meta}\n{reply}\n"),
            )
            .expect("fixture should write");
        }

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            CodexTranscriptAdapter::with_sessions_root(root.clone()),
        )]);
        let noon = parse_rfc3339_unix_ms("2026-07-14T12:00:00.000Z").unwrap();
        let half_past = parse_rfc3339_unix_ms("2026-07-14T12:30:00.000Z").unwrap();
        let result = tracker.sync_and_poll(&[
            PaneTranscriptBinding {
                key: (11, 1),
                agent: "codex".to_string(),
                session_id: None,
                cwd: Some("/work/shared".to_string()),
                shell_pid: None,
                process_started_at_unix_ms: Some(noon),
            },
            PaneTranscriptBinding {
                key: (11, 2),
                agent: "codex".to_string(),
                session_id: None,
                cwd: Some("/work/shared".to_string()),
                shell_pid: None,
                process_started_at_unix_ms: Some(half_past),
            },
        ]);

        let messages: HashMap<PaneTranscriptKey, &str> = result
            .ticks
            .iter()
            .filter_map(|tick| Some((tick.key, tick.state.last_message.as_deref()?)))
            .collect();
        assert_eq!(messages[&(11, 1)], "early pane");
        assert_eq!(messages[&(11, 2)], "late pane");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stale_single_transcript_does_not_bind_to_a_fresh_process() {
        let root = unique_temp_dir("stale-single");
        let cwd = "/work/stale";
        let project_dir = root.join(mangle_claude_cwd(cwd));
        fs::create_dir_all(&project_dir).expect("project dir should be created");
        let path = project_dir.join("old-session.jsonl");
        fs::write(
            &path,
            format!("{}\n", assistant_line("old reply", "Read", "/work/stale/a")),
        )
        .expect("fixture should write");
        let old_start = file_created_unix_ms(&path).expect("fixture should have creation time");
        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            ClaudeTranscriptAdapter::with_projects_root(root.clone()),
        )]);

        let result = tracker.sync_and_poll(&[PaneTranscriptBinding {
            key: (12, 1),
            agent: "claude".to_string(),
            session_id: None,
            cwd: Some(cwd.to_string()),
            shell_pid: None,
            process_started_at_unix_ms: Some(old_start + MAX_SESSION_START_SKEW_MS + 1),
        }]);

        assert!(result.ticks.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn same_pane_process_restart_rebinds_to_the_new_session() {
        let root = unique_temp_dir("same-pane-restart");
        let session_dir = root.join("2026/07/14");
        fs::create_dir_all(&session_dir).expect("session dir should be created");
        for (id, timestamp, text) in [
            ("restart-old", "2026-07-14T12:00:00.000Z", "old process"),
            ("restart-new", "2026-07-14T12:30:00.000Z", "new process"),
        ] {
            let meta = serde_json::json!({
                "type": "session_meta",
                "payload": {"id": id, "cwd": "/work/restart", "timestamp": timestamp}
            });
            let reply = serde_json::json!({
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}
            });
            fs::write(
                session_dir.join(format!("{id}.jsonl")),
                format!("{meta}\n{reply}\n"),
            )
            .expect("fixture should write");
        }
        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            CodexTranscriptAdapter::with_sessions_root(root.clone()),
        )]);
        let mut binding = PaneTranscriptBinding {
            key: (13, 1),
            agent: "codex".to_string(),
            session_id: None,
            cwd: Some("/work/restart".to_string()),
            shell_pid: None,
            process_started_at_unix_ms: parse_rfc3339_unix_ms("2026-07-14T12:00:00.000Z"),
        };
        let first = tracker.sync_and_poll(std::slice::from_ref(&binding));
        assert_eq!(
            first.ticks[0].state.last_message.as_deref(),
            Some("old process")
        );

        binding.process_started_at_unix_ms = parse_rfc3339_unix_ms("2026-07-14T12:30:00.000Z");
        let restarted = tracker.sync_and_poll(std::slice::from_ref(&binding));

        assert_eq!(restarted.removed, vec![(13, 1)]);
        assert_eq!(restarted.ticks[0].state.session_id, "restart-new");
        assert_eq!(
            restarted.ticks[0].state.last_message.as_deref(),
            Some("new process")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_codex_session_resolves_beyond_old_global_cap() {
        let root = unique_temp_dir("codex-uncapped");
        let session_dir = root.join("2026/07/14");
        fs::create_dir_all(&session_dir).expect("session dir should be created");
        for index in 0..300 {
            let id = format!("session-{index:03}");
            let meta = serde_json::json!({
                "type": "session_meta",
                "payload": {"id": id, "cwd": "/work/many"}
            });
            let reply = serde_json::json!({
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": id}]}
            });
            fs::write(
                session_dir.join(format!("{id}.jsonl")),
                format!("{meta}\n{reply}\n"),
            )
            .expect("fixture should write");
        }
        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            CodexTranscriptAdapter::with_sessions_root(root.clone()),
        )]);
        let result = tracker.sync_and_poll(&[PaneTranscriptBinding {
            key: (14, 1),
            agent: "codex".to_string(),
            session_id: Some("session-000".to_string()),
            cwd: Some("/work/many".to_string()),
            shell_pid: None,
            process_started_at_unix_ms: None,
        }]);

        assert_eq!(result.ticks[0].state.session_id, "session-000");
        assert_eq!(
            result.ticks[0].state.last_message.as_deref(),
            Some("session-000")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_tracker_reads_appended_messages_and_drops_unbound_panes() {
        let root = unique_temp_dir("tracker");
        // Lay out a real Claude project dir: <mangled cwd>/<sessionId>.jsonl.
        let cwd = "/work/repo";
        let project_dir = root.join(mangle_claude_cwd(cwd));
        fs::create_dir_all(&project_dir).expect("project dir should be created");
        let session_path = project_dir.join("sess-1.jsonl");
        fs::write(
            &session_path,
            format!("{}\n", assistant_line("Hello", "Read", "/work/repo/lib.rs")),
        )
        .expect("fixture should write");

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            ClaudeTranscriptAdapter::with_projects_root(root.clone()),
        )]);
        let binding = PaneTranscriptBinding {
            key: (3, 4),
            agent: "claude".to_string(),
            session_id: Some("sess-1".to_string()),
            cwd: Some(cwd.to_string()),
            shell_pid: None,
            process_started_at_unix_ms: None,
        };

        let result = tracker.sync_and_poll(std::slice::from_ref(&binding));
        assert_eq!(
            result.ticks.len(),
            1,
            "first poll folds the existing message"
        );
        assert!(result.ticks[0].baseline);
        assert_eq!(result.ticks[0].new_messages, 0);
        assert!(result.ticks[0].work_events.is_empty());
        assert_eq!(result.ticks[0].state.last_message.as_deref(), Some("Hello"));
        assert!(tracker.has_tracked_panes());
        let mut late = result;
        late.retain_tabs(&HashSet::new());
        assert!(
            late.ticks.is_empty(),
            "a late completion must not recreate a removed tab mirror"
        );

        // A second poll with no new bytes yields no ticks (change-gated).
        let result = tracker.sync_and_poll(std::slice::from_ref(&binding));
        assert!(result.ticks.is_empty(), "no new bytes yields no ticks");

        // Append a message; the next poll surfaces exactly the new turn.
        {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&session_path)
                .expect("append handle should open");
            writeln!(
                file,
                "{}",
                assistant_line("Second reply", "Edit", "/work/repo/main.rs")
            )
            .expect("append should write");
        }
        let result = tracker.sync_and_poll(std::slice::from_ref(&binding));
        assert_eq!(result.ticks.len(), 1);
        assert_eq!(result.ticks[0].new_messages, 1);
        assert_eq!(result.ticks[0].state.message_count, 2);
        assert_eq!(
            result.ticks[0].state.last_message.as_deref(),
            Some("Second reply")
        );

        // Truncation/rotation is consumed as a new baseline, not replayed as
        // fresh work. A genuinely appended turn after that baseline still emits.
        fs::write(
            &session_path,
            format!(
                "{}\n",
                assistant_line("Replayed old reply", "Edit", "/work/repo/main.rs")
            ),
        )
        .expect("rotated fixture should write");
        let result = tracker.sync_and_poll(std::slice::from_ref(&binding));
        assert!(result.ticks.is_empty(), "rotation history is not replayed");
        {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&session_path)
                .expect("append after rotation should open");
            writeln!(
                file,
                "{}",
                assistant_line("After rotation", "Edit", "/work/repo/after.rs")
            )
            .expect("append after rotation should write");
        }
        let result = tracker.sync_and_poll(std::slice::from_ref(&binding));
        assert_eq!(result.ticks.len(), 1);
        assert_eq!(result.ticks[0].new_messages, 1);

        // Dropping the binding removes the pane and reports it.
        let result = tracker.sync_and_poll(&[]);
        assert!(result.ticks.is_empty());
        assert_eq!(result.removed, vec![(3, 4)]);
        assert!(
            !tracker.has_tracked_panes(),
            "empty reconciliation releases worker tails"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn empty_transcript_baselines_then_preserves_every_operation_in_a_large_poll() {
        let root = unique_temp_dir("empty-baseline-large-delta");
        let cwd = "/work/empty";
        let project_dir = root.join(mangle_claude_cwd(cwd));
        fs::create_dir_all(&project_dir).unwrap();
        let session_path = project_dir.join("sess-empty.jsonl");
        fs::write(&session_path, "").unwrap();
        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            ClaudeTranscriptAdapter::with_projects_root(root.clone()),
        )]);
        let binding = PaneTranscriptBinding {
            key: (30, 2),
            agent: "claude".to_string(),
            session_id: Some("sess-empty".to_string()),
            cwd: Some(cwd.to_string()),
            shell_pid: None,
            process_started_at_unix_ms: None,
        };

        let baseline = tracker.sync_and_poll(std::slice::from_ref(&binding));
        assert_eq!(baseline.ticks.len(), 1);
        assert!(baseline.ticks[0].baseline);
        assert_eq!(baseline.ticks[0].new_messages, 0);
        assert!(baseline.ticks[0].work_events.is_empty());

        let appended = (0..25)
            .map(|index| assistant_line(&format!("edit {index}"), "Edit", "/work/empty/same.rs"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&session_path, format!("{appended}\n")).unwrap();
        let delta = tracker.sync_and_poll(std::slice::from_ref(&binding));
        assert_eq!(delta.ticks.len(), 1);
        assert!(!delta.ticks[0].baseline);
        assert_eq!(delta.ticks[0].new_messages, 25);
        let file_events: Vec<_> = delta.ticks[0]
            .work_events
            .iter()
            .filter_map(|event| match event {
                TranscriptWorkEvent::FileOperation { path, ordinal, .. } => {
                    Some((path.as_str(), *ordinal))
                }
                TranscriptWorkEvent::AssistantCompleted { .. } => None,
            })
            .collect();
        assert_eq!(file_events.len(), 25);
        assert!(file_events
            .iter()
            .all(|(path, _)| *path == "/work/empty/same.rs"));
        assert_eq!(file_events[0].1, 1);
        assert_eq!(file_events[24].1, 25);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn equal_and_larger_inode_replacements_are_replay_free_then_resume_appends() {
        let root = unique_temp_dir("inode-rotation");
        let cwd = "/work/rotation";
        let project_dir = root.join(mangle_claude_cwd(cwd));
        fs::create_dir_all(&project_dir).unwrap();
        let session_path = project_dir.join("sess-rotate.jsonl");
        let line = assistant_line("history", "Edit", "/work/rotation/history.rs");
        fs::write(&session_path, format!("{line}\n{line}\n")).unwrap();
        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            ClaudeTranscriptAdapter::with_projects_root(root.clone()),
        )]);
        let binding = PaneTranscriptBinding {
            key: (31, 2),
            agent: "claude".into(),
            session_id: Some("sess-rotate".into()),
            cwd: Some(cwd.into()),
            shell_pid: None,
            process_started_at_unix_ms: None,
        };
        assert!(tracker.sync_and_poll(std::slice::from_ref(&binding)).ticks[0].baseline);

        for (label, replay_count) in [("equal", 2), ("larger", 4)] {
            let replacement = project_dir.join(format!("replacement-{label}.jsonl"));
            fs::write(
                &replacement,
                format!("{}\n", vec![line.clone(); replay_count].join("\n")),
            )
            .unwrap();
            fs::rename(&replacement, &session_path).unwrap();
            let rotation = tracker.sync_and_poll(std::slice::from_ref(&binding));
            assert!(
                rotation.ticks.is_empty(),
                "{label} history is baseline-only"
            );

            let genuine = assistant_line(
                &format!("after-{label}"),
                "Edit",
                &format!("/work/rotation/{label}.rs"),
            );
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&session_path)
                .unwrap();
            writeln!(file, "{genuine}").unwrap();
            let appended = tracker.sync_and_poll(std::slice::from_ref(&binding));
            assert_eq!(appended.ticks.len(), 1);
            assert!(!appended.ticks[0].baseline);
            assert_eq!(appended.ticks[0].new_messages, 1);
        }

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn test_recent_files_records_only_writes_and_edits() {
        let adapter = ClaudeTranscriptAdapter::default();
        let lines = vec![
            assistant_line("making", "Write", "/a/x"),
            assistant_line("looking", "Read", "/a/y"),
            assistant_line("tweaking", "Edit", "/a/z"),
        ];

        let mut state = TranscriptState::default();
        adapter.ingest_lines(&lines, &mut state);

        // recent_files: only the created/edited files, in order, with ops.
        let paths: Vec<&str> = state.recent_files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["/a/x", "/a/z"], "reads are excluded");
        assert_eq!(state.recent_files[0].op, FileOp::Created);
        assert_eq!(state.recent_files[1].op, FileOp::Edited);
        assert!(
            state.recent_files.iter().all(|f| f.at_unix_ms > 0),
            "each entry carries a timestamp"
        );

        // files_touched is unchanged: it still records every referenced file,
        // including the read.
        assert!(state.files_touched.contains(&"/a/x".to_string()));
        assert!(state.files_touched.contains(&"/a/y".to_string()));
        assert!(state.files_touched.contains(&"/a/z".to_string()));
    }

    #[test]
    fn test_recent_files_dedup_keeps_latest_operation() {
        let adapter = ClaudeTranscriptAdapter::default();
        let mut state = TranscriptState::default();

        // First a Write, then (a later fold) an Edit of the same path.
        let first = adapter.ingest_lines(&[assistant_line("create", "Write", "/a/x")], &mut state);
        let second = adapter.ingest_lines(&[assistant_line("edit", "Edit", "/a/x")], &mut state);

        assert_eq!(state.recent_files.len(), 1, "deduped by path");
        let entry = &state.recent_files[0];
        assert_eq!(entry.path, "/a/x");
        assert_eq!(entry.op, FileOp::Edited, "newest operation wins");
        assert!(entry.at_unix_ms > 0);
        assert_eq!(first.file_events()[0].op, FileOp::Created);
        assert_eq!(second.file_events()[0].op, FileOp::Edited);
    }

    #[test]
    fn test_recent_files_caps_at_max_and_drops_oldest() {
        let adapter = ClaudeTranscriptAdapter::default();
        let mut state = TranscriptState::default();

        for i in 0..(MAX_FILES + 5) {
            adapter.ingest_lines(
                &[assistant_line("w", "Write", &format!("/a/f{i}"))],
                &mut state,
            );
        }

        assert_eq!(state.recent_files.len(), MAX_FILES, "list is capped");
        // The oldest few paths were drained from the front.
        assert_eq!(state.recent_files.first().unwrap().path, "/a/f5");
        assert_eq!(
            state.recent_files.last().unwrap().path,
            format!("/a/f{}", MAX_FILES + 4)
        );
    }

    #[test]
    fn test_resolve_touched_file_path_joins_relative_against_cwd() {
        // Absolute paths are returned verbatim regardless of cwd.
        assert_eq!(
            resolve_touched_file_path("/abs/x.rs", Some("/work/repo")),
            PathBuf::from("/abs/x.rs")
        );
        // Relative paths are joined onto the cwd.
        assert_eq!(
            resolve_touched_file_path("src/x.rs", Some("/work/repo")),
            PathBuf::from("/work/repo/src/x.rs")
        );
        // A relative path with no cwd stays relative.
        assert_eq!(
            resolve_touched_file_path("src/x.rs", None),
            PathBuf::from("src/x.rs")
        );
    }

    // ── Native turn evidence ────────────────────────────────────────────────
    //
    // Record shapes below are copied from live stores: `~/.claude/projects`,
    // `~/.codex/sessions`, and `~/.pi/agent/sessions`.

    const T0: &str = "2026-07-14T12:00:00.000Z";
    const T1: &str = "2026-07-14T12:00:30.000Z";
    const T2: &str = "2026-07-14T12:01:00.000Z";

    fn at(timestamp: &str) -> u64 {
        parse_rfc3339_unix_ms(timestamp).expect("fixture timestamp should parse")
    }

    fn fold(adapter: &dyn TranscriptAdapter, lines: &[serde_json::Value]) -> TranscriptState {
        let mut state = TranscriptState::default();
        let lines: Vec<String> = lines.iter().map(ToString::to_string).collect();
        adapter.ingest_lines(&lines, &mut state);
        state
    }

    #[test]
    fn claude_turn_evidence_separates_tool_use_and_streaming_from_a_completed_response() {
        let adapter = ClaudeTranscriptAdapter::with_projects_root(PathBuf::from("/nonexistent"));

        // A prompt opens the turn.
        let prompt = serde_json::json!({
            "type": "user", "timestamp": T0,
            "message": {"role": "user", "content": "do the thing"}
        });
        // An assistant record that stopped only to call a tool is NOT a
        // completed response.
        let tool_use = serde_json::json!({
            "type": "assistant", "timestamp": T1,
            "message": {"role": "assistant", "stop_reason": "tool_use", "content": [
                {"type": "tool_use", "name": "Read", "input": {"file_path": "/w/a.rs"}}
            ]}
        });
        // Neither is a record still streaming with a null stop reason.
        let streaming = serde_json::json!({
            "type": "assistant", "timestamp": T1,
            "message": {"role": "assistant", "stop_reason": null, "content": [
                {"type": "text", "text": "partial"}
            ]}
        });
        // A tool result feeds the still-open turn.
        let tool_result = serde_json::json!({
            "type": "user", "timestamp": T1, "toolUseResult": {"stdout": ""},
            "message": {"role": "user", "content": [{"type": "tool_result", "content": "ok"}]}
        });
        let completed = serde_json::json!({
            "type": "assistant", "timestamp": T2,
            "message": {"role": "assistant", "stop_reason": "end_turn", "content": [
                {"type": "text", "text": "all done"}
            ]}
        });

        assert_eq!(
            fold(&adapter, std::slice::from_ref(&prompt)).turn,
            PaneTurn::new(TurnPhase::Active, at(T0))
        );
        assert_eq!(
            fold(&adapter, &[prompt.clone(), tool_use.clone()]).turn,
            PaneTurn::new(TurnPhase::Active, at(T1))
        );
        assert_eq!(
            fold(&adapter, &[prompt.clone(), streaming]).turn,
            PaneTurn::new(TurnPhase::Active, at(T1))
        );
        assert_eq!(
            fold(&adapter, &[prompt.clone(), tool_use.clone(), tool_result]).turn,
            PaneTurn::new(TurnPhase::Active, at(T1))
        );
        assert_eq!(
            fold(&adapter, &[prompt, tool_use, completed]).turn,
            PaneTurn::new(TurnPhase::Completed, at(T2))
        );
    }

    #[test]
    fn claude_native_turn_id_follows_non_meta_user_uuid_and_clears_on_malformed_next_turn() {
        let adapter = ClaudeTranscriptAdapter::with_projects_root(PathBuf::from("/nonexistent"));
        let first = serde_json::json!({
            "type": "user", "timestamp": T0, "uuid": "claude-turn-1",
            "message": {"role": "user", "content": "first"}
        });
        let completed = serde_json::json!({
            "type": "assistant", "timestamp": T1,
            "message": {"role": "assistant", "stop_reason": "end_turn", "content": []}
        });
        let mut state = fold(&adapter, &[first, completed]);
        assert_eq!(
            state.native_turn_id,
            Some(ProviderNativeTurnId {
                provider: "claude".into(),
                id: "claude-turn-1".into(),
                observed_at_unix_ms: at(T0),
            })
        );

        let malformed_next = serde_json::json!({
            "type": "user", "timestamp": T2, "uuid": "\n",
            "message": {"role": "user", "content": "second"}
        });
        adapter.ingest_lines(&[malformed_next.to_string()], &mut state);
        assert_eq!(state.native_turn_id, None);
    }

    #[test]
    fn claude_turn_evidence_ignores_meta_records_sidechains_and_flags_api_errors() {
        let adapter = ClaudeTranscriptAdapter::with_projects_root(PathBuf::from("/nonexistent"));
        let completed = serde_json::json!({
            "type": "assistant", "timestamp": T0,
            "message": {"role": "assistant", "stop_reason": "end_turn", "content": []}
        });
        // Claude appends bookkeeping after a finished turn; none of it may
        // reopen the turn.
        let noise = [
            serde_json::json!({"type": "attachment", "timestamp": T1, "attachment": {}}),
            serde_json::json!({"type": "last-prompt", "leafUuid": "x"}),
            serde_json::json!({"type": "user", "timestamp": T1, "isMeta": true,
                "message": {"role": "user", "content": [{"type": "text", "text": "meta"}]}}),
            serde_json::json!({"type": "assistant", "timestamp": T1, "isSidechain": true,
                "message": {"role": "assistant", "stop_reason": "tool_use", "content": []}}),
        ];
        let mut lines = vec![completed];
        lines.extend(noise);
        assert_eq!(
            fold(&adapter, &lines).turn,
            PaneTurn::new(TurnPhase::Completed, at(T0))
        );

        let api_error = serde_json::json!({
            "type": "assistant", "timestamp": T2, "isApiErrorMessage": true,
            "message": {"role": "assistant", "content": [{"type": "text", "text": "API Error"}]}
        });
        assert_eq!(
            fold(&adapter, &[api_error]).turn,
            PaneTurn::new(TurnPhase::Errored, at(T2))
        );
    }

    #[test]
    fn codex_custom_home_discovers_current_format_working_turn() {
        let root = unique_temp_dir("codex-home");
        let custom = root.join("custom");
        let sessions = custom.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let meta = serde_json::json!({"type":"session_meta","payload":{
            "id":"inert-custom-session", "cwd":"/inert/project", "timestamp":T0
        }});
        let started = serde_json::json!({"type":"event_msg","timestamp":T1,
            "payload":{"type":"task_started","turn_id":"inert-turn"}});
        let file = sessions.join("rollout-inert.jsonl");
        fs::write(&file, format!("{meta}\n{started}\n")).unwrap();
        let adapter = CodexTranscriptAdapter::with_sessions_root(
            codex_sessions_root(Some(custom), Some(root.join("user"))).unwrap(),
        );
        let resolved = adapter
            .resolve_path(
                Some("inert-custom-session"),
                Some("/inert/project"),
                None,
                None,
            )
            .expect("explicit Codex home must be discoverable");
        assert_eq!(resolved.path, file);
        let mut state = TranscriptState::default();
        adapter.ingest_lines(&[meta.to_string(), started.to_string()], &mut state);
        assert_eq!(
            super::super::lifecycle::resolve(None, Some(state.turn), at(T1)),
            super::super::lifecycle::AgentLifecycle::Working
        );
        // Replayed historical activity must never become fresh merely by reading it.
        assert_eq!(
            super::super::lifecycle::resolve(None, Some(state.turn), at(T1) + 301_000),
            super::super::lifecycle::AgentLifecycle::Idle
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // The test owns the Child and always kills then waits below.
    fn live_codex_process_home_projects_current_turn_as_working() {
        let root = unique_temp_dir("codex-process-home");
        let project = root.join("project");
        let custom_home = root.join("pane-codex-home");
        let sessions = custom_home.join("sessions/2026/09/15");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&sessions).unwrap();
        let executable = root.join("codex");
        std::os::unix::fs::symlink("/bin/sleep", &executable).unwrap();
        let mut child = std::process::Command::new(&executable)
            .arg("30")
            .current_dir(&project)
            .env("CODEX_HOME", &custom_home)
            .spawn()
            .expect("inert Codex fixture should start");

        let observed_at = now_unix_ms();
        let meta = serde_json::json!({"type":"session_meta","payload":{
            "id":"live-custom-session", "cwd":project, "timestamp":observed_at
        }});
        let started = serde_json::json!({"type":"event_msg","timestamp":observed_at,
            "payload":{"type":"task_started","turn_id":"live-turn"}});
        fs::write(
            sessions.join("rollout-live-custom.jsonl"),
            format!("{meta}\n{started}\n"),
        )
        .unwrap();
        let binding = PaneTranscriptBinding {
            key: (31, 7),
            agent: "codex".into(),
            session_id: None,
            cwd: Some(project.to_string_lossy().into_owned()),
            shell_pid: Some(child.id() as i32),
            process_started_at_unix_ms: None,
        };

        let expected_root = custom_home.join("sessions");
        let detected_root = (0..100).find_map(|_| {
            let result = binding_codex_sessions_root(&binding);
            if result.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            result
        });
        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            CodexTranscriptAdapter::with_sessions_root(root.join("wrong-home/sessions")),
        )]);
        let result = tracker.sync_and_poll(std::slice::from_ref(&binding));

        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(detected_root.as_deref(), Some(expected_root.as_path()));
        assert_eq!(result.ticks.len(), 1);
        assert!(result.ticks[0].baseline);
        assert_eq!(
            super::super::lifecycle::resolve(None, Some(result.ticks[0].state.turn), observed_at,),
            super::super::lifecycle::AgentLifecycle::Working
        );
    }

    #[test]
    fn codex_turn_evidence_follows_task_started_and_task_complete() {
        let adapter = CodexTranscriptAdapter::with_sessions_root(PathBuf::from("/nonexistent"));
        let started = serde_json::json!({
            "type": "event_msg", "timestamp": T0, "payload": {"type": "task_started"}
        });
        // `token_count` is the only record Codex writes during a long
        // reasoning stretch — it must read as liveness, not silence.
        let thinking = serde_json::json!({
            "type": "event_msg", "timestamp": T1, "payload": {"type": "token_count"}
        });
        let complete = serde_json::json!({
            "type": "event_msg", "timestamp": T2, "payload": {"type": "task_complete"}
        });
        // Codex writes this *after* task_complete; a catch-all progress rule
        // would reopen every finished turn.
        let trailing = serde_json::json!({
            "type": "event_msg", "timestamp": T2, "payload": {"type": "thread_settings_applied"}
        });

        assert_eq!(
            fold(&adapter, &[started.clone(), thinking]).turn,
            PaneTurn::new(TurnPhase::Active, at(T1))
        );
        assert_eq!(
            fold(&adapter, &[started.clone(), complete.clone(), trailing]).turn,
            PaneTurn::new(TurnPhase::Completed, at(T2))
        );

        let errored = serde_json::json!({
            "type": "event_msg", "timestamp": T2,
            "payload": {"type": "error", "message": "stream disconnected"}
        });
        assert_eq!(
            fold(&adapter, &[started, errored]).turn,
            PaneTurn::new(TurnPhase::Errored, at(T2))
        );
    }

    #[test]
    fn codex_native_turn_id_accepts_started_or_delayed_completion_identity() {
        let adapter = CodexTranscriptAdapter::with_sessions_root(PathBuf::from("/nonexistent"));
        let started = serde_json::json!({
            "type": "event_msg", "timestamp": T0,
            "payload": {"type": "task_started", "turn_id": "codex-turn-1"}
        });
        let state = fold(&adapter, &[started]);
        assert_eq!(
            state
                .native_turn_id
                .as_ref()
                .map(|native| native.id.as_str()),
            Some("codex-turn-1")
        );

        let duplicate_user_message = serde_json::json!({
            "type": "event_msg", "timestamp": T1,
            "payload": {"type": "user_message", "message": "hello"}
        });
        assert_eq!(
            fold(
                &adapter,
                &[
                    serde_json::json!({
                        "type": "event_msg", "timestamp": T0,
                        "payload": {"type": "task_started", "turn_id": "codex-turn-1"}
                    }),
                    duplicate_user_message,
                ]
            )
            .native_turn_id
            .as_ref()
            .map(|native| native.id.as_str()),
            Some("codex-turn-1")
        );

        let missing = serde_json::json!({
            "type": "event_msg", "timestamp": T1, "payload": {"type": "task_started"}
        });
        let delayed = serde_json::json!({
            "type": "event_msg", "timestamp": T2,
            "payload": {"type": "task_complete", "turn_id": "codex-turn-2"}
        });
        let state = fold(&adapter, &[missing, delayed]);
        assert_eq!(
            state.native_turn_id,
            Some(ProviderNativeTurnId {
                provider: "codex".into(),
                id: "codex-turn-2".into(),
                observed_at_unix_ms: at(T2),
            })
        );

        let mut stale_state = fold(
            &adapter,
            &[
                serde_json::json!({
                    "type": "event_msg", "timestamp": T0,
                    "payload": {"type": "task_started", "turn_id": "codex-stale"}
                }),
                serde_json::json!({
                    "type": "event_msg", "timestamp": T1,
                    "payload": {"type": "task_complete", "turn_id": "codex-stale"}
                }),
            ],
        );
        let next_without_id = serde_json::json!({
            "type": "event_msg", "timestamp": T2,
            "payload": {"type": "user_message", "message": "next"}
        });
        adapter.ingest_lines(&[next_without_id.to_string()], &mut stale_state);
        assert_eq!(stale_state.native_turn_id, None);
    }

    #[test]
    fn unsupported_provider_keeps_native_turn_identity_absent() {
        let adapter = PiTranscriptAdapter::with_sessions_root(PathBuf::from("/nonexistent"));
        let prompt = serde_json::json!({
            "type": "message", "timestamp": T0, "uuid": "pi-turn",
            "message": {"role": "user", "content": "hello"}
        });
        assert_eq!(fold(&adapter, &[prompt]).native_turn_id, None);
    }

    #[test]
    fn pi_turn_evidence_follows_the_stop_reason() {
        let adapter = PiTranscriptAdapter::with_sessions_root(PathBuf::from("/nonexistent"));
        let tool_use = serde_json::json!({
            "type": "message", "timestamp": T0,
            "message": {"role": "assistant", "stopReason": "toolUse", "content": [
                {"type": "thinking", "thinking": "hidden"},
                {"type": "toolCall", "name": "read", "arguments": {"path": "/w/a.rs"}}
            ]}
        });
        let tool_result = serde_json::json!({
            "type": "message", "timestamp": T1,
            "message": {"role": "toolResult", "toolName": "read", "isError": false,
                        "content": [{"type": "text", "text": "ok"}]}
        });
        let stopped = serde_json::json!({
            "type": "message", "timestamp": T2,
            "message": {"role": "assistant", "stopReason": "stop", "content": [
                {"type": "text", "text": "done"}
            ]}
        });
        let errored = serde_json::json!({
            "type": "message", "timestamp": T2,
            "message": {"role": "assistant", "stopReason": "error", "content": []}
        });

        assert_eq!(
            fold(&adapter, &[tool_use.clone(), tool_result]).turn,
            PaneTurn::new(TurnPhase::Active, at(T1))
        );
        assert_eq!(
            fold(&adapter, &[tool_use.clone(), stopped]).turn,
            PaneTurn::new(TurnPhase::Completed, at(T2))
        );
        assert_eq!(
            fold(&adapter, &[tool_use, errored]).turn,
            PaneTurn::new(TurnPhase::Errored, at(T2))
        );
    }

    #[test]
    fn kimi_turn_evidence_follows_wire_step_boundaries() {
        let adapter = KimiTranscriptAdapter::with_sessions_root(PathBuf::from("/nonexistent"));
        let started = serde_json::json!({"type": "turn.prompt", "time": at(T0)});
        let thinking = serde_json::json!({
            "type": "context.append_loop_event", "time": at(T1),
            "event": {"type": "content.part", "part": {"type": "think", "think": "..."}}
        });
        let tool = serde_json::json!({
            "type": "context.append_loop_event", "time": at(T1),
            "event": {"type": "tool.call", "name": "Bash"}
        });
        let completed = serde_json::json!({
            "type": "context.append_loop_event", "time": at(T2),
            "event": {"type": "step.end"}
        });

        assert_eq!(
            fold(&adapter, &[started.clone(), thinking, tool]).turn,
            PaneTurn::new(TurnPhase::Active, at(T1))
        );
        assert_eq!(
            fold(&adapter, &[started, completed]).turn,
            PaneTurn::new(TurnPhase::Completed, at(T2))
        );
    }

    #[test]
    fn kimi_tracker_resolves_live_wire_log_by_cwd_and_process_start() {
        let root = unique_temp_dir("kimi-wire");
        let session_id = "246b7401-c81e-4273-8932-d0cc90bee679";
        let session_dir = root
            .join("wd-project")
            .join(format!("session_{session_id}"));
        let agent_dir = session_dir.join("agents/main");
        fs::create_dir_all(&agent_dir).expect("Kimi agent directory should be created");
        fs::write(
            session_dir.join("state.json"),
            serde_json::json!({"createdAt": T0, "workDir": "/work/kimi"}).to_string(),
        )
        .expect("Kimi state should write");
        fs::write(
            agent_dir.join("wire.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({"type": "metadata", "created_at": at(T0)}),
                serde_json::json!({
                    "type": "context.append_loop_event", "time": at(T2),
                    "event": {"type": "tool.call", "name": "Bash"}
                })
            ),
        )
        .expect("Kimi wire log should write");

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            KimiTranscriptAdapter::with_sessions_root(root.clone()),
        )]);
        let result = tracker.sync_and_poll(&[PaneTranscriptBinding {
            key: (23, 1),
            agent: "kimi".to_string(),
            session_id: None,
            cwd: Some("/work/kimi".to_string()),
            shell_pid: None,
            process_started_at_unix_ms: Some(at(T0)),
        }]);

        assert_eq!(result.ticks.len(), 1);
        assert_eq!(
            result.ticks[0].state.session_id,
            format!("session_{session_id}")
        );
        assert_eq!(
            result.ticks[0].state.turn,
            PaneTurn::new(TurnPhase::Active, at(T2))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn kimi_tracker_resolves_prefixed_session_id_from_resume_command() {
        let root = unique_temp_dir("kimi-prefixed-session");
        let session_id = "session_246b7401-c81e-4273-8932-d0cc90bee679";
        let session_dir = root.join("wd-project").join(session_id);
        let agent_dir = session_dir.join("agents/main");
        fs::create_dir_all(&agent_dir).expect("Kimi agent directory should be created");
        fs::write(
            session_dir.join("state.json"),
            serde_json::json!({"createdAt": T0, "workDir": "/work/kimi-resumed"}).to_string(),
        )
        .expect("Kimi state should write");
        fs::write(
            agent_dir.join("wire.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({"type": "metadata", "created_at": at(T0)}),
                serde_json::json!({"type": "turn.prompt", "time": at(T1)})
            ),
        )
        .expect("Kimi wire log should write");

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            KimiTranscriptAdapter::with_sessions_root(root.clone()),
        )]);
        let result = tracker.sync_and_poll(&[PaneTranscriptBinding {
            key: (24, 1),
            agent: "kimi".to_string(),
            session_id: Some(session_id.to_string()),
            cwd: Some("/work/kimi-resumed".to_string()),
            shell_pid: None,
            process_started_at_unix_ms: None,
        }]);

        assert_eq!(result.ticks.len(), 1);
        assert_eq!(result.ticks[0].state.session_id, session_id);
        assert_eq!(
            result.ticks[0].state.turn,
            PaneTurn::new(TurnPhase::Active, at(T1))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn two_codex_panes_sharing_a_cwd_keep_their_own_turn_state() {
        let root = unique_temp_dir("codex-two-pane-turns");
        let session_dir = root.join("2026/07/14");
        fs::create_dir_all(&session_dir).expect("session dir should be created");

        // Same cwd, two live sessions: one still working, one finished.
        for (id, started, last) in [
            (
                "codex-thinking",
                T0,
                serde_json::json!({"type": "event_msg", "timestamp": T2,
                                   "payload": {"type": "token_count"}}),
            ),
            (
                "codex-finished",
                T1,
                serde_json::json!({"type": "event_msg", "timestamp": T2,
                                   "payload": {"type": "task_complete"}}),
            ),
        ] {
            let meta = serde_json::json!({
                "type": "session_meta",
                "payload": {"id": id, "cwd": "/work/shared", "timestamp": started}
            });
            let task_started = serde_json::json!({
                "type": "event_msg", "timestamp": started, "payload": {"type": "task_started"}
            });
            fs::write(
                session_dir.join(format!("{id}.jsonl")),
                format!("{meta}\n{task_started}\n{last}\n"),
            )
            .expect("fixture should write");
        }

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            CodexTranscriptAdapter::with_sessions_root(root.clone()),
        )]);
        let result = tracker.sync_and_poll(&[
            PaneTranscriptBinding {
                key: (21, 1),
                agent: "codex".to_string(),
                session_id: None,
                cwd: Some("/work/shared".to_string()),
                shell_pid: None,
                process_started_at_unix_ms: Some(at(T0)),
            },
            PaneTranscriptBinding {
                key: (21, 2),
                agent: "codex".to_string(),
                session_id: None,
                cwd: Some("/work/shared".to_string()),
                shell_pid: None,
                process_started_at_unix_ms: Some(at(T1)),
            },
        ]);

        let turns: HashMap<PaneTranscriptKey, PaneTurn> = result
            .ticks
            .iter()
            .map(|tick| (tick.key, tick.state.turn))
            .collect();
        assert_eq!(turns[&(21, 1)], PaneTurn::new(TurnPhase::Active, at(T2)));
        assert_eq!(turns[&(21, 2)], PaneTurn::new(TurnPhase::Completed, at(T2)));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn two_pi_panes_sharing_a_cwd_keep_their_own_turn_state() {
        let root = unique_temp_dir("pi-two-pane-turns");
        let session_dir = root.join(mangle_pi_cwd("/work/pi-shared"));
        fs::create_dir_all(&session_dir).expect("session dir should be created");

        for (id, started, stop_reason) in
            [("pi-thinking", T0, "toolUse"), ("pi-finished", T1, "stop")]
        {
            let session = serde_json::json!({
                "type": "session", "id": id, "timestamp": started, "cwd": "/work/pi-shared"
            });
            let reply = serde_json::json!({
                "type": "message", "timestamp": T2,
                "message": {"role": "assistant", "stopReason": stop_reason,
                            "content": [{"type": "text", "text": id}]}
            });
            fs::write(
                session_dir.join(format!("2026-07-14T12-00-00-000Z_{id}.jsonl")),
                format!("{session}\n{reply}\n"),
            )
            .expect("fixture should write");
        }

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            PiTranscriptAdapter::with_sessions_root(root.clone()),
        )]);
        let result = tracker.sync_and_poll(&[
            PaneTranscriptBinding {
                key: (22, 1),
                agent: "pi".to_string(),
                session_id: None,
                cwd: Some("/work/pi-shared".to_string()),
                shell_pid: None,
                process_started_at_unix_ms: Some(at(T0)),
            },
            PaneTranscriptBinding {
                key: (22, 2),
                agent: "pi".to_string(),
                session_id: None,
                cwd: Some("/work/pi-shared".to_string()),
                shell_pid: None,
                process_started_at_unix_ms: Some(at(T1)),
            },
        ]);

        let turns: HashMap<PaneTranscriptKey, PaneTurn> = result
            .ticks
            .iter()
            .map(|tick| (tick.key, tick.state.turn))
            .collect();
        assert_eq!(turns[&(22, 1)], PaneTurn::new(TurnPhase::Active, at(T2)));
        assert_eq!(turns[&(22, 2)], PaneTurn::new(TurnPhase::Completed, at(T2)));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn two_claude_panes_in_one_project_track_thinking_and_waiting_separately() {
        let root = unique_temp_dir("claude-two-pane-turns");
        let cwd = "/work/claude-shared";
        let project_dir = root.join(mangle_claude_cwd(cwd));
        fs::create_dir_all(&project_dir).expect("project dir should be created");

        for (id, started, stop_reason) in [
            ("claude-thinking", T0, "tool_use"),
            ("claude-waiting", T1, "end_turn"),
        ] {
            let prompt = serde_json::json!({
                "type": "user", "timestamp": started,
                "message": {"role": "user", "content": "go"}
            });
            let reply = serde_json::json!({
                "type": "assistant", "timestamp": T2,
                "message": {"role": "assistant", "stop_reason": stop_reason,
                            "content": [{"type": "text", "text": id}]}
            });
            fs::write(
                project_dir.join(format!("{id}.jsonl")),
                format!("{prompt}\n{reply}\n"),
            )
            .expect("fixture should write");
        }

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            ClaudeTranscriptAdapter::with_projects_root(root.clone()),
        )]);
        let result = tracker.sync_and_poll(&[
            PaneTranscriptBinding {
                key: (23, 1),
                agent: "claude".to_string(),
                session_id: None,
                cwd: Some(cwd.to_string()),
                shell_pid: None,
                process_started_at_unix_ms: Some(at(T0)),
            },
            PaneTranscriptBinding {
                key: (23, 2),
                agent: "claude".to_string(),
                session_id: None,
                cwd: Some(cwd.to_string()),
                shell_pid: None,
                process_started_at_unix_ms: Some(at(T1)),
            },
        ]);

        let by_pane: HashMap<PaneTranscriptKey, (&str, PaneTurn)> = result
            .ticks
            .iter()
            .map(|tick| (tick.key, (tick.state.session_id.as_str(), tick.state.turn)))
            .collect();
        assert_eq!(
            by_pane[&(23, 1)],
            ("claude-thinking", PaneTurn::new(TurnPhase::Active, at(T2)))
        );
        assert_eq!(
            by_pane[&(23, 2)],
            (
                "claude-waiting",
                PaneTurn::new(TurnPhase::Completed, at(T2))
            )
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_long_thinking_gap_leaves_the_turn_open_without_any_new_record() {
        let root = unique_temp_dir("claude-long-think");
        let cwd = "/work/long-think";
        let project_dir = root.join(mangle_claude_cwd(cwd));
        fs::create_dir_all(&project_dir).expect("project dir should be created");
        let path = project_dir.join("thinking.jsonl");
        let prompt = serde_json::json!({
            "type": "user", "timestamp": T0, "message": {"role": "user", "content": "think hard"}
        });
        let tool_result = serde_json::json!({
            "type": "user", "timestamp": T1, "toolUseResult": {"stdout": ""},
            "message": {"role": "user", "content": [{"type": "tool_result", "content": "ok"}]}
        });
        fs::write(&path, format!("{prompt}\n{tool_result}\n")).expect("fixture should write");

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            ClaudeTranscriptAdapter::with_projects_root(root.clone()),
        )]);
        let binding = PaneTranscriptBinding {
            key: (24, 1),
            agent: "claude".to_string(),
            session_id: None,
            cwd: Some(cwd.to_string()),
            shell_pid: None,
            process_started_at_unix_ms: Some(at(T0)),
        };
        let first = tracker.sync_and_poll(std::slice::from_ref(&binding));
        let turn = first.ticks[0].state.turn;
        assert_eq!(turn, PaneTurn::new(TurnPhase::Active, at(T1)));

        // Nothing more is appended while the model reasons. Thirty seconds
        // later — far past the eight-second output-scan window that used to
        // collapse this pane to IDLE — the turn is still open.
        assert_eq!(
            crate::agents::resolve_agent_lifecycle(None, Some(turn), at(T1) + 30_000),
            crate::agents::AgentLifecycle::Working
        );
        // And a completion returns the pane to idle rather than latching.
        let completed = PaneTurn::new(TurnPhase::Completed, at(T2));
        assert_eq!(
            crate::agents::resolve_agent_lifecycle(None, Some(completed), at(T2) + 60_000),
            crate::agents::AgentLifecycle::Idle
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_replayed_transcript_is_stale_on_arrival_and_never_reads_as_live_work() {
        // Binding to a transcript whose last record is an hour old must not
        // make a quiet agent process look busy. The turn carries the record's
        // own timestamp, so it is already outside the freshness window.
        let root = unique_temp_dir("claude-replay");
        let cwd = "/work/replay";
        let project_dir = root.join(mangle_claude_cwd(cwd));
        fs::create_dir_all(&project_dir).expect("project dir should be created");
        let prompt = serde_json::json!({
            "type": "user", "timestamp": T0, "message": {"role": "user", "content": "old work"}
        });
        let tool_use = serde_json::json!({
            "type": "assistant", "timestamp": T1,
            "message": {"role": "assistant", "stop_reason": "tool_use", "content": []}
        });
        fs::write(
            project_dir.join("replayed.jsonl"),
            format!("{prompt}\n{tool_use}\n"),
        )
        .expect("fixture should write");

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            ClaudeTranscriptAdapter::with_projects_root(root.clone()),
        )]);
        let result = tracker.sync_and_poll(&[PaneTranscriptBinding {
            key: (25, 1),
            agent: "claude".to_string(),
            session_id: None,
            cwd: Some(cwd.to_string()),
            shell_pid: None,
            process_started_at_unix_ms: Some(at(T0)),
        }]);

        assert!(result.ticks[0].baseline);
        let turn = result.ticks[0].state.turn;
        assert_eq!(turn.phase, TurnPhase::Active);
        assert_eq!(
            crate::agents::resolve_agent_lifecycle(None, Some(turn), at(T1) + 3_600_000),
            crate::agents::AgentLifecycle::Idle
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn an_unrelated_native_session_is_rejected_and_leaves_no_turn_evidence() {
        // One transcript in the cwd, but the live process started well outside
        // the correlation window: no binding, so no turn evidence at all.
        let root = unique_temp_dir("claude-unrelated");
        let cwd = "/work/unrelated";
        let project_dir = root.join(mangle_claude_cwd(cwd));
        fs::create_dir_all(&project_dir).expect("project dir should be created");
        let reply = serde_json::json!({
            "type": "assistant", "timestamp": T0,
            "message": {"role": "assistant", "stop_reason": "tool_use", "content": []}
        });
        fs::write(
            project_dir.join("someone-elses.jsonl"),
            format!("{reply}\n"),
        )
        .expect("fixture should write");

        let tracker = TranscriptTracker::with_adapters(vec![Box::new(
            ClaudeTranscriptAdapter::with_projects_root(root.clone()),
        )]);
        let result = tracker.sync_and_poll(&[PaneTranscriptBinding {
            key: (26, 1),
            agent: "claude".to_string(),
            session_id: None,
            cwd: Some(cwd.to_string()),
            shell_pid: None,
            process_started_at_unix_ms: Some(at(T0) + MAX_SESSION_START_SKEW_MS + 1),
        }]);

        assert!(result.ticks.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    fn assert_child_agent_conformance(
        adapter: &dyn TranscriptAdapter,
        provider: &str,
        start: Vec<String>,
        complete: String,
    ) -> Vec<String> {
        let mut state = TranscriptState {
            agent: provider.into(),
            session_id: "parent-session".into(),
            ..Default::default()
        };
        adapter.ingest_lines(&start, &mut state);
        let stable_ids: Vec<String> = state
            .child_agents
            .iter()
            .map(|child| child.stable_id.clone())
            .collect();
        assert_eq!(stable_ids.len(), 2);
        assert_ne!(stable_ids[0], stable_ids[1]);
        assert!(state.child_agents.iter().all(|child| {
            child.provider == provider
                && child.parent_id == format!("{provider}:parent-session")
                && child.state == AgentLifecycle::Working
        }));

        adapter.ingest_lines(&start, &mut state);
        assert_eq!(state.child_agents.len(), 2, "replay must deduplicate");
        assert_eq!(
            state
                .child_agents
                .iter()
                .map(|child| child.stable_id.clone())
                .collect::<Vec<_>>(),
            stable_ids
        );

        adapter.ingest_lines(&[complete], &mut state);
        assert_eq!(state.child_agents[0].state, AgentLifecycle::Done);
        assert_eq!(state.child_agents[1].state, AgentLifecycle::Working);
        stable_ids
    }

    #[test]
    fn claude_and_codex_child_agents_share_one_reconciliation_contract() {
        let claude = ClaudeTranscriptAdapter::default();
        let claude_start = serde_json::json!({
            "type": "assistant", "timestamp": T0,
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tool-a", "name": "Agent",
                 "input": {"name": "researcher", "description": "inspect parser"}},
                {"type": "tool_use", "id": "tool-b", "name": "Task",
                "input": {"name": "researcher", "description": "inspect UI"}}
            ]}
        });
        let claude_complete = serde_json::json!({
            "type": "user", "timestamp": T1,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tool-a", "content": "done"}
            ]}
        });
        assert_child_agent_conformance(
            &claude,
            "claude",
            vec![claude_start.to_string()],
            claude_complete.to_string(),
        );

        let codex = CodexTranscriptAdapter::default();
        let codex_start = vec![
            serde_json::json!({
                "type": "response_item", "timestamp": T0,
                "payload": {"type": "function_call", "name": "spawn_agent",
                    "call_id": "call-a", "arguments": serde_json::json!({
                        "task_name": "research_a", "message": "inspect parser"
                    }).to_string()}
            })
            .to_string(),
            serde_json::json!({
                "type": "response_item", "timestamp": T0,
                "payload": {"type": "function_call_output", "call_id": "call-a",
                    "output": serde_json::json!({"task_name": "/root/research_a"}).to_string()}
            })
            .to_string(),
            serde_json::json!({
                "type": "response_item", "timestamp": T0,
                "payload": {"type": "function_call", "name": "spawn_agent",
                    "call_id": "call-b", "arguments": serde_json::json!({
                        "task_name": "research_b", "message": "inspect UI"
                    }).to_string()}
            })
            .to_string(),
            serde_json::json!({
                "type": "response_item", "timestamp": T0,
                "payload": {"type": "function_call_output", "call_id": "call-b",
                    "output": serde_json::json!({"task_name": "/root/research_b"}).to_string()}
            })
            .to_string(),
        ];
        let codex_complete = serde_json::json!({
            "type": "response_item", "timestamp": T1,
            "payload": {"type": "agent_message", "author": "/root/research_a",
                "recipient": "/root", "content": [{
                "type": "input_text",
                "text": "Message Type: FINAL_ANSWER\nTask name: /root/research_a\nSender: worker\nPayload:\ndone"
            }]}
        });
        let first_ids = assert_child_agent_conformance(
            &codex,
            "codex",
            codex_start.clone(),
            codex_complete.to_string(),
        );

        let mut resumed = TranscriptState {
            agent: "codex".into(),
            session_id: "parent-session".into(),
            ..Default::default()
        };
        codex.ingest_lines(&codex_start, &mut resumed);
        assert_eq!(
            resumed
                .child_agents
                .iter()
                .map(|child| child.stable_id.clone())
                .collect::<Vec<_>>(),
            first_ids,
            "parent resume must replay to the same canonical identities"
        );
    }

    #[test]
    fn changed_codex_child_records_degrade_to_parent_only() {
        let adapter = CodexTranscriptAdapter::default();
        let mut state = TranscriptState {
            agent: "codex".into(),
            session_id: "parent-session".into(),
            ..Default::default()
        };
        let unsupported = serde_json::json!({
            "type": "response_item", "timestamp": T0,
            "payload": {"type": "function_call", "name": "spawn_agent_v2",
                "call_id": "changed", "arguments": "not-json"}
        });
        let failed_call = serde_json::json!({
            "type": "response_item", "timestamp": T0,
            "payload": {"type": "function_call", "name": "spawn_agent",
                "call_id": "failed", "arguments": serde_json::json!({
                    "task_name": "never_started", "message": "cannot start"
                }).to_string()}
        });
        let failed_output = serde_json::json!({
            "type": "response_item", "timestamp": T1,
            "payload": {"type": "function_call_output", "call_id": "failed",
                "output": "collab spawn failed: agent thread limit reached"}
        });
        adapter.ingest_lines(
            &[
                unsupported.to_string(),
                failed_call.to_string(),
                failed_output.to_string(),
            ],
            &mut state,
        );
        assert!(state.child_agents.is_empty());
        assert_eq!(state.agent, "codex");
        assert_eq!(state.session_id, "parent-session");
    }
}
