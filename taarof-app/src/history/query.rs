use super::{
    HistoryFilters, HistoryOrder, HistoryPage, HistoryQueryToken, HistoryRecord,
    DEFAULT_HISTORY_LIMIT, DEFAULT_HISTORY_SCAN_BUDGET, HISTORY_SCHEMA, MAX_HISTORY_LIMIT,
    MAX_HISTORY_SCAN_BUDGET,
};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params_from_iter, Connection, OpenFlags};
use std::path::Path;

pub(crate) fn query(
    path: &Path,
    since_id: Option<u64>,
    limit: Option<usize>,
    filters: HistoryFilters,
    token: Option<&HistoryQueryToken>,
) -> Result<HistoryPage, String> {
    validate_filters(&filters)?;
    let requested_limit = limit.unwrap_or(DEFAULT_HISTORY_LIMIT);
    let limit = requested_limit.clamp(1, MAX_HISTORY_LIMIT);
    let requested_scan_budget = filters.scan_budget.unwrap_or(DEFAULT_HISTORY_SCAN_BUDGET);
    let scan_budget = requested_scan_budget.clamp(1, MAX_HISTORY_SCAN_BUDGET);
    let truncated =
        requested_limit > MAX_HISTORY_LIMIT || requested_scan_budget > MAX_HISTORY_SCAN_BUDGET;
    if token.is_some_and(HistoryQueryToken::is_cancelled) {
        return Err("history query cancelled".to_string());
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("could not open history database for reading: {error}"))?;
    conn.busy_timeout(std::time::Duration::from_millis(2_000))
        .map_err(|error| format!("could not configure history query timeout: {error}"))?;
    if let Some(token) = token {
        token.install(&conn);
    }

    let cursor = since_id
        .map(|id| {
            let id_param = i64::try_from(id)
                .map_err(|_| format!("history cursor id {id} is outside SQLite's range"))?;
            let ts = conn
                .query_row(
                    "SELECT ts_unix_ms FROM records WHERE id = ?",
                    [id_param],
                    |row| row.get::<_, u64>(0),
                )
                .map_err(|error| map_query_error(error, "resolve history cursor"))?;
            Ok::<_, String>((ts, id))
        })
        .transpose()?;
    let has_text = filters
        .text
        .as_deref()
        .is_some_and(|value| !value.is_empty());
    let candidate_limit = if has_text {
        scan_budget.saturating_add(1)
    } else {
        limit.saturating_add(1)
    };
    let (sql, mut params) = select_sql(cursor, candidate_limit, &filters);
    let mut statement = conn
        .prepare(&sql)
        .map_err(|error| format!("could not prepare history query: {error}"))?;
    let mut rows = statement
        .query(params_from_iter(params.drain(..)))
        .map_err(|error| map_query_error(error, "execute history query"))?;
    let mut candidates = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|error| map_query_error(error, "read history row"))?
    {
        candidates.push(
            row_to_record(row).map_err(|error| map_query_error(error, "decode history row"))?,
        );
    }

    let (records, next_id, has_more, scanned, scan_exhausted) =
        if let Some(needle) = filters.text.as_deref().filter(|value| !value.is_empty()) {
            let more_candidates = candidates.len() > scan_budget;
            candidates.truncate(scan_budget);
            let needle = needle.to_lowercase();
            let mut records = Vec::new();
            let mut scanned = 0;
            let mut next_id = since_id.unwrap_or(0);
            let candidate_count = candidates.len();
            for candidate in candidates {
                scanned += 1;
                next_id = candidate.id;
                if text_matches(&candidate, &needle) {
                    records.push(candidate);
                    if records.len() == limit {
                        break;
                    }
                }
            }
            let has_more = more_candidates || scanned < candidate_count;
            let scan_exhausted = more_candidates && scanned == scan_budget;
            (records, next_id, has_more, scanned, scan_exhausted)
        } else {
            let has_more = candidates.len() > limit;
            candidates.truncate(limit);
            let scanned = candidates.len();
            let next_id = candidates
                .last()
                .map(|record| record.id)
                .unwrap_or(since_id.unwrap_or(0));
            (candidates, next_id, has_more, scanned, false)
        };
    Ok(HistoryPage {
        schema: HISTORY_SCHEMA.to_string(),
        since_id,
        limit,
        next_id,
        has_more,
        scanned,
        scan_exhausted,
        truncated,
        filters,
        records,
    })
}

