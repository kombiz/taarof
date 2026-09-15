//! SQLite storage and startup migrations.
//!
//! Migrations are embedded in the binary and applied in filename order at
//! startup, tracked in a `schema_migrations` bookkeeping table so that
//! re-running is a no-op. The schema deliberately has no columns for terminal
//! content, input bytes, pasted text, bearer tokens, token hashes, local
//! paths, PIDs, or socket locations — audit rows carry metadata only.

use std::path::Path;

use rusqlite::Connection;

use crate::error::GatewayError;

/// Embedded migrations, applied in listed order. The version string is the
/// migration's stable identifier recorded in `schema_migrations`.
const MIGRATIONS: &[(&str, &str)] = &[(
    "0001_initial",
    include_str!("../migrations/0001_initial.sql"),
)];

/// Open the database at `path`, applying any pending migrations.
pub fn open(path: &Path) -> Result<Connection, GatewayError> {
    let conn = Connection::open(path)?;
    prepare(&conn)?;
    Ok(conn)
}

/// Open an in-memory database, applying migrations. Intended for tests.
pub fn open_in_memory() -> Result<Connection, GatewayError> {
    let conn = Connection::open_in_memory()?;
    prepare(&conn)?;
    Ok(conn)
}

/// Apply pragmas and run migrations on an open connection.
fn prepare(conn: &Connection) -> Result<(), GatewayError> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    apply_migrations(conn)?;
    Ok(())
}

/// Apply every embedded migration not yet recorded in `schema_migrations`.
fn apply_migrations(conn: &Connection) -> Result<(), GatewayError> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    TEXT PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        )",
        [],
    )?;

    for (version, sql) in MIGRATIONS {
        let already: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = ?1)",
            [version],
            |row| row.get(0),
        )?;
        if already {
            continue;
        }
        conn.execute_batch(sql)?;
        conn.execute(
            "INSERT INTO schema_migrations (version) VALUES (?1)",
            [version],
        )?;
    }
    Ok(())
}

/// A persisted paired-device record. Public-key material and metadata only —
/// no private keys, tokens, paths, PIDs, or sockets, honoring the redaction
/// schema. This is what survives a gateway restart (design spec line 94).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDevice {
    pub device_uuid: String,
    pub display_name: String,
    pub observe_public_key: Vec<u8>,
    pub control_public_key: Vec<u8>,
    /// Verified attestation facts as JSON (never private material).
    pub attestation_facts: Option<String>,
    /// Highest verified security level, e.g. "strongbox" or "tee".
    pub security_level: Option<String>,
    pub created_at_ms: u64,
    pub revoked: bool,
}

