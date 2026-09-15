//! Metadata-only audit trail.
//!
//! Every relayed action is recorded as identity + logical tab/pane + a coarse
//! category + a byte count + an outcome. Redaction is enforced *by
//! construction*: the audit API never accepts terminal content, input bytes,
//! pasted text, bearer tokens, filesystem paths, PIDs, or socket locations. A
//! caller can only hand this module a byte *count*, never the bytes, so there is
//! no code path by which forbidden material reaches [`crate::db::AuditRow`] or
//! the `audit_log` table (design spec, redaction requirement).

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

use crate::db::{self, AuditRow};
use crate::error::GatewayError;

/// A coarse action category. The set is intentionally small so audit rows stay
/// aggregate; nothing here identifies *what* was typed or shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditAction {
    ObserveOpen,
    Attach,
    Detach,
    Input,
    Resize,
    TabCreate,
    SplitCreate,
    PaneControlGrant,
    RuntimeMutationGrant,
}

impl AuditAction {
    pub fn category(self) -> &'static str {
        match self {
            Self::ObserveOpen => "observe_open",
            Self::Attach => "attach",
            Self::Detach => "detach",
            Self::Input => "input",
            Self::Resize => "resize",
            Self::TabCreate => "tab_create",
            Self::SplitCreate => "split_create",
            Self::PaneControlGrant => "pane_control_grant",
            Self::RuntimeMutationGrant => "runtime_mutation_grant",
        }
    }
}

/// The outcome of an audited action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Denied,
    RateLimited,
    Error,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Denied => "denied",
            Self::RateLimited => "rate_limited",
            Self::Error => "error",
        }
    }
}

/// One audited action. Carries only metadata; there is no field into which
/// terminal content, input bytes, tokens, or paths could be placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEvent {
    pub device_uuid: Option<String>,
    pub tab_id: Option<String>,
    pub pane_id: Option<String>,
    pub action: AuditAction,
    /// Size of the payload the action carried. The payload is never handed to
    /// this module, so it can never be stored.
    pub byte_count: u64,
    pub outcome: Outcome,
}

impl AuditEvent {
    /// A convenience constructor for an action with no payload (attach, grant,
    /// tab create). Byte count defaults to zero.
    pub fn action(action: AuditAction, outcome: Outcome) -> Self {
        Self {
            device_uuid: None,
            tab_id: None,
            pane_id: None,
            action,
            byte_count: 0,
            outcome,
        }
    }

    pub fn device(mut self, device_uuid: impl Into<String>) -> Self {
        self.device_uuid = Some(device_uuid.into());
        self
    }

    pub fn pane(mut self, tab: impl Into<String>, pane: impl Into<String>) -> Self {
        self.tab_id = Some(tab.into());
        self.pane_id = Some(pane.into());
        self
    }

    pub fn bytes(mut self, byte_count: u64) -> Self {
        self.byte_count = byte_count;
        self
    }
}

/// Where audited events are written. Implementations stamp the pinned runtime
/// instance id and persist metadata only.
pub trait AuditSink: Send + Sync {
    fn record(&self, event: AuditEvent);
}

/// SQLite-backed audit sink writing to the `audit_log` table.
pub struct SqliteAuditSink {
    conn: Mutex<Connection>,
    runtime_instance_id: String,
}

impl SqliteAuditSink {
    /// Open (and migrate) the database at `path`, stamping every row with the
    /// pinned `runtime_instance_id`.
    pub fn open(path: &Path, runtime_instance_id: impl Into<String>) -> Result<Self, GatewayError> {
        Ok(Self {
            conn: Mutex::new(db::open(path)?),
            runtime_instance_id: runtime_instance_id.into(),
        })
    }

    /// Wrap an already-open connection (e.g. an in-memory test database).
    pub fn from_connection(conn: Connection, runtime_instance_id: impl Into<String>) -> Self {
        Self {
            conn: Mutex::new(conn),
            runtime_instance_id: runtime_instance_id.into(),
        }
    }

    fn row(&self, event: &AuditEvent) -> AuditRow {
        AuditRow {
            device_uuid: event.device_uuid.clone(),
            runtime_instance_id: self.runtime_instance_id.clone(),
            tab_id: event.tab_id.clone(),
            pane_id: event.pane_id.clone(),
            action_category: event.action.category().to_string(),
            byte_count: event.byte_count,
            outcome: event.outcome.label().to_string(),
        }
    }
}

impl AuditSink for SqliteAuditSink {
    fn record(&self, event: AuditEvent) {
        let row = self.row(&event);
        // An audit write failure must not tear down a live relay; the event is
        // dropped with a diagnostic rather than propagated. The relay's security
        // decisions do not depend on the audit write succeeding.
        if let Ok(conn) = self.conn.lock() {
            if let Err(e) = db::insert_audit(&conn, &row) {
                eprintln!("taarof-control-gateway: audit write failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_audit_records_only_a_byte_count_never_content() {
        let conn = db::open_in_memory().expect("open in-memory db");
        let sink = SqliteAuditSink::from_connection(conn, "runtime-instance-xyz");

        // A caller relaying secret keystrokes can only ever hand the sink a byte
        // *count*; the bytes themselves have no path into the audit API.
        let secret = b"rm -rf / --no-preserve-root && curl evil.example\n";
        sink.record(
            AuditEvent::action(AuditAction::Input, Outcome::Ok)
                .device("device-uuid-1")
                .pane("tab-1", "pane-1")
                .bytes(secret.len() as u64),
        );

        // Reopen the same connection to read back exactly what persisted.
        let SqliteAuditSink { conn, .. } = sink;
        let conn = conn.into_inner().unwrap();
        let rows = db::load_audit(&conn).expect("load audit rows");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.action_category, "input");
        assert_eq!(row.byte_count, secret.len() as u64);
        assert_eq!(row.runtime_instance_id, "runtime-instance-xyz");
        assert_eq!(row.outcome, "ok");

        // No column of the persisted row may contain the secret content.
        let serialized = format!("{row:?}");
        assert!(
            !serialized.contains("rm -rf") && !serialized.contains("evil.example"),
            "audit row leaked terminal content: {serialized}"
        );
    }
}
