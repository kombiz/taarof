use super::{HistoryConfig, HistoryStorageStatus};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;
use std::time::{Duration, Instant};

const PASS_BUDGET: Duration = Duration::from_secs(2);
const DELETE_CHUNK: u64 = 2_000;
const VACUUM_PAGE_CHUNK: u64 = 512;
const VACUUM_THRESHOLD_PAGES: u64 = 128;
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);

pub(crate) struct MaintenanceState {
    pub(crate) next_attempt: Instant,
    pub(crate) consecutive_failures: u32,
}

impl MaintenanceState {
    pub(crate) fn due_now() -> Self {
        Self {
            next_attempt: Instant::now(),
            consecutive_failures: 0,
        }
    }

    pub(crate) fn is_due(&self) -> bool {
        Instant::now() >= self.next_attempt
    }

    pub(crate) fn wait_duration(&self) -> Duration {
        self.next_attempt.saturating_duration_since(Instant::now())
    }

    pub(crate) fn make_due(&mut self) {
        // Batch volume may advance a healthy cadence, but must never defeat a
        // failure backoff under sustained write traffic.
        if self.consecutive_failures == 0 {
            self.next_attempt = Instant::now();
        }
    }

    pub(crate) fn succeeded(&mut self, interval: Duration, pending: bool) {
        self.consecutive_failures = 0;
        self.next_attempt = Instant::now()
            + if pending {
                interval.min(Duration::from_secs(60))
            } else {
                interval
            };
    }

