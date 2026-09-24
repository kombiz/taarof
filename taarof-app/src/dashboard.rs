use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use crate::probe::{ProbeSnapshot, ProbeState};
use crate::tmux::TmuxTarget;

/// A tmux session that was explicitly detached by the user.
#[derive(Clone, Debug)]
pub struct DetachedSession {
    pub session_name: String,
    pub host: String,
    pub workspace: String,
    pub target: TmuxTarget,
    pub detached_at: Instant,
    pub last_command: Option<String>,
    pub finished: bool,
}

/// Serializable form for session.json persistence.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SavedDetachedSession {
    pub session_name: String,
    pub host: String,
    pub workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub ssh_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub last_command: Option<String>,
}

impl DetachedSession {
    pub fn matches_target(&self, session_name: &str, target: &TmuxTarget) -> bool {
        self.session_name == session_name && &self.target == target
    }

    pub fn to_saved(&self) -> SavedDetachedSession {
        SavedDetachedSession {
            session_name: self.session_name.clone(),
            host: self.host.clone(),
            workspace: self.workspace.clone(),
            ssh_target: self.target.ssh_target_string(),
            last_command: self.last_command.clone(),
        }
    }

    pub fn from_saved(saved: &SavedDetachedSession) -> Self {
        let target = match &saved.ssh_target {
            Some(ssh) => TmuxTarget::Remote {
                ssh_target: ssh.clone(),
            },
            None => TmuxTarget::Local,
        };
        Self {
            session_name: saved.session_name.clone(),
            host: saved.host.clone(),
            workspace: saved.workspace.clone(),
            target,
            detached_at: Instant::now(),
            last_command: saved.last_command.clone(),
            finished: false,
        }
    }
}

pub(crate) type DetachedSessionCommandKey = (TmuxTarget, String);

pub(crate) fn detached_session_command_key(
    target: &TmuxTarget,
    session_name: &str,
) -> DetachedSessionCommandKey {
    (target.clone(), session_name.to_string())
}

#[derive(Debug, PartialEq, Eq)]
struct DetachedFinishNotification {
    summary: String,
    body: String,
}

