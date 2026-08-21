//! Bounded, session-local operator memory derived from typed runtime evidence.
//!
//! Work records deliberately do not contain terminal output, transcript bodies,
//! activity text, tool payloads, or raw file paths. A verified, non-sensitive
//! repo-relative path may enter a short summary; absolute, outside-repo, and
//! suspicious paths never cross this persistence boundary.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

const DEFAULT_CAPACITY: usize = 500;
const DEFAULT_MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const SIDEBAR_LIMIT: usize = 8;
const EXACT_PR_REFRESH_LIMIT: usize = 8;
const EXACT_PR_REFRESH_CONCURRENCY: usize = 4;
const EXACT_PR_COMMAND_TIMEOUT: &str = "2s";
const MAX_REPLAY_WATERMARKS: usize = 4_096;
const MAX_PR_SNAPSHOTS: usize = 2_048;
const RECONCILIATION_EVENT_BURST_LIMIT: usize = 25;
// Reserve the final sequence value as a hard stop. Restored ledgers at this
// threshold are compactly renumbered before any new record can be appended.
const MAX_SAFE_NEXT_SEQ: u64 = u64::MAX - 1;
pub const WORK_STREAM_COLOR_COUNT: usize = 5;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    PaneBound,
    PaneUnbound,
    AgentStateChanged,
    AssistantMessageCompleted,
    FileTouched,
    TaskStatusChanged,
    TaskCreated,
    AgentReportedStarted,
    AgentReportedProgress,
    AgentReportedBlocked,
    AgentReportedFinished,
    PullRequestOpened,
    PullRequestDraftChanged,
    PullRequestReviewChanged,
    PullRequestChecksReady,
    PullRequestMerged,
    PullRequestClosed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    PaneBinding,
    AgentActivity,
    AgentTranscript,
    TranscriptFileOperation,
    PlanFile,
    AgentReport,
    GitHubPullRequest,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkAuthority {
    TaarofSession,
    AgentObservation,
    PlanCanonical,
    GitHubCanonical,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    Observed,
    CanonicalFile,
    RemoteVerified,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkPullRequestRef {
    pub repository: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub review_decision: Option<String>,
    #[serde(default)]
    pub checks: crate::tracking::PullRequestChecks,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkIdentity {
    pub session: String,
    /// Stable identity, distinct from the live workspace control-plane id.
    #[serde(default)]
    pub workspace_origin: String,
    /// Stable identity, distinct from the live tab control-plane id.
    #[serde(default)]
    pub tab_origin: String,
    /// Deterministic pane origin that survives runtime tab-id reassignment on
    /// restore. Runtime ids remain present for live control-plane correlation.
    pub pane_origin: String,
    pub workspace_id: u32,
    pub workspace_name: String,
    pub tab_id: u32,
    pub tab_name: String,
    pub pane_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_title: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkRecord {
    /// Ledger-local sequence. This is intentionally unrelated to EventStore seq.
    pub seq: u64,
    pub ts_unix_ms: u64,
    pub kind: WorkKind,
    pub summary: String,
    pub identity: WorkIdentity,
    pub evidence_source: EvidenceSource,
    pub authority: WorkAuthority,
    pub verification: VerificationState,
    /// Typed canonical task status captured at observation time. Legacy v1
    /// rows omit this and remain historical/unverifiable rather than parsing
    /// human-facing summary prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<WorkPullRequestRef>,
}

#[derive(Clone, Debug)]
pub struct WorkDraft {
    pub kind: WorkKind,
    pub summary: String,
    pub identity: WorkIdentity,
    pub evidence_source: EvidenceSource,
    pub authority: WorkAuthority,
    pub verification: VerificationState,
    pub dedupe_scope: String,
    pub fingerprint: String,
}

/// Operator-selected projection over the immutable chronological ledger.
/// Filtering never mutates, groups, or reorders the stored records.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkStreamFilter {
    #[default]
    All,
    Pane(String),
    Task(String),
    Attention,
    History,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkStreamPaneMeta {
    pub identity: Option<WorkIdentity>,
    pub agent_name: Option<String>,
    pub origin_state: WorkStreamOriginState,
    pub display_order: usize,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkStreamOriginState {
    Live,
    Lazy,
    #[default]
    Historical,
}

/// Session-lifetime display identity. The first five observed pane origins get
/// a stable slot. Overflow stays neutral unless the operator explicitly swaps
/// it into the fifth slot; this changes projection state, never ledger data.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkStreamPalette {
    #[serde(default)]
    origins: Vec<String>,
    #[serde(default)]
    color_slots: HashMap<String, u8>,
}

impl WorkStreamPalette {
    pub fn observe_origins<'a>(&mut self, origins: impl IntoIterator<Item = &'a str>) {
        for origin in origins {
            if self.origins.iter().any(|known| known == origin) {
                continue;
            }
            let ordinal = self.origins.len();
            self.origins.push(origin.to_string());
            if ordinal < WORK_STREAM_COLOR_COUNT {
                self.color_slots.insert(origin.to_string(), ordinal as u8);
            }
        }
    }

    pub fn observe(&mut self, records: &[WorkRecord]) {
        let mut ordered = records.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|record| record.seq);
        self.observe_origins(
            ordered
                .into_iter()
                .map(|record| record.identity.pane_origin.as_str()),
        );
    }

    pub fn promote(&mut self, pane_origin: &str) -> bool {
        if !self.origins.iter().any(|known| known == pane_origin)
            || self.color_slots.contains_key(pane_origin)
        {
            return false;
        }
        let slot = (WORK_STREAM_COLOR_COUNT - 1) as u8;
        self.color_slots.retain(|_, assigned| *assigned != slot);
        self.color_slots.insert(pane_origin.to_string(), slot);
        true
    }

    pub fn marker(&self, pane_origin: &str) -> Option<String> {
        self.origins
            .iter()
            .any(|known| known == pane_origin)
            .then(|| {
                self.color_slot(pane_origin)
                    .map_or_else(|| "OVF".to_string(), |slot| format!("P{}", slot + 1))
            })
    }

    pub fn color_slot(&self, pane_origin: &str) -> Option<u8> {
        self.color_slots.get(pane_origin).copied()
    }

    fn remove_origin(&mut self, pane_origin: &str) {
        self.origins.retain(|origin| origin != pane_origin);
        self.color_slots.remove(pane_origin);
    }

    fn sanitize(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.origins.retain(|origin| {
            valid_opaque(origin, MAX_ORIGIN_LEN, false) && seen.insert(origin.clone())
        });
        self.origins.truncate(DEFAULT_CAPACITY);
        let previous = std::mem::take(&mut self.color_slots);
        let mut assigned = std::collections::HashSet::new();
        for origin in &self.origins {
            let Some(slot) = previous.get(origin).copied() else {
                continue;
            };
            if usize::from(slot) < WORK_STREAM_COLOR_COUNT && assigned.insert(slot) {
                self.color_slots.insert(origin.clone(), slot);
            }
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkStreamPreferences {
    #[serde(default)]
    pub palette: WorkStreamPalette,
    #[serde(default)]
    pub filter: WorkStreamFilter,
}

impl WorkStreamPreferences {
    fn sanitize(&mut self) {
        self.palette.sanitize();
        let valid = match &self.filter {
            WorkStreamFilter::All | WorkStreamFilter::Attention | WorkStreamFilter::History => true,
            WorkStreamFilter::Pane(origin) => {
                valid_opaque(origin, MAX_ORIGIN_LEN, false)
                    && self.palette.origins.iter().any(|known| known == origin)
            }
            WorkStreamFilter::Task(task) => valid_display(task, MAX_TASK_ID_LEN),
        };
        if !valid {
            self.filter = WorkStreamFilter::All;
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkReconciliationStatus {
    Pending,
    Verified,
    Stale,
    Unverified,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkReconciliation {
    pub status: WorkReconciliationStatus,
    pub source: String,
    pub reason: String,
    pub origin_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at_unix_ms: Option<u64>,
}

impl WorkReconciliation {
    fn pending(source: &str, reason: &str) -> Self {
        Self {
            status: WorkReconciliationStatus::Pending,
            source: source.to_string(),
            reason: reason.to_string(),
            origin_status: "unknown".to_string(),
            current_value: None,
            checked_at_unix_ms: None,
        }
    }

    fn checked(status: WorkReconciliationStatus, source: &str, reason: &str, now: u64) -> Self {
        Self {
            status,
            source: source.to_string(),
            reason: reason.to_string(),
            origin_status: "unknown".to_string(),
            current_value: None,
            checked_at_unix_ms: Some(now),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkLedgerRestoreHealth {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub loaded_records: usize,
    pub rejected_records: usize,
}

impl Default for WorkLedgerRestoreHealth {
    fn default() -> Self {
        Self {
            status: "empty".to_string(),
            detail: None,
            loaded_records: 0,
            rejected_records: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkStreamEntry {
    pub record: WorkRecord,
    pub marker: String,
    pub color_slot: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkStreamLegendItem {
    pub pane_origin: String,
    pub workspace_origin: String,
    pub tab_origin: String,
    pub marker: String,
    pub color_slot: Option<u8>,
    pub workspace_id: u32,
    pub tab_id: u32,
    pub pane_id: u32,
    pub workspace_name: String,
    pub tab_name: String,
    pub agent_name: Option<String>,
    pub task_id: Option<String>,
    pub task_title: Option<String>,
    pub origin_state: WorkStreamOriginState,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkStreamProjection {
    pub entries: Vec<WorkStreamEntry>,
    pub legend: Vec<WorkStreamLegendItem>,
    pub overflow_count: usize,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalTaskState {
    Todo,
    InProgress,
    Blocked,
    Done,
    Cancelled,
    Absent,
    Unknown,
}

impl CanonicalTaskState {
    pub fn from_status(status: &str) -> Self {
        match status.trim().to_ascii_lowercase().as_str() {
            "todo" | "open" => Self::Todo,
            "in_progress" | "in-progress" | "doing" | "active" | "started" => Self::InProgress,
            "blocked" => Self::Blocked,
            "done" | "completed" | "closed" | "merged" => Self::Done,
            "cancelled" | "canceled" => Self::Cancelled,
            _ => Self::Unknown,
        }
    }

    fn is_live_open(self) -> bool {
        matches!(self, Self::Todo | Self::InProgress | Self::Blocked)
    }

    fn is_done_or_cancelled(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BindingState {
    Bound,
    Unbound,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Running,
    Idle,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct TaskTruthInput {
    pub pane_origin: String,
    pub task_id: Option<String>,
    pub origin: WorkStreamOriginState,
    pub bound_now: bool,
    pub execution_known: bool,
    pub agent_running: bool,
    pub canonical: CanonicalTaskState,
    pub reconciliation: WorkReconciliation,
    pub pull_request: Option<WorkPullRequestRef>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TruthMismatch {
    pub source: String,
    pub detail: String,
    pub current_value: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct TaskTruth {
    pub pane_origin: String,
    pub task_id: Option<String>,
    pub canonical: CanonicalTaskState,
    pub binding: BindingState,
    pub execution: ExecutionState,
    pub origin: WorkStreamOriginState,
    pub verification: WorkReconciliationStatus,
    pub verification_source: String,
    pub mismatch: Option<TruthMismatch>,
    pub last_checked_unix_ms: Option<u64>,
    pub counts_as_live_open: bool,
}

pub fn project_task_truth(input: &TaskTruthInput) -> TaskTruth {
    let binding = if input.origin == WorkStreamOriginState::Historical {
        BindingState::Unbound
    } else if input.bound_now {
        BindingState::Bound
    } else {
        BindingState::Unbound
    };
    let execution = match input.origin {
        WorkStreamOriginState::Lazy | WorkStreamOriginState::Historical => ExecutionState::Unknown,
        WorkStreamOriginState::Live if !input.execution_known => ExecutionState::Unknown,
        WorkStreamOriginState::Live if input.agent_running => ExecutionState::Running,
        WorkStreamOriginState::Live => ExecutionState::Idle,
    };
    // A stale GitHub observation is not itself a cross-authority mismatch: an
    // old open-PR row can legitimately be superseded by a merged PR. Only the
    // exact, currently checked GitHub value may disagree with canonical task
    // truth. When GitHub is unavailable `current_value` is absent, so we fail
    // closed instead of presenting a previously-open PR as a verified mismatch.
    let current_github_pr_is_open = input.pull_request.is_some()
        && input.reconciliation.source == "github"
        && input
            .reconciliation
            .current_value
            .as_deref()
            .and_then(|value| value.split_whitespace().next())
            .is_some_and(|state| state.eq_ignore_ascii_case("open"));
    let canonical_disagrees_with_open_pr = matches!(
        input.canonical,
        CanonicalTaskState::Done
            | CanonicalTaskState::Absent
            | CanonicalTaskState::Todo
            | CanonicalTaskState::Cancelled
    );
    let mismatch = (input.reconciliation.status == WorkReconciliationStatus::Stale
        && current_github_pr_is_open
        && canonical_disagrees_with_open_pr)
        .then(|| TruthMismatch {
            source: input.reconciliation.source.clone(),
            detail: input.reconciliation.reason.clone(),
            current_value: input.reconciliation.current_value.clone(),
        });
    TaskTruth {
        pane_origin: input.pane_origin.clone(),
        task_id: input.task_id.clone(),
        canonical: input.canonical,
        binding,
        execution,
        origin: input.origin,
        verification: input.reconciliation.status,
        verification_source: input.reconciliation.source.clone(),
        mismatch,
        last_checked_unix_ms: input.reconciliation.checked_at_unix_ms,
        counts_as_live_open: input.origin != WorkStreamOriginState::Historical
            && input.canonical.is_live_open(),
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq, Default)]
pub struct TaskTruthSummary {
    pub items: Vec<TaskTruth>,
    pub live_open_count: usize,
    pub historical_count: usize,
    pub mismatch_count: usize,
}

pub fn project_task_truth_summary(inputs: &[TaskTruthInput]) -> TaskTruthSummary {
    let items = inputs.iter().map(project_task_truth).collect::<Vec<_>>();
    TaskTruthSummary {
        live_open_count: items.iter().filter(|item| item.counts_as_live_open).count(),
        historical_count: items
            .iter()
            .filter(|item| {
                item.origin == WorkStreamOriginState::Historical
                    || item.canonical.is_done_or_cancelled()
            })
            .count(),
        mismatch_count: items.iter().filter(|item| item.mismatch.is_some()).count(),
        items,
    }
}

pub fn work_stream_needs_attention(record: &WorkRecord) -> bool {
    record.kind == WorkKind::AgentReportedBlocked
}

pub fn project_work_stream(
    records: &[WorkRecord],
    palette: &mut WorkStreamPalette,
    filter: &WorkStreamFilter,
    metadata: &HashMap<String, WorkStreamPaneMeta>,
) -> WorkStreamProjection {
    let mut visible_origins = metadata
        .iter()
        .filter(|(_, meta)| meta.origin_state != WorkStreamOriginState::Historical)
        .collect::<Vec<_>>();
    visible_origins.sort_by_key(|(_, meta)| meta.display_order);
    palette.observe_origins(visible_origins.iter().map(|(origin, _)| origin.as_str()));
    palette.observe(records);
    let mut ordered = records.to_vec();
    ordered.sort_by_key(|record| record.seq);

    let mut latest = HashMap::<String, WorkIdentity>::new();
    for record in &ordered {
        latest.insert(record.identity.pane_origin.clone(), record.identity.clone());
    }
    for (origin, meta) in metadata {
        if let Some(identity) = meta.identity.as_ref() {
            latest.insert(origin.clone(), identity.clone());
        }
    }
    let mut legend = palette
        .origins
        .iter()
        .filter_map(|origin| {
            let identity = latest.get(origin)?;
            let meta = metadata.get(origin).cloned().unwrap_or_default();
            Some(WorkStreamLegendItem {
                pane_origin: origin.clone(),
                workspace_origin: identity.workspace_origin.clone(),
                tab_origin: identity.tab_origin.clone(),
                marker: palette.marker(origin)?,
                color_slot: palette.color_slot(origin),
                workspace_id: identity.workspace_id,
                tab_id: identity.tab_id,
                pane_id: identity.pane_id,
                workspace_name: identity.workspace_name.clone(),
                tab_name: identity.tab_name.clone(),
                agent_name: meta.agent_name,
                task_id: identity.task_id.clone(),
                task_title: identity.task_title.clone(),
                origin_state: meta.origin_state,
            })
        })
        .collect::<Vec<_>>();
    legend.sort_by_key(|item| {
        palette
            .origins
            .iter()
            .position(|origin| origin == &item.pane_origin)
            .unwrap_or(usize::MAX)
    });

    let latest_task_status = ordered
        .iter()
        .filter_map(|record| {
            Some((
                (
                    record.identity.pane_origin.clone(),
                    record.identity.task_id.clone()?,
                ),
                record
                    .task_status
                    .as_deref()
                    .map(CanonicalTaskState::from_status)?,
            ))
        })
        .collect::<HashMap<_, _>>();
    let entries = ordered
        .into_iter()
        .filter(|record| {
            let historical = metadata
                .get(&record.identity.pane_origin)
                .is_some_and(|meta| meta.origin_state == WorkStreamOriginState::Historical)
                || record
                    .identity
                    .task_id
                    .as_ref()
                    .and_then(|task_id| {
                        latest_task_status
                            .get(&(record.identity.pane_origin.clone(), task_id.clone()))
                    })
                    .is_some_and(|status| status.is_done_or_cancelled());
            match filter {
                WorkStreamFilter::All => !historical,
                WorkStreamFilter::Pane(origin) => record.identity.pane_origin == *origin,
                WorkStreamFilter::Task(task_id) => {
                    record.identity.task_id.as_deref() == Some(task_id)
                }
                WorkStreamFilter::Attention => !historical && work_stream_needs_attention(record),
                WorkStreamFilter::History => historical,
            }
        })
        .map(|record| {
            let origin = record.identity.pane_origin.clone();
            WorkStreamEntry {
                marker: palette.marker(&origin).unwrap_or_else(|| "P?".to_string()),
                color_slot: palette.color_slot(&origin),
                record,
            }
        })
        .collect();
    let overflow_count = legend
        .iter()
        .filter(|item| item.color_slot.is_none())
        .count();
    WorkStreamProjection {
        entries,
        legend,
        overflow_count,
    }
}

const MAX_SESSION_LEN: usize = 256;
const MAX_ORIGIN_LEN: usize = 128;
const MAX_NAME_LEN: usize = 160;
const MAX_TASK_ID_LEN: usize = 128;
const MAX_TASK_TITLE_LEN: usize = 320;
const MAX_SUMMARY_LEN: usize = 400;

fn valid_opaque(value: &str, max_len: usize, allow_empty: bool) -> bool {
    (allow_empty || !value.is_empty())
        && value.len() <= max_len
        && !value.chars().any(char::is_control)
}

fn valid_display(value: &str, max_len: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max_len && !value.chars().any(char::is_control)
}

fn generic_summary(kind: WorkKind) -> &'static str {
    match kind {
        WorkKind::PaneBound => "Pane task binding changed",
        WorkKind::PaneUnbound => "Pane task binding cleared",
        WorkKind::AgentStateChanged => "Agent state changed",
        WorkKind::AssistantMessageCompleted => "Assistant message completed",
        WorkKind::FileTouched => "Agent changed a file",
        WorkKind::TaskStatusChanged => "Task status changed",
        WorkKind::TaskCreated => "A task was added to .plan",
        WorkKind::AgentReportedStarted => "Agent reported work started",
        WorkKind::AgentReportedProgress => "Agent reported progress",
        WorkKind::AgentReportedBlocked => "Agent reported blocked",
        WorkKind::AgentReportedFinished => "Agent reported work finished",
        WorkKind::PullRequestOpened => "Pull request opened",
        WorkKind::PullRequestDraftChanged => "Pull request draft readiness changed",
        WorkKind::PullRequestReviewChanged => "Pull request review decision changed",
        WorkKind::PullRequestChecksReady => "Pull request checks became ready",
        WorkKind::PullRequestMerged => "Pull request merged",
        WorkKind::PullRequestClosed => "Pull request closed",
    }
}

fn sanitize_record_fields(
    kind: WorkKind,
    summary: &mut String,
    identity: &mut WorkIdentity,
) -> bool {
    // Stable identifiers are opaque correlation keys. Rewriting them would
    // silently merge or orphan histories, so unsafe values reject the record.
    if !valid_opaque(&identity.session, MAX_SESSION_LEN, false)
        || !valid_opaque(&identity.pane_origin, MAX_ORIGIN_LEN, false)
        || !valid_opaque(&identity.workspace_origin, MAX_ORIGIN_LEN, true)
        || !valid_opaque(&identity.tab_origin, MAX_ORIGIN_LEN, true)
    {
        return false;
    }

    if !valid_display(&identity.workspace_name, MAX_NAME_LEN) {
        identity.workspace_name = "Workspace".to_string();
    }
    if !valid_display(&identity.tab_name, MAX_NAME_LEN) {
        identity.tab_name = "Tab".to_string();
    }
    let task_fields_safe = identity
        .task_id
        .as_deref()
        .is_none_or(|value| valid_display(value, MAX_TASK_ID_LEN))
        && identity
            .task_title
            .as_deref()
            .is_none_or(|value| valid_display(value, MAX_TASK_TITLE_LEN));
    if !task_fields_safe {
        identity.task_id = None;
        identity.task_title = None;
    }
    if !valid_display(summary, MAX_SUMMARY_LEN) {
        *summary = generic_summary(kind).to_string();
    }
    true
}

fn valid_agent_report_boundary(
    kind: WorkKind,
    evidence_source: EvidenceSource,
    summary: &str,
) -> bool {
    let report_kind = matches!(
        kind,
        WorkKind::AgentReportedStarted
            | WorkKind::AgentReportedProgress
            | WorkKind::AgentReportedBlocked
            | WorkKind::AgentReportedFinished
    );
    if evidence_source == EvidenceSource::AgentReport || report_kind {
        return evidence_source == EvidenceSource::AgentReport
            && crate::work_reporting::validate_agent_report_summary(kind, summary).is_ok();
    }
    true
}

fn sanitize_work_draft(mut draft: WorkDraft) -> Option<WorkDraft> {
    if !valid_agent_report_boundary(draft.kind, draft.evidence_source, &draft.summary) {
        return None;
    }
    sanitize_record_fields(draft.kind, &mut draft.summary, &mut draft.identity).then_some(draft)
}

fn is_pull_request_kind(kind: WorkKind) -> bool {
    matches!(
        kind,
        WorkKind::PullRequestOpened
            | WorkKind::PullRequestDraftChanged
            | WorkKind::PullRequestReviewChanged
            | WorkKind::PullRequestChecksReady
            | WorkKind::PullRequestMerged
            | WorkKind::PullRequestClosed
    )
}

fn valid_pull_request_ref(pr: &WorkPullRequestRef) -> bool {
    let Some((url_repository, url_number)) = crate::tracking::github_pr_identity_from_url(&pr.url)
    else {
        return false;
    };
    let repository = pr.repository.to_ascii_lowercase();
    pr.number > 0
        && pr.repository == repository
        && repository == url_repository
        && pr.number == url_number
        && valid_display(&pr.repository, 201)
        && valid_display(&pr.title, 320)
        && matches!(pr.state.as_str(), "open" | "merged" | "closed")
        && pr
            .review_decision
            .as_deref()
            .is_none_or(|value| valid_display(value, 64))
}

fn sanitize_work_record(mut record: WorkRecord) -> Option<WorkRecord> {
    if !valid_agent_report_boundary(record.kind, record.evidence_source, &record.summary) {
        return None;
    }
    let pr_kind = is_pull_request_kind(record.kind);
    if pr_kind != record.pull_request.is_some()
        || pr_kind
            && (record.evidence_source != EvidenceSource::GitHubPullRequest
                || record.authority != WorkAuthority::GitHubCanonical
                || record.verification != VerificationState::RemoteVerified
                || record
                    .pull_request
                    .as_ref()
                    .is_none_or(|pr| !valid_pull_request_ref(pr)))
    {
        return None;
    }
    if record.task_status.as_deref().is_some_and(|status| {
        !matches!(
            status,
            "todo" | "in_progress" | "in-progress" | "blocked" | "done" | "cancelled"
        )
    }) {
        record.task_status = None;
    }
    sanitize_record_fields(record.kind, &mut record.summary, &mut record.identity).then_some(record)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedLedger {
    schema: String,
    session: String,
    next_seq: u64,
    records: VecDeque<WorkRecord>,
    #[serde(default)]
    view: WorkStreamPreferences,
    #[serde(default)]
    last_message_ordinals: HashMap<String, u64>,
    #[serde(default)]
    last_file_ordinals: HashMap<String, u64>,
    #[serde(default)]
    pull_request_snapshots: HashMap<String, PullRequestStateSnapshot>,
}

fn valid_replay_scope(scope: &str) -> bool {
    !scope.is_empty() && scope.len() <= 1_024 && !scope.chars().any(char::is_control)
}

fn retain_bounded_by_key<T>(values: &mut HashMap<String, T>, limit: usize) -> bool {
    if values.len() <= limit {
        return false;
    }
    let mut keys = values.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    keys.truncate(limit);
    let retained = keys.into_iter().collect::<std::collections::HashSet<_>>();
    values.retain(|key, _| retained.contains(key));
    true
}

fn make_room_for_new_key<T>(values: &mut HashMap<String, T>, key: &str, limit: usize) {
    if values.contains_key(key) || values.len() < limit {
        return;
    }
    if let Some(eviction_key) = values.keys().min().cloned() {
        values.remove(&eviction_key);
    }
}

fn sanitize_ordinal_watermarks(values: &mut HashMap<String, u64>) -> bool {
    let before = values.len();
    values.retain(|scope, ordinal| valid_replay_scope(scope) && *ordinal > 0);
    retain_bounded_by_key(values, MAX_REPLAY_WATERMARKS) || values.len() != before
}

fn sanitize_pr_snapshots(values: &mut HashMap<String, PullRequestStateSnapshot>) -> bool {
    let before = values.len();
    values.retain(|scope, snapshot| {
        valid_replay_scope(scope)
            && matches!(snapshot.state.as_str(), "open" | "merged" | "closed")
            && snapshot
                .review_decision
                .as_deref()
                .is_none_or(|value| valid_display(value, 64))
    });
    retain_bounded_by_key(values, MAX_PR_SNAPSHOTS) || values.len() != before
}

enum PersistCommand {
    Wake,
    Flush(mpsc::Sender<io::Result<()>>),
    Shutdown,
}

struct PersistenceWorker {
    tx: Option<mpsc::Sender<PersistCommand>>,
    latest: Arc<Mutex<Option<PersistedLedger>>>,
    wake_pending: Arc<AtomicBool>,
    stats: Arc<PersistenceStats>,
    join: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct PersistenceStats {
    snapshot_requests: AtomicUsize,
    serializations: AtomicUsize,
    writes: AtomicUsize,
    last_serialization_thread: Mutex<Option<std::thread::ThreadId>>,
}

fn drain_latest_snapshot(
    path: &Path,
    latest: &Mutex<Option<PersistedLedger>>,
    wake_pending: &AtomicBool,
    stats: &PersistenceStats,
) -> (bool, io::Result<()>) {
    let mut attempted = false;
    let mut outcome = Ok(());
    loop {
        let snapshot = latest.lock().ok().and_then(|mut slot| slot.take());
        if let Some(snapshot) = snapshot {
            attempted = true;
            stats.serializations.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut thread) = stats.last_serialization_thread.lock() {
                *thread = Some(std::thread::current().id());
            }
            let write_result = match serde_json::to_string_pretty(&snapshot) {
                Ok(json) => match write_atomically(path, &json) {
                    Ok(()) => {
                        stats.writes.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    }
                    Err(error) => Err(error),
                },
                Err(error) => Err(io::Error::other(format!(
                    "could not serialize work ledger: {error}"
                ))),
            };
            if let Err(error) = &write_result {
                eprintln!("taarof: could not persist work ledger: {error}");
            }
            // Only the newest coalesced snapshot determines durability; a
            // later successful write contains and supersedes earlier state.
            outcome = write_result;
        }

        wake_pending.store(false, Ordering::Release);
        let has_latest = latest.lock().is_ok_and(|slot| slot.is_some());
        if !has_latest || wake_pending.swap(true, Ordering::AcqRel) {
            break;
        }
    }
    (attempted, outcome)
}

impl PersistenceWorker {
    fn start(path: PathBuf) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let latest = Arc::new(Mutex::new(None));
        let wake_pending = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(PersistenceStats::default());
        let thread_latest = Arc::clone(&latest);
        let thread_wake_pending = Arc::clone(&wake_pending);
        let thread_stats = Arc::clone(&stats);
        let join = std::thread::Builder::new()
            .name("taarof-work-ledger-writer".to_string())
            .spawn(move || {
                let mut last_write_error: Option<String> = None;
                while let Ok(command) = rx.recv() {
                    match command {
                        PersistCommand::Wake => {
                            let (attempted, result) = drain_latest_snapshot(
                                &path,
                                &thread_latest,
                                &thread_wake_pending,
                                &thread_stats,
                            );
                            if attempted {
                                last_write_error = result.err().map(|error| error.to_string());
                            }
                        }
                        PersistCommand::Flush(done) => {
                            let (attempted, result) = drain_latest_snapshot(
                                &path,
                                &thread_latest,
                                &thread_wake_pending,
                                &thread_stats,
                            );
                            if attempted {
                                last_write_error = result.err().map(|error| error.to_string());
                            }
                            let acknowledged = last_write_error.as_ref().map_or_else(
                                || Ok(()),
                                |error| Err(io::Error::other(error.clone())),
                            );
                            let _ = done.send(acknowledged);
                        }
                        PersistCommand::Shutdown => {
                            let _ = drain_latest_snapshot(
                                &path,
                                &thread_latest,
                                &thread_wake_pending,
                                &thread_stats,
                            );
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            tx: Some(tx),
            latest,
            wake_pending,
            stats,
            join: Some(join),
        })
    }

    fn schedule(&self, snapshot: PersistedLedger) {
        self.stats.snapshot_requests.fetch_add(1, Ordering::Relaxed);
        let Ok(mut latest) = self.latest.lock() else {
            return;
        };
        *latest = Some(snapshot);
        drop(latest);
        if !self.wake_pending.swap(true, Ordering::AcqRel)
            && self
                .tx
                .as_ref()
                .is_none_or(|tx| tx.send(PersistCommand::Wake).is_err())
        {
            self.wake_pending.store(false, Ordering::Release);
        }
    }
}

impl Drop for PersistenceWorker {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(PersistCommand::Shutdown);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub struct WorkLedger {
    session: String,
    next_seq: u64,
    records: VecDeque<WorkRecord>,
    capacity: usize,
    max_age_ms: u64,
    last_fingerprints: HashMap<String, String>,
    last_activity: HashMap<String, String>,
    last_message_ordinals: HashMap<String, u64>,
    last_file_ordinals: HashMap<String, u64>,
    plan_snapshots: HashMap<String, HashMap<String, (String, String, bool)>>,
    pull_request_snapshots: HashMap<String, PullRequestStateSnapshot>,
    reconciliation: HashMap<u64, WorkReconciliation>,
    view: WorkStreamPreferences,
    restore_health: WorkLedgerRestoreHealth,
    last_pr_reconciliation_at: u64,
    persistence_batch_depth: usize,
    persistence_dirty: bool,
    persistence: Option<PersistenceWorker>,
    history_sink: Option<crate::history::HistoryHandle>,
}

impl Default for WorkLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkLedger {
    pub fn new() -> Self {
        let session = crate::instance::session_name().unwrap_or_else(|| "default".to_string());
        #[cfg(test)]
        {
            Self::empty(session, DEFAULT_CAPACITY, DEFAULT_MAX_AGE_MS, None)
        }
        #[cfg(not(test))]
        {
            let path = storage_path();
            Self::load_or_empty(session, path, DEFAULT_CAPACITY, DEFAULT_MAX_AGE_MS)
        }
    }

    fn empty(
        session: String,
        capacity: usize,
        max_age_ms: u64,
        persistence: Option<PersistenceWorker>,
    ) -> Self {
        Self {
            session,
            next_seq: 1,
            records: VecDeque::new(),
            capacity: capacity.max(1),
            max_age_ms,
            last_fingerprints: HashMap::new(),
            last_activity: HashMap::new(),
            last_message_ordinals: HashMap::new(),
            last_file_ordinals: HashMap::new(),
            plan_snapshots: HashMap::new(),
            pull_request_snapshots: HashMap::new(),
            reconciliation: HashMap::new(),
            view: WorkStreamPreferences::default(),
            restore_health: WorkLedgerRestoreHealth::default(),
            last_pr_reconciliation_at: 0,
            persistence_batch_depth: 0,
            persistence_dirty: false,
            persistence,
            history_sink: None,
        }
    }

    fn load_or_empty(session: String, path: PathBuf, capacity: usize, max_age_ms: u64) -> Self {
        Self::load_or_empty_with_start(
            session,
            path,
            capacity,
            max_age_ms,
            PersistenceWorker::start,
        )
    }

    fn load_or_empty_with_start<F>(
        session: String,
        path: PathBuf,
        capacity: usize,
        max_age_ms: u64,
        start: F,
    ) -> Self
    where
        F: FnOnce(PathBuf) -> io::Result<PersistenceWorker>,
    {
        let load_result = match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<PersistedLedger>(&raw)
                .map(Some)
                .map_err(|error| format!("ledger file is corrupt or partial: {error}")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("ledger file could not be read: {error}")),
        };
        let persistence = match start(path) {
            Ok(worker) => Some(worker),
            Err(error) => {
                eprintln!(
                    "taarof: work ledger persistence unavailable; continuing in memory: {error}"
                );
                None
            }
        };
        let mut ledger = Self::empty(session, capacity, max_age_ms, persistence);
        match load_result {
            Ok(Some(mut saved))
                if matches!(
                    saved.schema.as_str(),
                    "taarof.work-ledger.v1" | "taarof.work-ledger.v2" | "taarof.work-ledger.v3"
                ) && saved.session == ledger.session =>
            {
                saved.view.sanitize();
                ledger.view = saved.view;
                let metadata_repaired =
                    sanitize_ordinal_watermarks(&mut saved.last_message_ordinals)
                        | sanitize_ordinal_watermarks(&mut saved.last_file_ordinals)
                        | sanitize_pr_snapshots(&mut saved.pull_request_snapshots);
                ledger.last_message_ordinals = saved.last_message_ordinals;
                ledger.last_file_ordinals = saved.last_file_ordinals;
                ledger.pull_request_snapshots = saved.pull_request_snapshots;
                let loaded_count = saved.records.len();
                let mut previous_seq = 0;
                ledger.records = saved
                    .records
                    .into_iter()
                    .filter_map(sanitize_work_record)
                    .filter(|record| {
                        let valid =
                            record.identity.session == ledger.session && record.seq > previous_seq;
                        if valid {
                            previous_seq = record.seq;
                        }
                        valid
                    })
                    .collect();
                for record in &ledger.records {
                    let Some(pr) = record.pull_request.as_ref() else {
                        continue;
                    };
                    let Some(task_id) = record.identity.task_id.as_deref() else {
                        continue;
                    };
                    if pr.state.is_empty() {
                        continue;
                    }
                    let scope = format!(
                        "{}:{}:{}:{}",
                        record.identity.pane_origin, pr.repository, pr.number, task_id
                    );
                    ledger
                        .pull_request_snapshots
                        .entry(scope)
                        .or_insert_with(|| PullRequestStateSnapshot {
                            state: pr.state.clone(),
                            draft: pr.is_draft,
                            review_decision: pr.review_decision.clone(),
                            checks: pr.checks,
                        });
                }
                let rejected_unsafe_record = ledger.records.len() != loaded_count;
                let pruned = ledger.prune(crate::events::unix_time_ms());
                let mut sequence_repaired = saved.next_seq >= MAX_SAFE_NEXT_SEQ;
                let last_seq = ledger.records.back().map_or(0, |record| record.seq);
                if last_seq >= MAX_SAFE_NEXT_SEQ.saturating_sub(1) {
                    for (index, record) in ledger.records.iter_mut().enumerate() {
                        record.seq = u64::try_from(index).unwrap_or(0).saturating_add(1);
                    }
                    sequence_repaired = true;
                }
                let last_seq = ledger.records.back().map_or(0, |record| record.seq);
                ledger.next_seq = if sequence_repaired {
                    last_seq.saturating_add(1).max(1)
                } else {
                    saved.next_seq.max(last_seq.saturating_add(1)).max(1)
                };
                let rejected_records = loaded_count.saturating_sub(ledger.records.len());
                let mut restore_details = Vec::new();
                if rejected_unsafe_record || pruned {
                    restore_details
                        .push("Unsafe, corrupt, or expired records were omitted during restore");
                }
                if metadata_repaired {
                    restore_details.push("Replay metadata was repaired during restore");
                }
                if sequence_repaired {
                    restore_details.push("Sequence values were repaired during restore");
                }
                ledger.restore_health = WorkLedgerRestoreHealth {
                    status: if rejected_unsafe_record
                        || pruned
                        || metadata_repaired
                        || sequence_repaired
                    {
                        "degraded".to_string()
                    } else {
                        "restored".to_string()
                    },
                    detail: (!restore_details.is_empty()).then(|| restore_details.join("; ")),
                    loaded_records: ledger.records.len(),
                    rejected_records,
                };
                ledger.initialize_reconciliation();
                if pruned
                    || rejected_unsafe_record
                    || metadata_repaired
                    || sequence_repaired
                    || saved.schema != "taarof.work-ledger.v3"
                {
                    ledger.schedule_persist();
                }
            }
            Ok(Some(_)) => {
                ledger.restore_health = WorkLedgerRestoreHealth {
                    status: "degraded".to_string(),
                    detail: Some(
                        "Ledger belongs to another session or uses an unsupported schema"
                            .to_string(),
                    ),
                    loaded_records: 0,
                    rejected_records: 0,
                };
            }
            Ok(None) => {}
            Err(detail) => {
                ledger.restore_health = WorkLedgerRestoreHealth {
                    status: "degraded".to_string(),
                    detail: Some(detail),
                    loaded_records: 0,
                    rejected_records: 0,
                };
            }
        }
        if ledger.persistence.is_none() {
            ledger.restore_health.status = "persistence_unavailable".to_string();
            ledger.restore_health.detail =
                Some("Work history is available in memory but cannot be persisted".to_string());
        }
        ledger
    }

    #[cfg(test)]
    fn with_storage(session: &str, path: PathBuf, capacity: usize, max_age_ms: u64) -> Self {
        Self::load_or_empty(session.to_string(), path, capacity, max_age_ms)
    }

    pub fn append(&mut self, draft: WorkDraft) -> Option<WorkRecord> {
        self.append_at_with_history(crate::events::unix_time_ms(), draft, true)
    }

    fn append_without_history(&mut self, draft: WorkDraft) -> Option<WorkRecord> {
        self.append_at_with_history(crate::events::unix_time_ms(), draft, false)
    }

    fn record_history(&self, record: &WorkRecord) {
        if let Some(history) = &self.history_sink {
            history.try_record_work(record);
        }
    }

    fn append_pull_request(
        &mut self,
        draft: WorkDraft,
        pull_request: WorkPullRequestRef,
    ) -> Option<WorkRecord> {
        if !is_pull_request_kind(draft.kind)
            || draft.evidence_source != EvidenceSource::GitHubPullRequest
            || draft.authority != WorkAuthority::GitHubCanonical
            || draft.verification != VerificationState::RemoteVerified
            || !valid_pull_request_ref(&pull_request)
        {
            return None;
        }
        let mut record =
            self.append_at_with_history(crate::events::unix_time_ms(), draft, false)?;
        record.pull_request = Some(pull_request.clone());
        if let Some(stored) = self
            .records
            .back_mut()
            .filter(|stored| stored.seq == record.seq)
        {
            stored.pull_request = Some(pull_request);
        }
        self.schedule_persist();
        self.record_history(&record);
        Some(record)
    }

    #[cfg(test)]
    fn append_at(&mut self, ts_unix_ms: u64, draft: WorkDraft) -> Option<WorkRecord> {
        self.append_at_with_history(ts_unix_ms, draft, true)
    }

    fn append_at_with_history(
        &mut self,
        ts_unix_ms: u64,
        draft: WorkDraft,
        record_history: bool,
    ) -> Option<WorkRecord> {
        if self.next_seq >= MAX_SAFE_NEXT_SEQ {
            return None;
        }
        let draft = sanitize_work_draft(draft)?;
        if self
            .last_fingerprints
            .get(&draft.dedupe_scope)
            .is_some_and(|last| last == &draft.fingerprint)
        {
            return None;
        }
        self.last_fingerprints
            .insert(draft.dedupe_scope.clone(), draft.fingerprint.clone());
        let record = WorkRecord {
            seq: self.next_seq,
            ts_unix_ms,
            kind: draft.kind,
            summary: draft.summary,
            identity: draft.identity,
            evidence_source: draft.evidence_source,
            authority: draft.authority,
            verification: draft.verification,
            task_status: None,
            pull_request: None,
        };
        self.next_seq = self.next_seq.saturating_add(1);
        self.records.push_back(record.clone());
        if record_history {
            self.record_history(&record);
        }
        let reconciliation = match record.authority {
            WorkAuthority::PlanCanonical => WorkReconciliation::checked(
                WorkReconciliationStatus::Verified,
                "plan",
                "Observed directly from the current .plan file",
                ts_unix_ms,
            ),
            WorkAuthority::GitHubCanonical => WorkReconciliation::checked(
                WorkReconciliationStatus::Verified,
                "github",
                "Observed directly from GitHub",
                ts_unix_ms,
            ),
            WorkAuthority::AgentObservation
                if matches!(
                    record.kind,
                    WorkKind::AgentReportedStarted
                        | WorkKind::AgentReportedProgress
                        | WorkKind::AgentReportedBlocked
                        | WorkKind::AgentReportedFinished
                ) =>
            {
                WorkReconciliation::pending(
                    "plan",
                    "Agent report is observational until compared with .plan",
                )
            }
            _ => WorkReconciliation::checked(
                WorkReconciliationStatus::Verified,
                "session",
                "Observed by this Taarof session",
                ts_unix_ms,
            ),
        };
        self.reconciliation.insert(record.seq, reconciliation);
        self.prune(ts_unix_ms);
        self.schedule_persist();
        Some(record)
    }

    pub fn install_history_sink(&mut self, history: crate::history::HistoryHandle) {
        self.history_sink = Some(history);
    }

    fn prune(&mut self, now: u64) -> bool {
        let before = self.records.len();
        let cutoff = now.saturating_sub(self.max_age_ms);
        while self
            .records
            .front()
            .is_some_and(|record| record.ts_unix_ms < cutoff)
        {
            self.records.pop_front();
        }
        while self.records.len() > self.capacity {
            self.records.pop_front();
        }
        let retained = self
            .records
            .iter()
            .map(|record| record.seq)
            .collect::<std::collections::HashSet<_>>();
        self.reconciliation.retain(|seq, _| retained.contains(seq));
        self.records.len() != before
    }

    /// Enforce age/count retention even when no new records arrive. The
    /// periodic work probe calls this on the main thread; persistence remains
    /// queued to the background writer and performs no GTK-thread disk IO.
    pub fn prune_at(&mut self, now: u64) -> bool {
        let changed = self.prune(now);
        if changed {
            self.schedule_persist();
        }
        changed
    }

    fn schedule_persist(&mut self) {
        if self.persistence_batch_depth > 0 {
            self.persistence_dirty = true;
            return;
        }
        let Some(worker) = &self.persistence else {
            return;
        };
        if !valid_opaque(&self.session, MAX_SESSION_LEN, false) {
            return;
        }
        let saved = PersistedLedger {
            schema: "taarof.work-ledger.v3".to_string(),
            session: self.session.clone(),
            next_seq: self.next_seq,
            records: self.records.clone(),
            view: self.view.clone(),
            last_message_ordinals: self.last_message_ordinals.clone(),
            last_file_ordinals: self.last_file_ordinals.clone(),
            pull_request_snapshots: self.pull_request_snapshots.clone(),
        };
        worker.schedule(saved);
    }

    fn begin_persistence_batch(&mut self) {
        self.persistence_batch_depth = self.persistence_batch_depth.saturating_add(1);
    }

    fn end_persistence_batch(&mut self) {
        self.persistence_batch_depth = self.persistence_batch_depth.saturating_sub(1);
        if self.persistence_batch_depth == 0 && std::mem::take(&mut self.persistence_dirty) {
            self.schedule_persist();
        }
    }

    pub fn records(&self) -> Vec<WorkRecord> {
        self.records.iter().cloned().collect()
    }

    pub fn view_preferences(&self) -> WorkStreamPreferences {
        self.view.clone()
    }

    pub fn set_view_preferences(&mut self, mut view: WorkStreamPreferences) {
        view.sanitize();
        if self.view != view {
            self.view = view;
            self.schedule_persist();
        }
    }

    fn marker_for_record(&mut self, record: &WorkRecord) -> (String, Option<u8>) {
        let before = self.view.palette.clone();
        self.view.palette.observe(std::slice::from_ref(record));
        if self.view.palette != before {
            self.schedule_persist();
        }
        (
            self.view
                .palette
                .marker(&record.identity.pane_origin)
                .unwrap_or_else(|| "OVF".to_string()),
            self.view.palette.color_slot(&record.identity.pane_origin),
        )
    }

    pub fn restore_health(&self) -> &WorkLedgerRestoreHealth {
        &self.restore_health
    }

    fn pull_request_reconciliation_due(&self, now: u64) -> bool {
        now.saturating_sub(self.last_pr_reconciliation_at) >= 30_000
    }

    pub fn reconciliation_for(&self, seq: u64) -> WorkReconciliation {
        self.reconciliation.get(&seq).cloned().unwrap_or_else(|| {
            WorkReconciliation::pending(
                "runtime",
                "Waiting for restored pane and canonical source reconciliation",
            )
        })
    }

    fn initialize_reconciliation(&mut self) {
        self.reconciliation.clear();
        for record in &self.records {
            let (source, reason) = match record.authority {
                WorkAuthority::PlanCanonical => ("plan", "Waiting for current .plan task state"),
                WorkAuthority::GitHubCanonical => {
                    ("github", "Waiting for current GitHub pull request state")
                }
                WorkAuthority::AgentObservation
                    if matches!(
                        record.kind,
                        WorkKind::AgentReportedStarted
                            | WorkKind::AgentReportedProgress
                            | WorkKind::AgentReportedBlocked
                            | WorkKind::AgentReportedFinished
                    ) =>
                {
                    ("plan", "Waiting to compare the agent report with .plan")
                }
                _ => ("session", "Historical session observation"),
            };
            let status = if source == "session" {
                WorkReconciliation::checked(
                    WorkReconciliationStatus::Verified,
                    source,
                    reason,
                    crate::events::unix_time_ms(),
                )
            } else {
                WorkReconciliation::pending(source, reason)
            };
            self.reconciliation.insert(record.seq, status);
        }
    }

    fn set_reconciliation(
        &mut self,
        seq: u64,
        mut next: WorkReconciliation,
    ) -> Option<(u64, WorkReconciliation)> {
        if next.origin_status == "unknown" {
            next.origin_status = self
                .reconciliation
                .get(&seq)
                .map(|current| current.origin_status.clone())
                .unwrap_or_else(|| "unknown".to_string());
        }
        if let Some(current) = self.reconciliation.get_mut(&seq) {
            if current.status == next.status
                && current.source == next.source
                && current.reason == next.reason
                && current.origin_status == next.origin_status
                && current.current_value == next.current_value
            {
                current.checked_at_unix_ms = next.checked_at_unix_ms;
                return None;
            }
        }
        self.reconciliation.insert(seq, next.clone());
        Some((seq, next))
    }

    pub fn clear_all(&mut self) -> usize {
        let removed = self.records.len();
        self.records.clear();
        self.reconciliation.clear();
        // Observation watermarks deliberately survive a clear. Otherwise the
        // next transcript/.plan/PR poll could replay deleted history.
        self.view = WorkStreamPreferences::default();
        self.restore_health = if self.persistence.is_some() {
            WorkLedgerRestoreHealth::default()
        } else {
            WorkLedgerRestoreHealth {
                status: "persistence_unavailable".to_string(),
                detail: Some(
                    "Work history is available in memory but cannot be persisted".to_string(),
                ),
                loaded_records: 0,
                rejected_records: 0,
            }
        };
        self.schedule_persist();
        removed
    }

    pub fn clear_pane(&mut self, pane_origin: &str) -> usize {
        let before = self.records.len();
        let removed_seqs = self
            .records
            .iter()
            .filter(|record| record.identity.pane_origin == pane_origin)
            .map(|record| record.seq)
            .collect::<Vec<_>>();
        self.records
            .retain(|record| record.identity.pane_origin != pane_origin);
        for seq in removed_seqs {
            self.reconciliation.remove(&seq);
        }
        let removed = before.saturating_sub(self.records.len());
        if removed > 0 {
            self.view.palette.remove_origin(pane_origin);
            if matches!(
                &self.view.filter,
                WorkStreamFilter::Pane(origin) if origin == pane_origin
            ) {
                self.view.filter = WorkStreamFilter::All;
            }
            self.schedule_persist();
        }
        removed
    }

    fn reconcile_runtime_origins(
        &mut self,
        live_origins: &std::collections::HashSet<&str>,
        now: u64,
    ) -> Vec<(u64, WorkReconciliation)> {
        let updates = self
            .records
            .iter()
            .filter_map(|record| {
                let origin_status = if live_origins.contains(record.identity.pane_origin.as_str()) {
                    "live"
                } else {
                    "historical"
                };
                if self
                    .reconciliation
                    .get(&record.seq)
                    .is_some_and(|current| current.origin_status == origin_status)
                {
                    return None;
                }
                let mut next = self.reconciliation_for(record.seq);
                next.origin_status = origin_status.to_string();
                next.checked_at_unix_ms = Some(now);
                Some((record.seq, next))
            })
            .collect::<Vec<_>>();
        updates
            .into_iter()
            .filter_map(|(seq, next)| self.set_reconciliation(seq, next))
            .collect()
    }

    fn reconcile_plan_for_pane(
        &mut self,
        pane: &PaneProbe,
        now: u64,
    ) -> Vec<(u64, WorkReconciliation)> {
        let updates = self
            .records
            .iter()
            .filter(|record| record.identity.pane_origin == pane.identity.pane_origin)
            .filter(|record| {
                record.authority == WorkAuthority::PlanCanonical
                    || matches!(
                        record.kind,
                        WorkKind::AgentReportedStarted
                            | WorkKind::AgentReportedProgress
                            | WorkKind::AgentReportedBlocked
                            | WorkKind::AgentReportedFinished
                    )
            })
            .filter_map(|record| {
                let task_id = record.identity.task_id.as_deref()?;
                let next = if !pane.plan_loaded {
                    WorkReconciliation::checked(
                        WorkReconciliationStatus::Stale,
                        "plan",
                        "Repository or .plan task source is unavailable",
                        now,
                    )
                } else if let Some(task) = pane.plan_tasks.iter().find(|task| task.id == task_id) {
                    let mut next = reconcile_record_with_plan(record, task, now);
                    next.current_value = Some(task.status.clone());
                    next
                } else {
                    WorkReconciliation::checked(
                        WorkReconciliationStatus::Stale,
                        "plan",
                        "Bound task no longer exists in .plan",
                        now,
                    )
                };
                Some((record.seq, next))
            })
            .collect::<Vec<_>>();
        updates
            .into_iter()
            .filter_map(|(seq, next)| self.set_reconciliation(seq, next))
            .collect()
    }

    fn reconcile_pull_requests(
        &mut self,
        verified: &VerifiedPullRequestBinding,
        result: Result<&crate::tracking::BranchPullRequestsData, &str>,
    ) -> Vec<(u64, WorkReconciliation)> {
        let now = crate::events::unix_time_ms();
        let current = result.ok().and_then(|data| {
            match crate::tracking::correlate_pull_request(
                &verified.task_id,
                &verified.base_repository,
                &verified.head_owner,
                &verified.branch,
                data,
            ) {
                crate::tracking::PullRequestCorrelation::Linked(pr) => Some(pr),
                _ => None,
            }
        });
        let unavailable = result.err();
        let updates = self
            .records
            .iter()
            .filter(|record| {
                record.identity.pane_origin == verified.identity.pane_origin
                    && record.identity.task_id.as_deref() == Some(verified.task_id.as_str())
                    && record.pull_request.is_some()
            })
            .map(|record| {
                let next = if unavailable.is_some() {
                    WorkReconciliation::checked(
                        WorkReconciliationStatus::Stale,
                        "github",
                        "GitHub is unavailable; preserving the last verified observation",
                        now,
                    )
                } else if let (Some(saved), Some(current)) = (record.pull_request.as_ref(), current)
                {
                    let current_matches = saved.number == current.number
                        && saved.state == current.state
                        && saved.is_draft == current.is_draft
                        && saved.review_decision == current.review_decision
                        && saved.checks == current.checks;
                    let mut next = WorkReconciliation::checked(
                        if current_matches {
                            WorkReconciliationStatus::Verified
                        } else {
                            WorkReconciliationStatus::Stale
                        },
                        "github",
                        if current_matches {
                            "GitHub still matches this pull request observation"
                        } else {
                            "GitHub pull request state has superseded this observation"
                        },
                        now,
                    );
                    next.current_value = Some(format!(
                        "{}{}",
                        current.state,
                        if current.is_draft { " (draft)" } else { "" }
                    ));
                    next
                } else {
                    WorkReconciliation::checked(
                        WorkReconciliationStatus::Stale,
                        "github",
                        "No unambiguous current pull request correlation was found",
                        now,
                    )
                };
                (record.seq, next)
            })
            .collect::<Vec<_>>();
        updates
            .into_iter()
            .filter_map(|(seq, next)| self.set_reconciliation(seq, next))
            .collect()
    }

    fn reconcile_exact_pull_request(
        &mut self,
        repository: &str,
        number: u64,
        result: Result<&crate::tracking::BranchPullRequestEntry, &str>,
    ) -> Vec<(u64, WorkReconciliation)> {
        let now = crate::events::unix_time_ms();
        let updates = self
            .records
            .iter()
            .filter(|record| {
                record
                    .pull_request
                    .as_ref()
                    .is_some_and(|pr| pr.repository == repository && pr.number == number)
            })
            .map(|record| {
                let next = match result {
                    Err(_) => WorkReconciliation::checked(
                        WorkReconciliationStatus::Stale,
                        "github",
                        "GitHub is unavailable; preserving the last verified observation",
                        now,
                    ),
                    Ok(current) => {
                        let saved = record.pull_request.as_ref().expect("filtered PR record");
                        let exact = current.number == number
                            && crate::tracking::github_pr_identity_from_url(
                                current.url.as_deref().unwrap_or(""),
                            ) == Some((repository.to_string(), number));
                        let matches = exact
                            && saved.state == current.state
                            && saved.is_draft == current.is_draft
                            && saved.review_decision == current.review_decision
                            && saved.checks == current.checks;
                        let mut next = WorkReconciliation::checked(
                            if matches {
                                WorkReconciliationStatus::Verified
                            } else {
                                WorkReconciliationStatus::Stale
                            },
                            "github",
                            if !exact {
                                "GitHub response did not match the stored repository and PR number"
                            } else if matches {
                                "GitHub still matches this pull request observation"
                            } else {
                                "GitHub pull request state has superseded this observation"
                            },
                            now,
                        );
                        if exact {
                            next.current_value = Some(format!(
                                "{}{}",
                                current.state,
                                if current.is_draft { " (draft)" } else { "" }
                            ));
                        }
                        next
                    }
                };
                (record.seq, next)
            })
            .collect::<Vec<_>>();
        updates
            .into_iter()
            .filter_map(|(seq, next)| self.set_reconciliation(seq, next))
            .collect()
    }

    fn projection_limit(&self) -> usize {
        self.capacity.max(1)
    }

    pub fn pane_records(&self, pane_origin: &str, limit: usize) -> Vec<WorkRecord> {
        let cutoff = crate::events::unix_time_ms().saturating_sub(self.max_age_ms);
        let mut records: Vec<_> = self
            .records
            .iter()
            .filter(|record| {
                record.ts_unix_ms >= cutoff && record.identity.pane_origin == pane_origin
            })
            .cloned()
            .collect();
        let keep = limit.max(1);
        if records.len() > keep {
            records.drain(0..records.len() - keep);
        }
        records
    }

    pub fn sidebar_records(&self, pane_origin: &str) -> Vec<WorkRecord> {
        self.pane_records(pane_origin, SIDEBAR_LIMIT)
    }

    pub fn metadata_json(&self) -> serde_json::Value {
        let cutoff = crate::events::unix_time_ms().saturating_sub(self.max_age_ms);
        let records: Vec<_> = self
            .records
            .iter()
            .filter(|record| record.ts_unix_ms >= cutoff)
            .cloned()
            .collect();
        let records = records
            .into_iter()
            .map(|record| {
                let reconciliation = self.reconciliation_for(record.seq);
                let mut value =
                    serde_json::to_value(record).unwrap_or_else(|_| serde_json::json!({}));
                if let Some(object) = value.as_object_mut() {
                    object.insert(
                        "reconciliation".to_string(),
                        serde_json::to_value(reconciliation).unwrap_or(serde_json::Value::Null),
                    );
                }
                value
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "schema": "taarof.work-ledger.v3",
            "session": valid_opaque(&self.session, MAX_SESSION_LEN, false)
                .then_some(self.session.as_str()),
            "next_seq": self.next_seq,
            "high_watermark": self.next_seq.saturating_sub(1),
            "stored": records.len(),
            "capacity": self.capacity,
            "max_age_ms": self.max_age_ms,
            "records": records,
            "view": self.view,
            "restore": self.restore_health,
        })
    }

    fn observe_activity(&mut self, pane_origin: &str, state: Option<&str>) -> Option<String> {
        let key = pane_origin.to_string();
        let next = state.unwrap_or("idle").to_string();
        let previous = self.last_activity.insert(key, next.clone());
        match previous {
            None if state.is_none() => None,
            Some(previous) if previous == next => None,
            _ => Some(next),
        }
    }

    fn observe_plan(
        &mut self,
        checkout: &str,
        tasks: &[ObservedPlanTask],
    ) -> Vec<(WorkKind, String, String, String)> {
        let next: HashMap<_, _> = tasks
            .iter()
            .filter(|task| {
                !task.id.trim().is_empty()
                    && !task.title.trim().is_empty()
                    && !task.status.trim().is_empty()
            })
            .map(|task| {
                (
                    task.id.clone(),
                    (
                        task.title.clone(),
                        task.status.clone(),
                        task.fully_specified,
                    ),
                )
            })
            .collect();
        let Some(previous) = self
            .plan_snapshots
            .insert(checkout.to_string(), next.clone())
        else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        for (id, (title, status, fully_specified)) in &next {
            match previous.get(id) {
                None if *fully_specified => changes.push((
                    WorkKind::TaskCreated,
                    id.clone(),
                    title.clone(),
                    format!("Task {id} was added to .plan"),
                )),
                Some((_, old_status, _)) if old_status != status => changes.push((
                    WorkKind::TaskStatusChanged,
                    id.clone(),
                    title.clone(),
                    format!("Task {id} status changed: {old_status} -> {status}"),
                )),
                _ => {}
            }
        }
        changes
    }

    fn observe_pull_request(
        &mut self,
        scope: &str,
        pr: &crate::tracking::BranchPullRequestEntry,
    ) -> Vec<(WorkKind, String)> {
        let next = PullRequestStateSnapshot {
            state: pr.state.clone(),
            draft: pr.is_draft,
            review_decision: pr.review_decision.clone(),
            checks: pr.checks,
        };
        make_room_for_new_key(&mut self.pull_request_snapshots, scope, MAX_PR_SNAPSHOTS);
        let previous = self
            .pull_request_snapshots
            .insert(scope.to_string(), next.clone());
        retain_bounded_by_key(&mut self.pull_request_snapshots, MAX_PR_SNAPSHOTS);
        self.schedule_persist();
        let Some(previous) = previous else {
            return match next.state.as_str() {
                "open" => vec![(
                    WorkKind::PullRequestOpened,
                    format!("PR #{} opened", pr.number),
                )],
                "merged" => vec![(
                    WorkKind::PullRequestMerged,
                    format!("PR #{} is merged", pr.number),
                )],
                "closed" => vec![(
                    WorkKind::PullRequestClosed,
                    format!("PR #{} is closed", pr.number),
                )],
                _ => Vec::new(),
            };
        };
        if previous == next {
            return Vec::new();
        }
        let mut changes = Vec::new();
        if previous.draft != next.draft && next.state == "open" {
            changes.push((
                WorkKind::PullRequestDraftChanged,
                format!(
                    "PR #{} changed to {}",
                    pr.number,
                    if next.draft {
                        "draft"
                    } else {
                        "ready for review"
                    }
                ),
            ));
        }
        if previous.review_decision != next.review_decision {
            let decision = next
                .review_decision
                .as_deref()
                .unwrap_or("none")
                .to_ascii_lowercase();
            changes.push((
                WorkKind::PullRequestReviewChanged,
                format!("PR #{} review decision: {decision}", pr.number),
            ));
        }
        if previous.checks != crate::tracking::PullRequestChecks::Ready
            && next.checks == crate::tracking::PullRequestChecks::Ready
        {
            changes.push((
                WorkKind::PullRequestChecksReady,
                format!("PR #{} checks are ready", pr.number),
            ));
        }
        if previous.state != next.state {
            match next.state.as_str() {
                "merged" => changes.push((
                    WorkKind::PullRequestMerged,
                    format!("PR #{} merged", pr.number),
                )),
                "closed" => changes.push((
                    WorkKind::PullRequestClosed,
                    format!("PR #{} closed", pr.number),
                )),
                "open" => changes.push((
                    WorkKind::PullRequestOpened,
                    format!("PR #{} opened", pr.number),
                )),
                _ => {}
            }
        }
        changes
    }

    fn note_file_ordinal(&mut self, scope: &str, ordinal: u64) -> bool {
        if !valid_replay_scope(scope) || ordinal == 0 {
            return false;
        }
        make_room_for_new_key(&mut self.last_file_ordinals, scope, MAX_REPLAY_WATERMARKS);
        let last = self
            .last_file_ordinals
            .entry(scope.to_string())
            .or_default();
        if ordinal <= *last {
            return false;
        }
        *last = ordinal;
        retain_bounded_by_key(&mut self.last_file_ordinals, MAX_REPLAY_WATERMARKS);
        self.schedule_persist();
        true
    }

    fn note_message_ordinal(&mut self, scope: &str, ordinal: u64) -> bool {
        if !valid_replay_scope(scope) {
            return false;
        }
        make_room_for_new_key(
            &mut self.last_message_ordinals,
            scope,
            MAX_REPLAY_WATERMARKS,
        );
        let last = self
            .last_message_ordinals
            .entry(scope.to_string())
            .or_default();
        if ordinal == 0 || ordinal <= *last {
            return false;
        }
        *last = ordinal;
        retain_bounded_by_key(&mut self.last_message_ordinals, MAX_REPLAY_WATERMARKS);
        self.schedule_persist();
        true
    }

    fn persistence_barrier(&self) -> Option<mpsc::Receiver<io::Result<()>>> {
        let Some(worker) = &self.persistence else {
            return None;
        };
        let (tx, rx) = mpsc::channel();
        worker
            .tx
            .as_ref()
            .expect("writer should be active")
            .send(PersistCommand::Flush(tx))
            .ok()?;
        Some(rx)
    }

    #[cfg(test)]
    fn flush(&self) {
        if let Some(rx) = self.persistence_barrier() {
            rx.recv()
                .expect("writer should acknowledge flush")
                .expect("writer should flush");
        }
    }

    #[cfg(test)]
    fn persistence_counts(&self) -> Option<(usize, usize, usize, Option<std::thread::ThreadId>)> {
        let worker = self.persistence.as_ref()?;
        Some((
            worker.stats.snapshot_requests.load(Ordering::Relaxed),
            worker.stats.serializations.load(Ordering::Relaxed),
            worker.stats.writes.load(Ordering::Relaxed),
            worker
                .stats
                .last_serialization_thread
                .lock()
                .ok()
                .and_then(|thread| *thread),
        ))
    }
}

#[cfg(not(test))]
fn storage_path() -> PathBuf {
    let root = dirs::data_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("taarof");
    storage_path_for(&root, crate::instance::session_name().as_deref())
}

fn storage_path_for(root: &Path, session: Option<&str>) -> PathBuf {
    match session {
        Some(session) => root.join(format!(
            "work-ledger-{}.json",
            crate::instance::session_storage_key_for(session)
        )),
        _ => root.join("work-ledger-default.json"),
    }
}

fn write_atomically(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    // `mode` only affects creation. A crash-left temp file may already exist
    // with broader permissions, so tighten it before any sensitive JSON write.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    fs::rename(tmp, path)
}

#[derive(Clone)]
struct PaneProbe {
    identity: WorkIdentity,
    activity: Option<String>,
    local_cwd: Option<String>,
    binding_checkout_root: Option<String>,
    binding_identity: Option<ProbeBindingIdentity>,
    plan_root: Option<String>,
    plan_tasks: Vec<ObservedPlanTask>,
    plan_loaded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProbeBindingIdentity {
    task_id: String,
    title: String,
    checkout_root: Option<String>,
    reporting_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedPlanTask {
    id: String,
    title: String,
    status: String,
    fully_specified: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct PullRequestStateSnapshot {
    state: String,
    draft: bool,
    review_decision: Option<String>,
    checks: crate::tracking::PullRequestChecks,
}

pub struct WorkProbe {
    panes: Vec<PaneProbe>,
    exact_pull_requests: Vec<ExactPullRequestProbe>,
}

#[derive(Clone)]
struct ExactPullRequestProbe {
    repository: String,
    number: u64,
    result: Result<crate::tracking::BranchPullRequestEntry, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedPullRequestBinding {
    pub identity: WorkIdentity,
    pub task_id: String,
    pub title: String,
    pub checkout_root: String,
    pub reporting_token: String,
    pub base_repository: String,
    pub head_owner: String,
    pub branch: String,
}

pub fn collect_probe(state: &crate::AppState) -> WorkProbe {
    let session = crate::instance::session_name().unwrap_or_else(|| "default".to_string());
    let now = crate::events::unix_time_ms();
    let refresh_pull_requests = state.work_ledger.pull_request_reconciliation_due(now);
    let mut panes = Vec::new();
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            let mut pane_ids: Vec<u32> =
                tab.panes.leaves().iter().map(|leaf| leaf.pane_id).collect();
            pane_ids.extend(
                state
                    .headless_panes
                    .keys()
                    .filter(|(tab_id, _)| *tab_id == tab.id)
                    .map(|(_, pane_id)| *pane_id),
            );
            if let Some(pending) = state.pending_tab_restores.get(&tab.id) {
                let mut next = 0;
                collect_saved_pane_ids(&pending.saved, &mut next, &mut pane_ids);
            }
            pane_ids.sort_unstable();
            pane_ids.dedup();
            for pane_id in pane_ids {
                let Some(pane_work_origin) = pane_work_origin_for(state, tab.id, pane_id) else {
                    continue;
                };
                let binding = state.pane_task_binding(tab.id, pane_id).or_else(|| {
                    state
                        .pending_tab_restores
                        .get(&tab.id)
                        .and_then(|pending| pending.saved.current_task_for_pane(pane_id))
                });
                let activity = tab
                    .pane_agent_activity(pane_id)
                    .map(|activity| format!("{:?}", activity.state).to_ascii_lowercase());
                let local_cwd = pane_probe_local_cwd(state, tab.id, pane_id);
                panes.push(PaneProbe {
                    identity: WorkIdentity {
                        session: session.clone(),
                        workspace_origin: workspace.work_origin.clone(),
                        tab_origin: tab.work_origin.clone(),
                        pane_origin: pane_origin(&session, &tab.work_origin, &pane_work_origin),
                        workspace_id: workspace.id,
                        workspace_name: workspace.name.clone(),
                        tab_id: tab.id,
                        tab_name: tab.name.clone(),
                        pane_id,
                        task_id: binding.map(|binding| binding.task_id.clone()),
                        task_title: binding.map(|binding| binding.title.clone()),
                    },
                    activity,
                    local_cwd,
                    binding_checkout_root: binding
                        .and_then(|binding| binding.checkout_root.as_deref())
                        .map(str::to_string),
                    binding_identity: binding.map(|binding| ProbeBindingIdentity {
                        task_id: binding.task_id.clone(),
                        title: binding.title.clone(),
                        checkout_root: binding.checkout_root.as_deref().map(str::to_string),
                        reporting_token: binding.reporting_token.clone(),
                    }),
                    plan_root: None,
                    plan_tasks: Vec::new(),
                    plan_loaded: false,
                });
            }
        }
    }
    let exact_pull_requests = if refresh_pull_requests {
        let mut seen = std::collections::HashSet::new();
        let mut probes = state
            .work_ledger
            .records()
            .into_iter()
            .filter_map(|record| record.pull_request)
            .filter(|pr| seen.insert((pr.repository.clone(), pr.number)))
            .map(|pr| ExactPullRequestProbe {
                repository: pr.repository,
                number: pr.number,
                result: Err("GitHub reconciliation has not run".to_string()),
            })
            .collect::<Vec<_>>();
        if probes.len() > EXACT_PR_REFRESH_LIMIT {
            // Rotate the bounded batch each refresh cycle so a large retained
            // history cannot monopolize the worker or permanently starve the
            // tail of the list.
            let batches = probes.len().div_ceil(EXACT_PR_REFRESH_LIMIT);
            let cycle = usize::try_from(now / 30_000).unwrap_or(0);
            probes.rotate_left((cycle % batches) * EXACT_PR_REFRESH_LIMIT);
            probes.truncate(EXACT_PR_REFRESH_LIMIT);
        }
        probes
    } else {
        Vec::new()
    };
    WorkProbe {
        panes,
        exact_pull_requests,
    }
}

/// Stable pane identities currently represented by the session runtime,
/// including headless and not-yet-materialized lazy restore panes. This is a
/// model-only projection and performs no filesystem or GTK work.
pub fn work_stream_runtime_identities(state: &crate::AppState) -> Vec<WorkIdentity> {
    collect_probe(state)
        .panes
        .into_iter()
        .map(|pane| pane.identity)
        .collect()
}

pub fn work_stream_pane_metadata(
    state: &crate::AppState,
    records: &[WorkRecord],
) -> HashMap<String, WorkStreamPaneMeta> {
    let mut metadata = HashMap::new();
    for (display_order, identity) in work_stream_runtime_identities(state)
        .into_iter()
        .enumerate()
    {
        let tab_id = identity.tab_id;
        let pane_id = identity.pane_id;
        let lazy = state.pending_tab_restores.contains_key(&tab_id);
        let agent_name = (!lazy)
            .then(|| {
                state
                    .runtime_probe
                    .as_ref()
                    .and_then(|snapshot| snapshot.pane_agents.get(&(tab_id, pane_id)))
                    .and_then(|status| status.agent_name.clone())
                    .or_else(|| {
                        state.find_tab(tab_id).and_then(|(_, tab)| {
                            (tab.agent_pane_id.unwrap_or(tab.focused_pane_id) == pane_id)
                                .then(|| tab.agent_name.clone())
                                .flatten()
                        })
                    })
            })
            .flatten();
        metadata.insert(
            identity.pane_origin.clone(),
            WorkStreamPaneMeta {
                identity: Some(identity),
                agent_name,
                origin_state: if lazy {
                    WorkStreamOriginState::Lazy
                } else {
                    WorkStreamOriginState::Live
                },
                display_order,
            },
        );
    }
    for record in records {
        let origin = &record.identity.pane_origin;
        if let Some(existing) = metadata.get_mut(origin) {
            if existing.origin_state == WorkStreamOriginState::Historical {
                // Retained panes may have been rebound over their lifetime.
                // Keep the newest record identity without changing the
                // origin's stable first-seen display order.
                existing.identity = Some(record.identity.clone());
            }
            continue;
        }
        let display_order = metadata.len();
        metadata.insert(
            origin.clone(),
            WorkStreamPaneMeta {
                identity: Some(record.identity.clone()),
                agent_name: None,
                origin_state: WorkStreamOriginState::Historical,
                display_order,
            },
        );
    }
    metadata
}

fn canonical_task_state(
    ledger: &WorkLedger,
    task_id: Option<&str>,
    checkout_root: Option<&str>,
    record: Option<&WorkRecord>,
    reconciliation: &WorkReconciliation,
) -> CanonicalTaskState {
    let Some(task_id) = task_id else {
        return CanonicalTaskState::Unknown;
    };
    if let Some(tasks) = checkout_root.and_then(|root| ledger.plan_snapshots.get(root)) {
        return tasks
            .get(task_id)
            .map(|(_, status, _)| CanonicalTaskState::from_status(status))
            .unwrap_or(CanonicalTaskState::Absent);
    }
    if reconciliation.source == "plan" {
        if let Some(status) = reconciliation.current_value.as_deref() {
            return CanonicalTaskState::from_status(status);
        }
        if reconciliation.status == WorkReconciliationStatus::Stale
            && reconciliation.reason.contains("no longer exists")
        {
            return CanonicalTaskState::Absent;
        }
    }
    record
        .and_then(|record| record.task_status.as_deref())
        .map_or(CanonicalTaskState::Unknown, CanonicalTaskState::from_status)
}

fn work_identity_checkout_root<'a>(
    state: &'a crate::AppState,
    identity: &WorkIdentity,
) -> Option<&'a str> {
    state
        .workspaces
        .iter()
        .find(|workspace| {
            (!identity.workspace_origin.is_empty()
                && workspace.work_origin == identity.workspace_origin)
                || (identity.workspace_origin.is_empty() && workspace.id == identity.workspace_id)
        })
        .and_then(|workspace| {
            workspace
                .working_tree_path
                .as_deref()
                .or(workspace.repo_root.as_deref())
        })
}

pub fn task_truth_inputs(
    state: &crate::AppState,
    metadata: &HashMap<String, WorkStreamPaneMeta>,
) -> Vec<TaskTruthInput> {
    let records = state.work_ledger.records();
    let process_truth_fresh = crate::runtime_probe::runtime_process_truth_is_fresh(state);
    let mut origins = metadata.keys().cloned().collect::<Vec<_>>();
    origins.sort_by_key(|origin| {
        metadata
            .get(origin)
            .map_or(usize::MAX, |meta| meta.display_order)
    });
    origins
        .into_iter()
        .filter_map(|pane_origin| {
            let meta = metadata.get(&pane_origin)?;
            let latest = records
                .iter()
                .rev()
                .find(|record| record.identity.pane_origin == pane_origin);
            let current_identity = meta.identity.as_ref();
            // A live or lazy runtime identity is authoritative even when it is
            // explicitly unbound. Falling back to the latest ledger row here
            // would attach a currently running, unbound agent to its previous
            // task. Historical origins instead use their latest retained row.
            let task_id = if meta.origin_state == WorkStreamOriginState::Historical {
                latest.and_then(|record| record.identity.task_id.clone())
            } else {
                current_identity.and_then(|identity| identity.task_id.clone())
            };
            let latest_task_record = records.iter().rev().find(|record| {
                record.identity.pane_origin == pane_origin
                    && record.identity.task_id.as_deref() == task_id.as_deref()
            });
            let latest_pr = records.iter().rev().find(|record| {
                record.identity.pane_origin == pane_origin
                    && record.identity.task_id.as_deref() == task_id.as_deref()
                    && record.pull_request.is_some()
            });
            let tab_id = current_identity.map(|identity| identity.tab_id);
            let pane_id = current_identity.map(|identity| identity.pane_id);
            let binding = tab_id.zip(pane_id).and_then(|(tab_id, pane_id)| {
                state.pane_task_binding(tab_id, pane_id).or_else(|| {
                    state
                        .pending_tab_restores
                        .get(&tab_id)
                        .and_then(|pending| pending.saved.current_task_for_pane(pane_id))
                })
            });
            let bound_now =
                binding.is_some_and(|binding| task_id.as_deref() == Some(binding.task_id.as_str()));
            let activity = tab_id.zip(pane_id).and_then(|(tab_id, pane_id)| {
                state
                    .find_tab(tab_id)
                    .and_then(|(_, tab)| tab.pane_agent_activity(pane_id))
            });
            let lifecycle = tab_id.zip(pane_id).and_then(|(tab_id, pane_id)| {
                state
                    .find_tab(tab_id)
                    .map(|(_, tab)| tab.pane_lifecycle(pane_id))
            });
            let has_fresh_activity =
                activity.is_some_and(crate::workspace::AgentActivity::is_fresh);
            let has_fresh_lifecycle = lifecycle
                .is_some_and(|state| !matches!(state, crate::agents::AgentLifecycle::Idle));
            let execution_known = meta.origin_state == WorkStreamOriginState::Live
                && (process_truth_fresh || has_fresh_activity || has_fresh_lifecycle);
            let agent_running = meta.origin_state == WorkStreamOriginState::Live
                && (lifecycle.is_some_and(crate::agents::AgentLifecycle::is_working)
                    || (process_truth_fresh
                        && tab_id.zip(pane_id).is_some_and(|target| {
                            state
                                .runtime_probe
                                .as_ref()
                                .and_then(|snapshot| snapshot.pane_agents.get(&target))
                                .is_some_and(|status| status.running)
                        })));
            let verification_record = latest_pr.or(latest_task_record);
            let mut reconciliation = verification_record.map_or_else(
                || WorkReconciliation::pending("runtime", "Waiting for reconciliation"),
                |record| state.work_ledger.reconciliation_for(record.seq),
            );
            let pull_request = latest_pr.and_then(|record| record.pull_request.clone());
            let canonical = canonical_task_state(
                &state.work_ledger,
                task_id.as_deref(),
                binding
                    .and_then(|binding| binding.checkout_root.as_deref())
                    .or_else(|| {
                        current_identity
                            .and_then(|identity| work_identity_checkout_root(state, identity))
                    }),
                latest_task_record,
                &reconciliation,
            );
            if pull_request.as_ref().is_some_and(|pr| {
                pr.state.eq_ignore_ascii_case("open")
                    && matches!(
                        canonical,
                        CanonicalTaskState::Todo
                            | CanonicalTaskState::Done
                            | CanonicalTaskState::Absent
                            | CanonicalTaskState::Cancelled
                    )
            }) && reconciliation.source == "github"
                && reconciliation.status == WorkReconciliationStatus::Verified
            {
                reconciliation.status = WorkReconciliationStatus::Stale;
                reconciliation.reason = format!(
                    "Verified open GitHub pull request disagrees with canonical {} task status",
                    match canonical {
                        CanonicalTaskState::Todo => "todo",
                        CanonicalTaskState::Done => "done",
                        CanonicalTaskState::Absent => "absent",
                        CanonicalTaskState::Cancelled => "cancelled",
                        _ => "current",
                    }
                );
                reconciliation.current_value = Some("open".to_string());
            }
            Some(TaskTruthInput {
                pane_origin,
                task_id,
                origin: meta.origin_state,
                bound_now,
                execution_known,
                agent_running,
                canonical,
                reconciliation,
                pull_request,
            })
        })
        .collect()
}

pub fn task_truth_for_pane_task(
    state: &crate::AppState,
    tab_id: u32,
    pane_id: u32,
    task_id: &str,
    canonical: CanonicalTaskState,
) -> Option<TaskTruth> {
    let records = state.work_ledger.records();
    let metadata = work_stream_pane_metadata(state, &records);
    let inputs = task_truth_inputs(state, &metadata);
    task_truth_for_pane_task_with(
        state, &metadata, &inputs, tab_id, pane_id, task_id, canonical,
    )
}

/// Per-pane task truth that reuses a `metadata`/`inputs` pair computed once by
/// the caller. Callers rendering many task rows in one refresh should build the
/// shared `work_stream_pane_metadata` + `task_truth_inputs` a single time and
/// thread them here, so the panel stays a single O(panes × records) pass
/// instead of rebuilding both per task row.
pub fn task_truth_for_pane_task_with(
    state: &crate::AppState,
    metadata: &HashMap<String, WorkStreamPaneMeta>,
    inputs: &[TaskTruthInput],
    tab_id: u32,
    pane_id: u32,
    task_id: &str,
    canonical: CanonicalTaskState,
) -> Option<TaskTruth> {
    let origin = metadata.iter().find_map(|(origin, meta)| {
        meta.identity
            .as_ref()
            .is_some_and(|identity| identity.tab_id == tab_id && identity.pane_id == pane_id)
            .then(|| origin.clone())
    })?;
    let mut input = inputs
        .iter()
        .find(|input| input.pane_origin == origin)?
        .clone();
    let same_task = input.task_id.as_deref() == Some(task_id);
    input.task_id = Some(task_id.to_string());
    if canonical != CanonicalTaskState::Unknown || input.canonical == CanonicalTaskState::Unknown {
        input.canonical = canonical;
    }
    input.bound_now = state
        .pane_task_binding(tab_id, pane_id)
        .or_else(|| {
            state
                .pending_tab_restores
                .get(&tab_id)
                .and_then(|pending| pending.saved.current_task_for_pane(pane_id))
        })
        .is_some_and(|binding| binding.task_id == task_id);
    if !same_task {
        // The pane's agent status belongs to the pane's current task (or to an
        // explicitly unbound pane), never to every unrelated task row shown in
        // the Tasks panel.
        input.agent_running = false;
        input.reconciliation = WorkReconciliation::pending(
            "plan",
            "Canonical task is loaded; no matching work observation has been checked",
        );
        input.pull_request = None;
    }
    Some(project_task_truth(&input))
}

pub fn task_truth_for_record(
    state: &crate::AppState,
    metadata: &HashMap<String, WorkStreamPaneMeta>,
    record: &WorkRecord,
) -> TaskTruth {
    let records = state.work_ledger.records();
    let latest_status_records = latest_task_status_records(&records);
    task_truth_for_record_with(state, metadata, &latest_status_records, record)
}

pub(crate) fn latest_task_status_records(
    records: &[WorkRecord],
) -> HashMap<(String, String), &WorkRecord> {
    records
        .iter()
        .filter_map(|record| {
            record.task_status.as_ref()?;
            Some((
                (
                    record.identity.pane_origin.clone(),
                    record.identity.task_id.clone()?,
                ),
                record,
            ))
        })
        .collect()
}

/// Per-record truth projection using a caller-provided latest-status index.
/// Hot callers build the index once per refresh so projecting every retained
/// ledger row remains linear instead of repeatedly cloning and scanning the
/// complete ledger.
pub fn task_truth_for_record_with(
    state: &crate::AppState,
    metadata: &HashMap<String, WorkStreamPaneMeta>,
    latest_status_records: &HashMap<(String, String), &WorkRecord>,
    record: &WorkRecord,
) -> TaskTruth {
    let meta = metadata.get(&record.identity.pane_origin);
    let origin = meta.map_or(WorkStreamOriginState::Historical, |meta| meta.origin_state);
    // Persisted numeric ids are observations, not stable runtime identity.
    // After restore the same pane origin can have different tab/pane ids, so
    // current binding and execution must resolve through the live metadata.
    let runtime_identity = (origin != WorkStreamOriginState::Historical)
        .then(|| meta.and_then(|meta| meta.identity.as_ref()))
        .flatten()
        .unwrap_or(&record.identity);
    let tab_id = runtime_identity.tab_id;
    let pane_id = runtime_identity.pane_id;
    let process_truth_fresh = crate::runtime_probe::runtime_process_truth_is_fresh(state);
    let activity = state
        .find_tab(tab_id)
        .and_then(|(_, tab)| tab.pane_agent_activity(pane_id));
    let lifecycle = state
        .find_tab(tab_id)
        .map(|(_, tab)| tab.pane_lifecycle(pane_id));
    let has_fresh_activity = activity.is_some_and(crate::workspace::AgentActivity::is_fresh);
    let has_fresh_lifecycle =
        lifecycle.is_some_and(|state| !matches!(state, crate::agents::AgentLifecycle::Idle));
    let binding = (origin != WorkStreamOriginState::Historical)
        .then(|| {
            state.pane_task_binding(tab_id, pane_id).or_else(|| {
                state
                    .pending_tab_restores
                    .get(&tab_id)
                    .and_then(|pending| pending.saved.current_task_for_pane(pane_id))
            })
        })
        .flatten();
    let mut reconciliation = state.work_ledger.reconciliation_for(record.seq);
    let canonical_record = record.identity.task_id.as_ref().and_then(|task_id| {
        latest_status_records
            .get(&(record.identity.pane_origin.clone(), task_id.clone()))
            .copied()
    });
    let canonical = canonical_task_state(
        &state.work_ledger,
        record.identity.task_id.as_deref(),
        binding
            .filter(|binding| record.identity.task_id.as_deref() == Some(binding.task_id.as_str()))
            .and_then(|binding| binding.checkout_root.as_deref())
            .or_else(|| work_identity_checkout_root(state, &record.identity)),
        canonical_record.or(Some(record)),
        &reconciliation,
    );
    if record.pull_request.as_ref().is_some_and(|pr| {
        pr.state.eq_ignore_ascii_case("open")
            && matches!(
                canonical,
                CanonicalTaskState::Todo
                    | CanonicalTaskState::Done
                    | CanonicalTaskState::Absent
                    | CanonicalTaskState::Cancelled
            )
    }) && reconciliation.source == "github"
        && reconciliation.status == WorkReconciliationStatus::Verified
    {
        reconciliation.status = WorkReconciliationStatus::Stale;
        reconciliation.reason =
            "Verified open GitHub pull request disagrees with canonical task status".into();
        reconciliation.current_value = Some("open".into());
    }
    project_task_truth(&TaskTruthInput {
        pane_origin: record.identity.pane_origin.clone(),
        task_id: record.identity.task_id.clone(),
        origin,
        bound_now: binding.is_some_and(|binding| {
            record.identity.task_id.as_deref() == Some(binding.task_id.as_str())
        }),
        execution_known: origin == WorkStreamOriginState::Live
            && (process_truth_fresh || has_fresh_activity || has_fresh_lifecycle),
        agent_running: origin == WorkStreamOriginState::Live
            && (lifecycle.is_some_and(crate::agents::AgentLifecycle::is_working)
                || (process_truth_fresh
                    && state
                        .runtime_probe
                        .as_ref()
                        .and_then(|snapshot| snapshot.pane_agents.get(&(tab_id, pane_id)))
                        .is_some_and(|status| status.running))),
        canonical,
        reconciliation,
        pull_request: record.pull_request.clone(),
    })
}

/// Complete native Work projection for query-state/web Monitor parity. This
/// reuses the same palette/filter projector as GTK and includes live panes
/// that do not yet have ledger records.
pub fn work_stream_snapshot_json(state: &crate::AppState) -> serde_json::Value {
    let records = state.work_ledger.records();
    let metadata = work_stream_pane_metadata(state, &records);
    let latest_status_records = latest_task_status_records(&records);
    let truth = project_task_truth_summary(&task_truth_inputs(state, &metadata));
    let mut view = state.work_ledger.view_preferences();
    let projection = project_work_stream(&records, &mut view.palette, &view.filter, &metadata);
    let entries = projection
        .entries
        .into_iter()
        .map(|entry| {
            work_stream_entry_snapshot_json(state, &metadata, &latest_status_records, entry)
        })
        .collect::<Vec<_>>();
    // `entries` retains the native persisted filter for compatibility. The
    // authenticated web client has its own explicit live/history control, so
    // expose the complete chronology additively instead of making that toggle
    // depend on whichever filter happens to be selected in GTK.
    let mut all_palette = view.palette.clone();
    let mut all_entries = project_work_stream(
        &records,
        &mut all_palette,
        &WorkStreamFilter::All,
        &metadata,
    )
    .entries;
    all_entries.extend(
        project_work_stream(
            &records,
            &mut all_palette,
            &WorkStreamFilter::History,
            &metadata,
        )
        .entries,
    );
    all_entries.sort_by_key(|entry| entry.record.seq);
    let all_entries = all_entries
        .into_iter()
        .map(|entry| {
            work_stream_entry_snapshot_json(state, &metadata, &latest_status_records, entry)
        })
        .collect::<Vec<_>>();
    let legend = projection
        .legend
        .into_iter()
        .map(|item| {
            serde_json::json!({
                "pane_origin": item.pane_origin,
                "workspace_origin": item.workspace_origin,
                "tab_origin": item.tab_origin,
                "marker": item.marker,
                "color_slot": item.color_slot,
                "workspace_id": item.workspace_id,
                "tab_id": item.tab_id,
                "pane_id": item.pane_id,
                "workspace_name": item.workspace_name,
                "tab_name": item.tab_name,
                "agent_name": item.agent_name,
                "task_id": item.task_id,
                "task_title": item.task_title,
                "origin_state": match item.origin_state {
                    WorkStreamOriginState::Live => "live",
                    WorkStreamOriginState::Lazy => "lazy",
                    WorkStreamOriginState::Historical => "historical",
                },
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "schema": "taarof.work-stream.v1",
        "session": crate::instance::session_name().unwrap_or_else(|| "default".to_string()),
        "filter": view.filter,
        "palette": view.palette,
        "legend": legend,
        "entries": entries,
        "all_entries": all_entries,
        "truth": truth.items,
        "counts": {
            "live_open": truth.live_open_count,
            "historical": truth.historical_count,
            "mismatch": truth.mismatch_count,
        },
        "overflow_count": projection.overflow_count,
        "restore": state.work_ledger.restore_health(),
    })
}

fn work_stream_entry_snapshot_json(
    state: &crate::AppState,
    metadata: &HashMap<String, WorkStreamPaneMeta>,
    latest_status_records: &HashMap<(String, String), &WorkRecord>,
    entry: WorkStreamEntry,
) -> serde_json::Value {
    let reconciliation = state.work_ledger.reconciliation_for(entry.record.seq);
    let truth = task_truth_for_record_with(state, metadata, latest_status_records, &entry.record);
    serde_json::json!({
        "record": entry.record,
        "marker": entry.marker,
        "color_slot": entry.color_slot,
        "reconciliation": reconciliation,
        "truth": truth,
    })
}

/// Read all bound pane-local plan files on a worker thread. A parse/read
/// failure leaves that pane empty; `observe_plan` retains its prior snapshot
/// by ignoring empty failed probes rather than treating them as deletion.
pub fn enrich_plan_probe(mut probe: WorkProbe) -> WorkProbe {
    for pane in &mut probe.panes {
        pane.binding_checkout_root = pane
            .binding_checkout_root
            .as_deref()
            .and_then(|root| canonical_directory(Path::new(root)));
        pane.plan_root = pane
            .local_cwd
            .as_deref()
            .and_then(|cwd| nearest_plan_root(Path::new(cwd)));
        if let Some(root) = pane.plan_root.as_deref() {
            if let Some(tasks) = load_plan_observations(Path::new(root)) {
                pane.plan_tasks = tasks;
                pane.plan_loaded = true;
            }
        }
    }
    enrich_exact_pull_requests_with(&mut probe.exact_pull_requests, fetch_exact_pull_request);
    probe
}

fn enrich_exact_pull_requests_with<F>(probes: &mut [ExactPullRequestProbe], fetch: F)
where
    F: Fn(&str, u64) -> Result<crate::tracking::BranchPullRequestEntry, String> + Sync,
{
    let queue = Mutex::new((0..probes.len()).collect::<VecDeque<_>>());
    let results = Mutex::new(Vec::with_capacity(probes.len()));
    std::thread::scope(|scope| {
        for _ in 0..probes.len().min(EXACT_PR_REFRESH_CONCURRENCY) {
            let fetch = &fetch;
            let queue = &queue;
            let results = &results;
            let probes = &*probes;
            scope.spawn(move || loop {
                let index = queue.lock().ok().and_then(|mut queue| queue.pop_front());
                let Some(index) = index else { break };
                let probe = &probes[index];
                let result = fetch(&probe.repository, probe.number);
                if let Ok(mut results) = results.lock() {
                    results.push((index, result));
                }
            });
        }
    });
    if let Ok(results) = results.into_inner() {
        for (index, result) in results {
            probes[index].result = result;
        }
    }
}

fn fetch_exact_pull_request(
    repository: &str,
    number: u64,
) -> Result<crate::tracking::BranchPullRequestEntry, String> {
    let output = std::process::Command::new("timeout")
        .args([
            EXACT_PR_COMMAND_TIMEOUT,
            "gh",
            "pr",
            "view",
            &number.to_string(),
            "--repo",
            repository,
            "--json",
            "number,title,url,state,isDraft,reviewDecision,statusCheckRollup,headRefName,headRepositoryOwner,body",
        ])
        .output()
        .map_err(|error| format!("GitHub refresh unavailable: {error}"))?;
    if !output.status.success() {
        return Err(if output.status.code() == Some(124) {
            "GitHub refresh timed out".to_string()
        } else {
            "GitHub refresh unavailable".to_string()
        });
    }
    let object = String::from_utf8_lossy(&output.stdout);
    let wrapped = format!("[{object}]");
    let data = crate::tracking::BranchPullRequestsData::from_gh_json_str(&wrapped)
        .ok_or_else(|| "GitHub response could not be parsed".to_string())?;
    data.pull_requests
        .into_iter()
        .find(|pr| pr.number == number)
        .ok_or_else(|| "GitHub response did not match the requested pull request".to_string())
}

fn nearest_plan_root(cwd: &Path) -> Option<String> {
    let cwd = cwd.canonicalize().ok()?;
    cwd.ancestors()
        .find(|candidate| candidate.join(".plan").is_dir())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

fn canonical_directory(path: &Path) -> Option<String> {
    let path = path.canonicalize().ok()?;
    path.is_dir().then(|| path.to_string_lossy().into_owned())
}

fn collect_saved_pane_ids(
    node: &crate::session::SavedPaneNode,
    next: &mut u32,
    pane_ids: &mut Vec<u32>,
) {
    match node {
        crate::session::SavedPaneNode::Leaf { .. } => {
            pane_ids.push(*next);
            *next += 1;
        }
        crate::session::SavedPaneNode::Split { first, second, .. } => {
            collect_saved_pane_ids(first, next, pane_ids);
            collect_saved_pane_ids(second, next, pane_ids);
        }
    }
}

fn pane_origin(session: &str, tab_work_origin: &str, pane_work_origin: &str) -> String {
    let raw = format!("{session}\0{tab_work_origin}\0{pane_work_origin}");
    format!("pane-{:016x}", stable_hash(raw.as_bytes()))
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn load_plan_observations(root: &Path) -> Option<Vec<ObservedPlanTask>> {
    let raw = fs::read_to_string(root.join(".plan/tasks.json")).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    let tasks = value.get("tasks")?.as_array()?;
    Some(
        tasks
            .iter()
            .filter_map(|task| {
                let id = task.get("id")?.as_str()?.trim().to_string();
                let title = task.get("title")?.as_str()?.trim().to_string();
                let status = task.get("status")?.as_str()?.trim().to_ascii_lowercase();
                let description = task
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .unwrap_or("");
                let verify_command = task
                    .get("verify_command")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .unwrap_or("");
                let acceptance = task
                    .get("acceptance_criteria")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|criteria| {
                        !criteria.is_empty()
                            && criteria.iter().all(|criterion| {
                                criterion
                                    .as_str()
                                    .is_some_and(|value| !value.trim().is_empty())
                            })
                    });
                let recognized_status = matches!(
                    status.as_str(),
                    "todo" | "in_progress" | "in-progress" | "blocked" | "done" | "cancelled"
                );
                Some(ObservedPlanTask {
                    fully_specified: !id.is_empty()
                        && !title.is_empty()
                        && !description.is_empty()
                        && !verify_command.is_empty()
                        && acceptance
                        && recognized_status,
                    id,
                    title,
                    status,
                })
            })
            .collect(),
    )
}

fn reconcile_record_with_plan(
    record: &WorkRecord,
    task: &ObservedPlanTask,
    now: u64,
) -> WorkReconciliation {
    match record.kind {
        WorkKind::AgentReportedFinished => WorkReconciliation::checked(
            if task.status == "done" {
                WorkReconciliationStatus::Verified
            } else {
                WorkReconciliationStatus::Unverified
            },
            "plan",
            if task.status == "done" {
                "Agent finished report agrees with current .plan status"
            } else {
                "Agent finished report does not mark the task done in .plan"
            },
            now,
        ),
        WorkKind::AgentReportedStarted
        | WorkKind::AgentReportedProgress
        | WorkKind::AgentReportedBlocked => WorkReconciliation::checked(
            if task.status == "done" {
                WorkReconciliationStatus::Stale
            } else {
                WorkReconciliationStatus::Unverified
            },
            "plan",
            if task.status == "done" {
                "Agent report predates the task's current done status"
            } else {
                "Agent report remains observational; .plan is canonical"
            },
            now,
        ),
        WorkKind::TaskStatusChanged => {
            let Some(observed) = record.task_status.as_deref() else {
                return WorkReconciliation::checked(
                    WorkReconciliationStatus::Unverified,
                    "plan",
                    "Legacy task event has no typed canonical status fact",
                    now,
                );
            };
            WorkReconciliation::checked(
                if observed == task.status {
                    WorkReconciliationStatus::Verified
                } else {
                    WorkReconciliationStatus::Stale
                },
                "plan",
                if observed == task.status {
                    "Current .plan status matches this observation"
                } else {
                    "Current .plan status has superseded this observation"
                },
                now,
            )
        }
        _ => WorkReconciliation::checked(
            WorkReconciliationStatus::Verified,
            "plan",
            "Task still exists in the canonical .plan file",
            now,
        ),
    }
}

fn verified_binding_is_current(
    state: &crate::AppState,
    verified: &VerifiedPullRequestBinding,
) -> bool {
    let Some((tab_id, pane_id)) = runtime_target_for_identity(state, &verified.identity) else {
        return false;
    };
    let Some(binding) = state.pane_task_binding(tab_id, pane_id).or_else(|| {
        state
            .pending_tab_restores
            .get(&tab_id)
            .and_then(|pending| pending.saved.current_task_for_pane(pane_id))
    }) else {
        return false;
    };
    binding.task_id == verified.task_id
        && binding.title == verified.title
        && binding.checkout_root.as_deref() == Some(verified.checkout_root.as_str())
        && binding.reporting_token == verified.reporting_token
}

fn pane_probe_is_current(state: &crate::AppState, pane: &PaneProbe) -> bool {
    let Some((tab_id, pane_id)) = runtime_target_for_identity(state, &pane.identity) else {
        return false;
    };
    let current = state.pane_task_binding(tab_id, pane_id).or_else(|| {
        state
            .pending_tab_restores
            .get(&tab_id)
            .and_then(|pending| pending.saved.current_task_for_pane(pane_id))
    });
    match (pane.binding_identity.as_ref(), current) {
        // An unbound pane's nearest `.plan` is derived from its local cwd.
        // Re-read that model context after off-thread enrichment so a pane
        // that `cd`s into another checkout cannot apply the old checkout's
        // task observations merely because its stable origin is unchanged.
        (None, None) => pane_probe_local_cwd(state, tab_id, pane_id) == pane.local_cwd,
        (Some(captured), Some(current)) => {
            captured.task_id == current.task_id
                && captured.title == current.title
                && captured.checkout_root.as_deref() == current.checkout_root.as_deref()
                && captured.reporting_token == current.reporting_token
        }
        _ => false,
    }
}

fn pane_probe_local_cwd(state: &crate::AppState, tab_id: u32, pane_id: u32) -> Option<String> {
    state.pane_local_cwd(tab_id, pane_id).ok().or_else(|| {
        state
            .pending_tab_restores
            .get(&tab_id)
            .and_then(|pending| pending.saved.local_cwd_for_pane(pane_id))
    })
}

impl crate::AppState {
    pub fn set_work_stream_preferences(&mut self, view: WorkStreamPreferences) {
        if self.work_ledger.view_preferences() == view {
            return;
        }
        self.work_ledger.set_view_preferences(view.clone());
        self.event_store.emit(
            "work_preferences_changed",
            serde_json::to_value(view).unwrap_or(serde_json::Value::Null),
        );
    }

    pub fn clear_session_work_ledger(&mut self) -> (usize, Option<mpsc::Receiver<io::Result<()>>>) {
        let removed = self.work_ledger.clear_all();
        let barrier = self.work_ledger.persistence_barrier();
        self.event_store.emit(
            "work_ledger_cleared",
            serde_json::json!({ "scope": "session", "removed": removed }),
        );
        (removed, barrier)
    }

    pub fn clear_pane_work_ledger(
        &mut self,
        pane_origin: &str,
    ) -> (usize, Option<mpsc::Receiver<io::Result<()>>>) {
        let removed = self.work_ledger.clear_pane(pane_origin);
        let barrier = self.work_ledger.persistence_barrier();
        self.event_store.emit(
            "work_ledger_cleared",
            serde_json::json!({
                "scope": "pane",
                "pane_origin": pane_origin,
                "removed": removed,
            }),
        );
        (removed, barrier)
    }

    fn emit_reconciliation_changes(&mut self, changes: Vec<(u64, WorkReconciliation)>) {
        if changes.len() > RECONCILIATION_EVENT_BURST_LIMIT {
            let mut by_status = std::collections::BTreeMap::new();
            let mut work_seq_min = u64::MAX;
            let mut work_seq_max = 0;
            for (work_seq, reconciliation) in &changes {
                work_seq_min = work_seq_min.min(*work_seq);
                work_seq_max = work_seq_max.max(*work_seq);
                let status = match reconciliation.status {
                    WorkReconciliationStatus::Pending => "pending",
                    WorkReconciliationStatus::Verified => "verified",
                    WorkReconciliationStatus::Stale => "stale",
                    WorkReconciliationStatus::Unverified => "unverified",
                };
                *by_status.entry(status).or_insert(0_usize) += 1;
            }
            self.event_store.emit(
                "work_reconciled_batch",
                serde_json::json!({
                    "total": changes.len(),
                    "by_status": by_status,
                    "work_seq_min": work_seq_min,
                    "work_seq_max": work_seq_max,
                    "per_entity_emitted": RECONCILIATION_EVENT_BURST_LIMIT,
                    "per_entity_omitted": changes.len() - RECONCILIATION_EVENT_BURST_LIMIT,
                    "truncated": true,
                    "resnapshot_required": true,
                }),
            );
        }
        for (work_seq, reconciliation) in changes.into_iter().take(RECONCILIATION_EVENT_BURST_LIMIT)
        {
            self.event_store.emit(
                "work_reconciled",
                serde_json::json!({
                    "work_seq": work_seq,
                    "reconciliation": reconciliation,
                }),
            );
        }
    }

    fn work_recorded_event_payload(&mut self, record: &WorkRecord) -> serde_json::Value {
        let current_target =
            runtime_target_for_identity(self, &record.identity).and_then(|(tab_id, pane_id)| {
                let (workspace, tab) = self.find_tab(tab_id)?;
                Some(serde_json::json!({
                    "workspace_id": workspace.id,
                    "tab_id": tab.id,
                    "pane_id": pane_id,
                    "workspace_origin": workspace.work_origin,
                    "tab_origin": tab.work_origin,
                    "pane_origin": record.identity.pane_origin,
                }))
            });
        let reconciliation = self.work_ledger.reconciliation_for(record.seq);
        let (marker, color_slot) = self.work_ledger.marker_for_record(record);
        serde_json::json!({
            "work_seq": record.seq,
            "record": record,
            "marker": marker,
            "color_slot": color_slot,
            "reconciliation": reconciliation,
            "current_target": current_target,
        })
    }

    fn append_work_record(&mut self, draft: WorkDraft) -> Option<WorkRecord> {
        let record = self.work_ledger.append(draft)?;
        let payload = self.work_recorded_event_payload(&record);
        self.event_store.emit("work_recorded", payload);
        Some(record)
    }

    fn append_plan_work_record(
        &mut self,
        draft: WorkDraft,
        task_status: &str,
    ) -> Option<WorkRecord> {
        let mut record = self.work_ledger.append_without_history(draft)?;
        record.task_status = Some(task_status.to_string());
        if let Some(stored) = self
            .work_ledger
            .records
            .back_mut()
            .filter(|stored| stored.seq == record.seq)
        {
            stored.task_status = record.task_status.clone();
        }
        self.work_ledger.schedule_persist();
        self.work_ledger.record_history(&record);
        let payload = self.work_recorded_event_payload(&record);
        self.event_store.emit("work_recorded", payload);
        Some(record)
    }

    fn append_pull_request_work_record(
        &mut self,
        draft: WorkDraft,
        pull_request: WorkPullRequestRef,
    ) -> Option<WorkRecord> {
        let record = self.work_ledger.append_pull_request(draft, pull_request)?;
        let payload = self.work_recorded_event_payload(&record);
        self.event_store.emit("work_recorded", payload);
        Some(record)
    }

    pub fn observe_verified_pull_requests(
        &mut self,
        verified: &VerifiedPullRequestBinding,
        data: &crate::tracking::BranchPullRequestsData,
    ) {
        if !verified_binding_is_current(self, verified) {
            return;
        }
        let Some((tab_id, pane_id)) = runtime_target_for_identity(self, &verified.identity) else {
            return;
        };
        let Some(binding) = self.pane_task_binding(tab_id, pane_id) else {
            return;
        };
        if binding.task_id != verified.task_id
            || binding.title != verified.title
            || binding.checkout_root.as_deref() != Some(verified.checkout_root.as_str())
            || binding.reporting_token != verified.reporting_token
        {
            return;
        }
        let Some(current_identity) = identity_for_pane(self, tab_id, pane_id, Some(binding)) else {
            return;
        };
        if current_identity.pane_origin != verified.identity.pane_origin
            || current_identity.task_id.as_deref() != Some(verified.task_id.as_str())
        {
            return;
        }
        let crate::tracking::PullRequestCorrelation::Linked(pr) =
            crate::tracking::correlate_pull_request(
                &verified.task_id,
                &verified.base_repository,
                &verified.head_owner,
                &verified.branch,
                data,
            )
        else {
            return;
        };
        let Some(url) = pr.url.as_deref() else {
            return;
        };
        let repository = verified.base_repository.as_str();
        let reference = WorkPullRequestRef {
            repository: repository.to_string(),
            number: pr.number,
            title: pr.title.clone(),
            url: url.to_string(),
            state: pr.state.clone(),
            is_draft: pr.is_draft,
            review_decision: pr.review_decision.clone(),
            checks: pr.checks,
        };
        if !valid_pull_request_ref(&reference) {
            return;
        }
        self.work_ledger.begin_persistence_batch();
        let scope = format!(
            "{}:{repository}:{}:{}",
            verified.identity.pane_origin, pr.number, verified.task_id
        );
        let changes = self.work_ledger.observe_pull_request(&scope, pr);
        for (kind, summary) in changes {
            let fingerprint = format!(
                "{kind:?}:{}:{}:{:?}:{:?}",
                pr.state, pr.is_draft, pr.review_decision, pr.checks
            );
            self.append_pull_request_work_record(
                WorkDraft {
                    kind,
                    summary,
                    identity: verified.identity.clone(),
                    evidence_source: EvidenceSource::GitHubPullRequest,
                    authority: WorkAuthority::GitHubCanonical,
                    verification: VerificationState::RemoteVerified,
                    dedupe_scope: format!("github-pr:{scope}:{kind:?}"),
                    fingerprint,
                },
                reference.clone(),
            );
        }
        self.work_ledger.end_persistence_batch();
        let changes = self.work_ledger.reconcile_pull_requests(verified, Ok(data));
        self.emit_reconciliation_changes(changes);
    }

    pub fn observe_work_probe(&mut self, probe: WorkProbe) {
        self.work_ledger.begin_persistence_batch();
        self.observe_work_probe_inner(probe, true);
        self.work_ledger.end_persistence_batch();
    }

    #[cfg(test)]
    fn observe_work_probe_unchecked(&mut self, probe: WorkProbe) {
        self.work_ledger.begin_persistence_batch();
        self.observe_work_probe_inner(probe, false);
        self.work_ledger.end_persistence_batch();
    }

    fn observe_work_probe_inner(&mut self, probe: WorkProbe, revalidate: bool) {
        let WorkProbe {
            panes,
            exact_pull_requests,
        } = probe;
        let panes = panes
            .into_iter()
            .filter(|pane| !revalidate || pane_probe_is_current(self, pane))
            .collect::<Vec<_>>();
        self.work_ledger.prune_at(crate::events::unix_time_ms());
        let now = crate::events::unix_time_ms();
        let current_identities = work_stream_runtime_identities(self);
        let live_origins = current_identities
            .iter()
            .map(|identity| identity.pane_origin.as_str())
            .collect::<std::collections::HashSet<_>>();
        for pane in &panes {
            if let Some(activity) = self
                .work_ledger
                .observe_activity(&pane.identity.pane_origin, pane.activity.as_deref())
            {
                let draft = WorkDraft {
                    kind: WorkKind::AgentStateChanged,
                    summary: format!("Agent state changed to {activity}"),
                    identity: pane.identity.clone(),
                    evidence_source: EvidenceSource::AgentActivity,
                    authority: WorkAuthority::AgentObservation,
                    verification: VerificationState::Observed,
                    dedupe_scope: format!("activity:{}", pane.identity.pane_origin),
                    fingerprint: activity.to_string(),
                };
                self.append_work_record(draft);
            }
        }

        let mut by_root: HashMap<String, Vec<PaneProbe>> = HashMap::new();
        for pane in &panes {
            if let Some(root) = pane.plan_root.clone() {
                by_root.entry(root).or_default().push(pane.clone());
            }
        }
        for (root, mut panes) in by_root {
            panes.sort_by_key(|pane| pane.identity.pane_origin.clone());
            let Some(observation) = panes.iter().find(|pane| pane.plan_loaded) else {
                // Preserve the last good snapshot across unreadable/partial files.
                continue;
            };
            let changes = self
                .work_ledger
                .observe_plan(&root, &observation.plan_tasks);
            for (kind, task_id, title, summary) in changes {
                let targets: Vec<&PaneProbe> = if kind == WorkKind::TaskStatusChanged {
                    panes
                        .iter()
                        .filter(|pane| {
                            pane.identity.task_id.as_deref() == Some(task_id.as_str())
                                && pane.binding_checkout_root.as_deref() == Some(root.as_str())
                        })
                        .collect()
                } else {
                    // A checkout-level task creation is emitted once, associated
                    // with the deterministic first observing pane.
                    panes.first().into_iter().collect()
                };
                for pane in targets {
                    let mut identity = pane.identity.clone();
                    identity.task_id = Some(task_id.clone());
                    identity.task_title = Some(title.clone());
                    // `observe_plan` already emits only real transitions. The
                    // sequence component permits remove -> re-add of the same
                    // task to create a second canonical creation record while
                    // unchanged probes remain silent.
                    let fingerprint = format!("{kind:?}:{summary}:{}", self.work_ledger.next_seq);
                    let task_status = observation
                        .plan_tasks
                        .iter()
                        .find(|task| task.id == task_id)
                        .map(|task| task.status.as_str())
                        .unwrap_or("todo");
                    self.append_plan_work_record(
                        WorkDraft {
                            kind,
                            summary: summary.clone(),
                            identity: identity.clone(),
                            evidence_source: EvidenceSource::PlanFile,
                            authority: WorkAuthority::PlanCanonical,
                            verification: VerificationState::CanonicalFile,
                            dedupe_scope: format!("plan:{root}:{task_id}:{}", identity.pane_origin),
                            fingerprint,
                        },
                        task_status,
                    );
                }
            }
        }

        for pane in &panes {
            let changes = self.work_ledger.reconcile_plan_for_pane(pane, now);
            self.emit_reconciliation_changes(changes);
        }
        if !exact_pull_requests.is_empty() {
            self.work_ledger.last_pr_reconciliation_at = now;
        }
        for pr_probe in exact_pull_requests {
            let changes = self.work_ledger.reconcile_exact_pull_request(
                &pr_probe.repository,
                pr_probe.number,
                pr_probe.result.as_ref().map_err(String::as_str),
            );
            self.emit_reconciliation_changes(changes);
        }
        // Reconcile origins after projecting the probe so records first
        // observed by this cycle settle immediately. Otherwise the next
        // identical probe emits a delayed unknown -> live transition.
        let changes = self
            .work_ledger
            .reconcile_runtime_origins(&live_origins, now);
        self.emit_reconciliation_changes(changes);
    }

    pub fn record_pane_binding_change(
        &mut self,
        tab_id: u32,
        pane_id: u32,
        old: Option<&crate::task_binding::PaneTaskBinding>,
        new: Option<&crate::task_binding::PaneTaskBinding>,
    ) {
        let same_task_context = old.zip(new).is_some_and(|(old, new)| {
            old.task_id == new.task_id
                && old.title == new.title
                && old.checkout_root == new.checkout_root
        });
        if old == new || same_task_context {
            return;
        }
        let Some(identity) = identity_for_pane(self, tab_id, pane_id, new.or(old)) else {
            return;
        };
        let (kind, summary, fingerprint) = match (old, new) {
            (None, Some(binding)) => (
                WorkKind::PaneBound,
                format!("Bound pane to {} - {}", binding.task_id, binding.title),
                format!("none->{}", binding.task_id),
            ),
            (Some(binding), None) => (
                WorkKind::PaneUnbound,
                format!(
                    "Cleared current task {} - {}",
                    binding.task_id, binding.title
                ),
                format!("{}->none", binding.task_id),
            ),
            (Some(old), Some(new)) => (
                WorkKind::PaneBound,
                format!("Changed current task: {} -> {}", old.task_id, new.task_id),
                format!("{}->{}", old.task_id, new.task_id),
            ),
            (None, None) => return,
        };
        let dedupe_scope = format!("binding:{}", identity.pane_origin);
        self.append_work_record(WorkDraft {
            kind,
            summary,
            identity,
            evidence_source: EvidenceSource::PaneBinding,
            authority: WorkAuthority::TaarofSession,
            verification: VerificationState::Observed,
            dedupe_scope,
            fingerprint: format!("{kind:?}:{fingerprint}"),
        });
    }

    pub fn record_agent_report(
        &mut self,
        tab_id: u32,
        pane_id: u32,
        milestone: crate::work_reporting::WorkMilestone,
        note: Option<&str>,
    ) -> bool {
        if crate::work_reporting::validate_note(note).is_err() {
            return false;
        }
        let Some(identity) = identity_for_pane(
            self,
            tab_id,
            pane_id,
            self.pane_task_binding(tab_id, pane_id),
        ) else {
            return false;
        };
        let (kind, base) = match milestone {
            crate::work_reporting::WorkMilestone::Started => (
                WorkKind::AgentReportedStarted,
                "Agent reported work started",
            ),
            crate::work_reporting::WorkMilestone::Progress => {
                (WorkKind::AgentReportedProgress, "Agent reported progress")
            }
            crate::work_reporting::WorkMilestone::Blocked => {
                (WorkKind::AgentReportedBlocked, "Agent reported blocked")
            }
            crate::work_reporting::WorkMilestone::Finished => (
                WorkKind::AgentReportedFinished,
                "Agent reported work finished",
            ),
        };
        let summary = note.map_or_else(|| base.to_string(), |note| format!("{base}: {note}"));
        let seq = self.work_ledger.next_seq;
        self.append_work_record(WorkDraft {
            kind,
            summary,
            identity: identity.clone(),
            evidence_source: EvidenceSource::AgentReport,
            authority: WorkAuthority::AgentObservation,
            verification: VerificationState::Observed,
            dedupe_scope: format!("agent-report:{}", identity.pane_origin),
            fingerprint: format!("{}:{seq}", milestone.label()),
        })
        .is_some()
    }

    pub fn project_agent_message(&mut self, payload: &serde_json::Value) {
        self.work_ledger.begin_persistence_batch();
        self.project_agent_message_inner(payload);
        self.work_ledger.end_persistence_batch();
    }

    fn project_agent_message_inner(&mut self, payload: &serde_json::Value) {
        let Some(tab_id) = payload.get("tab_id").and_then(serde_json::Value::as_u64) else {
            return;
        };
        let Some(pane_id) = payload.get("pane_id").and_then(serde_json::Value::as_u64) else {
            return;
        };
        let (Ok(tab_id), Ok(pane_id)) = (u32::try_from(tab_id), u32::try_from(pane_id)) else {
            return;
        };
        let Some(identity) = identity_for_pane(
            self,
            tab_id,
            pane_id,
            self.pane_task_binding(tab_id, pane_id),
        ) else {
            return;
        };
        let session = payload
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let checkout_root = self
            .pane_task_binding(tab_id, pane_id)
            .and_then(|binding| binding.checkout_root.as_deref())
            .map(PathBuf::from);

        // Presence is authoritative, including an empty array: never replay
        // legacy aggregates when the ordered producer explicitly reports no
        // work events.
        if let Some(value) = payload.get("work_events") {
            let Some(events) = value.as_array() else {
                return;
            };
            let start = events
                .len()
                .saturating_sub(self.work_ledger.projection_limit());
            for event in &events[start..] {
                match event.get("kind").and_then(serde_json::Value::as_str) {
                    Some("assistant_completed") => {
                        let ordinal = event
                            .get("ordinal")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0);
                        self.project_assistant_completion(&identity, session, ordinal);
                    }
                    Some("file_operation") => {
                        let ordinal = event
                            .get("ordinal")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0);
                        let path = event
                            .get("path")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("");
                        let op = event
                            .get("op")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("edit");
                        self.project_file_operation(
                            &identity,
                            session,
                            checkout_root.as_deref(),
                            path,
                            op,
                            ordinal,
                        );
                    }
                    _ => {}
                }
            }
            return;
        }

        // Backward-compatible projection for callers that predate the ordered
        // work-events field.
        let message_count = payload
            .get("message_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let reported_new_messages = payload
            .get("new_messages")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1);
        let new_messages = reported_new_messages
            .min(message_count)
            .min(self.work_ledger.projection_limit() as u64);
        if new_messages > 0 {
            let first_ordinal = message_count.saturating_sub(new_messages.saturating_sub(1));
            for offset in 0..new_messages {
                let ordinal = first_ordinal.saturating_add(offset);
                self.project_assistant_completion(&identity, session, ordinal);
            }
        }
        if let Some(files) = payload
            .get("recent_files")
            .and_then(serde_json::Value::as_array)
        {
            let start = files
                .len()
                .saturating_sub(self.work_ledger.projection_limit());
            for file in &files[start..] {
                let op = file
                    .get("op")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("edit");
                let ordinal = file
                    .get("ordinal")
                    .and_then(serde_json::Value::as_u64)
                    .or_else(|| file.get("at_unix_ms").and_then(serde_json::Value::as_u64))
                    .unwrap_or(0);
                let path = file
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                self.project_file_operation(
                    &identity,
                    session,
                    checkout_root.as_deref(),
                    path,
                    op,
                    ordinal,
                );
            }
        }
    }

    fn project_assistant_completion(
        &mut self,
        identity: &WorkIdentity,
        session: &str,
        ordinal: u64,
    ) {
        let message_scope = format!("message:{}:{session}", identity.pane_origin);
        if !self
            .work_ledger
            .note_message_ordinal(&message_scope, ordinal)
        {
            return;
        }
        self.append_work_record(WorkDraft {
            kind: WorkKind::AssistantMessageCompleted,
            summary: "Assistant message completed".to_string(),
            identity: identity.clone(),
            evidence_source: EvidenceSource::AgentTranscript,
            authority: WorkAuthority::AgentObservation,
            verification: VerificationState::Observed,
            dedupe_scope: message_scope,
            fingerprint: ordinal.to_string(),
        });
    }

    fn project_file_operation(
        &mut self,
        identity: &WorkIdentity,
        session: &str,
        checkout_root: Option<&Path>,
        path: &str,
        op: &str,
        ordinal: u64,
    ) {
        let file_scope = format!("file:{}:{session}", identity.pane_origin);
        if !self.work_ledger.note_file_ordinal(&file_scope, ordinal) {
            return;
        }
        // The raw path participates only in this in-memory evidence check.
        let safe_path = checkout_root.and_then(|root| safe_repo_relative_path(root, path));
        self.append_work_record(WorkDraft {
            kind: WorkKind::FileTouched,
            summary: match (op, safe_path.as_deref()) {
                ("write", Some(path)) => format!("Agent created {path}"),
                (_, Some(path)) => format!("Agent edited {path}"),
                ("write", None) => "Agent created a file".to_string(),
                _ => "Agent edited a file".to_string(),
            },
            identity: identity.clone(),
            evidence_source: EvidenceSource::TranscriptFileOperation,
            authority: WorkAuthority::AgentObservation,
            verification: VerificationState::Observed,
            dedupe_scope: file_scope,
            fingerprint: ordinal.to_string(),
        });
    }
}

fn safe_repo_relative_path(root: &Path, observed: &str) -> Option<String> {
    if observed.chars().any(char::is_control)
        || root.to_string_lossy().chars().any(char::is_control)
    {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let observed = Path::new(observed);
    let candidate = if observed.is_absolute() {
        observed.to_path_buf()
    } else {
        root.join(observed)
    };
    let candidate = candidate.canonicalize().ok()?;
    let relative = candidate.strip_prefix(&root).ok()?;
    let rendered = relative.to_string_lossy();
    if rendered.is_empty() || rendered.len() > 160 || rendered.chars().any(char::is_control) {
        return None;
    }
    let suspicious = relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        name == ".env"
            || name.starts_with(".env.")
            || [
                "secret",
                "token",
                "credential",
                "private",
                "id_rsa",
                "id_ed25519",
            ]
            .iter()
            .any(|needle| name.contains(needle))
            || [".pem", ".key", ".p12", ".pfx"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
    });
    (!suspicious).then(|| rendered.into_owned())
}

pub(crate) fn identity_for_pane(
    state: &crate::AppState,
    tab_id: u32,
    pane_id: u32,
    binding: Option<&crate::task_binding::PaneTaskBinding>,
) -> Option<WorkIdentity> {
    let (workspace, tab) = state.find_tab(tab_id)?;
    let pane_work_origin = pane_work_origin_for(state, tab_id, pane_id)?;
    Some(WorkIdentity {
        session: crate::instance::session_name().unwrap_or_else(|| "default".to_string()),
        workspace_origin: workspace.work_origin.clone(),
        tab_origin: tab.work_origin.clone(),
        pane_origin: pane_origin(
            &crate::instance::session_name().unwrap_or_else(|| "default".to_string()),
            &tab.work_origin,
            &pane_work_origin,
        ),
        workspace_id: workspace.id,
        workspace_name: workspace.name.clone(),
        tab_id,
        tab_name: tab.name.clone(),
        pane_id,
        task_id: binding.map(|binding| binding.task_id.clone()),
        task_title: binding.map(|binding| binding.title.clone()),
    })
}

fn pane_work_origin_for(state: &crate::AppState, tab_id: u32, pane_id: u32) -> Option<String> {
    state
        .find_tab(tab_id)
        .and_then(|(_, tab)| tab.panes.leaf(pane_id))
        .map(|leaf| leaf.work_origin.clone())
        .or_else(|| {
            state
                .headless_pane(tab_id, pane_id)
                .map(|pane| pane.work_origin.clone())
        })
        .or_else(|| {
            state
                .pending_tab_restores
                .get(&tab_id)
                .and_then(|pending| pending.saved.work_origin_for_pane(pane_id))
                .map(str::to_string)
        })
}

pub fn origin_for_pane(state: &crate::AppState, tab_id: u32, pane_id: u32) -> Option<String> {
    identity_for_pane(
        state,
        tab_id,
        pane_id,
        state.pane_task_binding(tab_id, pane_id),
    )
    .map(|identity| identity.pane_origin)
}

/// Resolve persisted stable origin metadata back to the current runtime ids.
/// Runtime ids in a record are intentionally ignored because restore may
/// regenerate them and tabs may move between workspaces.
pub fn runtime_target_for_identity(
    state: &crate::AppState,
    identity: &WorkIdentity,
) -> Option<(u32, u32)> {
    let current_session = crate::instance::session_name().unwrap_or_else(|| "default".to_string());
    if identity.session != current_session {
        return None;
    }
    let mut resolved = None;
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            if tab.work_origin != identity.tab_origin {
                continue;
            }
            let mut pane_ids: Vec<u32> =
                tab.panes.leaves().iter().map(|leaf| leaf.pane_id).collect();
            pane_ids.extend(
                state
                    .headless_panes
                    .keys()
                    .filter(|(tab_id, _)| *tab_id == tab.id)
                    .map(|(_, pane_id)| *pane_id),
            );
            if let Some(pending) = state.pending_tab_restores.get(&tab.id) {
                let mut next = 0;
                collect_saved_pane_ids(&pending.saved, &mut next, &mut pane_ids);
            }
            pane_ids.sort_unstable();
            pane_ids.dedup();
            for pane_id in pane_ids.into_iter().filter(|pane_id| {
                origin_for_pane(state, tab.id, *pane_id).as_deref()
                    == Some(identity.pane_origin.as_str())
            }) {
                if resolved.is_some() {
                    return None;
                }
                resolved = Some((tab.id, pane_id));
            }
        }
    }
    resolved
}

pub fn format_age(ts_unix_ms: u64, now_unix_ms: u64) -> String {
    let seconds = now_unix_ms.saturating_sub(ts_unix_ms) / 1_000;
    match seconds {
        0..=9 => "just now".to_string(),
        10..=59 => format!("{seconds}s ago"),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> WorkIdentity {
        WorkIdentity {
            session: "test".to_string(),
            workspace_origin: "workspace-test".to_string(),
            tab_origin: "tab-test".to_string(),
            pane_origin: "pane-test".to_string(),
            workspace_id: 1,
            workspace_name: "workspace".to_string(),
            tab_id: 2,
            tab_name: "tab".to_string(),
            pane_id: 3,
            task_id: Some("EXAMPLE-111".to_string()),
            task_title: Some("Ledger".to_string()),
        }
    }

    fn draft(key: &str, summary: &str) -> WorkDraft {
        WorkDraft {
            kind: WorkKind::AssistantMessageCompleted,
            summary: summary.to_string(),
            identity: identity(),
            evidence_source: EvidenceSource::AgentTranscript,
            authority: WorkAuthority::AgentObservation,
            verification: VerificationState::Observed,
            dedupe_scope: "test".to_string(),
            fingerprint: key.to_string(),
        }
    }

    fn work_stream_record(seq: u64, pane_number: u32, kind: WorkKind) -> WorkRecord {
        let mut identity = identity();
        identity.pane_origin = format!("pane-{pane_number}");
        identity.pane_id = pane_number;
        identity.task_id = Some(format!("TASK-{pane_number}"));
        identity.task_title = Some(format!("Task {pane_number}"));
        WorkRecord {
            seq,
            ts_unix_ms: seq * 10,
            kind,
            summary: format!("work {seq}"),
            identity,
            evidence_source: EvidenceSource::AgentTranscript,
            authority: WorkAuthority::AgentObservation,
            verification: VerificationState::Observed,
            task_status: None,
            pull_request: None,
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "taarof-work-ledger-{name}-{}-{}.json",
            std::process::id(),
            crate::events::unix_time_ms()
        ))
    }

    fn task_truth_input() -> TaskTruthInput {
        TaskTruthInput {
            pane_origin: "pane-test".into(),
            task_id: Some("EXAMPLE-131".into()),
            origin: WorkStreamOriginState::Live,
            bound_now: false,
            execution_known: true,
            agent_running: false,
            canonical: CanonicalTaskState::Todo,
            reconciliation: WorkReconciliation::pending("plan", "Waiting for reconciliation"),
            pull_request: None,
        }
    }

    #[test]
    fn task_truth_open_unbound_is_open_and_idle_not_running() {
        let truth = project_task_truth(&task_truth_input());
        assert_eq!(truth.canonical, CanonicalTaskState::Todo);
        assert_eq!(truth.binding, BindingState::Unbound);
        assert_eq!(truth.execution, ExecutionState::Idle);
        assert!(truth.counts_as_live_open);
    }

    #[test]
    fn runtime_probe_failure_makes_live_execution_unknown_not_idle() {
        let mut input = task_truth_input();
        input.execution_known = false;

        let truth = project_task_truth(&input);

        assert_eq!(truth.execution, ExecutionState::Unknown);
    }

    #[test]
    fn live_unbound_pane_does_not_inherit_its_previous_task() {
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "unbound-truth",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let mut previous = work_stream_record(1, pane_id, WorkKind::AgentReportedProgress);
        previous.identity = identity_for_pane(&state, tab_id, pane_id, None).unwrap();
        previous.identity.task_id = Some("OLD-TASK".into());
        previous.identity.task_title = Some("Previous task".into());
        let pane_origin = previous.identity.pane_origin.clone();
        state.work_ledger.records.push_back(previous);

        let records = state.work_ledger.records();
        let metadata = work_stream_pane_metadata(&state, &records);
        let truth = task_truth_inputs(&state, &metadata)
            .into_iter()
            .find(|input| input.pane_origin == pane_origin)
            .unwrap();

        assert_eq!(truth.task_id, None);
        assert!(!truth.bound_now);
    }

    #[test]
    fn historical_truth_uses_the_panes_latest_retained_task() {
        let mut state = crate::AppState::new();
        let mut first = work_stream_record(1, 77, WorkKind::AgentReportedProgress);
        first.identity.pane_origin = "historical-rebound".into();
        first.identity.task_id = Some("OLD-TASK".into());
        let mut latest = work_stream_record(2, 77, WorkKind::AgentReportedProgress);
        latest.identity.pane_origin = "historical-rebound".into();
        latest.identity.task_id = Some("NEW-TASK".into());
        state.work_ledger.records.push_back(first);
        state.work_ledger.records.push_back(latest);
        let records = state.work_ledger.records();

        let metadata = work_stream_pane_metadata(&state, &records);
        let truth = task_truth_inputs(&state, &metadata)
            .into_iter()
            .find(|input| input.pane_origin == "historical-rebound")
            .unwrap();

        assert_eq!(truth.task_id.as_deref(), Some("NEW-TASK"));
        assert_eq!(truth.origin, WorkStreamOriginState::Historical);
    }

    #[test]
    fn historical_truth_uses_the_surviving_workspaces_exact_plan() {
        let mut state = crate::AppState::new();
        let workspace = state
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == state.active_workspace)
            .unwrap();
        workspace.working_tree_path = Some("/repo-a".into());
        let workspace_origin = workspace.work_origin.clone();
        state.work_ledger.plan_snapshots.insert(
            "/repo-a".into(),
            HashMap::from([("TASK-1".into(), ("Still open".into(), "todo".into(), true))]),
        );
        let mut record = work_stream_record(1, 72, WorkKind::AgentReportedProgress);
        record.identity.workspace_id = state.active_workspace;
        record.identity.workspace_origin = workspace_origin;
        record.identity.task_id = Some("TASK-1".into());
        record.task_status = None;
        let pane_origin = record.identity.pane_origin.clone();
        let identity = record.identity.clone();
        state.work_ledger.records.push_back(record);
        let metadata = HashMap::from([(
            pane_origin.clone(),
            WorkStreamPaneMeta {
                identity: Some(identity),
                origin_state: WorkStreamOriginState::Historical,
                ..Default::default()
            },
        )]);

        let truth = project_task_truth(
            &task_truth_inputs(&state, &metadata)
                .into_iter()
                .find(|input| input.pane_origin == pane_origin)
                .unwrap(),
        );

        assert_eq!(truth.canonical, CanonicalTaskState::Todo);
        assert_eq!(truth.origin, WorkStreamOriginState::Historical);
        assert_eq!(truth.binding, BindingState::Unbound);
        assert!(!truth.counts_as_live_open);
    }

    #[test]
    fn task_truth_bound_idle_pane_is_bound_and_idle() {
        let mut input = task_truth_input();
        input.bound_now = true;
        let truth = project_task_truth(&input);
        assert_eq!(truth.binding, BindingState::Bound);
        assert_eq!(truth.execution, ExecutionState::Idle);
    }

    #[test]
    fn task_truth_bound_running_pane_is_bound_and_running() {
        let mut input = task_truth_input();
        input.bound_now = true;
        input.agent_running = true;
        let truth = project_task_truth(&input);
        assert_eq!(truth.binding, BindingState::Bound);
        assert_eq!(truth.execution, ExecutionState::Running);
    }

    #[test]
    fn task_truth_detected_agent_without_binding_is_running_and_unbound() {
        let mut input = task_truth_input();
        input.task_id = None;
        input.agent_running = true;
        let truth = project_task_truth(&input);
        assert_eq!(truth.binding, BindingState::Unbound);
        assert_eq!(truth.execution, ExecutionState::Running);
    }

    #[test]
    fn task_truth_historical_done_is_historical_and_excluded_from_live_open() {
        let mut input = task_truth_input();
        input.origin = WorkStreamOriginState::Historical;
        input.bound_now = true;
        input.agent_running = true;
        input.canonical = CanonicalTaskState::Done;
        let summary = project_task_truth_summary(&[input]);
        let truth = &summary.items[0];
        assert_eq!(truth.origin, WorkStreamOriginState::Historical);
        assert_eq!(truth.binding, BindingState::Unbound);
        assert_eq!(truth.execution, ExecutionState::Unknown);
        assert!(!truth.counts_as_live_open);
        assert_eq!(summary.historical_count, 1);
    }

    #[test]
    fn task_truth_historical_record_stays_historical_when_plan_task_still_open() {
        let mut input = task_truth_input();
        input.origin = WorkStreamOriginState::Historical;
        let truth = project_task_truth(&input);
        assert_eq!(truth.origin, WorkStreamOriginState::Historical);
        assert_eq!(truth.binding, BindingState::Unbound);
        assert_eq!(truth.execution, ExecutionState::Unknown);
        assert!(!truth.counts_as_live_open);
    }

    #[test]
    fn task_truth_stale_source_reports_verification_and_last_checked() {
        let mut input = task_truth_input();
        input.reconciliation = WorkReconciliation {
            status: WorkReconciliationStatus::Stale,
            source: "plan".into(),
            reason: "Task no longer exists in .plan".into(),
            origin_status: "live".into(),
            current_value: None,
            checked_at_unix_ms: Some(42),
        };
        let truth = project_task_truth(&input);
        assert_eq!(truth.verification, WorkReconciliationStatus::Stale);
        assert_eq!(truth.verification_source, "plan");
        assert_eq!(truth.last_checked_unix_ms, Some(42));
    }

    #[test]
    fn task_truth_open_pr_beside_todo_task_is_source_attributed_mismatch() {
        let mut input = task_truth_input();
        input.reconciliation = WorkReconciliation {
            status: WorkReconciliationStatus::Stale,
            source: "github".into(),
            reason: "Open pull request disagrees with canonical task status".into(),
            origin_status: "live".into(),
            current_value: Some("open".into()),
            checked_at_unix_ms: Some(84),
        };
        input.pull_request = Some(WorkPullRequestRef {
            repository: "owner/repo".into(),
            number: 131,
            title: "Task truth".into(),
            url: "https://github.com/owner/repo/pull/131".into(),
            state: "open".into(),
            is_draft: false,
            review_decision: None,
            checks: crate::tracking::PullRequestChecks::Pending,
        });
        let summary = project_task_truth_summary(&[input.clone()]);
        let mismatch = summary.items[0].mismatch.as_ref().unwrap();
        assert_eq!(mismatch.source, "github");
        assert_eq!(mismatch.current_value.as_deref(), Some("open"));
        assert_eq!(summary.mismatch_count, 1);
        assert_eq!(input.canonical, CanonicalTaskState::Todo);
    }

    #[test]
    fn task_truth_superseded_open_pr_observation_is_not_a_current_mismatch() {
        let mut input = task_truth_input();
        input.canonical = CanonicalTaskState::Done;
        input.reconciliation = WorkReconciliation {
            status: WorkReconciliationStatus::Stale,
            source: "github".into(),
            reason: "GitHub pull request state has superseded this observation".into(),
            origin_status: "live".into(),
            current_value: Some("merged".into()),
            checked_at_unix_ms: Some(85),
        };
        input.pull_request = Some(WorkPullRequestRef {
            repository: "owner/repo".into(),
            number: 131,
            title: "Task truth".into(),
            url: "https://github.com/owner/repo/pull/131".into(),
            state: "open".into(),
            is_draft: false,
            review_decision: None,
            checks: crate::tracking::PullRequestChecks::Pending,
        });

        assert_eq!(project_task_truth(&input).mismatch, None);
    }

    #[test]
    fn task_truth_unavailable_github_does_not_verify_a_mismatch() {
        let mut input = task_truth_input();
        input.reconciliation = WorkReconciliation {
            status: WorkReconciliationStatus::Stale,
            source: "github".into(),
            reason: "GitHub is unavailable; preserving the last verified observation".into(),
            origin_status: "live".into(),
            current_value: None,
            checked_at_unix_ms: Some(86),
        };
        input.pull_request = Some(WorkPullRequestRef {
            repository: "owner/repo".into(),
            number: 131,
            title: "Task truth".into(),
            url: "https://github.com/owner/repo/pull/131".into(),
            state: "open".into(),
            is_draft: false,
            review_decision: None,
            checks: crate::tracking::PullRequestChecks::Pending,
        });

        assert_eq!(project_task_truth(&input).mismatch, None);
    }

    #[test]
    fn ordering_and_dedupe_use_a_ledger_local_sequence() {
        let mut ledger = WorkLedger::empty("test".into(), 10, u64::MAX, None);
        assert_eq!(ledger.append_at(10, draft("one", "one")).unwrap().seq, 1);
        assert!(ledger.append_at(11, draft("one", "duplicate")).is_none());
        assert_eq!(ledger.append_at(12, draft("two", "two")).unwrap().seq, 2);
        assert_eq!(
            ledger.records().iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            ledger.append_at(13, draft("one", "one again")).unwrap().seq,
            3,
            "A -> B -> A is a real transition"
        );
    }

    #[test]
    fn unchanged_reconciliation_advances_freshness_without_a_stream_change() {
        let mut ledger = WorkLedger::empty("test".into(), 10, u64::MAX, None);
        let record = ledger.append_at(10, draft("one", "one")).unwrap();
        let mut refreshed = ledger.reconciliation_for(record.seq);
        refreshed.checked_at_unix_ms = Some(20);

        assert!(ledger.set_reconciliation(record.seq, refreshed).is_none());
        assert_eq!(
            ledger.reconciliation_for(record.seq).checked_at_unix_ms,
            Some(20)
        );
    }

    #[test]
    fn repeated_identical_probe_is_silent_and_does_not_duplicate_work() {
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "probe-dedupe",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        state.find_tab_mut(tab_id).unwrap().set_pane_agent_activity(
            pane_id,
            crate::workspace::AgentActivity::socket(
                crate::workspace::AgentActivityState::Running,
                "working",
                Some("codex".into()),
            ),
        );
        state.observe_work_probe(collect_probe(&state));
        let event_next_seq = state.event_store.next_seq();
        let record_count = state.work_ledger.records().len();

        state.observe_work_probe(collect_probe(&state));

        assert_eq!(state.event_store.next_seq(), event_next_seq);
        assert_eq!(state.work_ledger.records().len(), record_count);
    }

    #[test]
    fn large_origin_flip_emits_summary_plus_bounded_per_entity_transitions() {
        let mut state = crate::AppState::new();
        for index in 0..=RECONCILIATION_EVENT_BURST_LIMIT {
            let mut next = draft(&format!("record-{index}"), "work");
            next.dedupe_scope = format!("record-{index}");
            next.identity.pane_origin = format!("pane-{index}");
            state.work_ledger.append(next).unwrap();
        }
        let origins = state
            .work_ledger
            .records()
            .iter()
            .map(|record| record.identity.pane_origin.clone())
            .collect::<Vec<_>>();
        let live_origins = origins.iter().map(String::as_str).collect();
        state
            .work_ledger
            .reconcile_runtime_origins(&live_origins, 20);

        let changes = state
            .work_ledger
            .reconcile_runtime_origins(&std::collections::HashSet::new(), 30);
        state.emit_reconciliation_changes(changes);

        let events = state.event_store.entries();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "work_reconciled_batch")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "work_reconciled")
                .count(),
            RECONCILIATION_EVENT_BURST_LIMIT
        );
        let batch = events
            .iter()
            .find(|event| event.event_type == "work_reconciled_batch")
            .unwrap();
        assert_eq!(
            batch.payload["total"],
            serde_json::json!(RECONCILIATION_EVENT_BURST_LIMIT + 1)
        );
        assert_eq!(batch.payload["work_seq_min"], 1);
        assert_eq!(
            batch.payload["work_seq_max"],
            serde_json::json!(RECONCILIATION_EVENT_BURST_LIMIT + 1)
        );
        assert_eq!(
            batch.payload["by_status"]["verified"],
            batch.payload["total"]
        );
        assert_eq!(batch.payload["truncated"], true);
        assert_eq!(batch.payload["resnapshot_required"], true);
        assert_eq!(
            batch.payload["per_entity_emitted"],
            serde_json::json!(RECONCILIATION_EVENT_BURST_LIMIT)
        );
        assert_eq!(batch.payload["per_entity_omitted"], 1);
        let emitted_work_seqs = events
            .iter()
            .filter(|event| event.event_type == "work_reconciled")
            .map(|event| event.payload["work_seq"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            emitted_work_seqs,
            (1..=RECONCILIATION_EVENT_BURST_LIMIT as u64).collect::<Vec<_>>()
        );
    }

    #[test]
    fn small_origin_flip_keeps_per_entity_reconciliation_events() {
        let mut state = crate::AppState::new();
        for index in 0..2 {
            let mut next = draft(&format!("record-{index}"), "work");
            next.dedupe_scope = format!("record-{index}");
            next.identity.pane_origin = format!("pane-{index}");
            state.work_ledger.append(next).unwrap();
        }

        let changes = state
            .work_ledger
            .reconcile_runtime_origins(&std::collections::HashSet::new(), 20);
        state.emit_reconciliation_changes(changes);

        let events = state.event_store.entries();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "work_reconciled")
                .count(),
            2
        );
        assert!(events
            .iter()
            .all(|event| event.event_type != "work_reconciled_batch"));
    }

    #[test]
    fn agent_activity_dedupes_only_unchanged_consecutive_states() {
        let mut ledger = WorkLedger::empty("test".into(), 10, u64::MAX, None);
        assert!(ledger.observe_activity("pane", None).is_none());
        assert_eq!(
            ledger.observe_activity("pane", Some("running")).as_deref(),
            Some("running")
        );
        assert!(ledger.observe_activity("pane", Some("running")).is_none());
        assert_eq!(
            ledger
                .observe_activity("pane", Some("waitinginput"))
                .as_deref(),
            Some("waitinginput")
        );
        assert_eq!(
            ledger.observe_activity("pane", Some("running")).as_deref(),
            Some("running")
        );
        assert_eq!(
            ledger.observe_activity("pane", None).as_deref(),
            Some("idle")
        );
    }

    #[test]
    fn retention_is_bounded_by_count_and_age() {
        let mut ledger = WorkLedger::empty("test".into(), 2, 100, None);
        ledger.append_at(1_000, draft("one", "one"));
        ledger.append_at(1_050, draft("two", "two"));
        ledger.append_at(1_100, draft("three", "three"));
        assert_eq!(
            ledger.records().iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![2, 3]
        );
        ledger.append_at(1_200, draft("four", "four"));
        assert_eq!(
            ledger.records().iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    #[test]
    fn idle_prune_enforces_age_without_an_append() {
        let mut ledger = WorkLedger::empty("test".into(), 10, 100, None);
        ledger.append_at(1_000, draft("one", "one"));
        assert_eq!(ledger.metadata_json()["stored"], 0);
        assert!(ledger.sidebar_records("pane-test").is_empty());
        assert!(ledger.prune_at(1_101));
        assert!(ledger.records().is_empty());
        assert!(!ledger.prune_at(1_102), "unchanged idle prune dedupes");
    }

    #[test]
    fn persistence_restores_records_and_next_sequence() {
        let path = temp_path("restore");
        {
            let mut ledger = WorkLedger::with_storage("alpha", path.clone(), 10, u64::MAX);
            let mut first = draft("one", "one");
            first.identity.session = "alpha".into();
            ledger.append_at(10, first);
            ledger.flush();
        }
        let mut restored = WorkLedger::with_storage("alpha", path.clone(), 10, u64::MAX);
        assert_eq!(restored.records().len(), 1);
        let mut second = draft("two", "two");
        second.identity.session = "alpha".into();
        assert_eq!(restored.append_at(11, second).unwrap().seq, 2);
        restored.flush();
        let _ = fs::remove_file(path);
    }

    #[test]
    fn agent_report_boundary_rejects_bypasses_and_rewrites_hostile_storage() {
        let hostile = "Password RAW_PERSISTED_REPORT_SENTINEL";
        let summary = format!("Agent reported progress: {hostile}");
        let git_sha = "0123456789abcdef0123456789abcdef01234567";
        let mut state = crate::AppState::new();
        assert!(state
            .append_work_record(WorkDraft {
                kind: WorkKind::AgentReportedProgress,
                summary: summary.clone(),
                identity: identity(),
                evidence_source: EvidenceSource::AgentReport,
                authority: WorkAuthority::AgentObservation,
                verification: VerificationState::Observed,
                dedupe_scope: "hostile-agent-report".into(),
                fingerprint: "hostile-agent-report".into(),
            })
            .is_none());
        assert!(state
            .append_work_record(WorkDraft {
                kind: WorkKind::AgentReportedProgress,
                summary: format!("Agent reported progress: Verified commit {git_sha}"),
                identity: identity(),
                evidence_source: EvidenceSource::AgentReport,
                authority: WorkAuthority::AgentObservation,
                verification: VerificationState::Observed,
                dedupe_scope: "safe-agent-report".into(),
                fingerprint: "safe-agent-report".into(),
            })
            .is_some());
        assert!(state
            .append_work_record(WorkDraft {
                kind: WorkKind::AgentReportedProgress,
                summary: "Agent reported progress: token parser tests passed".into(),
                identity: identity(),
                evidence_source: EvidenceSource::AgentReport,
                authority: WorkAuthority::AgentObservation,
                verification: VerificationState::Observed,
                dedupe_scope: "ordinary-token-prose".into(),
                fingerprint: "ordinary-token-prose".into(),
            })
            .is_some());
        assert!(state
            .append_work_record(WorkDraft {
                kind: WorkKind::AgentReportedProgress,
                summary: format!("Agent reported progress: {git_sha}"),
                identity: identity(),
                evidence_source: EvidenceSource::AgentReport,
                authority: WorkAuthority::AgentObservation,
                verification: VerificationState::Observed,
                dedupe_scope: "bare-git-sha".into(),
                fingerprint: "bare-git-sha".into(),
            })
            .is_some());
        for surface in [
            serde_json::to_string(&state.work_ledger.records()).unwrap(),
            serde_json::to_string(&state.event_store.entries()).unwrap(),
            crate::api::build_state_snapshot(&state).to_string(),
            serde_json::to_string(&state.work_ledger.sidebar_records("pane-test")).unwrap(),
        ] {
            assert!(!surface.contains(hostile));
            assert!(surface.contains(git_sha));
            assert!(surface.contains("token parser tests passed"));
        }

        let path = temp_path("hostile-agent-report");
        fs::write(
            &path,
            serde_json::to_string(&PersistedLedger {
                schema: "taarof.work-ledger.v1".into(),
                session: "alpha".into(),
                next_seq: 2,
                records: VecDeque::from([WorkRecord {
                    seq: 1,
                    ts_unix_ms: crate::events::unix_time_ms(),
                    kind: WorkKind::AgentReportedProgress,
                    summary,
                    identity: identity(),
                    evidence_source: EvidenceSource::AgentReport,
                    authority: WorkAuthority::AgentObservation,
                    verification: VerificationState::Observed,
                    task_status: None,
                    pull_request: None,
                }]),
                view: WorkStreamPreferences::default(),
                last_message_ordinals: HashMap::new(),
                last_file_ordinals: HashMap::new(),
                pull_request_snapshots: HashMap::new(),
            })
            .unwrap(),
        )
        .unwrap();
        let ledger = WorkLedger::with_storage("alpha", path.clone(), 10, u64::MAX);
        assert!(ledger.records().is_empty());
        assert!(ledger.metadata_json()["records"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(ledger.sidebar_records("pane-test").is_empty());
        ledger.flush();
        assert!(!fs::read_to_string(&path).unwrap().contains(hostile));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn load_pruning_immediately_rewrites_expired_and_over_capacity_storage() {
        let path = temp_path("load-prune");
        let now = crate::events::unix_time_ms();
        let alpha_identity = || {
            let mut value = identity();
            value.session = "alpha".into();
            value
        };
        let records = VecDeque::from([
            WorkRecord {
                seq: 1,
                ts_unix_ms: now.saturating_sub(100_000),
                kind: WorkKind::AssistantMessageCompleted,
                summary: "expired".into(),
                identity: alpha_identity(),
                evidence_source: EvidenceSource::AgentTranscript,
                authority: WorkAuthority::AgentObservation,
                verification: VerificationState::Observed,
                task_status: None,
                pull_request: None,
            },
            WorkRecord {
                seq: 2,
                ts_unix_ms: now,
                kind: WorkKind::AssistantMessageCompleted,
                summary: "capacity-drop".into(),
                identity: alpha_identity(),
                evidence_source: EvidenceSource::AgentTranscript,
                authority: WorkAuthority::AgentObservation,
                verification: VerificationState::Observed,
                task_status: None,
                pull_request: None,
            },
            WorkRecord {
                seq: 3,
                ts_unix_ms: now,
                kind: WorkKind::AssistantMessageCompleted,
                summary: "keep-three".into(),
                identity: alpha_identity(),
                evidence_source: EvidenceSource::AgentTranscript,
                authority: WorkAuthority::AgentObservation,
                verification: VerificationState::Observed,
                task_status: None,
                pull_request: None,
            },
            WorkRecord {
                seq: 4,
                ts_unix_ms: now,
                kind: WorkKind::AssistantMessageCompleted,
                summary: "keep-four".into(),
                identity: alpha_identity(),
                evidence_source: EvidenceSource::AgentTranscript,
                authority: WorkAuthority::AgentObservation,
                verification: VerificationState::Observed,
                task_status: None,
                pull_request: None,
            },
        ]);
        fs::write(
            &path,
            serde_json::to_string(&PersistedLedger {
                schema: "taarof.work-ledger.v1".into(),
                session: "alpha".into(),
                next_seq: 5,
                records,
                view: WorkStreamPreferences::default(),
                last_message_ordinals: HashMap::new(),
                last_file_ordinals: HashMap::new(),
                pull_request_snapshots: HashMap::new(),
            })
            .unwrap(),
        )
        .unwrap();

        let ledger = WorkLedger::with_storage("alpha", path.clone(), 2, 10_000);
        ledger.flush();
        let saved: PersistedLedger =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved
                .records
                .iter()
                .map(|record| record.seq)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(saved.next_seq, 5);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn different_session_does_not_restore_another_namespace() {
        let path = temp_path("namespace");
        let mut alpha = WorkLedger::with_storage("alpha", path.clone(), 10, u64::MAX);
        alpha.append_at(10, draft("one", "one"));
        alpha.flush();
        let beta = WorkLedger::with_storage("beta", path.clone(), 10, u64::MAX);
        assert!(beta.records().is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn writer_preserves_fifo_order_and_private_permissions_even_with_stale_temp() {
        let root = temp_path("permissions-root").with_extension("dir");
        let path = root.join("ledger.json");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, "stale").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();
        {
            let mut ledger = WorkLedger::with_storage("alpha", path.clone(), 10, u64::MAX);
            ledger.append_at(10, draft("one", "one"));
            ledger.append_at(11, draft("two", "two"));
            ledger.append_at(12, draft("three", "three"));
            ledger.flush();
        }
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let saved: PersistedLedger =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved
                .records
                .iter()
                .map(|record| record.seq)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn batched_burst_serializes_off_thread_and_persists_only_latest_snapshot() {
        let path = temp_path("coalesced-burst");
        let mut ledger = WorkLedger::with_storage("alpha", path.clone(), 500, u64::MAX);
        ledger.begin_persistence_batch();
        for index in 0..500 {
            ledger.append_at(
                10 + index,
                draft(&format!("event-{index}"), &format!("event {index}")),
            );
        }
        ledger.end_persistence_batch();
        ledger.flush();

        let (requests, serializations, writes, serialization_thread) =
            ledger.persistence_counts().unwrap();
        assert_eq!(requests, 1, "one final snapshot is cloned for the burst");
        assert_eq!(serializations, 1, "only the latest snapshot is serialized");
        assert_eq!(writes, 1, "only the latest snapshot is fsynced");
        assert_ne!(
            serialization_thread,
            Some(std::thread::current().id()),
            "serde runs on the writer rather than the caller/GTK thread"
        );
        let saved: PersistedLedger =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.records.len(), 500);
        assert_eq!(saved.next_seq, 501);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn writer_spawn_failure_degrades_to_in_memory_without_panicking() {
        let path = temp_path("worker-failure");
        let mut ledger =
            WorkLedger::load_or_empty_with_start("alpha".into(), path, 10, u64::MAX, |_| {
                Err(io::Error::other("injected spawn failure"))
            });
        assert!(ledger.persistence.is_none());
        assert_eq!(ledger.append_at(10, draft("one", "one")).unwrap().seq, 1);
        assert_eq!(ledger.records().len(), 1);
    }

    #[test]
    fn atomic_writer_creates_private_storage_directory() {
        let root = temp_path("private-directory").with_extension("dir");
        let private = root.join("taarof");
        let path = private.join("ledger.json");
        write_atomically(&path, "{}").unwrap();
        assert_eq!(
            fs::metadata(&private).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn colliding_slugs_have_distinct_storage_paths() {
        let root = Path::new("/data/taarof");
        let slash = storage_path_for(root, Some("dev/tools"));
        let space = storage_path_for(root, Some("dev tools"));
        assert_ne!(slash, space);
        assert_ne!(
            storage_path_for(root, Some("a?={$}%[)^,b")),
            storage_path_for(root, Some(r"a#\,|~^@:?{b"))
        );
        assert_eq!(
            storage_path_for(root, None),
            root.join("work-ledger-default.json")
        );
    }

    #[test]
    fn records_serialize_authority_verification_and_no_payload_fields() {
        let mut ledger = WorkLedger::empty("test".into(), 10, u64::MAX, None);
        ledger.append_at(10, draft("one", "Assistant message completed"));
        let json = serde_json::to_string(&ledger.records()).unwrap();
        assert!(json.contains("agent_transcript"));
        assert!(json.contains("agent_observation"));
        assert!(json.contains("observed"));
        for forbidden in ["last_message", "terminal", "tool_payload", "file_path"] {
            assert!(!json.contains(forbidden));
        }
    }

    #[test]
    fn plan_observation_baselines_then_reports_create_and_status_change() {
        let mut ledger = WorkLedger::empty("test".into(), 10, u64::MAX, None);
        let first = vec![ObservedPlanTask {
            id: "A".into(),
            title: "First".into(),
            status: "todo".into(),
            fully_specified: true,
        }];
        assert!(ledger.observe_plan("/repo", &first).is_empty());
        let second = vec![
            ObservedPlanTask {
                id: "A".into(),
                title: "First".into(),
                status: "done".into(),
                fully_specified: true,
            },
            ObservedPlanTask {
                id: "B".into(),
                title: "Second".into(),
                status: "todo".into(),
                fully_specified: true,
            },
        ];
        let changes = ledger.observe_plan("/repo", &second);
        assert_eq!(changes.len(), 2);
        assert!(changes
            .iter()
            .any(|change| change.0 == WorkKind::TaskCreated));
        assert!(changes
            .iter()
            .any(|change| change.0 == WorkKind::TaskStatusChanged));
        assert!(ledger.observe_plan("/repo", &second).is_empty());
    }

    #[test]
    fn removal_and_readd_records_each_creation_without_unchanged_probe_spam() {
        fn pane(tasks: Vec<ObservedPlanTask>) -> PaneProbe {
            PaneProbe {
                identity: WorkIdentity {
                    task_id: None,
                    task_title: None,
                    ..identity()
                },
                activity: None,
                local_cwd: Some("/repo".into()),
                binding_checkout_root: None,
                binding_identity: None,
                plan_root: Some("/repo".into()),
                plan_tasks: tasks,
                plan_loaded: true,
            }
        }
        fn task() -> ObservedPlanTask {
            ObservedPlanTask {
                id: "A".into(),
                title: "First".into(),
                status: "todo".into(),
                fully_specified: true,
            }
        }

        let mut state = crate::AppState::new();
        state.observe_work_probe_unchecked(WorkProbe {
            panes: vec![pane(vec![])],
            exact_pull_requests: Vec::new(),
        });
        state.observe_work_probe_unchecked(WorkProbe {
            panes: vec![pane(vec![task()])],
            exact_pull_requests: Vec::new(),
        });
        state.observe_work_probe_unchecked(WorkProbe {
            panes: vec![pane(vec![task()])],
            exact_pull_requests: Vec::new(),
        });
        state.observe_work_probe_unchecked(WorkProbe {
            panes: vec![pane(vec![])],
            exact_pull_requests: Vec::new(),
        });
        state.observe_work_probe_unchecked(WorkProbe {
            panes: vec![pane(vec![task()])],
            exact_pull_requests: Vec::new(),
        });
        assert_eq!(
            state
                .work_ledger
                .records()
                .iter()
                .filter(|record| record.kind == WorkKind::TaskCreated)
                .count(),
            2
        );
    }

    #[test]
    fn unbound_local_pane_uses_nearest_plan_and_only_plan_deltas_create_tasks() {
        fn write_plan(root: &Path, tasks: serde_json::Value) {
            fs::create_dir_all(root.join(".plan")).unwrap();
            fs::write(
                root.join(".plan/tasks.json"),
                serde_json::json!({"tasks": tasks}).to_string(),
            )
            .unwrap();
        }
        fn task(id: &str, status: &str) -> serde_json::Value {
            serde_json::json!({
                "id": id,
                "title": format!("Task {id}"),
                "status": status,
                "description": "Real work",
                "acceptance_criteria": ["It works"],
                "verify_command": "cargo test"
            })
        }

        let outer = temp_path("nearest-plan").with_extension("dir");
        let inner = outer.join("packages/app");
        let cwd = inner.join("src/deep");
        fs::create_dir_all(&cwd).unwrap();
        write_plan(&outer, serde_json::json!([task("OUTER", "todo")]));
        write_plan(&inner, serde_json::json!([task("A", "todo")]));
        assert_eq!(
            nearest_plan_root(&cwd).as_deref(),
            Some(inner.canonicalize().unwrap().to_string_lossy().as_ref())
        );
        let empty_boundary = outer.join("packages/empty");
        let empty_cwd = empty_boundary.join("src/deep");
        fs::create_dir_all(empty_boundary.join(".plan")).unwrap();
        fs::create_dir_all(&empty_cwd).unwrap();
        assert_eq!(
            nearest_plan_root(&empty_cwd).as_deref(),
            Some(
                empty_boundary
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            ),
            "an unreadable/incomplete child boundary never falls through to a parent plan"
        );

        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "unbound",
            crate::HeadlessPaneSeed {
                cwd: Some(cwd.to_string_lossy().into_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        state.observe_work_probe(enrich_plan_probe(collect_probe(&state)));
        write_plan(
            &inner,
            serde_json::json!([task("A", "todo"), task("B", "todo")]),
        );
        state.observe_work_probe(enrich_plan_probe(collect_probe(&state)));
        state.observe_work_probe(enrich_plan_probe(collect_probe(&state)));
        let records = state.work_ledger.records();
        assert_eq!(
            records
                .iter()
                .filter(|record| record.kind == WorkKind::TaskCreated)
                .count(),
            1,
            "unbound baseline-to-add creates once and unchanged probes do not spam"
        );
        assert_eq!(
            records.last().unwrap().identity.task_id.as_deref(),
            Some("B")
        );

        write_plan(
            &inner,
            serde_json::json!([task("A", "todo"), task("B", "done")]),
        );
        state.observe_work_probe(enrich_plan_probe(collect_probe(&state)));
        assert!(state
            .work_ledger
            .records()
            .iter()
            .all(|record| record.kind != WorkKind::TaskStatusChanged));

        state.project_agent_message(&serde_json::json!({
            "tab_id": tab_id,
            "pane_id": pane_id,
            "session_id": "transcript-only",
            "work_events": [{"kind":"assistant_completed", "ordinal":1}]
        }));
        assert_eq!(
            state
                .work_ledger
                .records()
                .iter()
                .filter(|record| record.kind == WorkKind::TaskCreated)
                .count(),
            1,
            "transcript evidence cannot create canonical tasks"
        );
        let _ = fs::remove_dir_all(outer);
    }

    #[test]
    fn off_thread_probe_is_discarded_after_unbound_pane_changes_checkout() {
        fn write_plan(root: &Path, task_ids: &[&str]) {
            fs::create_dir_all(root.join(".plan")).unwrap();
            let tasks = task_ids
                .iter()
                .map(|id| {
                    serde_json::json!({
                        "id": id,
                        "title": format!("Task {id}"),
                        "status": "todo",
                        "description": "Real work",
                        "acceptance_criteria": ["It works"],
                        "verify_command": "cargo test"
                    })
                })
                .collect::<Vec<_>>();
            fs::write(
                root.join(".plan/tasks.json"),
                serde_json::json!({"tasks": tasks}).to_string(),
            )
            .unwrap();
        }

        let old_root = temp_path("unbound-old-checkout").with_extension("dir");
        let new_root = temp_path("unbound-new-checkout").with_extension("dir");
        let old_cwd = old_root.join("src");
        let new_cwd = new_root.join("src");
        fs::create_dir_all(&old_cwd).unwrap();
        fs::create_dir_all(&new_cwd).unwrap();
        write_plan(&old_root, &["OLD-A"]);
        write_plan(&new_root, &["NEW-A"]);

        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "unbound-switch",
            crate::HeadlessPaneSeed {
                cwd: Some(old_cwd.to_string_lossy().into_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        state.observe_work_probe(enrich_plan_probe(collect_probe(&state)));

        write_plan(&old_root, &["OLD-A", "OLD-B"]);
        let stale = enrich_plan_probe(collect_probe(&state));
        state
            .headless_pane_mut(tab_id, pane_id)
            .unwrap()
            .location_state
            .cwd = Some(new_cwd.to_string_lossy().into_owned());

        state.observe_work_probe(stale);
        assert!(
            state
                .work_ledger
                .records()
                .iter()
                .all(|record| { record.identity.task_id.as_deref() != Some("OLD-B") }),
            "a completed probe from the old nearest-plan context must be discarded"
        );

        // The next probe establishes the new checkout baseline without
        // carrying the old checkout's delta across the cwd switch.
        state.observe_work_probe(enrich_plan_probe(collect_probe(&state)));
        assert!(state.work_ledger.records().is_empty());
        let _ = fs::remove_dir_all(old_root);
        let _ = fs::remove_dir_all(new_root);
    }

    #[test]
    fn incomplete_plan_tasks_never_create_records() {
        let mut ledger = WorkLedger::empty("test".into(), 10, u64::MAX, None);
        ledger.observe_plan("/repo", &[]);
        let changes = ledger.observe_plan(
            "/repo",
            &[ObservedPlanTask {
                id: "B".into(),
                title: "Second".into(),
                status: "todo".into(),
                fully_specified: false,
            }],
        );
        assert!(changes.is_empty());
    }

    #[test]
    fn plan_file_requires_description_acceptance_and_verify_command_for_creation() {
        let root = std::env::temp_dir().join(format!(
            "taarof-ledger-plan-{}-{}",
            std::process::id(),
            crate::events::unix_time_ms()
        ));
        fs::create_dir_all(root.join(".plan")).unwrap();
        fs::write(
            root.join(".plan/tasks.json"),
            serde_json::json!({"tasks": [{
                "id": "A", "title": "Incomplete", "status": "todo",
                "description": "", "acceptance_criteria": [], "verify_command": ""
            }, {
                "id": "B", "title": "Complete", "status": "in_progress",
                "description": "Real work", "acceptance_criteria": ["It works"],
                "verify_command": "cargo test"
            }]})
            .to_string(),
        )
        .unwrap();
        let tasks = load_plan_observations(&root).expect("valid plan");
        assert_eq!(tasks.len(), 2);
        assert!(!tasks[0].fully_specified);
        assert!(tasks[1].fully_specified);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sidebar_records_are_oldest_to_newest_for_one_pane() {
        let mut ledger = WorkLedger::empty("test".into(), 20, u64::MAX, None);
        ledger.append_at(10, draft("one", "one"));
        ledger.append_at(20, draft("two", "two"));
        let records = ledger.sidebar_records("pane-test");
        assert_eq!(
            records
                .iter()
                .map(|record| record.summary.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        assert!(ledger.pane_records("other-pane", 8).is_empty());
    }

    #[test]
    fn work_stream_projects_two_panes_in_ledger_sequence_order() {
        let mut first = identity();
        first.pane_origin = "pane-first".into();
        first.pane_id = 7;
        let mut second = identity();
        second.pane_origin = "pane-second".into();
        second.pane_id = 9;
        let records = vec![
            WorkRecord {
                seq: 3,
                ts_unix_ms: 20,
                kind: WorkKind::AssistantMessageCompleted,
                summary: "later".into(),
                identity: second,
                evidence_source: EvidenceSource::AgentTranscript,
                authority: WorkAuthority::AgentObservation,
                verification: VerificationState::Observed,
                task_status: None,
                pull_request: None,
            },
            WorkRecord {
                seq: 2,
                ts_unix_ms: 10,
                kind: WorkKind::AssistantMessageCompleted,
                summary: "earlier".into(),
                identity: first,
                evidence_source: EvidenceSource::AgentTranscript,
                authority: WorkAuthority::AgentObservation,
                verification: VerificationState::Observed,
                task_status: None,
                pull_request: None,
            },
        ];
        let mut palette = WorkStreamPalette::default();
        let projection = project_work_stream(
            &records,
            &mut palette,
            &WorkStreamFilter::All,
            &HashMap::new(),
        );

        assert_eq!(
            projection
                .entries
                .iter()
                .map(|entry| entry.record.summary.as_str())
                .collect::<Vec<_>>(),
            vec!["earlier", "later"]
        );
        assert_eq!(projection.entries[0].marker, "P1");
        assert_eq!(projection.entries[1].marker, "P2");
        assert_eq!(projection.entries[0].color_slot, Some(0));
        assert_eq!(projection.entries[1].color_slot, Some(1));
    }

    #[test]
    fn history_filter_surfaces_done_and_historical_items() {
        let live = work_stream_record(1, 1, WorkKind::TaskStatusChanged);
        let mut done = work_stream_record(2, 2, WorkKind::TaskStatusChanged);
        done.task_status = Some("done".into());
        let historical = work_stream_record(3, 3, WorkKind::AgentReportedProgress);
        let metadata = HashMap::from([
            (
                "pane-1".into(),
                WorkStreamPaneMeta {
                    origin_state: WorkStreamOriginState::Live,
                    ..Default::default()
                },
            ),
            (
                "pane-2".into(),
                WorkStreamPaneMeta {
                    origin_state: WorkStreamOriginState::Live,
                    ..Default::default()
                },
            ),
            (
                "pane-3".into(),
                WorkStreamPaneMeta {
                    origin_state: WorkStreamOriginState::Historical,
                    ..Default::default()
                },
            ),
        ]);
        let mut palette = WorkStreamPalette::default();
        let history = project_work_stream(
            &[live, done, historical],
            &mut palette,
            &WorkStreamFilter::History,
            &metadata,
        );
        assert_eq!(
            history
                .entries
                .iter()
                .map(|entry| entry.record.seq)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn history_filter_does_not_merge_same_task_id_across_panes() {
        let mut open = work_stream_record(1, 1, WorkKind::TaskStatusChanged);
        open.identity.task_id = Some("SHARED-1".into());
        open.task_status = Some("todo".into());
        let mut done = work_stream_record(2, 2, WorkKind::TaskStatusChanged);
        done.identity.task_id = Some("SHARED-1".into());
        done.task_status = Some("done".into());
        let metadata = HashMap::from([
            (
                "pane-1".into(),
                WorkStreamPaneMeta {
                    origin_state: WorkStreamOriginState::Live,
                    ..Default::default()
                },
            ),
            (
                "pane-2".into(),
                WorkStreamPaneMeta {
                    origin_state: WorkStreamOriginState::Live,
                    ..Default::default()
                },
            ),
        ]);
        let mut palette = WorkStreamPalette::default();

        let live = project_work_stream(
            &[open.clone(), done.clone()],
            &mut palette,
            &WorkStreamFilter::All,
            &metadata,
        );
        let history = project_work_stream(
            &[open, done],
            &mut palette,
            &WorkStreamFilter::History,
            &metadata,
        );

        assert_eq!(
            live.entries
                .iter()
                .map(|entry| entry.record.seq)
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            history
                .entries
                .iter()
                .map(|entry| entry.record.seq)
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn record_truth_uses_latest_canonical_status_for_the_task_chronology() {
        let mut state = crate::AppState::new();
        let mut todo = work_stream_record(1, 71, WorkKind::TaskStatusChanged);
        todo.task_status = Some("todo".into());
        let mut done = todo.clone();
        done.seq = 2;
        done.ts_unix_ms = 20;
        done.task_status = Some("done".into());
        state.work_ledger.records.push_back(todo.clone());
        state.work_ledger.records.push_back(done);
        let records = state.work_ledger.records();
        let metadata = work_stream_pane_metadata(&state, &records);
        let latest_status_records = latest_task_status_records(&records);

        let truth = task_truth_for_record_with(&state, &metadata, &latest_status_records, &todo);

        assert_eq!(truth.canonical, CanonicalTaskState::Done);
        assert_eq!(truth.origin, WorkStreamOriginState::Historical);
    }

    #[test]
    fn record_truth_attributes_open_pr_mismatch_to_cancelled_task_row() {
        let mut state = crate::AppState::new();
        let mut record = work_stream_record(1, 73, WorkKind::PullRequestOpened);
        record.evidence_source = EvidenceSource::GitHubPullRequest;
        record.authority = WorkAuthority::GitHubCanonical;
        record.verification = VerificationState::RemoteVerified;
        record.task_status = Some("cancelled".into());
        record.pull_request = Some(WorkPullRequestRef {
            repository: "owner/repo".into(),
            number: 131,
            title: "Task truth".into(),
            url: "https://github.com/owner/repo/pull/131".into(),
            state: "open".into(),
            is_draft: false,
            review_decision: None,
            checks: crate::tracking::PullRequestChecks::Pending,
        });
        state.work_ledger.records.push_back(record.clone());
        state.work_ledger.reconciliation.insert(
            record.seq,
            WorkReconciliation::checked(
                WorkReconciliationStatus::Verified,
                "github",
                "Current GitHub pull request state verified",
                90,
            ),
        );
        let records = state.work_ledger.records();
        let metadata = work_stream_pane_metadata(&state, &records);
        let latest_status_records = latest_task_status_records(&records);

        let truth = task_truth_for_record_with(&state, &metadata, &latest_status_records, &record);

        assert_eq!(truth.canonical, CanonicalTaskState::Cancelled);
        assert_eq!(truth.verification, WorkReconciliationStatus::Stale);
        assert_eq!(
            truth.mismatch.as_ref().map(|item| item.source.as_str()),
            Some("github")
        );
        assert_eq!(
            truth
                .mismatch
                .as_ref()
                .and_then(|item| item.current_value.as_deref()),
            Some("open")
        );
    }

    #[test]
    fn exact_checkout_absence_does_not_borrow_same_id_from_another_plan() {
        let mut ledger = WorkLedger::empty("test".into(), 10, u64::MAX, None);
        ledger
            .plan_snapshots
            .insert("/repo-a".into(), HashMap::new());
        ledger.plan_snapshots.insert(
            "/repo-b".into(),
            HashMap::from([("TASK-1".into(), ("Other task".into(), "done".into(), true))]),
        );

        assert_eq!(
            canonical_task_state(
                &ledger,
                Some("TASK-1"),
                Some("/repo-a"),
                None,
                &WorkReconciliation::pending("plan", "Waiting for reconciliation"),
            ),
            CanonicalTaskState::Absent
        );
    }

    #[test]
    fn record_truth_resolves_current_ids_from_stable_pane_origin() {
        let root = temp_path("record-truth-current-origin").with_extension("dir");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join(".plan")).unwrap();
        fs::write(
            root.join(".plan/tasks.json"),
            r#"{"tasks":[{"id":"TASK-1","title":"Bound task","status":"todo"}]}"#,
        )
        .unwrap();
        let root = root.canonicalize().unwrap();
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "restored",
            crate::HeadlessPaneSeed {
                cwd: Some(root.to_string_lossy().into_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let binding = state.bind_pane_to_task(tab_id, pane_id, "TASK-1").unwrap();
        let mut record = work_stream_record(1, pane_id, WorkKind::TaskStatusChanged);
        record.identity = identity_for_pane(&state, tab_id, pane_id, Some(&binding)).unwrap();
        // Model a retained pre-restore row: the stable origin matches, while
        // the old control-plane ids no longer do.
        record.identity.tab_id = tab_id.saturating_add(100);
        record.identity.pane_id = pane_id.saturating_add(100);
        record.task_status = Some("todo".into());
        state.work_ledger.records.push_back(record.clone());
        let records = state.work_ledger.records();
        let metadata = work_stream_pane_metadata(&state, &records);

        let truth = task_truth_for_record(&state, &metadata, &record);

        assert_eq!(truth.origin, WorkStreamOriginState::Live);
        assert_eq!(truth.binding, BindingState::Bound);
        assert_eq!(
            truth.execution,
            ExecutionState::Unknown,
            "stable identity does not manufacture a process observation"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn live_open_count_excludes_done_and_historical() {
        let mut done = task_truth_input();
        done.pane_origin = "done".into();
        done.canonical = CanonicalTaskState::Done;
        let mut historical = task_truth_input();
        historical.pane_origin = "historical".into();
        historical.origin = WorkStreamOriginState::Historical;
        let summary = project_task_truth_summary(&[task_truth_input(), done, historical]);
        assert_eq!(summary.live_open_count, 1);
        assert_eq!(summary.historical_count, 2);
    }

    #[test]
    fn work_snapshot_keeps_complete_history_for_web_filtering() {
        let mut state = crate::AppState::new();
        let historical = work_stream_record(1, 91, WorkKind::AgentReportedProgress);
        state.work_ledger.records.push_back(historical);

        let snapshot = work_stream_snapshot_json(&state);
        assert!(snapshot["entries"].as_array().unwrap().is_empty());
        assert_eq!(snapshot["all_entries"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["all_entries"][0]["truth"]["origin"], "historical");
    }

    #[test]
    fn work_stream_uses_ledger_sequence_when_timestamps_regress() {
        let mut first = work_stream_record(1, 1, WorkKind::FileTouched);
        first.ts_unix_ms = 500;
        let mut second = work_stream_record(2, 2, WorkKind::FileTouched);
        second.ts_unix_ms = 100;
        let mut third = work_stream_record(3, 1, WorkKind::FileTouched);
        third.ts_unix_ms = 500;
        let mut palette = WorkStreamPalette::default();
        let projection = project_work_stream(
            &[third, second, first],
            &mut palette,
            &WorkStreamFilter::All,
            &HashMap::new(),
        );

        assert_eq!(
            projection
                .entries
                .iter()
                .map(|entry| entry.record.seq)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn work_stream_colors_five_panes_and_keeps_sixth_neutral() {
        let records = (1..=6)
            .map(|pane| work_stream_record(u64::from(pane), pane, WorkKind::FileTouched))
            .collect::<Vec<_>>();
        let mut palette = WorkStreamPalette::default();
        let projection = project_work_stream(
            &records,
            &mut palette,
            &WorkStreamFilter::All,
            &HashMap::new(),
        );

        assert_eq!(projection.legend.len(), 6);
        assert_eq!(projection.overflow_count, 1);
        assert_eq!(
            projection
                .legend
                .iter()
                .map(|item| item.color_slot)
                .collect::<Vec<_>>(),
            vec![Some(0), Some(1), Some(2), Some(3), Some(4), None]
        );
        assert_eq!(projection.legend[5].marker, "OVF");
    }

    #[test]
    fn work_stream_promotion_swaps_only_fifth_slot_without_losing_history() {
        let records = (1..=6)
            .map(|pane| work_stream_record(u64::from(pane), pane, WorkKind::FileTouched))
            .collect::<Vec<_>>();
        let mut palette = WorkStreamPalette::default();
        let before = project_work_stream(
            &records,
            &mut palette,
            &WorkStreamFilter::All,
            &HashMap::new(),
        );
        assert!(palette.promote("pane-6"));
        let after = project_work_stream(
            &records,
            &mut palette,
            &WorkStreamFilter::All,
            &HashMap::new(),
        );

        assert_eq!(
            before
                .entries
                .iter()
                .map(|entry| entry.record.seq)
                .collect::<Vec<_>>(),
            after
                .entries
                .iter()
                .map(|entry| entry.record.seq)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            after
                .legend
                .iter()
                .take(4)
                .map(|item| item.color_slot)
                .collect::<Vec<_>>(),
            vec![Some(0), Some(1), Some(2), Some(3)]
        );
        assert_eq!(after.legend[4].color_slot, None);
        assert_eq!(after.legend[5].color_slot, Some(4));
        assert_eq!(after.legend[4].marker, "OVF");
        assert_eq!(after.legend[5].marker, "P5");
    }

    #[test]
    fn work_stream_filters_are_stable_chronological_subsequences() {
        let mut records = vec![
            work_stream_record(4, 2, WorkKind::AgentReportedBlocked),
            work_stream_record(1, 1, WorkKind::FileTouched),
            work_stream_record(3, 1, WorkKind::AgentReportedProgress),
            work_stream_record(2, 2, WorkKind::FileTouched),
        ];
        records[2].identity.task_id = Some("TASK-1".into());
        let mut palette = WorkStreamPalette::default();
        let pane = project_work_stream(
            &records,
            &mut palette,
            &WorkStreamFilter::Pane("pane-1".into()),
            &HashMap::new(),
        );
        let task = project_work_stream(
            &records,
            &mut palette,
            &WorkStreamFilter::Task("TASK-1".into()),
            &HashMap::new(),
        );
        let attention = project_work_stream(
            &records,
            &mut palette,
            &WorkStreamFilter::Attention,
            &HashMap::new(),
        );

        assert_eq!(
            pane.entries
                .iter()
                .map(|entry| entry.record.seq)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(
            task.entries
                .iter()
                .map(|entry| entry.record.seq)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(
            attention
                .entries
                .iter()
                .map(|entry| entry.record.seq)
                .collect::<Vec<_>>(),
            vec![4]
        );
    }

    #[test]
    fn work_stream_legend_preserves_typed_live_lazy_and_historical_state() {
        let mut records = (1..=3)
            .map(|pane| work_stream_record(u64::from(pane), pane, WorkKind::FileTouched))
            .collect::<Vec<_>>();
        records[2].identity.task_id = None;
        records[2].identity.task_title = None;
        let metadata = HashMap::from([
            (
                "pane-1".into(),
                WorkStreamPaneMeta {
                    identity: Some(records[0].identity.clone()),
                    agent_name: Some("codex".into()),
                    origin_state: WorkStreamOriginState::Live,
                    display_order: 0,
                },
            ),
            (
                "pane-2".into(),
                WorkStreamPaneMeta {
                    identity: Some(records[1].identity.clone()),
                    agent_name: None,
                    origin_state: WorkStreamOriginState::Lazy,
                    display_order: 1,
                },
            ),
        ]);
        let mut palette = WorkStreamPalette::default();
        let projection =
            project_work_stream(&records, &mut palette, &WorkStreamFilter::All, &metadata);

        assert_eq!(
            projection
                .legend
                .iter()
                .map(|item| item.origin_state)
                .collect::<Vec<_>>(),
            vec![
                WorkStreamOriginState::Live,
                WorkStreamOriginState::Lazy,
                WorkStreamOriginState::Historical,
            ]
        );
        assert_eq!(projection.legend[0].agent_name.as_deref(), Some("codex"));
        assert_eq!(projection.legend[1].agent_name, None);
        assert_eq!(projection.legend[2].task_id, None);
    }

    #[test]
    fn safe_file_paths_are_repo_relative_and_sensitive_or_outside_paths_fall_back() {
        let root = std::env::temp_dir().join(format!(
            "taarof-ledger-paths-{}-{}",
            std::process::id(),
            crate::events::unix_time_ms()
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        fs::write(root.join(".env"), "SECRET=x").unwrap();
        let outside = root.parent().unwrap().join("outside-ledger.txt");
        fs::write(&outside, "").unwrap();
        assert_eq!(
            safe_repo_relative_path(&root, "src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert!(safe_repo_relative_path(&root, ".env").is_none());
        assert!(safe_repo_relative_path(&root, outside.to_string_lossy().as_ref()).is_none());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_file(outside);
    }

    #[test]
    fn control_character_paths_use_generic_records_events_query_and_sidebar_summaries() {
        let root = temp_path("control-paths").with_extension("dir");
        fs::create_dir_all(&root).unwrap();
        let paths = ["line\nbreak.rs", "tab\tname.rs", "escape\u{1b}name.rs"];
        for path in paths {
            fs::write(root.join(path), "").unwrap();
            assert!(safe_repo_relative_path(&root, path).is_none());
        }

        let mut state = crate::AppState::new();
        let identity = identity();
        for (index, path) in paths.into_iter().enumerate() {
            state.project_file_operation(
                &identity,
                "control-session",
                Some(&root),
                path,
                "edit",
                index as u64 + 1,
            );
        }
        let records = state.work_ledger.records();
        assert_eq!(records.len(), 3);
        assert!(records.iter().all(|record| {
            record.summary == "Agent edited a file" && !record.summary.chars().any(char::is_control)
        }));
        let query = crate::api::build_state_snapshot(&state).to_string();
        assert!(!query.chars().any(|ch| matches!(ch, '\n' | '\t' | '\u{1b}')));
        let event_payloads = state
            .event_store
            .entries()
            .into_iter()
            .filter(|event| event.event_type == "work_recorded")
            .map(|event| event.payload.to_string())
            .collect::<Vec<_>>();
        assert_eq!(event_payloads.len(), 3);
        assert!(event_payloads.iter().all(|payload| !payload
            .chars()
            .any(|ch| matches!(ch, '\n' | '\t' | '\u{1b}'))));
        let sidebar = state.work_ledger.sidebar_records(&identity.pane_origin);
        assert_eq!(sidebar.len(), 3);
        assert!(sidebar
            .iter()
            .all(|record| record.summary == "Agent edited a file"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stable_identity_survives_rename_move_and_eager_or_lazy_runtime_id_changes() {
        let mut state = crate::AppState::new();
        let source_workspace = state.active_workspace;
        let destination_workspace = state.create_workspace("destination", None);
        state.active_workspace = source_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            source_workspace,
            "source-tab",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let before = identity_for_pane(&state, tab_id, pane_id, None).unwrap();
        let destination_origin = state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == destination_workspace)
            .unwrap()
            .work_origin
            .clone();
        state
            .rename_workspace(source_workspace, "renamed-source")
            .unwrap();
        state.find_tab_mut(tab_id).unwrap().name = "renamed-tab".into();
        let renamed = identity_for_pane(&state, tab_id, pane_id, None).unwrap();
        assert_eq!(renamed.workspace_origin, before.workspace_origin);
        assert_eq!(renamed.tab_origin, before.tab_origin);
        assert_eq!(renamed.pane_origin, before.pane_origin);

        state
            .move_tab_to_workspace(tab_id, destination_workspace)
            .unwrap();
        let moved = identity_for_pane(&state, tab_id, pane_id, None).unwrap();
        assert_eq!(moved.workspace_origin, destination_origin);
        assert_ne!(moved.workspace_origin, before.workspace_origin);
        assert_eq!(moved.tab_origin, before.tab_origin);
        assert_eq!(moved.pane_origin, before.pane_origin);
        assert_eq!(moved.workspace_id, destination_workspace);
        assert_eq!(
            runtime_target_for_identity(&state, &before),
            Some((tab_id, pane_id)),
            "pre-move records continue to focus the uniquely moved tab origin"
        );

        let pane_token = state
            .headless_panes
            .get(&(tab_id, pane_id))
            .unwrap()
            .work_origin
            .clone();
        let mut eager = crate::AppState::new();
        let filler_workspace = eager.active_workspace;
        crate::seed_headless_terminal_tab(
            &mut eager,
            filler_workspace,
            "filler",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let eager_workspace = eager.create_workspace("restored", None);
        eager.apply_restored_workspace_work_origin(eager_workspace, Some(&moved.workspace_origin));
        let (eager_tab, eager_pane) = crate::seed_headless_terminal_tab(
            &mut eager,
            eager_workspace,
            "restored-tab",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        eager.apply_restored_tab_work_origin(eager_tab, Some(&moved.tab_origin));
        eager
            .headless_panes
            .get_mut(&(eager_tab, eager_pane))
            .unwrap()
            .work_origin = pane_token.clone();
        let eager_identity = identity_for_pane(&eager, eager_tab, eager_pane, None).unwrap();
        assert_eq!(eager_identity.workspace_origin, moved.workspace_origin);
        assert_eq!(eager_identity.tab_origin, moved.tab_origin);
        assert_eq!(eager_identity.pane_origin, moved.pane_origin);
        assert_ne!(eager_identity.tab_id, moved.tab_id);
        assert_eq!(
            runtime_target_for_identity(&eager, &moved),
            Some((eager_tab, eager_pane))
        );

        let mut lazy = crate::AppState::new();
        let filler_workspace = lazy.active_workspace;
        crate::seed_headless_terminal_tab(
            &mut lazy,
            filler_workspace,
            "filler",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let lazy_workspace = lazy.create_workspace("lazy-restored", None);
        lazy.apply_restored_workspace_work_origin(lazy_workspace, Some(&moved.workspace_origin));
        let lazy_tab = crate::seed_pending_restore_tab(
            &mut lazy,
            lazy_workspace,
            "lazy-tab",
            crate::session::SavedPaneNode::Leaf {
                work_origin: Some(pane_token),
                cwd: Some("/tmp".into()),
                ssh_command: None,
                tmux_session: None,
                tmux_host: None,
                current_task: None,
                agent_session: None,
            },
            Some("/tmp".into()),
        )
        .unwrap();
        lazy.apply_restored_tab_work_origin(lazy_tab, Some(&moved.tab_origin));
        let lazy_identity = identity_for_pane(&lazy, lazy_tab, 0, None).unwrap();
        assert_eq!(lazy_identity.workspace_origin, moved.workspace_origin);
        assert_eq!(lazy_identity.tab_origin, moved.tab_origin);
        assert_eq!(lazy_identity.pane_origin, moved.pane_origin);
        assert_ne!(lazy_identity.tab_id, moved.tab_id);
        assert_eq!(
            runtime_target_for_identity(&lazy, &moved),
            Some((lazy_tab, 0))
        );
    }

    #[test]
    fn pane_origin_is_unique_rename_stable_restorable_and_available_for_lazy_panes() {
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let seed = crate::HeadlessPaneSeed {
            cwd: Some("/same/cwd".into()),
            ..Default::default()
        };
        let (first_tab, first_pane) =
            crate::seed_headless_terminal_tab(&mut state, workspace, "same", seed.clone()).unwrap();
        let (second_tab, second_pane) =
            crate::seed_headless_terminal_tab(&mut state, workspace, "same", seed).unwrap();
        let first_origin = origin_for_pane(&state, first_tab, first_pane).unwrap();
        let second_origin = origin_for_pane(&state, second_tab, second_pane).unwrap();
        assert_ne!(
            first_origin, second_origin,
            "same-name/CWD tabs stay distinct"
        );

        state.find_tab_mut(first_tab).unwrap().name = "renamed".into();
        assert_eq!(
            origin_for_pane(&state, first_tab, first_pane).as_deref(),
            Some(first_origin.as_str()),
            "rename does not orphan ledger history"
        );

        let persisted = state.find_tab(first_tab).unwrap().1.work_origin.clone();
        let persisted_pane = state
            .headless_panes
            .get(&(first_tab, first_pane))
            .unwrap()
            .work_origin
            .clone();
        let mut restored = crate::AppState::new();
        let restored_workspace = restored.active_workspace;
        let (restored_tab, restored_pane) = crate::seed_headless_terminal_tab(
            &mut restored,
            restored_workspace,
            "restored",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        restored.apply_restored_tab_work_origin(restored_tab, Some(&persisted));
        restored
            .headless_panes
            .get_mut(&(restored_tab, restored_pane))
            .unwrap()
            .work_origin = persisted_pane;
        assert_eq!(
            origin_for_pane(&restored, restored_tab, restored_pane).as_deref(),
            Some(first_origin.as_str()),
            "persisted token reconnects restored history"
        );

        let lazy_tab = crate::seed_pending_restore_tab(
            &mut restored,
            restored_workspace,
            "lazy",
            crate::session::SavedPaneNode::Leaf {
                work_origin: Some("pane-lazy-test".into()),
                cwd: Some("/same/cwd".into()),
                ssh_command: None,
                tmux_session: None,
                tmux_host: None,
                current_task: None,
                agent_session: None,
            },
            Some("/same/cwd".into()),
        )
        .unwrap();
        let lazy_origin = origin_for_pane(&restored, lazy_tab, 0).unwrap();
        restored.find_tab_mut(lazy_tab).unwrap().name = "lazy-renamed".into();
        assert_eq!(
            origin_for_pane(&restored, lazy_tab, 0).unwrap(),
            lazy_origin
        );
    }

    #[test]
    fn runtime_focus_allows_workspace_moves_but_rejects_cross_session_or_ambiguous_identity() {
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "focus",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let valid = identity_for_pane(&state, tab_id, pane_id, None).unwrap();
        assert_eq!(
            runtime_target_for_identity(&state, &valid),
            Some((tab_id, pane_id))
        );

        let mut cross_session = valid.clone();
        cross_session.session = "another-session".into();
        assert_eq!(runtime_target_for_identity(&state, &cross_session), None);

        let mut cross_workspace = valid.clone();
        cross_workspace.workspace_origin = "workspace-another".into();
        assert_eq!(
            runtime_target_for_identity(&state, &cross_workspace),
            Some((tab_id, pane_id)),
            "workspace origin is historical metadata because tabs may move"
        );

        let (duplicate_tab, duplicate_pane) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "duplicate",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let original_tab_origin = state.find_tab(tab_id).unwrap().1.work_origin.clone();
        let original_pane_token = state
            .headless_panes
            .get(&(tab_id, pane_id))
            .unwrap()
            .work_origin
            .clone();
        state.find_tab_mut(duplicate_tab).unwrap().work_origin = original_tab_origin;
        state
            .headless_panes
            .get_mut(&(duplicate_tab, duplicate_pane))
            .unwrap()
            .work_origin = original_pane_token;
        assert_eq!(
            runtime_target_for_identity(&state, &cross_workspace),
            None,
            "duplicate stable origins fail closed instead of choosing a pane"
        );
    }

    #[test]
    fn split_close_save_restore_keeps_surviving_pane_history_when_ordinal_changes() {
        let original_tree = crate::session::SavedPaneNode::Split {
            direction: "vertical".into(),
            ratio: 0.5,
            first: Box::new(crate::session::SavedPaneNode::Leaf {
                work_origin: Some("pane-closed-earlier".into()),
                cwd: Some("/repo".into()),
                ssh_command: None,
                tmux_session: None,
                tmux_host: None,
                current_task: None,
                agent_session: None,
            }),
            second: Box::new(crate::session::SavedPaneNode::Leaf {
                work_origin: Some("pane-survivor-stable".into()),
                cwd: Some("/repo".into()),
                ssh_command: None,
                tmux_session: None,
                tmux_host: None,
                current_task: None,
                agent_session: None,
            }),
        };
        let mut before = crate::AppState::new();
        let before_workspace = before.active_workspace;
        let before_tab = crate::seed_pending_restore_tab(
            &mut before,
            before_workspace,
            "split",
            original_tree.clone(),
            Some("/repo".into()),
        )
        .unwrap();
        before.apply_restored_tab_work_origin(before_tab, Some("tab-sparse-restore"));
        let survivor_before = origin_for_pane(&before, before_tab, 1).unwrap();

        // Close the earlier leaf, persist the surviving subtree, and restore it.
        // Its transient DFS pane id changes from 1 to 0, while its token remains.
        let crate::session::SavedPaneNode::Split { second, .. } = original_tree else {
            unreachable!()
        };
        let serialized = serde_json::to_string(second.as_ref()).unwrap();
        let restored_tree: crate::session::SavedPaneNode =
            serde_json::from_str(&serialized).unwrap();
        let mut after = crate::AppState::new();
        let after_workspace = after.active_workspace;
        let after_tab = crate::seed_pending_restore_tab(
            &mut after,
            after_workspace,
            "restored",
            restored_tree,
            Some("/repo".into()),
        )
        .unwrap();
        after.apply_restored_tab_work_origin(after_tab, Some("tab-sparse-restore"));
        assert_eq!(
            origin_for_pane(&after, after_tab, 0).as_deref(),
            Some(survivor_before.as_str())
        );
    }

    #[test]
    fn duplicate_or_invalid_restored_origins_regenerate_instead_of_colliding() {
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (first, _) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "one",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let (second, _) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "two",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let claimed = state.apply_restored_tab_work_origin(first, Some("tab-persisted-origin"));
        let second_generated = state.find_tab(second).unwrap().1.work_origin.clone();
        assert_eq!(
            state.apply_restored_tab_work_origin(second, Some(&claimed)),
            second_generated
        );
        assert_eq!(
            state.apply_restored_tab_work_origin(second, Some("bad origin with spaces")),
            second_generated
        );
    }

    #[test]
    fn query_state_and_local_events_expose_safe_ledger_records() {
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "event-projection",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let live_identity = identity_for_pane(&state, tab_id, pane_id, None).unwrap();
        state.append_work_record(WorkDraft {
            kind: WorkKind::AssistantMessageCompleted,
            summary: "Assistant message completed".into(),
            identity: live_identity.clone(),
            evidence_source: EvidenceSource::AgentTranscript,
            authority: WorkAuthority::AgentObservation,
            verification: VerificationState::Observed,
            dedupe_scope: "api".into(),
            fingerprint: "api".into(),
        });
        let snapshot = crate::api::build_state_snapshot(&state);
        assert_eq!(snapshot["work_ledger"]["schema"], "taarof.work-ledger.v3");
        assert_eq!(snapshot["work_ledger"]["records"][0]["seq"], 1);
        let event = state
            .event_store
            .entries()
            .into_iter()
            .find(|event| event.event_type == "work_recorded")
            .expect("work event");
        let json = event.payload.to_string();
        assert!(json.contains("assistant_message_completed"));
        assert_eq!(event.payload["work_seq"], 1);
        assert_eq!(event.payload["marker"], "P1");
        assert_eq!(event.payload["color_slot"], 0);
        assert_eq!(event.payload["reconciliation"]["status"], "verified");
        assert_eq!(event.payload["current_target"]["workspace_id"], workspace);
        assert_eq!(event.payload["current_target"]["tab_id"], tab_id);
        assert_eq!(event.payload["current_target"]["pane_id"], pane_id);
        assert_eq!(
            event.payload["current_target"]["pane_origin"],
            live_identity.pane_origin
        );
        assert!(!json.contains("last_message"));
        assert!(!json.contains("tool_payload"));
    }

    #[test]
    fn final_record_boundary_sanitizes_all_display_values_and_drops_unsafe_origins() {
        let hostile = "RAW_SECRET_SENTINEL";
        let mut state = crate::AppState::new();
        let mut hostile_identity = identity();
        hostile_identity.workspace_name = format!("{hostile}\nworkspace");
        hostile_identity.tab_name = format!("{hostile}\ttab");
        hostile_identity.task_id = Some(format!("{hostile}\u{1b}-ID"));
        hostile_identity.task_title = Some(format!("{hostile}\rtitle"));
        state.append_work_record(WorkDraft {
            kind: WorkKind::TaskCreated,
            summary: format!("Task {hostile}\n was added"),
            identity: hostile_identity,
            evidence_source: EvidenceSource::PlanFile,
            authority: WorkAuthority::PlanCanonical,
            verification: VerificationState::CanonicalFile,
            dedupe_scope: "hostile-display".into(),
            fingerprint: "one".into(),
        });

        let mut oversized_identity = identity();
        oversized_identity.workspace_name = "w".repeat(MAX_NAME_LEN + 1);
        oversized_identity.tab_name = "t".repeat(MAX_NAME_LEN + 1);
        oversized_identity.task_id = Some("i".repeat(MAX_TASK_ID_LEN + 1));
        oversized_identity.task_title = Some("q".repeat(MAX_TASK_TITLE_LEN + 1));
        state.append_work_record(WorkDraft {
            kind: WorkKind::PaneBound,
            summary: "s".repeat(MAX_SUMMARY_LEN + 1),
            identity: oversized_identity,
            evidence_source: EvidenceSource::PaneBinding,
            authority: WorkAuthority::TaarofSession,
            verification: VerificationState::Observed,
            dedupe_scope: "oversized-display".into(),
            fingerprint: "two".into(),
        });

        let mut unsafe_origin = identity();
        unsafe_origin.tab_origin = "tab-unsafe\norigin".into();
        assert!(state
            .append_work_record(WorkDraft {
                kind: WorkKind::AssistantMessageCompleted,
                summary: "Assistant message completed".into(),
                identity: unsafe_origin,
                evidence_source: EvidenceSource::AgentTranscript,
                authority: WorkAuthority::AgentObservation,
                verification: VerificationState::Observed,
                dedupe_scope: "unsafe-origin".into(),
                fingerprint: "three".into(),
            })
            .is_none());

        let records = state.work_ledger.records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].summary, "A task was added to .plan");
        assert_eq!(records[1].summary, "Pane task binding changed");
        for record in &records {
            assert_eq!(record.identity.workspace_name, "Workspace");
            assert_eq!(record.identity.tab_name, "Tab");
            assert!(record.identity.task_id.is_none());
            assert!(record.identity.task_title.is_none());
            assert!(!record.summary.chars().any(char::is_control));
            assert!(record.summary.len() <= MAX_SUMMARY_LEN);
        }

        let events = state
            .event_store
            .entries()
            .into_iter()
            .filter(|event| event.event_type == "work_recorded")
            .map(|event| event.payload)
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0]["record"]["identity"]["workspace_name"],
            "Workspace"
        );
        assert_eq!(events[0]["record"]["identity"]["tab_name"], "Tab");
        assert!(events[0]["record"]["identity"]["task_id"].is_null());
        assert_eq!(events[0]["record"]["summary"], "A task was added to .plan");

        let query = crate::api::build_state_snapshot(&state);
        let query_records = query["work_ledger"]["records"].as_array().unwrap();
        assert_eq!(query_records.len(), 2);
        assert_eq!(query_records[1]["identity"]["workspace_name"], "Workspace");
        assert_eq!(query_records[1]["identity"]["tab_name"], "Tab");
        assert_eq!(query_records[1]["summary"], "Pane task binding changed");

        let sidebar = state.work_ledger.sidebar_records("pane-test");
        assert_eq!(sidebar, records);
        for surface in [
            serde_json::to_string(&records).unwrap(),
            serde_json::to_string(&events).unwrap(),
            query.to_string(),
            serde_json::to_string(&sidebar).unwrap(),
        ] {
            assert!(!surface.contains(hostile));
        }
    }

    #[test]
    fn projection_preserves_multiple_repeated_deltas_dedupes_ordinals_and_scrubs_payloads() {
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "projection",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let hostile = "HOSTILE_TRANSCRIPT_TOOL_SECRET_SENTINEL";
        let payload = serde_json::json!({
            "tab_id": tab_id,
            "pane_id": pane_id,
            "session_id": "native-session",
            "message_count": 3,
            "new_messages": 3,
            "last_message": hostile,
            "tool_payload": hostile,
            "recent_files": [
                {"path": format!("/outside/{hostile}"), "op": "edit", "at_unix_ms": 1, "ordinal": 1},
                {"path": format!("/outside/{hostile}"), "op": "edit", "at_unix_ms": 2, "ordinal": 2},
                {"path": format!("/outside/{hostile}"), "op": "write", "at_unix_ms": 3, "ordinal": 3}
            ]
        });
        state.project_agent_message(&payload);
        assert_eq!(
            state
                .work_ledger
                .records()
                .iter()
                .filter(|record| record.kind == WorkKind::AssistantMessageCompleted)
                .count(),
            3
        );
        assert_eq!(
            state
                .work_ledger
                .records()
                .iter()
                .filter(|record| record.kind == WorkKind::FileTouched)
                .count(),
            3,
            "repeated same-file operations remain distinct"
        );
        let before_replay = state.work_ledger.records().len();
        state.project_agent_message(&payload);
        assert_eq!(state.work_ledger.records().len(), before_replay);

        let records_json = serde_json::to_string(&state.work_ledger.records()).unwrap();
        let events_json = serde_json::to_string(&state.event_store.entries()).unwrap();
        let snapshot_json = crate::api::build_state_snapshot(&state).to_string();
        for safe_output in [&records_json, &events_json, &snapshot_json] {
            assert!(!safe_output.contains(hostile));
            assert!(!safe_output.contains("tool_payload"));
            assert!(!safe_output.contains("last_message"));
        }
    }

    #[test]
    fn present_empty_ordered_stream_is_authoritative_over_legacy_aggregates() {
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "ordered-empty",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        state.project_agent_message(&serde_json::json!({
            "tab_id": tab_id,
            "pane_id": pane_id,
            "session_id": "ordered",
            "message_count": 10,
            "new_messages": 10,
            "recent_files": [{"path": "/legacy", "op": "edit", "ordinal": 1}],
            "work_events": []
        }));
        assert!(state.work_ledger.records().is_empty());
    }

    #[test]
    fn ordered_projection_keeps_file_before_final_message() {
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "ordered-file-message",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        state.project_agent_message(&serde_json::json!({
            "tab_id": tab_id,
            "pane_id": pane_id,
            "session_id": "ordered-file-message",
            "work_events": [
                {"kind": "file_operation", "path": "/outside/a.rs", "op": "edit", "ordinal": 1},
                {"kind": "assistant_completed", "ordinal": 1}
            ]
        }));
        assert_eq!(
            state
                .work_ledger
                .records()
                .iter()
                .map(|record| record.kind)
                .collect::<Vec<_>>(),
            vec![WorkKind::FileTouched, WorkKind::AssistantMessageCompleted]
        );
    }

    #[test]
    fn ordered_mixed_stream_retains_chronological_suffix_without_regrouping() {
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "ordered-capacity",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let mut message_ordinal = 0_u64;
        let mut file_ordinal = 0_u64;
        let mut events = Vec::new();
        let mut expected = Vec::new();
        for index in 0..(DEFAULT_CAPACITY + 2) {
            if index % 2 == 0 {
                message_ordinal += 1;
                events.push(serde_json::json!({
                    "kind": "assistant_completed",
                    "ordinal": message_ordinal,
                }));
                if index >= 2 {
                    expected.push(WorkKind::AssistantMessageCompleted);
                }
            } else {
                file_ordinal += 1;
                events.push(serde_json::json!({
                    "kind": "file_operation",
                    "path": "/outside/repeated.rs",
                    "op": "edit",
                    "ordinal": file_ordinal,
                }));
                if index >= 2 {
                    expected.push(WorkKind::FileTouched);
                }
            }
        }
        state.project_agent_message(&serde_json::json!({
            "tab_id": tab_id,
            "pane_id": pane_id,
            "session_id": "ordered-capacity",
            "work_events": events,
        }));
        let records = state.work_ledger.records();
        assert_eq!(records.len(), DEFAULT_CAPACITY);
        assert_eq!(
            records.iter().map(|record| record.kind).collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn impossible_oversized_message_delta_is_bounded_by_ledger_retention() {
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "oversized",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        state.project_agent_message(&serde_json::json!({
            "tab_id": tab_id,
            "pane_id": pane_id,
            "session_id": "malformed",
            "message_count": u64::MAX,
            "new_messages": u64::MAX,
            "recent_files": []
        }));
        assert_eq!(
            state.work_ledger.records().len(),
            DEFAULT_CAPACITY,
            "projection loops at most the ledger's own retained capacity"
        );
    }

    #[test]
    fn shared_plan_status_delta_routes_to_the_matching_bound_pane() {
        fn pane(origin: &str, pane_id: u32, task_id: &str, status: &str) -> PaneProbe {
            PaneProbe {
                identity: WorkIdentity {
                    pane_origin: origin.to_string(),
                    pane_id,
                    task_id: Some(task_id.to_string()),
                    task_title: Some(task_id.to_string()),
                    ..identity()
                },
                activity: None,
                local_cwd: Some("/repo".to_string()),
                binding_checkout_root: Some("/repo".to_string()),
                binding_identity: None,
                plan_root: Some("/repo".to_string()),
                plan_tasks: vec![
                    ObservedPlanTask {
                        id: "A".into(),
                        title: "A".into(),
                        status: "todo".into(),
                        fully_specified: true,
                    },
                    ObservedPlanTask {
                        id: "B".into(),
                        title: "B".into(),
                        status: status.into(),
                        fully_specified: true,
                    },
                ],
                plan_loaded: true,
            }
        }
        let mut state = crate::AppState::new();
        state.observe_work_probe_unchecked(WorkProbe {
            panes: vec![
                pane("pane-a", 1, "A", "todo"),
                pane("pane-b", 2, "B", "todo"),
            ],
            exact_pull_requests: Vec::new(),
        });
        state.observe_work_probe_unchecked(WorkProbe {
            panes: vec![
                pane("pane-a", 1, "A", "done"),
                pane("pane-b", 2, "B", "done"),
            ],
            exact_pull_requests: Vec::new(),
        });
        let status_records: Vec<_> = state
            .work_ledger
            .records()
            .into_iter()
            .filter(|record| record.kind == WorkKind::TaskStatusChanged)
            .collect();
        assert_eq!(status_records.len(), 1);
        assert_eq!(status_records[0].identity.task_id.as_deref(), Some("B"));
        assert_eq!(status_records[0].identity.pane_origin, "pane-b");
    }

    #[test]
    fn same_task_id_in_other_checkout_never_routes_status_to_bound_pane() {
        fn probe(binding_root: &str, observed_root: &str, status: &str) -> WorkProbe {
            WorkProbe {
                panes: vec![PaneProbe {
                    identity: WorkIdentity {
                        pane_origin: "pane-bound-a".into(),
                        task_id: Some("SAME-1".into()),
                        task_title: Some("Same task".into()),
                        ..identity()
                    },
                    activity: None,
                    local_cwd: Some(observed_root.into()),
                    binding_checkout_root: Some(binding_root.into()),
                    binding_identity: None,
                    plan_root: Some(observed_root.into()),
                    plan_tasks: vec![ObservedPlanTask {
                        id: "SAME-1".into(),
                        title: "Same task".into(),
                        status: status.into(),
                        fully_specified: true,
                    }],
                    plan_loaded: true,
                }],
                exact_pull_requests: Vec::new(),
            }
        }

        let root = temp_path("two-checkout-routing").with_extension("dir");
        let checkout_a = root.join("a");
        let checkout_b = root.join("b");
        fs::create_dir_all(&checkout_a).unwrap();
        fs::create_dir_all(&checkout_b).unwrap();
        let a = canonical_directory(&checkout_a).unwrap();
        let b = canonical_directory(&checkout_b).unwrap();
        let mut state = crate::AppState::new();

        state.observe_work_probe_unchecked(probe(&a, &a, "todo"));
        state.observe_work_probe_unchecked(probe(&a, &b, "todo"));
        state.observe_work_probe_unchecked(probe(&a, &b, "done"));
        assert!(state
            .work_ledger
            .records()
            .iter()
            .all(|record| record.kind != WorkKind::TaskStatusChanged));

        state.observe_work_probe_unchecked(probe(&a, &a, "done"));
        let status_records = state
            .work_ledger
            .records()
            .into_iter()
            .filter(|record| record.kind == WorkKind::TaskStatusChanged)
            .collect::<Vec<_>>();
        assert_eq!(status_records.len(), 1);
        assert_eq!(status_records[0].identity.pane_origin, "pane-bound-a");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn off_thread_probe_is_discarded_after_binding_token_changes() {
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "probe-rebind",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let binding = |task: &str, token: &str| crate::task_binding::PaneTaskBinding {
            task_id: task.into(),
            title: format!("Task {task}"),
            checkout_root: Some("/repo".into()),
            reporting_token: token.into(),
        };
        state
            .headless_pane_mut(tab_id, pane_id)
            .unwrap()
            .current_task = Some(binding("TASK-A", "ctx-11111111111111111111111111111111"));

        let observed = |state: &crate::AppState, status: &str| {
            let mut probe = collect_probe(state);
            let pane = probe
                .panes
                .iter_mut()
                .find(|pane| pane.identity.tab_id == tab_id && pane.identity.pane_id == pane_id)
                .unwrap();
            pane.plan_root = Some("/repo".into());
            pane.plan_loaded = true;
            pane.plan_tasks = vec![ObservedPlanTask {
                id: "TASK-A".into(),
                title: "Task TASK-A".into(),
                status: status.into(),
                fully_specified: true,
            }];
            probe
        };
        state.observe_work_probe(observed(&state, "todo"));
        let stale = observed(&state, "done");

        // Simulate a rebind while filesystem/GitHub enrichment is in flight.
        state
            .headless_pane_mut(tab_id, pane_id)
            .unwrap()
            .current_task = Some(binding("TASK-B", "ctx-22222222222222222222222222222222"));
        state.observe_work_probe(stale);
        assert!(state
            .work_ledger
            .records()
            .iter()
            .all(|record| record.kind != WorkKind::TaskStatusChanged));
    }

    #[test]
    fn github_pr_transitions_are_ordered_deduped_and_terminal_discovery_is_honest() {
        fn pr() -> crate::tracking::BranchPullRequestEntry {
            crate::tracking::BranchPullRequestEntry {
                number: 321,
                title: "Correlate work".into(),
                state: "open".into(),
                is_draft: true,
                review_decision: None,
                url: Some("https://github.com/owner/repo/pull/321".into()),
                head_repository_owner: "owner".into(),
                head_ref_name: "agent/work".into(),
                base_ref_name: "main".into(),
                updated_at: None,
                checks: crate::tracking::PullRequestChecks::Pending,
                correlation: None,
                correlation_marker_present: false,
            }
        }
        let mut ledger = WorkLedger::empty("test".into(), 20, u64::MAX, None);
        let mut current = pr();
        assert_eq!(
            ledger.observe_pull_request("pane:owner/repo:321:TASK", &current),
            vec![(WorkKind::PullRequestOpened, "PR #321 opened".into())]
        );
        assert!(ledger
            .observe_pull_request("pane:owner/repo:321:TASK", &current)
            .is_empty());
        current.is_draft = false;
        current.review_decision = Some("APPROVED".into());
        current.checks = crate::tracking::PullRequestChecks::Ready;
        let changes = ledger.observe_pull_request("pane:owner/repo:321:TASK", &current);
        assert_eq!(
            changes.iter().map(|change| change.0).collect::<Vec<_>>(),
            vec![
                WorkKind::PullRequestDraftChanged,
                WorkKind::PullRequestReviewChanged,
                WorkKind::PullRequestChecksReady,
            ]
        );
        current.state = "merged".into();
        assert_eq!(
            ledger.observe_pull_request("pane:owner/repo:321:TASK", &current)[0].0,
            WorkKind::PullRequestMerged
        );

        let mut already_merged = pr();
        already_merged.state = "merged".into();
        let first_terminal =
            ledger.observe_pull_request("other:owner/repo:321:TASK", &already_merged);
        assert_eq!(first_terminal.len(), 1);
        assert_eq!(first_terminal[0].0, WorkKind::PullRequestMerged);
        assert!(!first_terminal
            .iter()
            .any(|change| change.0 == WorkKind::PullRequestOpened));
    }

    #[test]
    fn verified_pr_observer_rechecks_live_pane_binding_token() {
        let root = temp_path("verified-pr-binding").with_extension("dir");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join(".plan")).unwrap();
        fs::write(
            root.join(".plan/tasks.json"),
            r#"{"tasks":[{"id":"TASK-1","title":"Bound task","status":"todo"}]}"#,
        )
        .unwrap();
        let root = root.canonicalize().unwrap();
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "bound",
            crate::HeadlessPaneSeed {
                cwd: Some(root.to_string_lossy().into_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let stale_binding = state.bind_pane_to_task(tab_id, pane_id, "TASK-1").unwrap();
        let stale_identity =
            identity_for_pane(&state, tab_id, pane_id, Some(&stale_binding)).unwrap();
        let stale = VerifiedPullRequestBinding {
            identity: stale_identity,
            task_id: stale_binding.task_id.clone(),
            title: stale_binding.title.clone(),
            checkout_root: stale_binding.checkout_root.as_deref().unwrap().into(),
            reporting_token: stale_binding.reporting_token.clone(),
            base_repository: "owner/repo".into(),
            head_owner: "owner".into(),
            branch: "agent/work".into(),
        };
        let live_binding = state.bind_pane_to_task(tab_id, pane_id, "TASK-1").unwrap();
        assert_ne!(stale.reporting_token, live_binding.reporting_token);
        let data = crate::tracking::BranchPullRequestsData {
            total: 1,
            open: 1,
            draft: 0,
            merged: 0,
            closed: 0,
            pull_requests: vec![crate::tracking::BranchPullRequestEntry {
                number: 321,
                title: "Correlation".into(),
                state: "open".into(),
                is_draft: false,
                review_decision: None,
                url: Some("https://github.com/owner/repo/pull/321".into()),
                head_ref_name: "agent/work".into(),
                head_repository_owner: "owner".into(),
                base_ref_name: "main".into(),
                updated_at: None,
                checks: crate::tracking::PullRequestChecks::Pending,
                correlation: Some(crate::tracking::PullRequestCorrelationMetadata {
                    task_id: "TASK-1".into(),
                    repo: "owner/repo".into(),
                    branch: "agent/work".into(),
                    dispatch_source: "agent".into(),
                }),
                correlation_marker_present: true,
            }],
            query_complete: true,
        };
        state.observe_verified_pull_requests(&stale, &data);
        assert!(!state
            .work_ledger
            .records()
            .iter()
            .any(|record| is_pull_request_kind(record.kind)));

        let live = VerifiedPullRequestBinding {
            identity: identity_for_pane(&state, tab_id, pane_id, Some(&live_binding)).unwrap(),
            reporting_token: live_binding.reporting_token.clone(),
            ..stale
        };
        state.observe_verified_pull_requests(&live, &data);
        let pr_records = state
            .work_ledger
            .records()
            .into_iter()
            .filter(|record| is_pull_request_kind(record.kind))
            .collect::<Vec<_>>();
        assert_eq!(pr_records.len(), 1);
        assert_eq!(pr_records[0].kind, WorkKind::PullRequestOpened);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn github_pr_records_have_typed_remote_authority_without_body_payload() {
        let mut ledger = WorkLedger::empty("test".into(), 10, u64::MAX, None);
        let record = ledger
            .append_pull_request(
                WorkDraft {
                    kind: WorkKind::PullRequestOpened,
                    summary: "PR #321 opened".into(),
                    identity: identity(),
                    evidence_source: EvidenceSource::GitHubPullRequest,
                    authority: WorkAuthority::GitHubCanonical,
                    verification: VerificationState::RemoteVerified,
                    dedupe_scope: "pr:321:opened".into(),
                    fingerprint: "open".into(),
                },
                WorkPullRequestRef {
                    repository: "owner/repo".into(),
                    number: 321,
                    title: "Correlation".into(),
                    url: "https://github.com/owner/repo/pull/321".into(),
                    state: "open".into(),
                    is_draft: false,
                    review_decision: None,
                    checks: crate::tracking::PullRequestChecks::Pending,
                },
            )
            .unwrap();
        assert_eq!(record.authority, WorkAuthority::GitHubCanonical);
        assert_eq!(record.verification, VerificationState::RemoteVerified);
        assert_eq!(record.pull_request.unwrap().number, 321);
        let json = serde_json::to_string(&ledger.records()).unwrap();
        assert!(!json.contains("taarof-work"));
        assert!(!json.contains("dispatch_source"));
    }

    #[test]
    fn persisted_pr_validation_requires_typed_exact_and_consistent_identity() {
        fn reference() -> WorkPullRequestRef {
            WorkPullRequestRef {
                repository: "owner/repo".into(),
                number: 321,
                title: "Correlation".into(),
                url: "https://github.com/owner/repo/pull/321".into(),
                state: "open".into(),
                is_draft: false,
                review_decision: None,
                checks: crate::tracking::PullRequestChecks::Pending,
            }
        }
        fn record(pull_request: Option<WorkPullRequestRef>) -> WorkRecord {
            WorkRecord {
                seq: 1,
                ts_unix_ms: crate::events::unix_time_ms(),
                kind: WorkKind::PullRequestOpened,
                summary: "PR #321 opened".into(),
                identity: identity(),
                evidence_source: EvidenceSource::GitHubPullRequest,
                authority: WorkAuthority::GitHubCanonical,
                verification: VerificationState::RemoteVerified,
                task_status: None,
                pull_request,
            }
        }

        assert!(sanitize_work_record(record(Some(reference()))).is_some());
        assert!(sanitize_work_record(record(None)).is_none());

        let mut unsafe_reference = reference();
        unsafe_reference.url = "https://github.com/owner/repo/pull/321/files".into();
        assert!(sanitize_work_record(record(Some(unsafe_reference))).is_none());
        let mut unsafe_reference = reference();
        unsafe_reference.number = 322;
        assert!(sanitize_work_record(record(Some(unsafe_reference))).is_none());
        let mut unsafe_reference = reference();
        unsafe_reference.repository = "Owner/Repo".into();
        assert!(sanitize_work_record(record(Some(unsafe_reference))).is_none());
        let mut unsafe_reference = reference();
        unsafe_reference.title = "unsafe\ntitle".into();
        assert!(sanitize_work_record(record(Some(unsafe_reference))).is_none());

        let mut non_pr = record(Some(reference()));
        non_pr.kind = WorkKind::AssistantMessageCompleted;
        non_pr.evidence_source = EvidenceSource::AgentTranscript;
        non_pr.authority = WorkAuthority::AgentObservation;
        non_pr.verification = VerificationState::Observed;
        assert!(sanitize_work_record(non_pr).is_none());
    }

    #[test]
    fn unsafe_persisted_pr_record_is_dropped_and_storage_is_rewritten() {
        let path = temp_path("unsafe-pr-rewrite");
        let sentinel = "UNSAFE_PR_RECORD_SENTINEL";
        let unsafe_record = WorkRecord {
            seq: 1,
            ts_unix_ms: crate::events::unix_time_ms(),
            kind: WorkKind::PullRequestOpened,
            summary: "PR #321 opened".into(),
            identity: identity(),
            evidence_source: EvidenceSource::GitHubPullRequest,
            authority: WorkAuthority::GitHubCanonical,
            verification: VerificationState::RemoteVerified,
            task_status: None,
            pull_request: Some(WorkPullRequestRef {
                repository: "owner/repo".into(),
                number: 321,
                title: sentinel.into(),
                url: "https://github.com/owner/repo/pull/321?unsafe=1".into(),
                state: "open".into(),
                is_draft: false,
                review_decision: None,
                checks: crate::tracking::PullRequestChecks::Pending,
            }),
        };
        fs::write(
            &path,
            serde_json::to_string(&PersistedLedger {
                schema: "taarof.work-ledger.v1".into(),
                session: "alpha".into(),
                next_seq: 2,
                records: VecDeque::from([unsafe_record]),
                view: WorkStreamPreferences::default(),
                last_message_ordinals: HashMap::new(),
                last_file_ordinals: HashMap::new(),
                pull_request_snapshots: HashMap::new(),
            })
            .unwrap(),
        )
        .unwrap();

        let ledger = WorkLedger::with_storage("alpha", path.clone(), 10, u64::MAX);
        assert!(ledger.records().is_empty());
        ledger.flush();
        assert!(!fs::read_to_string(&path).unwrap().contains(sentinel));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn restored_pr_snapshot_dedupes_unchanged_verified_refresh() {
        let path = temp_path("restore-pr-snapshot");
        let mut work_identity = identity();
        work_identity.session = "alpha".into();
        work_identity.task_id = Some("TASK-1".into());
        let reference = WorkPullRequestRef {
            repository: "owner/repo".into(),
            number: 321,
            title: "Correlation".into(),
            url: "https://github.com/owner/repo/pull/321".into(),
            state: "open".into(),
            is_draft: false,
            review_decision: None,
            checks: crate::tracking::PullRequestChecks::Pending,
        };
        {
            let mut ledger = WorkLedger::with_storage("alpha", path.clone(), 10, u64::MAX);
            ledger.append_pull_request(
                WorkDraft {
                    kind: WorkKind::PullRequestOpened,
                    summary: "PR #321 opened".into(),
                    identity: work_identity,
                    evidence_source: EvidenceSource::GitHubPullRequest,
                    authority: WorkAuthority::GitHubCanonical,
                    verification: VerificationState::RemoteVerified,
                    dedupe_scope: "github-pr:open".into(),
                    fingerprint: "open".into(),
                },
                reference,
            );
            ledger.flush();
        }
        let mut restored = WorkLedger::with_storage("alpha", path.clone(), 10, u64::MAX);
        let current = crate::tracking::BranchPullRequestEntry {
            number: 321,
            title: "Correlation".into(),
            state: "open".into(),
            is_draft: false,
            review_decision: None,
            url: Some("https://github.com/owner/repo/pull/321".into()),
            head_repository_owner: "owner".into(),
            head_ref_name: "agent/work".into(),
            base_ref_name: "main".into(),
            updated_at: None,
            checks: crate::tracking::PullRequestChecks::Pending,
            correlation: None,
            correlation_marker_present: false,
        };
        assert!(restored
            .observe_pull_request("pane-test:owner/repo:321:TASK-1", &current)
            .is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn v2_roundtrip_restores_palette_promotion_and_filter() {
        let path = temp_path("view-roundtrip");
        let mut ledger = WorkLedger::with_storage("test", path.clone(), 10, u64::MAX);
        let mut palette = WorkStreamPalette::default();
        palette.observe_origins(["p1", "p2", "p3", "p4", "p5", "p6"]);
        assert!(palette.promote("p6"));
        ledger.set_view_preferences(WorkStreamPreferences {
            palette,
            filter: WorkStreamFilter::Pane("p6".into()),
        });
        ledger.append_at(10, draft("one", "one"));
        let mut view = ledger.view_preferences();
        view.filter = WorkStreamFilter::History;
        ledger.set_view_preferences(view);
        ledger.flush();

        let restored = WorkLedger::with_storage("test", path.clone(), 10, u64::MAX);
        let view = restored.view_preferences();
        assert_eq!(view.filter, WorkStreamFilter::History);
        assert_eq!(view.palette.marker("p6").as_deref(), Some("P5"));
        assert_eq!(restored.records().len(), 1);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn legacy_v1_without_view_migrates_with_defaults() {
        let path = temp_path("legacy-v1-no-view");
        fs::write(
            &path,
            r#"{"schema":"taarof.work-ledger.v1","session":"alpha","next_seq":4,"records":[]}"#,
        )
        .unwrap();
        let ledger = WorkLedger::with_storage("alpha", path.clone(), 10, u64::MAX);
        assert!(ledger.records().is_empty());
        assert_eq!(ledger.view_preferences(), WorkStreamPreferences::default());
        ledger.flush();
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("taarof.work-ledger.v3"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn persisted_palette_and_filter_sanitize_deterministically() {
        let mut view: WorkStreamPreferences = serde_json::from_value(serde_json::json!({
            "palette": {
                "origins": ["p1", "p2", "p1", "p3"],
                "color_slots": {"p1": 0, "p2": 0, "p3": 9}
            },
            "filter": {"pane": "missing"}
        }))
        .unwrap();
        view.sanitize();
        assert_eq!(view.palette.origins, vec!["p1", "p2", "p3"]);
        assert_eq!(view.palette.color_slot("p1"), Some(0));
        assert_eq!(view.palette.color_slot("p2"), None);
        assert_eq!(view.palette.color_slot("p3"), None);
        assert_eq!(view.filter, WorkStreamFilter::All);
    }

    #[test]
    fn corrupt_ledger_is_visible_but_startup_continues_empty() {
        let path = temp_path("corrupt-visible");
        fs::write(&path, "{\"schema\":").unwrap();
        let ledger = WorkLedger::with_storage("alpha", path.clone(), 10, u64::MAX);
        assert!(ledger.records().is_empty());
        assert_eq!(ledger.restore_health().status, "degraded");
        assert!(ledger
            .restore_health()
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("corrupt or partial")));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn restore_rejects_mixed_session_and_non_monotonic_rows() {
        let path = temp_path("mixed-session-seq");
        let mut first = work_stream_record(2, 1, WorkKind::AssistantMessageCompleted);
        first.identity.session = "alpha".into();
        let mut duplicate = first.clone();
        duplicate.summary = "duplicate".into();
        let mut foreign = work_stream_record(3, 2, WorkKind::AssistantMessageCompleted);
        foreign.identity.session = "beta".into();
        fs::write(
            &path,
            serde_json::to_string(&PersistedLedger {
                schema: "taarof.work-ledger.v2".into(),
                session: "alpha".into(),
                next_seq: 1,
                records: VecDeque::from([first, duplicate, foreign]),
                view: WorkStreamPreferences::default(),
                last_message_ordinals: HashMap::new(),
                last_file_ordinals: HashMap::new(),
                pull_request_snapshots: HashMap::new(),
            })
            .unwrap(),
        )
        .unwrap();
        let ledger = WorkLedger::with_storage("alpha", path.clone(), 10, u64::MAX);
        assert_eq!(ledger.records().len(), 1);
        assert_eq!(ledger.metadata_json()["next_seq"], 3);
        assert_eq!(ledger.restore_health().status, "degraded");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn clear_restart_does_not_replay_transcript_file_or_pr_observations() {
        let path = temp_path("clear-barrier");
        let mut ledger = WorkLedger::with_storage("test", path.clone(), 10, u64::MAX);
        ledger.append_at(10, draft("one", "one"));
        assert!(ledger.note_message_ordinal("pane:session", 7));
        assert!(ledger.note_file_ordinal("file:pane:session", 9));
        let pr = crate::tracking::BranchPullRequestEntry {
            number: 7,
            title: "Work".into(),
            state: "open".into(),
            is_draft: false,
            review_decision: None,
            url: Some("https://github.com/owner/repo/pull/7".into()),
            head_ref_name: "agent/work".into(),
            head_repository_owner: "owner".into(),
            base_ref_name: "main".into(),
            updated_at: None,
            checks: crate::tracking::PullRequestChecks::Pending,
            correlation: None,
            correlation_marker_present: false,
        };
        assert_eq!(
            ledger.observe_pull_request("pane:owner/repo:7:TASK", &pr)[0].0,
            WorkKind::PullRequestOpened
        );
        assert_eq!(ledger.clear_all(), 1);
        ledger
            .persistence_barrier()
            .unwrap()
            .recv()
            .expect("writer should acknowledge clear")
            .expect("clear should become durable");
        assert!(!ledger.note_message_ordinal("pane:session", 7));
        drop(ledger);
        let mut restored = WorkLedger::with_storage("test", path.clone(), 10, u64::MAX);
        assert!(restored.records().is_empty());
        assert!(!restored.note_message_ordinal("pane:session", 7));
        assert!(!restored.note_file_ordinal("file:pane:session", 9));
        assert!(restored
            .observe_pull_request("pane:owner/repo:7:TASK", &pr)
            .is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn persistence_barrier_reports_actual_write_failure() {
        let blocked_parent = temp_path("blocked-parent");
        fs::write(&blocked_parent, "not a directory").unwrap();
        let path = blocked_parent.join("ledger.json");
        let mut ledger = WorkLedger::with_storage("test", path, 10, u64::MAX);
        ledger.append_at(10, draft("one", "one"));
        ledger.clear_all();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while ledger.persistence_counts().unwrap().1 == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            ledger.persistence_counts().unwrap().1 > 0,
            "failed Wake should complete before the barrier is enqueued"
        );
        let result = ledger
            .persistence_barrier()
            .unwrap()
            .recv()
            .expect("writer should acknowledge failure");
        assert!(result.is_err());
        let _ = fs::remove_file(blocked_parent);
    }

    #[test]
    fn exhausted_persisted_next_sequences_are_repaired_before_append() {
        for exhausted in [u64::MAX - 1, u64::MAX] {
            let path = temp_path(&format!("exhausted-sequence-{exhausted}"));
            let record = work_stream_record(7, 1, WorkKind::AssistantMessageCompleted);
            fs::write(
                &path,
                serde_json::to_string(&PersistedLedger {
                    schema: "taarof.work-ledger.v3".into(),
                    session: "test".into(),
                    next_seq: exhausted,
                    records: VecDeque::from([record]),
                    view: WorkStreamPreferences::default(),
                    last_message_ordinals: HashMap::new(),
                    last_file_ordinals: HashMap::new(),
                    pull_request_snapshots: HashMap::new(),
                })
                .unwrap(),
            )
            .unwrap();
            let mut restored = WorkLedger::with_storage("test", path.clone(), 10, u64::MAX);
            let health = restored.restore_health();
            assert_eq!(health.status, "degraded");
            assert_eq!(health.rejected_records, 0);
            assert!(health
                .detail
                .as_deref()
                .unwrap()
                .contains("Sequence values"));
            assert!(!health.detail.as_deref().unwrap().contains("omitted"));
            assert_eq!(
                restored.append_at(11, draft("next", "next")).unwrap().seq,
                8
            );
            restored.flush();
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn finished_report_conflict_is_unverified_without_canonical_mutation() {
        let mut ledger = WorkLedger::empty("test".into(), 10, u64::MAX, None);
        let mut report = draft("finished", "Agent reported work finished");
        report.kind = WorkKind::AgentReportedFinished;
        report.evidence_source = EvidenceSource::AgentReport;
        let record = ledger.append_at(10, report).unwrap();
        let plan_task = ObservedPlanTask {
            id: "EXAMPLE-111".into(),
            title: "Ledger".into(),
            status: "todo".into(),
            fully_specified: true,
        };
        let pane = PaneProbe {
            identity: identity(),
            activity: None,
            local_cwd: None,
            binding_checkout_root: None,
            binding_identity: None,
            plan_root: Some("/repo".into()),
            plan_tasks: vec![plan_task.clone()],
            plan_loaded: true,
        };
        ledger.reconcile_plan_for_pane(&pane, 20);
        assert_eq!(
            ledger.reconciliation_for(record.seq).status,
            WorkReconciliationStatus::Unverified
        );
        assert_eq!(plan_task.status, "todo");
        assert_eq!(record.verification, VerificationState::Observed);
    }

    #[test]
    fn finished_and_transcript_observations_leave_exact_pr_fact_unchanged() {
        let mut ledger = WorkLedger::empty("test".into(), 10, u64::MAX, None);
        let mut pr_draft = draft("pr-opened", "PR #7 opened");
        pr_draft.kind = WorkKind::PullRequestOpened;
        pr_draft.evidence_source = EvidenceSource::GitHubPullRequest;
        pr_draft.authority = WorkAuthority::GitHubCanonical;
        pr_draft.verification = VerificationState::RemoteVerified;
        let pr = ledger
            .append_pull_request(
                pr_draft,
                WorkPullRequestRef {
                    repository: "owner/repo".into(),
                    number: 7,
                    title: "Canonical work".into(),
                    url: "https://github.com/owner/repo/pull/7".into(),
                    state: "open".into(),
                    is_draft: false,
                    review_decision: Some("approved".into()),
                    checks: crate::tracking::PullRequestChecks::Ready,
                },
            )
            .unwrap();
        let before_record = pr.clone();
        let before_reconciliation = ledger.reconciliation_for(pr.seq);

        ledger
            .append_at(11, draft("transcript", "Assistant message completed"))
            .unwrap();
        let mut finished = draft("finished", "Agent reported work finished");
        finished.kind = WorkKind::AgentReportedFinished;
        finished.evidence_source = EvidenceSource::AgentReport;
        ledger.append_at(12, finished).unwrap();
        ledger.reconcile_plan_for_pane(
            &PaneProbe {
                identity: identity(),
                activity: None,
                local_cwd: None,
                binding_checkout_root: None,
                binding_identity: None,
                plan_root: Some("/repo".into()),
                plan_tasks: vec![ObservedPlanTask {
                    id: "EXAMPLE-111".into(),
                    title: "Ledger".into(),
                    status: "in_progress".into(),
                    fully_specified: true,
                }],
                plan_loaded: true,
            },
            20,
        );

        let retained = ledger
            .records()
            .into_iter()
            .find(|record| record.seq == pr.seq)
            .expect("exact PR fact retained");
        assert_eq!(retained, before_record);
        assert_eq!(ledger.reconciliation_for(pr.seq), before_reconciliation);
        assert_eq!(retained.pull_request.as_ref().unwrap().state, "open");
    }

    #[test]
    fn missing_repository_and_deleted_task_reconcile_visibly_stale() {
        let mut ledger = WorkLedger::empty("test".into(), 10, u64::MAX, None);
        let record = ledger.append_at(10, draft("message", "message")).unwrap();
        // Generic transcript rows are not canonical task facts and therefore
        // are not overwritten by plan reconciliation.
        assert_eq!(
            ledger.reconciliation_for(record.seq).status,
            WorkReconciliationStatus::Verified
        );
        let mut plan_draft = draft("plan", "Task changed");
        plan_draft.kind = WorkKind::TaskStatusChanged;
        plan_draft.evidence_source = EvidenceSource::PlanFile;
        plan_draft.authority = WorkAuthority::PlanCanonical;
        plan_draft.verification = VerificationState::CanonicalFile;
        let plan_record = ledger.append_at(11, plan_draft).unwrap();
        let mut missing = PaneProbe {
            identity: identity(),
            activity: None,
            local_cwd: None,
            binding_checkout_root: None,
            binding_identity: None,
            plan_root: None,
            plan_tasks: Vec::new(),
            plan_loaded: false,
        };
        ledger.reconcile_plan_for_pane(&missing, 20);
        assert_eq!(
            ledger.reconciliation_for(plan_record.seq).status,
            WorkReconciliationStatus::Stale
        );
        assert!(ledger
            .reconciliation_for(plan_record.seq)
            .reason
            .contains("unavailable"));
        missing.plan_loaded = true;
        missing.plan_root = Some("/repo".into());
        ledger.reconcile_plan_for_pane(&missing, 21);
        assert!(ledger
            .reconciliation_for(plan_record.seq)
            .reason
            .contains("no longer exists"));
    }

    #[test]
    fn github_unavailable_keeps_exact_pr_fact_stale() {
        let mut ledger = WorkLedger::empty("test".into(), 10, u64::MAX, None);
        let mut pr_draft = draft("pr", "PR #7 opened");
        pr_draft.kind = WorkKind::PullRequestOpened;
        pr_draft.evidence_source = EvidenceSource::GitHubPullRequest;
        pr_draft.authority = WorkAuthority::GitHubCanonical;
        pr_draft.verification = VerificationState::RemoteVerified;
        let record = ledger
            .append_pull_request(
                pr_draft,
                WorkPullRequestRef {
                    repository: "owner/repo".into(),
                    number: 7,
                    title: "Work".into(),
                    url: "https://github.com/owner/repo/pull/7".into(),
                    state: "open".into(),
                    is_draft: false,
                    review_decision: None,
                    checks: crate::tracking::PullRequestChecks::Pending,
                },
            )
            .unwrap();
        ledger.reconcile_exact_pull_request("owner/repo", 7, Err("offline"));
        let reconciliation = ledger.reconciliation_for(record.seq);
        assert_eq!(reconciliation.status, WorkReconciliationStatus::Stale);
        assert_eq!(record.pull_request.as_ref().unwrap().state, "open");
        let merged = crate::tracking::BranchPullRequestEntry {
            number: 7,
            title: "Work".into(),
            state: "merged".into(),
            is_draft: false,
            review_decision: Some("approved".into()),
            url: Some("https://github.com/owner/repo/pull/7".into()),
            head_ref_name: "agent/work".into(),
            head_repository_owner: "owner".into(),
            base_ref_name: "main".into(),
            updated_at: None,
            checks: crate::tracking::PullRequestChecks::Ready,
            correlation: None,
            correlation_marker_present: false,
        };
        ledger.reconcile_exact_pull_request("owner/repo", 7, Ok(&merged));
        let reconciliation = ledger.reconciliation_for(record.seq);
        assert_eq!(reconciliation.status, WorkReconciliationStatus::Stale);
        assert_eq!(reconciliation.current_value.as_deref(), Some("merged"));
    }

    #[test]
    fn exact_pr_refresh_is_concurrent_and_globally_bounded() {
        let mut probes = (1..=EXACT_PR_REFRESH_LIMIT)
            .map(|number| ExactPullRequestProbe {
                repository: "owner/repo".into(),
                number: number as u64,
                result: Err("not run".into()),
            })
            .collect::<Vec<_>>();
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let started = std::time::Instant::now();
        enrich_exact_pull_requests_with(&mut probes, |repository, number| {
            let concurrent = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(concurrent, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(40));
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(crate::tracking::BranchPullRequestEntry {
                number,
                title: "Work".into(),
                state: "open".into(),
                is_draft: false,
                review_decision: None,
                url: Some(format!("https://github.com/{repository}/pull/{number}")),
                head_ref_name: "agent/work".into(),
                head_repository_owner: "owner".into(),
                base_ref_name: "main".into(),
                updated_at: None,
                checks: crate::tracking::PullRequestChecks::Pending,
                correlation: None,
                correlation_marker_present: false,
            })
        });
        assert!(started.elapsed() < std::time::Duration::from_millis(250));
        assert!((2..=EXACT_PR_REFRESH_CONCURRENCY).contains(&peak.load(Ordering::SeqCst)));
        assert!(probes.iter().all(|probe| probe.result.is_ok()));
    }

    #[test]
    fn five_hundred_retained_prs_produce_only_one_bounded_refresh_batch() {
        let mut state = crate::AppState::new();
        for number in 1..=500_u64 {
            let mut pr_draft = draft(&format!("pr-{number}"), &format!("PR #{number} opened"));
            pr_draft.kind = WorkKind::PullRequestOpened;
            pr_draft.evidence_source = EvidenceSource::GitHubPullRequest;
            pr_draft.authority = WorkAuthority::GitHubCanonical;
            pr_draft.verification = VerificationState::RemoteVerified;
            pr_draft.dedupe_scope = format!("pr-{number}");
            state.work_ledger.append_pull_request(
                pr_draft,
                WorkPullRequestRef {
                    repository: "owner/repo".into(),
                    number,
                    title: "Work".into(),
                    url: format!("https://github.com/owner/repo/pull/{number}"),
                    state: "open".into(),
                    is_draft: false,
                    review_decision: None,
                    checks: crate::tracking::PullRequestChecks::Pending,
                },
            );
        }
        assert_eq!(state.work_ledger.records().len(), 500);
        assert_eq!(
            collect_probe(&state).exact_pull_requests.len(),
            EXACT_PR_REFRESH_LIMIT
        );
    }
}
