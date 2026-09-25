use gtk::prelude::*;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const VIEW_SCHEMA_VERSION: u32 = 1;
const DEFAULT_RECENT_ALERT_LIMIT: usize = 20;
const FAILING_TEST_RUN_LIMIT: usize = 50;
const FAILING_TEST_RUN_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewPreset {
    AgentActivity,
    WorkspaceHealth,
    ListeningPorts,
    RecentAlerts,
    CurrentlyFlaggedPanes,
    FailingTestRuns,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedView {
    pub name: String,
    pub preset: ViewPreset,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ViewStore {
    #[serde(default = "view_schema_version")]
    version: u32,
    #[serde(default)]
    views: Vec<SavedView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewStoreSignature {
    modified: Option<std::time::SystemTime>,
    len: u64,
}

#[derive(Debug, Clone)]
struct ViewStoreCacheEntry {
    path: PathBuf,
    signature: Option<ViewStoreSignature>,
    store: ViewStore,
}

type ViewStoreCache = Option<ViewStoreCacheEntry>;

fn view_store_cache() -> &'static Mutex<ViewStoreCache> {
    static CACHE: OnceLock<Mutex<ViewStoreCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

impl Default for ViewStore {
    fn default() -> Self {
        Self {
            version: VIEW_SCHEMA_VERSION,
            views: Vec::new(),
        }
    }
}

impl ViewPreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::AgentActivity => "Agent Activity",
            Self::WorkspaceHealth => "Workspace Health",
            Self::ListeningPorts => "Listening Ports",
            Self::RecentAlerts => "Recent Alerts",
            Self::CurrentlyFlaggedPanes => "Currently Flagged Panes",
            Self::FailingTestRuns => "Failing Test Runs",
        }
    }

    pub fn default_name(self) -> &'static str {
        self.label()
    }

    /// Whether this preset needs structured data that taarof does not yet
    /// capture. When `true`, the saved-view panel renders the placeholder
    /// from [`ViewPreset::not_yet_wired_placeholder`] instead of trying to
    /// populate the view from real data.
    ///
    /// All currently-shipped presets draw from data that already exists in
    /// the runtime (workspace/tab state, the event store, the diagnostics
    /// ring buffer, the health snapshot). The hook stays at `false` for
    /// those presets and should only be flipped to `true` when adding a
    /// preset whose data pipeline is not yet implemented.
    pub fn requires_unimplemented_data(self) -> bool {
        match self {
            Self::AgentActivity
            | Self::WorkspaceHealth
            | Self::ListeningPorts
            | Self::RecentAlerts
            | Self::CurrentlyFlaggedPanes
            | Self::FailingTestRuns => false,
        }
    }

    /// Human-readable placeholder shown in the saved-view panel for any preset
    /// that [`requires_unimplemented_data`] returns `true` for.  The message
    /// must clearly indicate the view is not yet wired so the user understands
    /// no data will appear until the pipeline is implemented.
    pub fn not_yet_wired_placeholder(self) -> &'static str {
        "This view is not yet wired to a live data source. Check back after the underlying data pipeline is implemented."
    }
}

impl SavedView {
    pub fn effective_limit(&self) -> usize {
        match self.preset {
            ViewPreset::RecentAlerts => self.limit.unwrap_or(DEFAULT_RECENT_ALERT_LIMIT).max(1),
            _ => self.limit.unwrap_or(100).max(1),
        }
    }
}

pub fn list() -> io::Result<Vec<SavedView>> {
    let mut store = cached_store(&views_path())?;
    sort_views(&mut store.views);
    Ok(store.views)
}

#[cfg(test)]
pub fn find(name: &str) -> Option<SavedView> {
    list()
        .ok()?
        .into_iter()
        .find(|view| view.name.eq_ignore_ascii_case(name))
}

pub fn save(view: SavedView) -> io::Result<()> {
    let path = views_path();
    let mut store = load_store(&path)?;
    upsert_view(&mut store.views, view);
    sort_views(&mut store.views);
    save_store(&path, &store)?;
    cache_store(&path, &store);
    Ok(())
}

pub fn delete(name: &str) -> io::Result<bool> {
    let path = views_path();
    let mut store = load_store(&path)?;
    let before = store.views.len();
    store
        .views
        .retain(|view| !view.name.eq_ignore_ascii_case(name));

    if store.views.len() == before {
        return Ok(false);
    }

    sort_views(&mut store.views);
    save_store(&path, &store)?;
    cache_store(&path, &store);
    Ok(true)
}

pub fn populate_saved_view_panel(
    detail_box: &gtk::Box,
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    view: &SavedView,
) {
    clear_detail_box(detail_box);

    if view.preset.requires_unimplemented_data() {
        append_header(detail_box, &view.name, Some(view.preset.label()));
        append_empty_state(detail_box, view.preset.not_yet_wired_placeholder());
        return;
    }

    append_header(
        detail_box,
        &view.name,
        Some(&format!("{} · live view", view.preset.label())),
    );

    match view.preset {
        ViewPreset::AgentActivity => populate_agent_activity_view(detail_box, state),
        ViewPreset::WorkspaceHealth => populate_workspace_health_view(detail_box, state),
        ViewPreset::ListeningPorts => populate_listening_ports_view(detail_box, state),
        ViewPreset::RecentAlerts => {
            populate_recent_alerts_view(detail_box, state, view.effective_limit())
        }
        ViewPreset::CurrentlyFlaggedPanes => {
            populate_attention_list_view(detail_box, state, tab_list, term_stack)
        }
        ViewPreset::FailingTestRuns => populate_failing_test_runs_view(detail_box, state),
    }
}