fn detached_finish_notification(session: &DetachedSession) -> DetachedFinishNotification {
    let command = session
        .last_command
        .as_deref()
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .and_then(|command| command.rsplit('/').next())
        .unwrap_or("Command");
    let command = command
        .chars()
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + &command[first.len_utf8()..])
        .unwrap_or_else(|| "Command".to_string());
    let workspace = session.workspace.trim();
    let summary = if workspace.is_empty() {
        format!("{command} finished")
    } else {
        format!("{command} finished — {workspace}")
    };
    let session_name = session
        .session_name
        .strip_prefix("taarof--")
        .unwrap_or(&session.session_name);
    DetachedFinishNotification {
        summary,
        body: format!("Session {session_name} · {}", session.host),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionStatus {
    Running,
    Idle,
    Detached,
    Finished,
}

#[derive(Clone, Debug)]
pub struct DashboardSession {
    pub name: String,
    pub host: String,
    pub target: TmuxTarget,
    pub status: SessionStatus,
    pub command: Option<String>,
    pub started: Option<String>,
    pub pid: Option<u32>,
    pub output_preview: Vec<String>,
    pub is_detached: bool,
    pub is_taarof_managed: bool,
}

#[derive(Clone, Debug)]
pub struct DashboardHost {
    pub name: String,
    pub cpu_percent: Option<f64>,
    pub memory_percent: Option<f64>,
    pub session_count: u32,
    pub probe: ProbeSnapshot<()>,
}

pub(crate) type TmuxSessionSummary = (String, u64, bool, u32);
pub(crate) type LiveTmuxSessions = [(TmuxTarget, Vec<TmuxSessionSummary>)];

#[derive(Clone, Debug)]
pub(crate) struct DashboardPollContext {
    pub generation: u64,
    pub target_lifecycle_epochs: std::collections::HashMap<TmuxTarget, u64>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DashboardPollTracker {
    next_generation: u64,
    target_lifecycle_epochs: std::collections::HashMap<TmuxTarget, u64>,
    target_last_result_generation: std::collections::HashMap<TmuxTarget, u64>,
    last_completed_generation: u64,
}

impl DashboardPollTracker {
    pub fn begin(&mut self, targets: &[TmuxTarget]) -> DashboardPollContext {
        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = self.next_generation;
        DashboardPollContext {
            generation,
            target_lifecycle_epochs: targets
                .iter()
                .map(|target| {
                    (
                        target.clone(),
                        self.target_lifecycle_epochs
                            .get(target)
                            .copied()
                            .unwrap_or_default(),
                    )
                })
                .collect(),
        }
    }

    pub fn invalidate_target(&mut self, target: &TmuxTarget) {
        let epoch = self
            .target_lifecycle_epochs
            .entry(target.clone())
            .or_default();
        *epoch = epoch.wrapping_add(1);
    }

    pub fn result_is_current(&self, context: &DashboardPollContext, target: &TmuxTarget) -> bool {
        let captured_epoch = context.target_lifecycle_epochs.get(target);
        let current_epoch = self
            .target_lifecycle_epochs
            .get(target)
            .copied()
            .unwrap_or_default();
        captured_epoch.is_some_and(|captured| *captured == current_epoch)
            && self
                .target_last_result_generation
                .get(target)
                .is_none_or(|last| context.generation > *last)
    }

    pub fn record_result(&mut self, context: &DashboardPollContext, target: &TmuxTarget) {
        self.target_last_result_generation
            .insert(target.clone(), context.generation);
    }

    pub fn poll_is_newer_than_last_completion(&self, context: &DashboardPollContext) -> bool {
        context.generation > self.last_completed_generation
    }

    pub fn record_completion(&mut self, context: &DashboardPollContext) {
        self.last_completed_generation = self.last_completed_generation.max(context.generation);
    }
}

#[derive(Clone, Debug, Default)]
pub struct DashboardState {
    pub sessions: Vec<DashboardSession>,
    pub hosts: Vec<DashboardHost>,
    pub probe: ProbeSnapshot<()>,
}

impl DashboardState {
    pub fn apply_success(
        &mut self,
        sessions: Vec<DashboardSession>,
        hosts: Vec<DashboardHost>,
    ) -> crate::probe::ProbeTransition {
        self.sessions = sessions;
        self.hosts = hosts;
        self.probe.record_success(())
    }

    pub fn apply_failure(&mut self, error: impl Into<String>) -> crate::probe::ProbeTransition {
        self.probe.record_failure(error)
    }
}

use crate::workspace::Workspace;

pub fn aggregate_dashboard_state(
    workspaces: &[Workspace],
    detached: &[DetachedSession],
    tmux_sessions: &LiveTmuxSessions,
    capture_outputs: &std::collections::HashMap<String, Vec<String>>,
    session_prefix: &str,
) -> DashboardState {
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut sessions = Vec::new();
    let mut host_map: std::collections::HashMap<String, DashboardHost> =
        std::collections::HashMap::new();
    let mut seen_sessions = std::collections::HashSet::new();

    for (target, tmux_list) in tmux_sessions {
        let host = match target {
            TmuxTarget::Local => "localhost".to_string(),
            TmuxTarget::Remote { ssh_target } => ssh_target.clone(),
        };
        let host_entry = host_map.entry(host.clone()).or_insert_with(|| {
            let probe = workspaces
                .iter()
                .find(|ws| {
                    ws.tabs.iter().any(|tab| {
                        tab.panes.leaves().into_iter().any(|leaf| {
                            leaf.tmux_backing.as_ref().is_some_and(|backing| match &backing.target {
                                TmuxTarget::Local => matches!(target, TmuxTarget::Local),
                                TmuxTarget::Remote { ssh_target } => {
                                    matches!(target, TmuxTarget::Remote { ssh_target: current } if current == ssh_target)
                                }
                            })
                        })
                    })
                })
                .map(|ws| ws.host_status.clone())
                .unwrap_or_default();
            DashboardHost {
                name: host.clone(),
                cpu_percent: probe.value().map(|s| s.cpu_load_percent),
                memory_percent: probe.value().map(|s| s.memory_used_percent),
                session_count: 0,
                probe: probe.unit_meta(),
            }
        });
        host_entry.session_count = tmux_list.len() as u32;

        for (name, created, attached, _windows) in tmux_list {
            let is_taarof_managed = name.starts_with(session_prefix);
            let is_detached = detached.iter().any(|d| d.matches_target(name, target));
            let ds = detached.iter().find(|d| d.matches_target(name, target));

            let status = if is_detached {
                if ds.is_some_and(|d| d.finished) {
                    SessionStatus::Finished
                } else {
                    SessionStatus::Detached
                }
            } else if *attached {
                SessionStatus::Running
            } else {
                SessionStatus::Idle
            };

            let age_secs = now_epoch.saturating_sub(*created);
            let started = if age_secs < 60 {
                Some(format!("{}s", age_secs))
            } else if age_secs < 3600 {
                Some(format!("{}m", age_secs / 60))
            } else if age_secs < 86400 {
                Some(format!("{}h {}m", age_secs / 3600, (age_secs % 3600) / 60))
            } else {
                Some(format!("{}d", age_secs / 86400))
            };

            let output_preview = capture_outputs.get(name).cloned().unwrap_or_default();
            seen_sessions.insert((target.clone(), name.clone()));

            sessions.push(DashboardSession {
                name: name.clone(),
                host: host.clone(),
                target: target.clone(),
                status,
                command: ds.and_then(|d| d.last_command.clone()),
                started,
                pid: None,
                output_preview,
                is_detached,
                is_taarof_managed,
            });
        }
    }

    for ds in detached {
        if seen_sessions.contains(&(ds.target.clone(), ds.session_name.clone())) {
            continue;
        }

        let host_entry = host_map
            .entry(ds.host.clone())
            .or_insert_with(|| DashboardHost {
                name: ds.host.clone(),
                cpu_percent: None,
                memory_percent: None,
                session_count: 0,
                probe: ProbeSnapshot::default(),
            });
        host_entry.session_count += 1;

        sessions.push(DashboardSession {
            name: ds.session_name.clone(),
            host: ds.host.clone(),
            target: ds.target.clone(),
            status: if ds.finished {
                SessionStatus::Finished
            } else {
                SessionStatus::Detached
            },
            command: ds.last_command.clone(),
            started: None,
            pid: None,
            output_preview: capture_outputs
                .get(&ds.session_name)
                .cloned()
                .unwrap_or_default(),
            is_detached: true,
            is_taarof_managed: ds.session_name.starts_with(session_prefix),
        });
    }

    let mut state = DashboardState::default();
    let _ = state.apply_success(sessions, host_map.into_values().collect());
    state
}

use gtk::prelude::*;

const DASHBOARD_SESSION_LIST_NAME: &str = "dashboard-session-list";
const DASHBOARD_VIEW_LIST_NAME: &str = "dashboard-view-list";
const DASHBOARD_DETAIL_NAME: &str = "dashboard-detail";
const DASHBOARD_VIEWS_TTL: std::time::Duration = std::time::Duration::from_secs(5);
/// A failed read is visible immediately, but repeated redraws must not hammer
/// a saturated worker queue. It remains a failure and gets a fresh attempt
/// shortly afterwards or immediately after invalidation.
const DASHBOARD_VIEWS_ERROR_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// The dashboard may be refreshed from a one-second poll.  Project discovery
/// walks the filesystem and invokes Git, and the saved-view store performs
/// filesystem I/O, so GTK only renders this immutable snapshot.  The worker
/// receives path strings, never `AppState` or GTK objects.
#[derive(Clone, Default)]
struct DashboardViewsSnapshot {
    candidates: Vec<String>,
    views: Vec<crate::views::SavedView>,
    has_pinned_views: bool,
    error: Option<String>,
    fetched_at: Option<Instant>,
}

#[derive(Default)]
struct DashboardViewsCache {
    snapshot: Option<DashboardViewsSnapshot>,
    in_flight: Option<DashboardViewsInFlight>,
    generation: u64,
}

/// The precise owner of a dashboard view worker.  Candidate paths alone are
/// insufficient: invalidating while the same workspace remains active must
/// permit a replacement request instead of coalescing behind obsolete I/O.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DashboardViewsInFlight {
    candidates: Vec<String>,
    generation: u64,
}

thread_local! {
    static DASHBOARD_VIEWS_CACHE: RefCell<DashboardViewsCache> = RefCell::new(DashboardViewsCache::default());
}

/// Invalidate cached project configuration and saved-view data after an
/// in-process view mutation.  The generation also makes any already-running
/// worker response stale instead of letting it overwrite the new view list.
pub(crate) fn invalidate_dashboard_views() {
    DASHBOARD_VIEWS_CACHE.with(|cache| {
        invalidate_dashboard_views_cache(&mut cache.borrow_mut());
    });
}

fn invalidate_dashboard_views_cache(cache: &mut DashboardViewsCache) {
    cache.generation = cache.generation.wrapping_add(1);
    cache.snapshot = None;
    // A newly-invalidated identical candidate set must be able to submit a
    // replacement worker immediately. The old completion carries its own
    // generation and therefore cannot clear or overwrite this replacement.
    cache.in_flight = None;
}

fn dashboard_project_candidates(state: &crate::AppState) -> Vec<String> {
    let Some(workspace) = state.active_ws() else {
        return Vec::new();
    };
    let context_tab = state
        .active_tab()
        .filter(|tab| tab.kind == crate::TabKind::Terminal)
        .or_else(|| {
            workspace.last_active_tab.and_then(|tab_id| {
                workspace
                    .tabs
                    .iter()
                    .find(|tab| tab.id == tab_id && tab.kind == crate::TabKind::Terminal)
            })
        })
        .or_else(|| {
            workspace
                .tabs
                .iter()
                .find(|tab| tab.kind == crate::TabKind::Terminal)
        });
    let mut candidates = Vec::new();
    if let Some(tab) = context_tab {
        if let Some(cwd) = tab
            .panes
            .leaf(tab.focused_pane_id)
            .or_else(|| tab.panes.first_leaf())
            .and_then(|leaf| leaf.local_cwd())
        {
            candidates.push(cwd);
        }
        if let Some(cwd) = tab
            .discovery_cwd
            .as_deref()
            .filter(|cwd| !cwd.trim().is_empty())
        {
            candidates.push(cwd.to_string());
        }
    }
    candidates.extend(
        [
            workspace.working_tree_path.as_deref(),
            workspace.repo_root.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|cwd| !cwd.trim().is_empty())
        .map(str::to_string),
    );
    candidates.sort();
    candidates.dedup();
    candidates
}

fn load_dashboard_views(
    candidates: Vec<String>,
) -> Result<(Vec<crate::views::SavedView>, bool), String> {
    let project = candidates
        .iter()
        .find_map(|cwd| crate::project_config::load_for_workspace_cwd(cwd));
    let has_pinned_views = project
        .as_ref()
        .is_some_and(|config| !config.workspace.pinned_dashboard_views.is_empty());
    let views = crate::views::list().map_err(|error| error.to_string())?;
    Ok((
        crate::project_config::pinned_dashboard_views(views, project.as_ref()),
        has_pinned_views,
    ))
}

fn dashboard_views_snapshot(state: &crate::AppState) -> DashboardViewsSnapshot {
    let candidates = dashboard_project_candidates(state);
    DASHBOARD_VIEWS_CACHE.with(|cache| {
        cache
            .borrow()
            .snapshot
            .as_ref()
            .filter(|snapshot| snapshot.candidates == candidates)
            .cloned()
            .unwrap_or(DashboardViewsSnapshot {
                candidates,
                ..DashboardViewsSnapshot::default()
            })
    })
}

fn cached_dashboard_view(state: &crate::AppState, name: &str) -> Option<crate::views::SavedView> {
    dashboard_views_snapshot(state)
        .views
        .into_iter()
        .find(|view| view.name.eq_ignore_ascii_case(name))
}

fn dashboard_views_loading(candidates: &[String]) -> bool {
    DASHBOARD_VIEWS_CACHE.with(|cache| {
        cache
            .borrow()
            .in_flight
            .as_ref()
            .is_some_and(|request| request.candidates == candidates)
    })
}

fn dashboard_views_worker_key(candidates: &[String], generation: u64) -> String {
    format!("dashboard-views:{generation}:{}", candidates.join("\u{1f}"))
}

fn claim_dashboard_views_refresh(
    cache: &mut DashboardViewsCache,
    candidates: &[String],
) -> Option<DashboardViewsInFlight> {
    if cache.snapshot.as_ref().is_some_and(|snapshot| {
        snapshot.candidates == candidates
            && snapshot.fetched_at.is_some_and(|fetched_at| {
                fetched_at.elapsed()
                    < if snapshot.error.is_some() {
                        DASHBOARD_VIEWS_ERROR_RETRY
                    } else {
                        DASHBOARD_VIEWS_TTL
                    }
            })
    }) || cache
        .in_flight
        .as_ref()
        .is_some_and(|request| request.candidates == candidates)
    {
        return None;
    }
    let request = DashboardViewsInFlight {
        candidates: candidates.to_vec(),
        generation: cache.generation,
    };
    cache.in_flight = Some(request.clone());
    Some(request)
}

/// Release only the exact request that completed. An invalidated old worker
/// therefore cannot clear a same-candidates replacement request.
fn release_dashboard_views_request(
    cache: &mut DashboardViewsCache,
    request: &DashboardViewsInFlight,
) -> bool {
    if cache.in_flight.as_ref() == Some(request) {
        cache.in_flight = None;
        true
    } else {
        false
    }
}

fn ensure_dashboard_views(paned: &gtk::Paned, state: &Rc<RefCell<crate::AppState>>) {
    let candidates = {
        let state = state.borrow();
        dashboard_project_candidates(&state)
    };
    let Some(request) = DASHBOARD_VIEWS_CACHE
        .with(|cache| claim_dashboard_views_refresh(&mut cache.borrow_mut(), &candidates))
    else {
        return;
    };
    let generation = request.generation;
    // Include the cache generation in the global worker key.  Otherwise an
    // invalidate-during-flight with the same candidate paths would coalesce
    // with an obsolete worker and leave the new generation permanently busy.
    let key = dashboard_views_worker_key(&candidates, generation);
    let worker_candidates = candidates.clone();
    let paned_for_apply = paned.clone();
    let state_for_apply = state.clone();
    let candidates_for_apply = candidates.clone();
    let submission = crate::git::spawn_async_result(
        key,
        move || load_dashboard_views(worker_candidates),
        move |result| {
            DASHBOARD_VIEWS_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                let request = DashboardViewsInFlight {
                    candidates: candidates_for_apply.clone(),
                    generation,
                };
                if release_dashboard_views_request(&mut cache, &request) {
                    // Always release the matching in-flight owner, including
                    // an error completion.  A stale generation never owns the
                    // replacement slot and is therefore unable to clear it.
                } else {
                    return;
                }
                cache.snapshot = Some(match result {
                    Ok((views, has_pinned_views)) => DashboardViewsSnapshot {
                        candidates: candidates_for_apply.clone(),
                        views,
                        has_pinned_views,
                        error: None,
                        fetched_at: Some(Instant::now()),
                    },
                    Err(error) => DashboardViewsSnapshot {
                        candidates: candidates_for_apply.clone(),
                        views: Vec::new(),
                        has_pinned_views: false,
                        error: Some(error),
                        // Failure has a short retry cooldown, rather than
                        // pretending to be a healthy views snapshot.
                        fetched_at: Some(Instant::now()),
                    },
                });
            });
            // A completed response is only rendered if this is still the
            // dashboard's current workspace context; otherwise refresh starts
            // a new generation for the new candidate paths.
            refresh_dashboard_list(&paned_for_apply, &state_for_apply);
        },
    );
    if !matches!(submission, crate::git::GitAsyncSubmission::Started) {
        DASHBOARD_VIEWS_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if release_dashboard_views_request(&mut cache, &request) {
                let error = match submission {
                    crate::git::GitAsyncSubmission::Coalesced => {
                        "Dashboard views worker was already registered without this request; retry shortly"
                    }
                    crate::git::GitAsyncSubmission::Saturated => {
                        "Git worker queue is busy; dashboard views will retry shortly"
                    }
                    crate::git::GitAsyncSubmission::Started => unreachable!(),
                };
                cache.snapshot = Some(DashboardViewsSnapshot {
                    candidates,
                    views: Vec::new(),
                    has_pinned_views: false,
                    error: Some(error.into()),
                    fetched_at: Some(Instant::now()),
                });
            }
        });
        refresh_dashboard_list(paned, state);
    }
}