fn validate_filters(filters: &HistoryFilters) -> Result<(), String> {
    if filters
        .record_type
        .as_deref()
        .is_some_and(|value| !matches!(value, "event" | "diagnostic" | "work"))
    {
        return Err("record_type must be event, diagnostic, or work".to_string());
    }
    if filters
        .from_ts
        .zip(filters.to_ts)
        .is_some_and(|(from, to)| from > to)
    {
        return Err("from_ts must be less than or equal to to_ts".to_string());
    }
    if filters
        .severity
        .as_deref()
        .is_some_and(|value| !matches!(value, "info" | "warn" | "error"))
    {
        return Err("severity must be info, warn, or error".to_string());
    }
    if filters
        .text
        .as_deref()
        .is_some_and(|value| value.len() > 200 || value.chars().any(char::is_control))
    {
        return Err("text must be at most 200 characters and contain no control characters".into());
    }
    if filters.scan_budget == Some(0) {
        return Err("scan_budget must be greater than zero".to_string());
    }
    Ok(())
}

fn select_sql(
    cursor: Option<(u64, u64)>,
    limit: usize,
    filters: &HistoryFilters,
) -> (String, Vec<SqlValue>) {
    let index = preferred_index(filters);
    let mut sql = format!(
        "SELECT id, ts_unix_ms, record_type, subtype, source_space, source_seq, session, \
         workspace_origin, tab_origin, pane_origin, task_id, repository, authority, \
         verification, level, summary, attrs FROM records{index} WHERE 1 = 1"
    );
    let mut params = Vec::new();
    if let Some((ts, id)) = cursor {
        sql.push_str(match filters.order {
            HistoryOrder::Asc => " AND (ts_unix_ms, id) > (?, ?)",
            HistoryOrder::Desc => " AND (ts_unix_ms, id) < (?, ?)",
        });
        params.push(SqlValue::Integer(i64::try_from(ts).unwrap_or(i64::MAX)));
        params.push(SqlValue::Integer(i64::try_from(id).unwrap_or(i64::MAX)));
    }
    push_u64_filter(&mut sql, &mut params, "ts_unix_ms >= ?", filters.from_ts);
    push_u64_filter(&mut sql, &mut params, "ts_unix_ms <= ?", filters.to_ts);
    push_text_filter(
        &mut sql,
        &mut params,
        "record_type = ?",
        filters.record_type.as_deref(),
    );
    push_text_filter(
        &mut sql,
        &mut params,
        "session = ?",
        filters.session.as_deref(),
    );
    push_text_filter(
        &mut sql,
        &mut params,
        "workspace_origin = ?",
        filters.workspace.as_deref(),
    );
    push_text_filter(
        &mut sql,
        &mut params,
        "pane_origin = ?",
        filters.pane.as_deref(),
    );
    push_text_filter(
        &mut sql,
        &mut params,
        "task_id = ?",
        filters.task.as_deref(),
    );
    push_text_filter(
        &mut sql,
        &mut params,
        "repository = ?",
        filters.repository.as_deref(),
    );
    push_text_filter(
        &mut sql,
        &mut params,
        "authority = ?",
        filters.authority.as_deref(),
    );
    push_text_filter(
        &mut sql,
        &mut params,
        "verification = ?",
        filters.verification.as_deref(),
    );
    // Events and Work observations have a NULL level and are deliberately
    // excluded whenever a diagnostic severity is selected.
    push_text_filter(
        &mut sql,
        &mut params,
        "level = ?",
        filters.severity.as_deref(),
    );
    sql.push_str(match filters.order {
        HistoryOrder::Asc => " ORDER BY ts_unix_ms ASC, id ASC LIMIT ?",
        HistoryOrder::Desc => " ORDER BY ts_unix_ms DESC, id DESC LIMIT ?",
    });
    params.push(SqlValue::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
    (sql, params)
}

fn preferred_index(filters: &HistoryFilters) -> &'static str {
    if filters.severity.is_some() {
        " INDEXED BY idx_records_level"
    } else if filters.authority.is_some() && filters.verification.is_some() {
        " INDEXED BY idx_records_authority_verification"
    } else if filters.authority.is_some() {
        " INDEXED BY idx_records_authority"
    } else if filters.verification.is_some() {
        " INDEXED BY idx_records_verification"
    } else if filters.repository.is_some() {
        " INDEXED BY idx_records_repository"
    } else if filters.task.is_some() {
        " INDEXED BY idx_records_task"
    } else if filters.pane.is_some() {
        " INDEXED BY idx_records_pane"
    } else if filters.workspace.is_some() {
        " INDEXED BY idx_records_workspace"
    } else if filters.session.is_some() {
        " INDEXED BY idx_records_session"
    } else if filters.record_type.is_some() {
        " INDEXED BY idx_records_type"
    } else {
        " INDEXED BY idx_records_ts"
    }
}