fn populate_agent_activity_view(
    detail_box: &gtk::Box,
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
) {
    let st = state.borrow();
    let mut rows = Vec::new();

    for workspace in &st.workspaces {
        for tab in &workspace.tabs {
            let Some(activity) = tab.agent_activity.as_ref() else {
                let Some(subtitle) = agent_activity_fallback_subtitle(tab) else {
                    continue;
                };
                rows.push((format!("{} / {}", workspace.name, tab.name), subtitle));
                continue;
            };

            let mut parts = vec![match activity.state {
                crate::workspace::AgentActivityState::Idle => "idle".to_string(),
                crate::workspace::AgentActivityState::Running => "running".to_string(),
                crate::workspace::AgentActivityState::WaitingInput => "waiting-input".to_string(),
                crate::workspace::AgentActivityState::Errored => "errored".to_string(),
                crate::workspace::AgentActivityState::Done => "done".to_string(),
            }];
            if !activity.text.trim().is_empty() {
                parts.push(activity.text.clone());
            }
            if let Some(source) = activity.source.as_ref() {
                parts.push(format!("via {source}"));
            }

            rows.push((
                format!("{} / {}", workspace.name, tab.name),
                parts.join(" · "),
            ));
        }
    }

    if rows.is_empty() {
        append_empty_state(detail_box, "No active or recent agent activity");
        return;
    }

    for (title, subtitle) in rows {
        append_row(detail_box, &title, &subtitle);
    }
}

fn agent_activity_fallback_subtitle(tab: &crate::Tab) -> Option<String> {
    (tab.agent_running && tab.agent_activity.is_none()).then(|| "running".to_string())
}

fn populate_workspace_health_view(
    detail_box: &gtk::Box,
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
) {
    let st = state.borrow();
    let health = crate::api::build_health_snapshot(&st);
    let diagnostics = crate::diagnostics::snapshot();

    let runtime_summary = format!(
        "{} · {} degraded components · {} probe failures · {} command failures · {} dropped events{}",
        health["state"].as_str().unwrap_or("ok"),
        health["degraded_components"].as_u64().unwrap_or(0),
        health["probe_failures"].as_u64().unwrap_or(0),
        health["command_failures"].as_u64().unwrap_or(0),
        health["event_drops"].as_u64().unwrap_or(0),
        health["last_error_at_unix_ms"]
            .as_u64()
            .map(|timestamp| format!(" · last error {timestamp}"))
            .unwrap_or_default(),
    );
    append_row(detail_box, "Runtime Diagnostics", &runtime_summary);

    if let Some(components) = health["components"].as_array() {
        for component in components.iter().take(3) {
            let label = component["label"].as_str().unwrap_or("runtime");
            let state = component["state"].as_str().unwrap_or("unknown");
            let error = component["error"].as_str().unwrap_or("no error details");
            append_row(detail_box, &format!("{label} · {state}"), error);
        }
    }

    for record in diagnostics.recent.iter().rev().take(3) {
        append_row(
            detail_box,
            &format!("{} · {}", record.source, record.action),
            &format!("{} · {}", record.message, record.ts_unix_ms),
        );
    }

    if st.workspaces.is_empty() {
        append_empty_state(detail_box, "No workspaces available");
        return;
    }

    for workspace in &st.workspaces {
        let alert_count = workspace
            .tabs
            .iter()
            .filter(|tab| st.tab_needs_attention(tab))
            .count();
        let port_count: usize = workspace
            .tabs
            .iter()
            .map(|tab| tab.listening_ports.len())
            .sum();
        let running_agents = workspace
            .tabs
            .iter()
            .filter(|tab| tab.agent_running)
            .count();
        let run_status = match workspace.run_status {
            crate::workspace::WorkspaceStatus::Idle => "idle",
            crate::workspace::WorkspaceStatus::Running => "running",
            crate::workspace::WorkspaceStatus::Errored => "errored",
        };

        let subtitle = format!(
            "{} tabs · {} alerts · {} ports · {} agents · {}{}",
            workspace.tabs.len(),
            alert_count,
            port_count,
            running_agents,
            run_status,
            if workspace.is_worktree {
                " · worktree"
            } else {
                ""
            }
        );
        append_row(detail_box, &workspace.name, &subtitle);
    }
}

fn populate_listening_ports_view(
    detail_box: &gtk::Box,
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
) {
    let st = state.borrow();
    let mut rows = Vec::new();

    for workspace in &st.workspaces {
        for tab in &workspace.tabs {
            for port in &tab.listening_ports {
                rows.push((
                    format!("{}:{}", workspace.name, port),
                    format!("tab {}", tab.name),
                ));
            }
        }
    }

    if rows.is_empty() {
        append_empty_state(detail_box, "No listening ports detected");
        return;
    }

    rows.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    for (title, subtitle) in rows {
        append_row(detail_box, &title, &subtitle);
    }
}

