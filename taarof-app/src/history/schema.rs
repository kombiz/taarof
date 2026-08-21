use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::fs::{self, DirBuilder, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub(crate) struct Migration {
    pub to_version: u64,
    pub sql: &'static str,
}

pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        to_version: 1,
        sql: r#"
CREATE TABLE records (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts_unix_ms INTEGER NOT NULL,
  record_type TEXT NOT NULL,
  subtype TEXT NOT NULL,
  source_space TEXT,
  source_seq INTEGER,
  session TEXT NOT NULL,
  workspace_origin TEXT,
  tab_origin TEXT,
  pane_origin TEXT,
  task_id TEXT,
  repository TEXT,
  authority TEXT,
  verification TEXT,
  level TEXT,
  summary TEXT,
  attrs TEXT
);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE INDEX idx_records_ts ON records(ts_unix_ms, id);
CREATE INDEX idx_records_type ON records(record_type, ts_unix_ms, id);
CREATE INDEX idx_records_session ON records(session, ts_unix_ms, id);
CREATE INDEX idx_records_workspace ON records(workspace_origin, ts_unix_ms, id);
CREATE INDEX idx_records_pane ON records(pane_origin, ts_unix_ms, id);
CREATE INDEX idx_records_task ON records(task_id, ts_unix_ms, id);
CREATE INDEX idx_records_repository ON records(repository, ts_unix_ms, id);
CREATE INDEX idx_records_authority ON records(authority, ts_unix_ms, id);
CREATE INDEX idx_records_authority_verification ON records(authority, verification, ts_unix_ms, id);
CREATE INDEX idx_records_verification ON records(verification, ts_unix_ms, id);
"#,
    },
    Migration {
        to_version: 2,
        // idx_records_ts(ts_unix_ms, id) already covers the age-pruning
        // predicate and ordering; a second ts index would only add write cost.
        sql: "-- history retention and maintenance metadata is stored in meta",
    },
    Migration {
        to_version: 3,
        sql: "CREATE INDEX IF NOT EXISTS idx_records_level ON records(level, ts_unix_ms, id);",
    },
];

pub(crate) fn open_and_migrate(path: &Path) -> Result<Connection, String> {
    prepare_path(path)?;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| {
        format!(
            "could not open history database {}: {error}",
            path.display()
        )
    })?;
    configure_connection(&conn)?;
    apply_migrations(conn, MIGRATIONS)
}

fn prepare_path(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("history path {} has no parent", path.display()))?;
    if !parent.exists() {
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(parent).map_err(|error| {
            format!("could not create history dir {}: {error}", parent.display())
        })?;
    }
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("could not secure history dir {}: {error}", parent.display()))?;
    if !path.exists() {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| {
                format!(
                    "could not create history database {}: {error}",
                    path.display()
                )
            })?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
        format!(
            "could not secure history database {}: {error}",
            path.display()
        )
    })
}

pub(crate) fn configure_connection(conn: &Connection) -> Result<(), String> {
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|error| format!("could not enable history WAL: {error}"))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|error| format!("could not configure history sync: {error}"))?;
    conn.busy_timeout(std::time::Duration::from_millis(2_000))
        .map_err(|error| format!("could not configure history busy timeout: {error}"))?;
    Ok(())
}

fn apply_migrations(mut conn: Connection, migrations: &[Migration]) -> Result<Connection, String> {
    let current: u64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| format!("could not read history schema version: {error}"))?;
    let latest = migrations
        .last()
        .map_or(0, |migration| migration.to_version);
    if current > latest {
        return Err(format!(
            "history schema version {current} is newer than supported version {latest}"
        ));
    }
    let pending = migrations
        .iter()
        .filter(|migration| migration.to_version > current)
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(conn);
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("could not start history migration: {error}"))?;
    let mut expected = current;
    for migration in pending {
        if migration.to_version != expected + 1 {
            return Err(format!(
                "history migration sequence jumps from {expected} to {}",
                migration.to_version
            ));
        }
        tx.execute_batch(migration.sql).map_err(|error| {
            format!(
                "history migration to version {} failed: {error}",
                migration.to_version
            )
        })?;
        tx.pragma_update(None, "user_version", migration.to_version)
            .map_err(|error| format!("could not set history schema version: {error}"))?;
        expected = migration.to_version;
    }
    tx.commit()
        .map_err(|error| format!("could not commit history migration: {error}"))?;
    Ok(conn)
}