    pub(crate) fn failed(&mut self, interval: Duration) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let exponent = self.consecutive_failures.min(16);
        let multiplier = 1_u64 << exponent;
        let seconds = interval
            .as_secs()
            .saturating_mul(multiplier)
            .min(MAX_BACKOFF.as_secs());
        self.next_attempt = Instant::now() + Duration::from_secs(seconds.max(1));
    }

    pub(crate) fn next_run_at_unix_ms(&self) -> u64 {
        super::now_unix_ms().saturating_add(
            self.next_attempt
                .saturating_duration_since(Instant::now())
                .as_millis() as u64,
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MaintenanceOutcome {
    pub(crate) age_rows_removed: u64,
    pub(crate) count_rows_removed: u64,
    pub(crate) size_rows_removed: u64,
    pub(crate) duration_ms: u64,
    pub(crate) pending: bool,
    pub(crate) storage: HistoryStorageStatus,
}

impl MaintenanceOutcome {
    pub(crate) fn rows_removed(&self) -> u64 {
        self.age_rows_removed
            .saturating_add(self.count_rows_removed)
            .saturating_add(self.size_rows_removed)
    }
}

pub(crate) fn run_pass(
    conn: &mut Connection,
    path: &Path,
    config: &HistoryConfig,
) -> Result<MaintenanceOutcome, String> {
    let started = Instant::now();
    let deadline = started + PASS_BUDGET;

    let mut age_rows_removed = 0_u64;
    let mut count_rows_removed = 0_u64;
    let mut size_rows_removed = 0_u64;
    let mut budget_hit = false;

    if config.max_age_days > 0 {
        let cutoff = super::now_unix_ms()
            .saturating_sub(config.max_age_days.saturating_mul(24 * 60 * 60 * 1_000));
        loop {
            if Instant::now() >= deadline {
                budget_hit = true;
                break;
            }
            let removed = delete_chunk(
                conn,
                "DELETE FROM records WHERE id IN (SELECT id FROM records WHERE ts_unix_ms < ?1 ORDER BY ts_unix_ms, id LIMIT ?2)",
                params![cutoff, DELETE_CHUNK],
            )?;
            age_rows_removed = age_rows_removed.saturating_add(removed);
            if removed < DELETE_CHUNK {
                break;
            }
        }
    }

    if !budget_hit && config.max_records > 0 {
        let offset = config.max_records.saturating_sub(1);
        // Resolve the retained boundary once per pass. Re-running the large
        // OFFSET for every delete chunk made a single pass O(chunks * offset).
        let retained_boundary = conn
            .query_row(
                "SELECT id FROM records ORDER BY id DESC LIMIT 1 OFFSET ?1",
                [offset],
                |row| row.get::<_, u64>(0),
            )
            .optional()
            .map_err(|error| {
                format!("could not resolve history count-retention boundary: {error}")
            })?;
        while let Some(retained_boundary) = retained_boundary {
            if Instant::now() >= deadline {
                budget_hit = true;
                break;
            }
            let removed = delete_chunk(
                conn,
                "DELETE FROM records WHERE id IN (SELECT id FROM records WHERE id < ?1 ORDER BY id LIMIT ?2)",
                params![retained_boundary, DELETE_CHUNK],
            )?;
            count_rows_removed = count_rows_removed.saturating_add(removed);
            if removed < DELETE_CHUNK {
                break;
            }
        }
    }

    let retained_floor = if config.max_records == 0 {
        1_000
    } else {
        config.max_records.min(1_000)
    };
    if !budget_hit && config.max_bytes > 0 {
        let mut record_count = storage_snapshot(conn, path)?.record_count;
        loop {
            if Instant::now() >= deadline {
                budget_hit = true;
                break;
            }
            let page_count: u64 = conn
                .pragma_query_value(None, "page_count", |row| row.get(0))
                .map_err(|error| format!("could not read history page count: {error}"))?;
            let free_pages: u64 = conn
                .pragma_query_value(None, "freelist_count", |row| row.get(0))
                .map_err(|error| format!("could not read history free pages: {error}"))?;
            let page_size: u64 = conn
                .pragma_query_value(None, "page_size", |row| row.get(0))
                .map_err(|error| format!("could not read history page size: {error}"))?;
            // Physical main/WAL bytes do not fall until checkpoint/vacuum. Use
            // live pages here so a tiny overage cannot repeatedly delete rows
            // all the way to the retained floor merely because reclamation has
            // not run yet. WAL bytes are also excluded: checkpointing can
            // remove them without deleting history.
            let live_bytes = page_count
                .saturating_sub(free_pages)
                .saturating_mul(page_size);
            if live_bytes <= config.max_bytes || record_count <= retained_floor {
                break;
            }
            let average_row_bytes = live_bytes
                .saturating_add(record_count.saturating_sub(1))
                .checked_div(record_count)
                .unwrap_or(1)
                .max(1);
            let overage = live_bytes.saturating_sub(config.max_bytes);
            let rows_for_overage = overage
                .saturating_add(average_row_bytes.saturating_sub(1))
                .checked_div(average_row_bytes)
                .unwrap_or(1)
                .max(1);
            let limit = DELETE_CHUNK
                .min(rows_for_overage)
                .min(record_count.saturating_sub(retained_floor));
            let removed = delete_chunk(
                conn,
                "DELETE FROM records WHERE id IN (SELECT id FROM records ORDER BY ts_unix_ms, id LIMIT ?1)",
                params![limit],
            )?;
            size_rows_removed = size_rows_removed.saturating_add(removed);
            if removed == 0 {
                break;
            }
            record_count = record_count.saturating_sub(removed);
        }
    }

    checkpoint_truncate(conn)?;
    let mut free_pages: u64 = conn
        .pragma_query_value(None, "freelist_count", |row| row.get(0))
        .map_err(|error| format!("could not read history free pages: {error}"))?;
    let physical_bytes = std::fs::metadata(path)
        .map(|value| value.len())
        .unwrap_or(0)
        .saturating_add(
            std::fs::metadata(&super::sidecar_paths(path)[0])
                .map(|value| value.len())
                .unwrap_or(0),
        );
    if free_pages >= VACUUM_THRESHOLD_PAGES
        || (config.max_bytes > 0 && physical_bytes > config.max_bytes && free_pages > 0)
    {
        let mut page_count: u64 = conn
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .map_err(|error| format!("could not read history page count: {error}"))?;
        let target = page_count.saturating_sub(VACUUM_PAGE_CHUNK.min(free_pages));
        while page_count > target && free_pages > 0 {
            if Instant::now() >= deadline {
                budget_hit = true;
                break;
            }
            // Some SQLite builds reclaim only the immediately-truncatable tail
            // page per invocation even when N is larger. Repeating the bounded
            // pragma preserves the 512-page cap across both behaviours.
            conn.execute_batch("PRAGMA incremental_vacuum(512)")
                .map_err(|error| format!("could not incrementally vacuum history: {error}"))?;
            let next_page_count: u64 = conn
                .pragma_query_value(None, "page_count", |row| row.get(0))
                .map_err(|error| format!("could not read history page count: {error}"))?;
            if next_page_count >= page_count {
                break;
            }
            page_count = next_page_count;
            free_pages = conn
                .pragma_query_value(None, "freelist_count", |row| row.get(0))
                .map_err(|error| format!("could not read history free pages: {error}"))?;
        }
    }
    super::schema::secure_sidecars(path)?;

    let mut storage = storage_snapshot(conn, path)?;
    storage.soft_max_bytes = config.max_bytes;
    let age_pending = config.max_age_days > 0 && has_expired(conn, config.max_age_days)?;
    let count_pending = config.max_records > 0 && storage.record_count > config.max_records;
    let size_pending = config.max_bytes > 0
        && storage.main_bytes.saturating_add(storage.wal_bytes) > config.max_bytes
        && (storage.record_count > retained_floor || storage.free_pages >= VACUUM_THRESHOLD_PAGES);

    Ok(MaintenanceOutcome {
        age_rows_removed,
        count_rows_removed,
        size_rows_removed,
        duration_ms: started.elapsed().as_millis() as u64,
        pending: budget_hit || age_pending || count_pending || size_pending,
        storage,
    })
}

fn delete_chunk<P>(conn: &mut Connection, sql: &str, params: P) -> Result<u64, String>
where
    P: rusqlite::Params,
{
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("could not start history maintenance transaction: {error}"))?;
    let removed = tx
        .execute(sql, params)
        .map_err(|error| format!("history retention delete failed: {error}"))?;
    tx.commit()
        .map_err(|error| format!("could not commit history retention delete: {error}"))?;
    Ok(removed as u64)
}

fn checkpoint_truncate(conn: &Connection) -> Result<(), String> {
    // SQLite reports -1 for the log/checkpoint counts when a checkpoint could
    // not start, so these fields must remain signed even though `busy` is 0/1.
    let (busy, _, _): (i64, i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|error| format!("could not checkpoint history WAL: {error}"))?;
    if busy == 0 {
        Ok(())
    } else {
        Err("could not truncate history WAL because a reader is busy; will retry".to_string())
    }
}

fn has_expired(conn: &Connection, max_age_days: u64) -> Result<bool, String> {
    let cutoff =
        super::now_unix_ms().saturating_sub(max_age_days.saturating_mul(24 * 60 * 60 * 1_000));
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM records WHERE ts_unix_ms < ? LIMIT 1)",
        [cutoff],
        |row| row.get(0),
    )
    .map_err(|error| format!("could not check age retention convergence: {error}"))
}

