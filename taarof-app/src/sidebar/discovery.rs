//! Sidebar discovery-backed detail sections and `.plan` monitoring.
//!
//! This module owns mise/action discovery, tasks/features refresh, and the
//! per-tab `.plan` file monitor lifecycle. It must not own sidebar layout or
//! workspace/tab row construction.

use super::view_model::{tab_row_handle, PlanMonitorState};
use super::*;
use crate::tracking::{PlanTasksData, TrackingData};

// Poll interval for the in-memory task discovery cache. This timer checks
// whether the background worker has stored its results yet — it does NOT invoke
// mise or SSH itself. A shorter interval feels more responsive but wastes
// main-loop wakeups on slow discovery (remote SSH can take several seconds).
// 500 ms balances perceived latency against idle-CPU churn; the spinner in the
// sidebar communicates that work is in progress while the user waits.
const TASK_DISCOVERY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug)]
struct DiscoverySnapshot {
    target: crate::mise::DiscoveryTarget,
    tasks: Vec<crate::mise::MiseTask>,
    discovery_cwd: Option<String>,
    actions: Vec<crate::workspace::WorkspaceAction>,
    task_buttons: Vec<crate::workspace::WorkspaceTaskButton>,
    tracking: Option<TrackingData>,
    plan_tasks: Option<PlanTasksData>,
}

/// Why task discovery could not start for a tab.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DiscoveryFailureReason {
    /// The tab_id is not in the active workspace (or does not exist).
    TabNotFound,
    /// A remote session was detected (OSC 7 host or an ssh process in the pane)
    /// but taarof could not resolve a reachable host and directory to probe.
    UnresolvedRemoteContext,
    /// The tab exists locally but its shell has not reported a working directory yet.
    NoWorkingDirectory,
}

impl DiscoveryFailureReason {
    fn toast_message(&self) -> &'static str {
        match self {
            Self::TabNotFound => "Could not discover tasks: no active terminal tab",
            Self::UnresolvedRemoteContext => {
                "Could not discover tasks: detected a remote SSH session but could not resolve a target host and directory"
            }
            Self::NoWorkingDirectory => {
                "Could not discover tasks: no working directory reported yet"
            }
        }
    }
}

/// Classify why `resolve_discovery_target` returned `None` for a given tab.
fn classify_discovery_failure(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
) -> DiscoveryFailureReason {
    let st = state.borrow();
    let Some((_, tab)) = st.find_tab(tab_id) else {
        return DiscoveryFailureReason::TabNotFound;
    };
    // Delegate to the same signal-based classification target resolution uses so
    // a manually-ssh'd pane that could not be resolved reports remote context
    // rather than a misleading "no working directory".
    match crate::mise::classify_discovery_target_failure(tab) {
        crate::mise::DiscoveryTargetFailure::UnresolvedRemoteContext => {
            DiscoveryFailureReason::UnresolvedRemoteContext
        }
        crate::mise::DiscoveryTargetFailure::NoWorkingDirectory => {
            DiscoveryFailureReason::NoWorkingDirectory
        }
    }
}

/// Discover mise tasks and `.plan` tracking data for a specific tab.
/// Stores results on the `Tab` and refreshes the discovery-backed sidebar UI.
///
/// `reveal` distinguishes an explicit user "Discover Tasks" action (`true`,
/// which surfaces the task UI for this tab regardless of `[tasks] enabled`)
/// from automatic background discovery on restore (`false`, which still
/// refreshes mise actions and the tool-version chip but leaves task UI hidden
/// when the feature is off). It is monotonic: discovery never un-reveals a tab.
pub(crate) fn discover_for_tab(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    tab_id: u32,
    reveal: bool,
) {
    let Some(target) = resolve_discovery_target(state, tab_id) else {
        // Only surface toasts for explicit "Discover Tasks" actions; background
        // discovery on template restore or session load should fail silently.
        if reveal {
            let reason = classify_discovery_failure(state, tab_id);
            crate::show_error_toast(reason.toast_message());
        }
        return;
    };

    {
        let mut st = state.borrow_mut();
        if reveal {
            st.revealed_task_tabs.insert(tab_id);
        }
        crate::mise::update_discovery_binary_cache(&mut st, tab_id, &target);
    }

    let (request, tasks) = prepare_discovery_snapshot(&target);
    submit_discovery_snapshot(state, tab_list, tab_id, target.clone(), tasks, reveal);

    match request {
        crate::mise::TaskDiscoveryRequest::UseCached => {
            clear_task_discovery_poll(tab_list, tab_id);
        }
        crate::mise::TaskDiscoveryRequest::Pending | crate::mise::TaskDiscoveryRequest::Start => {
            ensure_task_discovery_poll(state, tab_list, tab_id, target);
        }
    }
}

