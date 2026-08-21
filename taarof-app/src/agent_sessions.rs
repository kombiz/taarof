#[cfg(feature = "opencode-history")]
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::workspace::Tab;
use crate::AppState;

const AGENT_SESSION_SCAN_TTL: Duration = Duration::from_secs(20);
const MAX_SESSIONS_PER_PROVIDER: usize = 50;
const MAX_FILE_CANDIDATES: usize = 200;
const MAX_SAMPLE_LINES: usize = 64;

/// Newest record files enumerated per provider on a remote host.
const REMOTE_MAX_FILES_PER_PROVIDER: usize = 20;
/// Lines read from each remote record file before any are selected.
const REMOTE_MAX_SCANNED_LINES: usize = 32;
/// Record lines kept from each remote file — enough for the session header the
/// parsers need plus an early user message for the title, not a transcript.
const REMOTE_MAX_SAMPLE_LINES: usize = 8;
/// Per-line character cap. Measured against real stores (2026-08-14): a codex
/// `session_meta` line — always line 1, and the only line carrying the id and
/// cwd — runs 18k–48k chars because it embeds the full instruction preamble;
/// claude first lines reach ~60k. An over-cap line is **dropped whole and
/// counted**, never truncated: a truncated JSON line is invalid JSON, so
/// truncation silently destroys the record instead of reporting a loss.
const REMOTE_MAX_LINE_CHARS: usize = 65536;
/// Byte budget per remote record file, applied only *after* the first kept
/// line. The first line is where every provider puts the session id and cwd, so
/// the budget must never be able to reject the one line that carries the
/// record — it trims the extra lines sampled for a title. A file stopped by
/// this budget is counted, not silently cut. Deliberately below
/// [`REMOTE_MAX_LINE_CHARS`]: a real codex header alone exceeds it, and must
/// still come through.
const REMOTE_MAX_BLOCK_BYTES: usize = 32768;
/// Sessions kept per remote host after parsing.
const REMOTE_MAX_SESSIONS_PER_HOST: usize = 60;
/// Hard cap on the bytes read back from one remote enumeration. Sized against
/// the per-block budget above so a normal round is never clipped by the reader.
const REMOTE_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Wall-clock deadline for one remote enumeration, connect included.
const REMOTE_SCAN_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a background round is kicked. Matches the catalog TTL: a scan that
/// misses the cache is the signal that remote data is worth refreshing.
const REMOTE_REFRESH_INTERVAL: Duration = AGENT_SESSION_SCAN_TTL;
/// How long a completed round's records stay `stale: false`. Deliberately
/// wider than the refresh interval: a healthy host is refreshed every
/// [`REMOTE_REFRESH_INTERVAL`], so a window equal to the interval would report
/// every host stale in steady state, and `stale` has to mean "degraded", not
/// "normal cache age". A host reads stale only once it has missed several
/// rounds, failed, or never been probed.
const REMOTE_FRESHNESS_WINDOW: Duration = Duration::from_secs(90);
/// Block framing for the remote enumeration stream. Content lines are prefixed
/// with `|` so no file can forge a header from its own contents.
const REMOTE_BLOCK_HEADER: &str = "##TAAROF-REMOTE-V1";
const REMOTE_BLOCK_END: &str = "##TAAROF-REMOTE-END";
/// Per-block loss report: `##TAAROF-REMOTE-STATS <dropped> <budget-stopped>`.
const REMOTE_BLOCK_STATS: &str = "##TAAROF-REMOTE-STATS";
const REMOTE_CONTENT_PREFIX: char = '|';

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AgentSessionProviderStatus {
    pub name: String,
    pub ok: bool,
    pub history_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub session_count: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LiveAgentBinding {
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub workspace_id: u32,
    pub workspace_name: String,
    pub tab_id: u32,
    pub tab_name: String,
    pub pane_id: u32,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AgentSessionRecord {
    pub agent: String,
    pub session_id: String,
    pub title: String,
    pub cwd: String,
    /// Source host name for a record discovered over SSH. Absent — and so
    /// absent from the serialized payload — for every local record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_unix_ms: Option<u64>,
    pub updated_at_unix_ms: u64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_binding: Option<LiveAgentBinding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_unavailable_reason: Option<String>,
}

/// Per-host status for remote discovery. One entry per host taarof has a
/// reason to probe, whether or not the last probe succeeded.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RemoteHostStatus {
    pub host: String,
    pub ssh_target: String,
    /// Whether the most recent probe of this host completed.
    pub ok: bool,
    /// Whether these records are degraded rather than merely cached: set when a
    /// probe failed, when none has completed yet, or when the last good round
    /// has aged past the freshness window (several missed refreshes). A healthy
    /// host being refreshed on schedule reports `false`.
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub session_count: usize,
    /// Record lines that existed on the host but are not represented above:
    /// over the per-line cap, or not valid JSON. Non-zero means this host is
    /// under-reported.
    pub dropped_lines: usize,
    /// Record files whose per-file byte budget stopped the sample early.
    pub truncated_files: usize,
    /// Plain-language summary of any under-reporting, absent when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// When the records below were observed, absent until a probe succeeds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AgentSessionsSnapshot {
    pub schema: &'static str,
    pub generated_at_unix_ms: u64,
    pub providers: Vec<AgentSessionProviderStatus>,
    pub sessions: Vec<AgentSessionRecord>,
    /// Additive: omitted entirely when no remote host is configured or live,
    /// so a local-only payload is byte-identical to the pre-EXAMPLE-178 schema.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remote_hosts: Vec<RemoteHostStatus>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentSessionDiscovery {
    pub providers: Vec<AgentSessionProviderStatus>,
    pub sessions: Vec<AgentSessionRecord>,
    pub remote_hosts: Vec<RemoteHostStatus>,
}

#[derive(Clone, Debug)]
struct CachedDiscovery {
    observed_at: Instant,
    discovery: AgentSessionDiscovery,
}

pub trait AgentSessionScanner: Send + Sync {
    fn scan(&self) -> AgentSessionDiscovery;
}

pub struct AgentSessionCatalog {
    scanner: Arc<dyn AgentSessionScanner>,
    ttl: Duration,
    cache: Mutex<Option<CachedDiscovery>>,
}

impl AgentSessionCatalog {
    pub fn with_scanner(scanner: Arc<dyn AgentSessionScanner>) -> Self {
        Self::with_scanner_and_ttl(scanner, AGENT_SESSION_SCAN_TTL)
    }

    pub fn with_scanner_and_ttl(scanner: Arc<dyn AgentSessionScanner>, ttl: Duration) -> Self {
        Self {
            scanner,
            ttl,
            cache: Mutex::new(None),
        }
    }

    pub async fn snapshot(&self, live_bindings: Vec<LiveAgentBinding>) -> AgentSessionsSnapshot {
        let discovery = if let Some(discovery) = self.cached_discovery() {
            discovery
        } else {
            let scanner = Arc::clone(&self.scanner);
            match tokio::task::spawn_blocking(move || scanner.scan()).await {
                Ok(discovery) => {
                    self.store_discovery(discovery.clone());
                    discovery
                }
                Err(error) => self
                    .cached_discovery()
                    .unwrap_or_else(|| internal_scan_error(&error.to_string())),
            }
        };
        finalize_snapshot(discovery, &live_bindings)
    }

    pub fn snapshot_blocking(&self, live_bindings: Vec<LiveAgentBinding>) -> AgentSessionsSnapshot {
        let discovery = if let Some(discovery) = self.cached_discovery() {
            discovery
        } else {
            let discovery = self.scanner.scan();
            self.store_discovery(discovery.clone());
            discovery
        };
        finalize_snapshot(discovery, &live_bindings)
    }

    fn cached_discovery(&self) -> Option<AgentSessionDiscovery> {
        let cache = self
            .cache
            .lock()
            .expect("agent session cache lock should hold");
        cache
            .as_ref()
            .filter(|cached| cached.observed_at.elapsed() < self.ttl)
            .map(|cached| cached.discovery.clone())
    }

    fn store_discovery(&self, discovery: AgentSessionDiscovery) {
        let mut cache = self
            .cache
            .lock()
            .expect("agent session cache lock should hold");
        *cache = Some(CachedDiscovery {
            observed_at: Instant::now(),
            discovery,
        });
    }
}

pub fn default_catalog() -> Arc<AgentSessionCatalog> {
    static CATALOG: OnceLock<Arc<AgentSessionCatalog>> = OnceLock::new();
    CATALOG
        .get_or_init(|| {
            Arc::new(AgentSessionCatalog::with_scanner(Arc::new(
                DefaultAgentSessionScanner::default(),
            )))
        })
        .clone()
}

/// Capture the process-dependent discovery roots on GTK before any persistence
/// worker can use the default catalog. Scanning those roots remains off GTK.
pub fn initialize_default_catalog() -> Arc<AgentSessionCatalog> {
    default_catalog()
}

#[derive(Clone, Debug)]
pub struct DiscoveryRoots {
    claude_projects_dir: Option<PathBuf>,
    codex_sessions_dir: Option<PathBuf>,
    pi_sessions_dir: Option<PathBuf>,
    pi_session_map_path: Option<PathBuf>,
    kimi_session_index_path: Option<PathBuf>,
    kimi_sessions_dir: Option<PathBuf>,
    opencode_db_path: Option<PathBuf>,
}

impl DiscoveryRoots {
    fn from_home(home: Option<PathBuf>) -> Self {
        let home = home.unwrap_or_else(|| PathBuf::from("/"));
        Self {
            claude_projects_dir: Some(home.join(".claude/projects")),
            codex_sessions_dir: Some(home.join(".codex/sessions")),
            pi_sessions_dir: Some(home.join(".pi/agent/sessions")),
            pi_session_map_path: Some(home.join(".pi/pi-acp/session-map.json")),
            kimi_session_index_path: Some(home.join(".kimi-code/session_index.jsonl")),
            kimi_sessions_dir: Some(home.join(".kimi-code/sessions")),
            opencode_db_path: Some(home.join(".local/share/opencode/opencode.db")),
        }
    }
}

pub struct DefaultAgentSessionScanner {
    roots: DiscoveryRoots,
    remote: Option<Arc<RemoteSessionProbe>>,
}

impl Default for DefaultAgentSessionScanner {
    fn default() -> Self {
        Self {
            roots: DiscoveryRoots::from_home(dirs::home_dir()),
            remote: Some(Arc::new(RemoteSessionProbe::default())),
        }
    }
}

impl DefaultAgentSessionScanner {
    #[cfg(test)]
    fn with_roots_and_remote(
        roots: DiscoveryRoots,
        remote: Option<Arc<RemoteSessionProbe>>,
    ) -> Self {
        Self { roots, remote }
    }
}

impl AgentSessionScanner for DefaultAgentSessionScanner {
    fn scan(&self) -> AgentSessionDiscovery {
        let mut providers = Vec::new();
        let mut sessions = Vec::new();

        let (claude_status, mut claude_sessions) = discover_claude_sessions(
            self.roots.claude_projects_dir.as_deref(),
            MAX_SESSIONS_PER_PROVIDER,
        );
        providers.push(claude_status);
        sessions.append(&mut claude_sessions);

        let (codex_status, mut codex_sessions) = discover_codex_sessions(
            self.roots.codex_sessions_dir.as_deref(),
            MAX_SESSIONS_PER_PROVIDER,
        );
        providers.push(codex_status);
        sessions.append(&mut codex_sessions);

        let (pi_status, mut pi_sessions) = discover_pi_sessions(
            self.roots.pi_sessions_dir.as_deref(),
            self.roots.pi_session_map_path.as_deref(),
            MAX_SESSIONS_PER_PROVIDER,
        );
        providers.push(pi_status);
        sessions.append(&mut pi_sessions);

        let (kimi_status, mut kimi_sessions) = discover_kimi_sessions(
            self.roots.kimi_session_index_path.as_deref(),
            self.roots.kimi_sessions_dir.as_deref(),
            MAX_SESSIONS_PER_PROVIDER,
        );
        providers.push(kimi_status);
        sessions.append(&mut kimi_sessions);

        let (opencode_status, mut opencode_sessions) = discover_opencode_sessions(
            self.roots.opencode_db_path.as_deref(),
            MAX_SESSIONS_PER_PROVIDER,
        );
        providers.push(opencode_status);
        sessions.append(&mut opencode_sessions);

        providers.push(AgentSessionProviderStatus {
            name: "copilot".to_string(),
            ok: true,
            history_available: false,
            warning: Some(
                "Copilot history is unavailable on this host; only live Copilot tabs are shown."
                    .to_string(),
            ),
            error: None,
            session_count: 0,
        });

        // Remote sections are served from the probe's cache and never block
        // this scan: a slow or dead host degrades to a stale/error section
        // while every local provider above is already collected.
        let remote_hosts = match self.remote.as_ref() {
            Some(probe) => {
                Arc::clone(probe).refresh_if_due();
                let (remote_hosts, mut remote_sessions) = probe.sections();
                sessions.append(&mut remote_sessions);
                remote_hosts
            }
            None => Vec::new(),
        };

        AgentSessionDiscovery {
            providers,
            sessions,
            remote_hosts,
        }
    }
}

/// A host taarof already has a reason to reach: either configured with an
/// `ssh_target`, or backing a live remote tmux pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteHostTarget {
    pub host: String,
    pub ssh_target: String,
}

