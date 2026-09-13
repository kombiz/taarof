use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::fs::{create_dir_all, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(not(test))]
use std::sync::{Mutex, OnceLock};

const DEFAULT_RECENT_RECORD_LIMIT: usize = 64;
const DEFAULT_LOG_MAX_BYTES: u64 = 256 * 1024;
const DEFAULT_ARCHIVE_COUNT: usize = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticLevel {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiagnosticRecord {
    pub ts_unix_ms: u64,
    pub level: DiagnosticLevel,
    pub category: String,
    pub source: String,
    pub action: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub details: Option<Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiagnosticCounters {
    pub total_records: u64,
    pub warning_records: u64,
    pub error_records: u64,
    pub command_failures: u64,
    pub probe_failures: u64,
    pub event_drops: u64,
    pub lifecycle_events: u64,
    pub write_failures: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiagnosticRetention {
    pub recent_record_limit: usize,
    pub log_max_bytes: u64,
    pub archive_count: usize,
}

impl Default for DiagnosticRetention {
    fn default() -> Self {
        Self {
            recent_record_limit: DEFAULT_RECENT_RECORD_LIMIT,
            log_max_bytes: DEFAULT_LOG_MAX_BYTES,
            archive_count: DEFAULT_ARCHIVE_COUNT,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiagnosticSnapshot {
    pub log_path: Option<String>,
    pub archive_path: Option<String>,
    pub retention: DiagnosticRetention,
    pub counters: DiagnosticCounters,
    pub last_failure_at_unix_ms: Option<u64>,
    pub last_event_drop_at_unix_ms: Option<u64>,
    pub recent: Vec<DiagnosticRecord>,
}

#[derive(Debug)]
pub struct DiagnosticJournal {
    log_path: Option<PathBuf>,
    archive_path: Option<PathBuf>,
    retention: DiagnosticRetention,
    counters: DiagnosticCounters,
    last_failure_at_unix_ms: Option<u64>,
    last_event_drop_at_unix_ms: Option<u64>,
    recent: VecDeque<DiagnosticRecord>,
}

impl Default for DiagnosticJournal {
    fn default() -> Self {
        let (log_path, archive_path) = default_log_paths();
        let mut journal =
            Self::new_with_paths(log_path, archive_path, DiagnosticRetention::default());
        journal.load_persisted_records();
        journal
    }
}

impl DiagnosticJournal {
    pub fn new_with_paths(
        log_path: Option<PathBuf>,
        archive_path: Option<PathBuf>,
        retention: DiagnosticRetention,
    ) -> Self {
        let recent_record_limit = retention.recent_record_limit.max(1);
        Self {
            log_path,
            archive_path,
            retention,
            counters: DiagnosticCounters::default(),
            last_failure_at_unix_ms: None,
            last_event_drop_at_unix_ms: None,
            recent: VecDeque::with_capacity(recent_record_limit),
        }
    }

    pub fn snapshot(&self) -> DiagnosticSnapshot {
        DiagnosticSnapshot {
            log_path: self
                .log_path
                .as_ref()
                .map(|path| path.display().to_string()),
            archive_path: self
                .archive_path
                .as_ref()
                .map(|path| path.display().to_string()),
            retention: self.retention.clone(),
            counters: self.counters.clone(),
            last_failure_at_unix_ms: self.last_failure_at_unix_ms,
            last_event_drop_at_unix_ms: self.last_event_drop_at_unix_ms,
            recent: self.recent.iter().cloned().collect(),
        }
    }

    pub fn record(&mut self, record: DiagnosticRecord) {
        let record = sanitize_record(record);
        self.apply_record(record.clone());
        if let Err(error) = self.persist_record(&record) {
            self.counters.write_failures += 1;
            eprintln!("taarof: diag[write-failure/journal] {error}");
        }
    }

    fn load_persisted_records(&mut self) {
        let paths = [self.archive_path.clone(), self.log_path.clone()];
        for path in paths.into_iter().flatten() {
            let Ok(mut file) = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
            else {
                continue;
            };
            if !file.metadata().is_ok_and(|metadata| metadata.is_file())
                || file
                    .set_permissions(std::fs::Permissions::from_mode(0o600))
                    .is_err()
            {
                continue;
            }
            let mut contents = String::new();
            if file.read_to_string(&mut contents).is_err() {
                continue;
            }
            for line in contents.lines() {
                let Ok(record) = serde_json::from_str::<DiagnosticRecord>(line) else {
                    continue;
                };
                self.apply_record(sanitize_record(record));
            }
        }
    }

    fn apply_record(&mut self, record: DiagnosticRecord) {
        self.counters.total_records += 1;
        match record.level {
            DiagnosticLevel::Warn => self.counters.warning_records += 1,
            DiagnosticLevel::Error => self.counters.error_records += 1,
            DiagnosticLevel::Info => {}
        }
        match record.category.as_str() {
            "command_failure" => {
                self.counters.command_failures += 1;
                self.last_failure_at_unix_ms = Some(record.ts_unix_ms);
            }
            "probe_failure" => {
                self.counters.probe_failures += 1;
                self.last_failure_at_unix_ms = Some(record.ts_unix_ms);
            }
            "event_drop" => {
                self.counters.event_drops += 1;
                self.last_failure_at_unix_ms = Some(record.ts_unix_ms);
                self.last_event_drop_at_unix_ms = Some(record.ts_unix_ms);
            }
            "lifecycle" => {
                self.counters.lifecycle_events += 1;
            }
            _ => {}
        }

        if self.recent.len() >= self.retention.recent_record_limit.max(1) {
            self.recent.pop_front();
        }
        self.recent.push_back(record);
    }

    fn persist_record(&self, record: &DiagnosticRecord) -> Result<(), String> {
        let Some(log_path) = &self.log_path else {
            return Ok(());
        };

        if let Some(parent) = log_path.parent() {
            create_dir_all(parent)
                .map_err(|error| format!("could not create diagnostics dir: {:?}", error.kind()))?;
        }

        let serialized = serde_json::to_string(record)
            .map_err(|error| format!("could not serialize diagnostic record: {error}"))?;

        // Harden an existing journal before it can become the rotated archive.
        drop(
            open_private_log(log_path)
                .map_err(|error| format!("could not secure diagnostics log: {:?}", error.kind()))?,
        );
        self.rotate_if_needed(log_path, serialized.len() as u64 + 1)?;
        let mut file = open_private_log(log_path)
            .map_err(|error| format!("could not open diagnostics log: {:?}", error.kind()))?;
        writeln!(file, "{serialized}")
            .map_err(|error| format!("could not write diagnostics log: {:?}", error.kind()))?;
        Ok(())
    }

    fn rotate_if_needed(&self, log_path: &Path, incoming_bytes: u64) -> Result<(), String> {
        let Ok(metadata) = std::fs::metadata(log_path) else {
            return Ok(());
        };
        if metadata.len().saturating_add(incoming_bytes) <= self.retention.log_max_bytes {
            return Ok(());
        }

        let Some(archive_path) = &self.archive_path else {
            let _ = std::fs::remove_file(log_path);
            return Ok(());
        };

        let _ = std::fs::remove_file(archive_path);
        std::fs::rename(log_path, archive_path)
            .map_err(|error| format!("could not rotate diagnostics log: {:?}", error.kind()))?;
        Ok(())
    }
}

// Diagnostics share the history scalar policy, but also retain bounded safe
// operational details (for example asset lookup paths) needed by local support.
// Raw argv, stderr and arbitrary objects never belong in an observation sink.
pub(crate) fn safe_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 400
        && !value.chars().any(char::is_control)
        && !crate::work_reporting::looks_like_credential(value)
        && !has_quoted_credential_key(value)
        && !crate::work_reporting::looks_like_uri_credential(value)
        && !crate::work_reporting::looks_like_terminal_output(value)
        && !crate::work_reporting::contains_high_entropy_token(value))
    .then(|| value.to_string())
}

// The ordinary key=value/key:value matcher must also handle structured error
// strings. Decode JSON string tokens so quoted and Unicode-escaped key spellings
// cannot hide a sensitive field. Scalars reaching here are bounded to 400 bytes.
fn has_quoted_credential_key(value: &str) -> bool {
    for (offset, ch) in value.char_indices() {
        if ch != '"' {
            continue;
        }
        let mut tokens = serde_json::Deserializer::from_str(&value[offset..]).into_iter::<String>();
        let Some(Ok(key)) = tokens.next() else {
            continue;
        };
        if value[offset + tokens.byte_offset()..]
            .trim_start()
            .starts_with(':')
            && crate::work_reporting::looks_like_credential(&format!("{key}=value"))
        {
            return true;
        }
    }
    false
}

fn safe_identifier(value: &str, fallback: &str) -> String {
    safe_text(value)
        .filter(|value| {
            value.len() <= 64
                && value.bytes().all(|ch| {
                    ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-' | b'.' | b'/' | b':')
                })
        })
        .unwrap_or_else(|| fallback.to_string())
}

fn failure_kind(text: &str) -> &'static str {
    let lower = text
        .chars()
        .take(1024)
        .collect::<String>()
        .to_ascii_lowercase();
    for (needle, kind) in [
        ("permission denied", "permission_denied"),
        ("no such file", "not_found"),
        ("not found", "not_found"),
        ("timed out", "timed_out"),
        ("timeout", "timed_out"),
        ("connection refused", "connection_refused"),
        ("authentication failed", "authentication_failed"),
    ] {
        if lower.contains(needle) {
            return kind;
        }
    }
    "unclassified"
}

pub(crate) fn sanitize_record(mut record: DiagnosticRecord) -> DiagnosticRecord {
    let mut details = serde_json::Map::new();
    let allowed: &[&str] = match record.category.as_str() {
        "command_failure" => &["exit_code", "signal", "status", "timeout_secs"],
        "probe_failure" => &["state", "attempt"],
        "event_drop" => &[
            "dropped_total",
            "drops_in_window",
            "rate_window_ms",
            "abnormal_rate",
            "capacity",
            "dropped_seq",
            "oldest_retained_seq",
        ],
        "history_backpressure" => &["dropped_total", "queue_capacity"],
        "lifecycle" => &["pid", "session_name"],
        "http_asset_lookup" => &["attempted_paths"],
        _ => &[],
    };
    if let Some(input) = record.details.as_ref().and_then(Value::as_object) {
        for key in allowed
            .iter()
            .copied()
            .chain(["failure_kind", "message_redacted"])
        {
            let Some(value) = input.get(key) else {
                continue;
            };
            let safe = match (key, value) {
                ("attempted_paths", Value::Array(paths)) => Some(Value::Array(
                    paths
                        .iter()
                        .take(16)
                        .filter_map(Value::as_str)
                        .filter_map(safe_text)
                        .map(Value::String)
                        .collect(),
                )),
                ("state" | "status" | "session_name" | "failure_kind", Value::String(text)) => {
                    safe_text(text).map(Value::String)
                }
                ("abnormal_rate" | "message_redacted", Value::Bool(_)) => Some(value.clone()),
                (_, Value::Number(_))
                    if !matches!(
                        key,
                        "state"
                            | "status"
                            | "session_name"
                            | "failure_kind"
                            | "message_redacted"
                            | "attempted_paths"
                    ) =>
                {
                    Some(value.clone())
                }
                _ => None,
            };
            if let Some(safe) = safe {
                details.insert(key.to_string(), safe);
            }
        }
        // Preserve a bounded cause class from discarded subprocess output, never
        // the output itself. Existing safe failure_kind makes this idempotent.
        if !details.contains_key("failure_kind") {
            for key in ["error", "stderr"] {
                if let Some(text) = input.get(key).and_then(Value::as_str) {
                    let kind = failure_kind(text);
                    if kind != "unclassified" {
                        details.insert("failure_kind".to_string(), Value::String(kind.to_string()));
                        break;
                    }
                }
            }
        }
    }
    record.category = safe_identifier(&record.category, "invalid_category");
    record.source = safe_identifier(&record.source, "redacted_source");
    record.action = safe_identifier(&record.action, "redacted_action");
    record.message = safe_text(&record.message).unwrap_or_else(|| {
        details.insert("message_redacted".to_string(), Value::Bool(true));
        details
            .entry("failure_kind")
            .or_insert_with(|| Value::String(failure_kind(&record.message).to_string()));
        "Diagnostic text withheld; see source, action and failure metadata".to_string()
    });
    record.details = (!details.is_empty()).then_some(Value::Object(details));
    record
}

// This is the production admission seam. Test sinks exercise the same order
// without touching the process journal, stderr or an operator's history.
fn record_to_sinks(
    record: DiagnosticRecord,
    stderr: impl FnOnce(&DiagnosticRecord),
    journal: impl FnOnce(&DiagnosticRecord),
    history: impl FnOnce(&DiagnosticRecord),
) {
    let record = sanitize_record(record);
    stderr(&record);
    journal(&record);
    history(&record);
}

fn open_private_log(path: &Path) -> std::io::Result<std::fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "diagnostic journal is not a regular file",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn default_log_paths() -> (Option<PathBuf>, Option<PathBuf>) {
    let base_dir = dirs::state_dir()
        .or_else(dirs::data_dir)
        .map(|dir| dir.join("taarof"));
    let Some(base_dir) = base_dir else {
        return (None, None);
    };

    let file_name = match crate::instance::session_storage_key() {
        Some(session) => format!("diagnostics-{session}.jsonl"),
        None => "diagnostics.jsonl".to_string(),
    };
    let log_path = base_dir.join(file_name);
    let archive_path = log_path.with_extension("jsonl.1");
    (Some(log_path), Some(archive_path))
}

fn stderr_line(record: &DiagnosticRecord) -> String {
    format!(
        "taarof: diag[{}/{}/{}] {}{}",
        record.category,
        record.source,
        record.action,
        record.message,
        record
            .details
            .as_ref()
            .map(|details| format!(" {details}"))
            .unwrap_or_default()
    )
}

#[cfg(not(test))]
fn tagged_stderr(record: &DiagnosticRecord) {
    eprintln!("{}", stderr_line(record));
}

fn make_record(
    level: DiagnosticLevel,
    category: &str,
    source: &str,
    action: &str,
    message: impl Into<String>,
    details: Option<Value>,
) -> DiagnosticRecord {
    DiagnosticRecord {
        ts_unix_ms: unix_time_ms(),
        level,
        category: category.to_string(),
        source: source.to_string(),
        action: action.to_string(),
        message: message.into(),
        details,
    }
}

fn web_asset_lookup_details(attempted_paths: &[PathBuf]) -> Value {
    let attempted_paths = attempted_paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();

    serde_json::json!({
        "attempted_paths": attempted_paths,
    })
}

#[cfg(not(test))]
fn process_journal() -> &'static Mutex<DiagnosticJournal> {
    static JOURNAL: OnceLock<Mutex<DiagnosticJournal>> = OnceLock::new();
    JOURNAL.get_or_init(|| Mutex::new(DiagnosticJournal::default()))
}

#[cfg(not(test))]
fn process_history_sink() -> &'static Mutex<Option<crate::history::HistoryHandle>> {
    static SINK: OnceLock<Mutex<Option<crate::history::HistoryHandle>>> = OnceLock::new();
    SINK.get_or_init(|| Mutex::new(None))
}

#[cfg(not(test))]
pub fn install_history_sink(sink: crate::history::HistoryHandle) {
    if let Ok(mut current) = process_history_sink().lock() {
        *current = Some(sink);
    }
}

#[cfg(test)]
pub fn install_history_sink(_sink: crate::history::HistoryHandle) {}

#[cfg(not(test))]
fn record_global(record: DiagnosticRecord) {
    record_global_with_history(record, true);
}

#[cfg(not(test))]
fn record_global_with_history(record: DiagnosticRecord, include_history: bool) {
    record_to_sinks(
        record,
        tagged_stderr,
        |record| {
            if let Ok(mut journal) = process_journal().lock() {
                journal.record(record.clone());
            }
        },
        |record| {
            let history_sink = include_history
                .then(process_history_sink)
                .and_then(|sink| sink.lock().ok().and_then(|sink| sink.clone()));
            if let Some(sink) = history_sink {
                sink.try_record_diagnostic(record);
            }
        },
    );
}

#[cfg(not(test))]
pub fn snapshot() -> DiagnosticSnapshot {
    process_journal()
        .lock()
        .map(|journal| journal.snapshot())
        .unwrap_or_default()
}

#[cfg(test)]
pub fn snapshot() -> DiagnosticSnapshot {
    DiagnosticSnapshot::default()
}

#[cfg(not(test))]
pub fn record_lifecycle(action: &str, message: impl Into<String>, details: Option<Value>) {
    record_global(make_record(
        DiagnosticLevel::Info,
        "lifecycle",
        "runtime",
        action,
        message,
        details,
    ));
}

#[cfg(test)]
pub fn record_lifecycle(_action: &str, _message: impl Into<String>, _details: Option<Value>) {}

#[cfg(all(not(test), feature = "history"))]
pub fn record_history_backpressure(message: impl Into<String>, details: Option<Value>) {
    record_global_with_history(
        make_record(
            DiagnosticLevel::Warn,
            "history_backpressure",
            "history",
            "enqueue",
            message,
            details,
        ),
        false,
    );
}

#[cfg(test)]
pub fn record_history_backpressure(_message: impl Into<String>, _details: Option<Value>) {}

#[cfg(not(test))]
pub fn record_config_error(message: impl Into<String>, details: Option<Value>) {
    record_global(make_record(
        DiagnosticLevel::Error,
        "configuration",
        "config.toml",
        "validate",
        message,
        details,
    ));
}

#[cfg(test)]
pub fn record_config_error(_message: impl Into<String>, _details: Option<Value>) {}

#[cfg(all(not(test), feature = "history"))]
pub fn record_history_maintenance(
    level: DiagnosticLevel,
    action: &str,
    message: impl Into<String>,
    details: Option<Value>,
) {
    record_global_with_history(
        make_record(
            level,
            "history_maintenance",
            "history",
            action,
            message,
            details,
        ),
        false,
    );
}

#[cfg(test)]
pub fn record_history_maintenance(
    _level: DiagnosticLevel,
    _action: &str,
    _message: impl Into<String>,
    _details: Option<Value>,
) {
}

#[cfg(not(test))]
pub fn record_command_failure(
    source: &str,
    action: &str,
    message: impl Into<String>,
    details: Option<Value>,
) {
    record_global(make_record(
        DiagnosticLevel::Error,
        "command_failure",
        source,
        action,
        message,
        details,
    ));
}

#[cfg(test)]
pub fn record_command_failure(
    _source: &str,
    _action: &str,
    _message: impl Into<String>,
    _details: Option<Value>,
) {
}

#[cfg(not(test))]
pub fn record_probe_failure(
    source: &str,
    action: &str,
    message: impl Into<String>,
    details: Option<Value>,
) {
    record_global(make_record(
        DiagnosticLevel::Warn,
        "probe_failure",
        source,
        action,
        message,
        details,
    ));
}

#[cfg(test)]
pub fn record_probe_failure(
    _source: &str,
    _action: &str,
    _message: impl Into<String>,
    _details: Option<Value>,
) {
}

#[cfg(not(test))]
pub fn record_event_drop(message: impl Into<String>, details: Option<Value>) {
    record_global(make_record(
        DiagnosticLevel::Warn,
        "event_drop",
        "events",
        "ring-overflow",
        message,
        details,
    ));
}

#[cfg(test)]
pub fn record_event_drop(_message: impl Into<String>, _details: Option<Value>) {}

#[cfg(not(test))]
pub fn record_web_asset_lookup_failure(message: impl Into<String>, attempted_paths: &[PathBuf]) {
    record_global(make_record(
        DiagnosticLevel::Info,
        "http_asset_lookup",
        "http",
        "serve-web-assets",
        message,
        Some(web_asset_lookup_details(attempted_paths)),
    ));
}

#[cfg(not(test))]
pub fn reset_journal_for_tests(log_path: Option<PathBuf>, archive_path: Option<PathBuf>) {
    if let Ok(mut journal) = process_journal().lock() {
        *journal = DiagnosticJournal::new_with_paths(
            log_path,
            archive_path,
            DiagnosticRetention::default(),
        );
    }
}

#[cfg(test)]
pub fn record_web_asset_lookup_failure(_message: impl Into<String>, _attempted_paths: &[PathBuf]) {}

#[cfg(test)]
pub fn reset_journal_for_tests(_log_path: Option<PathBuf>, _archive_path: Option<PathBuf>) {}

#[cfg(test)]
mod tests {
    use super::{DiagnosticJournal, DiagnosticLevel, DiagnosticRetention};

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "taarof-diagnostics-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    fn sensitive_record() -> super::DiagnosticRecord {
        super::make_record(
            DiagnosticLevel::Error,
            "command_failure",
            "git",
            "worktree-list",
            "permission denied: password=INERT_TEST_VALUE",
            Some(serde_json::json!({
                "exit_code": 128, "stderr": "INERT_PRIVATE_OUTPUT",
                "argv": ["git", "--password", "INERT_TEST_VALUE"],
                "error": "permission denied: password=INERT_TEST_VALUE",
                "status": "exit status: 128"
            })),
        )
    }

    #[test]
    fn diagnostic_admission_redacts_recent_jsonl_and_restored_records() {
        let dir = unique_temp_dir("admission");
        let log = dir.join("diagnostics.jsonl");
        let mut journal = DiagnosticJournal::new_with_paths(
            Some(log.clone()),
            None,
            DiagnosticRetention::default(),
        );
        journal.record(sensitive_record());
        for text in [
            serde_json::to_string(&journal.snapshot()).unwrap(),
            std::fs::read_to_string(&log).unwrap(),
        ] {
            assert!(
                !text.contains("INERT_"),
                "sensitive fixture escaped admission"
            );
            assert!(text.contains("worktree-list"));
            assert!(text.contains("128"));
        }
        // Older versions may have retained raw records. Loading must not project
        // those records back into the recent/API sink.
        std::fs::write(&log, serde_json::to_string(&sensitive_record()).unwrap()).unwrap();
        let mut restored =
            DiagnosticJournal::new_with_paths(Some(log), None, DiagnosticRetention::default());
        restored.load_persisted_records();
        assert!(!serde_json::to_string(&restored.snapshot())
            .unwrap()
            .contains("INERT_"));
    }

    #[test]
    fn diagnostic_admission_private_files_survive_rotation_and_reopen() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_temp_dir("private");
        let log = dir.join("diagnostics.jsonl");
        let archive = dir.join("diagnostics.jsonl.1");
        let mut journal = DiagnosticJournal::new_with_paths(
            Some(log.clone()),
            Some(archive.clone()),
            DiagnosticRetention {
                log_max_bytes: 1,
                ..DiagnosticRetention::default()
            },
        );
        journal.record(sensitive_record());
        assert_eq!(
            std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Simulate an existing file created by an older release under umask 000.
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o666)).unwrap();
        journal.record(sensitive_record());
        for path in [&log, &archive] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(!std::fs::read_to_string(path).unwrap().contains("INERT_"));
        }
    }

    #[test]
    fn diagnostic_admission_every_sink_receives_only_sanitized_data() {
        let dir = unique_temp_dir("all-sinks");
        let log = dir.join("diagnostics.jsonl");
        let archive = dir.join("diagnostics.jsonl.1");
        let mut journal = DiagnosticJournal::new_with_paths(
            Some(log.clone()),
            Some(archive.clone()),
            DiagnosticRetention {
                log_max_bytes: 1,
                ..DiagnosticRetention::default()
            },
        );
        let mut stderr = String::new();
        let mut history_input = Vec::new();
        for _ in 0..2 {
            super::record_to_sinks(
                sensitive_record(),
                |record| stderr.push_str(&super::stderr_line(record)),
                |record| journal.record(record.clone()),
                |record| history_input.push(record.clone()),
            );
        }
        for text in [
            stderr,
            serde_json::to_string(&journal.snapshot()).unwrap(),
            std::fs::read_to_string(log).unwrap(),
            std::fs::read_to_string(archive).unwrap(),
            serde_json::to_string(&history_input).unwrap(),
        ] {
            assert!(!text.contains("INERT_"));
            assert!(text.contains("git"));
            assert!(text.contains("worktree-list"));
            assert!(text.contains("permission_denied"));
            assert!(text.contains("128"));
        }
        let safe = &history_input[0];
        assert_eq!(safe.details.as_ref().unwrap()["exit_code"], 128);
        assert_eq!(
            safe.details.as_ref().unwrap()["failure_kind"],
            "permission_denied"
        );
        assert_eq!(
            serde_json::to_value(super::sanitize_record(safe.clone())).unwrap(),
            serde_json::to_value(safe).unwrap()
        );

        #[cfg(feature = "history")]
        {
            let (handle, reader) = crate::history::HistoryHandle::open_at(
                crate::history::HistoryConfig {
                    enabled: true,
                    ..Default::default()
                },
                dir.join("history.sqlite3"),
            );
            for record in &history_input {
                handle.try_record_diagnostic(record);
            }
            handle.flush().unwrap();
            let page = reader.query(None, Some(10), Default::default()).unwrap();
            let text = serde_json::to_string(&page).unwrap();
            assert!(!text.contains("INERT_"));
            assert!(text.contains("worktree-list"));
            assert!(text.contains("permission_denied"));
            assert!(text.contains("128"));
        }
    }

    #[test]
    fn diagnostic_admission_preserves_safe_context_and_rejects_hostile_fields() {
        let safe = super::sanitize_record(super::make_record(
            DiagnosticLevel::Error,
            "command_failure",
            "git",
            "worktree-list",
            "git executable was not found",
            Some(
                serde_json::json!({"exit_code":127, "status":"exit status: 127", "stderr":"ordinary output also omitted"}),
            ),
        ));
        assert_eq!(safe.message, "git executable was not found");
        assert!(safe.details.unwrap().get("stderr").is_none());
        for value in [
            "password=INERT_TEST_VALUE",
            "Bearer INERT_TEST_VALUE",
            "postgresql://user:pass@example.test/db",
            "\u{1b}[31mINERT_TEST_VALUE",
            "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
        ] {
            let record = super::sanitize_record(super::make_record(
                DiagnosticLevel::Warn,
                value,
                value,
                value,
                value,
                Some(serde_json::json!({"nested": {"secret": value}})),
            ));
            assert!(!serde_json::to_string(&record).unwrap().contains(value));
        }
        let asset = super::sanitize_record(super::make_record(
            DiagnosticLevel::Info,
            "http_asset_lookup",
            "http",
            "serve-web-assets",
            "web assets unavailable",
            Some(
                serde_json::json!({"attempted_paths":["/tmp/taarof-web-dist", "https://user:password@example.test/", "password=INERT_TEST_VALUE"]}),
            ),
        ));
        assert_eq!(
            asset.details.unwrap()["attempted_paths"],
            serde_json::json!(["/tmp/taarof-web-dist"])
        );
    }

    #[test]
    fn diagnostic_admission_quoted_fields_are_redacted_before_all_sinks() {
        let dir = unique_temp_dir("quoted-fields");
        let log = dir.join("diagnostics.jsonl");
        let mut journal = DiagnosticJournal::new_with_paths(
            Some(log.clone()),
            None,
            DiagnosticRetention::default(),
        );
        #[cfg(feature = "history")]
        let (handle, reader) = crate::history::HistoryHandle::open_at(
            crate::history::HistoryConfig {
                enabled: true,
                ..Default::default()
            },
            dir.join("history.sqlite3"),
        );
        for message in [
            r#"{"password":"INERTshort","status":"failed"}"#,
            r#"{ "password" : "INERTshort", "status" : "failed" }"#,
            r#"command failed: {"password":"INERTshort"}"#,
            r#"{"\u0070assword":"INERTshort"}"#,
        ] {
            let record = super::make_record(
                DiagnosticLevel::Error,
                "command_failure",
                "git",
                "worktree-list",
                message,
                Some(serde_json::json!({"status":message, "exit_code":128})),
            );
            let mut stderr = String::new();
            let mut history_input = String::new();
            super::record_to_sinks(
                record,
                |record| stderr = super::stderr_line(record),
                |record| journal.record(record.clone()),
                |record| {
                    history_input = serde_json::to_string(record).unwrap();
                    #[cfg(feature = "history")]
                    handle.try_record_diagnostic(record);
                },
            );
            for text in [
                stderr,
                history_input,
                serde_json::to_string(&journal.snapshot()).unwrap(),
                std::fs::read_to_string(&log).unwrap(),
            ] {
                assert!(
                    !text.contains("INERTshort"),
                    "quoted credential escaped admission"
                );
                assert!(text.contains("worktree-list"));
                assert!(text.contains("128"));
            }
        }
        #[cfg(feature = "history")]
        {
            handle.flush().unwrap();
            let page = reader.query(None, Some(10), Default::default()).unwrap();
            let serialized = serde_json::to_string(&page).unwrap();
            assert!(!serialized.contains("INERTshort"));
            assert!(serialized.contains("worktree-list"));
        }
        assert_eq!(
            super::safe_text("Operation failed; retry after permission repair"),
            Some("Operation failed; retry after permission repair".to_string())
        );
        assert_eq!(
            super::safe_text(r#"{"status":"failed"}"#),
            Some(r#"{"status":"failed"}"#.to_string())
        );
    }

    #[test]
    fn diagnostic_admission_refuses_symlink_journals() {
        let dir = unique_temp_dir("symlink");
        let target = dir.join("untouched");
        std::fs::write(&target, "inert sentinel").unwrap();
        let log = dir.join("diagnostics.jsonl");
        std::os::unix::fs::symlink(&target, &log).unwrap();
        let mut journal =
            DiagnosticJournal::new_with_paths(Some(log), None, DiagnosticRetention::default());
        journal.record(sensitive_record());
        assert_eq!(journal.snapshot().counters.write_failures, 1);
        assert_eq!(std::fs::read_to_string(target).unwrap(), "inert sentinel");
    }

    #[test]
    fn journal_retains_recent_records_and_counts_failures() {
        let dir = unique_temp_dir("recent");
        let log_path = dir.join("diagnostics.jsonl");
        let archive_path = dir.join("diagnostics.jsonl.1");
        let retention = DiagnosticRetention {
            recent_record_limit: 2,
            log_max_bytes: 4096,
            archive_count: 1,
        };
        let mut journal =
            DiagnosticJournal::new_with_paths(Some(log_path), Some(archive_path), retention);

        journal.record(super::make_record(
            DiagnosticLevel::Error,
            "command_failure",
            "git",
            "worktree-list",
            "git failed",
            None,
        ));
        journal.record(super::make_record(
            DiagnosticLevel::Warn,
            "probe_failure",
            "terminal",
            "tmux-pane-info",
            "probe stale",
            None,
        ));
        journal.record(super::make_record(
            DiagnosticLevel::Info,
            "lifecycle",
            "runtime",
            "startup",
            "session started",
            None,
        ));

        let snapshot = journal.snapshot();
        assert_eq!(snapshot.counters.total_records, 3);
        assert_eq!(snapshot.counters.command_failures, 1);
        assert_eq!(snapshot.counters.probe_failures, 1);
        assert_eq!(snapshot.counters.lifecycle_events, 1);
        assert_eq!(snapshot.recent.len(), 2);
        assert_eq!(snapshot.recent[0].action, "tmux-pane-info");
        assert_eq!(snapshot.recent[1].action, "startup");
    }

    #[test]
    fn journal_rotates_log_when_it_exceeds_limit() {
        let dir = unique_temp_dir("rotate");
        let log_path = dir.join("diagnostics.jsonl");
        let archive_path = dir.join("diagnostics.jsonl.1");
        let retention = DiagnosticRetention {
            recent_record_limit: 8,
            log_max_bytes: 180,
            archive_count: 1,
        };
        let mut journal = DiagnosticJournal::new_with_paths(
            Some(log_path.clone()),
            Some(archive_path.clone()),
            retention,
        );

        for idx in 0..6 {
            journal.record(super::make_record(
                DiagnosticLevel::Error,
                "command_failure",
                "mise",
                "discover",
                format!("failure #{idx}"),
                Some(serde_json::json!({ "idx": idx })),
            ));
        }

        assert!(log_path.exists(), "current log should exist");
        assert!(
            archive_path.exists(),
            "archive log should exist after rotation"
        );
    }

    #[test]
    fn journal_loads_existing_records_from_disk() {
        let dir = unique_temp_dir("load");
        let log_path = dir.join("diagnostics.jsonl");
        let archive_path = dir.join("diagnostics.jsonl.1");
        std::fs::write(
            &log_path,
            [
                serde_json::to_string(&super::make_record(
                    DiagnosticLevel::Error,
                    "command_failure",
                    "git",
                    "create-worktree",
                    "branch exists",
                    None,
                ))
                .unwrap(),
                serde_json::to_string(&super::make_record(
                    DiagnosticLevel::Warn,
                    "event_drop",
                    "events",
                    "ring-overflow",
                    "dropped event",
                    None,
                ))
                .unwrap(),
            ]
            .join("\n"),
        )
        .expect("fixture log should be written");

        let retention = DiagnosticRetention {
            recent_record_limit: 8,
            log_max_bytes: 4096,
            archive_count: 1,
        };
        let mut journal =
            DiagnosticJournal::new_with_paths(Some(log_path), Some(archive_path), retention);
        journal.load_persisted_records();
        let snapshot = journal.snapshot();
        assert_eq!(snapshot.counters.total_records, 2);
        assert_eq!(snapshot.counters.command_failures, 1);
        assert_eq!(snapshot.counters.event_drops, 1);
    }

    #[test]
    fn web_asset_lookup_details_preserve_attempted_paths() {
        let attempted_path = std::path::PathBuf::from("/tmp/taarof-web-dist");
        let details = super::web_asset_lookup_details(std::slice::from_ref(&attempted_path));

        assert_eq!(
            details,
            serde_json::json!({
                "attempted_paths": ["/tmp/taarof-web-dist"],
            })
        );
    }
}