pub(crate) fn storage_snapshot(
    conn: &Connection,
    path: &Path,
) -> Result<HistoryStorageStatus, String> {
    let page_count = conn
        .pragma_query_value(None, "page_count", |row| row.get::<_, u64>(0))
        .map_err(|error| format!("could not read history page count: {error}"))?;
    let free_pages = conn
        .pragma_query_value(None, "freelist_count", |row| row.get::<_, u64>(0))
        .map_err(|error| format!("could not read history free pages: {error}"))?;
    let (record_count, oldest, newest): (u64, Option<u64>, Option<u64>) = conn
        .query_row(
            "SELECT COUNT(*), MIN(ts_unix_ms), MAX(ts_unix_ms) FROM records",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| format!("could not read history record metrics: {error}"))?;
    let main_bytes = std::fs::metadata(path)
        .map(|value| value.len())
        .unwrap_or(0);
    let wal_path = super::sidecar_paths(path)[0].clone();
    let wal_bytes = std::fs::metadata(wal_path)
        .map(|value| value.len())
        .unwrap_or(0);
    Ok(HistoryStorageStatus {
        main_bytes,
        wal_bytes,
        soft_max_bytes: 0,
        page_count,
        free_pages,
        record_count,
        oldest_record_ts_unix_ms: oldest,
        newest_record_ts_unix_ms: newest,
    })
}