pub fn build_dashboard_widget(
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
) -> gtk::Paned {
    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_position(300);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    paned.add_css_class("dashboard-paned");

    let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    sidebar_box.set_width_request(300);
    sidebar_box.set_margin_top(8);
    sidebar_box.set_margin_bottom(8);
    sidebar_box.set_margin_start(8);
    sidebar_box.set_margin_end(8);

    let sessions_header = gtk::Label::new(Some("Sessions"));
    sessions_header.add_css_class("heading");
    sessions_header.set_halign(gtk::Align::Start);
    sidebar_box.append(&sessions_header);

    let session_scroll = gtk::ScrolledWindow::new();
    session_scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    session_scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    session_scroll.set_vexpand(true);

    let session_list = gtk::ListBox::new();
    session_list.set_selection_mode(gtk::SelectionMode::Single);
    session_list.add_css_class("dashboard-session-list");
    session_list.set_widget_name(DASHBOARD_SESSION_LIST_NAME);
    session_scroll.set_child(Some(&session_list));
    sidebar_box.append(&session_scroll);

    let views_header = gtk::Label::new(Some("Saved Views"));
    views_header.add_css_class("heading");
    views_header.set_halign(gtk::Align::Start);
    sidebar_box.append(&views_header);

    let view_scroll = gtk::ScrolledWindow::new();
    view_scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    view_scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    view_scroll.set_min_content_height(140);

    let view_list = gtk::ListBox::new();
    view_list.set_selection_mode(gtk::SelectionMode::Single);
    view_list.add_css_class("dashboard-view-list");
    view_list.set_widget_name(DASHBOARD_VIEW_LIST_NAME);
    view_scroll.set_child(Some(&view_list));
    sidebar_box.append(&view_scroll);

    let detail_scroll = gtk::ScrolledWindow::new();
    detail_scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    detail_scroll.set_vscrollbar_policy(gtk::PolicyType::Automatic);

    let detail_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    detail_box.set_margin_top(16);
    detail_box.set_margin_bottom(16);
    detail_box.set_margin_start(16);
    detail_box.set_margin_end(16);
    detail_box.add_css_class("dashboard-detail");
    detail_box.set_widget_name(DASHBOARD_DETAIL_NAME);
    populate_dashboard_placeholder(&detail_box, "Select a session or saved view");
    detail_scroll.set_child(Some(&detail_box));

    paned.set_start_child(Some(&sidebar_box));
    paned.set_end_child(Some(&detail_scroll));

    let detail_box_for_session = detail_box.clone();
    let state_for_session = state.clone();
    let term_stack_for_session = term_stack.clone();
    let tab_list_for_session = tab_list.clone();
    let window_for_session = window.clone();
    let view_list_for_session = view_list.clone();
    session_list.connect_row_selected(move |_list, row| {
        let Some(row) = row else {
            return;
        };
        state_for_session.borrow_mut().selected_dashboard_view = None;
        view_list_for_session.unselect_all();
        let session_name = row.widget_name().to_string();
        populate_detail_panel(
            &detail_box_for_session,
            &state_for_session,
            &term_stack_for_session,
            &tab_list_for_session,
            &window_for_session,
            &session_name,
        );
    });

    let detail_box_for_view = detail_box.clone();
    let state_for_view = state.clone();
    let session_list_for_view = session_list.clone();
    let tab_list_for_view = tab_list.clone();
    let term_stack_for_view = term_stack.clone();
    view_list.connect_row_selected(move |_list, row| {
        let Some(row) = row else {
            return;
        };
        session_list_for_view.unselect_all();
        let view_name = row.widget_name().to_string();
        state_for_view.borrow_mut().selected_dashboard_view = Some(view_name.clone());
        if let Some(view) = cached_dashboard_view(&state_for_view.borrow(), &view_name) {
            crate::views::populate_saved_view_panel(
                &detail_box_for_view,
                &state_for_view,
                &tab_list_for_view,
                &term_stack_for_view,
                &view,
            );
        } else {
            populate_dashboard_placeholder(&detail_box_for_view, "Saved view not found");
        }
    });

    paned
}