fn resolve_discovery_target(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
) -> Option<crate::mise::DiscoveryTarget> {
    let st = state.borrow();
    crate::mise::task_target_for_tab(&st, tab_id)
}

fn build_discovery_snapshot(
    target: &crate::mise::DiscoveryTarget,
    tasks: Vec<crate::mise::MiseTask>,
) -> DiscoverySnapshot {
    let (discovery_cwd, tracking, project, plan_tasks) = match target {
        crate::mise::DiscoveryTarget::Local { cwd, .. } => {
            let cwd = cwd.clone();
            let root = std::path::PathBuf::from(&cwd);
            let tracking = TrackingData::load(&root);
            let project = crate::project_config::load_for_workspace_cwd(&cwd);
            let plan_tasks = PlanTasksData::load(&root);
            (Some(cwd), tracking, project, plan_tasks)
        }
        crate::mise::DiscoveryTarget::Remote { .. } => (None, None, None, None),
    };
    let actions = crate::mise::standard_actions_from_tasks(&tasks);
    let task_buttons = crate::mise::task_buttons_from_tasks(&tasks, project.as_ref());
    DiscoverySnapshot {
        target: target.clone(),
        tasks,
        discovery_cwd,
        actions,
        task_buttons,
        tracking,
        plan_tasks,
    }
}

fn prepare_discovery_snapshot(
    target: &crate::mise::DiscoveryTarget,
) -> (
    crate::mise::TaskDiscoveryRequest,
    Vec<crate::mise::MiseTask>,
) {
    let request = crate::mise::spawn_task_discovery(target.clone());
    let tasks = match request {
        crate::mise::TaskDiscoveryRequest::UseCached => {
            match crate::mise::cached_task_discovery(target) {
                crate::mise::CachedTaskDiscovery::Ready(tasks) => tasks,
                crate::mise::CachedTaskDiscovery::Pending
                | crate::mise::CachedTaskDiscovery::Missing => Vec::new(),
            }
        }
        crate::mise::TaskDiscoveryRequest::Pending | crate::mise::TaskDiscoveryRequest::Start => {
            Vec::new()
        }
    };
    (request, tasks)
}

/// All project config and `.plan` reads happen in this bounded worker. The
/// immutable target is checked again before the GTK-side apply, so an old cwd
/// can never overwrite a tab that navigated while discovery was in flight.
fn submit_discovery_snapshot(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    tab_id: u32,
    target: crate::mise::DiscoveryTarget,
    tasks: Vec<crate::mise::MiseTask>,
    reveal: bool,
) {
    let Some(handle) = tab_row_handle(tab_list, tab_id) else {
        return;
    };
    let generation = handle.discovery_generation.get().wrapping_add(1);
    handle.discovery_generation.set(generation);
    let worker_target = target.clone();
    let apply_target = target.clone();
    let state_for_apply = state.clone();
    let tab_list_for_apply = tab_list.clone();
    let submission = crate::git::spawn_async_result(
        format!("sidebar-discovery:{tab_id}:{generation}:{target:?}"),
        move || Ok(build_discovery_snapshot(&worker_target, tasks)),
        move |result| {
            let Ok(snapshot) = result else {
                if reveal {
                    crate::show_error_toast(
                        "Task discovery worker did not complete; retry discovery",
                    );
                }
                return;
            };
            if resolve_discovery_target(&state_for_apply, tab_id).as_ref() != Some(&apply_target) {
                return;
            }
            let Some(handle) = tab_row_handle(&tab_list_for_apply, tab_id) else {
                return;
            };
            if handle.discovery_generation.get() != generation {
                return;
            }
            apply_discovery_snapshot(&state_for_apply, &tab_list_for_apply, tab_id, snapshot);
        },
    );
    if matches!(submission, crate::git::GitAsyncSubmission::Saturated) && reveal {
        crate::show_error_toast("Task discovery queue is busy; retry discovery shortly");
    }
}

