use adw::prelude::*;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::tracking::TrackingData;
use crate::workspace::{Tab, Workspace, WorkspaceTaskButton};
use crate::AppState;

#[derive(Clone)]
pub struct WorkspaceInspector {
    pub container: gtk::CenterBox,
    title: gtk::Label,
    subtitle: gtk::Label,
    body: gtk::Box,
    term_stack: gtk::Stack,
    last_snapshot: Rc<RefCell<Option<WorkspaceInspectorData>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceInspectorData {
    workspace_name: String,
    subtitle: String,
    summary: WorkspaceSummary,
    tracking: Option<TrackingData>,
    tabs: Vec<InspectorTabRow>,
    ports: Vec<InspectorPortRow>,
    repo: WorkspaceRepoContext,
    quick_actions: Vec<WorkspaceTaskButton>,
    quick_action_source: Option<String>,
    quick_action_source_tab_id: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceSummary {
    tab_count: usize,
    pane_count: usize,
    running_count: usize,
    attention_count: usize,
    port_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceRepoContext {
    active_tab_name: Option<String>,
    branch_name: Option<String>,
    repo_root: Option<String>,
    worktree_path: Option<String>,
    linked_issue: Option<String>,
    host_config_name: Option<String>,
    env_var_count: usize,
    is_worktree: bool,
    tmux_backed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InspectorTabRow {
    name: String,
    is_active: bool,
    pane_count: usize,
    status: InspectorTabStatus,
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InspectorTabStatus {
    Attention,
    Running,
    Done,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InspectorPortRow {
    port: u16,
    tabs: Vec<String>,
}

impl WorkspaceInspector {
    pub fn show(
        &self,
        state: &Rc<RefCell<AppState>>,
        tab_list: &gtk::Box,
        window: &adw::ApplicationWindow,
    ) {
        self.refresh(state, tab_list, window);
        self.container.set_visible(true);
        self.container.grab_focus();
    }

    pub fn hide(&self, state: &Rc<RefCell<AppState>>) {
        self.container.set_visible(false);
        if let Some(terminal) = crate::get_active_terminal(state) {
            terminal.grab_focus();
        }
    }

    pub fn refresh_if_visible(
        &self,
        state: &Rc<RefCell<AppState>>,
        tab_list: &gtk::Box,
        window: &adw::ApplicationWindow,
    ) {
        if self.container.is_visible() {
            self.refresh(state, tab_list, window);
        }
    }

    pub fn refresh(
        &self,
        state: &Rc<RefCell<AppState>>,
        tab_list: &gtk::Box,
        window: &adw::ApplicationWindow,
    ) {
        let next = {
            let st = state.borrow();
            st.active_ws()
                .map(|workspace| snapshot_workspace_for_state(&st, workspace))
        };

        let mut last = self.last_snapshot.borrow_mut();
        if *last == next {
            return;
        }
        *last = next.clone();
        drop(last);

        while let Some(child) = self.body.first_child() {
            self.body.remove(&child);
        }

        let Some(data) = next else {
            self.title.set_text("Workspace Inspector");
            self.subtitle.set_text("No active workspace");
            self.subtitle.set_visible(true);
            self.body
                .append(&empty_state_label("No workspace is currently active."));
            return;
        };

        self.title.set_text(&data.workspace_name);
        self.subtitle.set_text(&data.subtitle);
        self.subtitle.set_visible(!data.subtitle.is_empty());

        // `.plan` task tracking and the mise quick-action buttons are opt-in:
        // shown when `[tasks] enabled`, or when the source tab was explicitly
        // revealed via "Discover Tasks" this session. The rest of the inspector
        // (summary/activity/ports/repo) is always shown.
        let tasks_enabled = crate::config::tasks_config().enabled
            || data
                .quick_action_source_tab_id
                .is_some_and(|id| state.borrow().revealed_task_tabs.contains(&id));

        self.body.append(&build_summary_section(&data.summary));
        if tasks_enabled {
            self.body.append(&build_tracking_section(&data.tracking));
        }
        self.body.append(&build_activity_section(&data.tabs));
        self.body.append(&build_ports_section(&data.ports));
        self.body.append(&build_repo_section(&data.repo));
        if tasks_enabled {
            self.body.append(&build_quick_actions_section(
                &data,
                state,
                tab_list,
                &self.term_stack,
                window,
            ));
        }
    }
}

pub fn build_workspace_inspector(
    state: Rc<RefCell<AppState>>,
    tab_list: gtk::Box,
    term_stack: gtk::Stack,
    window: adw::ApplicationWindow,
) -> WorkspaceInspector {
    let title = gtk::Label::new(Some("Workspace Inspector"));
    title.set_halign(gtk::Align::Start);
    title.add_css_class("workspace-inspector-title");

    let subtitle = gtk::Label::new(None);
    subtitle.set_halign(gtk::Align::Start);
    subtitle.add_css_class("workspace-inspector-subtitle");
    subtitle.set_wrap(true);

    let title_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    title_box.append(&title);
    title_box.append(&subtitle);

    let refresh_button = gtk::Button::with_label("Refresh");
    refresh_button.add_css_class("workspace-inspector-header-button");

    let close_button = gtk::Button::with_label("Close");
    close_button.add_css_class("workspace-inspector-header-button");

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.append(&refresh_button);
    actions.append(&close_button);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    header.add_css_class("workspace-inspector-header");
    header.append(&title_box);
    header.append(&actions);

    title_box.set_hexpand(true);
    actions.set_halign(gtk::Align::End);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 14);
    body.add_css_class("workspace-inspector-body");

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .child(&body)
        .build();
    scroll.set_vexpand(true);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 16);
    card.add_css_class("workspace-inspector");
    card.set_margin_top(24);
    card.set_margin_bottom(24);
    card.set_margin_start(24);
    card.set_margin_end(24);
    card.set_size_request(720, 520);
    card.append(&header);
    card.append(&scroll);

    let container = gtk::CenterBox::new();
    container.set_orientation(gtk::Orientation::Vertical);
    container.add_css_class("workspace-inspector-backdrop");
    container.set_hexpand(true);
    container.set_vexpand(true);
    container.set_focusable(true);
    container.set_visible(false);
    container.set_center_widget(Some(&card));

    let inspector = WorkspaceInspector {
        container,
        title,
        subtitle,
        body,
        term_stack,
        last_snapshot: Rc::new(RefCell::new(None)),
    };

    {
        let inspector = inspector.clone();
        let state = state.clone();
        close_button.connect_clicked(move |_| {
            inspector.hide(&state);
        });
    }

    {
        let inspector = inspector.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let window = window.clone();
        refresh_button.connect_clicked(move |_| {
            if let Some(tab_id) = state.borrow().active_tab().map(|tab| tab.id) {
                // Header refresh re-reads data without newly revealing task UI.
                crate::sidebar::discover_for_tab(&state, &tab_list, tab_id, false);
            }
            inspector.refresh(&state, &tab_list, &window);
        });
    }

    {
        let inspector = inspector.clone();
        let state = state.clone();
        let key_ctrl = gtk::EventControllerKey::new();
        let inspector_for_keys = inspector.clone();
        key_ctrl.connect_key_pressed(move |_ctrl, key, _code, _mods| {
            if key == gdk::Key::Escape {
                inspector_for_keys.hide(&state);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        inspector.container.add_controller(key_ctrl);
    }

    {
        let inspector = inspector.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let window = window.clone();
        glib::timeout_add_seconds_local(1, move || {
            inspector.refresh_if_visible(&state, &tab_list, &window);
            glib::ControlFlow::Continue
        });
    }

    inspector
}

#[cfg(test)]
fn snapshot_workspace(workspace: &Workspace) -> WorkspaceInspectorData {
    snapshot_workspace_with_attention(workspace, |tab| tab.needs_attention)
}

fn snapshot_workspace_for_state(state: &AppState, workspace: &Workspace) -> WorkspaceInspectorData {
    snapshot_workspace_with_attention(workspace, |tab| state.tab_needs_attention(tab))
}

fn snapshot_workspace_with_attention(
    workspace: &Workspace,
    needs_attention: impl Fn(&crate::Tab) -> bool,
) -> WorkspaceInspectorData {
    let active_tab = workspace
        .tabs
        .iter()
        .find(|tab| tab.id == workspace.active_tab);
    let tracking = tracking_for_workspace(workspace, active_tab);
    let (quick_actions, quick_action_source, quick_action_source_tab_id) =
        quick_actions_for_workspace(workspace, active_tab);
    let ports = collect_ports(workspace);

    let summary = WorkspaceSummary {
        tab_count: workspace.tabs.len(),
        pane_count: workspace
            .tabs
            .iter()
            .map(|tab| tab.panes.leaf_count())
            .sum(),
        running_count: workspace
            .tabs
            .iter()
            .filter(|tab| {
                matches!(
                    if needs_attention(tab) {
                        crate::workspace::TabPrimaryState::Alert
                    } else {
                        tab.primary_state()
                    },
                    crate::workspace::TabPrimaryState::Running
                )
            })
            .count(),
        attention_count: workspace
            .tabs
            .iter()
            .filter(|tab| needs_attention(tab))
            .count(),
        port_count: ports.len(),
    };

    let repo = WorkspaceRepoContext {
        active_tab_name: active_tab.map(|tab| tab.name.clone()),
        branch_name: workspace.branch_name.clone(),
        repo_root: workspace.repo_root.clone(),
        worktree_path: workspace.working_tree_path.clone(),
        linked_issue: workspace.linked_issue.clone(),
        host_config_name: workspace.host_config_name.clone(),
        env_var_count: workspace.env_vars.len(),
        is_worktree: workspace.is_worktree,
        tmux_backed: workspace.tmux_backed,
    };

    let subtitle = build_workspace_subtitle(&repo);
    let tabs = workspace
        .tabs
        .iter()
        .map(|tab| snapshot_tab_row(tab, tab.id == workspace.active_tab))
        .collect();

    WorkspaceInspectorData {
        workspace_name: workspace.name.clone(),
        subtitle,
        summary,
        tracking,
        tabs,
        ports,
        repo,
        quick_actions,
        quick_action_source,
        quick_action_source_tab_id,
    }
}

fn tracking_for_workspace(workspace: &Workspace, active_tab: Option<&Tab>) -> Option<TrackingData> {
    active_tab
        .and_then(|tab| tab.tracking_data.clone())
        .or_else(|| {
            workspace
                .tabs
                .iter()
                .find_map(|tab| tab.tracking_data.clone())
        })
}

/// Maximum number of mise task chips shown in the inspector when no pinned-tasks config
/// is present. Pinned tasks (from `.taarof.config`) are shown as-is without a cap.
const MAX_TASK_CHIPS: usize = 3;

fn quick_actions_for_workspace(
    workspace: &Workspace,
    active_tab: Option<&Tab>,
) -> (Vec<WorkspaceTaskButton>, Option<String>, Option<u32>) {
    let source = active_tab
        .filter(|tab| has_task_buttons(tab))
        .or_else(|| workspace.tabs.iter().find(|tab| has_task_buttons(tab)));

    match source {
        Some(tab) => {
            let mut buttons = resolved_task_buttons(tab);
            buttons.truncate(MAX_TASK_CHIPS);
            (buttons, Some(tab.name.clone()), Some(tab.id))
        }
        None => (Vec::new(), active_tab.map(|tab| tab.name.clone()), None),
    }
}

fn has_task_buttons(tab: &Tab) -> bool {
    !tab.task_buttons.is_empty() || !tab.discovered_actions.is_empty()
}

fn resolved_task_buttons(tab: &Tab) -> Vec<WorkspaceTaskButton> {
    if !tab.task_buttons.is_empty() {
        return tab.task_buttons.clone();
    }

    tab.discovered_actions
        .iter()
        .copied()
        .map(WorkspaceTaskButton::from_action)
        .collect()
}

fn collect_ports(workspace: &Workspace) -> Vec<InspectorPortRow> {
    let mut ports = BTreeMap::<u16, Vec<String>>::new();
    for tab in &workspace.tabs {
        for port in &tab.listening_ports {
            ports.entry(*port).or_default().push(tab.name.clone());
        }
    }

    ports
        .into_iter()
        .map(|(port, tabs)| InspectorPortRow { port, tabs })
        .collect()
}

fn build_workspace_subtitle(repo: &WorkspaceRepoContext) -> String {
    let mut parts = Vec::new();

    if let Some(branch) = repo
        .branch_name
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(branch.to_string());
    }
    if repo.is_worktree {
        parts.push("worktree".into());
    }
    if repo.tmux_backed {
        parts.push("tmux mode".into());
    }
    if let Some(tab_name) = repo
        .active_tab_name
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(format!("active tab: {tab_name}"));
    }

    parts.join(" • ")
}

fn snapshot_tab_row(tab: &Tab, is_active: bool) -> InspectorTabRow {
    let pane_count = tab.panes.leaf_count();
    let mut detail_parts = Vec::new();

    if let Some(activity) = tab.agent_activity.as_ref() {
        detail_parts.push(activity.text.clone());
        if let Some(source) = activity.source.as_deref().filter(|value| !value.is_empty()) {
            detail_parts.push(source.to_string());
        }
    } else if let Some(agent_name) = tab.agent_name.as_deref().filter(|value| !value.is_empty()) {
        detail_parts.push(agent_name.to_string());
    }

    if let Some(action) = tab.workspace_action {
        detail_parts.push(format!("task {}", action.task_name()));
    }

    if !tab.listening_ports.is_empty() {
        detail_parts.push(crate::agents::format_ports_label(&tab.listening_ports));
    }

    detail_parts.push(format!(
        "{pane_count} {}",
        pluralize(pane_count, "pane", "panes")
    ));

    InspectorTabRow {
        name: tab.name.clone(),
        is_active,
        pane_count,
        status: tab_status(tab),
        detail: detail_parts.join(" • "),
    }
}

fn tab_status(tab: &Tab) -> InspectorTabStatus {
    match tab.primary_state() {
        crate::workspace::TabPrimaryState::Alert => InspectorTabStatus::Attention,
        crate::workspace::TabPrimaryState::Running => InspectorTabStatus::Running,
        crate::workspace::TabPrimaryState::Done => InspectorTabStatus::Done,
        crate::workspace::TabPrimaryState::Idle => InspectorTabStatus::Idle,
    }
}

fn build_summary_section(summary: &WorkspaceSummary) -> gtk::Widget {
    let section = build_section("Summary");
    let body = section_body(&section);

    let stats = gtk::FlowBox::new();
    stats.add_css_class("workspace-inspector-stats");
    stats.set_selection_mode(gtk::SelectionMode::None);
    stats.set_column_spacing(10);
    stats.set_row_spacing(10);
    stats.set_max_children_per_line(5);

    for (value, label) in [
        (
            summary.tab_count.to_string(),
            pluralized_label(summary.tab_count, "Tab", "Tabs"),
        ),
        (
            summary.pane_count.to_string(),
            pluralized_label(summary.pane_count, "Pane", "Panes"),
        ),
        (summary.running_count.to_string(), "Running".into()),
        (summary.attention_count.to_string(), "Attention".into()),
        (
            summary.port_count.to_string(),
            pluralized_label(summary.port_count, "Port", "Ports"),
        ),
    ] {
        let chip = summary_chip(&value, &label);
        stats.insert(&chip, -1);
    }

    body.append(&stats);
    section.upcast()
}

fn build_tracking_section(tracking: &Option<TrackingData>) -> gtk::Widget {
    let section = build_section("Progress");
    let body = section_body(&section);

    if let Some(data) = tracking {
        let progress = gtk::ProgressBar::new();
        progress.set_fraction(data.percent_complete().clamp(0.0, 1.0));
        progress.set_show_text(true);
        progress.set_text(Some(&format!("{}/{}", data.done, data.total)));
        progress.add_css_class("tracking-progress");
        body.append(&progress);

        let summary = gtk::Label::new(Some(&format!(
            "{} done • {} in progress • {} queued",
            data.done, data.in_progress, data.queued
        )));
        summary.set_halign(gtk::Align::Start);
        summary.add_css_class("workspace-inspector-note");
        body.append(&summary);

        let listbox = gtk::ListBox::new();
        listbox.add_css_class("tracking-list");
        listbox.set_selection_mode(gtk::SelectionMode::None);
        for feature in &data.features {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.add_css_class("tracking-row");

            let id = gtk::Label::new(Some(&feature.id));
            id.add_css_class("tracking-id");
            id.set_halign(gtk::Align::Start);

            let title = gtk::Label::new(Some(&feature.title));
            title.add_css_class("tracking-title");
            title.set_halign(gtk::Align::Start);
            title.set_hexpand(true);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);

            let badge = status_badge(&feature.status, tracking_status_class(&feature.status));
            badge.add_css_class("tracking-status");

            row.append(&id);
            row.append(&title);
            row.append(&badge);
            listbox.append(&row);
        }
        body.append(&listbox);
    } else {
        body.append(&empty_state_label(
            "No feature tracking data found for this workspace yet.",
        ));
    }

    section.upcast()
}

fn build_activity_section(tabs: &[InspectorTabRow]) -> gtk::Widget {
    let section = build_section("Activity");
    let body = section_body(&section);

    if tabs.is_empty() {
        body.append(&empty_state_label("No tabs in this workspace."));
        return section.upcast();
    }

    let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
    for tab in tabs {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 4);
        row.add_css_class("workspace-inspector-tab-row");
        if tab.is_active {
            row.add_css_class("active");
        }

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let name = gtk::Label::new(Some(&tab.name));
        name.set_halign(gtk::Align::Start);
        name.set_hexpand(true);
        name.add_css_class("workspace-inspector-tab-name");

        let badge = status_badge(tab_status_label(tab.status), tab_status_class(tab.status));
        top.append(&name);
        if tab.is_active {
            top.append(&status_badge("Active", "workspace-inspector-badge-active"));
        }
        top.append(&badge);

        let detail = gtk::Label::new(Some(&tab.detail));
        detail.set_halign(gtk::Align::Start);
        detail.set_wrap(true);
        detail.add_css_class("workspace-inspector-tab-detail");

        row.append(&top);
        row.append(&detail);
        list.append(&row);
    }

    body.append(&list);
    section.upcast()
}

fn build_ports_section(ports: &[InspectorPortRow]) -> gtk::Widget {
    let section = build_section("Ports");
    let body = section_body(&section);

    if ports.is_empty() {
        body.append(&empty_state_label("No listening ports detected."));
        return section.upcast();
    }

    let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
    for entry in ports {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("workspace-inspector-port-row");

        let port = gtk::Label::new(Some(&format!(":{}", entry.port)));
        port.add_css_class("workspace-inspector-port");
        port.set_halign(gtk::Align::Start);

        let tabs = gtk::Label::new(Some(&entry.tabs.join(", ")));
        tabs.set_halign(gtk::Align::Start);
        tabs.set_wrap(true);
        tabs.set_hexpand(true);
        tabs.add_css_class("workspace-inspector-port-tabs");

        row.append(&port);
        row.append(&tabs);
        list.append(&row);
    }

    body.append(&list);
    section.upcast()
}

fn build_repo_section(repo: &WorkspaceRepoContext) -> gtk::Widget {
    let section = build_section("Repo Context");
    let body = section_body(&section);

    let rows = [
        ("Active Tab", repo.active_tab_name.as_deref()),
        ("Branch", repo.branch_name.as_deref()),
        ("Repository", repo.repo_root.as_deref()),
        ("Worktree", repo.worktree_path.as_deref()),
        ("Host", repo.host_config_name.as_deref()),
        ("Issue", repo.linked_issue.as_deref()),
    ];

    let grid = gtk::Grid::new();
    grid.add_css_class("workspace-inspector-grid");
    grid.set_column_spacing(12);
    grid.set_row_spacing(8);

    let mut next_row = 0;
    for (label, value) in rows {
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            continue;
        };
        append_repo_row(&grid, next_row, label, value);
        next_row += 1;
    }

    append_repo_row(
        &grid,
        next_row,
        "tmux Mode",
        if repo.tmux_backed {
            "enabled"
        } else {
            "disabled"
        },
    );
    next_row += 1;

    if repo.env_var_count > 0 {
        append_repo_row(
            &grid,
            next_row,
            "Environment",
            &format!("{} vars", repo.env_var_count),
        );
        next_row += 1;
    }

    if repo.is_worktree && repo.worktree_path.is_none() {
        append_repo_row(&grid, next_row, "Worktree", "linked worktree");
        next_row += 1;
    }

    if next_row == 0 {
        body.append(&empty_state_label(
            "No repository metadata is attached to this workspace.",
        ));
    } else {
        body.append(&grid);
    }

    section.upcast()
}

fn build_quick_actions_section(
    data: &WorkspaceInspectorData,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) -> gtk::Widget {
    let section = build_section("Quick Actions");
    let body = section_body(&section);

    build_quick_action_source_note(&body, data);

    let actions = gtk::FlowBox::new();
    actions.set_selection_mode(gtk::SelectionMode::None);
    actions.set_column_spacing(8);
    actions.set_row_spacing(8);
    actions.set_max_children_per_line(6);

    build_task_action_chips(&actions, data, state, tab_list, term_stack, window);
    build_utility_action_chips(&actions, state, tab_list, window);
    build_workspace_toggle_chip(&actions, data, state);
    build_attention_chip(&actions, data, window);

    body.append(&actions);
    section.upcast()
}

/// Append a source note or empty-state label to `body` based on whether the
/// workspace has any cached quick actions.
fn build_quick_action_source_note(body: &gtk::Box, data: &WorkspaceInspectorData) {
    if data.quick_actions.is_empty() {
        let source_note = data
            .quick_action_source
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(|name| {
                format!("No cached conventional tasks for {name}. Use Refresh to probe again.")
            })
            .unwrap_or_else(|| {
                "No cached conventional tasks for the active workspace. Use Refresh to probe again."
                    .into()
            });
        body.append(&empty_state_label(&source_note));
    } else if let Some(source) = data
        .quick_action_source
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        let note = gtk::Label::new(Some(&format!("Actions sourced from {source}")));
        note.set_halign(gtk::Align::Start);
        note.add_css_class("workspace-inspector-note");
        body.append(&note);
    }
}

/// Insert one chip per cached workspace task into `actions`.
/// GTK-free half of the inspector task-chip callback.
pub(crate) fn inspector_task_launch_request(
    state: &AppState,
    source_tab_id: u32,
    task_name: impl Into<String>,
) -> crate::task_launch::TaskLaunchRequest {
    crate::task_launch::TaskLaunchRequest::for_discovered_tab(
        crate::task_launch::TaskLaunchSurface::Inspector,
        state,
        source_tab_id,
        task_name,
        None,
    )
}

fn build_task_action_chips(
    actions: &gtk::FlowBox,
    data: &WorkspaceInspectorData,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    for action in &data.quick_actions {
        let button = gtk::Button::with_label(&action.label);
        button.add_css_class("workspace-action-button");
        button.add_css_class("mise-task-chip");
        let task_name = action.task_name.clone();
        let source_tab_id = data.quick_action_source_tab_id;
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        button.connect_clicked(move |_| {
            let Some(source_tab_id) = source_tab_id else {
                crate::show_error_toast("No tab selected for this action");
                return;
            };
            let request = {
                let st = state.borrow();
                inspector_task_launch_request(&st, source_tab_id, task_name.clone())
            };
            if let Err(error) =
                crate::palette::launch_task(&request, &state, &tab_list, &term_stack, &window)
            {
                crate::show_error_toast(&error.to_string());
            }
        });
        actions.insert(&button, -1);
    }
}

/// Insert the fixed utility chips: Discover Tasks, New Tab, New tmux Tab.
fn build_utility_action_chips(
    actions: &gtk::FlowBox,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
) {
    {
        let state = state.clone();
        let tab_list = tab_list.clone();
        let inspector_refresh = gtk::Button::with_label("Discover Tasks");
        inspector_refresh.add_css_class("workspace-action-button");
        inspector_refresh.connect_clicked(move |_| {
            if let Some(tab_id) = state.borrow().active_tab().map(|tab| tab.id) {
                crate::task_panel::discover_tasks_explicitly(&state, &tab_list, tab_id);
            }
        });
        actions.insert(&inspector_refresh, -1);
    }
    {
        let window = window.clone();
        let button = gtk::Button::with_label("New Tab");
        button.add_css_class("workspace-action-button");
        button.connect_clicked(move |_| {
            crate::keybindings::activate(&window, crate::keybindings::Action::NewTab);
        });
        actions.insert(&button, -1);
    }
    {
        let window = window.clone();
        let button = gtk::Button::with_label("New tmux Tab");
        button.add_css_class("workspace-action-button");
        button.connect_clicked(move |_| {
            crate::keybindings::activate(&window, crate::keybindings::Action::NewTmuxTab);
        });
        actions.insert(&button, -1);
    }
}

/// Insert the tmux-workspace toggle chip.
fn build_workspace_toggle_chip(
    actions: &gtk::FlowBox,
    data: &WorkspaceInspectorData,
    state: &Rc<RefCell<AppState>>,
) {
    let ws_id = state.borrow().active_ws().map(|ws| ws.id);
    let state = state.clone();
    let button = gtk::Button::with_label(if data.repo.tmux_backed {
        "tmux Workspace: On"
    } else {
        "tmux Workspace: Off"
    });
    button.add_css_class("workspace-action-button");
    button.connect_clicked(move |button| {
        let Some(ws_id) = ws_id else {
            return;
        };
        if let Some(enabled) = crate::sidebar::toggle_workspace_tmux_backed(&state, ws_id) {
            button.set_label(if enabled {
                "tmux Workspace: On"
            } else {
                "tmux Workspace: Off"
            });
        }
    });
    actions.insert(&button, -1);
}

/// Insert the Jump Attention chip when the workspace has attention tabs.
fn build_attention_chip(
    actions: &gtk::FlowBox,
    data: &WorkspaceInspectorData,
    window: &adw::ApplicationWindow,
) {
    if data.summary.attention_count > 0 {
        let window = window.clone();
        let button = gtk::Button::with_label("Jump Attention");
        button.add_css_class("workspace-action-button");
        button.connect_clicked(move |_| {
            crate::keybindings::activate(&window, crate::keybindings::Action::JumpAttention);
        });
        actions.insert(&button, -1);
    }
}

fn build_section(title: &str) -> gtk::Box {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 10);
    container.add_css_class("workspace-inspector-section");

