//! Optional, sanitized, observational SQLite history.
//!
//! This store is deliberately independent from Taarof's canonical JSON
//! persistence. Producers only enqueue allowlisted metadata; SQLite work is
//! performed off the GTK thread.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(feature = "history")]
mod maintenance;
#[cfg(feature = "history")]
mod query;
#[cfg(feature = "history")]
mod sanitize;
#[cfg(feature = "history")]
mod schema;
#[cfg(feature = "history")]
mod writer;

pub const HISTORY_SCHEMA: &str = "taarof.history.v1";
pub const HISTORY_SCHEMA_VERSION: u64 = 3;
pub const DEFAULT_HISTORY_LIMIT: usize = 100;
pub const MAX_HISTORY_LIMIT: usize = 500;
pub const DEFAULT_HISTORY_SCAN_BUDGET: usize = 20_000;
pub const MAX_HISTORY_SCAN_BUDGET: usize = 100_000;

#[derive(Clone, Debug)]
pub struct HistoryConfig {
    pub enabled: bool,
    pub max_age_days: u64,
    pub max_records: u64,
    pub max_bytes: u64,
    pub maintenance_interval_minutes: u64,
    pub queue_capacity: usize,
    pub record_events: bool,
    pub record_diagnostics: bool,
    pub record_work: bool,
    pub config_error: Option<String>,
}

pub const MIN_HISTORY_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_HISTORY_BYTES: u64 = 64 * 1024 * 1024 * 1024;

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_age_days: 30,
            max_records: 200_000,
            max_bytes: 256 * 1024 * 1024,
            maintenance_interval_minutes: 15,
            queue_capacity: 4_096,
            record_events: true,
            record_diagnostics: true,
            record_work: true,
            config_error: None,
        }
    }
}

