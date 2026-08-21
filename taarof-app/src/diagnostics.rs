use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
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
        self.apply_record(record.clone());
        if let Err(error) = self.persist_record(&record) {
            self.counters.write_failures += 1;
            eprintln!("taarof: diag[write-failure/journal] {error}");
        }
    }

    fn load_persisted_records(&mut self) {
        let paths = [self.archive_path.clone(), self.log_path.clone()];
        for path in paths.into_iter().flatten() {
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in contents.lines() {
                let Ok(record) = serde_json::from_str::<DiagnosticRecord>(line) else {
                    continue;
                };
                self.apply_record(record);
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
            create_dir_all(parent).map_err(|error| {
                format!(
                    "could not create diagnostics dir {}: {error}",
                    parent.display()
                )
            })?;
        }

        let serialized = serde_json::to_string(record)
            .map_err(|error| format!("could not serialize diagnostic record: {error}"))?;

        self.rotate_if_needed(log_path, serialized.len() as u64 + 1)?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .map_err(|error| format!("could not open {}: {error}", log_path.display()))?;
        writeln!(file, "{serialized}")
            .map_err(|error| format!("could not write {}: {error}", log_path.display()))?;
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
        std::fs::rename(log_path, archive_path).map_err(|error| {
            format!(
                "could not rotate diagnostics log {} -> {}: {error}",
                log_path.display(),
                archive_path.display()
            )
        })?;
        Ok(())
    }
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

#[cfg(not(test))]
fn tagged_stderr(record: &DiagnosticRecord) {
    eprintln!(
        "taarof: diag[{}/{}/{}] {}",
        record.category, record.source, record.action, record.message
    );
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
    tagged_stderr(&record);
    if let Ok(mut journal) = process_journal().lock() {
        journal.record(record.clone());
    }
    let history_sink = include_history
        .then(process_history_sink)
        .and_then(|sink| sink.lock().ok().and_then(|sink| sink.clone()));
    if let Some(sink) = history_sink {
        sink.try_record_diagnostic(&record);
    }
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