    let title_label = gtk::Label::new(Some(title));
    title_label.set_halign(gtk::Align::Start);
    title_label.add_css_class("workspace-inspector-section-title");

    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.add_css_class("workspace-inspector-section-body");
    body.set_widget_name("workspace-inspector-section-body");

    container.append(&title_label);
    container.append(&body);
    container
}

fn section_body(section: &gtk::Box) -> gtk::Box {
    section
        .last_child()
        .and_then(|child| child.downcast::<gtk::Box>().ok())
        .expect("workspace inspector section should have a body")
}

fn summary_chip(value: &str, label: &str) -> gtk::Widget {
    let chip = gtk::Box::new(gtk::Orientation::Vertical, 2);
    chip.add_css_class("workspace-inspector-stat");

    let value_label = gtk::Label::new(Some(value));
    value_label.add_css_class("workspace-inspector-stat-value");
    value_label.set_halign(gtk::Align::Start);

    let label_label = gtk::Label::new(Some(label));
    label_label.add_css_class("workspace-inspector-stat-label");
    label_label.set_halign(gtk::Align::Start);

    chip.append(&value_label);
    chip.append(&label_label);
    chip.upcast()
}

fn append_repo_row(grid: &gtk::Grid, row: i32, label: &str, value: &str) {
    let key = gtk::Label::new(Some(label));
    key.set_halign(gtk::Align::Start);
    key.add_css_class("workspace-inspector-grid-key");

    let value = gtk::Label::new(Some(value));
    value.set_halign(gtk::Align::Start);
    value.set_wrap(true);
    value.set_hexpand(true);
    value.add_css_class("workspace-inspector-grid-value");

    grid.attach(&key, 0, row, 1, 1);
    grid.attach(&value, 1, row, 1, 1);
}