impl HistoryConfig {
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        if self.max_age_days != 0 && !(1..=3_650).contains(&self.max_age_days) {
            errors.push(format!(
                "history.max_age_days is {}; expected 0 (unbounded) or 1..=3650",
                self.max_age_days
            ));
        }
        if self.max_records != 0 && !(1_000..=10_000_000).contains(&self.max_records) {
            errors.push(format!(
                "history.max_records is {}; expected 0 (unbounded) or 1000..=10000000",
                self.max_records
            ));
        }
        if self.max_bytes != 0 && !(MIN_HISTORY_BYTES..=MAX_HISTORY_BYTES).contains(&self.max_bytes)
        {
            errors.push(format!(
                "history.max_bytes is {}; expected 0 (unbounded) or {}..={} bytes (8 MiB..=64 GiB), leaving room for a checkpointed WAL",
                self.max_bytes, MIN_HISTORY_BYTES, MAX_HISTORY_BYTES
            ));
        }
        if !(1..=1_440).contains(&self.maintenance_interval_minutes) {
            errors.push(format!(
                "history.maintenance_interval_minutes is {}; expected 1..=1440",
                self.maintenance_interval_minutes
            ));
        }
        if !(64..=1_048_576).contains(&self.queue_capacity) {
            errors.push(format!(
                "history.queue_capacity is {}; expected 64..=1048576",
                self.queue_capacity
            ));
        }
        if self.enabled && !self.record_events && !self.record_diagnostics && !self.record_work {
            errors.push(
                "history is enabled but record_events, record_diagnostics, and record_work are all false; enable at least one record class"
                    .to_string(),
            );
        }
        if self.enabled && self.max_age_days == 0 && self.max_records == 0 && self.max_bytes == 0 {
            errors.push(
                "history is enabled with max_age_days, max_records, and max_bytes all set to 0; configure at least one retention bound"
                    .to_string(),
            );
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[derive(Clone, Debug)]
pub enum HistoryRecordDraft {
    Event(crate::events::EventRecord),
    Diagnostic(crate::diagnostics::DiagnosticRecord),
    Work(Box<crate::work_ledger::WorkRecord>),
}

impl HistoryRecordDraft {
    pub fn event(
        seq: u64,
        ts_unix_ms: u64,
        event_type: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self::Event(crate::events::EventRecord {
            seq,
            ts_unix_ms,
            event_type: event_type.into(),
            payload,
        })
    }

    pub fn diagnostic(
        ts_unix_ms: u64,
        level: &str,
        category: impl Into<String>,
        source: impl Into<String>,
        action: impl Into<String>,
        message: impl Into<String>,
        details: Option<serde_json::Value>,
    ) -> Option<Self> {
        let level = match level {
            "info" => crate::diagnostics::DiagnosticLevel::Info,
            "warn" => crate::diagnostics::DiagnosticLevel::Warn,
            "error" => crate::diagnostics::DiagnosticLevel::Error,
            _ => return None,
        };
        Some(Self::Diagnostic(crate::diagnostics::DiagnosticRecord {
            ts_unix_ms,
            level,
            category: category.into(),
            source: source.into(),
            action: action.into(),
            message: message.into(),
            details,
        }))
    }

    pub fn work(record: crate::work_ledger::WorkRecord) -> Self {
        Self::Work(Box::new(record))
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HistoryOrder {
    #[default]
    Asc,
    Desc,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct HistoryFilters {
    #[serde(default)]
    pub from_ts: Option<u64>,
    #[serde(default)]
    pub to_ts: Option<u64>,
    #[serde(default)]
    pub record_type: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub pane: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub authority: Option<String>,
    #[serde(default)]
    pub verification: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
    /// Case-insensitive bounded match over sanitized `summary` and `subtype` only.
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub order: HistoryOrder,
    #[serde(default)]
    pub scan_budget: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct HistoryRecord {
    pub id: u64,
    pub ts_unix_ms: u64,
    pub record_type: String,
    pub subtype: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_space: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_seq: Option<u64>,
    pub session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attrs: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct HistoryPage {
    pub schema: String,
    pub since_id: Option<u64>,
    pub limit: usize,
    pub next_id: u64,
    pub has_more: bool,
    pub scanned: usize,
    pub scan_exhausted: bool,
    /// True when a requested limit or scan budget exceeded the supported bound.
    pub truncated: bool,
    pub filters: HistoryFilters,
    pub records: Vec<HistoryRecord>,
}

/// Cooperative cancellation handle for a single SQLite history query.
///
/// Cancelling before the read connection is ready is remembered; cancelling
/// after it is ready delegates to SQLite's thread-safe interrupt handle.
#[derive(Clone, Default)]
pub struct HistoryQueryToken {
    inner: Arc<HistoryQueryTokenInner>,
}

#[derive(Default)]
struct HistoryQueryTokenInner {
    cancelled: AtomicBool,
    #[cfg(feature = "history")]
    interrupt: Mutex<Option<rusqlite::InterruptHandle>>,
}

impl HistoryQueryToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        #[cfg(feature = "history")]
        if let Ok(interrupt) = self.inner.interrupt.lock() {
            if let Some(interrupt) = interrupt.as_ref() {
                interrupt.interrupt();
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    #[cfg(feature = "history")]
    fn install(&self, conn: &rusqlite::Connection) {
        if let Ok(mut interrupt) = self.inner.interrupt.lock() {
            *interrupt = Some(conn.get_interrupt_handle());
        }
        if self.is_cancelled() {
            self.cancel();
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LastDurableSeq {
    pub event: u64,
    pub work: u64,
    pub id: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct HistoryStatus {
    pub available: bool,
    pub enabled: bool,
    pub schema_version: u64,
    pub last_durable_seq: LastDurableSeq,
    pub queue_depth: usize,
    pub lag_ms: u64,
    pub dropped: u64,
    pub last_commit_at_unix_ms: Option<u64>,
    pub state: String,
    pub reason: Option<String>,
    pub storage: HistoryStorageStatus,
    pub maintenance: HistoryMaintenanceStatus,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct HistoryStorageStatus {
    pub main_bytes: u64,
    pub wal_bytes: u64,
    pub soft_max_bytes: u64,
    pub page_count: u64,
    pub free_pages: u64,
    pub record_count: u64,
    pub oldest_record_ts_unix_ms: Option<u64>,
    pub newest_record_ts_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct HistoryMaintenanceStatus {
    pub last_run_at_unix_ms: Option<u64>,
    pub next_run_at_unix_ms: Option<u64>,
    pub last_result: String,
    pub rows_removed_last: u64,
    pub rows_removed_total: u64,
    pub duration_ms: u64,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
}

impl Default for HistoryMaintenanceStatus {
    fn default() -> Self {
        Self {
            last_run_at_unix_ms: None,
            next_run_at_unix_ms: None,
            last_result: "never".to_string(),
            rows_removed_last: 0,
            rows_removed_total: 0,
            duration_ms: 0,
            consecutive_failures: 0,
            last_error: None,
        }
    }
}

pub(crate) struct HistoryStatusInner {
    enabled: bool,
    available: AtomicBool,
    schema_version: AtomicU64,
    last_event_seq: AtomicU64,
    last_work_seq: AtomicU64,
    last_id: AtomicU64,
    queue_depth: AtomicUsize,
    dropped: AtomicU64,
    #[cfg(feature = "history")]
    backpressure_diagnostic_pending: AtomicBool,
    last_commit_at: AtomicU64,
    oldest_queued_at: AtomicU64,
    state: Mutex<String>,
    reason: Mutex<Option<String>>,
    storage: Mutex<HistoryStorageStatus>,
    maintenance: Mutex<HistoryMaintenanceStatus>,
}

impl HistoryStatusInner {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            available: AtomicBool::new(false),
            schema_version: AtomicU64::new(0),
            last_event_seq: AtomicU64::new(0),
            last_work_seq: AtomicU64::new(0),
            last_id: AtomicU64::new(0),
            queue_depth: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            #[cfg(feature = "history")]
            backpressure_diagnostic_pending: AtomicBool::new(false),
            last_commit_at: AtomicU64::new(0),
            oldest_queued_at: AtomicU64::new(0),
            state: Mutex::new(if enabled { "starting" } else { "disabled" }.to_string()),
            reason: Mutex::new((!enabled).then(|| "history is disabled by configuration".into())),
            storage: Mutex::new(HistoryStorageStatus::default()),
            maintenance: Mutex::new(HistoryMaintenanceStatus::default()),
        }
    }

    pub(crate) fn set_state(&self, state: &str, reason: Option<String>) {
        if let Ok(mut value) = self.state.lock() {
            *value = state.to_string();
        }
        if let Ok(mut value) = self.reason.lock() {
            *value = reason;
        }
    }

    fn snapshot(&self) -> HistoryStatus {
        let queue_depth = self.queue_depth.load(Ordering::Relaxed);
        let oldest_queued_at = self.oldest_queued_at.load(Ordering::Relaxed);
        HistoryStatus {
            available: self.available.load(Ordering::Acquire),
            enabled: self.enabled,
            schema_version: self.schema_version.load(Ordering::Relaxed),
            last_durable_seq: LastDurableSeq {
                event: self.last_event_seq.load(Ordering::Relaxed),
                work: self.last_work_seq.load(Ordering::Relaxed),
                id: self.last_id.load(Ordering::Relaxed),
            },
            queue_depth,
            lag_ms: if queue_depth == 0 || oldest_queued_at == 0 {
                0
            } else {
                now_unix_ms().saturating_sub(oldest_queued_at)
            },
            dropped: self.dropped.load(Ordering::Relaxed),
            last_commit_at_unix_ms: nonzero(self.last_commit_at.load(Ordering::Relaxed)),
            state: self
                .state
                .lock()
                .map(|value| value.clone())
                .unwrap_or_else(|_| "degraded".to_string()),
            reason: self.reason.lock().ok().and_then(|value| value.clone()),
            storage: self
                .storage
                .lock()
                .map(|value| value.clone())
                .unwrap_or_default(),
            maintenance: self
                .maintenance
                .lock()
                .map(|value| value.clone())
                .unwrap_or_default(),
        }
    }
}

fn nonzero(value: u64) -> Option<u64> {
    (value != 0).then_some(value)
}

#[derive(Clone)]
pub struct HistoryHandle {
    status: Arc<HistoryStatusInner>,
    config: HistoryConfig,
    #[cfg(feature = "history")]
    writer: Option<Arc<writer::WriterClient>>,
}

impl std::fmt::Debug for HistoryHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HistoryHandle")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct HistoryReader {
    status: Arc<HistoryStatusInner>,
    #[cfg(feature = "history")]
    path: Option<Arc<PathBuf>>,
}

impl HistoryHandle {
    pub fn disabled() -> (Self, HistoryReader) {
        let status = Arc::new(HistoryStatusInner::new(false));
        (
            Self {
                status: Arc::clone(&status),
                config: HistoryConfig::default(),
                #[cfg(feature = "history")]
                writer: None,
            },
            HistoryReader {
                status,
                #[cfg(feature = "history")]
                path: None,
            },
        )
    }

    pub fn open(config: HistoryConfig) -> (Self, HistoryReader) {
        if let Some(error) = config.config_error.clone() {
            return Self::misconfigured(config, error);
        }
        if let Err(errors) = config.validate() {
            return Self::misconfigured(config, errors.join("; "));
        }
        if !config.enabled {
            return Self::disabled();
        }
        let Some(path) = default_history_path() else {
            return Self::unavailable(config, "no XDG state or data directory is available");
        };
        Self::open_at(config, path)
    }

    pub fn open_at(config: HistoryConfig, path: PathBuf) -> (Self, HistoryReader) {
        if let Some(error) = config.config_error.clone() {
            return Self::misconfigured(config, error);
        }
        if let Err(errors) = config.validate() {
            return Self::misconfigured(config, errors.join("; "));
        }
        if !config.enabled {
            return Self::disabled();
        }
        let status = Arc::new(HistoryStatusInner::new(true));

        #[cfg(feature = "history")]
        {
            let writer = match writer::WriterClient::start(
                path.clone(),
                config.clone(),
                Arc::clone(&status),
            ) {
                Ok(writer) => Some(Arc::new(writer)),
                Err(error) => {
                    status.set_state("degraded", Some(error));
                    None
                }
            };
            let reader = HistoryReader {
                status: Arc::clone(&status),
                path: Some(Arc::new(path)),
            };
            (
                Self {
                    status,
                    config,
                    writer,
                },
                reader,
            )
        }

        #[cfg(not(feature = "history"))]
        {
            let _ = path;
            status.set_state(
                "unavailable",
                Some("binary was built without the history feature".to_string()),
            );
            let reader = HistoryReader {
                status: Arc::clone(&status),
            };
            (Self { status, config }, reader)
        }
    }

    fn unavailable(config: HistoryConfig, reason: &str) -> (Self, HistoryReader) {
        let status = Arc::new(HistoryStatusInner::new(true));
        status.set_state("unavailable", Some(reason.to_string()));
        (
            Self {
                status: Arc::clone(&status),
                config,
                #[cfg(feature = "history")]
                writer: None,
            },
            HistoryReader {
                status,
                #[cfg(feature = "history")]
                path: None,
            },
        )
    }

    fn misconfigured(config: HistoryConfig, reason: String) -> (Self, HistoryReader) {
        let status = Arc::new(HistoryStatusInner::new(config.enabled));
        status.set_state("misconfigured", Some(reason));
        (
            Self {
                status: Arc::clone(&status),
                config,
                #[cfg(feature = "history")]
                writer: None,
            },
            HistoryReader {
                status,
                #[cfg(feature = "history")]
                path: None,
            },
        )
    }

    pub fn status(&self) -> HistoryStatus {
        self.status.snapshot()
    }

    pub fn records_events(&self) -> bool {
        self.config.enabled && self.config.record_events
    }

    pub fn records_diagnostics(&self) -> bool {
        self.config.enabled && self.config.record_diagnostics
    }

    pub fn records_work(&self) -> bool {
        self.config.enabled && self.config.record_work
    }

    pub fn try_record(&self, draft: HistoryRecordDraft) {
        #[cfg(feature = "history")]
        {
            let allowed = match &draft {
                HistoryRecordDraft::Event(record) => {
                    self.records_events() && sanitize::event_is_allowlisted(&record.event_type)
                }
                HistoryRecordDraft::Diagnostic(_) => self.records_diagnostics(),
                HistoryRecordDraft::Work(_) => self.records_work(),
            };
            if allowed {
                if let Some(writer) = &self.writer {
                    writer.try_record(draft);
                }
            }
        }
        #[cfg(not(feature = "history"))]
        let _ = draft;
    }

    pub fn try_record_event(&self, record: &crate::events::EventRecord) {
        #[cfg(feature = "history")]
        if self.records_events() && sanitize::event_is_allowlisted(&record.event_type) {
            self.try_record(HistoryRecordDraft::Event(record.clone()));
        }
        #[cfg(not(feature = "history"))]
        let _ = record;
    }

    pub fn try_record_diagnostic(&self, record: &crate::diagnostics::DiagnosticRecord) {
        #[cfg(feature = "history")]
        if self.records_diagnostics() {
            self.try_record(HistoryRecordDraft::Diagnostic(record.clone()));
        }
        #[cfg(not(feature = "history"))]
        let _ = record;
    }

    pub fn try_record_work(&self, record: &crate::work_ledger::WorkRecord) {
        #[cfg(feature = "history")]
        if self.records_work() {
            self.try_record(HistoryRecordDraft::Work(Box::new(record.clone())));
        }
        #[cfg(not(feature = "history"))]
        let _ = record;
    }

    pub fn flush(&self) -> Result<(), String> {
        #[cfg(feature = "history")]
        if let Some(writer) = &self.writer {
            return writer.flush();
        }
        Ok(())
    }
}

impl HistoryReader {
    pub fn disabled() -> Self {
        HistoryHandle::disabled().1
    }

    pub fn status(&self) -> HistoryStatus {
        self.status.snapshot()
    }

    pub fn query(
        &self,
        since_id: Option<u64>,
        limit: Option<usize>,
        filters: HistoryFilters,
    ) -> Result<HistoryPage, String> {
        #[cfg(feature = "history")]
        {
            if !self.status.available.load(Ordering::Acquire) {
                return Err(self
                    .status
                    .snapshot()
                    .reason
                    .unwrap_or_else(|| "history is unavailable".to_string()));
            }
            let path = self
                .path
                .as_deref()
                .ok_or_else(|| "history is disabled".to_string())?;
            query::query(path, since_id, limit, filters, None)
        }
        #[cfg(not(feature = "history"))]
        {
            let _ = (since_id, limit, filters);
            Err("binary was built without the history feature".to_string())
        }
    }

    pub fn query_with_token(
        &self,
        since_id: Option<u64>,
        limit: Option<usize>,
        filters: HistoryFilters,
        token: &HistoryQueryToken,
    ) -> Result<HistoryPage, String> {
        #[cfg(feature = "history")]
        {
            if !self.status.available.load(Ordering::Acquire) {
                return Err(self
                    .status
                    .snapshot()
                    .reason
                    .unwrap_or_else(|| "history is unavailable".to_string()));
            }
            let path = self
                .path
                .as_deref()
                .ok_or_else(|| "history is disabled".to_string())?;
            query::query(path, since_id, limit, filters, Some(token))
        }
        #[cfg(not(feature = "history"))]
        {
            let _ = (since_id, limit, filters, token);
            Err("binary was built without the history feature".to_string())
        }
    }
}

pub fn default_history_path() -> Option<PathBuf> {
    let base = dirs::state_dir().or_else(dirs::data_dir)?.join("taarof");
    let filename = crate::instance::session_storage_key()
        .map(|key| format!("history-{key}.sqlite3"))
        .unwrap_or_else(|| "history.sqlite3".to_string());
    Some(base.join(filename))
}

pub fn sidecar_paths(path: &Path) -> [PathBuf; 2] {
    let display = path.as_os_str().to_string_lossy();
    [
        PathBuf::from(format!("{display}-wal")),
        PathBuf::from(format!("{display}-shm")),
    ]
}

pub(crate) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