fn populate_dashboard_placeholder(detail_box: &gtk::Box, message: &str) {
    while let Some(child) = detail_box.first_child() {
        detail_box.remove(&child);
    }

    let placeholder = gtk::Label::new(Some(message));
    placeholder.add_css_class("dim-label");
    placeholder.set_vexpand(true);
    placeholder.set_valign(gtk::Align::Center);
    placeholder.set_halign(gtk::Align::Start);
    detail_box.append(&placeholder);
}

fn probe_badge(text: &str, state: ProbeState) -> gtk::Label {
    let badge = gtk::Label::new(Some(text));
    badge.add_css_class("dashboard-status-badge");
    badge.add_css_class(match state {
        ProbeState::Ok => "status-running",
        ProbeState::Stale => "status-stale",
        ProbeState::Unknown => "status-unknown",
        ProbeState::Error => "status-error",
    });
    badge
}

fn host_probe_label(host: &DashboardHost) -> Option<String> {
    match host.probe.state {
        ProbeState::Stale => Some(format!("{} (stale)", host.name)),
        ProbeState::Error => Some(format!("{} (error)", host.name)),
        ProbeState::Unknown => Some(format!("{} (pending)", host.name)),
        ProbeState::Ok => None,
    }
}

fn populate_detail_panel(
    detail_box: &gtk::Box,
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    session_name: &str,
) {
    while let Some(child) = detail_box.first_child() {
        detail_box.remove(&child);
    }

    let st = state.borrow();
    let ds = st
        .dashboard_state
        .sessions
        .iter()
        .find(|s| s.name == session_name);
    let Some(session) = ds else {
        let label = gtk::Label::new(Some("Session not found"));
        label.add_css_class("dim-label");
        detail_box.append(&label);
        return;
    };

    detail_box.append(&build_session_header(session, &st.dashboard_state.probe));
    detail_box.append(&build_session_metadata_grid(session));

    let host_info = st
        .dashboard_state
        .hosts
        .iter()
        .find(|h| h.name == session.host);
    if let Some(host) = host_info {
        detail_box.append(&build_host_resources_section(host));
    }

    if let Some(output_widgets) = build_output_preview_section(session) {
        detail_box.append(&output_widgets.0);
        detail_box.append(&output_widgets.1);
    }

    let is_detached = session.is_detached;
    let is_taarof_managed = session.is_taarof_managed;
    let session_status = session.status.clone();
    let sname = session.name.clone();
    let session_target = session.target.clone();
    let can_detach = !is_detached
        && is_taarof_managed
        && crate::terminal::resolve_attached_session_pane(&st, &sname, &session_target).is_some();

    drop(st);

    detail_box.append(&build_session_action_buttons(
        state,
        term_stack,
        tab_list,
        window,
        &sname,
        &session_target,
        session_status,
        is_detached,
        can_detach,
    ));
}

/// Build the session header row: name label + status badge + optional probe badge.
fn build_session_header(
    session: &DashboardSession,
    probe: &crate::probe::ProbeSnapshot<()>,
) -> gtk::Box {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let name_label = gtk::Label::new(Some(&session.name));
    name_label.add_css_class("title-3");
    name_label.set_halign(gtk::Align::Start);
    header.append(&name_label);

    let status_text = match session.status {
        SessionStatus::Running => "running",
        SessionStatus::Idle => "idle",
        SessionStatus::Detached => "detached",
        SessionStatus::Finished => "finished",
    };
    let status_badge = gtk::Label::new(Some(status_text));
    status_badge.add_css_class("dashboard-status-badge");
    let status_class = match session.status {
        SessionStatus::Running => "status-running",
        SessionStatus::Idle => "status-idle",
        SessionStatus::Detached => "status-detached",
        SessionStatus::Finished => "status-finished",
    };
    status_badge.add_css_class(status_class);
    header.append(&status_badge);

    if probe.state.is_degraded() {
        let badge = probe_badge(probe.state.label(), probe.state);
        header.append(&badge);
    }
    header
}

/// Build the metadata grid (Host, tmux name, optional command, optional age).
fn build_session_metadata_grid(session: &DashboardSession) -> gtk::Grid {
    let grid = gtk::Grid::new();
    grid.set_row_spacing(4);
    grid.set_column_spacing(16);
    let meta = [
        ("Host", session.host.clone()),
        ("tmux", session.name.clone()),
    ];
    for (i, (key, val)) in meta.iter().enumerate() {
        let key_label = gtk::Label::new(Some(key));
        key_label.add_css_class("dim-label");
        key_label.set_halign(gtk::Align::Start);
        grid.attach(&key_label, 0, i as i32, 1, 1);
        let val_label = gtk::Label::new(Some(val));
        val_label.set_halign(gtk::Align::Start);
        grid.attach(&val_label, 1, i as i32, 1, 1);
    }
    if let Some(ref cmd) = session.command {
        let cmd_label = gtk::Label::new(Some("Command"));
        cmd_label.add_css_class("dim-label");
        cmd_label.set_halign(gtk::Align::Start);
        grid.attach(&cmd_label, 0, meta.len() as i32, 1, 1);
        let cmd_val = gtk::Label::new(Some(cmd));
        cmd_val.set_halign(gtk::Align::Start);
        grid.attach(&cmd_val, 1, meta.len() as i32, 1, 1);
    }
    if let Some(ref age) = session.started {
        let age_key = gtk::Label::new(Some("Age"));
        age_key.add_css_class("dim-label");
        age_key.set_halign(gtk::Align::Start);
        grid.attach(&age_key, 0, (meta.len() + 1) as i32, 1, 1);
        let age_val = gtk::Label::new(Some(age));
        age_val.set_halign(gtk::Align::Start);
        grid.attach(&age_val, 1, (meta.len() + 1) as i32, 1, 1);
    }
    grid
}

/// Build the host resource bars (CPU/memory) and optional probe error label.
fn build_host_resources_section(host: &DashboardHost) -> gtk::Box {
    let resources_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    resources_box.set_margin_top(8);

    if host.probe.state.is_degraded() || matches!(host.probe.state, ProbeState::Unknown) {
        let message = match host.probe.state {
            ProbeState::Stale => host
                .probe
                .error
                .as_deref()
                .map(|error| format!("Host metrics stale: {error}"))
                .unwrap_or_else(|| "Host metrics stale".to_string()),
            ProbeState::Error => host
                .probe
                .error
                .as_deref()
                .map(|error| format!("Host metrics unavailable: {error}"))
                .unwrap_or_else(|| "Host metrics unavailable".to_string()),
            ProbeState::Unknown => "Host metrics pending".to_string(),
            ProbeState::Ok => String::new(),
        };
        if !message.is_empty() {
            let probe_label = gtk::Label::new(Some(&message));
            probe_label.add_css_class("dim-label");
            probe_label.add_css_class(if matches!(host.probe.state, ProbeState::Error) {
                "probe-error"
            } else {
                "probe-stale"
            });
            probe_label.set_halign(gtk::Align::Start);
            resources_box.append(&probe_label);
        }
    }

    if let Some(cpu) = host.cpu_percent {
        let cpu_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let cpu_label = gtk::Label::new(Some("CPU"));
        cpu_label.add_css_class("dim-label");
        cpu_label.set_width_chars(6);
        cpu_box.append(&cpu_label);
        let cpu_bar = gtk::ProgressBar::new();
        cpu_bar.set_fraction(cpu / 100.0);
        cpu_bar.set_hexpand(true);
        cpu_bar.add_css_class("dashboard-resource-bar");
        cpu_box.append(&cpu_bar);
        let cpu_pct = gtk::Label::new(Some(&format!("{:.0}%", cpu)));
        cpu_pct.set_width_chars(5);
        cpu_box.append(&cpu_pct);
        resources_box.append(&cpu_box);
    }

    if let Some(mem) = host.memory_percent {
        let mem_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let mem_label = gtk::Label::new(Some("Memory"));
        mem_label.add_css_class("dim-label");
        mem_label.set_width_chars(6);
        mem_box.append(&mem_label);
        let mem_bar = gtk::ProgressBar::new();
        mem_bar.set_fraction(mem / 100.0);
        mem_bar.set_hexpand(true);
        mem_bar.add_css_class("dashboard-resource-bar");
        mem_box.append(&mem_bar);
        let mem_pct = gtk::Label::new(Some(&format!("{:.0}%", mem)));
        mem_pct.set_width_chars(5);
        mem_box.append(&mem_pct);
        resources_box.append(&mem_box);
    }

    resources_box
}