/// Where the remote host list comes from. Injected so tests never read the
/// live config or the running workspace.
pub trait RemoteHostInventory: Send + Sync {
    fn hosts(&self) -> Vec<RemoteHostTarget>;
}

/// The seam that actually runs the enumeration argv. The production impl
/// shells out to `ssh`; tests inject fixture output instead.
pub trait RemoteCommandRunner: Send + Sync {
    fn run(&self, argv: &[String]) -> Result<String, String>;
}

/// The single bounded, read-only enumeration script run once per host per
/// refresh. It prints paths, mtimes and the first few JSONL lines of the newest
/// record files — never environment values, never whole transcripts.
///
/// Every emitted record line is whole. A line over the per-line cap, or a file
/// stopped by the per-block byte budget, is reported in a trailing stats line
/// rather than truncated: truncating a JSONL line yields invalid JSON, which
/// destroys the record silently instead of reporting the loss.
pub(crate) fn remote_enumeration_script() -> String {
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

/// One BatchMode ssh invocation carrying the enumeration script. Built with
/// the same non-interactive wrapper the tmux poller uses, so auth prompts and
/// hangs are impossible by construction.
pub(crate) fn remote_enumeration_command(ssh_target: &str) -> Vec<String> {
    crate::tmux::wrap_for_target_noninteractive(
        &crate::tmux::TmuxTarget::Remote {
            ssh_target: ssh_target.to_string(),
        },
        vec![
            "sh".to_string(),
            "-c".to_string(),
            remote_enumeration_script(),
        ],
    )
}

/// `ssh -t <target> 'cd <quoted cwd> && <provider resume>'`. The local resume
/// command is escaped whole so it survives as one word on the remote shell.
pub(crate) fn remote_resume_command(
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RemoteParseStats {
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
    fn lost_lines(&self) -> usize {
        self.dropped_lines + self.unparsable_lines
    }

    /// A short, honest summary, or None when nothing was lost.
    fn warning(&self) -> Option<String> {
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
pub(crate) fn parse_remote_enumeration_output(
    target: &RemoteHostTarget,
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
            session.host = Some(target.host.clone());
            session.resume_command = Some(remote_resume_command(
                &target.ssh_target,
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
                status: "recent".to_string(),
                live_binding: None,
                resume_command: Some(build_resume_command("kimi", &cwd, &session_id)),
                resume_unavailable_reason: None,
            })
        })
        .collect()
}

#[derive(Clone, Debug, Default)]
struct RemoteHostCache {
    ssh_target: String,
    sessions: Vec<AgentSessionRecord>,
    stats: RemoteParseStats,
    observed_at: Option<Instant>,
    observed_at_unix_ms: Option<u64>,
    last_error: Option<String>,
    ever_succeeded: bool,
}

#[derive(Default)]
struct RemoteProbeState {
    hosts: Vec<(String, RemoteHostCache)>,
    last_round_at: Option<Instant>,
    refresh_in_flight: bool,
}

/// Owns the remote half of discovery: the per-host cache, the refresh cadence,
/// and the honesty of the degraded state. `sections()` never performs I/O — it
/// reports what the last completed round found, so the local snapshot is never
/// held up by a slow host.
pub struct RemoteSessionProbe {
    runner: Arc<dyn RemoteCommandRunner>,
    inventory: Arc<dyn RemoteHostInventory>,
    /// How often a round is kicked.
    refresh_interval: Duration,
    /// How long a completed round stays non-stale. Wider than the refresh
    /// interval on purpose — see [`REMOTE_FRESHNESS_WINDOW`].
    freshness_window: Duration,
    background_refresh: bool,
    state: Mutex<RemoteProbeState>,
}

impl Default for RemoteSessionProbe {
    fn default() -> Self {
        Self {
            runner: Arc::new(SshRemoteCommandRunner),
            inventory: Arc::new(LiveRemoteHostInventory),
            refresh_interval: REMOTE_REFRESH_INTERVAL,
            freshness_window: REMOTE_FRESHNESS_WINDOW,
            background_refresh: true,
            state: Mutex::new(RemoteProbeState::default()),
        }
    }
}

impl RemoteSessionProbe {
    /// A probe that only refreshes when asked. Used by tests that want a
    /// deterministic round and no thread.
    #[cfg(test)]
    fn manual(
        runner: Arc<dyn RemoteCommandRunner>,
        inventory: Arc<dyn RemoteHostInventory>,
    ) -> Self {
        Self {
            background_refresh: false,
            ..Self::background(runner, inventory, REMOTE_REFRESH_INTERVAL)
        }
    }

    /// A probe that refreshes off-thread exactly as production does, with a
    /// caller-chosen refresh interval so the TTL gate is testable.
    #[cfg(test)]
    fn background(
        runner: Arc<dyn RemoteCommandRunner>,
        inventory: Arc<dyn RemoteHostInventory>,
        refresh_interval: Duration,
    ) -> Self {
        Self {
            runner,
            inventory,
            refresh_interval,
            freshness_window: REMOTE_FRESHNESS_WINDOW,
            background_refresh: true,
            state: Mutex::new(RemoteProbeState::default()),
        }
    }

    /// Kick a background refresh when the cached round has aged past the
    /// refresh interval. Returns immediately; at most one round is ever in
    /// flight.
    fn refresh_if_due(self: Arc<Self>) {
        self.refresh_if_due_with(|probe| {
            std::thread::Builder::new()
                .name("taarof-remote-agent-sessions".to_string())
                .spawn(move || probe.run_round())
                .map(|_| ())
                .map_err(|_| ())
        });
    }

    /// The gate, the in-flight latch, and the latch's error path, with the
    /// thread spawn injected so a test can force a spawn failure.
    fn refresh_if_due_with(self: Arc<Self>, spawn: impl FnOnce(Arc<Self>) -> Result<(), ()>) {
        if !self.background_refresh {
            return;
        }
        {
            let mut state = self.lock_state();
            let due = state
                .last_round_at
                .is_none_or(|observed| observed.elapsed() >= self.refresh_interval);
            if !due || state.refresh_in_flight {
                return;
            }
            state.refresh_in_flight = true;
        }
        if spawn(Arc::clone(&self)).is_err() {
            // The latch must not stick, or remote discovery would go quiet for
            // the rest of the process.
            self.lock_state().refresh_in_flight = false;
        }
    }

    /// One bounded ssh command per host, run inline. Tests drive this directly
    /// so a round is deterministic; production always refreshes off-thread.
    #[cfg(test)]
    fn refresh_blocking(&self) {
        self.run_round();
    }

    fn run_round(&self) {
        let targets = self.inventory.hosts();
        let mut observed: Vec<(String, RemoteHostCache)> = Vec::new();
        for target in targets {
            let previous = self.cached_host(&target.host);
            let outcome = self
                .runner
                .run(&remote_enumeration_command(&target.ssh_target));
            let cache = match outcome {
                Ok(output) => {
                    let (sessions, stats) = parse_remote_enumeration_output(&target, &output);
                    RemoteHostCache {
                        ssh_target: target.ssh_target.clone(),
                        sessions,
                        stats,
                        observed_at: Some(Instant::now()),
                        observed_at_unix_ms: Some(unix_time_ms()),
                        last_error: None,
                        ever_succeeded: true,
                    }
                }
                Err(error) => {
                    let previous = previous.unwrap_or_default();
                    RemoteHostCache {
                        ssh_target: target.ssh_target.clone(),
                        sessions: previous.sessions,
                        stats: previous.stats,
                        observed_at: previous.observed_at,
                        observed_at_unix_ms: previous.observed_at_unix_ms,
                        last_error: Some(error),
                        ever_succeeded: previous.ever_succeeded,
                    }
                }
            };
            observed.push((target.host, cache));
        }

        let mut state = self.lock_state();
        state.hosts = observed;
        state.last_round_at = Some(Instant::now());
        // Whoever set the latch, the round it guarded is over.
        state.refresh_in_flight = false;
    }

    /// The cached view: one section per host taarof means to probe, plus its
    /// records. Performs no I/O, so it can never hold up the local snapshot.
    /// A host that has not been probed yet still gets a section, marked stale —
    /// silence would read as "no remote agents", which is a different claim.
    fn sections(&self) -> (Vec<RemoteHostStatus>, Vec<AgentSessionRecord>) {
        let targets = self.inventory.hosts();
        let state = self.lock_state();
        let mut statuses = Vec::new();
        let mut sessions = Vec::new();
        for target in targets {
            let cache = state
                .hosts
                .iter()
                .find(|(name, _)| name == &target.host)
                .map(|(_, cache)| cache);
            let Some(cache) = cache else {
                statuses.push(RemoteHostStatus {
                    host: target.host,
                    ssh_target: target.ssh_target,
                    ok: true,
                    stale: true,
                    error: None,
                    session_count: 0,
                    dropped_lines: 0,
                    truncated_files: 0,
                    warning: Some(
                        "This host has not been probed yet; its first results arrive with the next refresh."
                            .to_string(),
                    ),
                    observed_at_unix_ms: None,
                });
                continue;
            };
            // Freshness is judged against the window, not the refresh interval:
            // a healthy host refreshed on schedule must not read as degraded.
            let fresh = cache.last_error.is_none()
                && cache
                    .observed_at
                    .is_some_and(|observed| observed.elapsed() < self.freshness_window);
            statuses.push(RemoteHostStatus {
                host: target.host,
                ssh_target: cache.ssh_target.clone(),
                ok: cache.last_error.is_none() && cache.ever_succeeded,
                stale: !fresh,
                error: cache.last_error.clone(),
                session_count: cache.sessions.len(),
                dropped_lines: cache.stats.lost_lines(),
                truncated_files: cache.stats.truncated_files,
                warning: cache.stats.warning(),
                observed_at_unix_ms: cache.observed_at_unix_ms,
            });
            sessions.extend(cache.sessions.iter().cloned());
        }
        (statuses, sessions)
    }

    #[cfg(test)]
    fn refresh_in_flight(&self) -> bool {
        self.lock_state().refresh_in_flight
    }

    #[cfg(test)]
    fn completed_a_round(&self) -> bool {
        self.lock_state().last_round_at.is_some()
    }

    fn cached_host(&self, host: &str) -> Option<RemoteHostCache> {
        self.lock_state()
            .hosts
            .iter()
            .find(|(name, _)| name == host)
            .map(|(_, cache)| cache.clone())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, RemoteProbeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Configured remote hosts, plus the ssh targets of live remote tmux panes.
struct LiveRemoteHostInventory;

impl RemoteHostInventory for LiveRemoteHostInventory {
    fn hosts(&self) -> Vec<RemoteHostTarget> {
        let mut targets: Vec<RemoteHostTarget> = Vec::new();
        for host in crate::config::remote_hosts() {
            let Some(ssh_target) = host.ssh_target else {
                continue;
            };
            if targets.iter().any(|known| known.ssh_target == ssh_target) {
                continue;
            }
            targets.push(RemoteHostTarget {
                host: host.name,
                ssh_target,
            });
        }
        for ssh_target in live_remote_ssh_targets() {
            if targets.iter().any(|known| known.ssh_target == ssh_target) {
                continue;
            }
            targets.push(RemoteHostTarget {
                host: ssh_target.clone(),
                ssh_target,
            });
        }
        targets
    }
}

/// ssh targets of live remote tmux panes, published from the GTK thread by
/// [`build_live_agent_bindings`] and read by the off-thread probe.
fn live_remote_ssh_target_store() -> &'static Mutex<Vec<String>> {
    static TARGETS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    TARGETS.get_or_init(|| Mutex::new(Vec::new()))
}

fn live_remote_ssh_targets() -> Vec<String> {
    live_remote_ssh_target_store()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Publish the ssh targets worth probing, from the GTK thread.
///
/// The inventory is deliberately a little wider than "hosts of live remote
/// panes": a **detached** remote tmux session is taarof presence on that host
/// too — it is the case where finding a resumable agent matters most — and the
/// probe is bounded per host, so including them costs one more ssh round in the
/// window and nothing else.
fn publish_live_remote_ssh_targets(state: &AppState) {
    let mut targets: Vec<String> = Vec::new();
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            for leaf in tab.panes.leaves() {
                let Some(ssh_target) = leaf
                    .tmux_backing
                    .as_ref()
                    .and_then(|backing| backing.target.ssh_target_string())
                else {
                    continue;
                };
                if !targets.contains(&ssh_target) {
                    targets.push(ssh_target);
                }
            }
        }
    }
    for detached in &state.detached_sessions {
        let Some(ssh_target) = detached.target.ssh_target_string() else {
            continue;
        };
        if !targets.contains(&ssh_target) {
            targets.push(ssh_target);
        }
    }
    *live_remote_ssh_target_store()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = targets;
}

/// Runs the enumeration argv with a hard deadline, reaping the child on every
/// path. Environment scrubbing follows the same seam pane spawns use.
struct SshRemoteCommandRunner;

impl RemoteCommandRunner for SshRemoteCommandRunner {
    fn run(&self, argv: &[String]) -> Result<String, String> {
        let Some((program, args)) = argv.split_first() else {
            return Err("remote enumeration argv was empty".to_string());
        };
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        crate::child_env::prepare_child_command(&mut command, &[]);
        // The bounded wait below owns and reaps this child on every path.
        #[allow(clippy::disallowed_methods)]
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to spawn remote enumeration: {error}"))?;

        let stdout = child.stdout.take().map(read_remote_pipe);
        let stderr = child.stderr.take().map(read_remote_pipe);
        let deadline = Instant::now() + REMOTE_SCAN_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("remote enumeration wait failed: {error}"));
                }
            }
        };

        let stdout = stdout
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default();
        let stderr = stderr
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default();
        let Some(status) = status else {
            return Err(format!(
                "remote enumeration timed out after {}s",
                REMOTE_SCAN_TIMEOUT.as_secs()
            ));
        };
        if !status.success() {
            let detail = stderr.trim();
            let detail = if detail.is_empty() {
                "no stderr".to_string()
            } else {
                detail.chars().take(200).collect()
            };
            return Err(format!("remote enumeration failed: {detail}"));
        }
        Ok(stdout)
    }
}

