use super::{HistoryRecord, HistoryRecordDraft};
use serde_json::{Map, Value};

use crate::diagnostics::safe_text;

pub(crate) const HISTORY_EVENT_ALLOWLIST: &[(&str, &[&str])] = &[
    ("session_started", &["session_name"]),
    ("session_stopping", &["session_name"]),
    (
        "workspace_created",
        &["workspace_id", "workspace_origin", "name"],
    ),
    (
        "workspace_renamed",
        &["workspace_id", "workspace_origin", "name"],
    ),
    (
        "command_exited",
        &["workspace_origin", "tab_origin", "pane_origin", "exit_code"],
    ),
    (
        "probe_state_changed",
        &["workspace_origin", "tab_origin", "pane_origin", "state"],
    ),
    (
        "alert_raised",
        &["workspace_origin", "tab_origin", "pane_origin", "kind"],
    ),
    (
        "agent_activity_changed",
        &[
            "workspace_origin",
            "tab_origin",
            "pane_origin",
            "state",
            "agent",
        ],
    ),
    (
        "agent_message",
        &["workspace_origin", "tab_origin", "pane_origin"],
    ),
    ("socket_message_received", &["action"]),
    ("http_control_action", &["action"]),
    (
        "work_recorded",
        &[
            "workspace_origin",
            "tab_origin",
            "pane_origin",
            "task_id",
            "kind",
            "authority",
            "verification",
        ],
    ),
    ("work_preferences_changed", &["filter"]),
    ("work_ledger_cleared", &["scope"]),
    ("work_reconciled", &["seq", "status", "source"]),
    ("work_reconciled_batch", &["count"]),
];

pub(crate) fn event_is_allowlisted(event_type: &str) -> bool {
    HISTORY_EVENT_ALLOWLIST
        .iter()
        .any(|(allowed, _)| *allowed == event_type)
}

pub(crate) fn sanitize(draft: HistoryRecordDraft) -> Option<HistoryRecord> {
    match draft {
        HistoryRecordDraft::Event(record) => sanitize_event(record),
        HistoryRecordDraft::Diagnostic(record) => sanitize_diagnostic(record),
        HistoryRecordDraft::Work(record) => sanitize_work(*record),
    }
}

fn sanitize_event(record: crate::events::EventRecord) -> Option<HistoryRecord> {
    let allowed = HISTORY_EVENT_ALLOWLIST
        .iter()
        .find_map(|(event_type, keys)| (*event_type == record.event_type).then_some(*keys))?;
    let attrs = project_object(&record.payload, allowed);
    let field = |name: &str| attrs.get(name).and_then(Value::as_str).map(str::to_string);
    let session = field("session_name")
        .or_else(crate::instance::session_name)
        .unwrap_or_else(|| "default".to_string());
    Some(HistoryRecord {
        id: 0,
        ts_unix_ms: record.ts_unix_ms,
        record_type: "event".to_string(),
        subtype: record.event_type,
        source_space: Some("event".to_string()),
        source_seq: Some(record.seq),
        session,
        workspace_origin: field("workspace_origin"),
        tab_origin: field("tab_origin"),
        pane_origin: field("pane_origin"),
        task_id: field("task_id"),
        repository: field("repository"),
        authority: field("authority"),
        verification: field("verification"),
        level: None,
        summary: None,
        attrs: (!attrs.is_empty()).then_some(Value::Object(attrs)),
    })
}

fn sanitize_diagnostic(record: crate::diagnostics::DiagnosticRecord) -> Option<HistoryRecord> {
    let allowed = diagnostic_detail_allowlist(&record.category);
    let subtype = safe_enum(&record.category, 64);
    let category_sanitized = subtype.is_none();
    let attrs = record
        .details
        .as_ref()
        .map(|details| project_object(details, allowed))
        .unwrap_or_default();
    Some(HistoryRecord {
        id: 0,
        ts_unix_ms: record.ts_unix_ms,
        record_type: "diagnostic".to_string(),
        subtype: subtype.unwrap_or_else(|| "invalid_category".to_string()),
        source_space: None,
        source_seq: None,
        session: crate::instance::session_name().unwrap_or_else(|| "default".to_string()),
        workspace_origin: None,
        tab_origin: None,
        pane_origin: None,
        task_id: None,
        repository: None,
        authority: None,
        verification: None,
        level: serde_json::to_value(record.level)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string)),
        summary: scrub_scalar(&Value::String(record.message))
            .and_then(|v| v.as_str().map(str::to_string)),
        attrs: {
            let mut projected = attrs;
            if category_sanitized {
                projected.insert("category_sanitized".to_string(), Value::Bool(true));
            }
            for (key, value) in [("source", record.source), ("action", record.action)] {
                if let Some(value) = safe_enum(&value, 64) {
                    projected.insert(key.to_string(), Value::String(value));
                }
            }
            (!projected.is_empty()).then_some(Value::Object(projected))
        },
    })
}

