#[cfg(feature = "opencode-history")]
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
const MAX_FILE_CANDIDATES: usize = 200;
const MAX_SAMPLE_LINES: usize = 64;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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
    /// Last user-authored message; absent when no reliable timestamp is available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_user_message_at_unix_ms: Option<u64>,
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
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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
pub struct DiscoveryRoots {
    pub claude_projects_dir: Option<PathBuf>,
    pub codex_sessions_dir: Option<PathBuf>,
    pub pi_sessions_dir: Option<PathBuf>,
    pub pi_session_map_path: Option<PathBuf>,
    pub kimi_session_index_path: Option<PathBuf>,
    pub kimi_sessions_dir: Option<PathBuf>,
    pub opencode_db_path: Option<PathBuf>,
}

impl DiscoveryRoots {
    pub fn from_home(home: Option<PathBuf>) -> Self {
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

pub fn discover_claude_sessions(
    root: Option<&Path>,
    limit: usize,
) -> (AgentSessionProviderStatus, Vec<AgentSessionRecord>) {
    discover_jsonl_provider(root, "claude", limit, parse_claude_session)
}

pub fn discover_codex_sessions(
    root: Option<&Path>,
    limit: usize,
) -> (AgentSessionProviderStatus, Vec<AgentSessionRecord>) {
    discover_jsonl_provider(root, "codex", limit, parse_codex_session)
}

pub fn discover_pi_sessions(
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
                    // Pi's ACP map can retain paths after a transcript is moved
                    // or removed. A stale reference is not a store failure.
                    if fs::metadata(&path)
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                    {
                        continue;
                    }
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
        if let Some(root) = sessions_root.filter(|root| root.exists()) {
            paths = match collect_jsonl_candidates(root) {
                Ok(paths) => paths,
                Err(error) => return provider_failure("pi", error),
            }
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
    let mut failure = None;
    for path in paths.into_iter().take(limit) {
        let modified_at_unix_ms = file_modified_unix_ms(&path).unwrap_or_else(unix_time_ms);
        let lines = match read_jsonl_sample_lines(&path) {
            Ok(lines) => lines,
            Err(error) => {
                failure = Some(error);
                continue;
            }
        };
        if let Some(mut session) = parse_pi_session(&path, modified_at_unix_ms, &lines) {
            session.last_user_message_at_unix_ms =
                crate::message_time::last_user_message(&path, "pi");
            sessions.push(session);
        } else {
            failure = Some("Pi history has no valid session metadata.".into());
        }
    }
    sessions.sort_by_key(|session| Reverse(session.updated_at_unix_ms));
    sessions.truncate(limit);
    (
        AgentSessionProviderStatus {
            name: "pi".to_string(),
            ok: failure.is_none(),
            history_available: true,
            warning: None,
            error: failure,
            session_count: sessions.len(),
        },
        sessions,
    )
}

pub fn discover_kimi_sessions(
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

    let entries = match read_bounded_jsonl(index_path, 10_000, true) {
        Ok(entries) => entries,
        Err(error) => return provider_failure("kimi", error),
    };
    let entry_count = entries.len();
    let mut sessions = entries
        .into_iter()
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
                last_user_message_at_unix_ms: None,
                status: "recent".to_string(),
                live_binding: None,
                resume_command: Some(build_resume_command("kimi", &cwd, &session_id)),
                resume_unavailable_reason: None,
            })
        })
        .collect::<Vec<_>>();
    let malformed = sessions.len() != entry_count;
    sessions.sort_by_key(|session| Reverse(session.updated_at_unix_ms));
    sessions.truncate(limit);

    (
        AgentSessionProviderStatus {
            name: "kimi".to_string(),
            ok: !malformed,
            history_available: true,
            warning: None,
            error: malformed.then(|| "Kimi index contains invalid session metadata.".into()),
            session_count: sessions.len(),
        },
        sessions,
    )
}

#[cfg(feature = "opencode-history")]
pub fn discover_opencode_sessions(
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
            last_user_message_at_unix_ms: None,
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
pub fn discover_opencode_sessions(
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

    let candidates = match collect_jsonl_candidates(root) {
        Ok(candidates) => candidates,
        Err(error) => return provider_failure(provider, error),
    };
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
    let mut failure = None;
    let mut missing_metadata = false;
    for candidate in candidates.into_iter().take(limit) {
        let lines = match read_jsonl_sample_lines(&candidate.path) {
            Ok(lines) => lines,
            Err(error) => {
                failure = Some(error);
                continue;
            }
        };
        if let Some(mut session) = parser(&candidate.path, candidate.modified_at_unix_ms, &lines) {
            session.last_user_message_at_unix_ms =
                crate::message_time::last_user_message(&candidate.path, provider);
            sessions.push(session);
        } else {
            missing_metadata = true;
        }
    }
    // A store also holds JSONL that is not a session transcript (Claude workflow
    // journals, bridge stubs). Only a store with no admissible session is degraded.
    if missing_metadata && sessions.is_empty() {
        failure.get_or_insert_with(|| "Session history has no valid session metadata.".into());
    }
    sessions.sort_by_key(|session| Reverse(session.updated_at_unix_ms));
    sessions.truncate(limit);
    (
        AgentSessionProviderStatus {
            name: provider.to_string(),
            ok: failure.is_none(),
            history_available: true,
            warning: None,
            error: failure,
            session_count: sessions.len(),
        },
        sessions,
    )
}

pub fn parse_claude_session(
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
        last_user_message_at_unix_ms: None,
        status: "recent".to_string(),
        live_binding: None,
        resume_command: Some(build_resume_command("claude", &cwd, &session_id)),
        resume_unavailable_reason: None,
    })
}