fn populate_recent_alerts_view(
    detail_box: &gtk::Box,
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
    limit: usize,
) {
    let st = state.borrow();
    let events: Vec<_> = st
        .event_store
        .entries()
        .into_iter()
        .rev()
        .filter(|event| event.event_type.starts_with("alert_"))
        .take(limit)
        .collect();

    if events.is_empty() {
        append_empty_state(detail_box, "No recent alerts recorded");
        return;
    }

    for event in events {
        let tab_name = event
            .payload
            .get("tab_name")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown tab");
        let message = event
            .payload
            .get("message")
            .and_then(|value| value.as_str())
            .unwrap_or(&event.event_type);
        let source = event
            .payload
            .get("source")
            .and_then(|value| value.as_str())
            .unwrap_or("system");
        append_row(
            detail_box,
            &format!("#{} · {}", event.seq, tab_name),
            &format!("{} · {}", message, source),
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FailingTestRunEntry {
    tab_id: u32,
    pane_id: u32,
    title: String,
    subtitle: String,
}

fn looks_like_test_command(command: &str) -> bool {
    let normalized = command.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return false;
    }

    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    if tokens.len() >= 2 {
        let first = command_basename(tokens[0]);
        let second = tokens[1];
        if matches!(first, "cargo" | "go" | "npm" | "yarn") && second == "test" {
            return true;
        }
        if first == "bundle" && second == "exec" {
            return tokens
                .get(2)
                .is_some_and(|third| command_basename(third) == "rspec");
        }
    }

    tokens.iter().any(|token| {
        matches!(
            command_basename(token),
            "test" | "pytest" | "jest" | "vitest" | "mocha" | "rspec"
        )
    }) || looks_like_mise_test_command(&tokens)
}

fn command_basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

fn looks_like_mise_test_command(tokens: &[&str]) -> bool {
    tokens.iter().enumerate().any(|(index, token)| {
        if command_basename(token) != "mise" {
            return false;
        }
        tokens.get(index + 1).is_some_and(|next| *next == "run")
            && tokens
                .iter()
                .skip(index + 2)
                .any(|arg| arg.starts_with("test"))
    })
}

fn format_event_age(ts_unix_ms: u64) -> String {
    let age_ms = crate::events::unix_time_ms().saturating_sub(ts_unix_ms);
    let age_secs = age_ms / 1000;
    if age_secs < 60 {
        format!("{age_secs}s ago")
    } else if age_secs < 3600 {
        format!("{}m ago", age_secs / 60)
    } else {
        format!("{}h ago", age_secs / 3600)
    }
}

fn normalize_failure_snippet(snippet: &str) -> Option<String> {
    let lines: Vec<&str> = snippet
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }

    let start = lines.len().saturating_sub(4);
    Some(lines[start..].join("\n"))
}

fn failing_test_run_entries(state: &crate::AppState) -> Vec<FailingTestRunEntry> {
    let cutoff = crate::events::unix_time_ms().saturating_sub(FAILING_TEST_RUN_WINDOW_MS);
    state
        .event_store
        .entries()
        .into_iter()
        .rev()
        .filter(|event| event.event_type == "command_exited" && event.ts_unix_ms >= cutoff)
        .filter_map(|event| {
            let exit_code = event
                .payload
                .get("exit_code")
                .and_then(|value| value.as_i64())?;
            if exit_code == 0 {
                return None;
            }

            let command = event
                .payload
                .get("command")
                .and_then(|value| value.as_str())?;
            if !looks_like_test_command(command) {
                return None;
            }

            let tab_id = event
                .payload
                .get("tab_id")
                .and_then(|value| value.as_u64())
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or_default();
            let tab_name = event
                .payload
                .get("tab_name")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown tab");
            let pane_id = event
                .payload
                .get("pane_id")
                .and_then(|value| value.as_u64())
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or_default();
            let snippet = event
                .payload
                .get("snippet")
                .and_then(|value| value.as_str())
                .and_then(normalize_failure_snippet);
            let mut subtitle = format!(
                "pane {pane_id} · {command} · exit {exit_code} · {}",
                format_event_age(event.ts_unix_ms),
            );
            if let Some(snippet) = snippet {
                subtitle.push('\n');
                subtitle.push_str(&snippet);
            }

            Some(FailingTestRunEntry {
                tab_id,
                pane_id,
                title: tab_name.to_string(),
                subtitle,
            })
        })
        .take(FAILING_TEST_RUN_LIMIT)
        .collect()
}

fn populate_failing_test_runs_view(
    detail_box: &gtk::Box,
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
) {
    let rows = {
        let st = state.borrow();
        failing_test_run_entries(&st)
    };

    if rows.is_empty() {
        append_empty_state(detail_box, "No recent failing test runs");
        return;
    }

    for row in rows {
        append_row(detail_box, &row.title, &row.subtitle);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AttentionListEntry {
    tab_id: u32,
    pane_id: u32,
    title: String,
    subtitle: String,
}

fn attention_list_entries(state: &crate::AppState) -> Vec<AttentionListEntry> {
    crate::attention::attention_targets_at(state, crate::events::unix_time_ms())
        .into_iter()
        .map(|target| {
            let reason = match target.evidence.freshness {
                crate::attention::AttentionFreshness::Fresh => {
                    target.evidence.reason.label().to_string()
                }
                crate::attention::AttentionFreshness::Stale => "Unknown reason (stale)".into(),
                crate::attention::AttentionFreshness::Conflicting => {
                    "Unknown reason (conflicting)".into()
                }
                crate::attention::AttentionFreshness::Unknown => "Unknown reason".into(),
            };
            let provider = target.evidence.provider.as_deref().unwrap_or("unknown provider");
            let verified = target
                .evidence
                .last_verified_unix_ms
                .map(format_event_age)
                .map(|age| format!("verified {age}"))
                .unwrap_or_else(|| "verification time unknown".into());
            AttentionListEntry {
                tab_id: target.tab_id,
                pane_id: target.pane_id,
                title: format!(
                    "{} / {} · pane {}",
                    target.workspace_name, target.tab_name, target.pane_id
                ),
                subtitle: format!(
                    "{reason} · {provider} · {} · repository {} · worktree {} · {} via {} · {verified}",
                    target.machine,
                    target.repository,
                    target.worktree,
                    target.evidence.authority.wire(),
                    target.evidence.provenance,
                ),
            }
        })
        .collect()
}

pub(crate) fn first_attention_target(state: &crate::AppState) -> Option<(u32, u32)> {
    crate::attention::attention_targets_at(state, crate::events::unix_time_ms())
        .first()
        .map(|target| (target.tab_id, target.pane_id))
}

pub(crate) fn jump_to_attention_target(
    state: &mut crate::AppState,
    tab_id: u32,
    pane_id: u32,
) -> bool {
    let Some(tab) = state.find_tab_mut(tab_id) else {
        return false;
    };
    if !tab.panes.contains_pane(pane_id) {
        return false;
    }
    tab.focused_pane_id = pane_id;
    state.activate_tab(tab_id).is_some()
}

pub(crate) fn focus_attention_target(
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    tab_id: u32,
    pane_id: u32,
) -> bool {
    let target_exists = {
        let mut state = state.borrow_mut();
        jump_to_attention_target(&mut state, tab_id, pane_id)
    };
    target_exists && crate::sidebar::focus_agent_pane(state, tab_list, term_stack, tab_id, pane_id)
}

fn populate_attention_list_view(
    detail_box: &gtk::Box,
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
) {
    let rows = {
        let st = state.borrow();
        attention_list_entries(&st)
    };

    if rows.is_empty() {
        append_empty_state(detail_box, "No panes currently flagged for attention");
        return;
    }

    for entry in rows {
        append_attention_row(detail_box, state, tab_list, term_stack, &entry);
    }
}

fn clear_detail_box(detail_box: &gtk::Box) {
    while let Some(child) = detail_box.first_child() {
        detail_box.remove(&child);
    }
}

fn append_header(detail_box: &gtk::Box, title: &str, subtitle: Option<&str>) {
    let title_label = gtk::Label::new(Some(title));
    title_label.add_css_class("title-3");
    title_label.set_halign(gtk::Align::Start);
    detail_box.append(&title_label);

    if let Some(subtitle) = subtitle {
        let subtitle_label = gtk::Label::new(Some(subtitle));
        subtitle_label.add_css_class("dim-label");
        subtitle_label.add_css_class("caption");
        subtitle_label.set_halign(gtk::Align::Start);
        subtitle_label.set_wrap(true);
        detail_box.append(&subtitle_label);
    }
}

fn append_empty_state(detail_box: &gtk::Box, message: &str) {
    let label = gtk::Label::new(Some(message));
    label.add_css_class("dim-label");
    label.set_halign(gtk::Align::Start);
    label.set_wrap(true);
    detail_box.append(&label);
}

fn append_row(detail_box: &gtk::Box, title: &str, subtitle: &str) {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
    row.set_margin_top(6);
    row.set_margin_bottom(6);

    let title_label = gtk::Label::new(Some(title));
    title_label.set_halign(gtk::Align::Start);
    title_label.set_wrap(true);
    row.append(&title_label);

    let subtitle_label = gtk::Label::new(Some(subtitle));
    subtitle_label.add_css_class("dim-label");
    subtitle_label.add_css_class("caption");
    subtitle_label.set_halign(gtk::Align::Start);
    subtitle_label.set_wrap(true);
    row.append(&subtitle_label);

    detail_box.append(&row);
}

fn append_attention_row(
    detail_box: &gtk::Box,
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    entry: &AttentionListEntry,
) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.set_margin_top(6);
    row.set_margin_bottom(6);

    let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
    labels.set_hexpand(true);

    let title_label = gtk::Label::new(Some(&entry.title));
    title_label.set_halign(gtk::Align::Start);
    title_label.set_hexpand(true);
    title_label.set_wrap(true);
    labels.append(&title_label);

    let subtitle_label = gtk::Label::new(Some(&entry.subtitle));
    subtitle_label.add_css_class("dim-label");
    subtitle_label.add_css_class("caption");
    subtitle_label.set_halign(gtk::Align::Start);
    subtitle_label.set_wrap(true);
    labels.append(&subtitle_label);

    row.append(&labels);

    let jump_button = gtk::Button::with_label("Jump");
    jump_button.add_css_class("flat");
    jump_button.set_valign(gtk::Align::Center);
    {
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let tab_id = entry.tab_id;
        let pane_id = entry.pane_id;
        jump_button.connect_clicked(move |_| {
            if !focus_attention_target(&state, &tab_list, &term_stack, tab_id, pane_id) {
                crate::show_error_toast("Attention target is no longer available");
                return;
            }
            crate::dashboard::refresh_dashboard_if_open(&state, &term_stack);
        });
    }
    row.append(&jump_button);

    detail_box.append(&row);
}

fn view_schema_version() -> u32 {
    VIEW_SCHEMA_VERSION
}

fn views_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config")
        })
        .join("taarof/views.json")
}