/// Build the output-preview section. Returns `Some((label, frame))` when the
/// session has preview lines, `None` otherwise.
fn build_output_preview_section(session: &DashboardSession) -> Option<(gtk::Label, gtk::Frame)> {
    if session.output_preview.is_empty() {
        return None;
    }
    let output_label = gtk::Label::new(Some("LAST OUTPUT"));
    output_label.add_css_class("dim-label");
    output_label.set_halign(gtk::Align::Start);
    output_label.set_margin_top(8);

    let output_text = session.output_preview.join("\n");
    let text_view = gtk::TextView::new();
    text_view.set_editable(false);
    text_view.set_cursor_visible(false);
    text_view.set_monospace(true);
    text_view.add_css_class("dashboard-output-preview");
    text_view.buffer().set_text(&output_text);
    let output_frame = gtk::Frame::new(None);
    output_frame.set_child(Some(&text_view));

    Some((output_label, output_frame))
}

/// Build the Attach / Detach / Kill action button row.
#[allow(clippy::too_many_arguments)]
fn build_session_action_buttons(
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    sname: &str,
    session_target: &crate::tmux::TmuxTarget,
    session_status: SessionStatus,
    is_detached: bool,
    can_detach: bool,
) -> gtk::Box {
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_margin_top(12);

    if is_detached || session_status == SessionStatus::Idle {
        let attach_btn = gtk::Button::with_label("Attach");
        attach_btn.add_css_class("suggested-action");
        let state_attach = state.clone();
        let term_stack_attach = term_stack.clone();
        let tab_list_attach = tab_list.clone();
        let window_attach = window.clone();
        let sname_attach = sname.to_string();
        let target_attach = session_target.clone();
        attach_btn.connect_clicked(move |_| {
            crate::terminal::attach_session_async(
                &state_attach,
                &term_stack_attach,
                &tab_list_attach,
                &window_attach,
                &sname_attach,
                &target_attach,
                |result| {
                    if let Err(err) = result {
                        crate::show_error_toast(&format!("Attach failed: {err}"));
                    }
                },
            );
        });
        actions.append(&attach_btn);
    }

    if can_detach {
        let detach_btn = gtk::Button::with_label("Detach");
        let state_detach = state.clone();
        let term_stack_detach = term_stack.clone();
        let tab_list_detach = tab_list.clone();
        let window_detach = window.clone();
        let sname_detach = sname.to_string();
        let target_detach = session_target.clone();
        detach_btn.connect_clicked(move |_| {
            match crate::terminal::detach_session_by_name(
                &state_detach,
                &term_stack_detach,
                &tab_list_detach,
                &sname_detach,
                &target_detach,
            ) {
                Ok(_) => {
                    crate::sidebar::refresh_background_section(
                        &tab_list_detach,
                        &state_detach,
                        &term_stack_detach,
                        &window_detach,
                    );
                    crate::dashboard::refresh_dashboard_if_open(&state_detach, &term_stack_detach);
                }
                Err(err) => {
                    crate::show_error_toast(&format!("Detach failed: {err}"));
                }
            }
        });
        actions.append(&detach_btn);
    }

    let kill_btn = gtk::Button::with_label("Kill");
    kill_btn.add_css_class("destructive-action");
    let state_kill = state.clone();
    let tab_list_kill = tab_list.clone();
    let term_stack_kill = term_stack.clone();
    let window_kill = window.clone();
    let sname_kill = sname.to_string();
    let target_kill = session_target.clone();
    kill_btn.connect_clicked(move |_| {
        let state_for_apply = state_kill.clone();
        let tab_list_for_apply = tab_list_kill.clone();
        let term_stack_for_apply = term_stack_kill.clone();
        let window_for_apply = window_kill.clone();
        crate::terminal::kill_session_by_name_with_hint_async(
            &state_kill,
            &window_kill,
            &sname_kill,
            Some(&target_kill),
            move |result| {
                if let Err(error) = result {
                    crate::show_error_toast(&error);
                }
                crate::sidebar::refresh_background_section(
                    &tab_list_for_apply,
                    &state_for_apply,
                    &term_stack_for_apply,
                    &window_for_apply,
                );
            },
        );
    });
    actions.append(&kill_btn);

    actions
}

pub fn refresh_dashboard_list(
    paned: &gtk::Paned,
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
) {
    let Some(session_list) = dashboard_list_box(paned, DASHBOARD_SESSION_LIST_NAME) else {
        return;
    };
    let Some(view_list) = dashboard_list_box(paned, DASHBOARD_VIEW_LIST_NAME) else {
        return;
    };
    let Some(detail_box) = dashboard_detail_box(paned) else {
        return;
    };

    let selected_session_name = session_list
        .selected_row()
        .map(|row| row.widget_name().to_string());
    let selected_view_name = {
        let st = state.borrow();
        st.selected_dashboard_view.clone()
    };

    while let Some(child) = session_list.first_child() {
        session_list.remove(&child);
    }
    while let Some(child) = view_list.first_child() {
        view_list.remove(&child);
    }

    let st = state.borrow();
    let dashboard_probe_state = st.dashboard_state.probe.state;
    for session in &st.dashboard_state.sessions {
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(&session.name);

        let hbox = gtk::Box::new(gtk::Orientation::Vertical, 2);
        hbox.set_margin_top(6);
        hbox.set_margin_bottom(6);
        hbox.set_margin_start(8);
        hbox.set_margin_end(8);

        let line1 = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let dot = match session.status {
            SessionStatus::Running => "\u{25CF}",
            SessionStatus::Idle => "\u{25CF}",
            SessionStatus::Detached => "\u{25CB}",
            SessionStatus::Finished => "\u{2713}",
        };
        let dot_label = gtk::Label::new(Some(dot));
        let dot_class = match session.status {
            SessionStatus::Running => "dot-running",
            SessionStatus::Idle => "dot-idle",
            SessionStatus::Detached | SessionStatus::Finished => "dot-detached",
        };
        dot_label.add_css_class(dot_class);
        line1.append(&dot_label);

        let name_label = gtk::Label::new(Some(&session.name));
        name_label.set_halign(gtk::Align::Start);
        name_label.set_hexpand(true);
        name_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        if session.is_detached {
            name_label.add_css_class("dim-label");
        }
        line1.append(&name_label);
        hbox.append(&line1);

        let line2 = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let host_text = st
            .dashboard_state
            .hosts
            .iter()
            .find(|host| host.name == session.host)
            .and_then(host_probe_label)
            .unwrap_or_else(|| session.host.clone());
        let host_label = gtk::Label::new(Some(&host_text));
        host_label.add_css_class("dim-label");
        host_label.add_css_class("caption");
        if host_text != session.host {
            host_label.add_css_class("probe-stale");
        }
        host_label.set_halign(gtk::Align::Start);
        line2.append(&host_label);
        if let Some(ref cmd) = session.command {
            let cmd_label = gtk::Label::new(Some(cmd));
            cmd_label.add_css_class("dim-label");
            cmd_label.add_css_class("caption");
            cmd_label.set_halign(gtk::Align::End);
            cmd_label.set_hexpand(true);
            cmd_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            line2.append(&cmd_label);
        }
        hbox.append(&line2);

        row.set_child(Some(&hbox));
        session_list.append(&row);
    }
    drop(st);

    ensure_dashboard_views(paned, state);
    let view_snapshot = {
        let st = state.borrow();
        dashboard_views_snapshot(&st)
    };
    let view_snapshot_loading = dashboard_views_loading(&view_snapshot.candidates);
    let views = view_snapshot.views;
    let has_pinned_views = view_snapshot.has_pinned_views;

    for view in views {
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(&view.name);

        let hbox = gtk::Box::new(gtk::Orientation::Vertical, 2);
        hbox.set_margin_top(6);
        hbox.set_margin_bottom(6);
        hbox.set_margin_start(8);
        hbox.set_margin_end(8);

        let name_label = gtk::Label::new(Some(&view.name));
        name_label.set_halign(gtk::Align::Start);
        name_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        hbox.append(&name_label);

        let preset_label = gtk::Label::new(Some(view.preset.label()));
        preset_label.add_css_class("dim-label");
        preset_label.add_css_class("caption");
        preset_label.set_halign(gtk::Align::Start);
        hbox.append(&preset_label);

        row.set_child(Some(&hbox));
        view_list.append(&row);
    }

    if let Some(view_name) = selected_view_name {
        if let Some(row) = dashboard_row_by_name(&view_list, &view_name) {
            view_list.select_row(Some(&row));
        } else if view_snapshot_loading {
            populate_dashboard_placeholder(&detail_box, "Loading saved view…");
        } else {
            state.borrow_mut().selected_dashboard_view = None;
            let message = view_snapshot
                .error
                .as_deref()
                .map(|error| format!("Saved views could not refresh: {error}"))
                .unwrap_or_else(|| "Saved view not found".to_string());
            populate_dashboard_placeholder(&detail_box, &message);
        }
        return;
    }

    if let Some(session_name) = selected_session_name {
        if let Some(row) = dashboard_row_by_name(&session_list, &session_name) {
            session_list.select_row(Some(&row));
            return;
        }
    }

    let placeholder = match view_snapshot.error.as_deref() {
        Some(error) => format!("Saved views could not refresh: {error}"),
        None => match dashboard_probe_state {
            ProbeState::Stale => "Dashboard data is stale. Select a session or saved view",
            ProbeState::Error => "Dashboard refresh failed. Select a session or saved view",
            ProbeState::Unknown => "Dashboard data is pending. Select a session or saved view",
            ProbeState::Ok => "Select a session or saved view",
        }
        .to_string(),
    };

    if has_pinned_views {
        if let Some(row) = view_list.row_at_index(0) {
            view_list.select_row(Some(&row));
            return;
        }
    }

    populate_dashboard_placeholder(&detail_box, &placeholder);
}