fn read_remote_pipe<R>(mut pipe: R) -> std::thread::JoinHandle<String>
where
    R: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        // Bounded at the read, not after it: a host that streams without end
        // must not be able to grow this buffer without end.
        let mut bounded = std::io::Read::take(&mut pipe, REMOTE_MAX_OUTPUT_BYTES as u64);
        if std::io::Read::read_to_end(&mut bounded, &mut bytes).is_err() {
            return String::new();
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

pub fn build_live_agent_bindings(state: &AppState) -> Vec<LiveAgentBinding> {
    publish_live_remote_ssh_targets(state);
    let mut bindings = Vec::new();
    let process_truth_fresh = crate::runtime_probe::runtime_process_truth_is_fresh(state);
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            if !tab.agent_running {
                continue;
            }
            // One binding per running pane agent so multi-agent tabs expose
            // every session, not only the tab-level primary.
            let pane_agents: Vec<(u32, crate::agents::AgentStatus)> = if process_truth_fresh {
                state
                    .runtime_probe
                    .as_ref()
                    .map(|snapshot| {
                        tab.panes
                            .leaves()
                            .into_iter()
                            .filter_map(|leaf| {
                                snapshot
                                    .pane_agents
                                    .get(&(tab.id, leaf.pane_id))
                                    .filter(|status| status.running)
                                    .map(|status| (leaf.pane_id, status.clone()))
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            if pane_agents.is_empty() {
                let Some(agent) = tab.agent_name.as_deref().map(normalize_agent_name) else {
                    continue;
                };
                let pane_id = tab.agent_pane_id.unwrap_or(tab.focused_pane_id);
                bindings.push(LiveAgentBinding {
                    agent,
                    session_id: tab.agent_session_id.clone(),
                    cwd: live_pane_cwd(tab, workspace, pane_id),
                    workspace_id: workspace.id,
                    workspace_name: workspace.name.clone(),
                    tab_id: tab.id,
                    tab_name: tab.name.clone(),
                    pane_id,
                });
                continue;
            }

            for (pane_id, status) in pane_agents {
                let Some(agent) = status.agent_name.as_deref().map(normalize_agent_name) else {
                    continue;
                };
                bindings.push(LiveAgentBinding {
                    agent,
                    session_id: status.session_id.clone(),
                    cwd: live_pane_cwd(tab, workspace, pane_id),
                    workspace_id: workspace.id,
                    workspace_name: workspace.name.clone(),
                    tab_id: tab.id,
                    tab_name: tab.name.clone(),
                    pane_id,
                });
            }
        }
    }
    bindings
}

fn live_pane_cwd(
    tab: &Tab,
    workspace: &crate::workspace::Workspace,
    preferred_pane: u32,
) -> Option<String> {
    tab.panes
        .leaves()
        .into_iter()
        .find(|leaf| leaf.pane_id == preferred_pane)
        .and_then(|leaf| leaf.location_state.cwd.clone())
        .or_else(|| {
            tab.panes
                .leaves()
                .into_iter()
                .find_map(|leaf| leaf.location_state.cwd.clone())
        })
        .or_else(|| tab.discovery_cwd.clone())
        .or_else(|| workspace.working_tree_path.clone())
        .or_else(|| workspace.repo_root.clone())
}

fn finalize_snapshot(
    discovery: AgentSessionDiscovery,
    live_bindings: &[LiveAgentBinding],
) -> AgentSessionsSnapshot {
    let live_by_session: HashMap<(String, String), LiveAgentBinding> = live_bindings
        .iter()
        .filter_map(|binding| {
            binding
                .session_id
                .as_ref()
                .map(|session_id| ((binding.agent.clone(), session_id.clone()), binding.clone()))
        })
        .collect();
    let unique_live_by_cwd: HashMap<(String, String), LiveAgentBinding> =
        unique_live_bindings_by_cwd(live_bindings);

    let mut sessions = discovery.sessions;
    for session in &mut sessions {
        // Live bindings describe local panes only; a remote record must never
        // borrow one just because it shares an agent and a cwd string.
        let matched = if session.host.is_some() {
            None
        } else {
            live_by_session
                .get(&(session.agent.clone(), session.session_id.clone()))
                .cloned()
                .or_else(|| {
                    unique_live_by_cwd
                        .get(&(session.agent.clone(), session.cwd.clone()))
                        .cloned()
                })
        };
        session.status = if matched.is_some() {
            "active".to_string()
        } else {
            "recent".to_string()
        };
        session.live_binding = matched;
    }
    sessions.sort_by_key(|session| {
        (
            Reverse(session.status == "active"),
            Reverse(session.updated_at_unix_ms),
        )
    });

    AgentSessionsSnapshot {
        schema: "taarof.agent-sessions.v1",
        generated_at_unix_ms: unix_time_ms(),
        providers: discovery.providers,
        sessions,
        remote_hosts: discovery.remote_hosts,
    }
}

fn unique_live_bindings_by_cwd(
    live_bindings: &[LiveAgentBinding],
) -> HashMap<(String, String), LiveAgentBinding> {
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    for binding in live_bindings {
        let Some(cwd) = binding.cwd.as_ref() else {
            continue;
        };
        *counts
            .entry((binding.agent.clone(), cwd.clone()))
            .or_insert(0) += 1;
    }

    live_bindings
        .iter()
        .filter_map(|binding| {
            let cwd = binding.cwd.as_ref()?;
            let key = (binding.agent.clone(), cwd.clone());
            (counts.get(&key) == Some(&1)).then(|| (key, binding.clone()))
        })
        .collect()
}

fn internal_scan_error(message: &str) -> AgentSessionDiscovery {
    AgentSessionDiscovery {
        providers: vec![AgentSessionProviderStatus {
            name: "scan".to_string(),
            ok: false,
            history_available: false,
            warning: None,
            error: Some(format!("agent session scan failed: {message}")),
            session_count: 0,
        }],
        sessions: Vec::new(),
        remote_hosts: Vec::new(),
    }
}

fn discover_claude_sessions(
    root: Option<&Path>,
    limit: usize,
) -> (AgentSessionProviderStatus, Vec<AgentSessionRecord>) {
    discover_jsonl_provider(root, "claude", limit, parse_claude_session)
}

fn discover_codex_sessions(
    root: Option<&Path>,
    limit: usize,
) -> (AgentSessionProviderStatus, Vec<AgentSessionRecord>) {
    discover_jsonl_provider(root, "codex", limit, parse_codex_session)
}

fn discover_pi_sessions(
    sessions_root: Option<&Path>,
    session_map_path: Option<&Path>,
    limit: usize,
) -> (AgentSessionProviderStatus, Vec<AgentSessionRecord>) {
    let mut paths = Vec::new();
    if let Some(session_map_path) = session_map_path {
        match read_pi_session_map(session_map_path) {
            Ok(mapped_paths) => {
                let mut seen = HashSet::new();
                for path in mapped_paths {
                    if seen.insert(path.clone()) {
                        paths.push(path);
                    }
                }
            }
            Err(error) if error != "pi session map missing" => {
                return (
                    AgentSessionProviderStatus {
                        name: "pi".to_string(),
                        ok: false,
                        history_available: false,
                        warning: None,
                        error: Some(error),
                        session_count: 0,
                    },
                    Vec::new(),
                );
            }
            Err(_) => {}
        }
    }

    if paths.is_empty() {
        if let Some(root) = sessions_root {
            paths = collect_jsonl_candidates(root)
                .into_iter()
                .map(|candidate| candidate.path)
                .take(limit)
                .collect();
        }
    }

    if paths.is_empty() {
        return (
            AgentSessionProviderStatus {
                name: "pi".to_string(),
                ok: true,
                history_available: false,
                warning: Some("No local Pi session store was found.".to_string()),
                error: None,
                session_count: 0,
            },
            Vec::new(),
        );
    }

    let mut sessions = Vec::new();
    for path in paths.into_iter().take(limit) {
        let modified_at_unix_ms = file_modified_unix_ms(&path).unwrap_or_else(unix_time_ms);
        if let Ok(lines) = read_jsonl_sample_lines(&path) {
            if let Some(session) = parse_pi_session(&path, modified_at_unix_ms, &lines) {
                sessions.push(session);
            }
        }
    }
    sessions.sort_by_key(|session| Reverse(session.updated_at_unix_ms));
    sessions.truncate(limit);
    (
        AgentSessionProviderStatus {
            name: "pi".to_string(),
            ok: true,
            history_available: true,
            warning: None,
            error: None,
            session_count: sessions.len(),
        },
        sessions,
    )
}

fn discover_kimi_sessions(
    index_path: Option<&Path>,
    sessions_root: Option<&Path>,
    limit: usize,
) -> (AgentSessionProviderStatus, Vec<AgentSessionRecord>) {
    let Some(index_path) = index_path.filter(|path| path.exists()) else {
        return (
            AgentSessionProviderStatus {
                name: "kimi".to_string(),
                ok: true,
                history_available: false,
                warning: Some("No local Kimi session index was found.".to_string()),
                error: None,
                session_count: 0,
            },
            Vec::new(),
        );
    };

    let file = match File::open(index_path) {
        Ok(file) => file,
        Err(error) => {
            return (
                AgentSessionProviderStatus {
                    name: "kimi".to_string(),
                    ok: false,
                    history_available: false,
                    warning: None,
                    error: Some(format!("Could not read Kimi session index: {error}")),
                    session_count: 0,
                },
                Vec::new(),
            );
        }
    };

    let mut sessions = BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter_map(|entry| {
            let session_id = entry.get("sessionId")?.as_str()?.to_string();
            let session_dir = PathBuf::from(entry.get("sessionDir")?.as_str()?);
            let session_dir = if session_dir.is_absolute() {
                session_dir
            } else {
                sessions_root?.join(session_dir)
            };
            let cwd = entry.get("workDir")?.as_str()?.to_string();
            let updated_at_unix_ms = file_modified_unix_ms(&session_dir.join("state.json"))
                .or_else(|| file_modified_unix_ms(&session_dir))
                .unwrap_or_default();
            Some(AgentSessionRecord {
                agent: "kimi".to_string(),
                session_id: session_id.clone(),
                title: fallback_title("", "kimi", &session_id),
                cwd: cwd.clone(),
                host: None,
                repo_root: find_repo_root_string(&cwd),
                started_at_unix_ms: None,
                updated_at_unix_ms,
                status: "recent".to_string(),
                live_binding: None,
                resume_command: Some(build_resume_command("kimi", &cwd, &session_id)),
                resume_unavailable_reason: None,
            })
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| Reverse(session.updated_at_unix_ms));
    sessions.truncate(limit);

    (
        AgentSessionProviderStatus {
            name: "kimi".to_string(),
            ok: true,
            history_available: true,
            warning: None,
            error: None,
            session_count: sessions.len(),
        },
        sessions,
    )
}

#[cfg(feature = "opencode-history")]
fn discover_opencode_sessions(
    db_path: Option<&Path>,
    limit: usize,
) -> (AgentSessionProviderStatus, Vec<AgentSessionRecord>) {
    let Some(db_path) = db_path.filter(|path| path.exists()) else {
        return (
            AgentSessionProviderStatus {
                name: "opencode".to_string(),
                ok: true,
                history_available: false,
                warning: Some("No local OpenCode session database was found.".to_string()),
                error: None,
                session_count: 0,
            },
            Vec::new(),
        );
    };

    let connection = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(connection) => connection,
        Err(error) => {
            return (
                AgentSessionProviderStatus {
                    name: "opencode".to_string(),
                    ok: false,
                    history_available: false,
                    warning: None,
                    error: Some(format!("Could not open OpenCode session database: {error}")),
                    session_count: 0,
                },
                Vec::new(),
            );
        }
    };

    let mut statement = match connection.prepare(
        "select id, directory, title, time_created, time_updated from session order by time_updated desc limit ?1",
    ) {
        Ok(statement) => statement,
        Err(error) => {
            return (
                AgentSessionProviderStatus {
                    name: "opencode".to_string(),
                    ok: false,
                    history_available: false,
                    warning: None,
                    error: Some(format!("Could not query OpenCode sessions: {error}")),
                    session_count: 0,
                },
                Vec::new(),
            );
        }
    };

    let rows = statement.query_map([limit as i64], |row| {
        let session_id: String = row.get(0)?;
        let cwd: String = row.get(1)?;
        let title: String = row.get(2)?;
        let started_at_unix_ms: i64 = row.get(3)?;
        let updated_at_unix_ms: i64 = row.get(4)?;
        Ok(AgentSessionRecord {
            agent: "opencode".to_string(),
            session_id: session_id.clone(),
            title: fallback_title(&title, "opencode", &session_id),
            cwd: cwd.clone(),
            host: None,
            repo_root: find_repo_root_string(&cwd),
            started_at_unix_ms: Some(started_at_unix_ms.max(0) as u64),
            updated_at_unix_ms: updated_at_unix_ms.max(0) as u64,
            status: "recent".to_string(),
            live_binding: None,
            resume_command: Some(build_resume_command("opencode", &cwd, &session_id)),
            resume_unavailable_reason: None,
        })
    });

    match rows {
        Ok(rows) => {
            let sessions: Vec<AgentSessionRecord> = rows.filter_map(Result::ok).collect();
            (
                AgentSessionProviderStatus {
                    name: "opencode".to_string(),
                    ok: true,
                    history_available: true,
                    warning: None,
                    error: None,
                    session_count: sessions.len(),
                },
                sessions,
            )
        }
        Err(error) => (
            AgentSessionProviderStatus {
                name: "opencode".to_string(),
                ok: false,
                history_available: false,
                warning: None,
                error: Some(format!("Could not read OpenCode sessions: {error}")),
                session_count: 0,
            },
            Vec::new(),
        ),
    }
}

#[cfg(not(feature = "opencode-history"))]
fn discover_opencode_sessions(
    _db_path: Option<&Path>,
    _limit: usize,
) -> (AgentSessionProviderStatus, Vec<AgentSessionRecord>) {
    (
        AgentSessionProviderStatus {
            name: "opencode".to_string(),
            ok: true,
            history_available: false,
            warning: Some("OpenCode history support was not compiled into this build.".to_string()),
            error: None,
            session_count: 0,
        },
        Vec::new(),
    )
}

fn discover_jsonl_provider(
    root: Option<&Path>,
    provider: &str,
    limit: usize,
    parser: fn(&Path, u64, &[Value]) -> Option<AgentSessionRecord>,
) -> (AgentSessionProviderStatus, Vec<AgentSessionRecord>) {
    let Some(root) = root.filter(|path| path.exists()) else {
        return (
            AgentSessionProviderStatus {
                name: provider.to_string(),
                ok: true,
                history_available: false,
                warning: Some(format!("No local {} session store was found.", provider)),
                error: None,
                session_count: 0,
            },
            Vec::new(),
        );
    };

    let candidates = collect_jsonl_candidates(root);
    if candidates.is_empty() {
        return (
            AgentSessionProviderStatus {
                name: provider.to_string(),
                ok: true,
                history_available: true,
                warning: None,
                error: None,
                session_count: 0,
            },
            Vec::new(),
        );
    }

    let mut sessions = Vec::new();
    for candidate in candidates.into_iter().take(limit) {
        let Ok(lines) = read_jsonl_sample_lines(&candidate.path) else {
            continue;
        };
        if let Some(session) = parser(&candidate.path, candidate.modified_at_unix_ms, &lines) {
            sessions.push(session);
        }
    }
    sessions.sort_by_key(|session| Reverse(session.updated_at_unix_ms));
    sessions.truncate(limit);
    (
        AgentSessionProviderStatus {
            name: provider.to_string(),
            ok: true,
            history_available: true,
            warning: None,
            error: None,
            session_count: sessions.len(),
        },
        sessions,
    )
}

fn parse_claude_session(
    _path: &Path,
    modified_at_unix_ms: u64,
    lines: &[Value],
) -> Option<AgentSessionRecord> {
    let session_id = first_string(lines, &["sessionId"])?;
    let cwd = first_string(lines, &["cwd"])?;
    let title =
        first_user_message(lines).unwrap_or_else(|| fallback_title("", "claude", &session_id));
    Some(AgentSessionRecord {
        agent: "claude".to_string(),
        session_id: session_id.clone(),
        title,
        cwd: cwd.clone(),
        host: None,
        repo_root: find_repo_root_string(&cwd),
        started_at_unix_ms: Some(modified_at_unix_ms),
        updated_at_unix_ms: modified_at_unix_ms,
        status: "recent".to_string(),
        live_binding: None,
        resume_command: Some(build_resume_command("claude", &cwd, &session_id)),
        resume_unavailable_reason: None,
    })
}

fn parse_codex_session(
    _path: &Path,
    modified_at_unix_ms: u64,
    lines: &[Value],
) -> Option<AgentSessionRecord> {
    let meta = lines
        .iter()
        .find(|value| value.get("type").and_then(Value::as_str) == Some("session_meta"))?;
    let session_id = meta
        .pointer("/payload/id")
        .and_then(Value::as_str)?
        .to_string();
    let cwd = meta
        .pointer("/payload/cwd")
        .and_then(Value::as_str)?
        .to_string();
    let title = lines
        .iter()
        .find_map(codex_user_message)
        .unwrap_or_else(|| fallback_title("", "codex", &session_id));
    Some(AgentSessionRecord {
        agent: "codex".to_string(),
        session_id: session_id.clone(),
        title,
        cwd: cwd.clone(),
        host: None,
        repo_root: find_repo_root_string(&cwd),
        started_at_unix_ms: Some(modified_at_unix_ms),
        updated_at_unix_ms: modified_at_unix_ms,
        status: "recent".to_string(),
        live_binding: None,
        resume_command: Some(build_resume_command("codex", &cwd, &session_id)),
        resume_unavailable_reason: None,
    })
}

fn parse_pi_session(
    path: &Path,
    modified_at_unix_ms: u64,
    lines: &[Value],
) -> Option<AgentSessionRecord> {
    let session = lines
        .iter()
        .find(|value| value.get("type").and_then(Value::as_str) == Some("session"))?;
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| session_id_from_filename(path))?;
    let cwd = session.get("cwd").and_then(Value::as_str)?.to_string();
    let title = pi_user_message(lines).unwrap_or_else(|| fallback_title("", "pi", &session_id));
    Some(AgentSessionRecord {
        agent: "pi".to_string(),
        session_id: session_id.clone(),
        title,
        cwd: cwd.clone(),
        host: None,
        repo_root: find_repo_root_string(&cwd),
        started_at_unix_ms: Some(modified_at_unix_ms),
        updated_at_unix_ms: modified_at_unix_ms,
        status: "recent".to_string(),
        live_binding: None,
        resume_command: Some(build_resume_command("pi", &cwd, &session_id)),
        resume_unavailable_reason: None,
    })
}

fn read_pi_session_map(path: &Path) -> Result<Vec<PathBuf>, String> {
    if !path.exists() {
        return Err("pi session map missing".to_string());
    }
    let content = fs::read_to_string(path)
        .map_err(|error| format!("Could not read Pi session map: {error}"))?;
    let parsed: Value = serde_json::from_str(&content)
        .map_err(|error| format!("Could not parse Pi session map: {error}"))?;
    let Some(sessions) = parsed.get("sessions").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    Ok(sessions
        .values()
        .filter_map(|entry| entry.get("sessionFile").and_then(Value::as_str))
        .map(PathBuf::from)
        .collect())
}

fn collect_jsonl_candidates(root: &Path) -> Vec<FileCandidate> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if file_type.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            {
                let modified_at_unix_ms = entry
                    .metadata()
                    .ok()
                    .and_then(|metadata| metadata_modified_unix_ms(&metadata))
                    .unwrap_or_default();
                files.push(FileCandidate {
                    path,
                    modified_at_unix_ms,
                });
            }
        }
    }
    files.sort_by_key(|candidate| Reverse(candidate.modified_at_unix_ms));
    files.truncate(MAX_FILE_CANDIDATES);
    files
}

