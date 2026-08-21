use super::{
    HistoryConfig, HistoryRecord, HistoryRecordDraft, HistoryStatusInner, HISTORY_SCHEMA_VERSION,
};
use rusqlite::{params, Connection, Error as SqlError, ErrorCode, TransactionBehavior};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_BATCH: usize = 512;
const MAINTENANCE_AFTER_BATCHES: u64 = 64;

enum WriterCommand {
    Draft(Box<HistoryRecordDraft>),
    Flush(mpsc::Sender<Result<(), String>>),
    Shutdown,
}

pub(crate) struct WriterClient {
    tx: mpsc::Sender<WriterCommand>,
    status: Arc<HistoryStatusInner>,
    queue_capacity: usize,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl WriterClient {
    pub(crate) fn start(
        path: PathBuf,
        config: HistoryConfig,
        status: Arc<HistoryStatusInner>,
    ) -> Result<Self, String> {
        let capacity = config.queue_capacity.max(1);
        // Draft capacity is reserved atomically before enqueueing. The
        // underlying channel stays unbounded so Flush and Shutdown can never
        // park a caller behind a full draft queue.
        let (tx, rx) = mpsc::channel();
        let thread_status = Arc::clone(&status);
        let join = std::thread::Builder::new()
            .name("taarof-history-writer".to_string())
            .spawn(move || run_writer(path, config, thread_status, rx))
            .map_err(|error| format!("could not start history writer: {error}"))?;
        Ok(Self {
            tx,
            status,
            queue_capacity: capacity,
            join: Mutex::new(Some(join)),
        })
    }

    pub(crate) fn try_record(&self, draft: HistoryRecordDraft) {
        let previous_depth = match self.status.queue_depth.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |depth| (depth < self.queue_capacity).then_some(depth + 1),
        ) {
            Ok(depth) => depth,
            Err(_) => {
                self.record_drop();
                return;
            }
        };
        if previous_depth == 0 {
            self.status
                .oldest_queued_at
                .store(super::now_unix_ms(), Ordering::Release);
        }
        match self.tx.send(WriterCommand::Draft(Box::new(draft))) {
            Ok(()) => {}
            Err(_) => {
                finish_pending(&self.status, 1);
                self.status.available.store(false, Ordering::Release);
                self.status.set_state(
                    "degraded",
                    Some("history writer channel is closed".to_string()),
                );
                self.record_drop();
            }
        }
    }

    fn record_drop(&self) {
        let dropped = self.status.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        self.status.set_state(
            "degraded",
            Some(format!(
                "history queue reached its capacity of {}; records were dropped",
                self.queue_capacity
            )),
        );
        if dropped == 1 || dropped.is_power_of_two() {
            self.status
                .backpressure_diagnostic_pending
                .store(true, Ordering::Release);
        }
    }

    pub(crate) fn flush(&self) -> Result<(), String> {
        let (done_tx, done_rx) = mpsc::channel();
        // This send is non-blocking because the command channel is unbounded.
        // FIFO ordering still makes the acknowledgement a durable barrier for
        // every draft that was enqueued before it.
        self.tx
            .send(WriterCommand::Flush(done_tx))
            .map_err(|_| "history writer channel is closed".to_string())?;
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "timed out flushing history".to_string())?
    }
}