/// Insert a newly confirmed device. `created_at` is stored as decimal Unix
/// milliseconds (TEXT), and `revoked_at` is left NULL.
pub fn insert_device(conn: &Connection, device: &StoredDevice) -> Result<(), GatewayError> {
    conn.execute(
        "INSERT INTO devices
             (device_uuid, display_name, observe_public_key, control_public_key,
              attestation_facts, security_level, created_at, revoked_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
        rusqlite::params![
            device.device_uuid,
            device.display_name,
            device.observe_public_key,
            device.control_public_key,
            device.attestation_facts,
            device.security_level,
            device.created_at_ms.to_string(),
        ],
    )?;
    Ok(())
}

/// Load every paired device, oldest first.
pub fn load_devices(conn: &Connection) -> Result<Vec<StoredDevice>, GatewayError> {
    let mut stmt = conn.prepare(
        "SELECT device_uuid, display_name, observe_public_key, control_public_key,
                attestation_facts, security_level, created_at, revoked_at
         FROM devices ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        let created_at: String = row.get(6)?;
        let revoked_at: Option<String> = row.get(7)?;
        Ok(StoredDevice {
            device_uuid: row.get(0)?,
            display_name: row.get(1)?,
            observe_public_key: row.get(2)?,
            control_public_key: row.get(3)?,
            attestation_facts: row.get(4)?,
            security_level: row.get(5)?,
            created_at_ms: created_at.parse().unwrap_or(0),
            revoked: revoked_at.is_some(),
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(GatewayError::from)
}

/// A metadata-only audit row. By construction it can hold identity, logical
/// tab/pane identifiers, a coarse action category, a byte count, and an outcome
/// — and nothing else. There is deliberately no field for terminal content,
/// input bytes, pasted text, bearer tokens, token hashes, filesystem paths,
/// PIDs, or socket locations (design spec, redaction requirement).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRow {
    /// The acting device, or `None` for a pre-authentication event.
    pub device_uuid: Option<String>,
    /// The pinned runtime this gateway serves.
    pub runtime_instance_id: String,
    pub tab_id: Option<String>,
    pub pane_id: Option<String>,
    /// Coarse action category, e.g. "attach", "input", "tab_create".
    pub action_category: String,
    /// Size of the associated payload. The payload itself is never stored.
    pub byte_count: u64,
    /// "ok", "denied", "rate_limited", "error", etc.
    pub outcome: String,
}

/// Append one metadata-only audit row.
pub fn insert_audit(conn: &Connection, row: &AuditRow) -> Result<(), GatewayError> {
    conn.execute(
        "INSERT INTO audit_log
             (device_uuid, runtime_instance_id, tab_id, pane_id,
              action_category, byte_count, outcome)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            row.device_uuid,
            row.runtime_instance_id,
            row.tab_id,
            row.pane_id,
            row.action_category,
            row.byte_count as i64,
            row.outcome,
        ],
    )?;
    Ok(())
}

/// Load every audit row, oldest first. Intended for tests and operator review.
pub fn load_audit(conn: &Connection) -> Result<Vec<AuditRow>, GatewayError> {
    let mut stmt = conn.prepare(
        "SELECT device_uuid, runtime_instance_id, tab_id, pane_id,
                action_category, byte_count, outcome
         FROM audit_log ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        let byte_count: i64 = row.get(5)?;
        Ok(AuditRow {
            device_uuid: row.get(0)?,
            runtime_instance_id: row.get(1)?,
            tab_id: row.get(2)?,
            pane_id: row.get(3)?,
            action_category: row.get(4)?,
            byte_count: byte_count.max(0) as u64,
            outcome: row.get(6)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(GatewayError::from)
}

/// Mark a device revoked. Idempotent: an already-revoked row is untouched.
pub fn set_device_revoked(
    conn: &Connection,
    device_uuid: &str,
    revoked_at_ms: u64,
) -> Result<(), GatewayError> {
    conn.execute(
        "UPDATE devices SET revoked_at = ?2 WHERE device_uuid = ?1 AND revoked_at IS NULL",
        rusqlite::params![device_uuid, revoked_at_ms.to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get::<_, bool>(0),
        )
        .unwrap()
    }

    #[test]
    fn migrations_create_expected_tables() {
        let conn = open_in_memory().expect("open in-memory db");
        assert!(table_exists(&conn, "devices"), "devices table missing");
        assert!(table_exists(&conn, "audit_log"), "audit_log table missing");
        assert!(
            table_exists(&conn, "schema_migrations"),
            "schema_migrations table missing"
        );
    }

    #[test]
    fn migrations_are_idempotent() {
        let conn = open_in_memory().expect("open in-memory db");
        // A second preparation pass must not error or re-apply.
        apply_migrations(&conn).expect("re-apply migrations");
        let applied: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(applied, MIGRATIONS.len() as i64);
    }

    #[test]
    fn audit_log_has_no_content_columns() {
        // Guard the redaction-by-construction invariant: audit rows must never
        // be able to hold terminal content, tokens, paths, PIDs, or sockets.
        let conn = open_in_memory().expect("open in-memory db");
        let mut stmt = conn.prepare("PRAGMA table_info(audit_log)").unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for forbidden in [
            "content",
            "output",
            "input",
            "paste",
            "token",
            "token_hash",
            "path",
            "pid",
            "socket",
        ] {
            assert!(
                !columns.iter().any(|c| c.contains(forbidden)),
                "audit_log must not contain a `{forbidden}` column, found {columns:?}"
            );
        }
    }
}