fn read_jsonl_sample_lines(path: &Path) -> Result<Vec<Value>, String> {
    let file =
        File::open(path).map_err(|error| format!("Could not open {}: {error}", path.display()))?;
    let reader = BufReader::new(file);
    Ok(reader
        .lines()
        .take(MAX_SAMPLE_LINES)
        .filter_map(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .collect())
}

fn first_string(lines: &[Value], keys: &[&str]) -> Option<String> {
    lines.iter().find_map(|line| {
        keys.iter()
            .find_map(|key| line.get(*key).and_then(Value::as_str))
            .map(str::to_string)
    })
}

fn first_user_message(lines: &[Value]) -> Option<String> {
    lines.iter().find_map(|line| {
        let role = line.pointer("/message/role").and_then(Value::as_str);
        if role != Some("user") && line.get("type").and_then(Value::as_str) != Some("user") {
            return None;
        }
        extract_text_from_content(
            line.get("message")
                .and_then(|message| message.get("content")),
        )
        .or_else(|| {
            line.pointer("/message/content")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
    })
}

fn codex_user_message(line: &Value) -> Option<String> {
    if line.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    if line.pointer("/payload/type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    if line.pointer("/payload/role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    extract_text_from_content(line.pointer("/payload/content"))
}

fn pi_user_message(lines: &[Value]) -> Option<String> {
    lines.iter().find_map(|line| {
        if line.get("type").and_then(Value::as_str) != Some("message") {
            return None;
        }
        if line.pointer("/message/role").and_then(Value::as_str) != Some("user") {
            return None;
        }
        extract_text_from_content(line.pointer("/message/content"))
    })
}

fn extract_text_from_content(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(text) => Some(compact_title(text)),
        Value::Array(items) => items.iter().find_map(|item| {
            item.get("text")
                .and_then(Value::as_str)
                .map(compact_title)
                .filter(|text| !text.is_empty())
        }),
        _ => None,
    }
}

fn compact_title(text: &str) -> String {
    const MAX_LEN: usize = 72;
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_LEN {
        compact
    } else {
        let prefix: String = compact.chars().take(MAX_LEN.saturating_sub(3)).collect();
        format!("{prefix}...")
    }
}

fn fallback_title(title: &str, provider: &str, session_id: &str) -> String {
    let title = title.trim();
    if !title.is_empty() {
        return compact_title(title);
    }
    format!(
        "{provider} {}",
        session_id.chars().take(8).collect::<String>()
    )
}

fn find_repo_root_string(cwd: &str) -> Option<String> {
    find_repo_root(Path::new(cwd)).map(|path| path.to_string_lossy().into_owned())
}

fn find_repo_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

pub(crate) fn build_resume_command(provider: &str, cwd: &str, session_id: &str) -> String {
    let escaped_cwd = shell_escape(cwd);
    let escaped_session_id = shell_escape(session_id);
    let command = match provider {
        "claude" => format!("claude --resume {escaped_session_id}"),
        "codex" => format!("codex resume {escaped_session_id}"),
        "pi" => format!("pi --session {escaped_session_id}"),
        "kimi" => format!("kimi --session {escaped_session_id}"),
        "copilot" => format!("copilot --resume={escaped_session_id}"),
        "opencode" => format!("opencode --session {escaped_session_id}"),
        other => format!("{} {escaped_session_id}", shell_escape(other)),
    };
    format!("cd {escaped_cwd} && {command}")
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '-' | '_' | ':'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn normalize_agent_name(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "claude code" | "claude-code" => "claude".to_string(),
        "codex-cli" => "codex".to_string(),
        "copilot-cli" | "copilot cli" => "copilot".to_string(),
        "opencode" | "open code" => "opencode".to_string(),
        "pii" | "pi" => "pi".to_string(),
        other => other.to_string(),
    }
}

pub(crate) fn most_recent_discovered_session<'a>(
    discovery: &'a AgentSessionDiscovery,
    agent: &str,
    cwd: &str,
) -> Option<&'a AgentSessionRecord> {
    let agent = normalize_agent_name(agent);
    discovery
        .sessions
        .iter()
        // Callers resolve a *local* pane's session id. A remote host running
        // the same agent under the same repo path is a routine collision, and
        // binding a local pane to a remote session id would be silently wrong.
        .filter(|record| record.host.is_none())
        .filter(|record| normalize_agent_name(&record.agent) == agent && record.cwd == cwd)
        .max_by_key(|record| record.updated_at_unix_ms)
}

fn session_id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    stem.rsplit_once('_')
        .map(|(_, session_id)| session_id.to_string())
        .or_else(|| Some(stem.to_string()))
}