fn dashboard_list_box(paned: &gtk::Paned, widget_name: &str) -> Option<gtk::ListBox> {
    let start_child = paned.start_child()?;
    crate::sidebar::find_widget_by_name(start_child.upcast_ref(), widget_name)
        .and_then(|widget| widget.downcast::<gtk::ListBox>().ok())
}

fn dashboard_detail_box(paned: &gtk::Paned) -> Option<gtk::Box> {
    let end_child = paned.end_child()?;
    crate::sidebar::find_widget_by_name(end_child.upcast_ref(), DASHBOARD_DETAIL_NAME)
        .and_then(|widget| widget.downcast::<gtk::Box>().ok())
}

fn dashboard_row_by_name(list_box: &gtk::ListBox, name: &str) -> Option<gtk::ListBoxRow> {
    let mut idx = 0;
    while let Some(row) = list_box.row_at_index(idx) {
        if row.widget_name().as_str() == name {
            return Some(row);
        }
        idx += 1;
    }
    None
}

/// Refresh the dashboard list widget if a dashboard tab exists in any workspace.
fn dashboard_tab_id(workspaces: &[crate::Workspace]) -> Option<u32> {
    workspaces
        .iter()
        .flat_map(|ws| ws.tabs.iter())
        .find(|t| t.kind == crate::workspace::TabKind::Dashboard)
        .map(|t| t.id)
}

pub fn refresh_dashboard_if_open(
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
    term_stack: &gtk::Stack,
) {
    let tab_id = {
        let st = state.borrow();
        dashboard_tab_id(&st.workspaces)
    };
    let Some(tab_id) = tab_id else { return };

    let stack_name = crate::sidebar::tab_root_widget_name(tab_id);
    let Some(child) = term_stack.child_by_name(&stack_name) else {
        return;
    };
    let Some(paned) = child.downcast_ref::<gtk::Paned>() else {
        return;
    };

    refresh_dashboard_list(paned, state);
}

/// Check detached sessions for finished commands and send desktop notifications.
/// Returns list of session names that newly finished.
pub fn check_and_notify_finished(
    detached: &mut [DetachedSession],
    current_commands: &std::collections::HashMap<DetachedSessionCommandKey, String>,
) -> Vec<String> {
    let shells = ["bash", "zsh", "fish", "sh", "dash"];
    let mut newly_finished = Vec::new();

    for ds in detached.iter_mut() {
        if ds.finished {
            continue;
        }
        let key = detached_session_command_key(&ds.target, &ds.session_name);
        if let Some(current_cmd) = current_commands.get(&key) {
            let is_shell = shells.iter().any(|s| current_cmd.trim() == *s);
            if is_shell && ds.last_command.is_some() {
                ds.finished = true;
                newly_finished.push(ds.session_name.clone());

                let notification = detached_finish_notification(ds);
                if let Err(e) = notify_rust::Notification::new()
                    .summary(&notification.summary)
                    .body(&notification.body)
                    .icon("utilities-terminal")
                    .timeout(notify_rust::Timeout::Milliseconds(5000))
                    .show()
                {
                    eprintln!("taarof: notification failed: {e}");
                }
            }
        }
    }

    newly_finished
}

/// Open the dashboard tab: creates the tab in state, builds the widget, adds
/// it to term_stack, and wires a sidebar row.  If a dashboard tab already
/// exists the function just focuses it.  Returns the tab id.
pub fn open_dashboard(
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
) -> Option<u32> {
    let existing_tab_id = {
        let st = state.borrow();
        dashboard_tab_id(&st.workspaces)
    };

    if let Some(tab_id) = existing_tab_id {
        crate::sidebar::activate_tab(tab_list, state, term_stack, tab_id);
        refresh_dashboard_if_open(state, term_stack);
        return Some(tab_id);
    }

    let tab_id = state.borrow_mut().create_dashboard_tab()?;

    let widget = build_dashboard_widget(state, term_stack, tab_list, window);
    let stack_name = crate::sidebar::tab_root_widget_name(tab_id);
    term_stack.add_named(&widget, Some(&stack_name));
    term_stack.set_visible_child_name(&stack_name);

    crate::sidebar::add_tab_row(tab_list, state, term_stack, tab_id, "Dashboard", true);

    // Populate the dashboard immediately.
    refresh_dashboard_list(&widget, state);
    Some(tab_id)
}