fn restore_durable_status(conn: &Connection, status: &HistoryStatusInner) {
    let max_id = conn
        .query_row("SELECT COALESCE(MAX(id), 0) FROM records", [], |row| {
            row.get::<_, u64>(0)
        })
        .unwrap_or(0);
    let max_event = conn
        .query_row(
            "SELECT COALESCE(MAX(source_seq), 0) FROM records WHERE source_space = 'event'",
            [],
            |row| row.get::<_, u64>(0),
        )
        .unwrap_or(0);
    let max_work = conn
        .query_row(
            "SELECT COALESCE(MAX(source_seq), 0) FROM records WHERE source_space = 'work'",
            [],
            |row| row.get::<_, u64>(0),
        )
        .unwrap_or(0);
    let read_meta = |key: &str| {
        conn.query_row("SELECT value FROM meta WHERE key = ?", [key], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    };
    let last_id = read_meta("last_id").unwrap_or(max_id);
    let last_event = read_meta("last_event_seq").unwrap_or(max_event);
    let last_work = read_meta("last_work_seq").unwrap_or(max_work);
    let last_commit = read_meta("last_commit_at_unix_ms").unwrap_or(0);
    status.last_id.store(last_id, Ordering::Relaxed);
    status.last_event_seq.store(last_event, Ordering::Relaxed);
    status.last_work_seq.store(last_work, Ordering::Relaxed);
    status.last_commit_at.store(last_commit, Ordering::Relaxed);
}

impl Drop for WriterClient {
    fn drop(&mut self) {
        let _ = self.tx.send(WriterCommand::Shutdown);
        if let Ok(mut join) = self.join.lock() {
            if let Some(join) = join.take() {
                let _ = join.join();
            }
        }
    }
}

fn open_with_recovery(path: &Path) -> Result<(Connection, Option<String>), String> {
    match super::schema::open_and_migrate(path) {
        Ok(conn) => {
            super::schema::secure_sidecars(path)?;
            Ok((conn, None))
        }
        Err(error) if looks_corrupt(&error) && path.exists() => {
            let quarantine = PathBuf::from(format!(
                "{}.corrupt-{}",
                path.display(),
                super::now_unix_ms()
            ));
            std::fs::rename(path, &quarantine).map_err(|rename_error| {
                format!(
                    "history is corrupt and could not be quarantined at {}: {rename_error}; {error}",
                    quarantine.display()
                )
            })?;
            for sidecar in super::sidecar_paths(path) {
                let _ = std::fs::remove_file(sidecar);
            }
            let conn = super::schema::open_and_migrate(path).map_err(|retry_error| {
                format!(
                    "history was quarantined at {} but reinitialization failed: {retry_error}",
                    quarantine.display()
                )
            })?;
            super::schema::secure_sidecars(path)?;
            Ok((
                conn,
                Some(format!(
                    "corrupt history was quarantined at {}; a fresh database was initialized",
                    quarantine.display()
                )),
            ))
        }
        Err(error) => Err(error),
    }
}

fn looks_corrupt(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("not a database") || lower.contains("database disk image is malformed")
}

fn run_writer(
    path: PathBuf,
    config: HistoryConfig,
    status: Arc<HistoryStatusInner>,
    rx: mpsc::Receiver<WriterCommand>,
) {
    let (mut conn, recovery_reason) = match open_with_recovery(&path) {
        Ok(opened) => opened,
        Err(error) => {
            status.available.store(false, Ordering::Release);
            status.set_state("degraded", Some(error.clone()));
            run_unavailable_writer(&status, &rx, config.queue_capacity.max(1), error);
            return;
        }
    };
    status.available.store(true, Ordering::Release);
    status
        .schema_version
        .store(HISTORY_SCHEMA_VERSION, Ordering::Relaxed);
    restore_durable_status(&conn, &status);
    if let Some(reason) = recovery_reason {
        status.set_state("degraded", Some(reason));
    } else {
        status.set_state("ok", None);
    }

    let maintenance_interval =
        Duration::from_secs(config.maintenance_interval_minutes.saturating_mul(60));
    let mut maintenance = super::maintenance::MaintenanceState::due_now();
    run_maintenance(
        &mut conn,
        &path,
        &config,
        &status,
        &mut maintenance,
        maintenance_interval,
    );
    let mut deferred = VecDeque::new();
    let mut last_error: Option<String> = None;
    let mut committed_batches = 0_u64;
    loop {
        // Check the explicit batch trigger before receiving another command.
        // A perpetually non-empty channel must not starve maintenance by
        // making every zero-duration recv_timeout return a draft.
        if maintenance.is_due() {
            run_maintenance(
                &mut conn,
                &path,
                &config,
                &status,
                &mut maintenance,
                maintenance_interval,
            );
            committed_batches = 0;
        }
        let command = if let Some(command) = deferred.pop_front() {
            Some(command)
        } else {
            match rx.recv_timeout(maintenance.wait_duration()) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    run_maintenance(
                        &mut conn,
                        &path,
                        &config,
                        &status,
                        &mut maintenance,
                        maintenance_interval,
                    );
                    committed_batches = 0;
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        match command {
            Some(WriterCommand::Draft(first)) => {
                let mut drafts = vec![*first];
                let deadline = Instant::now() + Duration::from_millis(200);
                while drafts.len() < MAX_BATCH {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    match rx.recv_timeout(remaining) {
                        Ok(WriterCommand::Draft(draft)) => {
                            drafts.push(*draft);
                        }
                        Ok(other) => {
                            deferred.push_back(other);
                            break;
                        }
                        Err(
                            mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected,
                        ) => {
                            break;
                        }
                    }
                }
                let pending_count = drafts.len();
                let draft_count = u64::try_from(pending_count).unwrap_or(u64::MAX);
                match persist_batch(&mut conn, &path, drafts, &status) {
                    Ok(()) => {
                        committed_batches = committed_batches.saturating_add(1);
                        if committed_batches >= MAINTENANCE_AFTER_BATCHES {
                            maintenance.make_due();
                        }
                        last_error = None;
                        status.available.store(true, Ordering::Release);
                        status.set_state("ok", None);
                    }
                    Err(error) => {
                        status.dropped.fetch_add(draft_count, Ordering::Relaxed);
                        if error.contains("database is corrupt") {
                            status.available.store(false, Ordering::Release);
                        }
                        status.set_state("degraded", Some(error.clone()));
                        last_error = Some(error);
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
                finish_pending(&status, pending_count);
                report_backpressure(&status, config.queue_capacity.max(1));
            }
            Some(WriterCommand::Flush(done)) => {
                report_backpressure(&status, config.queue_capacity.max(1));
                let result = last_error.clone().map_or(Ok(()), Err);
                let _ = done.send(result);
            }
            Some(WriterCommand::Shutdown) | None => break,
        }
    }
    status.available.store(false, Ordering::Release);
}

fn run_maintenance(
    conn: &mut Connection,
    path: &Path,
    config: &HistoryConfig,
    status: &HistoryStatusInner,
    state: &mut super::maintenance::MaintenanceState,
    interval: Duration,
) {
    if !state.is_due() {
        return;
    }
    let started_at = super::now_unix_ms();
    let started = Instant::now();
    let was_failed = state.consecutive_failures > 0;
    let storage_before = super::maintenance::storage_snapshot(conn, path).ok();
    let conversion_error = super::schema::ensure_incremental_autovacuum(conn, path).err();
    let result = super::maintenance::run_pass(conn, path, config);
    let duration_ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(mut outcome) if conversion_error.is_none() => {
            outcome.duration_ms = duration_ms;
            state.succeeded(interval, outcome.pending);
            update_maintenance_success(status, state, started_at, &outcome);
            if was_failed {
                crate::diagnostics::record_history_maintenance(
                    crate::diagnostics::DiagnosticLevel::Info,
                    "recovered",
                    "history maintenance recovered",
                    Some(serde_json::json!({
                        "rows_removed": outcome.rows_removed(),
                        "duration_ms": outcome.duration_ms,
                    })),
                );
            }
        }
        Ok(mut outcome) => {
            outcome.duration_ms = duration_ms;
            let error = conversion_error.unwrap_or_else(|| "history maintenance failed".into());
            state.failed(interval);
            update_maintenance_failure(
                status,
                state,
                started_at,
                outcome.duration_ms,
                Some(&outcome),
                &error,
            );
            if !was_failed {
                report_maintenance_failure(&error, state.consecutive_failures);
            }
        }
        Err(error) => {
            let error = conversion_error
                .map(|conversion| format!("{conversion}; {error}"))
                .unwrap_or(error);
            state.failed(interval);
            let failure_outcome = maintenance_failure_outcome(
                conn,
                path,
                config,
                storage_before.as_ref(),
                duration_ms,
            );
            update_maintenance_failure(
                status,
                state,
                started_at,
                duration_ms,
                failure_outcome.as_ref(),
                &error,
            );
            if !was_failed {
                report_maintenance_failure(&error, state.consecutive_failures);
            }
        }
    }
}

fn maintenance_failure_outcome(
    conn: &Connection,
    path: &Path,
    config: &HistoryConfig,
    storage_before: Option<&super::HistoryStorageStatus>,
    duration_ms: u64,
) -> Option<super::maintenance::MaintenanceOutcome> {
    let mut storage = super::maintenance::storage_snapshot(conn, path).ok()?;
    storage.soft_max_bytes = config.max_bytes;
    let rows_removed = storage_before
        .map(|before| before.record_count.saturating_sub(storage.record_count))
        .unwrap_or(0);
    Some(super::maintenance::MaintenanceOutcome {
        // A failed pass may have crossed multiple retention stages before the
        // failing operation. The public metric is the total, so preserve that
        // accurately without pretending the stage breakdown is known.
        age_rows_removed: rows_removed,
        count_rows_removed: 0,
        size_rows_removed: 0,
        duration_ms,
        pending: true,
        storage,
    })
}

fn update_maintenance_success(
    status: &HistoryStatusInner,
    state: &super::maintenance::MaintenanceState,
    started_at: u64,
    outcome: &super::maintenance::MaintenanceOutcome,
) {
    if let Ok(mut storage) = status.storage.lock() {
        *storage = outcome.storage.clone();
    }
    if let Ok(mut maintenance) = status.maintenance.lock() {
        let removed = outcome.rows_removed();
        maintenance.last_run_at_unix_ms = Some(started_at);
        maintenance.next_run_at_unix_ms = Some(state.next_run_at_unix_ms());
        maintenance.last_result = if outcome.pending { "pending" } else { "ok" }.to_string();
        maintenance.rows_removed_last = removed;
        maintenance.rows_removed_total = maintenance.rows_removed_total.saturating_add(removed);
        maintenance.duration_ms = outcome.duration_ms;
        maintenance.consecutive_failures = 0;
        maintenance.last_error = None;
    }
}

fn update_maintenance_failure(
    status: &HistoryStatusInner,
    state: &super::maintenance::MaintenanceState,
    started_at: u64,
    duration_ms: u64,
    outcome: Option<&super::maintenance::MaintenanceOutcome>,
    error: &str,
) {
    if let Some(outcome) = outcome {
        if let Ok(mut storage) = status.storage.lock() {
            *storage = outcome.storage.clone();
        }
    }
    if let Ok(mut maintenance) = status.maintenance.lock() {
        let removed = outcome.map_or(0, super::maintenance::MaintenanceOutcome::rows_removed);
        maintenance.last_run_at_unix_ms = Some(started_at);
        maintenance.next_run_at_unix_ms = Some(state.next_run_at_unix_ms());
        maintenance.last_result = "failed".to_string();
        maintenance.rows_removed_last = removed;
        maintenance.rows_removed_total = maintenance.rows_removed_total.saturating_add(removed);
        maintenance.duration_ms = duration_ms;
        maintenance.consecutive_failures = state.consecutive_failures;
        maintenance.last_error = Some(error.to_string());
    }
}

fn report_maintenance_failure(error: &str, consecutive_failures: u32) {
    crate::diagnostics::record_history_maintenance(
        crate::diagnostics::DiagnosticLevel::Warn,
        "failed",
        format!("history maintenance failed and will retry with backoff: {error}"),
        Some(serde_json::json!({
            "consecutive_failures": consecutive_failures,
        })),
    );
}

fn run_unavailable_writer(
    status: &HistoryStatusInner,
    rx: &mpsc::Receiver<WriterCommand>,
    queue_capacity: usize,
    error: String,
) {
    while let Ok(command) = rx.recv() {
        match command {
            WriterCommand::Draft(_) => {
                finish_pending(status, 1);
                status.dropped.fetch_add(1, Ordering::Relaxed);
                report_backpressure(status, queue_capacity);
            }
            WriterCommand::Flush(done) => {
                report_backpressure(status, queue_capacity);
                let _ = done.send(Err(error.clone()));
            }
            WriterCommand::Shutdown => break,
        }
    }
}

fn report_backpressure(status: &HistoryStatusInner, queue_capacity: usize) {
    if !status
        .backpressure_diagnostic_pending
        .swap(false, Ordering::AcqRel)
    {
        return;
    }
    let dropped = status.dropped.load(Ordering::Relaxed);
    crate::diagnostics::record_history_backpressure(
        format!("history queue overflowed; dropped {dropped} records"),
        Some(serde_json::json!({
            "dropped_total": dropped,
            "queue_capacity": queue_capacity,
        })),
    );
}

fn finish_pending(status: &HistoryStatusInner, count: usize) {
    let previous = status.queue_depth.fetch_sub(count, Ordering::AcqRel);
    debug_assert!(previous >= count);
    if previous <= count {
        status.oldest_queued_at.store(0, Ordering::Release);
        if status.queue_depth.load(Ordering::Acquire) > 0
            && status.oldest_queued_at.load(Ordering::Acquire) == 0
        {
            status
                .oldest_queued_at
                .store(super::now_unix_ms(), Ordering::Release);
        }
    }
}

fn persist_batch(
    conn: &mut Connection,
    path: &Path,
    drafts: Vec<HistoryRecordDraft>,
    status: &HistoryStatusInner,
) -> Result<(), String> {
    let records = drafts
        .into_iter()
        .filter_map(super::sanitize::sanitize)
        .collect::<Vec<_>>();
    if records.is_empty() {
        return Ok(());
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(classify_sql_error)?;
    let mut durable_event = status.last_event_seq.load(Ordering::Relaxed);
    let mut durable_work = status.last_work_seq.load(Ordering::Relaxed);
    let mut durable_id = status.last_id.load(Ordering::Relaxed);
    {
        let mut statement = tx
            .prepare_cached(
                "INSERT INTO records (ts_unix_ms, record_type, subtype, source_space, source_seq, session, workspace_origin, tab_origin, pane_origin, task_id, repository, authority, verification, level, summary, attrs) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .map_err(classify_sql_error)?;
        for record in records {
            insert_record(&mut statement, &record)?;
            durable_id = u64::try_from(tx.last_insert_rowid()).unwrap_or(durable_id);
            match (record.source_space.as_deref(), record.source_seq) {
                (Some("event"), Some(seq)) => durable_event = durable_event.max(seq),
                (Some("work"), Some(seq)) => durable_work = durable_work.max(seq),
                _ => {}
            }
        }
    }
    let committed_at = super::now_unix_ms();
    for (key, value) in [
        ("last_commit_at_unix_ms", committed_at),
        ("last_event_seq", durable_event),
        ("last_work_seq", durable_work),
        ("last_id", durable_id),
    ] {
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value.to_string()],
        )
        .map_err(classify_sql_error)?;
    }
    tx.commit().map_err(classify_sql_error)?;
    super::schema::secure_sidecars(path)?;
    status
        .last_event_seq
        .store(durable_event, Ordering::Relaxed);
    status.last_work_seq.store(durable_work, Ordering::Relaxed);
    status.last_id.store(durable_id, Ordering::Relaxed);
    status.last_commit_at.store(committed_at, Ordering::Release);
    Ok(())
}

fn insert_record(
    statement: &mut rusqlite::CachedStatement<'_>,
    record: &HistoryRecord,
) -> Result<(), String> {
    let attrs = record
        .attrs
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| format!("could not serialize sanitized history attributes: {error}"))?;
    statement
        .execute(params![
            record.ts_unix_ms,
            record.record_type,
            record.subtype,
            record.source_space,
            record.source_seq,
            record.session,
            record.workspace_origin,
            record.tab_origin,
            record.pane_origin,
            record.task_id,
            record.repository,
            record.authority,
            record.verification,
            record.level,
            record.summary,
            attrs,
        ])
        .map_err(classify_sql_error)?;
    Ok(())
}

fn classify_sql_error(error: SqlError) -> String {
    match &error {
        SqlError::SqliteFailure(code, _)
            if matches!(
                code.code,
                ErrorCode::DiskFull | ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
            ) =>
        {
            format!("history storage temporarily unavailable: {error}")
        }
        SqlError::SqliteFailure(code, _)
            if matches!(
                code.code,
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
            ) =>
        {
            format!("history database is corrupt: {error}")
        }
        _ => format!("history write failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{DiagnosticLevel, DiagnosticRecord};
    use crate::events::EventRecord;
    use crate::history::{HistoryFilters, HistoryHandle};
    use crate::work_ledger::{
        EvidenceSource, VerificationState, WorkAuthority, WorkIdentity, WorkKind, WorkRecord,
    };
    use std::os::unix::fs::PermissionsExt;

    fn temp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "taarof-history-{name}-{}-{nanos}",
                std::process::id()
            ))
            .join("history.sqlite3")
    }

    fn enabled_config() -> HistoryConfig {
        HistoryConfig {
            enabled: true,
            queue_capacity: 64,
            ..HistoryConfig::default()
        }
    }

    #[test]
    fn records_survive_restart_and_files_are_private() {
        let path = temp_path("restart");
        let (handle, reader) = HistoryHandle::open_at(enabled_config(), path.clone());
        handle.try_record_event(&EventRecord {
            seq: 9,
            ts_unix_ms: super::super::now_unix_ms(),
            event_type: "session_started".to_string(),
            payload: serde_json::json!({"session_name": "test"}),
        });
        handle.try_record_diagnostic(&DiagnosticRecord {
            ts_unix_ms: super::super::now_unix_ms(),
            level: DiagnosticLevel::Warn,
            category: "probe_failure".to_string(),
            source: "runtime".to_string(),
            action: "poll".to_string(),
            message: "probe failed safely".to_string(),
            details: Some(serde_json::json!({"state": "degraded"})),
        });
        handle.try_record_work(&WorkRecord {
            seq: 3,
            ts_unix_ms: super::super::now_unix_ms(),
            kind: WorkKind::TaskStatusChanged,
            summary: "EXAMPLE-133 moved to in progress".to_string(),
            identity: WorkIdentity {
                session: "test".to_string(),
                workspace_origin: "workspace-0123456789abcdef".to_string(),
                tab_origin: "tab-0123456789abcdef".to_string(),
                pane_origin: "pane-0123456789abcdef".to_string(),
                workspace_id: 1,
                workspace_name: "test".to_string(),
                tab_id: 2,
                tab_name: "test".to_string(),
                pane_id: 3,
                task_id: Some("EXAMPLE-133".to_string()),
                task_title: None,
            },
            evidence_source: EvidenceSource::PlanFile,
            authority: WorkAuthority::PlanCanonical,
            verification: VerificationState::CanonicalFile,
            task_status: Some("in_progress".to_string()),
            pull_request: None,
        });
        handle.flush().unwrap();
        let page = reader.query(None, None, HistoryFilters::default()).unwrap();
        assert_eq!(page.records.len(), 3);
        assert_eq!(page.records[0].source_seq, Some(9));
        assert_eq!(page.records[2].task_id.as_deref(), Some("EXAMPLE-133"));
        assert_eq!(page.records[2].authority.as_deref(), Some("plan_canonical"));
        assert_eq!(handle.status().last_durable_seq.work, 3);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for sidecar in crate::history::sidecar_paths(&path) {
            if sidecar.exists() {
                assert_eq!(
                    std::fs::metadata(sidecar).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        drop(handle);
        let (reopened, reopened_reader) = HistoryHandle::open_at(enabled_config(), path.clone());
        reopened.flush().unwrap();
        assert_eq!(reopened.status().last_durable_seq.event, 9);
        assert_eq!(reopened.status().last_durable_seq.work, 3);
        assert!(reopened.status().last_commit_at_unix_ms.is_some());
        assert_eq!(reopened.status().maintenance.last_result, "ok");
        assert_eq!(reopened.status().storage.record_count, 3);
        assert_eq!(
            reopened_reader
                .query(None, None, HistoryFilters::default())
                .unwrap()
                .records
                .len(),
            3
        );
        drop(reopened);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn restart_resumes_maintenance_without_losing_durable_sequence() {
        let path = temp_path("restart-maintenance");
        let mut config = enabled_config();
        config.max_age_days = 1;
        let (handle, _) = HistoryHandle::open_at(config.clone(), path.clone());
        let now = super::super::now_unix_ms();
        for (seq, ts) in [(1, now.saturating_sub(2 * 24 * 60 * 60 * 1_000)), (2, now)] {
            handle.try_record_event(&EventRecord {
                seq,
                ts_unix_ms: ts,
                event_type: "session_started".to_string(),
                payload: serde_json::json!({"session_name": "restart"}),
            });
        }
        handle.flush().unwrap();
        assert_eq!(handle.status().last_durable_seq.event, 2);
        drop(handle);

        let (reopened, reader) = HistoryHandle::open_at(config, path.clone());
        reopened.flush().unwrap();
        let maintenance_status = reopened.status();
        assert_eq!(
            maintenance_status.maintenance.last_result, "ok",
            "{maintenance_status:?}"
        );
        let records = reader
            .query(None, None, HistoryFilters::default())
            .unwrap()
            .records;
        assert_eq!(records.len(), 1, "{maintenance_status:?}");
        assert_eq!(records[0].source_seq, Some(2));
        assert_eq!(reopened.status().last_durable_seq.event, 2);
        drop(reopened);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn failed_checkpoint_reports_rows_already_removed_and_current_storage() {
        let path = temp_path("checkpoint-failure-metrics");
        let mut conn = super::super::schema::open_and_migrate(&path).unwrap();
        super::super::schema::ensure_incremental_autovacuum(&conn, &path).unwrap();
        let expired = super::super::now_unix_ms().saturating_sub(2 * 24 * 60 * 60 * 1_000);
        conn.execute(
            "INSERT INTO records (ts_unix_ms, record_type, subtype, session, summary) VALUES (?1, 'diagnostic', 'test', 'test', 'expired')",
            [expired],
        )
        .unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();

        let reader = Connection::open(&path).unwrap();
        super::super::schema::configure_connection(&reader).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        assert_eq!(
            reader
                .query_row("SELECT COUNT(*) FROM records", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            1
        );

        let mut config = enabled_config();
        config.max_age_days = 1;
        let status = HistoryStatusInner::new(true);
        let mut state = super::super::maintenance::MaintenanceState::due_now();
        run_maintenance(
            &mut conn,
            &path,
            &config,
            &status,
            &mut state,
            Duration::from_secs(60),
        );

        let snapshot = status.snapshot();
        assert_eq!(snapshot.maintenance.last_result, "failed");
        assert_eq!(snapshot.maintenance.rows_removed_last, 1);
        assert_eq!(snapshot.maintenance.rows_removed_total, 1);
        assert_eq!(snapshot.storage.record_count, 0);
        assert_eq!(snapshot.storage.soft_max_bytes, config.max_bytes);
        assert!(snapshot
            .maintenance
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("reader is busy")));

        reader.execute_batch("ROLLBACK").unwrap();
        drop(reader);
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn bounded_queue_never_blocks_producer() {
        let path = temp_path("bounded");
        let mut config = enabled_config();
        config.queue_capacity = 64;
        let (handle, _) = HistoryHandle::open_at(config, path.clone());
        let started = Instant::now();
        for seq in 1..10_000 {
            handle.try_record_event(&EventRecord {
                seq,
                ts_unix_ms: seq,
                event_type: "session_started".to_string(),
                payload: serde_json::json!({"session_name": "test"}),
            });
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(handle.status().dropped > 0);
        drop(handle);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn locked_startup_does_not_block_open_or_producer() {
        let path = temp_path("locked-startup");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lock = Connection::open(&path).unwrap();
        super::super::schema::configure_connection(&lock).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = Instant::now();
        let (handle, _) = HistoryHandle::open_at(enabled_config(), path.clone());
        assert!(started.elapsed() < Duration::from_millis(500));
        let emitted = Instant::now();
        handle.try_record_event(&EventRecord {
            seq: 1,
            ts_unix_ms: super::super::now_unix_ms(),
            event_type: "session_started".to_string(),
            payload: serde_json::json!({"session_name": "test"}),
        });
        assert!(emitted.elapsed() < Duration::from_millis(100));

        lock.execute_batch("ROLLBACK").unwrap();
        handle.flush().unwrap();
        assert_eq!(handle.status().last_durable_seq.event, 1);
        drop(handle);
        drop(lock);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_database_is_quarantined_and_reinitialized() {
        let path = temp_path("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not sqlite").unwrap();
        let (handle, _) = HistoryHandle::open_at(enabled_config(), path.clone());
        handle.flush().unwrap();
        assert!(handle.status().available);
        assert_eq!(handle.status().state, "degraded");
        let quarantined = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"));
        assert!(quarantined);
        drop(handle);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unavailable_storage_degrades_asynchronously_and_counts_drafts() {
        let parent = temp_path("unavailable");
        std::fs::create_dir_all(parent.parent().unwrap()).unwrap();
        std::fs::write(&parent, b"not a directory").unwrap();
        let path = parent.join("history.sqlite3");

        let started = Instant::now();
        let (handle, reader) = HistoryHandle::open_at(enabled_config(), path);
        assert!(started.elapsed() < Duration::from_millis(500));
        handle.try_record_event(&EventRecord {
            seq: 1,
            ts_unix_ms: super::super::now_unix_ms(),
            event_type: "session_started".to_string(),
            payload: serde_json::json!({"session_name": "test"}),
        });
        assert!(handle.flush().is_err());
        let status = handle.status();
        assert!(!status.available);
        assert_eq!(status.state, "degraded");
        assert_eq!(status.dropped, 1);
        assert_eq!(status.queue_depth, 0);
        assert!(reader.query(None, None, HistoryFilters::default()).is_err());

        drop(handle);
        let _ = std::fs::remove_dir_all(parent.parent().unwrap());
    }

    #[test]
    fn disk_full_is_classified_as_temporary_storage_degradation() {
        let error = SqlError::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            Some("database or disk is full".to_string()),
        );

        assert!(classify_sql_error(error).contains("temporarily unavailable"));
    }

    #[test]
    fn sensitive_scalar_bytes_never_reach_database_or_sidecars() {
        let path = temp_path("sanitize-raw");
        let (handle, _) = HistoryHandle::open_at(enabled_config(), path.clone());
        let secrets = [
            "Bearer abcdefghijklmnopqrstuvwxyz",
            "password=hunter2",
            "postgresql://user:pass@example.test/db",
            "\u{1b}[31mterminal output",
            "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
            "ordinary-environment-value-1337",
            "ssh-private-material-0123456789abcdef",
        ];
        // The last two values are ordinary scalars that become sensitive only
        // in terminal/environment fields. Do not put them in the diagnostic
        // message field, where ordinary bounded text is intentionally allowed.
        for secret in &secrets[..5] {
            handle.try_record_diagnostic(&DiagnosticRecord {
                ts_unix_ms: super::super::now_unix_ms(),
                level: DiagnosticLevel::Error,
                category: "command_failure".to_string(),
                source: "runtime".to_string(),
                action: "test".to_string(),
                message: (*secret).to_string(),
                details: Some(serde_json::json!({"argv": *secret, "exit_code": 1})),
            });
        }
        handle.try_record_event(&EventRecord {
            seq: 99,
            ts_unix_ms: super::super::now_unix_ms(),
            event_type: "session_started".to_string(),
            payload: serde_json::json!({
                "session_name": "test",
                "typed_input": secrets[5],
                "terminal_body": secrets[6],
            }),
        });
        handle.try_record_diagnostic(&DiagnosticRecord {
            ts_unix_ms: super::super::now_unix_ms(),
            level: DiagnosticLevel::Error,
            category: "command_failure".to_string(),
            source: "runtime".to_string(),
            action: "test".to_string(),
            message: "bounded failure summary".to_string(),
            details: Some(serde_json::json!({
                "environment": {"TAAROF_TEST": secrets[5]},
                "ssh_secret": secrets[6],
            })),
        });
        handle.try_record_work(&WorkRecord {
            seq: 100,
            ts_unix_ms: super::super::now_unix_ms(),
            kind: WorkKind::AssistantMessageCompleted,
            summary: secrets[5].to_string(),
            identity: WorkIdentity {
                session: "test".to_string(),
                workspace_origin: "workspace-0123456789abcdef".to_string(),
                tab_origin: "tab-0123456789abcdef".to_string(),
                pane_origin: "pane-0123456789abcdef".to_string(),
                workspace_id: 1,
                workspace_name: "test".to_string(),
                tab_id: 2,
                tab_name: "test".to_string(),
                pane_id: 3,
                task_id: Some("EXAMPLE-133".to_string()),
                task_title: None,
            },
            evidence_source: EvidenceSource::AgentReport,
            authority: WorkAuthority::AgentObservation,
            verification: VerificationState::Observed,
            task_status: None,
            pull_request: None,
        });
        handle.flush().unwrap();
        drop(handle);
        let mut raw = std::fs::read(&path).unwrap();
        for sidecar in crate::history::sidecar_paths(&path) {
            if let Ok(bytes) = std::fs::read(sidecar) {
                raw.extend(bytes);
            }
        }
        for secret in secrets {
            assert!(
                !raw.windows(secret.len())
                    .any(|window| window == secret.as_bytes()),
                "secret reached SQLite bytes: {secret:?}"
            );
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn disabled_history_creates_no_file() {
        let path = temp_path("disabled");
        let (handle, reader) = HistoryHandle::open_at(HistoryConfig::default(), path.clone());
        handle.try_record_event(&EventRecord {
            seq: 1,
            ts_unix_ms: 1,
            event_type: "session_started".to_string(),
            payload: serde_json::json!({"session_name": "test"}),
        });
        assert!(!handle.status().enabled);
        assert!(reader.query(None, None, HistoryFilters::default()).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn direct_open_rejects_invalid_history_configuration() {
        let path = temp_path("invalid-config");
        let config = HistoryConfig {
            enabled: true,
            queue_capacity: 1,
            ..HistoryConfig::default()
        };
        let (handle, reader) = HistoryHandle::open_at(config, path.clone());
        assert_eq!(handle.status().state, "misconfigured");
        assert!(handle
            .status()
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("history.queue_capacity is 1")));
        assert!(!path.exists());
        assert!(reader.query(None, None, HistoryFilters::default()).is_err());
    }
}