pub(crate) fn secure_sidecars(path: &Path) -> Result<(), String> {
    for sidecar in super::sidecar_paths(path) {
        if sidecar.exists() {
            fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o600)).map_err(|error| {
                format!(
                    "could not secure history sidecar {}: {error}",
                    sidecar.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Convert pre-v2 databases from auto_vacuum=NONE to INCREMENTAL once.
///
/// This is intentionally non-transactional because SQLite requires `VACUUM`
/// to rebuild the file after changing the pragma. The writer thread is the
/// sole caller, so this never blocks GTK or query handling.
pub(crate) fn ensure_incremental_autovacuum(
    conn: &Connection,
    path: &Path,
) -> Result<bool, String> {
    let converted = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'autovacuum_converted'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .is_some();
    let mode: u64 = conn
        .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
        .map_err(|error| format!("could not read auto_vacuum for {}: {error}", path.display()))?;
    if converted && mode == 2 {
        return Ok(false);
    }
    if mode != 2 {
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .map_err(|error| {
                format!(
                    "could not enable incremental auto_vacuum for {}: {error}",
                    path.display()
                )
            })?;
        conn.execute_batch("VACUUM").map_err(|error| {
            format!(
                "could not convert {} to incremental auto_vacuum: {error}",
                path.display()
            )
        })?;
    }
    let verified: u64 = conn
        .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
        .map_err(|error| {
            format!(
                "could not verify auto_vacuum for {}: {error}",
                path.display()
            )
        })?;
    if verified != 2 {
        return Err(format!(
            "incremental auto_vacuum conversion for {} did not take effect (mode {verified})",
            path.display()
        ));
    }
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('autovacuum_converted', '1') ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )
    .map_err(|error| format!("could not record auto_vacuum conversion: {error}"))?;
    secure_sidecars(path)?;
    Ok(mode != 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
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

    #[test]
    fn empty_database_migrates_atomically_to_latest() {
        let path = temp_path("migration");
        let conn = open_and_migrate(&path).unwrap();
        let version: u64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, super::super::HISTORY_SCHEMA_VERSION);
        assert_eq!(
            conn.query_row("SELECT count(*) FROM records", [], |row| row
                .get::<_, u64>(0))
                .unwrap(),
            0
        );
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn version_two_database_reopens_with_the_severity_index() {
        let path = temp_path("v2-to-v3");
        prepare_path(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        configure_connection(&conn).unwrap();
        let v2 = &MIGRATIONS[..2];
        let conn = apply_migrations(conn, v2).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        drop(conn);

        let conn = open_and_migrate(&path).unwrap();
        let version: u64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let index_count: u64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_records_level'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, 3);
        assert_eq!(index_count, 1);
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn failed_migration_rolls_back_schema_and_user_version() {
        const BROKEN: &[Migration] = &[Migration {
            to_version: 1,
            sql: "CREATE TABLE partial_record (id INTEGER); THIS IS NOT SQL;",
        }];
        let path = temp_path("migration-rollback");
        prepare_path(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        configure_connection(&conn).unwrap();
        assert!(apply_migrations(conn, BROKEN).is_err());

        let conn = Connection::open(&path).unwrap();
        let version: u64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let partial_tables: u64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'partial_record'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, 0);
        assert_eq!(partial_tables, 0);
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn existing_timestamp_index_covers_age_pruning_predicate() {
        let path = temp_path("age-plan");
        let conn = open_and_migrate(&path).unwrap();
        let details = conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT id FROM records WHERE ts_unix_ms < ?1 ORDER BY ts_unix_ms, id LIMIT ?2",
            )
            .unwrap()
            .query_map(rusqlite::params![1_u64, 2_000_u64], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("; ");
        assert!(details.contains("idx_records_ts"), "query plan: {details}");
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