fn sanitize_work(record: crate::work_ledger::WorkRecord) -> Option<HistoryRecord> {
    let subtype = serde_json::to_value(record.kind)
        .ok()?
        .as_str()?
        .to_string();
    let authority = serde_json::to_value(record.authority)
        .ok()?
        .as_str()?
        .to_string();
    let verification = serde_json::to_value(record.verification)
        .ok()?
        .as_str()?
        .to_string();
    let evidence_source = serde_json::to_value(record.evidence_source)
        .ok()?
        .as_str()?
        .to_string();
    let repository = record
        .pull_request
        .as_ref()
        .and_then(|pr| safe_enum(&pr.repository, 200));
    let mut attrs = Map::new();
    attrs.insert(
        "evidence_source".to_string(),
        Value::String(evidence_source),
    );
    if let Some(status) = record
        .task_status
        .as_deref()
        .and_then(|status| safe_enum(status, 32))
    {
        attrs.insert("task_status".to_string(), Value::String(status));
    }
    if let Some(pr) = &record.pull_request {
        let mut projected = Map::new();
        projected.insert("number".to_string(), Value::from(pr.number));
        projected.insert("is_draft".to_string(), Value::from(pr.is_draft));
        if let Some(value) = safe_enum(&pr.state, 16) {
            projected.insert("state".to_string(), Value::String(value));
        }
        if let Some(value) = pr
            .review_decision
            .as_deref()
            .and_then(|value| safe_enum(value, 64))
        {
            projected.insert("review_decision".to_string(), Value::String(value));
        }
        if let Some(value) = repository.clone() {
            projected.insert("repository".to_string(), Value::String(value));
        }
        attrs.insert("pull_request".to_string(), Value::Object(projected));
    }
    let summary = (record.kind != crate::work_ledger::WorkKind::AssistantMessageCompleted)
        .then(|| scrub_scalar(&Value::String(record.summary)))
        .flatten()
        .and_then(|value| value.as_str().map(str::to_string));
    Some(HistoryRecord {
        id: 0,
        ts_unix_ms: record.ts_unix_ms,
        record_type: "work".to_string(),
        subtype,
        source_space: Some("work".to_string()),
        source_seq: Some(record.seq),
        session: safe_text(&record.identity.session)?,
        workspace_origin: safe_origin(&record.identity.workspace_origin, "workspace-"),
        tab_origin: safe_origin(&record.identity.tab_origin, "tab-"),
        pane_origin: safe_origin(&record.identity.pane_origin, "pane-"),
        task_id: record
            .identity
            .task_id
            .as_deref()
            .and_then(|value| safe_enum(value, 128)),
        repository,
        authority: Some(authority),
        verification: Some(verification),
        level: None,
        summary,
        attrs: Some(Value::Object(attrs)),
    })
}

fn diagnostic_detail_allowlist(category: &str) -> &'static [&'static str] {
    match category {
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
        "command_failure" => &[
            "exit_code",
            "signal",
            "status",
            "failure_kind",
            "message_redacted",
            "timeout_secs",
        ],
        "probe_failure" => &["state", "attempt", "failure_kind", "message_redacted"],
        "lifecycle" => &["pid", "session_name"],
        _ => &[],
    }
}

fn project_object(value: &Value, allowed: &[&str]) -> Map<String, Value> {
    let Some(object) = value.as_object() else {
        return Map::new();
    };
    allowed
        .iter()
        .filter_map(|key| {
            object
                .get(*key)
                .and_then(|value| scrub_field(key, value))
                .map(|value| ((*key).to_string(), value))
        })
        .collect()
}

fn scrub_field(key: &str, value: &Value) -> Option<Value> {
    let origin_prefix = match key {
        "workspace_origin" => Some("workspace-"),
        "tab_origin" => Some("tab-"),
        "pane_origin" => Some("pane-"),
        _ => None,
    };
    if let Some(prefix) = origin_prefix {
        return value
            .as_str()
            .and_then(|value| safe_origin(value, prefix))
            .map(Value::String);
    }
    scrub_scalar(value)
}

fn safe_origin(value: &str, prefix: &str) -> Option<String> {
    (value.starts_with(prefix)
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then(|| value.to_string())
}

pub(crate) fn scrub_scalar(value: &Value) -> Option<Value> {
    match value {
        Value::Null => Some(Value::Null),
        Value::Bool(value) => Some(Value::Bool(*value)),
        Value::Number(value) => Some(Value::Number(value.clone())),
        Value::String(value) => safe_text(value).map(Value::String),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn safe_enum(value: &str, max: usize) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= max
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':')))
    .then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubber_rejects_secrets_terminal_output_and_containers() {
        for value in [
            "Bearer abcdefghijklmnopqrstuvwxyz",
            "password=hunter2",
            "postgresql://user:pass@example.test/db",
            "$ cargo test",
            "\u{1b}[31mterminal output",
            "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
        ] {
            assert!(scrub_scalar(&Value::String(value.to_string())).is_none());
        }
        assert!(scrub_scalar(&serde_json::json!({"nested": true})).is_none());
        assert_eq!(scrub_scalar(&Value::from(42)), Some(Value::from(42)));
    }

    #[test]
    fn unlisted_event_is_not_persisted() {
        assert!(!event_is_allowlisted("terminal_input"));
        assert!(event_is_allowlisted("session_started"));
    }

    #[test]
    fn invalid_diagnostic_category_is_marked_instead_of_silently_dropped() {
        let record = sanitize_diagnostic(crate::diagnostics::DiagnosticRecord {
            ts_unix_ms: 1,
            level: crate::diagnostics::DiagnosticLevel::Warn,
            category: "future category".to_string(),
            source: "history".to_string(),
            action: "record".to_string(),
            message: "category was sanitized".to_string(),
            details: None,
        })
        .expect("the diagnostic should remain observable");
        assert_eq!(record.subtype, "invalid_category");
        assert_eq!(
            record
                .attrs
                .as_ref()
                .and_then(|attrs| attrs.get("category_sanitized")),
            Some(&Value::Bool(true))
        );
    }
}