fn apply_discovery_snapshot(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    tab_id: u32,
    snapshot: DiscoverySnapshot,
) {
    let Some(handle) = tab_row_handle(tab_list, tab_id) else {
        return;
    };
    {
        let mut st = state.borrow_mut();
        if let Some(tab) = st.find_tab_mut(tab_id) {
            tab.discovery_cwd = snapshot.discovery_cwd.clone();
            tab.discovered_actions = snapshot.actions.clone();
            tab.task_buttons = snapshot.task_buttons.clone();
            tab.tracking_data = snapshot.tracking.clone();
        }
        st.task_discovery_snapshots.insert(
            tab_id,
            crate::task_launch::TaskDiscoverySnapshot::from_tasks(
                snapshot.target.clone(),
                &snapshot.tasks,
            ),
        );
    }
    *handle.plan_tasks.borrow_mut() = snapshot.plan_tasks;

    refresh_tab_detail_section(tab_list, state, tab_id);
    update_plan_monitor(tab_list, state, tab_id, snapshot.discovery_cwd.as_deref());
}

/// Drop task affordances as soon as the pane's task target changes, then begin
/// discovery for the new target. This is intentionally driven by terminal
/// location updates rather than a later click: a visible A-task must never
/// survive after the pane has moved to B.
pub(crate) fn invalidate_and_rediscover_if_target_changed(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    tab_id: u32,
) {
    let changed = {
        let st = state.borrow();
        let current = crate::mise::task_target_for_tab(&st, tab_id);
        task_discovery_target_changed(st.task_discovery_snapshots.get(&tab_id), current.as_ref())
    };
    if !changed {
        return;
    }

    clear_task_discovery_poll(tab_list, tab_id);
    {
        let mut st = state.borrow_mut();
        st.task_discovery_snapshots.remove(&tab_id);
        if let Some(tab) = st.find_tab_mut(tab_id) {
            tab.discovery_cwd = None;
            tab.discovered_actions.clear();
            tab.task_buttons.clear();
            tab.tracking_data = None;
        }
    }
    refresh_tab_detail_section(tab_list, state, tab_id);
    discover_for_tab(state, tab_list, tab_id, false);
}

fn task_discovery_target_changed(
    previous: Option<&crate::task_launch::TaskDiscoverySnapshot>,
    current: Option<&crate::mise::DiscoveryTarget>,
) -> bool {
    match (previous, current) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(previous), Some(current)) => {
            !crate::mise::same_task_target(previous.target(), current)
        }
    }
}

fn clear_task_discovery_poll(tab_list: &gtk::Box, tab_id: u32) {
    let Some(handle) = tab_row_handle(tab_list, tab_id) else {
        return;
    };
    handle.clear_discovery_poll();
}

