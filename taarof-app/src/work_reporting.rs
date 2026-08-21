//! Explicit same-user work-reporting contract for task-bound panes.
//!
//! A reporting token is only a stale-context nonce. It does not grant task
//! authority and a report never mutates `.plan/tasks.json` or a tracker mirror.

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const MAX_REPORT_NOTE_BYTES: usize = 240;
pub const REPORTING_INSTRUCTIONS: &str = "Resolve the pane-local checkout and .plan/tasks.json. Mark the bound task in_progress only when substantive work starts; report started, progress, blocked, and finished milestones with taarof work-report. Reports and transcripts are observational and never mutate .plan, GitHub, or Linear, including after session restore. Mark done in .plan only after acceptance and tests pass. Add fully specified follow-up tasks to .plan; GitHub Issues and Linear are mirrors. PRs require one taarof.pr-work.v1 marker with verified task_id, normalized base owner/repo, exact head branch, and dispatch_source; fork head ownership comes from GitHub. Never include a local path or infer correlation from prose, titles, issue numbers, or ambiguity. Pending, stale, unverified, or historical work is not canonical evidence. Never report secrets, terminal contents, full transcripts, or tool payloads. If context is stale, unbound, or mismatched, stop and do not guess.";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkMilestone {
    Started,
    Progress,
    Blocked,
    Finished,
}

impl WorkMilestone {
    pub fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Progress => "progress",
            Self::Blocked => "blocked",
            Self::Finished => "finished",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkContext {
    pub session: String,
    pub workspace_origin: String,
    pub tab_origin: String,
    pub pane_origin: String,
    pub task_id: String,
    pub task_title: String,
    pub checkout_root: String,
    pub binding_token: String,
    pub tab_id: u32,
    pub pane_id: u32,
}

#[derive(Clone, Debug)]
pub struct WorkReport {
    pub milestone: WorkMilestone,
    pub note: Option<String>,
    pub context: WorkContext,
}

fn canonical_string(path: &Path) -> Option<String> {
    path.canonicalize()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

pub fn context_for_pane(
    state: &crate::AppState,
    tab_id: u32,
    pane_id: u32,
) -> Result<WorkContext, String> {
    let (workspace, tab) = state.find_tab(tab_id).ok_or("stale pane: tab not found")?;
    let binding = state
        .pane_task_binding(tab_id, pane_id)
        .ok_or("pane is not bound to a task")?;
    if !crate::task_binding::valid_reporting_token(&binding.reporting_token) {
        return Err("task binding has no valid reporting context; bind it again".to_string());
    }
    let cwd = state
        .pane_local_cwd(tab_id, pane_id)
        .map_err(|_| "bound pane has no local checkout cwd".to_string())?;
    let status = crate::task_binding::current_task_status(Some(&cwd), None, false, Some(binding))
        .filter(|status| status.resolved)
        .ok_or("bound task does not resolve in the pane-local checkout")?;
    let pinned = status
        .checkout_root
        .as_deref()
        .and_then(|root| canonical_string(Path::new(root)))
        .ok_or("bound checkout is unavailable")?;
    let current = crate::task_binding::plan_root_from_pane_cwd(&cwd)
        .as_deref()
        .and_then(canonical_string)
        .ok_or("pane cwd does not resolve to a checkout with .plan/tasks.json")?;
    if current != pinned {
        return Err("pane checkout no longer matches the bound checkout".to_string());
    }
    let pane_origin = crate::work_ledger::origin_for_pane(state, tab_id, pane_id)
        .ok_or("stale pane: stable pane identity unavailable")?;
    Ok(WorkContext {
        session: crate::instance::session_name().unwrap_or_else(|| "default".to_string()),
        workspace_origin: workspace.work_origin.clone(),
        tab_origin: tab.work_origin.clone(),
        pane_origin,
        task_id: binding.task_id.clone(),
        task_title: binding.title.clone(),
        checkout_root: pinned,
        binding_token: binding.reporting_token.clone(),
        tab_id,
        pane_id,
    })
}

pub fn validate_report(state: &crate::AppState, report: &WorkReport) -> Result<(u32, u32), String> {
    validate_note(report.note.as_deref())?;
    let expected_session = crate::instance::session_name().unwrap_or_else(|| "default".to_string());
    if report.context.session != expected_session {
        return Err("report context belongs to another taarof session".to_string());
    }

    let mut matched = None;
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            let mut pane_ids: Vec<u32> =
                tab.panes.leaves().iter().map(|leaf| leaf.pane_id).collect();
            pane_ids.extend(
                state
                    .headless_panes
                    .keys()
                    .filter_map(|(tab_id, pane_id)| (*tab_id == tab.id).then_some(*pane_id)),
            );
            pane_ids.sort_unstable();
            pane_ids.dedup();
            for pane_id in pane_ids {
                if crate::work_ledger::origin_for_pane(state, tab.id, pane_id).as_deref()
                    == Some(report.context.pane_origin.as_str())
                    && matched.replace((tab.id, pane_id)).is_some()
                {
                    return Err("stable pane identity is ambiguous".to_string());
                }
            }
        }
    }
    let (tab_id, pane_id) = matched.ok_or("stale pane context")?;
    let live = context_for_pane(state, tab_id, pane_id)?;
    if live.workspace_origin != report.context.workspace_origin
        || live.tab_origin != report.context.tab_origin
        || live.task_id != report.context.task_id
        || live.checkout_root != report.context.checkout_root
        || live.binding_token != report.context.binding_token
    {
        return Err("work report context is stale or mismatched".to_string());
    }
    Ok((tab_id, pane_id))
}