fn cached_store(path: &Path) -> io::Result<ViewStore> {
    let current_signature = load_store_signature(path)?;

    if let Some(cached_entry) = view_store_cache()
        .lock()
        .expect("view cache lock should not be poisoned")
        .as_ref()
        .filter(|cached_entry| {
            cached_entry.path == path && cached_entry.signature == current_signature
        })
    {
        return Ok(cached_entry.store.clone());
    }

    let store = load_store(path)?;
    cache_store(path, &store);
    Ok(store)
}

fn cache_store(path: &Path, store: &ViewStore) {
    let signature = load_store_signature(path).ok().flatten();
    *view_store_cache()
        .lock()
        .expect("view cache lock should not be poisoned") = Some(ViewStoreCacheEntry {
        path: path.to_path_buf(),
        signature,
        store: store.clone(),
    });
}

fn load_store_signature(path: &Path) -> io::Result<Option<ViewStoreSignature>> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(ViewStoreSignature {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn load_store(path: &Path) -> io::Result<ViewStore> {
    if !path.exists() {
        return Ok(ViewStore::default());
    }

    let content = std::fs::read_to_string(path)?;
    serde_json::from_str(&content).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn save_store(path: &Path, store: &ViewStore) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(store)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    crate::private_atomic_file::replace(path, &json)
}

fn upsert_view(views: &mut Vec<SavedView>, view: SavedView) {
    if let Some(existing) = views
        .iter_mut()
        .find(|existing| existing.name.eq_ignore_ascii_case(&view.name))
    {
        *existing = view;
    } else {
        views.push(view);
    }
}

fn sort_views(views: &mut [SavedView]) {
    views.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.preset.cmp(&right.preset))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::PaneNode;
    use crate::workspace::{AgentActivity, AgentActivityOrigin, AgentActivityState, Tab, TabKind};
    use crate::AppState;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::time::Instant;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct ScopedEnv {
        lock_dir: PathBuf,
        previous: Vec<(&'static str, Option<OsString>)>,
    }

    impl ScopedEnv {
        fn set(pairs: &[(&'static str, OsString)]) -> Self {
            let lock_dir = acquire_env_lock();
            let mut previous = Vec::with_capacity(pairs.len());
            for (key, value) in pairs {
                previous.push((*key, std::env::var_os(key)));
                std::env::set_var(key, value);
            }
            Self { lock_dir, previous }
        }
    }

    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            for (key, value) in self.previous.iter().rev() {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
            let _ = std::fs::remove_dir(&self.lock_dir);
        }
    }

    #[test]
    fn private_atomic_save_replaces_public_destination() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path();
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        save_store(&path, &ViewStore::default()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        load_store(&path).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    fn temp_path() -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time");
        let dir = std::env::temp_dir().join(format!("taarof-view-test-{}", ts.as_nanos()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("views.json")
    }

    fn temp_config_home(label: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time");
        let dir =
            std::env::temp_dir().join(format!("taarof-view-config-{label}-{}", ts.as_nanos()));
        std::fs::create_dir_all(&dir).expect("create config home");
        dir
    }

    fn config_views_path(config_home: &Path) -> PathBuf {
        config_home.join("taarof/views.json")
    }

    fn write_view_fixture(path: &Path, views: Vec<SavedView>) {
        let store = ViewStore {
            version: VIEW_SCHEMA_VERSION,
            views,
        };
        save_store(path, &store).expect("fixture should be written");
    }

    fn acquire_env_lock() -> PathBuf {
        let lock_dir = std::env::temp_dir().join("taarof-test-env-lock");
        loop {
            match std::fs::create_dir(&lock_dir) {
                Ok(()) => return lock_dir,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(err) => panic!("failed to acquire env lock: {err}"),
            }
        }
    }

    fn stub_tab(id: u32) -> Tab {
        Tab {
            id,
            name: format!("Tab {id}"),
            work_origin: crate::workspace::new_tab_work_origin(),
            kind: TabKind::Terminal,
            panes: Box::new(PaneNode::Stub { pane_id: id + 100 }),
            focused_pane_id: id + 100,
            next_pane_id: id + 101,
            pane_zoom: None,
            close_on_exit: true,
            respawn_on_exit: None,
            agent_running: false,
            agent_name: None,
            agent_session_id: None,
            agent_pane_id: None,
            listening_ports: Vec::new(),
            listening_ports_updated_at_unix_ms: None,
            socket_agent_activity: None,
            pane_agent_activity: HashMap::new(),
            pane_explicit_observation: HashMap::new(),
            agent_activity: None,
            needs_attention: false,
            notified: false,
            notification_msg: None,
            notification_pane_id: None,
            pane_last_notified: std::collections::HashMap::new(),
            pane_turn: std::collections::HashMap::new(),
            workspace_action: None,
            discovery_cwd: None,
            discovered_actions: Vec::new(),
            task_buttons: Vec::new(),
            tracking_data: None,
        }
    }

    fn add_tab_to_workspace(state: &mut AppState, ws_id: u32, mut tab: Tab) -> u32 {
        let tab_id = tab.id;
        let workspace = state
            .workspaces
            .iter_mut()
            .find(|ws| ws.id == ws_id)
            .expect("workspace should exist");
        if workspace.active_tab == 0 {
            workspace.active_tab = tab_id;
        }
        tab.focused_pane_id = tab.focused_pane_id.max(1);
        workspace.tabs.push(tab);
        tab_id
    }

    fn state_with_attention_tabs() -> AppState {
        let mut state = AppState::new();
        let ws_id = state.workspaces[0].id;

        let mut editor = stub_tab(11);
        editor.name = "editor".into();
        editor.needs_attention = true;
        editor.notification_msg = Some("Build done".into());
        editor.focused_pane_id = 111;
        editor.panes = Box::new(PaneNode::Stub { pane_id: 111 });
        add_tab_to_workspace(&mut state, ws_id, editor);

        let mut logs = stub_tab(12);
        logs.name = "logs".into();
        logs.focused_pane_id = 222;
        logs.panes = Box::new(PaneNode::Stub { pane_id: 222 });
        let logs_id = add_tab_to_workspace(&mut state, ws_id, logs);
        state.workspaces[0].active_tab = logs_id;

        state.event_store.emit(
            "alert_raised",
            serde_json::json!({
                "tab_id": 11,
                "tab_name": "editor",
                "message": "Build done",
                "source": "socket-agent-status",
            }),
        );

        state
    }

    #[test]
    fn saved_view_roundtrips_through_json() {
        let view = SavedView {
            name: "Recent Alerts".into(),
            preset: ViewPreset::RecentAlerts,
            limit: Some(25),
        };

        let json = serde_json::to_string(&view).expect("serialize view");
        let restored: SavedView = serde_json::from_str(&json).expect("deserialize view");
        assert_eq!(restored, view);
    }

    #[test]
    fn save_store_roundtrips_views() {
        let path = temp_path();
        let store = ViewStore {
            version: VIEW_SCHEMA_VERSION,
            views: vec![SavedView {
                name: "Ports".into(),
                preset: ViewPreset::ListeningPorts,
                limit: None,
            }],
        };

        save_store(&path, &store).expect("save view store");
        let restored = load_store(&path).expect("load view store");
        assert_eq!(restored.views.len(), 1);
        assert_eq!(restored.views[0].name, "Ports");
    }

    #[test]
    fn upsert_view_replaces_case_insensitive_match() {
        let mut views = vec![SavedView {
            name: "Ports".into(),
            preset: ViewPreset::ListeningPorts,
            limit: None,
        }];

        upsert_view(
            &mut views,
            SavedView {
                name: "ports".into(),
                preset: ViewPreset::RecentAlerts,
                limit: Some(10),
            },
        );

        assert_eq!(views.len(), 1);
        assert_eq!(views[0].preset, ViewPreset::RecentAlerts);
        assert_eq!(views[0].limit, Some(10));
    }

    #[test]
    fn sort_views_orders_case_insensitively() {
        let mut views = vec![
            SavedView {
                name: "recent alerts".into(),
                preset: ViewPreset::RecentAlerts,
                limit: Some(10),
            },
            SavedView {
                name: "Agent Activity".into(),
                preset: ViewPreset::AgentActivity,
                limit: None,
            },
        ];

        sort_views(&mut views);
        assert_eq!(views[0].name, "Agent Activity");
        assert_eq!(views[1].name, "recent alerts");
    }

    #[test]
    fn recent_alerts_default_limit_is_applied() {
        let view = SavedView {
            name: "Recent Alerts".into(),
            preset: ViewPreset::RecentAlerts,
            limit: None,
        };
        assert_eq!(view.effective_limit(), DEFAULT_RECENT_ALERT_LIMIT);
    }

    #[test]
    fn test_project_config_pins_dashboard_views() {
        let global = vec![SavedView {
            name: "Workspace Health".into(),
            preset: ViewPreset::WorkspaceHealth,
            limit: None,
        }];
        let project = crate::project_config::ProjectConfig {
            workspace: crate::project_config::ProjectWorkspaceConfig {
                pinned_dashboard_views: vec![ViewPreset::RecentAlerts, ViewPreset::ListeningPorts],
            },
            ..Default::default()
        };

        let views = crate::project_config::pinned_dashboard_views(global, Some(&project));

        assert_eq!(views.len(), 3);
        assert_eq!(views[0].preset, ViewPreset::RecentAlerts);
        assert_eq!(views[0].name, "Recent Alerts");
        assert_eq!(views[1].preset, ViewPreset::ListeningPorts);
        assert_eq!(views[1].name, "Listening Ports");
        assert_eq!(views[2].preset, ViewPreset::WorkspaceHealth);
    }

    #[test]
    fn public_view_api_roundtrips_through_xdg_config_home() {
        let config_home = temp_config_home("public-api");
        let env = ScopedEnv::set(&[("XDG_CONFIG_HOME", config_home.clone().into_os_string())]);

        save(SavedView {
            name: "Recent Alerts".into(),
            preset: ViewPreset::RecentAlerts,
            limit: None,
        })
        .expect("first saved view should persist");
        save(SavedView {
            name: "Workspace Health".into(),
            preset: ViewPreset::WorkspaceHealth,
            limit: None,
        })
        .expect("second saved view should persist");
        save(SavedView {
            name: "recent alerts".into(),
            preset: ViewPreset::AgentActivity,
            limit: Some(3),
        })
        .expect("case-insensitive overwrite should persist");

        let listed = list().expect("saved views should load");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "recent alerts");
        assert_eq!(listed[0].preset, ViewPreset::AgentActivity);
        assert_eq!(listed[1].name, "Workspace Health");

        let found = find("RECENT ALERTS").expect("view lookup should be case-insensitive");
        assert_eq!(found.preset, ViewPreset::AgentActivity);
        assert_eq!(found.limit, Some(3));
        assert_eq!(found.effective_limit(), 3);

        let deleted = delete("workspace health").expect("existing saved view should delete");
        assert!(deleted);
        assert!(!delete("missing").expect("missing saved view delete should not error"));

        let listed_after_delete = list().expect("remaining saved views should load");
        assert_eq!(listed_after_delete.len(), 1);
        assert_eq!(listed_after_delete[0].name, "recent alerts");

        drop(env);
        let _ = std::fs::remove_dir_all(config_home);
    }

    #[test]
    fn public_view_api_observes_external_edits_after_cache_warm() {
        let config_home = temp_config_home("external-edit");
        let views_path = config_views_path(&config_home);
        let env = ScopedEnv::set(&[("XDG_CONFIG_HOME", config_home.clone().into_os_string())]);

        save(SavedView {
            name: "Recent Alerts".into(),
            preset: ViewPreset::RecentAlerts,
            limit: Some(5),
        })
        .expect("initial view should persist");
        let warmed = list().expect("cache should warm");
        assert_eq!(warmed.len(), 1);

        write_view_fixture(
            &views_path,
            vec![
                SavedView {
                    name: "Ports".into(),
                    preset: ViewPreset::ListeningPorts,
                    limit: None,
                },
                SavedView {
                    name: "Workspace Health".into(),
                    preset: ViewPreset::WorkspaceHealth,
                    limit: Some(17),
                },
            ],
        );

        let refreshed = list().expect("external edit should invalidate cache");
        assert_eq!(refreshed.len(), 2);
        assert_eq!(refreshed[0].name, "Ports");
        assert_eq!(refreshed[1].name, "Workspace Health");
        assert_eq!(refreshed[1].limit, Some(17));

        drop(env);
        let _ = std::fs::remove_dir_all(config_home);
    }

    #[test]
    fn public_view_api_save_preserves_external_changes() {
        let config_home = temp_config_home("external-save");
        let views_path = config_views_path(&config_home);
        let env = ScopedEnv::set(&[("XDG_CONFIG_HOME", config_home.clone().into_os_string())]);

        save(SavedView {
            name: "Recent Alerts".into(),
            preset: ViewPreset::RecentAlerts,
            limit: Some(5),
        })
        .expect("initial view should persist");
        let warmed = list().expect("cache should warm");
        assert_eq!(warmed.len(), 1);

        write_view_fixture(
            &views_path,
            vec![
                SavedView {
                    name: "Listening Ports".into(),
                    preset: ViewPreset::ListeningPorts,
                    limit: None,
                },
                SavedView {
                    name: "Workspace Health".into(),
                    preset: ViewPreset::WorkspaceHealth,
                    limit: Some(11),
                },
            ],
        );

        save(SavedView {
            name: "Agent Activity".into(),
            preset: ViewPreset::AgentActivity,
            limit: Some(3),
        })
        .expect("save should merge with externally updated store");

        let merged = list().expect("merged store should load");
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].name, "Agent Activity");
        assert_eq!(merged[1].name, "Listening Ports");
        assert_eq!(merged[2].name, "Workspace Health");
        assert_eq!(merged[2].limit, Some(11));

        drop(env);
        let _ = std::fs::remove_dir_all(config_home);
    }

    #[test]
    fn agent_activity_fallback_only_includes_agent_running_tabs() {
        let mut tab = stub_tab(1);
        assert_eq!(agent_activity_fallback_subtitle(&tab), None);

        tab.agent_running = true;
        assert_eq!(
            agent_activity_fallback_subtitle(&tab),
            Some("running".to_string())
        );

        tab.agent_activity = Some(AgentActivity {
            state: AgentActivityState::Running,
            text: "working".to_string(),
            source: Some("copilot".to_string()),
            origin: AgentActivityOrigin::Socket,
            updated_at: Instant::now(),
            observed_at_unix_ms: crate::events::unix_time_ms(),
        });
        assert_eq!(agent_activity_fallback_subtitle(&tab), None);
    }

    #[test]
    fn test_attention_list_view_includes_flagged_panes() {
        let mut state = state_with_attention_tabs();
        state.set_socket_notification(11, "Build done".to_string());
        let tab = state.find_tab_mut(11).expect("editor tab");
        tab.needs_attention = false;
        tab.notification_msg = None;

        let rows = attention_list_entries(&state);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tab_id, 11);
        assert_eq!(rows[0].pane_id, 111);
        assert_eq!(rows[0].title, "default / editor · pane 111");
        assert!(rows[0].subtitle.contains("Unknown reason"));
        assert!(rows[0].subtitle.contains("system"));
        assert_eq!(
            ViewPreset::CurrentlyFlaggedPanes.default_name(),
            "Currently Flagged Panes"
        );
    }

    #[test]
    fn attention_list_shows_exact_identity_and_canonical_evidence() {
        let mut state = state_with_attention_tabs();
        state.workspaces[0].repo_root = Some("/src/taarof".into());
        state.workspaces[0].working_tree_path = Some("/worktrees/TAAROF-27".into());
        state.workspaces[0].host_config_name = Some("workstation".into());
        let tab = state.find_tab_mut(11).expect("editor tab");
        tab.set_pane_agent_activity(
            111,
            AgentActivity::socket(
                AgentActivityState::WaitingInput,
                "waiting for input",
                Some("codex".into()),
            ),
        );

        let rows = attention_list_entries(&state);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pane_id, 111);
        assert!(rows[0].title.contains("editor"));
        assert!(rows[0].title.contains("pane 111"));
        for expected in [
            "Waiting for input",
            "codex",
            "workstation",
            "/src/taarof",
            "/worktrees/TAAROF-27",
            "provider_explicit",
            "socket",
            "verified",
        ] {
            assert!(rows[0].subtitle.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn attention_list_order_ignores_alert_event_volume() {
        let mut state = state_with_attention_tabs();
        let logs = state.find_tab_mut(12).expect("logs tab");
        logs.needs_attention = true;
        logs.notification_msg = Some("Logs ready".into());
        state.event_store.emit(
            "alert_raised",
            serde_json::json!({
                "tab_id": 12,
                "tab_name": "logs",
                "message": "Logs ready",
                "source": "socket-agent-status",
            }),
        );

        assert_eq!(
            attention_list_entries(&state)
                .iter()
                .map(|entry| entry.tab_id)
                .collect::<Vec<_>>(),
            vec![11, 12]
        );
    }

    #[test]
    fn keyboard_attention_target_uses_the_same_exact_pane_as_the_first_row() {
        let mut state = state_with_attention_tabs();
        let tab = state.find_tab_mut(11).expect("editor tab");
        tab.notification_pane_id = Some(111);

        let first = attention_list_entries(&state).remove(0);
        assert_eq!(
            first_attention_target(&state),
            Some((first.tab_id, first.pane_id))
        );
    }

    #[test]
    fn test_attention_list_view_jump_action_focuses_pane() {
        let mut state = state_with_attention_tabs();
        let ws_id = state.workspaces[0].id;
        let target = attention_list_entries(&state)
            .into_iter()
            .next()
            .expect("flagged pane should exist");

        assert_ne!(state.workspaces[0].active_tab, target.tab_id);
        assert!(jump_to_attention_target(
            &mut state,
            target.tab_id,
            target.pane_id
        ));

        assert_eq!(state.active_workspace, ws_id);
        assert_eq!(state.workspaces[0].active_tab, target.tab_id);
        let tab = state
            .find_tab(target.tab_id)
            .expect("tab should still exist")
            .1;
        assert_eq!(tab.focused_pane_id, target.pane_id);
        assert!(!tab.needs_attention);
        assert!(tab.notification_msg.is_none());
    }

    #[test]
    fn attention_jump_refuses_a_removed_pane_without_switching_tabs() {
        let mut state = state_with_attention_tabs();
        let original_tab = state.workspaces[0].active_tab;

        assert!(!jump_to_attention_target(&mut state, 11, 999));
        assert_eq!(state.workspaces[0].active_tab, original_tab);
    }

    #[test]
    fn test_failing_test_runs_view_shows_failing_test_command() {
        let mut state = AppState::new();
        state.event_store.emit(
            "command_exited",
            serde_json::json!({
                "tab_id": 7,
                "tab_name": "tests",
                "pane_id": 3,
                "command": "cargo test failing_test",
                "exit_code": 101,
                "snippet": "running 1 test\nfailing_test ... FAILED",
            }),
        );

        let entries = failing_test_run_entries(&state);
        assert_eq!(entries.len(), 1);
        assert!(!ViewPreset::FailingTestRuns.requires_unimplemented_data());
        assert_eq!(entries[0].tab_id, 7);
        assert_eq!(entries[0].pane_id, 3);
        assert_eq!(entries[0].title, "tests");
        assert!(entries[0].subtitle.contains("cargo test failing_test"));
        assert!(entries[0].subtitle.contains("exit 101"));
        assert!(entries[0].subtitle.contains("FAILED"));
    }

    #[test]
    fn test_failing_test_runs_view_ignores_non_test_commands() {
        let mut state = AppState::new();
        state.event_store.emit(
            "command_exited",
            serde_json::json!({
                "tab_id": 7,
                "tab_name": "shell",
                "pane_id": 0,
                "command": "false",
                "exit_code": 1,
                "snippet": "command failed",
            }),
        );

        let entries = failing_test_run_entries(&state);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_failing_test_runs_view_matches_generic_test_commands() {
        let mut state = AppState::new();
        state.event_store.emit(
            "command_exited",
            serde_json::json!({
                "tab_id": 8,
                "tab_name": "ci",
                "pane_id": 2,
                "command": "./test smoke",
                "exit_code": 1,
                "snippet": "smoke failed",
            }),
        );
        state.event_store.emit(
            "command_exited",
            serde_json::json!({
                "tab_id": 9,
                "tab_name": "mise",
                "pane_id": 4,
                "command": "mise run deploy-contest",
                "exit_code": 1,
                "snippet": "should not match",
            }),
        );

        let entries = failing_test_run_entries(&state);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "ci");
        assert!(entries[0].subtitle.contains("./test smoke"));
    }

    #[test]
    fn test_failing_test_runs_view_retention_bounded() {
        let mut state = AppState::new();
        let now = crate::events::unix_time_ms();
        let stale = now.saturating_sub(25 * 60 * 60 * 1000);
        state.event_store.emit_at(
            stale,
            "command_exited",
            serde_json::json!({
                "tab_id": 1,
                "tab_name": "old",
                "pane_id": 0,
                "command": "cargo test old_case",
                "exit_code": 101,
                "snippet": "old failure",
            }),
        );

        for index in 0..55 {
            state.event_store.emit_at(
                now + index,
                "command_exited",
                serde_json::json!({
                    "tab_id": index + 10,
                    "tab_name": format!("test-{index}"),
                    "pane_id": 0,
                    "command": format!("cargo test case_{index}"),
                    "exit_code": 101,
                    "snippet": format!("failure {index}"),
                }),
            );
        }

        let entries = failing_test_run_entries(&state);
        assert_eq!(entries.len(), 50);
        assert_eq!(entries[0].title, "test-54");
        assert_eq!(entries[49].title, "test-5");
        assert!(entries
            .iter()
            .all(|entry| !entry.subtitle.contains("old_case")));
    }

    /// Guards that all currently-shipped presets report a real data source.
    /// `FailingTestRuns` is backed by `command_exited` events emitted from
    /// `terminal/process.rs`; `CurrentlyFlaggedPanes` is backed by
    /// `needs_attention`/`alert_raised` events emitted from `app_runtime.rs`,
    /// `terminal/signals.rs`, and `socket.rs`.  Both pipelines are live; the
    /// hook must stay `false` for every shipped preset.
    #[test]
    fn test_all_shipped_presets_return_false_for_requires_unimplemented_data() {
        for preset in [
            ViewPreset::AgentActivity,
            ViewPreset::WorkspaceHealth,
            ViewPreset::ListeningPorts,
            ViewPreset::RecentAlerts,
            ViewPreset::CurrentlyFlaggedPanes,
            ViewPreset::FailingTestRuns,
        ] {
            assert!(
                !preset.requires_unimplemented_data(),
                "{preset:?} has a live data pipeline and must return false (issue #37)"
            );
        }
    }

    /// Specifically guards the two presets called out in issue #37.
    /// Both are wired: `FailingTestRuns` via `command_exited` events from
    /// `terminal/process.rs`; `CurrentlyFlaggedPanes` via `alert_raised`
    /// events from `app_runtime.rs`, `socket.rs`, and `terminal/signals.rs`.
    #[test]
    fn test_issue_37_presets_are_wired_to_live_data_sources() {
        assert!(
            !ViewPreset::FailingTestRuns.requires_unimplemented_data(),
            "FailingTestRuns is backed by command_exited events emitted in terminal/process.rs"
        );
        assert!(
            !ViewPreset::CurrentlyFlaggedPanes.requires_unimplemented_data(),
            "CurrentlyFlaggedPanes is backed by alert_raised events emitted in app_runtime.rs/socket.rs"
        );
    }

    /// The placeholder returned by `not_yet_wired_placeholder` must always
    /// communicate that the view is not yet wired, so the user understands
    /// no data will appear.  This is a safety net for any future preset that
    /// sets `requires_unimplemented_data() == true`.
    #[test]
    fn test_not_yet_wired_placeholder_communicates_missing_pipeline() {
        // The same generic message applies to every preset; verify its wording.
        let message = ViewPreset::AgentActivity.not_yet_wired_placeholder();
        assert!(!message.is_empty(), "placeholder must not be empty");
        assert!(
            message.contains("not yet wired") || message.contains("not wired"),
            "placeholder must communicate that the view is not yet wired: {message}"
        );
    }
}