pub fn open_saved_view(
    name: &str,
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
) {
    // Lookup is driven by the dashboard's asynchronous, project-aware cache.
    // Keeping only the requested name on GTK lets a cold cache resolve it
    // without synchronously walking Git/config/view files here.
    state.borrow_mut().selected_dashboard_view = Some(name.to_string());
    let _ = open_dashboard(state, term_stack, tab_list, window);
    refresh_dashboard_if_open(state, term_stack);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::ProbeState;
    use crate::{pane::PaneNode, Tab, Workspace, WorkspaceStatus};
    use std::collections::HashMap;

    fn workspace_with_tabs(id: u32, tabs: Vec<Tab>, active_tab: u32) -> Workspace {
        Workspace {
            id,
            work_origin: crate::workspace::new_workspace_work_origin(),
            name: format!("ws-{id}"),
            collapsed: false,
            repo_root: None,
            is_worktree: false,
            working_tree_path: None,
            branch_name: None,
            linked_issue: None,
            env_vars: HashMap::new(),
            run_status: WorkspaceStatus::Idle,
            tabs,
            active_tab,
            last_active_tab: None,
            tmux_backed: false,
            host_config_name: None,
            host_status: crate::probe::ProbeSnapshot::default(),
        }
    }

    fn terminal_tab(id: u32, name: &str) -> Tab {
        Tab {
            id,
            name: name.into(),
            work_origin: crate::workspace::new_tab_work_origin(),
            kind: crate::workspace::TabKind::Terminal,
            panes: Box::new(PaneNode::Stub { pane_id: id + 1000 }),
            focused_pane_id: id + 1000,
            next_pane_id: id + 1001,
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

    fn dashboard_tab(id: u32) -> Tab {
        Tab {
            id,
            name: "Dashboard".into(),
            work_origin: crate::workspace::new_tab_work_origin(),
            kind: crate::workspace::TabKind::Dashboard,
            panes: Box::new(PaneNode::Empty),
            focused_pane_id: 0,
            next_pane_id: 0,
            pane_zoom: None,
            close_on_exit: false,
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

    #[test]
    fn test_dashboard_tab_id_returns_none_when_dashboard_absent() {
        let workspaces = vec![workspace_with_tabs(1, vec![terminal_tab(11, "Shell")], 11)];
        assert_eq!(dashboard_tab_id(&workspaces), None);
    }

    #[test]
    fn test_dashboard_tab_id_finds_dashboard_across_workspaces() {
        let workspaces = vec![
            workspace_with_tabs(1, vec![terminal_tab(11, "Shell")], 11),
            workspace_with_tabs(2, vec![dashboard_tab(21)], 21),
        ];
        assert_eq!(dashboard_tab_id(&workspaces), Some(21));
    }

    #[test]
    fn test_aggregate_empty() {
        let state =
            aggregate_dashboard_state(&[], &[], &[], &std::collections::HashMap::new(), "taarof");
        assert!(state.sessions.is_empty());
        assert!(state.hosts.is_empty());
    }

    #[test]
    fn dashboard_state_marks_stale_on_failure_and_recovers() {
        let mut state = DashboardState::default();
        let sessions = vec![DashboardSession {
            name: "taarof--default--t1--0".into(),
            host: "localhost".into(),
            target: TmuxTarget::Local,
            status: SessionStatus::Running,
            command: Some("cargo test".into()),
            started: Some("1m".into()),
            pid: None,
            output_preview: Vec::new(),
            is_detached: false,
            is_taarof_managed: true,
        }];
        let hosts = vec![DashboardHost {
            name: "localhost".into(),
            cpu_percent: Some(10.0),
            memory_percent: Some(20.0),
            session_count: 1,
            probe: crate::probe::ProbeSnapshot::default(),
        }];

        let transition = state.apply_success(sessions.clone(), hosts.clone());
        assert_eq!(transition.current, ProbeState::Ok);
        assert_eq!(state.probe.state, ProbeState::Ok);
        assert_eq!(state.sessions.len(), 1);

        let transition = state.apply_failure("timeout");
        assert_eq!(transition.current, ProbeState::Stale);
        assert_eq!(state.probe.state, ProbeState::Stale);
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.hosts.len(), 1);
        assert_eq!(state.probe.error.as_deref(), Some("timeout"));

        let transition = state.apply_success(Vec::new(), Vec::new());
        assert_eq!(transition.current, ProbeState::Ok);
        assert_eq!(state.probe.state, ProbeState::Ok);
        assert!(state.sessions.is_empty());
        assert!(state.hosts.is_empty());
        assert_eq!(state.probe.error, None);
    }

    #[test]
    fn dashboard_view_invalidation_replaces_same_candidate_flight_and_old_completion_cannot_release_it(
    ) {
        let candidates = vec!["/repo".to_string()];
        let mut cache = DashboardViewsCache::default();
        let old = claim_dashboard_views_refresh(&mut cache, &candidates)
            .expect("first dashboard request should start");

        invalidate_dashboard_views_cache(&mut cache);

        let replacement = claim_dashboard_views_refresh(&mut cache, &candidates)
            .expect("same candidates must refresh after invalidation");
        assert_ne!(old.generation, replacement.generation);
        assert_ne!(
            dashboard_views_worker_key(&old.candidates, old.generation),
            dashboard_views_worker_key(&replacement.candidates, replacement.generation),
            "the global worker gate must not coalesce an invalidated request"
        );
        assert!(
            !release_dashboard_views_request(&mut cache, &old),
            "old completion must not clear the replacement ledger"
        );
        assert_eq!(cache.in_flight.as_ref(), Some(&replacement));
        assert!(release_dashboard_views_request(&mut cache, &replacement));
        assert!(cache.in_flight.is_none());
    }

    #[test]
    fn test_aggregate_local_sessions() {
        let tmux_sessions = vec![(
            TmuxTarget::Local,
            vec![
                ("taarof--default--t3--0".to_string(), 1711800000, true, 1),
                ("other-session".to_string(), 1711700000, false, 1),
            ],
        )];
        let state = aggregate_dashboard_state(
            &[],
            &[],
            &tmux_sessions,
            &std::collections::HashMap::new(),
            "taarof",
        );
        assert_eq!(state.sessions.len(), 2);
        assert!(state.sessions[0].is_taarof_managed);
        assert!(!state.sessions[1].is_taarof_managed);
        assert_eq!(state.sessions[0].status, SessionStatus::Running);
        assert_eq!(state.sessions[1].status, SessionStatus::Idle);
        assert_eq!(state.hosts.len(), 1);
        assert_eq!(state.hosts[0].session_count, 2);
    }

    #[test]
    fn test_aggregate_detached_session() {
        let detached = vec![DetachedSession {
            session_name: "taarof--default--t3--0".into(),
            host: "localhost".into(),
            workspace: "default".into(),
            target: TmuxTarget::Local,
            detached_at: Instant::now(),
            last_command: Some("cargo build".into()),
            finished: false,
        }];
        let tmux_sessions = vec![(
            TmuxTarget::Local,
            vec![("taarof--default--t3--0".to_string(), 1711800000, false, 1)],
        )];
        let state = aggregate_dashboard_state(
            &[],
            &detached,
            &tmux_sessions,
            &std::collections::HashMap::new(),
            "taarof",
        );
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].status, SessionStatus::Detached);
        assert!(state.sessions[0].is_detached);
        assert_eq!(state.sessions[0].command, Some("cargo build".into()));
    }

    #[test]
    fn test_aggregate_finished_session() {
        let detached = vec![DetachedSession {
            session_name: "taarof--default--t3--0".into(),
            host: "localhost".into(),
            workspace: "default".into(),
            target: TmuxTarget::Local,
            detached_at: Instant::now(),
            last_command: Some("cargo test".into()),
            finished: true,
        }];
        let tmux_sessions = vec![(
            TmuxTarget::Local,
            vec![("taarof--default--t3--0".to_string(), 1711800000, false, 1)],
        )];
        let state = aggregate_dashboard_state(
            &[],
            &detached,
            &tmux_sessions,
            &std::collections::HashMap::new(),
            "taarof",
        );
        assert_eq!(state.sessions[0].status, SessionStatus::Finished);
    }

    #[test]
    fn test_aggregate_output_preview() {
        let tmux_sessions = vec![(
            TmuxTarget::Local,
            vec![("my-session".to_string(), 1711800000, true, 1)],
        )];
        let mut capture_outputs = std::collections::HashMap::new();
        capture_outputs.insert(
            "my-session".to_string(),
            vec!["line1".to_string(), "line2".to_string()],
        );
        let state = aggregate_dashboard_state(&[], &[], &tmux_sessions, &capture_outputs, "taarof");
        assert_eq!(state.sessions[0].output_preview, vec!["line1", "line2"]);
    }

    #[test]
    fn test_aggregate_includes_detached_sessions_missing_from_live_tmux_results() {
        let detached = vec![DetachedSession {
            session_name: "taarof--remote--t9--0".into(),
            host: "user@remote".into(),
            workspace: "remote".into(),
            target: TmuxTarget::Remote {
                ssh_target: "user@remote".into(),
            },
            detached_at: Instant::now(),
            last_command: Some("cargo test".into()),
            finished: false,
        }];
        let state = aggregate_dashboard_state(
            &[],
            &detached,
            &[],
            &std::collections::HashMap::new(),
            "taarof",
        );
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].host, "user@remote");
        assert_eq!(state.sessions[0].status, SessionStatus::Detached);
        assert_eq!(
            state.sessions[0].target,
            TmuxTarget::Remote {
                ssh_target: "user@remote".into()
            }
        );
    }

    #[test]
    fn test_aggregate_matches_detached_sessions_by_target_identity() {
        let detached = vec![DetachedSession {
            session_name: "shared-name".into(),
            host: "user@remote".into(),
            workspace: "remote".into(),
            target: TmuxTarget::Remote {
                ssh_target: "user@remote".into(),
            },
            detached_at: Instant::now(),
            last_command: Some("cargo test".into()),
            finished: false,
        }];
        let tmux_sessions = vec![(
            TmuxTarget::Local,
            vec![("shared-name".to_string(), 1711800000, false, 1)],
        )];

        let state = aggregate_dashboard_state(
            &[],
            &detached,
            &tmux_sessions,
            &std::collections::HashMap::new(),
            "taarof",
        );

        assert_eq!(state.sessions.len(), 2);
        assert_eq!(state.sessions[0].target, TmuxTarget::Local);
        assert_eq!(state.sessions[0].status, SessionStatus::Idle);
        assert!(!state.sessions[0].is_detached);
        assert_eq!(
            state.sessions[1].target,
            TmuxTarget::Remote {
                ssh_target: "user@remote".into()
            }
        );
        assert_eq!(state.sessions[1].status, SessionStatus::Detached);
        assert!(state.sessions[1].is_detached);
    }

    #[test]
    fn test_saved_detached_session_roundtrip_local() {
        let saved = SavedDetachedSession {
            session_name: "taarof--default--t3--0".into(),
            host: "localhost".into(),
            workspace: "default".into(),
            ssh_target: None,
            last_command: Some("cargo build".into()),
        };
        let json = serde_json::to_string(&saved).unwrap();
        let restored: SavedDetachedSession = serde_json::from_str(&json).unwrap();
        assert_eq!(saved, restored);
    }

    #[test]
    fn test_saved_detached_session_roundtrip_remote() {
        let saved = SavedDetachedSession {
            session_name: "taarof--infra--t7--1".into(),
            host: "ci.internal".into(),
            workspace: "infra".into(),
            ssh_target: Some("user@ci.internal".into()),
            last_command: None,
        };
        let json = serde_json::to_string(&saved).unwrap();
        let restored: SavedDetachedSession = serde_json::from_str(&json).unwrap();
        assert_eq!(saved, restored);
    }

    #[test]
    fn test_detached_session_to_saved_local() {
        let ds = DetachedSession {
            session_name: "taarof--default--t3--0".into(),
            host: "localhost".into(),
            workspace: "default".into(),
            target: TmuxTarget::Local,
            detached_at: Instant::now(),
            last_command: Some("npm run dev".into()),
            finished: false,
        };
        let saved = ds.to_saved();
        assert_eq!(saved.session_name, "taarof--default--t3--0");
        assert_eq!(saved.ssh_target, None);
        assert_eq!(saved.last_command, Some("npm run dev".into()));
    }

    #[test]
    fn test_detached_session_from_saved_remote() {
        let saved = SavedDetachedSession {
            session_name: "taarof--infra--t7--1".into(),
            host: "ci.internal".into(),
            workspace: "infra".into(),
            ssh_target: Some("user@ci.internal".into()),
            last_command: None,
        };
        let ds = DetachedSession::from_saved(&saved);
        assert_eq!(
            ds.target,
            TmuxTarget::Remote {
                ssh_target: "user@ci.internal".into()
            }
        );
        assert!(!ds.finished);
    }

    #[test]
    fn test_ssh_target_none_omitted_from_json() {
        let saved = SavedDetachedSession {
            session_name: "s".into(),
            host: "localhost".into(),
            workspace: "w".into(),
            ssh_target: None,
            last_command: None,
        };
        let json = serde_json::to_string(&saved).unwrap();
        assert!(!json.contains("ssh_target"));
        assert!(!json.contains("last_command"));
    }

    #[test]
    fn test_check_finished_detects_shell_return() {
        let mut detached = vec![DetachedSession {
            session_name: "taarof--default--t3--0".into(),
            host: "localhost".into(),
            workspace: "default".into(),
            target: TmuxTarget::Local,
            detached_at: Instant::now(),
            last_command: Some("cargo build".into()),
            finished: false,
        }];
        let mut cmds = std::collections::HashMap::new();
        cmds.insert(
            detached_session_command_key(&TmuxTarget::Local, "taarof--default--t3--0"),
            "zsh".into(),
        );

        let finished = check_and_notify_finished(&mut detached, &cmds);
        assert_eq!(finished, vec!["taarof--default--t3--0"]);
        assert!(detached[0].finished);
    }

    #[test]
    fn detached_finish_notification_names_command_workspace_session_and_host() {
        let detached = DetachedSession {
            session_name: "taarof--default--t3--0".into(),
            host: "c1-box".into(),
            workspace: "LM-HQ".into(),
            target: TmuxTarget::Local,
            detached_at: Instant::now(),
            last_command: Some("/usr/bin/codex".into()),
            finished: true,
        };

        let notification = detached_finish_notification(&detached);
        assert_eq!(notification.summary, "Codex finished — LM-HQ");
        assert_eq!(notification.body, "Session default--t3--0 · c1-box");
    }

    #[test]
    fn test_check_finished_ignores_still_running() {
        let mut detached = vec![DetachedSession {
            session_name: "taarof--default--t3--0".into(),
            host: "localhost".into(),
            workspace: "default".into(),
            target: TmuxTarget::Local,
            detached_at: Instant::now(),
            last_command: Some("cargo build".into()),
            finished: false,
        }];
        let mut cmds = std::collections::HashMap::new();
        cmds.insert(
            detached_session_command_key(&TmuxTarget::Local, "taarof--default--t3--0"),
            "cargo".into(),
        );

        let finished = check_and_notify_finished(&mut detached, &cmds);
        assert!(finished.is_empty());
        assert!(!detached[0].finished);
    }

    #[test]
    fn test_check_finished_skips_already_finished() {
        let mut detached = vec![DetachedSession {
            session_name: "s".into(),
            host: "localhost".into(),
            workspace: "w".into(),
            target: TmuxTarget::Local,
            detached_at: Instant::now(),
            last_command: Some("make".into()),
            finished: true,
        }];
        let mut cmds = std::collections::HashMap::new();
        cmds.insert(
            detached_session_command_key(&TmuxTarget::Local, "s"),
            "zsh".into(),
        );

        let finished = check_and_notify_finished(&mut detached, &cmds);
        assert!(finished.is_empty());
    }

    #[test]
    fn test_check_finished_disambiguates_same_name_by_target() {
        let session_name = "taarof--shared--t3--0";
        let remote_target = TmuxTarget::Remote {
            ssh_target: "builder@ci-box".into(),
        };
        let mut detached = vec![
            DetachedSession {
                session_name: session_name.into(),
                host: "localhost".into(),
                workspace: "default".into(),
                target: TmuxTarget::Local,
                detached_at: Instant::now(),
                last_command: Some("cargo build".into()),
                finished: false,
            },
            DetachedSession {
                session_name: session_name.into(),
                host: "ci-box".into(),
                workspace: "default".into(),
                target: remote_target.clone(),
                detached_at: Instant::now(),
                last_command: Some("cargo build".into()),
                finished: false,
            },
        ];
        let mut cmds = std::collections::HashMap::new();
        cmds.insert(
            detached_session_command_key(&remote_target, session_name),
            "zsh".into(),
        );
        cmds.insert(
            detached_session_command_key(&TmuxTarget::Local, session_name),
            "cargo".into(),
        );

        let finished = check_and_notify_finished(&mut detached, &cmds);

        assert_eq!(finished, vec![session_name.to_string()]);
        assert!(!detached[0].finished);
        assert!(detached[1].finished);
    }
}