fn file_modified_unix_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata_modified_unix_ms(&metadata))
}

fn metadata_modified_unix_ms(metadata: &fs::Metadata) -> Option<u64> {
    metadata.modified().ok().and_then(system_time_to_unix_ms)
}

fn system_time_to_unix_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

fn unix_time_ms() -> u64 {
    system_time_to_unix_ms(SystemTime::now()).unwrap_or_default()
}

#[derive(Clone, Debug)]
struct FileCandidate {
    path: PathBuf,
    modified_at_unix_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct FixedScanner {
        discovery: AgentSessionDiscovery,
    }

    impl AgentSessionScanner for FixedScanner {
        fn scan(&self) -> AgentSessionDiscovery {
            self.discovery.clone()
        }
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "taarof-agent-sessions-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    #[test]
    fn catalog_marks_matching_live_session_active() {
        let discovery = AgentSessionDiscovery {
            providers: vec![AgentSessionProviderStatus {
                name: "codex".to_string(),
                ok: true,
                history_available: true,
                warning: None,
                error: None,
                session_count: 1,
            }],
            sessions: vec![AgentSessionRecord {
                agent: "codex".to_string(),
                session_id: "session-123".to_string(),
                title: "Fix bug".to_string(),
                cwd: "/repo".to_string(),
                host: None,
                repo_root: Some("/repo".to_string()),
                started_at_unix_ms: Some(1),
                updated_at_unix_ms: 2,
                status: "recent".to_string(),
                live_binding: None,
                resume_command: Some("codex resume session-123".to_string()),
                resume_unavailable_reason: None,
            }],
            remote_hosts: Vec::new(),
        };
        let catalog = AgentSessionCatalog::with_scanner(Arc::new(FixedScanner { discovery }));
        let snapshot = catalog.snapshot_blocking(vec![LiveAgentBinding {
            agent: "codex".to_string(),
            session_id: Some("session-123".to_string()),
            cwd: Some("/repo".to_string()),
            workspace_id: 7,
            workspace_name: "repo".to_string(),
            tab_id: 8,
            tab_name: "Codex".to_string(),
            pane_id: 0,
        }]);

        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(snapshot.sessions[0].status, "active");
        assert_eq!(
            snapshot.sessions[0]
                .live_binding
                .as_ref()
                .expect("live binding should exist")
                .workspace_id,
            7
        );
    }

    #[test]
    fn codex_discovery_reads_session_meta() {
        let dir = unique_temp_dir("codex");
        let session_path = dir.join("sessions/2026/05/20");
        fs::create_dir_all(&session_path).expect("codex session dir should exist");
        let file_path = session_path.join("rollout-2026-05-20T00-00-00-session-123.jsonl");
        fs::write(
            &file_path,
            "{\"timestamp\":\"2026-05-20T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"session-123\",\"cwd\":\"/tmp/project\"}}\n{\"timestamp\":\"2026-05-20T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Implement issue 133\"}]}}\n",
        )
        .expect("codex fixture should write");

        let (status, sessions) = discover_codex_sessions(Some(dir.join("sessions").as_path()), 10);
        assert!(status.ok);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "session-123");
        assert_eq!(sessions[0].cwd, "/tmp/project");
        assert_eq!(sessions[0].title, "Implement issue 133");
    }