fn status_badge(label: &str, class_name: &str) -> gtk::Label {
    let badge = gtk::Label::new(Some(label));
    badge.add_css_class("workspace-inspector-badge");
    badge.add_css_class(class_name);
    badge
}

fn tracking_status_class(status: &str) -> &'static str {
    match status {
        "done" => "tracking-status-done",
        "queued" => "tracking-status-queued",
        "in-progress" => "tracking-status-in-progress",
        _ => "workspace-inspector-badge-idle",
    }
}

fn tab_status_label(status: InspectorTabStatus) -> &'static str {
    match status {
        InspectorTabStatus::Attention => "Attention",
        InspectorTabStatus::Running => "Running",
        InspectorTabStatus::Done => "Ready",
        InspectorTabStatus::Idle => "Idle",
    }
}

fn tab_status_class(status: InspectorTabStatus) -> &'static str {
    match status {
        InspectorTabStatus::Attention => "workspace-inspector-badge-attention",
        InspectorTabStatus::Running => "workspace-inspector-badge-running",
        InspectorTabStatus::Done => "workspace-inspector-badge-done",
        InspectorTabStatus::Idle => "workspace-inspector-badge-idle",
    }
}

fn empty_state_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_halign(gtk::Align::Start);
    label.set_wrap(true);
    label.add_css_class("workspace-inspector-empty");
    label
}