pub fn validate_note(note: Option<&str>) -> Result<(), String> {
    let Some(note) = note else {
        return Ok(());
    };
    if note.trim().is_empty() {
        return Err("report note must not be empty".to_string());
    }
    if note.len() > MAX_REPORT_NOTE_BYTES {
        return Err(format!(
            "report note exceeds {MAX_REPORT_NOTE_BYTES} UTF-8 bytes"
        ));
    }
    if note.chars().any(char::is_control) {
        return Err("report note must be one printable line".to_string());
    }
    let trimmed = note.trim();
    if looks_like_structured_payload(trimmed)
        || looks_like_credential(trimmed)
        || looks_like_uri_credential(trimmed)
        || looks_like_terminal_output(trimmed)
        || contains_high_entropy_token(trimmed)
    {
        return Err("report note is not safe bounded milestone text".to_string());
    }
    Ok(())
}

fn looks_like_structured_payload(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let parsed_container = serde_json::from_str::<serde_json::Value>(value)
        .ok()
        .is_some_and(|parsed| parsed.is_object() || parsed.is_array());
    parsed_container
        || (value.starts_with('{') && value.ends_with('}'))
        || (value.starts_with('[') && value.ends_with(']'))
        || [
            "tool_call",
            "tool call",
            "tool payload",
            "function_call",
            "function call",
            "\"arguments\"",
            "\"tool\"",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

pub(crate) fn looks_like_credential(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let compact: String = lower
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect();
    let assignment = [
        "aws_secret_access_key",
        "client_secret",
        "client-secret",
        "password",
        "passwd",
        "access_token",
        "refresh_token",
        "api_key",
        "api-key",
        "apikey",
        "token",
        "secret",
        "credential",
        "authorization",
    ]
    .iter()
    .any(|key| compact.contains(&format!("{key}=")) || compact.contains(&format!("{key}:")));
    assignment
        || has_credential_label_with_value(&lower)
        || lower.contains("authorization:")
        || lower.contains("bearer ")
        || lower.contains("-----begin ")
        || lower.contains("-----end ")
        || lower.contains("private key")
        || [
            "ghp_",
            "github_pat_",
            "sk-",
            "xoxb-",
            "xoxp-",
            "xoxa-",
            "xoxr-",
            "akia",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

fn has_credential_label_with_value(lower: &str) -> bool {
    let words = lower
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')))
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    words.iter().enumerate().any(|(index, word)| {
        let value_index = if matches!(
            *word,
            "password"
                | "passwd"
                | "token"
                | "secret"
                | "credential"
                | "authorization"
                | "client_secret"
                | "client-secret"
                | "access_token"
                | "refresh_token"
                | "api_key"
                | "api-key"
                | "apikey"
                | "aws_secret_access_key"
        ) {
            Some(index + 1)
        } else if (*word == "api" && words.get(index + 1) == Some(&"key"))
            || (*word == "client" && words.get(index + 1) == Some(&"secret"))
        {
            Some(index + 2)
        } else {
            None
        };
        let Some(value_index) = value_index else {
            return false;
        };
        let Some(immediate_value) = words.get(value_index) else {
            return false;
        };
        let is_only_remaining_word = value_index + 1 == words.len();
        is_only_remaining_word || looks_like_secret_value(immediate_value)
    })
}

fn looks_like_secret_value(value: &str) -> bool {
    let has_alpha = value.chars().any(|ch| ch.is_ascii_alphabetic());
    let has_digit = value.chars().any(|ch| ch.is_ascii_digit());
    let has_secret_punctuation = value
        .chars()
        .any(|ch| matches!(ch, '_' | '-' | '+' | '/' | '=' | '.'));
    value.len() >= 16
        || (value.len() >= 6 && has_alpha && has_digit)
        || (value.len() >= 8 && has_secret_punctuation)
}

pub(crate) fn looks_like_uri_credential(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if [
        "postgres://",
        "postgresql://",
        "mysql://",
        "mariadb://",
        "mongodb://",
        "mongodb+srv://",
        "redis://",
    ]
    .iter()
    .any(|scheme| lower.contains(scheme))
    {
        return true;
    }
    value.split_whitespace().any(|word| {
        word.find("://")
            .is_some_and(|scheme_end| word[scheme_end + 3..].contains('@'))
    })
}

pub(crate) fn looks_like_terminal_output(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    ["$ ", "# ", "% ", "> "]
        .iter()
        .any(|prompt| value.starts_with(prompt))
        || lower.starts_with("last login:")
        || lower.starts_with("command not found:")
        || (value.contains('@') && (value.contains("$ ") || value.contains("# ")))
}

pub(crate) fn contains_high_entropy_token(value: &str) -> bool {
    value
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || "_-+/=.".contains(ch)))
        .filter(|candidate| candidate.len() >= 24)
        .any(|candidate| {
            let jwt_parts = candidate.split('.').collect::<Vec<_>>();
            if jwt_parts.len() == 3
                && jwt_parts.iter().all(|part| {
                    part.len() >= 8
                        && part
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
                })
            {
                return true;
            }
            if candidate.len() < 32 {
                return false;
            }
            let lower = candidate.chars().any(|ch| ch.is_ascii_lowercase());
            let upper = candidate.chars().any(|ch| ch.is_ascii_uppercase());
            let digit = candidate.chars().any(|ch| ch.is_ascii_digit());
            let distinct = candidate
                .chars()
                .collect::<std::collections::HashSet<_>>()
                .len();
            if candidate.len() == 40 && candidate.chars().all(|ch| ch.is_ascii_hexdigit()) {
                return false;
            }
            let long_hex =
                candidate.len() >= 40 && candidate.chars().all(|ch| ch.is_ascii_hexdigit());
            long_hex || (lower && upper && digit && distinct >= 12)
        })
}

pub fn validate_agent_report_summary(
    kind: crate::work_ledger::WorkKind,
    summary: &str,
) -> Result<(), String> {
    let base = match kind {
        crate::work_ledger::WorkKind::AgentReportedStarted => "Agent reported work started",
        crate::work_ledger::WorkKind::AgentReportedProgress => "Agent reported progress",
        crate::work_ledger::WorkKind::AgentReportedBlocked => "Agent reported blocked",
        crate::work_ledger::WorkKind::AgentReportedFinished => "Agent reported work finished",
        _ => return Err("agent report has an invalid work kind".to_string()),
    };
    if summary == base {
        return Ok(());
    }
    let note = summary
        .strip_prefix(base)
        .and_then(|suffix| suffix.strip_prefix(": "))
        .ok_or_else(|| "agent report has an invalid summary".to_string())?;
    validate_note(Some(note))
}

pub fn context_file_path(runtime_dir: &Path, result_id: &str) -> PathBuf {
    let safe: String = result_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    runtime_dir.join("taarof-work-context").join(format!(
        "{safe}-{}.env",
        crate::task_binding::new_reporting_token()
    ))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn shell_exports(context: &WorkContext) -> String {
    let fields = [
        ("TAAROF_WORK_SESSION", context.session.as_str()),
        (
            "TAAROF_WORK_WORKSPACE_ORIGIN",
            context.workspace_origin.as_str(),
        ),
        ("TAAROF_WORK_TAB_ORIGIN", context.tab_origin.as_str()),
        ("TAAROF_WORK_PANE_ORIGIN", context.pane_origin.as_str()),
        ("TAAROF_WORK_TASK_ID", context.task_id.as_str()),
        ("TAAROF_WORK_TASK_TITLE", context.task_title.as_str()),
        ("TAAROF_WORK_CHECKOUT_ROOT", context.checkout_root.as_str()),
        ("TAAROF_WORK_BINDING_TOKEN", context.binding_token.as_str()),
        ("TAAROF_WORK_INSTRUCTIONS", REPORTING_INSTRUCTIONS),
    ];
    fields
        .iter()
        .map(|(key, value)| format!("export {key}={}", shell_quote(value)))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

struct TempFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextWriteCheckpoint {
    AfterOpen,
    AfterWrite,
    AfterSync,
    AfterChmod,
    BeforeRename,
}

fn write_context_file_inner<F>(
    path: &Path,
    context: &WorkContext,
    mut checkpoint: F,
) -> io::Result<()>
where
    F: FnMut(ContextWriteCheckpoint) -> io::Result<()>,
{
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "context path has no parent"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let temp = path.with_extension(format!(
        "tmp-{}",
        crate::task_binding::new_reporting_token()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let mut guard = TempFileGuard::new(temp.clone());
    checkpoint(ContextWriteCheckpoint::AfterOpen)?;
    file.write_all(shell_exports(context).as_bytes())?;
    checkpoint(ContextWriteCheckpoint::AfterWrite)?;
    file.sync_all()?;
    checkpoint(ContextWriteCheckpoint::AfterSync)?;
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
    checkpoint(ContextWriteCheckpoint::AfterChmod)?;
    checkpoint(ContextWriteCheckpoint::BeforeRename)?;
    fs::rename(&temp, path)?;
    guard.disarm();
    Ok(())
}

pub fn write_context_file(path: &Path, context: &WorkContext) -> io::Result<()> {
    write_context_file_inner(path, context, |_| Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_context() -> WorkContext {
        WorkContext {
            session: "default".into(),
            workspace_origin: "workspace-a".into(),
            tab_origin: "tab-a".into(),
            pane_origin: "pane-a".into(),
            task_id: "EXAMPLE-112".into(),
            task_title: "Agent's report".into(),
            checkout_root: "/tmp/work tree".into(),
            binding_token: crate::task_binding::new_reporting_token(),
            tab_id: 7,
            pane_id: 9,
        }
    }

    fn test_checkout(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "taarof-work-reporting-{label}-{}-{}",
            std::process::id(),
            crate::task_binding::new_reporting_token()
        ));
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join(".plan")).unwrap();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::write(
            root.join(".plan/tasks.json"),
            r#"{"tasks":[{"id":"EXAMPLE-112","title":"Report work","status":"todo"}]}"#,
        )
        .unwrap();
        root
    }

    #[test]
    fn note_validation_rejects_secrets_payloads_controls_and_oversize() {
        assert!(validate_note(Some("Parser tests passed")).is_ok());
        assert!(validate_note(Some("token parser tests passed")).is_ok());
        assert!(validate_note(Some("0123456789abcdef0123456789abcdef01234567")).is_ok());
        assert!(validate_note(Some(
            "Verified commit 0123456789abcdef0123456789abcdef01234567"
        ))
        .is_ok());
        for bad in [
            "password=hunter2",
            "Password hunter2",
            "Token abc123",
            "Token 0123456789abcdef0123456789abcdef01234567",
            "token=0123456789abcdef0123456789abcdef01234567", // gitleaks:allow -- synthetic rejection fixture
            "Token abcdefghijklmnop",
            "AWS_SECRET_ACCESS_KEY = abc123",
            "client_secret: abc123",
            "api key rotated; api_key = abc123",
            "Authorization: Bearer abc",
            "Bearer abcdefghijklmnopqrstuvwxyz",
            "-----BEGIN CERTIFICATE-----", // gitleaks:allow -- bare PEM header, no key material
            "-----BEGIN OPENSSH PRIVATE KEY-----", // gitleaks:allow -- bare PEM header, no key material
            "ghp_1234567890", // gitleaks:allow -- synthetic rejection fixture
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c", // gitleaks:allow -- public JWT test vector
            "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "postgresql://user:pass@db.example/app",
            "https://user:pass@example.test/path",
            r#"{\"tool_call\":\"bash\",\"arguments\":{}}"#,
            r#"[\"complete\", {\"tool\":\"bash\"}]"#,
            "$ cargo test",
            "user@host:~$ cargo test",
            "Last login: yesterday",
            "\u{1b}[31mterminal output",
            "line one\nline two",
        ] {
            assert!(validate_note(Some(bad)).is_err(), "accepted {bad:?}");
        }
        assert!(validate_note(Some(&"x".repeat(MAX_REPORT_NOTE_BYTES + 1))).is_err());
    }

    #[test]
    fn private_context_file_is_atomic_and_shell_quoted() {
        let root = std::env::temp_dir().join(format!(
            "taarof-work-context-test-{}-{}",
            std::process::id(),
            crate::task_binding::new_reporting_token()
        ));
        let path = context_file_path(&root, "EXAMPLE-112");
        let context = sample_context();
        write_context_file(&path, &context).unwrap();
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("TAAROF_WORK_TASK_ID='EXAMPLE-112'"));
        assert!(text.contains("TAAROF_WORK_TASK_TITLE='Agent'\\''s report'"));
        assert!(!text.contains("export TAAROF_WORK_TAB_ID"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn context_temp_file_is_removed_at_every_pre_rename_failure_checkpoint() {
        for failure in [
            ContextWriteCheckpoint::AfterOpen,
            ContextWriteCheckpoint::AfterWrite,
            ContextWriteCheckpoint::AfterSync,
            ContextWriteCheckpoint::AfterChmod,
            ContextWriteCheckpoint::BeforeRename,
        ] {
            let root = std::env::temp_dir().join(format!(
                "taarof-work-context-failure-{failure:?}-{}-{}",
                std::process::id(),
                crate::task_binding::new_reporting_token()
            ));
            let path = context_file_path(&root, "EXAMPLE-112");
            let error = write_context_file_inner(&path, &sample_context(), |checkpoint| {
                if checkpoint == failure {
                    Err(io::Error::other("injected context write failure"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Other);
            assert!(!path.exists(), "final file survived {failure:?}");
            let entries = fs::read_dir(path.parent().unwrap())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert!(entries.is_empty(), "temp file survived {failure:?}");
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn report_requires_exact_live_binding_and_never_mutates_plan() {
        let root = test_checkout("exact");
        let other = test_checkout("other-worktree");
        let plan_path = root.join(".plan/tasks.json");
        let before = fs::read(&plan_path).unwrap();
        let mut state = crate::AppState::new();
        let workspace = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "agent",
            crate::HeadlessPaneSeed {
                cwd: Some(root.join("src/nested").to_string_lossy().into_owned()),
                shell_running: true,
                ..Default::default()
            },
        )
        .unwrap();
        state
            .bind_pane_to_task(tab_id, pane_id, "EXAMPLE-112")
            .unwrap();
        let context = context_for_pane(&state, tab_id, pane_id).unwrap();
        let report = WorkReport {
            milestone: WorkMilestone::Started,
            note: Some("Implementation started".into()),
            context: context.clone(),
        };
        assert_eq!(validate_report(&state, &report), Ok((tab_id, pane_id)));
        assert!(state.record_agent_report(
            tab_id,
            pane_id,
            report.milestone,
            report.note.as_deref()
        ));
        assert_eq!(fs::read(&plan_path).unwrap(), before);
        let hostile = "Password RAW_REPORT_SENTINEL";
        assert!(!state.record_agent_report(
            tab_id,
            pane_id,
            WorkMilestone::Progress,
            Some(hostile)
        ));
        for surface in [
            serde_json::to_string(&state.work_ledger.records()).unwrap(),
            serde_json::to_string(&state.event_store.entries()).unwrap(),
            crate::api::build_state_snapshot(&state).to_string(),
            serde_json::to_string(&state.work_ledger.sidebar_records(&context.pane_origin))
                .unwrap(),
        ] {
            assert!(!surface.contains(hostile));
        }
        let record = state.work_ledger.records().last().cloned().unwrap();
        assert_eq!(
            record.kind,
            crate::work_ledger::WorkKind::AgentReportedStarted
        );
        assert_eq!(
            record.evidence_source,
            crate::work_ledger::EvidenceSource::AgentReport
        );
        assert_eq!(
            record.authority,
            crate::work_ledger::WorkAuthority::AgentObservation
        );
        assert_eq!(record.identity.task_id.as_deref(), Some("EXAMPLE-112"));

        state
            .headless_pane_mut(tab_id, pane_id)
            .unwrap()
            .location_state
            .cwd = Some(other.join("src").to_string_lossy().into_owned());
        assert!(validate_report(&state, &report)
            .unwrap_err()
            .contains("checkout"));
        state
            .headless_pane_mut(tab_id, pane_id)
            .unwrap()
            .location_state
            .cwd = Some(root.join("src").to_string_lossy().into_owned());
        state
            .bind_pane_to_task(tab_id, pane_id, "EXAMPLE-112")
            .unwrap();
        assert!(validate_report(&state, &report)
            .unwrap_err()
            .contains("stale or mismatched"));
        assert!(state.clear_pane_task_binding(tab_id, pane_id));
        assert!(validate_report(&state, &report).is_err());
        assert_eq!(fs::read(&plan_path).unwrap(), before);
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(other);
    }
}