fn ensure_task_discovery_poll(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    tab_id: u32,
    target: crate::mise::DiscoveryTarget,
) {
    let Some(handle) = tab_row_handle(tab_list, tab_id) else {
        return;
    };
    if handle.discovery_poll_source.borrow().is_some() {
        return;
    }

    let state = state.clone();
    let tab_list = tab_list.clone();
    let target_for_poll = target.clone();
    let source_id = glib::timeout_add_local(TASK_DISCOVERY_POLL_INTERVAL, move || {
        match crate::mise::cached_task_discovery(&target_for_poll) {
            crate::mise::CachedTaskDiscovery::Ready(tasks) => {
                clear_task_discovery_poll(&tab_list, tab_id);
                submit_discovery_snapshot(
                    &state,
                    &tab_list,
                    tab_id,
                    target_for_poll.clone(),
                    tasks,
                    false,
                );
                glib::ControlFlow::Break
            }
            crate::mise::CachedTaskDiscovery::Pending => glib::ControlFlow::Continue,
            crate::mise::CachedTaskDiscovery::Missing => {
                clear_task_discovery_poll(&tab_list, tab_id);
                glib::ControlFlow::Break
            }
        }
    });
    handle.set_discovery_poll(source_id);
}

fn update_plan_monitor(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    discovery_cwd: Option<&str>,
) {
    let Some(handle) = tab_row_handle(tab_list, tab_id) else {
        return;
    };
    let discovery_generation = handle.discovery_generation.get();
    handle.clear_plan_monitor();

    let Some(discovery_cwd) = discovery_cwd.map(str::to_string) else {
        return;
    };

    let plan_dir = std::path::PathBuf::from(&discovery_cwd).join(".plan");

    let file = gio::File::for_path(&plan_dir);
    if let Ok(monitor) = file.monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
    {
        let state = state.clone();
        let tab_list_for_monitor = tab_list.clone();
        let discovery_cwd = discovery_cwd.clone();
        let pending = Rc::new(std::cell::Cell::new(false));
        let refresh_source = Rc::new(RefCell::new(None));
        let pending_for_cb = pending.clone();
        let refresh_source_for_cb = refresh_source.clone();
        monitor.connect_changed(move |_, _, _, _| {
            if pending_for_cb.get() {
                return;
            }
            pending_for_cb.set(true);
            let state = state.clone();
            let tab_list = tab_list_for_monitor.clone();
            let discovery_cwd = discovery_cwd.clone();
            let pending = pending_for_cb.clone();
            let refresh_source = refresh_source_for_cb.clone();
            let source_id =
                glib::timeout_add_local_once(std::time::Duration::from_secs(1), move || {
                    pending.set(false);
                    refresh_source.borrow_mut().take();
                    let state_for_apply = state.clone();
                    let tab_list_for_apply = tab_list.clone();
                    let worker_cwd = discovery_cwd.clone();
                    let apply_cwd = discovery_cwd.clone();
                    let submission = crate::git::spawn_async_result(
                        format!(
                            "sidebar-plan-refresh:{tab_id}:{discovery_generation}:{worker_cwd}"
                        ),
                        move || {
                            let root = std::path::PathBuf::from(&worker_cwd);
                            Ok((TrackingData::load(&root), PlanTasksData::load(&root)))
                        },
                        move |result| {
                            let Ok((tracking, plan_tasks)) = result else {
                                crate::show_error_toast(
                                    "Plan refresh worker did not complete; the sidebar will retry",
                                );
                                return;
                            };
                            let current_cwd = state_for_apply
                                .borrow()
                                .find_tab(tab_id)
                                .and_then(|(_, tab)| tab.discovery_cwd.clone());
                            if current_cwd.as_deref() != Some(apply_cwd.as_str()) {
                                return;
                            }
                            let Some(handle) = tab_row_handle(&tab_list_for_apply, tab_id) else {
                                return;
                            };
                            if handle.discovery_generation.get() != discovery_generation {
                                return;
                            }
                            if let Some(tab) = state_for_apply.borrow_mut().find_tab_mut(tab_id) {
                                tab.tracking_data = tracking;
                            }
                            *handle.plan_tasks.borrow_mut() = plan_tasks;
                            refresh_tab_detail_section(
                                &tab_list_for_apply,
                                &state_for_apply,
                                tab_id,
                            );
                        },
                    );
                    if matches!(submission, crate::git::GitAsyncSubmission::Saturated) {
                        crate::show_error_toast(
                            "Plan refresh queue is busy; the sidebar will retry",
                        );
                    }
                });
            refresh_source_for_cb.borrow_mut().replace(source_id);
        });

        handle.set_plan_monitor(PlanMonitorState {
            _monitor: monitor,
            pending,
            refresh_source,
        });
    }
}