pub fn parse_codex_session(
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
        last_user_message_at_unix_ms: None,
        status: "recent".to_string(),
        live_binding: None,
        resume_command: Some(build_resume_command("codex", &cwd, &session_id)),
        resume_unavailable_reason: None,
    })
}

pub fn parse_pi_session(
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
        last_user_message_at_unix_ms: None,
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
    let mut content = Vec::new();
    File::open(path)
        .map_err(|_| "Could not read Pi session map.")?
        .take(1_048_577)
        .read_to_end(&mut content)
        .map_err(|_| "Could not read Pi session map.")?;
    if content.len() > 1_048_576 {
        return Err("Pi session map byte limit exceeded.".into());
    }
    let parsed: Value =
        serde_json::from_slice(&content).map_err(|_| "Could not parse Pi session map.")?;
    let Some(sessions) = parsed.get("sessions").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    Ok(sessions
        .values()
        .filter_map(|entry| entry.get("sessionFile").and_then(Value::as_str))
        .map(PathBuf::from)
        .collect())
}

fn collect_jsonl_candidates(root: &Path) -> Result<Vec<FileCandidate>, String> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
    let mut visited = 0;
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|_| "Session store directory is unreadable.")?;
        for entry in entries {
            visited += 1;
            if visited > 100_000 {
                return Err("Session store traversal limit exceeded.".into());
            }
            let entry = entry.map_err(|_| "Session store entry is unreadable.")?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|_| "Session store entry type is unreadable.")?;
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
    Ok(files)
}

fn provider_failure(
    provider: &str,
    error: String,
) -> (AgentSessionProviderStatus, Vec<AgentSessionRecord>) {
    (
        AgentSessionProviderStatus {
            name: provider.into(),
            ok: false,
            history_available: false,
            warning: None,
            error: Some(error),
            session_count: 0,
        },
        Vec::new(),
    )
}

fn read_jsonl_sample_lines(path: &Path) -> Result<Vec<Value>, String> {
    read_bounded_jsonl(path, MAX_SAMPLE_LINES, false)
}

fn read_bounded_jsonl(
    path: &Path,
    max_lines: usize,
    require_eof: bool,
) -> Result<Vec<Value>, String> {
    const MAX_LINE_BYTES: u64 = 1_048_576;
    const MAX_TOTAL_BYTES: usize = 16 * 1_048_576;
    let file = File::open(path).map_err(|_| "Session history is unreadable.")?;
    let mut reader = BufReader::new(file);
    let mut values = Vec::new();
    let mut total = 0;
    for _ in 0..max_lines {
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take(MAX_LINE_BYTES + 1)
            .read_until(b'\n', &mut line)
            .map_err(|_| "Session history read failed.")?;
        if count == 0 {
            return Ok(values);
        }
        total += count;
        if count as u64 > MAX_LINE_BYTES || total > MAX_TOTAL_BYTES {
            return Err("Session history byte limit exceeded.".into());
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        values.push(
            serde_json::from_slice(&line)
                .map_err(|_| "Session history contains malformed JSON.")?,
        );
    }
    if require_eof
        && !reader
            .fill_buf()
            .map_err(|_| "Session history read failed.")?
            .is_empty()
    {
        return Err("Session history record limit exceeded.".into());
    }
    Ok(values)
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

pub fn compact_title(text: &str) -> String {
    const MAX_LEN: usize = 72;
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_LEN {
        compact
    } else {
        let prefix: String = compact.chars().take(MAX_LEN.saturating_sub(3)).collect();
        format!("{prefix}...")
    }
}

pub fn fallback_title(title: &str, provider: &str, session_id: &str) -> String {
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

pub fn build_resume_command(provider: &str, cwd: &str, session_id: &str) -> String {
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

pub fn shell_escape(value: &str) -> String {
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

pub fn normalize_agent_name(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "claude code" | "claude-code" => "claude".to_string(),
        "codex-cli" => "codex".to_string(),
        "copilot-cli" | "copilot cli" => "copilot".to_string(),
        "opencode" | "open code" => "opencode".to_string(),
        "pii" | "pi" => "pi".to_string(),
        other => other.to_string(),
    }
}

pub fn most_recent_discovered_session<'a>(
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

pub fn unix_time_ms() -> u64 {
    system_time_to_unix_ms(SystemTime::now()).unwrap_or_default()
}

#[derive(Clone, Debug)]
struct FileCandidate {
    path: PathBuf,
    modified_at_unix_ms: u64,
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    #[test]
    fn malformed_and_oversized_history_degrade_only_the_provider() {
        let root = std::env::temp_dir().join(format!("agent-admission-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for content in [
            "{bad json".to_string(),
            "{}".to_string(),
            "x".repeat(1_048_577),
        ] {
            fs::write(root.join("bad.jsonl"), content).unwrap();
            let (status, sessions) = discover_codex_sessions(Some(&root), 50);
            assert!(!status.ok);
            assert!(status.error.is_some());
            assert!(sessions.is_empty());
        }
        let (status, _) = discover_codex_sessions(Some(&root.join("bad.jsonl")), 50);
        assert!(
            !status.ok,
            "a non-directory store must not be healthy empty"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn kimi_malformed_index_reports_degradation() {
        let root =
            std::env::temp_dir().join(format!("agent-kimi-admission-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let index = root.join("index.jsonl");
        fs::write(&index, "{bad json").unwrap();
        let (status, _) = discover_kimi_sessions(Some(&index), Some(&root), 50);
        assert!(!status.ok);
        fs::remove_dir_all(root).unwrap();
    }
}