fn pluralized_label(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        singular.into()
    } else {
        plural.into()
    }
}

fn pluralize<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

#[cfg(test)]
mod tests {
    use super::{snapshot_workspace, snapshot_workspace_for_state};
    use crate::pane::PaneNode;
    use crate::workspace::{Tab, Workspace, WorkspaceStatus};
    use crate::AppState;
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_repo() -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time");
        let root = std::env::temp_dir().join(format!("taarof-inspector-test-{}", ts.as_nanos()));
        fs::create_dir_all(&root).expect("create temp repo");
        root
    }

    fn write_features(repo_root: &Path, json: &str) {
        let plan_dir = repo_root.join(".plan");
        fs::create_dir_all(&plan_dir).expect("create .plan");
        fs::write(plan_dir.join("features.json"), json).expect("write features.json");
    }

    fn stub_tab(id: u32, name: &str) -> Tab {
        Tab {
            id,
            name: name.into(),
            work_origin: crate::workspace::new_tab_work_origin(),
            kind: crate::workspace::TabKind::Terminal,
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

    fn workspace(repo_root: Option<String>, tabs: Vec<Tab>, active_tab: u32) -> Workspace {
        Workspace {
            id: 1,
            work_origin: crate::workspace::new_workspace_work_origin(),
            name: "repo".into(),
            collapsed: false,
            repo_root,
            is_worktree: false,
            working_tree_path: None,
            branch_name: Some("main".into()),
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

    #[test]
    fn snapshot_uses_active_tab_quick_actions_first() {
        let mut first = stub_tab(1, "Shell 1");
        first.discovered_actions = vec![crate::WorkspaceAction::Dev];
        let mut second = stub_tab(2, "Shell 2");
        second.discovered_actions = vec![crate::WorkspaceAction::Test];

        let data = snapshot_workspace(&workspace(None, vec![first, second], 2));

        assert_eq!(
            data.quick_actions,
            vec![crate::workspace::WorkspaceTaskButton::from_action(
                crate::WorkspaceAction::Test,
            )]
        );
        assert_eq!(data.quick_action_source.as_deref(), Some("Shell 2"));
        assert_eq!(data.quick_action_source_tab_id, Some(2));
    }

    #[test]
    fn snapshot_counts_persistent_socket_attention() {
        let tab = stub_tab(1, "Shell");
        let mut state = AppState::new();
        state.workspaces = vec![workspace(None, vec![tab], 1)];
        state.active_workspace = 1;
        state.set_socket_notification(1, "Build done".to_string());

        let data = snapshot_workspace_for_state(&state, &state.workspaces[0]);

        assert_eq!(data.summary.attention_count, 1);
        assert_eq!(data.summary.running_count, 0);
    }

    #[test]
    fn snapshot_uses_inactive_tab_quick_actions_when_active_tab_is_empty() {
        let first = stub_tab(1, "Shell 1");
        let mut second = stub_tab(2, "Shell 2");
        second.discovered_actions = vec![crate::WorkspaceAction::Test];

        let data = snapshot_workspace(&workspace(None, vec![first, second], 1));

        assert_eq!(
            data.quick_actions,
            vec![crate::workspace::WorkspaceTaskButton::from_action(
                crate::WorkspaceAction::Test,
            )]
        );
        assert_eq!(data.quick_action_source.as_deref(), Some("Shell 2"));
        assert_eq!(data.quick_action_source_tab_id, Some(2));
    }

    #[test]
    fn snapshot_does_not_read_tracking_from_workspace_root_when_tab_cache_is_empty() {
        let repo = temp_repo();
        write_features(
            &repo,
            r#"{"features":[
                {"id":"F027","title":"Workspace Inspector","status":"in-progress"},
                {"id":"F028","title":"Sidebar Customization","status":"queued"}
            ]}"#,
        );

        let data = snapshot_workspace(&workspace(
            Some(repo.to_string_lossy().into_owned()),
            vec![stub_tab(1, "Shell 1")],
            1,
        ));

        assert!(
            data.tracking.is_none(),
            "the GTK inspector must only render tracking supplied by the tab cache"
        );
    }

    #[test]
    fn snapshot_aggregates_ports_across_tabs() {
        let mut first = stub_tab(1, "Shell 1");
        first.listening_ports = vec![3000, 8080];
        let mut second = stub_tab(2, "Shell 2");
        second.listening_ports = vec![8080, 5432];

        let data = snapshot_workspace(&workspace(None, vec![first, second], 1));

        assert_eq!(data.summary.port_count, 3);
        assert_eq!(data.ports.len(), 3);
        assert_eq!(data.ports[0].port, 3000);
        assert_eq!(data.ports[1].port, 5432);
        assert_eq!(data.ports[2].port, 8080);
        assert_eq!(
            data.ports[2].tabs,
            vec!["Shell 1".to_string(), "Shell 2".to_string()]
        );
    }

    // ── Mise task chips in the workspace inspector ──

    /// Inspector snapshot carries mise task chips when the active tab has task buttons.
    #[test]
    fn test_inspector_renders_mise_task_chips() {
        let mut tab = stub_tab(1, "dev-shell");
        tab.task_buttons = vec![
            crate::workspace::WorkspaceTaskButton {
                label: "Dev".into(),
                task_name: "dev".into(),
            },
            crate::workspace::WorkspaceTaskButton {
                label: "Test".into(),
                task_name: "test".into(),
            },
            crate::workspace::WorkspaceTaskButton {
                label: "Build".into(),
                task_name: "build".into(),
            },
        ];

        let data = snapshot_workspace(&workspace(None, vec![tab], 1));

        assert_eq!(data.quick_actions.len(), 3);
        assert_eq!(data.quick_actions[0].task_name, "dev");
        assert_eq!(data.quick_actions[1].task_name, "test");
        assert_eq!(data.quick_actions[2].task_name, "build");
        assert_eq!(data.quick_action_source.as_deref(), Some("dev-shell"));
        assert_eq!(data.quick_action_source_tab_id, Some(1));
    }

    /// When a tab carries `task_buttons` from `.taarof.config` pinned tasks, the inspector
    /// surfaces exactly those tasks (not the default top-3 standard actions).
    #[test]
    fn test_inspector_respects_pinned_tasks_from_project_config() {
        let mut tab = stub_tab(1, "shell");
        // Simulate what task_buttons_from_tasks produces when [mise].pinned_tasks = ["lint", "fmt"]
        tab.task_buttons = vec![
            crate::workspace::WorkspaceTaskButton {
                label: "lint".into(),
                task_name: "lint".into(),
            },
            crate::workspace::WorkspaceTaskButton {
                label: "fmt".into(),
                task_name: "fmt".into(),
            },
        ];

        let data = snapshot_workspace(&workspace(None, vec![tab], 1));

        assert_eq!(data.quick_actions.len(), 2);
        assert_eq!(data.quick_actions[0].task_name, "lint");
        assert_eq!(data.quick_actions[1].task_name, "fmt");
    }

    /// When the inspector snapshot has a task chip, the `quick_action_source_tab_id` is set
    /// so the click handler can resolve a discovery target and open a new tab for the task.
    #[test]
    fn test_inspector_chip_click_opens_new_tab() {
        let mut tab = stub_tab(7, "work-tab");
        tab.task_buttons = vec![crate::workspace::WorkspaceTaskButton {
            label: "Dev".into(),
            task_name: "dev".into(),
        }];

        let data = snapshot_workspace(&workspace(None, vec![tab], 7));

        // The chip is present and carries the correct task name.
        assert_eq!(data.quick_actions.len(), 1);
        assert_eq!(data.quick_actions[0].task_name, "dev");

        // The source tab id is set — this is what the click handler uses to call
        // `mise::discovery_target_for_tab` before passing the request through
        // the shared task-launch planner.
        assert_eq!(
            data.quick_action_source_tab_id,
            Some(7),
            "source_tab_id must be set so the click handler can resolve a discovery target"
        );

        // No mise project → no chips (empty-state path).
        let empty_tab = stub_tab(8, "empty-tab");
        let empty_data = snapshot_workspace(&workspace(None, vec![empty_tab], 8));
        assert!(
            empty_data.quick_actions.is_empty(),
            "no task buttons → no chips in inspector"
        );
    }
}
