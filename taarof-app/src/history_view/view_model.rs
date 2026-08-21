use crate::history::{HistoryPage, HistoryRecord};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LivePane {
    pub workspace_origin: String,
    pub pane_origin: String,
    pub tab_id: u32,
    pub pane_id: u32,
    pub task_id: Option<String>,
    pub checked_at_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LivePullRequest {
    pub repository: String,
    pub number: u64,
    pub url: String,
    pub checked_at_unix_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveSnapshot {
    pub panes: Vec<LivePane>,
    pub pull_requests: Vec<LivePullRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneNavigation {
    pub tab_id: u32,
    pub pane_id: u32,
    pub checked_at_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskNavigation {
    pub task_id: String,
    pub tab_id: u32,
    pub pane_id: u32,
    pub checked_at_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestNavigation {
    pub url: String,
    pub checked_at_unix_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NavigationEligibility {
    pub pane: Option<PaneNavigation>,
    pub task: Option<TaskNavigation>,
    pub pull_request: Option<PullRequestNavigation>,
    pub pane_note: Option<String>,
    pub task_note: Option<String>,
    pub pull_request_note: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HistoryRow {
    pub record: HistoryRecord,
    pub historical_label: String,
    pub type_label: String,
    pub source_label: String,
    pub authority_label: String,
    pub verification_label: String,
    pub observed_at_unix_ms: u64,
    pub snippet: String,
    pub navigation: NavigationEligibility,
}

pub fn project_row(record: &HistoryRecord, live: &LiveSnapshot) -> HistoryRow {
    HistoryRow {
        record: record.clone(),
        historical_label: "Historical observation".to_string(),
        type_label: format!("{} / {}", record.record_type, record.subtype),
        source_label: match (&record.source_space, record.source_seq) {
            (Some(space), Some(seq)) => format!("{space} #{seq}"),
            (Some(space), None) => space.clone(),
            (None, Some(seq)) => format!("sequence #{seq}"),
            (None, None) => "local history".to_string(),
        },
        authority_label: record
            .authority
            .clone()
            .unwrap_or_else(|| "observational".into()),
        verification_label: record
            .verification
            .clone()
            .unwrap_or_else(|| "unverified".into()),
        observed_at_unix_ms: record.ts_unix_ms,
        snippet: record
            .summary
            .clone()
            .unwrap_or_else(|| record.subtype.clone()),
        navigation: navigation_eligibility(record, live),
    }
}

pub fn navigation_eligibility(
    record: &HistoryRecord,
    live: &LiveSnapshot,
) -> NavigationEligibility {
    let mut result = NavigationEligibility::default();

    if let Some(origin) = record.pane_origin.as_deref() {
        let matches = live
            .panes
            .iter()
            .filter(|pane| pane.pane_origin == origin)
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [pane] => {
                result.pane = Some(PaneNavigation {
                    tab_id: pane.tab_id,
                    pane_id: pane.pane_id,
                    checked_at_unix_ms: pane.checked_at_unix_ms,
                });
            }
            [] => result.pane_note = Some("Pane no longer present".into()),
            _ => result.pane_note = Some("Pane identity is ambiguous".into()),
        }
    }

    if let Some(task_id) = record.task_id.as_deref() {
        let matches = live
            .panes
            .iter()
            .filter(|pane| {
                pane.task_id.as_deref() == Some(task_id)
                    && record
                        .workspace_origin
                        .as_deref()
                        .is_none_or(|origin| pane.workspace_origin == origin)
            })
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [pane] => {
                result.task = Some(TaskNavigation {
                    task_id: task_id.to_string(),
                    tab_id: pane.tab_id,
                    pane_id: pane.pane_id,
                    checked_at_unix_ms: pane.checked_at_unix_ms,
                });
            }
            [] => result.task_note = Some("Task no longer present in a live workspace".into()),
            _ => result.task_note = Some("Task identity is ambiguous".into()),
        }
    }

    if let (Some(repository), Some(number)) =
        (record.repository.as_deref(), pull_request_number(record))
    {
        let repository = repository.to_ascii_lowercase();
        let matches = live
            .pull_requests
            .iter()
            .filter(|pull_request| {
                pull_request.repository.to_ascii_lowercase() == repository
                    && pull_request.number == number
            })
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [pull_request] => {
                result.pull_request = Some(PullRequestNavigation {
                    url: pull_request.url.clone(),
                    checked_at_unix_ms: pull_request.checked_at_unix_ms,
                });
            }
            [] => {
                result.pull_request_note =
                    Some("Pull request no longer present in live state".into())
            }
            _ => result.pull_request_note = Some("Pull request identity is ambiguous".into()),
        }
    }

    result
}

fn pull_request_number(record: &HistoryRecord) -> Option<u64> {
    record
        .attrs
        .as_ref()?
        .get("pull_request")?
        .get("number")?
        .as_u64()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderState {
    Loading,
    Empty,
    Ready,
    Stale,
    Unavailable(String),
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct HistoryViewModel {
    pub generation: u64,
    pub rows: Vec<HistoryRow>,
    pub next_id: Option<u64>,
    pub has_more: bool,
    pub scanned: usize,
    pub scan_exhausted: bool,
    pub render_state: RenderState,
}

impl Default for HistoryViewModel {
    fn default() -> Self {
        Self {
            generation: 0,
            rows: Vec::new(),
            next_id: None,
            has_more: false,
            scanned: 0,
            scan_exhausted: false,
            render_state: RenderState::Empty,
        }
    }
}

impl HistoryViewModel {
    pub fn begin_query(&mut self, append: bool) -> u64 {
        self.generation = self.generation.saturating_add(1);
        if !append {
            self.rows.clear();
            self.next_id = None;
            self.has_more = false;
            self.scanned = 0;
        }
        self.render_state = RenderState::Loading;
        self.generation
    }

    pub fn apply_page(
        &mut self,
        generation: u64,
        page: HistoryPage,
        live: &LiveSnapshot,
        append: bool,
        live_is_stale: bool,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        if !append {
            self.rows.clear();
            self.scanned = 0;
        }
        self.rows
            .extend(page.records.iter().map(|record| project_row(record, live)));
        self.next_id = Some(page.next_id);
        self.has_more = page.has_more;
        self.scanned = self.scanned.saturating_add(page.scanned);
        self.scan_exhausted = page.scan_exhausted;
        self.render_state = if live_is_stale && !self.rows.is_empty() {
            RenderState::Stale
        } else if self.rows.is_empty() {
            RenderState::Empty
        } else {
            RenderState::Ready
        };
        true
    }

    pub fn fail(&mut self, generation: u64, error: String, unavailable: bool) -> bool {
        if generation != self.generation {
            return false;
        }
        self.render_state = if unavailable {
            RenderState::Unavailable(error)
        } else {
            RenderState::Failed(error)
        };
        true
    }
}

/// Export only the durable store's sanitized record projection. No terminal,
/// transcript, argv, input, token, or environment field exists in this shape.
pub fn export_page(rows: &[HistoryRow]) -> String {
    let records = rows.iter().map(|row| &row.record).collect::<Vec<_>>();
    serde_json::to_string_pretty(&records).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{HistoryFilters, HistoryOrder};

    fn record() -> HistoryRecord {
        HistoryRecord {
            id: 1,
            ts_unix_ms: 10,
            record_type: "work".into(),
            subtype: "started".into(),
            source_space: Some("work".into()),
            source_seq: Some(7),
            session: "test".into(),
            workspace_origin: Some("workspace-a".into()),
            tab_origin: Some("tab-a".into()),
            pane_origin: Some("pane-a".into()),
            task_id: Some("EXAMPLE-135".into()),
            repository: Some("owner/repo".into()),
            authority: Some("plan_canonical".into()),
            verification: Some("canonical_file".into()),
            level: None,
            summary: Some("Implementation started".into()),
            attrs: Some(serde_json::json!({"pull_request": {"number": 42}})),
        }
    }

    fn live() -> LiveSnapshot {
        LiveSnapshot {
            panes: vec![LivePane {
                workspace_origin: "workspace-a".into(),
                pane_origin: "pane-a".into(),
                tab_id: 2,
                pane_id: 3,
                task_id: Some("EXAMPLE-135".into()),
                checked_at_unix_ms: 20,
            }],
            pull_requests: vec![LivePullRequest {
                repository: "owner/repo".into(),
                number: 42,
                url: "https://github.com/owner/repo/pull/42".into(),
                checked_at_unix_ms: 21,
            }],
        }
    }

    #[test]
    fn projection_keeps_history_and_live_provenance_separate() {
        let row = project_row(&record(), &live());
        assert_eq!(row.historical_label, "Historical observation");
        assert_eq!(row.source_label, "work #7");
        assert_eq!(row.authority_label, "plan_canonical");
        assert_eq!(row.navigation.pane.unwrap().checked_at_unix_ms, 20);
        assert_eq!(row.observed_at_unix_ms, 10);
    }

    #[test]
    fn navigation_requires_one_exact_live_identity() {
        let source = record();
        let exact = navigation_eligibility(&source, &live());
        assert!(exact.pane.is_some() && exact.task.is_some() && exact.pull_request.is_some());

        let missing = navigation_eligibility(&source, &LiveSnapshot::default());
        assert!(missing.pane.is_none() && missing.task.is_none() && missing.pull_request.is_none());
        assert!(missing.pane_note.unwrap().contains("no longer"));

        let mut ambiguous = live();
        ambiguous.panes.push(ambiguous.panes[0].clone());
        ambiguous.pull_requests.push(LivePullRequest {
            url: "https://github.com/other/repo/pull/42".into(),
            ..ambiguous.pull_requests[0].clone()
        });
        let ambiguous = navigation_eligibility(&source, &ambiguous);
        assert!(ambiguous.pane_note.unwrap().contains("ambiguous"));
        assert!(ambiguous.task_note.unwrap().contains("ambiguous"));
        assert!(ambiguous.pull_request_note.unwrap().contains("ambiguous"));
    }

    #[test]
    fn render_state_machine_accumulates_pages_and_ignores_old_generations() {
        let mut model = HistoryViewModel::default();
        assert_eq!(model.render_state, RenderState::Empty);
        let generation = model.begin_query(false);
        assert_eq!(model.render_state, RenderState::Loading);
        let page = HistoryPage {
            schema: "taarof.history.v1".into(),
            since_id: None,
            limit: 100,
            next_id: 1,
            has_more: true,
            scanned: 1,
            scan_exhausted: false,
            truncated: false,
            filters: HistoryFilters {
                order: HistoryOrder::Desc,
                ..HistoryFilters::default()
            },
            records: vec![record()],
        };
        assert!(model.apply_page(generation, page, &live(), false, false));
        assert_eq!(model.render_state, RenderState::Ready);
        assert_eq!(model.rows.len(), 1);
        assert!(!model.fail(generation - 1, "old".into(), false));

        let stale_generation = model.begin_query(false);
        let stale_page = HistoryPage {
            schema: "taarof.history.v1".into(),
            since_id: None,
            limit: 100,
            next_id: 1,
            has_more: false,
            scanned: 1,
            scan_exhausted: false,
            truncated: false,
            filters: HistoryFilters::default(),
            records: vec![record()],
        };
        assert!(model.apply_page(stale_generation, stale_page, &live(), false, true));
        assert_eq!(model.render_state, RenderState::Stale);

        let generation = model.begin_query(true);
        assert!(model.fail(generation, "storage unavailable".into(), true));
        assert!(matches!(model.render_state, RenderState::Unavailable(_)));
        let generation = model.begin_query(false);
        assert!(model.fail(generation, "broken".into(), false));
        assert!(matches!(model.render_state, RenderState::Failed(_)));
    }

    #[test]
    fn export_contains_only_the_sanitized_record_shape() {
        let row = project_row(&record(), &live());
        let exported = export_page(&[row]);
        let value: serde_json::Value = serde_json::from_str(&exported).unwrap();
        let object = value[0].as_object().unwrap();
        for forbidden in ["terminal", "transcript", "input", "token", "argv", "env"] {
            assert!(!object.contains_key(forbidden));
            assert!(!exported.contains(&format!("\"{forbidden}\"")));
        }
    }
}