pub(super) fn refresh_tab_detail_section(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
) {
    let Some(handle) = tab_row_handle(tab_list, tab_id) else {
        return;
    };

    let (task_buttons, plan_tasks) = {
        let st = state.borrow();
        let Some((_, tab)) = st.find_tab(tab_id) else {
            return;
        };
        // The global tasks feature renders in the right-hand Tasks dock. Keep
        // tab rows as navigation in that mode; inline details remain only as the
        // explicit "Discover Tasks" fallback when the dock is disabled.
        let show_tasks = sidebar_task_details_visible(
            crate::config::tasks_config().enabled,
            st.revealed_task_tabs.contains(&tab_id),
        );
        if show_tasks {
            (tab.task_buttons.clone(), handle.plan_tasks.borrow().clone())
        } else {
            (Vec::new(), None)
        }
    };

    let actions_box = handle.actions_box.clone();
    while let Some(child) = actions_box.first_child() {
        actions_box.remove(&child);
    }
    if task_buttons.is_empty() {
        actions_box.set_visible(false);
    } else {
        for task in &task_buttons {
            let button = gtk::Button::with_label(&task.label);
            button.add_css_class("workspace-action-button");
            let task_name = task.task_name.clone();
            let state = state.clone();
            let tab_list = tab_list.clone();
            let term_stack = handle.term_stack.clone();
            let button_for_root = button.clone();
            button.connect_clicked(move |_| {
                let request = {
                    let st = state.borrow();
                    sidebar_task_launch_request(&st, tab_id, task_name.clone())
                };
                let Some(window) = button_for_root
                    .root()
                    .and_downcast::<adw::ApplicationWindow>()
                else {
                    crate::show_error_toast(
                        "Could not open a task tab: the sidebar is not attached to a window",
                    );
                    return;
                };
                if let Err(error) =
                    crate::palette::launch_task(&request, &state, &tab_list, &term_stack, &window)
                {
                    crate::show_error_toast(&error.to_string());
                }
            });
            actions_box.append(&button);
        }
        let refresh_btn = gtk::Button::with_label("\u{21bb}");
        refresh_btn.add_css_class("workspace-action-button");
        refresh_btn.set_tooltip_text(Some("Re-discover tasks"));
        {
            let state = state.clone();
            let tab_list = tab_list.clone();
            refresh_btn.connect_clicked(move |_| {
                crate::task_panel::discover_tasks_explicitly(&state, &tab_list, tab_id);
            });
        }
        actions_box.append(&refresh_btn);
        actions_box.set_visible(!super::compact_mode_active());
    }

    let tracking_box = handle.tracking_box.clone();
    while let Some(child) = tracking_box.first_child() {
        tracking_box.remove(&child);
    }
    if let Some(data) = plan_tasks {
        let progress = gtk::ProgressBar::new();
        let fraction = if data.total == 0 {
            0.0
        } else {
            data.done as f64 / data.total as f64
        };
        progress.set_fraction(fraction.clamp(0.0, 1.0));
        progress.set_show_text(true);
        progress.set_text(Some(&format!("{}/{}", data.done, data.total)));
        progress.add_css_class("tracking-progress");
        tracking_box.append(&progress);

        let summary = gtk::Label::new(Some(&format!(
            "{} ready • {} active • {} blocked",
            data.ready, data.in_progress, data.blocked
        )));
        summary.add_css_class("task-panel-note");
        summary.set_halign(gtk::Align::Start);
        tracking_box.append(&summary);

        let listbox = gtk::ListBox::new();
        listbox.add_css_class("tracking-list");
        listbox.set_selection_mode(gtk::SelectionMode::None);
        for task in data.tasks.iter().filter(|task| !task.is_done()).take(3) {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.add_css_class("tracking-row");

            let id = gtk::Label::new(Some(&task.id));
            id.add_css_class("tracking-id");
            id.set_halign(gtk::Align::Start);

            let title = gtk::Label::new(Some(&task.title));
            title.add_css_class("tracking-title");
            title.set_halign(gtk::Align::Start);
            title.set_hexpand(true);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);

            let badge = gtk::Label::new(Some(&task.status));
            badge.add_css_class("tracking-status");
            match task.status.as_str() {
                "in-progress" | "in_progress" | "doing" | "active" | "started" => {
                    badge.add_css_class("tracking-status-in-progress")
                }
                _ => badge.add_css_class("tracking-status-queued"),
            }

            row.append(&id);
            row.append(&title);
            row.append(&badge);
            listbox.append(&row);
        }
        tracking_box.append(&listbox);
        tracking_box.set_visible(!super::compact_mode_active());
    } else {
        tracking_box.set_visible(false);
    }
}