    #[test]
    fn pi_discovery_prefers_session_map() {
        let dir = unique_temp_dir("pi");
        let sessions_dir = dir.join(".pi/agent/sessions/workspace");
        fs::create_dir_all(&sessions_dir).expect("pi session dir should exist");
        let session_file = sessions_dir.join("2026-05-20T00-00-00-000Z_pi-session.jsonl");
        fs::write(
            &session_file,
            "{\"type\":\"session\",\"id\":\"pi-session\",\"timestamp\":\"2026-05-20T00:00:00Z\",\"cwd\":\"/tmp/pi\"}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"Resume deploy work\"}]}}\n",
        )
        .expect("pi fixture should write");

        let session_map_path = dir.join(".pi/pi-acp");
        fs::create_dir_all(&session_map_path).expect("pi map dir should exist");
        fs::write(
            session_map_path.join("session-map.json"),
            format!(
                "{{\"version\":1,\"sessions\":{{\"pi-session\":{{\"sessionId\":\"pi-session\",\"cwd\":\"/tmp/pi\",\"sessionFile\":\"{}\",\"updatedAt\":\"2026-05-20T00:00:01Z\"}}}}}}",
                session_file.display()
            ),
        )
        .expect("pi session map should write");

        let (status, sessions) = discover_pi_sessions(
            Some(dir.join(".pi/agent/sessions").as_path()),
            Some(dir.join(".pi/pi-acp/session-map.json").as_path()),
            10,
        );
        assert!(status.ok);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "pi-session");
        assert_eq!(sessions[0].title, "Resume deploy work");
    }

    #[test]
    fn kimi_discovery_reads_index_and_builds_verified_resume_command() {
        let dir = unique_temp_dir("kimi");
        let sessions_dir = dir.join(".kimi-code/sessions");
        let session_dir = sessions_dir.join("kimi-session");
        fs::create_dir_all(&session_dir).expect("kimi session dir should exist");
        fs::write(session_dir.join("state.json"), "{}").expect("kimi state should write");
        let index_path = dir.join(".kimi-code/session_index.jsonl");
        fs::write(
            &index_path,
            format!(
                "{{\"sessionId\":\"kimi-session\",\"sessionDir\":\"{}\",\"workDir\":\"/tmp/kimi project\"}}\n",
                session_dir.display()
            ),
        )
        .expect("kimi index should write");

        let (status, sessions) = discover_kimi_sessions(Some(&index_path), Some(&sessions_dir), 10);
        assert!(status.ok);
        assert!(status.history_available);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "kimi-session");
        assert_eq!(sessions[0].cwd, "/tmp/kimi project");
        assert_eq!(
            sessions[0].resume_command.as_deref(),
            Some("cd '/tmp/kimi project' && kimi --session kimi-session")
        );
        assert!(sessions[0].resume_unavailable_reason.is_none());
    }

    #[test]
    fn most_recent_discovered_session_matches_normalized_agent_and_exact_cwd() {
        let record =
            |agent: &str, session_id: &str, cwd: &str, updated_at_unix_ms| AgentSessionRecord {
                agent: agent.into(),
                session_id: session_id.into(),
                title: "session".into(),
                cwd: cwd.into(),
                host: None,
                repo_root: None,
                started_at_unix_ms: None,
                updated_at_unix_ms,
                status: "recent".into(),
                live_binding: None,
                resume_command: None,
                resume_unavailable_reason: None,
            };
        let remote = |session_id: &str, cwd: &str, updated_at_unix_ms| AgentSessionRecord {
            host: Some("gpu-box".into()),
            ..record("codex", session_id, cwd, updated_at_unix_ms)
        };
        let discovery = AgentSessionDiscovery {
            providers: Vec::new(),
            sessions: vec![
                record("codex", "old", "/repo", 1),
                record("codex", "wrong-cwd", "/other", 3),
                record("codex", "new", "/repo", 2),
                // Same agent, same repo path, different host — the routine
                // collision that must never bind to a local pane.
                remote("remote-newest", "/repo", 99),
            ],
            remote_hosts: Vec::new(),
        };

        assert_eq!(
            most_recent_discovered_session(&discovery, "codex-cli", "/repo")
                .map(|session| session.session_id.as_str()),
            Some("new"),
            "a remote record must never resolve a local pane's session id"
        );
    }

    #[test]
    fn kimi_discovery_missing_index_is_degraded_not_failed() {
        let dir = unique_temp_dir("kimi-missing");
        let (status, sessions) = discover_kimi_sessions(
            Some(&dir.join("missing.jsonl")),
            Some(&dir.join("sessions")),
            10,
        );
        assert!(status.ok);
        assert!(!status.history_available);
        assert!(status.warning.is_some());
        assert!(status.error.is_none());
        assert!(sessions.is_empty());
    }

    #[cfg(feature = "opencode-history")]
    #[test]
    fn opencode_discovery_reads_sqlite_rows() {
        let dir = unique_temp_dir("opencode");
        let db_path = dir.join("opencode.db");
        let connection = Connection::open(&db_path).expect("sqlite db should open");
        connection
            .execute_batch(
                "create table session (
                    id text primary key,
                    project_id text not null,
                    slug text not null,
                    directory text not null,
                    title text not null,
                    version text not null default '1',
                    share_url text,
                    summary_additions integer,
                    summary_deletions integer,
                    summary_files integer,
                    summary_diffs text,
                    revert text,
                    permission text,
                    time_created integer not null,
                    time_updated integer not null,
                    time_compacting integer,
                    time_archived integer,
                    workspace_id text,
                    path text,
                    agent text,
                    model text,
                    cost real default 0 not null,
                    tokens_input integer default 0 not null,
                    tokens_output integer default 0 not null,
                    tokens_reasoning integer default 0 not null,
                    tokens_cache_read integer default 0 not null,
                    tokens_cache_write integer default 0 not null
                );
                insert into session (id, project_id, slug, directory, title, version, time_created, time_updated)
                values ('ses_123', 'proj_1', 'steady-river', '/tmp/opencode', 'Issue 133', '1', 10, 20);",
            )
            .expect("opencode schema should write");

        drop(connection);

        let (status, sessions) = discover_opencode_sessions(Some(db_path.as_path()), 10);
        assert!(status.ok);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "ses_123");
        assert_eq!(sessions[0].title, "Issue 133");
    }

    #[cfg(feature = "opencode-history")]
    #[test]
    fn opencode_discovery_reports_degraded_when_db_missing() {
        let dir = unique_temp_dir("opencode-missing");
        let db_path = dir.join("opencode.db");

        let (status, sessions) = discover_opencode_sessions(Some(db_path.as_path()), 10);

        assert!(status.ok);
        assert!(!status.history_available);
        assert!(status.warning.is_some());
        assert!(status.error.is_none());
        assert_eq!(status.session_count, 0);
        assert!(sessions.is_empty());
    }

    #[cfg(feature = "opencode-history")]
    #[test]
    fn opencode_discovery_reports_error_on_unreadable_db() {
        let dir = unique_temp_dir("opencode-unreadable");
        let db_path = dir.join("opencode.db");
        fs::write(&db_path, b"this is not sqlite").expect("junk db should write");

        let (status, sessions) = discover_opencode_sessions(Some(db_path.as_path()), 10);

        assert!(!status.ok);
        assert!(!status.history_available);
        assert!(status.warning.is_none());
        assert!(status.error.is_some());
        assert_eq!(status.session_count, 0);
        assert!(sessions.is_empty());
    }

    #[cfg(not(feature = "opencode-history"))]
    #[test]
    fn opencode_adapter_absent_reports_unavailable() {
        let (status, sessions) = discover_opencode_sessions(None, 10);

        assert!(status.ok);
        assert!(!status.history_available);
        assert_eq!(
            status.warning.as_deref(),
            Some("OpenCode history support was not compiled into this build.")
        );
        assert!(status.error.is_none());
        assert_eq!(status.session_count, 0);
        assert!(sessions.is_empty());
    }

    #[test]
    fn compact_title_truncates_on_character_boundaries() {
        let title = compact_title(&"🚀".repeat(80));

        assert!(title.ends_with("..."));
        assert_eq!(title.chars().count(), 72);
    }

    #[test]
    fn shell_escape_handles_quotes() {
        assert_eq!(
            build_resume_command("claude", "/tmp/it's-here", "abc123"),
            "cd '/tmp/it'\"'\"'s-here' && claude --resume abc123"
        );
        assert_eq!(
            build_resume_command("custom; unsafe", "/tmp", "abc123"),
            "cd /tmp && 'custom; unsafe' abc123",
            "a persisted custom agent name must remain one shell word"
        );
    }

    /// EXAMPLE-178 — remote agent-session discovery over SSH.
    ///
    /// The module path gives every test in here the shared
    /// `remote_agent_sessions` filter substring required by
    /// `.plan/conventions.md`.
    mod remote_agent_sessions {
        use super::*;
        use std::fs;
        use std::sync::Mutex;

        const CODEX_FIXTURE_LINE: &str = "{\"timestamp\":\"2026-05-20T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"remote-codex-1\",\"cwd\":\"/srv/project\"}}";
        const CLAUDE_FIXTURE_LINE: &str =
            "{\"sessionId\":\"remote-claude-1\",\"cwd\":\"/srv/it's here\",\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Ship the remote probe\"}}";

        /// Injected stand-in for the real `ssh` runner: hands back scripted
        /// results in order so no test ever touches the network.
        struct ScriptedRunner {
            results: Mutex<Vec<Result<String, String>>>,
            calls: Mutex<Vec<Vec<String>>>,
        }

        impl ScriptedRunner {
            fn new(results: Vec<Result<String, String>>) -> Self {
                Self {
                    results: Mutex::new(results),
                    calls: Mutex::new(Vec::new()),
                }
            }

            fn calls(&self) -> Vec<Vec<String>> {
                self.calls.lock().expect("calls lock").clone()
            }
        }

        impl RemoteCommandRunner for ScriptedRunner {
            fn run(&self, argv: &[String]) -> Result<String, String> {
                self.calls.lock().expect("calls lock").push(argv.to_vec());
                let mut results = self.results.lock().expect("results lock");
                if results.is_empty() {
                    return Err("scripted runner exhausted".to_string());
                }
                results.remove(0)
            }
        }

        struct FixedHosts(Vec<RemoteHostTarget>);

        impl RemoteHostInventory for FixedHosts {
            fn hosts(&self) -> Vec<RemoteHostTarget> {
                self.0.clone()
            }
        }

        fn gpu_box() -> RemoteHostTarget {
            RemoteHostTarget {
                host: "gpu-box".to_string(),
                ssh_target: "kombiz@gpu-box.ts".to_string(),
            }
        }

        fn remote_output() -> String {
            format!(
                "{REMOTE_BLOCK_HEADER} codex 1747699200.5 /tmp/user/.codex/sessions/a.jsonl\n\
                 |{CODEX_FIXTURE_LINE}\n\
                 {REMOTE_BLOCK_END}\n\
                 {REMOTE_BLOCK_HEADER} claude 1747699100.0 /tmp/user/.claude/projects/b.jsonl\n\
                 |{CLAUDE_FIXTURE_LINE}\n\
                 {REMOTE_BLOCK_END}\n"
            )
        }

        fn probe_with(
            results: Vec<Result<String, String>>,
        ) -> (Arc<RemoteSessionProbe>, Arc<ScriptedRunner>) {
            let runner = Arc::new(ScriptedRunner::new(results));
            let probe = Arc::new(RemoteSessionProbe::manual(
                Arc::clone(&runner) as Arc<dyn RemoteCommandRunner>,
                Arc::new(FixedHosts(vec![gpu_box()])),
            ));
            (probe, runner)
        }

        /// A probe that refreshes off-thread exactly as production does.
        fn background_probe_with(
            results: Vec<Result<String, String>>,
            refresh_interval: Duration,
        ) -> (Arc<RemoteSessionProbe>, Arc<ScriptedRunner>) {
            let runner = Arc::new(ScriptedRunner::new(results));
            let probe = Arc::new(RemoteSessionProbe::background(
                Arc::clone(&runner) as Arc<dyn RemoteCommandRunner>,
                Arc::new(FixedHosts(vec![gpu_box()])),
                refresh_interval,
            ));
            (probe, runner)
        }

        /// Poll until `condition` holds, or fail — never an unconditional
        /// sleep, which would either flake or waste time.
        fn wait_until(label: &str, condition: impl Fn() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if condition() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("timed out waiting for {label}");
        }

        fn local_codex_roots(label: &str) -> DiscoveryRoots {
            let dir = unique_temp_dir(label);
            let sessions = dir.join(".codex/sessions/2026/05/20");
            fs::create_dir_all(&sessions).expect("local codex dir should exist");
            fs::write(
                sessions.join("rollout-local.jsonl"),
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"local-codex-1\",\"cwd\":\"/local/project\"}}\n",
            )
            .expect("local codex fixture should write");
            DiscoveryRoots::from_home(Some(dir))
        }

        #[test]
        fn test_remote_enumeration_command_is_batchmode_and_bounded() {
            let argv = remote_enumeration_command("kombiz@gpu-box.ts");

            assert_eq!(argv.first().map(String::as_str), Some("ssh"));
            assert!(
                argv.windows(2)
                    .any(|pair| pair[0] == "-o" && pair[1] == "BatchMode=yes"),
                "remote probe must never prompt: {argv:?}"
            );
            assert!(
                argv.windows(2)
                    .any(|pair| pair[0] == "-o" && pair[1] == "ConnectTimeout=5"),
                "remote probe must bound connect time: {argv:?}"
            );
            assert!(
                argv.iter().any(|arg| arg == "kombiz@gpu-box.ts"),
                "ssh target must appear exactly as configured: {argv:?}"
            );

            let script = remote_enumeration_script();
            assert!(
                script.contains(&format!("head -n {REMOTE_MAX_FILES_PER_PROVIDER}")),
                "newest-N file bound missing"
            );
            assert!(
                script.contains(&format!("head -n {REMOTE_MAX_SCANNED_LINES}")),
                "per-file scan bound missing"
            );
            assert!(
                script.contains(&format!("maxlines={REMOTE_MAX_SAMPLE_LINES}")),
                "per-file kept-line bound missing"
            );
            assert!(
                script.contains(&format!("maxlen={REMOTE_MAX_LINE_CHARS}")),
                "per-line bound missing"
            );
            assert!(
                script.contains(&format!("maxbytes={REMOTE_MAX_BLOCK_BYTES}")),
                "per-file byte budget missing"
            );
            assert!(
                !script.contains("cut -c"),
                "record lines must be dropped whole, never truncated into invalid JSON"
            );
            for root in [
                ".claude/projects",
                ".codex/sessions",
                ".pi/agent/sessions",
                ".kimi-code/session_index.jsonl",
            ] {
                assert!(script.contains(root), "provider root {root} missing");
            }
            for forbidden in ["printenv", "env |", "rm ", "chmod", "cat ", ">>"] {
                assert!(
                    !script.contains(forbidden),
                    "remote probe must stay read-only and value-blind, found {forbidden:?}"
                );
            }
        }

        /// The script and the parser are two halves of one contract, so run
        /// the real script through a local `sh` — never ssh — against a
        /// fixture home and parse what actually comes back.
        #[test]
        fn test_remote_enumeration_script_round_trips_through_a_local_shell() {
            let home = unique_temp_dir("remote-round-trip");
            let codex = home.join(".codex/sessions/2026/05/20");
            fs::create_dir_all(&codex).expect("codex fixture dir");
            fs::write(
                codex.join("rollout-a.jsonl"),
                format!("{CODEX_FIXTURE_LINE}\n"),
            )
            .expect("codex fixture");
            let claude = home.join(".claude/projects/p");
            fs::create_dir_all(&claude).expect("claude fixture dir");
            fs::write(
                claude.join("b.jsonl"),
                // The second line tries to forge a block header from inside a
                // record file; the `|` content prefix must defuse it.
                format!("{CLAUDE_FIXTURE_LINE}\n{REMOTE_BLOCK_HEADER} codex 1 /forged\n"),
            )
            .expect("claude fixture");

            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(remote_enumeration_script())
                .env("HOME", &home)
                .output()
                .expect("local shell should run the enumeration script");
            assert!(
                output.status.success(),
                "script failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );

            let (sessions, stats) = parse_remote_enumeration_output(
                &gpu_box(),
                &String::from_utf8_lossy(&output.stdout),
            );
            assert_eq!(
                stats,
                RemoteParseStats {
                    // The forged-header line below is real content that could
                    // not be used, so it is counted rather than passed over.
                    unparsable_lines: 1,
                    ..RemoteParseStats::default()
                },
                "in-cap fixture lines must survive, and the forged line must be counted"
            );
            let ids: Vec<&str> = sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect();
            assert!(ids.contains(&"remote-codex-1"), "got {ids:?}");
            assert!(ids.contains(&"remote-claude-1"), "got {ids:?}");
            assert!(
                !ids.contains(&"forged"),
                "a record file must not be able to forge a block header"
            );
            assert!(sessions
                .iter()
                .all(|session| session.host.as_deref() == Some("gpu-box")));
            assert!(sessions
                .iter()
                .all(|session| session.updated_at_unix_ms > 0));
        }

        #[test]
        fn test_remote_record_resume_command_is_host_qualified_and_quoted() {
            assert_eq!(
                remote_resume_command("gpu-box.ts", "claude", "/srv/project", "abc123"),
                "ssh -t gpu-box.ts 'cd /srv/project && claude --resume abc123'"
            );

            let quoted =
                remote_resume_command("kombiz@gpu-box.ts", "codex", "/srv/it's here", "s 1");
            assert_eq!(
                quoted,
                "ssh -t 'kombiz@gpu-box.ts' 'cd '\"'\"'/srv/it'\"'\"'\"'\"'\"'\"'\"'\"'s here'\"'\"' && codex resume '\"'\"'s 1'\"'\"''",
                "the whole remote payload must survive as one quoted shell word"
            );

            let (probe, _runner) = probe_with(vec![Ok(remote_output())]);
            probe.refresh_blocking();
            let (_, sessions) = probe.sections();
            let claude = sessions
                .iter()
                .find(|session| session.agent == "claude")
                .expect("remote claude record should exist");
            assert_eq!(claude.host.as_deref(), Some("gpu-box"));
            assert_eq!(
                claude.resume_command.as_deref(),
                Some(
                    remote_resume_command(
                        "kombiz@gpu-box.ts",
                        "claude",
                        "/srv/it's here",
                        "remote-claude-1"
                    )
                    .as_str()
                )
            );
            assert!(
                claude.repo_root.is_none(),
                "a remote cwd must never be probed against the local filesystem"
            );
        }

        #[test]
        fn test_remote_scan_failure_marks_host_stale_without_dropping_local() {
            let (probe, runner) = probe_with(vec![
                Ok(remote_output()),
                Err("ssh: connect to host gpu-box.ts port 22: Connection refused".to_string()),
            ]);
            let scanner = DefaultAgentSessionScanner::with_roots_and_remote(
                local_codex_roots("remote-stale"),
                Some(Arc::clone(&probe)),
            );

            probe.refresh_blocking();
            let fresh = scanner.scan();
            assert_eq!(runner.calls().len(), 1, "one ssh command per host per scan");
            let fresh_host = fresh
                .remote_hosts
                .first()
                .expect("host section should exist");
            assert!(fresh_host.ok && !fresh_host.stale);
            assert_eq!(fresh_host.host, "gpu-box");
            assert!(fresh_host.error.is_none());
            assert!(fresh
                .sessions
                .iter()
                .any(|session| session.session_id == "local-codex-1"));
            assert!(fresh
                .sessions
                .iter()
                .any(|session| session.session_id == "remote-codex-1"));

            probe.refresh_blocking();
            let degraded = scanner.scan();
            let degraded_host = degraded
                .remote_hosts
                .first()
                .expect("host section should survive a failure");
            assert!(!degraded_host.ok, "a failed host must not report ok");
            assert!(degraded_host.stale, "a failed host must be marked stale");
            assert!(degraded_host
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Connection refused"));
            assert!(
                degraded
                    .sessions
                    .iter()
                    .any(|session| session.session_id == "local-codex-1"),
                "local results must survive a remote failure"
            );
            assert!(
                degraded
                    .sessions
                    .iter()
                    .any(|session| session.session_id == "remote-codex-1"),
                "the last good remote records stay, marked stale"
            );
        }

        #[test]
        fn test_remote_records_merge_additively_into_snapshot_schema() {
            let local_only = AgentSessionDiscovery {
                providers: Vec::new(),
                sessions: vec![AgentSessionRecord {
                    agent: "codex".to_string(),
                    session_id: "local-codex-1".to_string(),
                    title: "Local".to_string(),
                    cwd: "/local/project".to_string(),
                    host: None,
                    repo_root: None,
                    started_at_unix_ms: None,
                    updated_at_unix_ms: 5,
                    status: "recent".to_string(),
                    live_binding: None,
                    resume_command: Some(
                        "cd /local/project && codex resume local-codex-1".to_string(),
                    ),
                    resume_unavailable_reason: None,
                }],
                remote_hosts: Vec::new(),
            };
            let local_snapshot = AgentSessionCatalog::with_scanner(Arc::new(FixedScanner {
                discovery: local_only,
            }))
            .snapshot_blocking(Vec::new());
            let local_json =
                serde_json::to_value(&local_snapshot).expect("snapshot should serialize");
            assert_eq!(local_json["schema"], "taarof.agent-sessions.v1");
            assert!(
                local_json.get("remote_hosts").is_none(),
                "a local-only snapshot must serialize exactly as before"
            );
            assert!(
                local_json["sessions"][0].get("host").is_none(),
                "local records must not gain a host key"
            );

            let (probe, _runner) = probe_with(vec![Ok(remote_output())]);
            let scanner = DefaultAgentSessionScanner::with_roots_and_remote(
                local_codex_roots("remote-additive"),
                Some(Arc::clone(&probe)),
            );
            probe.refresh_blocking();
            let snapshot = AgentSessionCatalog::with_scanner(Arc::new(FixedScanner {
                discovery: scanner.scan(),
            }))
            .snapshot_blocking(Vec::new());
            let json = serde_json::to_value(&snapshot).expect("snapshot should serialize");

            assert_eq!(json["schema"], "taarof.agent-sessions.v1");
            let remote_hosts = json["remote_hosts"]
                .as_array()
                .expect("remote_hosts should serialize when remote hosts exist");
            assert_eq!(remote_hosts.len(), 1);
            assert_eq!(remote_hosts[0]["host"], "gpu-box");
            assert_eq!(remote_hosts[0]["ssh_target"], "kombiz@gpu-box.ts");

            let sessions = json["sessions"].as_array().expect("sessions array");
            let local = sessions
                .iter()
                .find(|session| session["session_id"] == "local-codex-1")
                .expect("local record should still be present");
            assert!(local.get("host").is_none());
            let remote = sessions
                .iter()
                .find(|session| session["session_id"] == "remote-codex-1")
                .expect("remote record should be merged in");
            assert_eq!(remote["host"], "gpu-box");
            assert!(remote["resume_command"]
                .as_str()
                .expect("remote resume command")
                .starts_with("ssh -t "));
        }

        // --- Review round 2 ---------------------------------------------

        /// `stale` has to mean degraded. The refresh interval and the freshness
        /// window are separate for exactly this reason: a host refreshed on
        /// schedule sits past the interval most of the time, and reporting that
        /// as stale would make the flag meaningless.
        #[test]
        fn test_remote_healthy_host_is_not_stale_once_the_refresh_interval_elapses() {
            let (probe, _runner) = probe_with(vec![Ok(remote_output())]);
            probe.refresh_blocking();

            assert!(
                probe.refresh_interval < probe.freshness_window,
                "a freshness window at or below the refresh interval reports every \
                 healthy host stale"
            );

            let (statuses, _) = probe.sections();
            let host = statuses.first().expect("host section");
            assert!(host.ok);
            assert!(
                !host.stale,
                "a host whose round just completed must not be stale"
            );

            // Simulate a round that finished a full refresh interval ago — the
            // steady state for a healthy host between refreshes.
            {
                let mut state = probe.lock_state();
                let (_, cache) = state.hosts.first_mut().expect("cached host");
                cache.observed_at = Some(Instant::now() - probe.refresh_interval);
            }
            let (statuses, _) = probe.sections();
            assert!(
                !statuses[0].stale,
                "a healthy host one refresh interval old must still not be stale"
            );

            // Past the freshness window it is genuinely degraded.
            {
                let mut state = probe.lock_state();
                let (_, cache) = state.hosts.first_mut().expect("cached host");
                cache.observed_at = Some(Instant::now() - probe.freshness_window);
            }
            let (statuses, _) = probe.sections();
            assert!(
                statuses[0].stale,
                "a host past the freshness window has missed several rounds"
            );
        }

        /// The very first query after process start has no remote data yet.
        /// That is a documented consequence, and it must read as "not probed",
        /// never as "this host has no agents".
        #[test]
        fn test_remote_host_before_first_round_is_stale_with_no_sessions() {
            let (probe, runner) = probe_with(vec![Ok(remote_output())]);

            let (statuses, sessions) = probe.sections();
            assert!(sessions.is_empty());
            assert_eq!(runner.calls().len(), 0, "sections() must not perform I/O");
            let host = statuses
                .first()
                .expect("an unprobed host still gets a section");
            assert_eq!(host.host, "gpu-box");
            assert!(host.stale, "no data yet is not fresh data");
            assert_eq!(host.session_count, 0);
            assert!(host.observed_at_unix_ms.is_none());
            assert!(
                host.warning
                    .as_deref()
                    .unwrap_or_default()
                    .contains("not been probed yet"),
                "an empty first result must say why"
            );
        }

        #[test]
        fn test_remote_background_refresh_runs_one_command_per_host_per_interval() {
            // A long interval: the first call refreshes, later calls are gated.
            let (probe, runner) = background_probe_with(
                vec![Ok(remote_output()), Ok(remote_output())],
                Duration::from_secs(600),
            );

            Arc::clone(&probe).refresh_if_due();
            wait_until("the first background round", || probe.completed_a_round());
            assert_eq!(runner.calls().len(), 1);

            for _ in 0..10 {
                Arc::clone(&probe).refresh_if_due();
            }
            assert_eq!(
                runner.calls().len(),
                1,
                "the interval gate must hold: one command per host per window"
            );
            assert!(!probe.refresh_in_flight());

            // Once the interval has elapsed, the next scan refreshes again.
            {
                let mut state = probe.lock_state();
                state.last_round_at = Some(Instant::now() - Duration::from_secs(601));
            }
            Arc::clone(&probe).refresh_if_due();
            wait_until("the second background round", || runner.calls().len() == 2);
            assert_eq!(runner.calls().len(), 2);
        }

        #[test]
        fn test_remote_background_refresh_keeps_at_most_one_round_in_flight() {
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();

            struct BlockingRunner {
                calls: Mutex<usize>,
                started: std::sync::mpsc::Sender<()>,
                release: Mutex<std::sync::mpsc::Receiver<()>>,
            }

            impl RemoteCommandRunner for BlockingRunner {
                fn run(&self, _argv: &[String]) -> Result<String, String> {
                    *self.calls.lock().expect("calls lock") += 1;
                    self.started.send(()).expect("started signal");
                    self.release
                        .lock()
                        .expect("release lock")
                        .recv()
                        .expect("release signal");
                    Ok(String::new())
                }
            }

            let runner = Arc::new(BlockingRunner {
                calls: Mutex::new(0),
                started: started_tx,
                release: Mutex::new(release_rx),
            });
            let probe = Arc::new(RemoteSessionProbe::background(
                Arc::clone(&runner) as Arc<dyn RemoteCommandRunner>,
                Arc::new(FixedHosts(vec![gpu_box()])),
                Duration::from_millis(0),
            ));

            Arc::clone(&probe).refresh_if_due();
            started_rx.recv().expect("the first round should start");
            assert!(probe.refresh_in_flight());

            // A zero interval means every one of these is "due"; only the
            // in-flight latch can stop a second round.
            for _ in 0..10 {
                Arc::clone(&probe).refresh_if_due();
            }
            assert_eq!(
                *runner.calls.lock().expect("calls lock"),
                1,
                "the in-flight latch must keep a second round from starting"
            );

            release_tx.send(()).expect("release the blocked round");
            wait_until("the in-flight latch to clear", || {
                !probe.refresh_in_flight()
            });
            assert_eq!(*runner.calls.lock().expect("calls lock"), 1);
        }

        /// If the latch stuck on a failed spawn, remote discovery would go
        /// quiet for the life of the process — silently.
        #[test]
        fn test_remote_refresh_clears_in_flight_latch_when_the_spawn_fails() {
            let (probe, runner) =
                background_probe_with(vec![Ok(remote_output())], Duration::from_millis(0));

            Arc::clone(&probe).refresh_if_due_with(|_| Err(()));
            assert_eq!(runner.calls().len(), 0, "no round should have run");
            assert!(
                !probe.refresh_in_flight(),
                "a failed spawn must not leave the latch set"
            );

            // The next attempt must still be able to run.
            Arc::clone(&probe).refresh_if_due();
            wait_until("a round after the failed spawn", || {
                runner.calls().len() == 1
            });
        }

        /// A record line over the per-line cap is dropped whole and counted.
        /// Truncating it would yield invalid JSON and destroy the record with
        /// no trace — which is exactly what the 2000-char cap used to do to
        /// every codex `session_meta` line (measured 18k–48k chars).
        #[test]
        fn test_remote_over_cap_record_line_is_dropped_and_counted_not_truncated() {
            let home = unique_temp_dir("remote-over-cap");
            let codex = home.join(".codex/sessions/2026/05/20");
            fs::create_dir_all(&codex).expect("codex fixture dir");

            // In-cap: a realistic codex session_meta with a large instruction
            // preamble. This is the case the old cap silently destroyed.
            let padding = "x".repeat(20_000);
            fs::write(
                codex.join("rollout-big-but-valid.jsonl"),
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"remote-codex-big\",\
                     \"cwd\":\"/srv/big\",\"instructions\":\"{padding}\"}}}}\n"
                ),
            )
            .expect("in-cap fixture");

            // Over-cap: beyond REMOTE_MAX_LINE_CHARS.
            let oversize = "y".repeat(REMOTE_MAX_LINE_CHARS + 10);
            fs::write(
                codex.join("rollout-over-cap.jsonl"),
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"remote-codex-oversize\",\
                     \"cwd\":\"/srv/oversize\",\"instructions\":\"{oversize}\"}}}}\n"
                ),
            )
            .expect("over-cap fixture");

            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(remote_enumeration_script())
                .env("HOME", &home)
                .output()
                .expect("local shell should run the enumeration script");
            assert!(output.status.success());

            let (sessions, stats) = parse_remote_enumeration_output(
                &gpu_box(),
                &String::from_utf8_lossy(&output.stdout),
            );
            let ids: Vec<&str> = sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect();

            assert!(
                ids.contains(&"remote-codex-big"),
                "a 20k-char session_meta line must survive; got {ids:?}"
            );
            assert!(
                !ids.contains(&"remote-codex-oversize"),
                "an over-cap line must not be half-parsed"
            );
            assert_eq!(
                stats.dropped_lines, 1,
                "the over-cap line must be counted, not silently discarded"
            );
            assert_eq!(
                stats.unparsable_lines, 0,
                "dropping whole lines must never produce invalid JSON"
            );
            assert!(
                stats
                    .warning()
                    .unwrap_or_default()
                    .contains("under-reported"),
                "the loss must reach the caller in words"
            );

            const {
                assert!(
                    REMOTE_MAX_BLOCK_BYTES < REMOTE_MAX_LINE_CHARS,
                    "the first-line guarantee is only meaningful while a single \
                     header line can exceed the per-file budget"
                )
            };

            // And the loss must survive onto the host section a caller reads.
            let (probe, _runner) = probe_with(vec![Ok(
                String::from_utf8_lossy(&output.stdout).into_owned()
            )]);
            probe.refresh_blocking();
            let (statuses, _) = probe.sections();
            assert_eq!(statuses[0].dropped_lines, 1);
            assert!(statuses[0].warning.is_some());
        }

        /// A real codex `session_meta` line is larger than the per-file byte
        /// budget on its own, and it is the only line carrying the id and cwd.
        /// The budget must trim the extra title lines, never the record.
        #[test]
        fn test_remote_header_line_survives_a_per_file_budget_smaller_than_itself() {
            let home = unique_temp_dir("remote-first-line");
            let codex = home.join(".codex/sessions/2026/05/20");
            fs::create_dir_all(&codex).expect("codex fixture dir");

            // Header alone exceeds the per-file budget but is within the line cap.
            let preamble = "z".repeat(REMOTE_MAX_BLOCK_BYTES + 4096);
            assert!(preamble.len() < REMOTE_MAX_LINE_CHARS);
            fs::write(
                codex.join("rollout-fat-header.jsonl"),
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"remote-codex-fat\",\
                     \"cwd\":\"/srv/fat\",\"instructions\":\"{preamble}\"}}}}\n\
                     {{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\
                     \"role\":\"user\",\"content\":[{{\"text\":\"trimmed by the budget\"}}]}}}}\n"
                ),
            )
            .expect("fat-header fixture");

            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(remote_enumeration_script())
                .env("HOME", &home)
                .output()
                .expect("local shell should run the enumeration script");
            assert!(output.status.success());

            let (sessions, stats) = parse_remote_enumeration_output(
                &gpu_box(),
                &String::from_utf8_lossy(&output.stdout),
            );
            let record = sessions
                .iter()
                .find(|session| session.session_id == "remote-codex-fat")
                .expect("the record-bearing header line must survive the budget");
            assert_eq!(record.cwd, "/srv/fat");
            assert_eq!(
                stats.truncated_files, 1,
                "the trimmed title line must be reported, not hidden"
            );
            assert_eq!(stats.dropped_lines, 0);
        }

        /// Live bindings describe local panes. A remote record sharing an agent
        /// and cwd with a live local pane — the same repo path on two hosts —
        /// must not be marked active or handed that pane.
        #[test]
        fn test_remote_record_does_not_borrow_a_matching_local_live_binding() {
            let (probe, _runner) = probe_with(vec![Ok(remote_output())]);
            probe.refresh_blocking();
            let scanner = DefaultAgentSessionScanner::with_roots_and_remote(
                local_codex_roots("remote-live-binding"),
                Some(Arc::clone(&probe)),
            );
            let discovery = scanner.scan();
            let remote_cwd = discovery
                .sessions
                .iter()
                .find(|session| session.session_id == "remote-codex-1")
                .expect("remote codex record")
                .cwd
                .clone();

            // A live local pane on the *same* agent and cwd as the remote record.
            let snapshot = finalize_snapshot(
                discovery,
                &[LiveAgentBinding {
                    agent: "codex".to_string(),
                    session_id: None,
                    cwd: Some(remote_cwd.clone()),
                    workspace_id: 3,
                    workspace_name: "local".to_string(),
                    tab_id: 4,
                    tab_name: "Codex".to_string(),
                    pane_id: 5,
                }],
            );

            let remote = snapshot
                .sessions
                .iter()
                .find(|session| session.session_id == "remote-codex-1")
                .expect("remote record should survive finalize");
            assert_eq!(remote.host.as_deref(), Some("gpu-box"));
            assert!(
                remote.live_binding.is_none(),
                "a remote record must not borrow a local pane's live binding"
            );
            assert_eq!(
                remote.status, "recent",
                "and must not be reported as active on this host"
            );
        }
    }
}