#[cfg(test)]
pub(crate) fn pass_budget() -> Duration {
    PASS_BUDGET
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OpenFlags;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "taarof-maintenance-{name}-{}-{nanos}",
                std::process::id()
            ))
            .join("history.sqlite3")
    }

    fn insert_record(conn: &Connection, ts: u64, summary: &str) {
        conn.execute(
            "INSERT INTO records (ts_unix_ms, record_type, subtype, session, summary) VALUES (?1, 'diagnostic', 'test', 'test', ?2)",
            params![ts, summary],
        )
        .unwrap();
    }

    fn age_only_config() -> HistoryConfig {
        HistoryConfig {
            enabled: true,
            max_age_days: 1,
            max_records: 0,
            max_bytes: 0,
            maintenance_interval_minutes: 1,
            queue_capacity: 64,
            ..HistoryConfig::default()
        }
    }

    #[test]
    fn age_pruning_removes_only_expired_history_and_no_source_of_truth_files() {
        let path = temp_path("age");
        let fixture = path.parent().unwrap();
        let sentinels = [
            fixture.join(".plan/tasks.json"),
            fixture.join("session.json"),
            fixture.join("work-ledger.jsonl"),
            fixture.join("transcript.txt"),
            fixture.join("repo/source.rs"),
        ];
        for (index, sentinel) in sentinels.iter().enumerate() {
            std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
            std::fs::write(sentinel, format!("sentinel-{index}")).unwrap();
        }
        let before = sentinels
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect::<Vec<_>>();

        let mut conn = super::super::schema::open_and_migrate(&path).unwrap();
        let now = super::super::now_unix_ms();
        insert_record(
            &conn,
            now.saturating_sub(2 * 24 * 60 * 60 * 1_000),
            "expired",
        );
        insert_record(&conn, now, "current");
        let outcome = run_pass(&mut conn, &path, &age_only_config()).unwrap();
        assert_eq!(outcome.age_rows_removed, 1);
        assert_eq!(outcome.count_rows_removed, 0);
        assert_eq!(outcome.size_rows_removed, 0);
        assert_eq!(outcome.storage.record_count, 1);
        assert_eq!(
            conn.query_row("SELECT summary FROM records", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "current"
        );
        for (sentinel, expected) in sentinels.iter().zip(before) {
            assert_eq!(std::fs::read(sentinel).unwrap(), expected);
        }
        drop(conn);
        let _ = std::fs::remove_dir_all(fixture);
    }

    #[test]
    fn count_pruning_resolves_one_boundary_and_keeps_the_newest_records() {
        let path = temp_path("count");
        let mut conn = super::super::schema::open_and_migrate(&path).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        for index in 0..2_500_u64 {
            insert_record(&conn, index, "count retention");
        }
        conn.execute_batch("COMMIT").unwrap();
        let config = HistoryConfig {
            enabled: true,
            max_age_days: 0,
            max_records: 1_000,
            max_bytes: 0,
            maintenance_interval_minutes: 1,
            queue_capacity: 64,
            ..HistoryConfig::default()
        };

        let outcome = run_pass(&mut conn, &path, &config).unwrap();
        assert_eq!(outcome.count_rows_removed, 1_500);
        assert_eq!(outcome.storage.record_count, 1_000);
        assert_eq!(
            conn.query_row("SELECT MIN(id) FROM records", [], |row| row
                .get::<_, u64>(0))
                .unwrap(),
            1_501
        );
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn checkpoint_truncates_and_resecures_wal() {
        let path = temp_path("wal");
        let mut conn = super::super::schema::open_and_migrate(&path).unwrap();
        insert_record(&conn, super::super::now_unix_ms(), "wal record");
        let wal = super::super::sidecar_paths(&path)[0].clone();
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
        run_pass(&mut conn, &path, &HistoryConfig::default()).unwrap();
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
        assert_eq!(
            std::fs::metadata(&wal).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn v1_style_database_converts_and_incremental_vacuum_reduces_pages() {
        let path = temp_path("convert");
        let conn = super::super::schema::open_and_migrate(&path).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        drop(conn);
        let conn = super::super::schema::open_and_migrate(&path).unwrap();
        let initial_mode: u64 = conn
            .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
            .unwrap();
        assert_eq!(initial_mode, 0);
        assert!(super::super::schema::ensure_incremental_autovacuum(&conn, &path).unwrap());
        let converted_mode: u64 = conn
            .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
            .unwrap();
        assert_eq!(converted_mode, 2);

        let payload = "x".repeat(4_096);
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        for index in 0..1_500 {
            insert_record(&conn, index, &payload);
        }
        conn.execute_batch("COMMIT; PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        conn.execute("DELETE FROM records", []).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let before: u64 = conn
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .unwrap();
        for _ in 0..16 {
            conn.execute_batch("PRAGMA incremental_vacuum(512)")
                .unwrap();
            let free: u64 = conn
                .pragma_query_value(None, "freelist_count", |row| row.get(0))
                .unwrap();
            if free == 0 {
                break;
            }
        }
        let after: u64 = conn
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .unwrap();
        assert!(
            after < before,
            "page_count did not fall: {before} -> {after}"
        );
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn size_pruning_converges_in_bounded_passes_while_a_producer_writes() {
        let path = temp_path("size");
        let mut conn = super::super::schema::open_and_migrate(&path).unwrap();
        super::super::schema::ensure_incremental_autovacuum(&conn, &path).unwrap();
        let payload = "s".repeat(4_096);
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        for index in 0..2_500 {
            insert_record(&conn, index, &payload);
        }
        conn.execute_batch("COMMIT; PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let produced = Arc::new(AtomicU64::new(0));
        let producer_path = path.clone();
        let producer_stop = Arc::clone(&stop);
        let producer_count = Arc::clone(&produced);
        let producer = std::thread::spawn(move || {
            let writer = Connection::open(&producer_path).unwrap();
            super::super::schema::configure_connection(&writer).unwrap();
            while !producer_stop.load(Ordering::Acquire) {
                if writer
                    .execute(
                        "INSERT INTO records (ts_unix_ms, record_type, subtype, session, summary) VALUES (?1, 'event', 'producer', 'test', 'live')",
                        [super::super::now_unix_ms()],
                    )
                    .is_ok()
                {
                    producer_count.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });

        let config = HistoryConfig {
            enabled: true,
            max_age_days: 0,
            max_records: 0,
            max_bytes: super::super::MIN_HISTORY_BYTES,
            maintenance_interval_minutes: 1,
            queue_capacity: 64,
            ..HistoryConfig::default()
        };
        for _ in 0..16 {
            let started = Instant::now();
            match run_pass(&mut conn, &path, &config) {
                Ok(outcome) => {
                    assert!(started.elapsed() <= pass_budget() + Duration::from_secs(1));
                    if !outcome.pending {
                        break;
                    }
                }
                Err(error) if error.contains("busy") => {}
                Err(error) => panic!("unexpected maintenance failure: {error}"),
            }
        }
        stop.store(true, Ordering::Release);
        producer.join().unwrap();
        assert!(produced.load(Ordering::Relaxed) > 0);
        // Settle any WAL frames committed between the last maintenance
        // snapshot and the producer observing the stop flag.
        let mut converged = false;
        for _ in 0..16 {
            let outcome = run_pass(&mut conn, &path, &config).unwrap();
            if !outcome.pending {
                converged = true;
                break;
            }
        }
        let storage = storage_snapshot(&conn, &path).unwrap();
        assert!(
            converged,
            "oversized database did not converge: main={} wal={} pages={} free={} records={}",
            storage.main_bytes,
            storage.wal_bytes,
            storage.page_count,
            storage.free_pages,
            storage.record_count
        );
        assert!(storage.main_bytes + storage.wal_bytes <= config.max_bytes);
        assert!(storage.record_count >= 1_000);
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn small_size_overage_does_not_prune_to_the_retained_floor() {
        let path = temp_path("small-size-overage");
        let mut conn = super::super::schema::open_and_migrate(&path).unwrap();
        super::super::schema::ensure_incremental_autovacuum(&conn, &path).unwrap();
        let payload = "s".repeat(4_096);
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        for index in 0..2_500 {
            insert_record(&conn, index, &payload);
        }
        conn.execute_batch("COMMIT; PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let before = storage_snapshot(&conn, &path).unwrap();
        assert!(before.main_bytes > super::super::MIN_HISTORY_BYTES + 512 * 1_024);
        let config = HistoryConfig {
            enabled: true,
            max_age_days: 0,
            max_records: 0,
            max_bytes: before.main_bytes - 512 * 1_024,
            maintenance_interval_minutes: 1,
            queue_capacity: 64,
            ..HistoryConfig::default()
        };

        let outcome = run_pass(&mut conn, &path, &config).unwrap();

        assert!(outcome.size_rows_removed > 0);
        assert!(
            outcome.storage.record_count > 1_000,
            "small overage removed {} rows and left only the floor",
            outcome.size_rows_removed
        );
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn read_only_failure_preserves_rows_and_backoff_is_bounded() {
        let path = temp_path("readonly");
        let conn = super::super::schema::open_and_migrate(&path).unwrap();
        insert_record(
            &conn,
            super::super::now_unix_ms().saturating_sub(2 * 24 * 60 * 60 * 1_000),
            "keep readable",
        );
        drop(conn);
        let mut read_only = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let error = run_pass(&mut read_only, &path, &age_only_config()).unwrap_err();
        assert!(error.contains("retention delete failed"));
        assert_eq!(
            read_only
                .query_row("SELECT COUNT(*) FROM records", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            1
        );
        let mut state = MaintenanceState::due_now();
        state.failed(Duration::from_secs(60));
        let backed_off_until = state.next_attempt;
        state.make_due();
        assert_eq!(state.next_attempt, backed_off_until);
        for _ in 0..20 {
            state.failed(Duration::from_secs(60));
        }
        assert!(state.wait_duration() <= MAX_BACKOFF);
        assert!(state.wait_duration() > Duration::from_secs(60));
        drop(read_only);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