fn text_matches(record: &HistoryRecord, needle: &str) -> bool {
    record.subtype.to_lowercase().contains(needle)
        || record
            .summary
            .as_deref()
            .is_some_and(|summary| summary.to_lowercase().contains(needle))
}

fn map_query_error(error: rusqlite::Error, action: &str) -> String {
    if matches!(
        &error,
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::OperationInterrupted
    ) {
        "history query cancelled".to_string()
    } else {
        format!("could not {action}: {error}")
    }
}

fn push_u64_filter(sql: &mut String, params: &mut Vec<SqlValue>, clause: &str, value: Option<u64>) {
    if let Some(value) = value {
        sql.push_str(" AND ");
        sql.push_str(clause);
        params.push(SqlValue::Integer(i64::try_from(value).unwrap_or(i64::MAX)));
    }
}

fn push_text_filter(
    sql: &mut String,
    params: &mut Vec<SqlValue>,
    clause: &str,
    value: Option<&str>,
) {
    if let Some(value) = value {
        sql.push_str(" AND ");
        sql.push_str(clause);
        params.push(SqlValue::Text(value.to_string()));
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryRecord> {
    let attrs: Option<String> = row.get(16)?;
    Ok(HistoryRecord {
        id: row.get(0)?,
        ts_unix_ms: row.get(1)?,
        record_type: row.get(2)?,
        subtype: row.get(3)?,
        source_space: row.get(4)?,
        source_seq: row.get(5)?,
        session: row.get(6)?,
        workspace_origin: row.get(7)?,
        tab_origin: row.get(8)?,
        pane_origin: row.get(9)?,
        task_id: row.get(10)?,
        repository: row.get(11)?,
        authority: row.get(12)?,
        verification: row.get(13)?,
        level: row.get(14)?,
        summary: row.get(15)?,
        attrs: attrs.and_then(|value| serde_json::from_str(&value).ok()),
    })
}

#[cfg(test)]
pub(crate) fn explain_query_plan(
    conn: &Connection,
    filters: &HistoryFilters,
) -> Result<String, String> {
    // Exercise a subsequent page: the original regression only appeared once
    // a cursor predicate and filtered ordering were combined.
    let (sql, params) = select_sql(Some((2_500, 2_500)), 10, filters);
    let mut statement = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .map_err(|error| error.to_string())?;
    let plans = statement
        .query_map(params_from_iter(params), |row| row.get::<_, String>(3))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(plans.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "taarof-history-index-{}-{nanos}",
                std::process::id()
            ))
            .join("history.sqlite3")
    }

    #[test]
    fn every_filter_uses_an_indexed_search() {
        let path = temp_path();
        let conn = crate::history::schema::open_and_migrate(&path).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        for id in 0..5_000_u64 {
            tx.execute(
                "INSERT INTO records (ts_unix_ms, record_type, subtype, session, workspace_origin, pane_origin, task_id, repository, authority, verification) VALUES (?, 'event', 'test', 'test', 'ws', 'pane', 'EXAMPLE-133', 'owner/repo', 'plan_canonical', 'canonical_file')",
                [id],
            )
            .unwrap();
        }
        tx.commit().unwrap();

        for (filters, expected_index) in [
            (
                HistoryFilters {
                    from_ts: Some(1),
                    ..HistoryFilters::default()
                },
                "idx_records_ts",
            ),
            (
                HistoryFilters {
                    to_ts: Some(2),
                    ..HistoryFilters::default()
                },
                "idx_records_ts",
            ),
            (
                HistoryFilters {
                    record_type: Some("event".into()),
                    ..HistoryFilters::default()
                },
                "idx_records_type",
            ),
            (
                HistoryFilters {
                    session: Some("test".into()),
                    ..HistoryFilters::default()
                },
                "idx_records_session",
            ),
            (
                HistoryFilters {
                    workspace: Some("ws".into()),
                    ..HistoryFilters::default()
                },
                "idx_records_workspace",
            ),
            (
                HistoryFilters {
                    pane: Some("pane".into()),
                    ..HistoryFilters::default()
                },
                "idx_records_pane",
            ),
            (
                HistoryFilters {
                    task: Some("EXAMPLE-133".into()),
                    ..HistoryFilters::default()
                },
                "idx_records_task",
            ),
            (
                HistoryFilters {
                    repository: Some("owner/repo".into()),
                    ..HistoryFilters::default()
                },
                "idx_records_repository",
            ),
            (
                HistoryFilters {
                    authority: Some("plan_canonical".into()),
                    ..HistoryFilters::default()
                },
                "idx_records_authority",
            ),
            (
                HistoryFilters {
                    verification: Some("canonical_file".into()),
                    ..HistoryFilters::default()
                },
                "idx_records_verification",
            ),
            (
                HistoryFilters {
                    authority: Some("plan_canonical".into()),
                    verification: Some("canonical_file".into()),
                    ..HistoryFilters::default()
                },
                "idx_records_authority_verification",
            ),
            (
                HistoryFilters {
                    severity: Some("warn".into()),
                    ..HistoryFilters::default()
                },
                "idx_records_level",
            ),
        ] {
            let plan = explain_query_plan(&conn, &filters).unwrap();
            assert!(plan.contains("SEARCH records"), "{plan}");
            assert!(!plan.contains("SCAN records"), "{plan}");
            assert!(!plan.contains("TEMP B-TREE"), "{plan}");
            assert!(plan.contains(expected_index), "{plan}");
        }
        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn filtered_pages_advance_by_timestamp_and_last_returned_id_without_offset() {
        let path = temp_path();
        let conn = crate::history::schema::open_and_migrate(&path).unwrap();
        for (timestamp, record_type, task) in [
            (30_u64, "work", "EXAMPLE-133"),
            (10, "event", "OTHER"),
            (20, "work", "EXAMPLE-133"),
            (40, "work", "OTHER"),
            (30, "work", "EXAMPLE-133"),
        ] {
            conn.execute(
                "INSERT INTO records (ts_unix_ms, record_type, subtype, session, task_id) VALUES (?, ?, 'test', 'test', ?)",
                rusqlite::params![timestamp, record_type, task],
            )
            .unwrap();
        }

        let filters = HistoryFilters {
            record_type: Some("work".to_string()),
            task: Some("EXAMPLE-133".to_string()),
            ..HistoryFilters::default()
        };
        let first = query(&path, None, Some(2), filters.clone(), None).unwrap();
        assert_eq!(
            first
                .records
                .iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            vec![3, 1]
        );
        assert_eq!(first.next_id, 1);
        assert!(first.has_more);
        assert_eq!(first.filters, filters);

        let second = query(&path, Some(first.next_id), Some(2), filters, None).unwrap();
        assert_eq!(
            second
                .records
                .iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            vec![5]
        );
        assert_eq!(second.next_id, 5);
        assert!(!second.has_more);

        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn ascending_and_descending_keysets_have_no_duplicates_or_gaps() {
        let path = temp_path();
        let conn = crate::history::schema::open_and_migrate(&path).unwrap();
        for timestamp in [20_u64, 10, 30, 20, 40] {
            conn.execute(
                "INSERT INTO records (ts_unix_ms, record_type, subtype, session) VALUES (?, 'event', 'test', 'test')",
                [timestamp],
            )
            .unwrap();
        }

        for (order, expected) in [
            (HistoryOrder::Asc, vec![2, 1, 4, 3, 5]),
            (HistoryOrder::Desc, vec![5, 3, 4, 1, 2]),
        ] {
            let filters = HistoryFilters {
                order,
                ..HistoryFilters::default()
            };
            let mut cursor = None;
            let mut ids = Vec::new();
            loop {
                let page = query(&path, cursor, Some(2), filters.clone(), None).unwrap();
                ids.extend(page.records.iter().map(|record| record.id));
                if !page.has_more {
                    break;
                }
                cursor = Some(page.next_id);
            }
            assert_eq!(ids, expected);
        }

        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn text_search_is_bounded_and_resumes_after_the_last_scanned_candidate() {
        let path = temp_path();
        let conn = crate::history::schema::open_and_migrate(&path).unwrap();
        for (timestamp, subtype, summary) in [
            (1_u64, "quiet", "nothing"),
            (2, "quiet", "nothing"),
            (3, "match-kind", "nothing"),
            (4, "quiet", "MATCH summary"),
            (5, "quiet", "nothing"),
        ] {
            conn.execute(
                "INSERT INTO records (ts_unix_ms, record_type, subtype, session, summary) VALUES (?, 'diagnostic', ?, 'test', ?)",
                rusqlite::params![timestamp, subtype, summary],
            )
            .unwrap();
        }
        let filters = HistoryFilters {
            text: Some("match".into()),
            scan_budget: Some(2),
            ..HistoryFilters::default()
        };
        let first = query(&path, None, Some(10), filters.clone(), None).unwrap();
        assert!(first.records.is_empty());
        assert_eq!(first.scanned, 2);
        assert!(first.scan_exhausted);
        assert_eq!(first.next_id, 2);

        let second = query(&path, Some(first.next_id), Some(10), filters, None).unwrap();
        assert_eq!(
            second
                .records
                .iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(second.scanned, 2);
        assert!(second.has_more);

        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn severity_excludes_records_without_a_diagnostic_level() {
        let path = temp_path();
        let conn = crate::history::schema::open_and_migrate(&path).unwrap();
        for (record_type, level) in [
            ("event", None),
            ("work", None),
            ("diagnostic", Some("warn")),
            ("diagnostic", Some("error")),
        ] {
            conn.execute(
                "INSERT INTO records (ts_unix_ms, record_type, subtype, session, level) VALUES (1, ?, 'test', 'test', ?)",
                rusqlite::params![record_type, level],
            )
            .unwrap();
        }
        let page = query(
            &path,
            None,
            None,
            HistoryFilters {
                severity: Some("warn".into()),
                ..HistoryFilters::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].level.as_deref(), Some("warn"));

        drop(conn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn cancelled_token_returns_a_clean_error() {
        let path = temp_path();
        let _conn = crate::history::schema::open_and_migrate(&path).unwrap();
        let token = HistoryQueryToken::new();
        token.cancel();
        let error = query(&path, None, None, HistoryFilters::default(), Some(&token)).unwrap_err();
        assert_eq!(error, "history query cancelled");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