fn sidebar_task_details_visible(tasks_dock_enabled: bool, tab_was_revealed: bool) -> bool {
    !tasks_dock_enabled && tab_was_revealed
}

/// GTK-free half of the sidebar task-button callback. Keeping this adapter
/// executable lets the entry-surface harness invoke the same request builder
/// as a real button click without fabricating a lower-level planner request.
pub(crate) fn sidebar_task_launch_request(
    state: &AppState,
    tab_id: u32,
    task_name: impl Into<String>,
) -> crate::task_launch::TaskLaunchRequest {
    crate::task_launch::TaskLaunchRequest::for_discovered_tab(
        crate::task_launch::TaskLaunchSurface::Sidebar,
        state,
        tab_id,
        task_name,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        build_discovery_snapshot, classify_discovery_failure, prepare_discovery_snapshot,
        sidebar_task_details_visible, task_discovery_target_changed, DiscoveryFailureReason,
    };
    use crate::pane::PaneNode;
    use crate::workspace::{Tab, TabKind, WorkspaceAction};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    fn stub_tab(id: u32) -> Tab {
        Tab {
            id,
            name: "Test".to_string(),
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
    fn discovery_failure_reason_tab_not_found() {
        let state = Rc::new(RefCell::new(crate::AppState::new()));
        // Tab ID 99 is not in the state
        let reason = classify_discovery_failure(&state, 99);
        assert_eq!(reason, DiscoveryFailureReason::TabNotFound);
    }

    #[test]
    fn discovery_failure_reason_no_working_directory() {
        let mut app_state = crate::AppState::new();
        let tab = stub_tab(42);
        app_state.workspaces[0].tabs.push(tab);
        app_state.workspaces[0].active_tab = 42;
        let state = Rc::new(RefCell::new(app_state));

        let reason = classify_discovery_failure(&state, 42);
        // Stub pane has no leaves, so no SSH host can be found → NoWorkingDirectory
        assert_eq!(reason, DiscoveryFailureReason::NoWorkingDirectory);
    }

    #[test]
    fn discovery_failure_reason_messages_are_distinct() {
        // All three failure reasons must produce different, non-empty messages.
        let msgs = [
            DiscoveryFailureReason::TabNotFound.toast_message(),
            DiscoveryFailureReason::UnresolvedRemoteContext.toast_message(),
            DiscoveryFailureReason::NoWorkingDirectory.toast_message(),
        ];
        for msg in &msgs {
            assert!(!msg.is_empty(), "toast message must not be empty");
        }
        assert_ne!(msgs[0], msgs[1]);
        assert_ne!(msgs[1], msgs[2]);
        assert_ne!(msgs[0], msgs[2]);
    }

    fn remote_discovery_target(cwd: &str) -> crate::mise::DiscoveryTarget {
        crate::mise::DiscoveryTarget::Remote {
            host: "devbox".into(),
            cwd: cwd.into(),
            ssh_argv: vec!["ssh".into(), "devbox".into()],
        }
    }

    #[test]
    fn test_sidebar_discovery_handles_remote_target() {
        let _guard = crate::mise::task_discovery_test_guard();
        crate::mise::clear_task_discovery_cache_for_test();
        crate::mise::clear_discover_tasks_test_probe();

        let remote_cwd = "/srv/api";
        let snapshot = build_discovery_snapshot(
            &remote_discovery_target(remote_cwd),
            vec![crate::mise::MiseTask {
                name: "test".into(),
                description: "Run tests".into(),
                hide: false,
                source: None,
                global: false,
            }],
        );

        assert_eq!(snapshot.actions, vec![WorkspaceAction::Test]);
        assert!(snapshot.discovery_cwd.is_none());
        assert!(snapshot.tracking.is_none());
    }

    #[test]
    fn task_details_stay_out_of_sidebar_when_dock_is_enabled() {
        assert!(!sidebar_task_details_visible(true, true));
        assert!(!sidebar_task_details_visible(true, false));
        assert!(sidebar_task_details_visible(false, true));
        assert!(!sidebar_task_details_visible(false, false));
    }

    #[test]
    fn cwd_or_remote_location_change_invalidates_the_discovered_task_identity() {
        let local_a = crate::mise::DiscoveryTarget::Local {
            cwd: "/workspace/a".into(),
            binary_path: Some("/usr/bin/true".into()),
        };
        let local_b = crate::mise::DiscoveryTarget::Local {
            cwd: "/workspace/b".into(),
            binary_path: Some("/usr/bin/true".into()),
        };
        let snapshot = crate::task_launch::TaskDiscoverySnapshot::with_task_names(
            local_a.clone(),
            ["dev".to_string()],
        );
        assert!(!task_discovery_target_changed(
            Some(&snapshot),
            Some(&local_a)
        ));
        assert!(task_discovery_target_changed(
            Some(&snapshot),
            Some(&local_b)
        ));
        assert!(task_discovery_target_changed(Some(&snapshot), None));
    }

    #[test]
    fn test_sidebar_does_not_stall_on_slow_remote() {
        let _guard = crate::mise::task_discovery_test_guard();
        crate::mise::clear_task_discovery_cache_for_test();
        crate::mise::clear_discover_tasks_test_probe();

        let remote_cwd = "/srv/slow";
        let target = remote_discovery_target(remote_cwd);
        let probe_delay = Duration::from_secs(1);
        crate::mise::install_discover_tasks_test_probe(
            target.clone(),
            vec![crate::mise::MiseTask {
                name: "test".into(),
                description: "Run tests".into(),
                hide: false,
                source: None,
                global: false,
            }],
            probe_delay,
        );

        let started_at = Instant::now();
        let (request, tasks) = prepare_discovery_snapshot(&target);

        assert!(
            started_at.elapsed() < probe_delay / 4,
            "sidebar discovery should not block on slow remote discovery"
        );
        assert!(matches!(
            request,
            crate::mise::TaskDiscoveryRequest::Start | crate::mise::TaskDiscoveryRequest::Pending
        ));
        assert!(tasks.is_empty());

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let crate::mise::CachedTaskDiscovery::Ready(tasks) =
                crate::mise::cached_task_discovery(&target)
            {
                assert_eq!(crate::mise::discover_tasks_test_probe_call_count(), 1);
                assert_eq!(
                    build_discovery_snapshot(&target, tasks).actions,
                    vec![WorkspaceAction::Test]
                );
                crate::mise::clear_discover_tasks_test_probe();
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        crate::mise::clear_discover_tasks_test_probe();
        panic!("timed out waiting for remote sidebar discovery to finish");
    }
}
