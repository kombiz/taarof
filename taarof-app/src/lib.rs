mod agent_projection;
mod agent_sessions;
mod agents;
mod api;
mod app_runtime;
mod app_session;
mod attention;
mod browser;
mod child_env;
mod child_process;
pub mod config;
mod dashboard;
mod diagnostics;
mod events;
mod git;
pub mod history;
mod history_view;
mod host;
pub mod http;
mod inspector;
mod instance;
mod keybindings;
mod loop_runner;
mod mise;
mod palette;
mod pane;
mod peek;
#[cfg(feature = "harness")]
pub mod performance;
mod private_atomic_file;
mod probe;
mod project_config;
mod projects;
pub mod pty_broker;
mod runtime;
pub mod runtime_identity;
mod runtime_probe;
pub mod session;
mod sidebar;
pub mod socket;
mod sound;
mod ssh_config;
pub mod task_binding;
mod task_launch;
mod task_panel;
mod templates;
mod terminal;
mod tmux;
mod tracking;
mod update_watch;
mod views;
pub mod work_ledger;
pub mod work_reporting;
mod workspace;

use adw::prelude::*;
use vte::prelude::*;

// Tests that acquire or iterate the process-global GLib default main context
// (`glib::MainContext::default()`, `timeout_add_local*`, `idle_add_local*`,
// file monitors) race under cargo's parallel test threads: the local source
// helpers panic with "default main context already acquired by another thread"
// when another thread currently owns that single global context. Serialize
// those tests through one shared lock so only one runs at a time.
#[cfg(test)]
pub(crate) fn glib_main_context_test_guard() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock, PoisonError};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

// Global toast overlay reference for cross-module error notifications
// Uses thread-local storage since GTK widgets are not Send/Sync
use std::cell::RefCell;
thread_local! {
    static TOAST_OVERLAY: RefCell<Option<adw::ToastOverlay>> = const { RefCell::new(None) };
    static CSS_PROVIDERS: RefCell<Option<(gtk::CssProvider, gtk::CssProvider)>> = const { RefCell::new(None) };
    static PROJECT_REGISTRATION_INFLIGHT: Cell<bool> = const { Cell::new(false) };
}

/// Set the toast overlay reference (called once during build_ui)
fn set_toast_overlay(overlay: adw::ToastOverlay) {
    TOAST_OVERLAY.with(|o| {
        *o.borrow_mut() = Some(overlay);
    });
}

/// Show a transient error toast notification to the user.
/// This is the standard way to report user-facing operation failures.
/// Falls back to eprintln if toast infrastructure is not available.
pub fn show_error_toast(message: &str) {
    show_toast_with_timeout(message, 3);
}

/// Show a transient informational/success toast. Longer-lived than an error toast
/// so users can read a result (e.g. a freshly opened PR URL) before it dismisses.
pub fn show_toast(message: &str) {
    show_toast_with_timeout(message, 6);
}

fn show_toast_with_timeout(message: &str, timeout: u32) {
    TOAST_OVERLAY.with(|o| {
        if let Some(ref overlay) = *o.borrow() {
            let toast = adw::Toast::new(message);
            toast.set_timeout(timeout);
            overlay.add_toast(toast);
        } else {
            eprintln!("taarof: {message}");
        }
    });
}

/// Handles needed to materialize a lazily-restored tab that `sync_active_selection_ui`
/// does not otherwise have in scope. Set once in `build_ui` before any user
/// activation can occur, mirroring the `TOAST_OVERLAY` thread-local above.
struct LazyRestoreCtx {
    window: adw::ApplicationWindow,
    restore_legend_dismiss: Rc<dyn Fn(u32)>,
}

thread_local! {
    static LAZY_RESTORE_CTX: RefCell<Option<LazyRestoreCtx>> = const { RefCell::new(None) };
}

fn set_lazy_restore_ctx(window: adw::ApplicationWindow, restore_legend_dismiss: Rc<dyn Fn(u32)>) {
    LAZY_RESTORE_CTX.with(|ctx| {
        *ctx.borrow_mut() = Some(LazyRestoreCtx {
            window,
            restore_legend_dismiss,
        });
    });
}

/// If `tab_id` is a not-yet-spawned lazily-restored tab, build its real pane
/// tree and spawn its shells now. No-ops (returns `false`) when the tab is not
/// pending or the restore context has not been installed yet. Called from
/// `sidebar::sync_active_selection_ui`, which every activation path funnels
/// through, so this single hook covers tab switches, workspace switches, and
/// close-tab neighbour selection.
pub(crate) fn materialize_active_tab_if_pending(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    tab_id: u32,
) -> bool {
    LAZY_RESTORE_CTX.with(|ctx| {
        let ctx = ctx.borrow();
        let Some(ctx) = ctx.as_ref() else {
            return false;
        };
        terminal::materialize_pending_tab(
            state,
            term_stack,
            tab_list,
            &ctx.window,
            tab_id,
            Some(ctx.restore_legend_dismiss.clone()),
        )
    })
}

use std::cell::Cell;
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

#[cfg(test)]
use app_session::normalize_persisted_path;
use app_session::workspace_git_discovery_path;
pub(crate) use runtime::ChordMode;
pub use runtime::{AppState, BroadcastScope, MoveTabError, RuntimeHandle};
pub use terminal::{build_send_to_pane_payload, SendToPaneMode};
#[cfg(any(test, feature = "harness", debug_assertions))]
pub use terminal::{exercise_broadcast_commit_reentrancy_harness, BroadcastCommitHarnessResult};
pub use terminal::{plan_restored_spawns, AgentResumeOffer, PlannedRestoreSpawn};
#[doc(hidden)]
pub use tracking::{
    BranchPullRequestEntry, BranchPullRequestsData, PullRequestChecks,
    PullRequestCorrelationMetadata,
};
pub use workspace::{Tab, TabKind, Workspace, WorkspaceAction, WorkspaceStatus};

#[doc(hidden)]
pub fn reset_diagnostics_journal_for_tests(
    log_path: Option<PathBuf>,
    archive_path: Option<PathBuf>,
) {
    diagnostics::reset_journal_for_tests(log_path, archive_path);
}

const PCRE2_CASELESS: u32 = 0x0000_0008;
const PCRE2_MULTILINE: u32 = 0x0000_0400;

#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct HeadlessPaneSeed {
    pub cwd: Option<String>,
    pub cwd_host: Option<String>,
    pub shell_running: bool,
    pub has_child_process: bool,
    pub remote_shell: bool,
    pub ssh_command: Option<Vec<String>>,
    pub tmux_session: Option<String>,
    pub tmux_ssh_target: Option<String>,
}

#[doc(hidden)]
pub fn seed_headless_terminal_tab(
    state: &mut AppState,
    workspace_id: u32,
    tab_name: &str,
    pane: HeadlessPaneSeed,
) -> Option<(u32, u32)> {
    let HeadlessPaneSeed {
        cwd,
        cwd_host,
        shell_running,
        has_child_process,
        remote_shell,
        ssh_command,
        tmux_session,
        tmux_ssh_target,
    } = pane;
    let tab_id = state.next_id;
    state.next_id += 1;
    let pane_id = state.next_id;
    state.next_id += 1;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let tmux_backing = tmux_session.map(|session_name| pane::TmuxBacking {
        session_name,
        target: match tmux_ssh_target {
            Some(ssh_target) => tmux::TmuxTarget::Remote { ssh_target },
            None => tmux::TmuxTarget::Local,
        },
        expected_generation: None,
        pane_info: probe::ProbeSnapshot::default(),
    });
    let workspace_idx = state
        .workspaces
        .iter()
        .position(|ws| ws.id == workspace_id)?;
    state.headless_panes.insert(
        (tab_id, pane_id),
        runtime::HeadlessPaneState {
            work_origin: crate::pane::new_pane_work_origin(),
            shell_running,
            tmux_backing: tmux_backing.clone(),
            location_state: pane::PaneLocationState {
                cwd: cwd.clone(),
                cwd_host: cwd_host.clone(),
                updated_at_unix_ms: Some(now),
            },
            process_state: pane::PaneProcessState {
                has_child_process,
                remote_shell,
                ssh_command: ssh_command.clone(),
                updated_at_unix_ms: Some(now),
            },
            current_task: None,
        },
    );
    let workspace = &mut state.workspaces[workspace_idx];
    if workspace.active_tab != 0 {
        workspace.last_active_tab = Some(workspace.active_tab);
    }
    workspace.tabs.push(Tab {
        id: tab_id,
        name: tab_name.to_string(),
        work_origin: crate::workspace::new_tab_work_origin(),
        kind: TabKind::Terminal,
        panes: Box::new(pane::PaneNode::Stub { pane_id }),
        focused_pane_id: pane_id,
        next_pane_id: pane_id + 1,
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
        pane_agent_activity: std::collections::HashMap::new(),
        pane_explicit_observation: std::collections::HashMap::new(),
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
    });
    workspace.active_tab = tab_id;
    state.active_workspace = workspace_id;
    Some((tab_id, pane_id))
}

/// Test seam mirroring [`seed_headless_terminal_tab`] for lazy session restore
/// (EXAMPLE-84): push a Terminal tab whose live `panes` is `PaneNode::Empty` and
/// stash its saved layout in `pending_tab_restores`, so headless tests can
/// exercise the pending-tab save round-trip without any GTK widgets.
#[doc(hidden)]
pub fn seed_pending_restore_tab(
    state: &mut AppState,
    workspace_id: u32,
    tab_name: &str,
    mut saved: session::SavedPaneNode,
    cwd: Option<String>,
) -> Option<u32> {
    saved.normalize_work_origins();
    let tab_id = state.next_id;
    state.next_id += 1;
    let workspace_idx = state
        .workspaces
        .iter()
        .position(|ws| ws.id == workspace_id)?;
    let workspace = &mut state.workspaces[workspace_idx];
    if workspace.active_tab != 0 {
        workspace.last_active_tab = Some(workspace.active_tab);
    }
    workspace.tabs.push(Tab {
        id: tab_id,
        name: tab_name.to_string(),
        work_origin: crate::workspace::new_tab_work_origin(),
        kind: TabKind::Terminal,
        panes: Box::new(pane::PaneNode::Empty),
        focused_pane_id: 0,
        next_pane_id: 0,
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
        pane_agent_activity: std::collections::HashMap::new(),
        pane_explicit_observation: std::collections::HashMap::new(),
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
    });
    workspace.active_tab = tab_id;
    state.active_workspace = workspace_id;
    state.pending_tab_restores.insert(
        tab_id,
        session::PendingTabRestore {
            saved,
            cwd,
            show_restore_legend: false,
        },
    );
    Some(tab_id)
}

/// Test seam: snapshot the canonical `AppState` into the serializable v2 session
/// payload, exercising the same pure path the autosave uses. GTK-free.
#[doc(hidden)]
pub fn snapshot_session_state_for_test(state: &AppState) -> session::SessionStateV2 {
    app_session::build_session_state_v2(state, None, 1200, 800)
}

const DESKTOP_APP_ID: &str = "io.github.kombiz.taarof";

pub(crate) fn apply_git_info_to_workspace(workspace: &mut Workspace, git_info: &git::GitInfo) {
    workspace.repo_root = git_info.repo_root.clone();
    workspace.branch_name = git_info.branch.clone();
    workspace.is_worktree = git_info.is_linked_worktree();
    workspace.working_tree_path = git_info.working_tree_path().map(str::to_string);
}

pub(crate) fn workspace_name_from_git_info(git_info: &git::GitInfo) -> Option<String> {
    git_info.workspace_root().map(git::workspace_name_from_repo)
}

/// The sidebar's shipped workspace label carries both the user-facing name and
/// the freshly observed Git state. Call this only from the async apply side so
/// launch-to-first-usable-tab never waits on Git filesystem reads.
pub(crate) fn workspace_header_label(name: &str, git_info: &git::GitInfo) -> String {
    let status = git::format_status(git_info);
    if status.is_empty() {
        name.to_string()
    } else {
        format!("{name}  {status}")
    }
}

fn workspace_action_from_chord_key(
    key: gdk::Key,
    chord_map: &BTreeMap<char, WorkspaceAction>,
) -> Option<WorkspaceAction> {
    let ch = keybindings::followup_key_from_keyval(key)?;
    chord_map.get(&ch).copied()
}

fn leader_action_from_chord_key(
    key: gdk::Key,
    leader_map: &BTreeMap<char, keybindings::Action>,
) -> Option<keybindings::Action> {
    let ch = keybindings::followup_key_from_keyval(key)?;
    leader_map.get(&ch).copied()
}

const RESTORE_LEGEND_TIMEOUT: Duration = Duration::from_secs(10);

struct RestoreLegendUi {
    container: gtk::Box,
    items_box: gtk::Box,
    state: Rc<RefCell<AppState>>,
    last_active_tab: Cell<Option<u32>>,
    rendered_tab: Cell<Option<u32>>,
    generation: Cell<u64>,
}

impl RestoreLegendUi {
    fn new(state: Rc<RefCell<AppState>>) -> Rc<Self> {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 8);
        container.add_css_class("restore-legend-overlay");
        container.set_halign(gtk::Align::Center);
        container.set_valign(gtk::Align::Start);
        container.set_margin_top(18);
        container.set_visible(false);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.add_css_class("restore-legend-header");

        let title = gtk::Label::new(Some("Restored panes"));
        title.add_css_class("restore-legend-title");
        title.set_halign(gtk::Align::Start);
        title.set_hexpand(true);
        title.set_xalign(0.0);

        let close_button = gtk::Button::from_icon_name("window-close-symbolic");
        close_button.add_css_class("flat");
        close_button.add_css_class("restore-legend-close");
        close_button.set_focus_on_click(false);

        header.append(&title);
        header.append(&close_button);

        let items_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        items_box.add_css_class("restore-legend-items");

        container.append(&header);
        container.append(&items_box);

        let overlay = Rc::new(Self {
            container,
            items_box,
            state,
            last_active_tab: Cell::new(None),
            rendered_tab: Cell::new(None),
            generation: Cell::new(0),
        });

        {
            let overlay = overlay.clone();
            close_button.connect_clicked(move |_| {
                overlay.dismiss_active(true);
            });
        }

        overlay
    }

    fn bump_generation(&self) -> u64 {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        generation
    }

    fn hide(&self) {
        self.bump_generation();
        self.rendered_tab.set(None);
        self.container.set_visible(false);
    }

    fn populate(&self, legend: &session::RestoredTabLegend) {
        while let Some(child) = self.items_box.first_child() {
            self.items_box.remove(&child);
        }

        for item in &legend.items {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.add_css_class("restore-legend-row");

            let number = gtk::Label::new(Some(&item.number.to_string()));
            number.add_css_class("restore-legend-number");
            number.set_xalign(0.0);
            row.append(&number);

            if item.is_ssh {
                let badge = gtk::Label::new(Some("ssh"));
                badge.add_css_class("restore-legend-badge");
                row.append(&badge);
            }

            let label = gtk::Label::new(Some(&item.label));
            label.add_css_class("restore-legend-label");
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            row.append(&label);

            self.items_box.append(&row);
        }
    }

    fn dismiss_tab(&self, tab_id: u32, refocus_terminal: bool) {
        self.state.borrow_mut().dismiss_restored_legend(tab_id);
        if self.rendered_tab.get() == Some(tab_id) {
            self.hide();
            if refocus_terminal {
                if let Some(terminal) = get_active_terminal(&self.state) {
                    terminal.grab_focus();
                }
            }
        }
    }

    fn dismiss_active(&self, refocus_terminal: bool) {
        if let Some(tab_id) = self.rendered_tab.get() {
            self.dismiss_tab(tab_id, refocus_terminal);
        }
    }

    fn refresh(self: &Rc<Self>) {
        let active_tab_id = {
            let st = self.state.borrow();
            st.active_tab().map(|tab| tab.id)
        };

        let previous_active = self.last_active_tab.replace(active_tab_id);
        if previous_active != active_tab_id {
            if let Some(previous_tab_id) = previous_active {
                self.state
                    .borrow_mut()
                    .dismiss_restored_legend(previous_tab_id);
            }
        }

        let legend = active_tab_id.and_then(|tab_id| {
            let st = self.state.borrow();
            st.restored_legend(tab_id).cloned()
        });

        let Some(legend) = legend else {
            self.hide();
            return;
        };

        if self.rendered_tab.get() == Some(legend.tab_id) && self.container.is_visible() {
            return;
        }

        self.populate(&legend);
        self.rendered_tab.set(Some(legend.tab_id));
        self.container.set_visible(true);

        let generation = self.bump_generation();
        let overlay = self.clone();
        let tab_id = legend.tab_id;
        glib::timeout_add_local(RESTORE_LEGEND_TIMEOUT, move || {
            if overlay.generation.get() == generation && overlay.rendered_tab.get() == Some(tab_id)
            {
                overlay.dismiss_tab(tab_id, false);
            }
            glib::ControlFlow::Break
        });
    }
}

struct ChordOverlayEntry {
    key_label: String,
    label: String,
    target: keybindings::ChordTarget,
}

fn dismiss_chord_overlay(overlay: &gtk::Box, state: &Rc<RefCell<AppState>>) {
    overlay.set_visible(false);
    state.borrow_mut().dismiss_chord();
    if let Some(terminal) = get_active_terminal(state) {
        terminal.grab_focus();
    }
}

fn populate_chord_overlay(
    overlay: &gtk::Box,
    entries: &[ChordOverlayEntry],
    empty_text: &str,
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
) {
    while let Some(child) = overlay.first_child() {
        overlay.remove(&child);
    }

    if entries.is_empty() {
        let empty = gtk::Label::new(Some(empty_text));
        empty.add_css_class("quick-action-empty");
        overlay.append(&empty);
        return;
    }

    for entry in entries {
        let button = gtk::Button::with_label(&format!("[{}]{}", entry.key_label, entry.label));
        button.add_css_class("quick-action-button");
        let target = entry.target;
        let window = window.clone();
        let state = state.clone();
        let overlay_for_action = overlay.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        button.connect_clicked(move |_| match target {
            keybindings::ChordTarget::Bindable(action) => {
                keybindings::activate(&window, action);
            }
            keybindings::ChordTarget::Workspace(action) => {
                execute_workspace_action(
                    &state,
                    &tab_list,
                    &term_stack,
                    &window,
                    &overlay_for_action,
                    action,
                );
            }
        });
        overlay.append(&button);
    }
}

fn overlay_key_label(ch: char) -> String {
    if ch.is_ascii_alphabetic() {
        ch.to_ascii_uppercase().to_string()
    } else {
        ch.to_string()
    }
}

fn quick_action_overlay_entries(
    actions: &[WorkspaceAction],
    chord_map: &BTreeMap<char, WorkspaceAction>,
) -> Vec<ChordOverlayEntry> {
    actions
        .iter()
        .filter_map(|action| {
            let key = chord_map
                .iter()
                .find_map(|(key, target)| (*target == *action).then_some(*key))?;
            Some(ChordOverlayEntry {
                key_label: overlay_key_label(key),
                label: action.compact_label().to_string(),
                target: keybindings::ChordTarget::Workspace(*action),
            })
        })
        .collect()
}

fn leader_overlay_entries(
    leader_map: &BTreeMap<char, keybindings::Action>,
) -> Vec<ChordOverlayEntry> {
    leader_map
        .iter()
        .map(|(key, action)| ChordOverlayEntry {
            key_label: overlay_key_label(*key),
            label: action.label().to_string(),
            target: keybindings::ChordTarget::Bindable(*action),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)] // UI handles stay explicit at this overlay boundary.
fn show_chord_overlay(
    overlay: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    mode: ChordMode,
    entries: &[ChordOverlayEntry],
    empty_text: &str,
    window: &adw::ApplicationWindow,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    timeout: Duration,
) {
    let generation = state.borrow_mut().activate_chord(mode);
    populate_chord_overlay(
        overlay, entries, empty_text, window, state, tab_list, term_stack,
    );
    overlay.set_visible(true);
    overlay.grab_focus();

    let state = state.clone();
    let overlay = overlay.clone();
    glib::timeout_add_local(timeout, move || {
        if state.borrow_mut().expire_chord(generation) {
            overlay.set_visible(false);
            if let Some(terminal) = get_active_terminal(&state) {
                terminal.grab_focus();
            }
        }
        glib::ControlFlow::Break
    });
}

fn activate_keybinding_action(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<AppState>>,
    action: keybindings::Action,
) {
    match action {
        keybindings::Action::Copy => {
            if let Some(terminal) = get_active_terminal(state) {
                terminal::copy_vte_selection_and_record(&terminal);
            }
        }
        _ => {
            keybindings::activate(window, action);
        }
    }
}

/// GTK-free half of the `ChordTarget::Workspace` callback. It retains the
/// typed chord target rather than synthesizing a `win.run-*` action name.
pub(crate) fn workspace_chord_task_launch_request(
    state: &AppState,
    action: WorkspaceAction,
) -> task_launch::TaskLaunchRequest {
    let source_tab_id = state.active_tab().map(|tab| tab.id).unwrap_or_default();
    task_launch::TaskLaunchRequest::for_discovered_tab(
        task_launch::TaskLaunchSurface::BindableAction,
        state,
        source_tab_id,
        action.task_name(),
        Some(action),
    )
}

fn execute_workspace_action(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
    chord_overlay: &gtk::Box,
    action: WorkspaceAction,
) {
    let request = {
        let st = state.borrow();
        workspace_chord_task_launch_request(&st, action)
    };
    if let Err(error) = palette::launch_task(&request, state, tab_list, term_stack, window) {
        show_error_toast(&error.to_string());
    }
    dismiss_chord_overlay(chord_overlay, state);
}

fn refresh_broadcast_indicator(
    indicator: &gtk::Box,
    label: &gtk::Label,
    state: &Rc<RefCell<AppState>>,
) {
    let scope = state.borrow().broadcast_scope();
    let Some(scope) = scope else {
        indicator.set_visible(false);
        label.set_text("");
        return;
    };

    let target_count = terminal::broadcast_target_count(state);
    let pane_label = if target_count == 1 { "pane" } else { "panes" };
    label.set_text(&format!(
        "Broadcast input: {} • {} {}",
        scope.label(),
        target_count,
        pane_label
    ));
    indicator.set_visible(true);
}

// ── Application entry ──

/// Keep GDK from binding the Wayland `wp_presentation` protocol.
///
/// GTK4 (through at least 4.22) tracks every committed frame's presentation
/// feedback in a per-surface `GPtrArray` and removes entries with a linear
/// scan per `presented`/`discarded` event
/// (gdk/wayland/gdkwaylandpresentationtime.c). Compositors that withhold
/// feedback for non-visible surfaces (Hyprland with the output off or the
/// window on another workspace) let that array grow for hours; the deferred
/// event flood on return then drains it in O(n²), blocking the GTK main
/// thread for 30–60s (issue #300). Never binding the protocol disables the
/// tracking entirely; the frame clock falls back to frame-time estimates,
/// which is fine for a terminal. Set `TAAROF_KEEP_WP_PRESENTATION=1` to opt
/// back in (e.g. to profile frame timings).
fn disable_wayland_presentation_feedback() {
    if std::env::var_os("TAAROF_KEEP_WP_PRESENTATION").is_some() {
        return;
    }
    let existing = std::env::var("GDK_WAYLAND_DISABLE").unwrap_or_default();
    if let Some(merged) = gdk_wayland_disable_with_presentation(&existing) {
        std::env::set_var("GDK_WAYLAND_DISABLE", merged);
    }
}

/// Append `wp_presentation` to a `GDK_WAYLAND_DISABLE` value, or `None` when
/// it is already present. Tokens are separated by any of `:;, \t` (the set GDK
/// itself accepts).
fn gdk_wayland_disable_with_presentation(existing: &str) -> Option<String> {
    if existing
        .split([':', ';', ',', ' ', '\t'])
        .any(|token| token == "wp_presentation")
    {
        return None;
    }
    if existing.is_empty() {
        Some("wp_presentation".to_string())
    } else {
        Some(format!("{existing},wp_presentation"))
    }
}

fn validate_agent_launcher_identity(
    agent: &std::path::Path,
    app: &runtime_identity::BuildIdentity,
) -> Result<(), &'static str> {
    let mut identity_command = std::process::Command::new(agent);
    identity_command.arg("--build-info");
    child_env::prepare_child_command(&mut identity_command, &[]);
    let output = match identity_command.output() {
        Ok(output) if output.status.success() => output.stdout,
        _ => return Err("the channel-matched agent launcher identity is unreadable"),
    };
    let launcher: serde_json::Value = serde_json::from_slice(&output)
        .map_err(|_| "the channel-matched agent launcher identity is malformed")?;
    let launcher_revision = launcher
        .get("source_revision")
        .and_then(|value| value.as_str());
    let launcher_dirty = launcher
        .get("source_dirty")
        .and_then(|value| value.as_bool());
    if app.source_revision.as_deref().is_none()
        || app.source_revision.as_deref() != launcher_revision
        || app.source_dirty.is_none()
        || app.source_dirty != launcher_dirty
    {
        return Err("the agent launcher does not match the running app build");
    }
    Ok(())
}

fn validate_resume_helper_identity(
    actual: &runtime_identity::BuildIdentity,
    expected: &runtime_identity::BuildIdentity,
) -> Result<(), &'static str> {
    if actual == expected {
        Ok(())
    } else {
        Err("the resume helper does not match the parent app build")
    }
}

fn app_build_info() -> serde_json::Value {
    serde_json::json!({
        "schema": "taarof.app-build.v1",
        "app_version": env!("CARGO_PKG_VERSION"),
        "build": runtime_identity::build_identity(),
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedAgentResumeHandoff {
    reference: agent_session_core::StableRef,
    expected_app_build: runtime_identity::BuildIdentity,
}

pub(crate) fn saved_agent_resume_handoff(
    reference: agent_session_core::StableRef,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&SavedAgentResumeHandoff {
        reference,
        expected_app_build: runtime_identity::build_identity(),
    })
}

/// Print the build identity embedded in this exact app artifact. The
/// continuity fixture uses this headless path before GTK starts so its receipt
/// can bind both the app and launcher artifacts to their compiled provenance.
pub fn run_build_info_cli(args: &[String]) -> Option<i32> {
    if args.len() != 1 || args[0] != "--build-info" {
        return None;
    }
    println!("{}", app_build_info());
    Some(0)
}

/// Execute a saved Resume action through the channel-matched standalone
/// launcher. This runs in a child copy of the exact parent app artifact, before
/// GTK initialization, so provider discovery cannot block the GTK main thread.
pub fn run_saved_agent_resume_cli(args: &[String]) -> Option<i32> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;

    if args.first().map(String::as_str) != Some("--resume-saved-agent") {
        return None;
    }
    let fail = |message: &str| {
        eprintln!("taarof: Resume agent conversation unavailable: {message}");
        Some(2)
    };
    if args.len() != 2 {
        return fail("exact provider session identity is missing");
    }
    let handoff: SavedAgentResumeHandoff = match serde_json::from_str(&args[1]) {
        Ok(handoff) => handoff,
        Err(_) => return fail("exact provider session identity is malformed"),
    };
    let reference = handoff.reference;
    if reference.provider_id.is_empty()
        || reference.host_identity.is_empty()
        || reference.session_id.is_empty()
    {
        return fail("exact provider session identity is incomplete");
    }

    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("TAAROF_AGENT_BINARY") {
        candidates.push(std::path::PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("TAAROF_INSTALLED_BINARY") {
        if let Some(parent) = std::path::Path::new(&path).parent() {
            candidates.push(parent.join("agent"));
        }
    }
    if let Ok(path) = std::env::current_exe() {
        if let Some(parent) = path.parent() {
            candidates.push(parent.join("agent"));
        }
    }
    candidates.dedup();
    let Some(agent) = candidates.into_iter().find(|path| {
        path.is_absolute()
            && std::fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
    }) else {
        return fail("the channel-matched agent launcher is missing");
    };

    let app = runtime_identity::build_identity();
    if let Err(error) = validate_resume_helper_identity(&app, &handoff.expected_app_build) {
        return fail(error);
    }
    if let Err(error) = validate_agent_launcher_identity(&agent, &app) {
        return fail(error);
    }

    let selector = match serde_json::to_string(&reference) {
        Ok(selector) => selector,
        Err(_) => return fail("exact provider session identity could not be encoded"),
    };
    let mut command = std::process::Command::new(agent);
    command.args(["--resume", &selector]);
    child_env::prepare_child_command(&mut command, &[]);
    let error = command.exec();
    eprintln!("taarof: Resume agent conversation failed: {error}");
    Some(2)
}

pub fn run() {
    // SAFETY: this is the first operation in run(), before GTK or any taarof
    // worker starts; the callee also rejects process-wide re-entry.
    let mut startup_environment = unsafe { child_env::initialize_startup_environment() };
    disable_wayland_presentation_feedback();
    let app_id = instance::application_id(DESKTOP_APP_ID);
    let app = adw::Application::builder().application_id(&app_id).build();

    app.connect_startup(|_| {
        let _ = config::reload_live_config();
        load_css();
    });
    let resume_agents_after_reload =
        Cell::new(startup_environment.take_resume_agents_after_reload());
    app.connect_activate(move |app| {
        activate_app(app, resume_agents_after_reload.replace(false));
    });
    app.run();
}

fn activate_app(app: &adw::Application, resume_agents_after_reload: bool) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }

    if let Some(window) = app.windows().into_iter().next() {
        window.present();
        return;
    }

    build_ui(app, resume_agents_after_reload);
}

fn load_css() {
    CSS_PROVIDERS.with(|slot| {
        let mut providers = slot.borrow_mut();
        let (provider, override_provider) = providers.get_or_insert_with(|| {
            let provider = gtk::CssProvider::new();
            let override_provider = gtk::CssProvider::new();
            let display = gdk::Display::default().expect("Could not get default display");
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            gtk::style_context_add_provider_for_display(
                &display,
                &override_provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
            (provider, override_provider)
        });
        provider.load_from_data(&config::appearance_css(include_str!(
            "../resources/style.css"
        )));
        override_provider.load_from_data(&config::sidebar_override_css().unwrap_or_default());
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarDividerPosition {
    FromStart(i32),
    FromEnd(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SidebarPanedPolicy {
    divider_position: SidebarDividerPosition,
    shrink_start_child: bool,
    shrink_end_child: bool,
    resize_start_child: bool,
    resize_end_child: bool,
}

fn sidebar_paned_policy(
    position: config::SidebarPosition,
    sidebar_width: i32,
) -> SidebarPanedPolicy {
    let sidebar_width = sidebar_width.max(1);
    match position {
        config::SidebarPosition::Left => SidebarPanedPolicy {
            divider_position: SidebarDividerPosition::FromStart(sidebar_width),
            shrink_start_child: true,
            shrink_end_child: false,
            resize_start_child: false,
            resize_end_child: true,
        },
        config::SidebarPosition::Right => SidebarPanedPolicy {
            divider_position: SidebarDividerPosition::FromEnd(sidebar_width),
            shrink_start_child: false,
            shrink_end_child: true,
            resize_start_child: true,
            resize_end_child: false,
        },
    }
}

fn configure_sidebar_layout(
    content: &gtk::Paned,
    sidebar_box: &gtk::Box,
    main_content: &impl IsA<gtk::Widget>,
) {
    let sidebar_width = sidebar_box.width_request();
    let position = config::sidebar_position();
    let policy = sidebar_paned_policy(position, sidebar_width);
    content.set_shrink_start_child(policy.shrink_start_child);
    content.set_shrink_end_child(policy.shrink_end_child);
    content.set_resize_start_child(policy.resize_start_child);
    content.set_resize_end_child(policy.resize_end_child);

    match position {
        config::SidebarPosition::Left => {
            content.set_start_child(Some(sidebar_box));
            content.set_end_child(Some(main_content));
            if let SidebarDividerPosition::FromStart(position) = policy.divider_position {
                content.set_position(position);
            }
        }
        config::SidebarPosition::Right => {
            content.set_start_child(Some(main_content));
            content.set_end_child(Some(sidebar_box));

            let content_for_alloc = content.clone();
            glib::idle_add_local_once(move || {
                let total = content_for_alloc.width();
                if let SidebarDividerPosition::FromEnd(sidebar_width) = policy.divider_position {
                    if total > sidebar_width {
                        content_for_alloc.set_position(total - sidebar_width);
                    }
                }
            });
        }
    }
}

#[derive(Clone)]
struct LiveConfigUiHandles {
    state: Rc<RefCell<AppState>>,
    tab_list: gtk::Box,
    content: gtk::Paned,
    sidebar: gtk::Box,
    main_content: gtk::Box,
    tmux_button: gtk::Button,
    task_dock: gtk::Box,
}

impl LiveConfigUiHandles {
    fn apply(&self) {
        load_css();
        let compact = config::sidebar_compact();
        sidebar::apply_compact_mode(&self.sidebar, compact);
        configure_sidebar_layout(&self.content, &self.sidebar, &self.main_content);
        if !compact {
            sidebar::refresh_all_tab_rows(&self.tab_list, &self.state);
            sidebar::refresh_all_workspace_header_meta(&self.tab_list, &self.state);
        }
        sidebar::refresh_all_workspace_icons(&self.tab_list, &self.state);
        sidebar::refresh_tmux_button(&self.tmux_button);
        self.task_dock.set_visible(config::dock_visible());
    }
}

#[derive(Debug, Default)]
struct LiveConfigWatchState {
    generation: u64,
    in_flight: bool,
    reload_queued: bool,
    cancelled: bool,
    error_reported: bool,
}

impl LiveConfigWatchState {
    fn note_change(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn begin_reload(&mut self) -> Option<u64> {
        if self.cancelled {
            return None;
        }
        if self.in_flight {
            self.reload_queued = true;
            return None;
        }
        self.in_flight = true;
        Some(self.generation)
    }

    fn finish_reload(&mut self, generation: u64) -> (bool, bool) {
        self.in_flight = false;
        let apply = !self.cancelled && generation == self.generation;
        let rerun = !self.cancelled && self.reload_queued;
        self.reload_queued = false;
        (apply, rerun)
    }

    fn cancel(&mut self) {
        self.cancelled = true;
        self.reload_queued = false;
        self.generation = self.generation.wrapping_add(1);
    }

    fn should_report_error(&mut self) -> bool {
        if self.error_reported {
            return false;
        }
        self.error_reported = true;
        true
    }

    fn clear_error(&mut self) {
        self.error_reported = false;
    }
}

#[derive(Clone)]
struct LiveConfigWatcher {
    monitor: gio::FileMonitor,
    debounce_source: Rc<RefCell<Option<glib::SourceId>>>,
    state: Rc<RefCell<LiveConfigWatchState>>,
}

impl LiveConfigWatcher {
    fn cancel(&self) {
        self.monitor.cancel();
        if let Some(source) = self.debounce_source.borrow_mut().take() {
            source.remove();
        }
        self.state.borrow_mut().cancel();
    }
}

fn config_error_message(findings: Vec<config::Finding>) -> String {
    findings
        .into_iter()
        .find(|finding| finding.severity == config::Severity::Error)
        .map(|finding| finding.message)
        .unwrap_or_else(|| "configuration validation failed".to_string())
}

fn start_live_config_reload(
    watch_state: Rc<RefCell<LiveConfigWatchState>>,
    ui: LiveConfigUiHandles,
) {
    let Some(generation) = watch_state.borrow_mut().begin_reload() else {
        return;
    };
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(config::load_live_config_if_valid).await;
        let (apply, rerun) = watch_state.borrow_mut().finish_reload(generation);
        if apply {
            match result {
                Ok(Ok(snapshot)) => {
                    config::install_live_config(snapshot);
                    watch_state.borrow_mut().clear_error();
                    ui.apply();
                    show_toast("Configuration reloaded, including tmux hosts");
                }
                Ok(Err(findings)) => {
                    let message = config_error_message(findings);
                    if watch_state.borrow_mut().should_report_error() {
                        show_error_toast(&format!("Configuration not reloaded: {message}"));
                    }
                }
                Err(_) => {
                    let message = "configuration worker failed";
                    if watch_state.borrow_mut().should_report_error() {
                        show_error_toast(&format!("Configuration not reloaded: {message}"));
                    }
                }
            }
        }
        if rerun {
            start_live_config_reload(watch_state, ui);
        }
    });
}

fn install_live_config_watcher(ui: LiveConfigUiHandles) -> Option<LiveConfigWatcher> {
    let path = config::app_config_path();
    let parent = path.parent()?;
    let directory = gio::File::for_path(parent);
    let monitor = match directory
        .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
    {
        Ok(monitor) => monitor,
        Err(error) => {
            show_error_toast(&format!("Configuration watching is unavailable: {error}"));
            return None;
        }
    };
    let debounce_source: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let watch_state = Rc::new(RefCell::new(LiveConfigWatchState::default()));
    let target_path = path.clone();
    let debounce_for_signal = debounce_source.clone();
    let state_for_signal = watch_state.clone();
    monitor.connect_changed(move |_, file, other_file, _| {
        let targets_config = file.path().as_deref() == Some(target_path.as_path())
            || other_file.and_then(gio::File::path).as_deref() == Some(target_path.as_path());
        if !targets_config || state_for_signal.borrow().cancelled {
            return;
        }
        state_for_signal.borrow_mut().note_change();
        if let Some(source) = debounce_for_signal.borrow_mut().take() {
            source.remove();
        }
        let state = state_for_signal.clone();
        let ui = ui.clone();
        let debounce = debounce_for_signal.clone();
        let source = glib::timeout_add_local_once(Duration::from_millis(200), move || {
            debounce.borrow_mut().take();
            start_live_config_reload(state, ui);
        });
        *debounce_for_signal.borrow_mut() = Some(source);
    });

    Some(LiveConfigWatcher {
        monitor,
        debounce_source,
        state: watch_state,
    })
}

// Per-concern action registration helpers extracted from `build_ui`.
// Each helper is behavior-preserving: the previous inline blocks registered
// the same `gio::SimpleAction` against `window` and wired the same closures.
// Splitting them up makes the structure of `build_ui` scannable and lets
// each concern be tested or extended in isolation.

/// Add a bindable window handler while recording the *actual* GTK action name.
///
/// The ledger is checked after all installers run. Keeping `add_action` behind
/// this helper means deleting, duplicating, or renaming a handler cannot leave
/// a successfully-started window with a silently dead keybinding.
fn add_bindable_window_action(
    window: &adw::ApplicationWindow,
    ledger: &mut keybindings::WindowActionLedger,
    action: keybindings::Action,
    handler: &gio::SimpleAction,
) {
    let name = handler.name();
    ledger.register(action, name.as_str());
    window.add_action(handler);
}

/// Check the actual GTK window action map as well as the typed registration
/// ledger. This is the reverse half of the contract: a newly added window
/// handler cannot bypass the keybinding vocabulary.
fn validate_window_action_contract(
    window: &adw::ApplicationWindow,
    ledger: &keybindings::WindowActionLedger,
) {
    ledger
        .validate()
        .expect("bindable window action contract must be complete before shortcuts are installed");

    let actual = window.list_actions();
    let unexpected: Vec<_> = actual
        .iter()
        .map(|name| name.as_str())
        .filter(|name| !ledger.handler_names().any(|registered| registered == *name))
        .collect();
    assert!(
        unexpected.is_empty(),
        "window actions without keybindings::Action variants: {unexpected:?}"
    );

    let missing: Vec<_> = ledger
        .handler_names()
        .filter(|name| !actual.iter().any(|registered| registered.as_str() == *name))
        .collect();
    assert!(
        missing.is_empty(),
        "bindable window actions recorded but not installed: {missing:?}"
    );
}

fn toggled_dock_visibility(current: bool) -> bool {
    !current
}

#[allow(clippy::too_many_arguments)] // UI handles stay explicit at this installer boundary.
fn install_dock_action(
    window: &adw::ApplicationWindow,
    action_ledger: &mut keybindings::WindowActionLedger,
    state: &Rc<RefCell<AppState>>,
    task_dock: &gtk::Box,
    sidebar_box: &gtk::Box,
    tab_list: &gtk::Box,
    content: &gtk::Paned,
    main_content: &gtk::Box,
) {
    let action = gio::SimpleAction::new("toggle-dock", None);
    let task_dock = task_dock.clone();
    let state_for_dock = state.clone();
    action.connect_activate(move |_, _| {
        task_dock.set_visible(toggled_dock_visibility(task_dock.is_visible()));
        if let Some(terminal) = get_active_terminal(&state_for_dock) {
            terminal.grab_focus();
        }
    });
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::ToggleDock,
        &action,
    );

    let action = gio::SimpleAction::new("toggle-sidebar-compact", None);
    let sidebar_box = sidebar_box.clone();
    let tab_list = tab_list.clone();
    let content = content.clone();
    let main_content = main_content.clone();
    let state = state.clone();
    action.connect_activate(move |_, _| {
        let compact = !sidebar_box.has_css_class("compact");
        sidebar::apply_compact_mode(&sidebar_box, compact);
        configure_sidebar_layout(&content, &sidebar_box, &main_content);
        if !compact {
            sidebar::refresh_all_tab_rows(&tab_list, &state);
            sidebar::refresh_all_workspace_header_meta(&tab_list, &state);
        }
        if let Some(terminal) = get_active_terminal(&state) {
            terminal.grab_focus();
        }
    });
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::ToggleSidebarCompact,
        &action,
    );
}

fn install_new_tab_actions(
    window: &adw::ApplicationWindow,
    action_ledger: &mut keybindings::WindowActionLedger,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
) {
    // Ctrl+T = new tab
    let action_new_tab = gio::SimpleAction::new("new-tab", None);
    {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        let window_for_newtab = window.clone();
        action_new_tab.connect_activate(move |_, _| {
            let ws_id = {
                let st = state.borrow();
                st.active_ws().map(|ws| ws.id)
            };
            if let Some(ws_id) = ws_id {
                sidebar::open_new_tab_in_workspace(
                    &tab_list,
                    &state,
                    &term_stack,
                    ws_id,
                    &window_for_newtab,
                );
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::NewTab,
        &action_new_tab,
    );

    // Ctrl+Shift+T = new tmux tab
    let action_new_tmux_tab = gio::SimpleAction::new("new-tmux-tab", None);
    {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        let window_for_tmux_tab = window.clone();
        action_new_tmux_tab.connect_activate(move |_, _| {
            if let Some(message) = terminal::explicit_tmux_tab_ui_state()
                .unavailable_message
                .as_deref()
            {
                show_error_toast(message);
                return;
            }
            sidebar::show_create_tmux_tab_dialog(
                &state,
                &term_stack,
                &tab_list,
                &window_for_tmux_tab,
            );
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::NewTmuxTab,
        &action_new_tmux_tab,
    );

    // Ctrl+PageUp = previous tab
    let action_previous_tab = gio::SimpleAction::new("previous-tab", None);
    {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        action_previous_tab.connect_activate(move |_, _| {
            if sidebar::activate_previous_tab(&tab_list, &state, &term_stack) {
                if let Some(terminal) = get_active_terminal(&state) {
                    terminal.grab_focus();
                }
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::PreviousTab,
        &action_previous_tab,
    );

    let action_register_project = gio::SimpleAction::new("register-project", None);
    {
        let state = state.clone();
        action_register_project.connect_activate(move |_, _| {
            register_focused_project(&state);
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::RegisterProject,
        &action_register_project,
    );
}

struct ProjectRegistrationGuard;

impl Drop for ProjectRegistrationGuard {
    fn drop(&mut self) {
        PROJECT_REGISTRATION_INFLIGHT.with(|inflight| inflight.set(false));
    }
}

fn finish_project_registration(result: Result<String, String>) {
    match result {
        Ok(name) => show_toast(&format!("Registered project {name}")),
        Err(error) => show_error_toast(&format!("Could not register project: {error}")),
    }
}

fn register_focused_project(state: &Rc<RefCell<AppState>>) {
    if PROJECT_REGISTRATION_INFLIGHT.with(|inflight| inflight.replace(true)) {
        show_toast("Project registration is already in progress");
        return;
    }
    let registration_guard = ProjectRegistrationGuard;
    let source = match projects::focused_project_source(&state.borrow()) {
        Ok(source) => source,
        Err(error) => {
            finish_project_registration(Err(error));
            return;
        }
    };
    match source {
        projects::FocusedProjectSource::Local {
            cwd,
            preferred_open,
        } => {
            glib::spawn_future_local(async move {
                let _registration_guard = registration_guard;
                let result = gio::spawn_blocking(move || {
                    projects::project_from_local_checkout(&cwd, preferred_open).and_then(
                        |project| {
                            let name = project.display_name.clone();
                            projects::save(project).map_err(|error| error.to_string())?;
                            Ok(name)
                        },
                    )
                })
                .await
                .unwrap_or_else(|_| Err("local Git discovery failed".to_string()));
                finish_project_registration(result);
            });
        }
        projects::FocusedProjectSource::Remote {
            cwd,
            host_name,
            ssh_target,
            ssh_argv,
            preferred_open,
        } => {
            let argv = tracking::remote_git_identity_command(ssh_argv.as_deref(), &host_name, &cwd);
            glib::spawn_future_local(async move {
                let _registration_guard = registration_guard;
                let outcome = gio::spawn_blocking(move || {
                    tracking::classify_remote_git_identity(
                        terminal::run_tmux_command_sync_result_without_diagnostics(&argv),
                    )
                })
                .await
                .unwrap_or(tracking::RemoteGitIdentityOutcome::Error(
                    tracking::RemoteGitIdentityError::Transport,
                ));
                let tracking::RemoteGitIdentityOutcome::GitHub(identity) = outcome else {
                    finish_project_registration(Err(
                        "remote Git identity is unavailable".to_string()
                    ));
                    return;
                };
                let result = gio::spawn_blocking(move || {
                    projects::project_from_remote_identity(
                        &host_name,
                        &ssh_target,
                        ssh_argv.as_deref(),
                        identity,
                        preferred_open,
                    )
                    .and_then(|project| {
                        let name = project.display_name.clone();
                        projects::save(project).map_err(|error| error.to_string())?;
                        Ok(name)
                    })
                })
                .await
                .unwrap_or_else(|_| Err("remote project registration failed".to_string()));
                finish_project_registration(result);
            });
        }
    }
}

// Extracted for readability; reducing args would just rebundle them into a struct for no real benefit.
#[allow(clippy::too_many_arguments)]
fn install_search_palette_actions(
    window: &adw::ApplicationWindow,
    action_ledger: &mut keybindings::WindowActionLedger,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    search_bar: &gtk::Box,
    search_entry: &gtk::SearchEntry,
    cmd_palette: &palette::CommandPalette,
    workspace_inspector: &inspector::WorkspaceInspector,
    history_view: &history_view::ui::HistoryView,
) {
    // F002: Ctrl+Shift+F = toggle search overlay
    let action_search = gio::SimpleAction::new("search-toggle", None);
    {
        let search_bar = search_bar.clone();
        let search_entry = search_entry.clone();
        let state_for_search = state.clone();
        action_search.connect_activate(move |_, _| {
            if search_bar.is_visible() {
                // If already visible, search next
                if let Some(terminal) = get_active_terminal(&state_for_search) {
                    terminal.search_find_next();
                }
            } else {
                search_bar.set_visible(true);
                search_entry.grab_focus();
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::SearchToggle,
        &action_search,
    );

    let action_copy_recent_output = gio::SimpleAction::new("copy-recent-output", None);
    {
        let state = state.clone();
        action_copy_recent_output.connect_activate(move |_, _| {
            if let Err(err) = terminal::copy_recent_output_from_active_terminal(&state) {
                show_error_toast(&err);
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::CopyRecentOutput,
        &action_copy_recent_output,
    );

    // Copy the focused pane's last agent message as pristine markdown; fall back
    // to recent-output capture (with an explanatory toast) when the pane has no
    // resolvable transcript (EXAMPLE-91).
    let action_copy_last_message = gio::SimpleAction::new("copy-last-message", None);
    {
        let state = state.clone();
        action_copy_last_message.connect_activate(move |_, _| {
            match terminal::copy_last_message_from_active_pane(&state) {
                Ok(()) => show_toast("Copied last agent message"),
                Err(err) => show_error_toast(&err),
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::CopyLastMessage,
        &action_copy_last_message,
    );

    // Jump-to-prompt scroll actions. Terminal-scoped bindings forward here via
    // window.activate_action, so both must exist as window actions.
    let action_jump_previous_prompt = gio::SimpleAction::new("jump-previous-prompt", None);
    {
        let state = state.clone();
        action_jump_previous_prompt.connect_activate(move |_, _| {
            terminal::jump_active_pane_to_prompt(&state, terminal::PromptJump::Previous);
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::JumpPreviousPrompt,
        &action_jump_previous_prompt,
    );

    let action_jump_next_prompt = gio::SimpleAction::new("jump-next-prompt", None);
    {
        let state = state.clone();
        action_jump_next_prompt.connect_activate(move |_, _| {
            terminal::jump_active_pane_to_prompt(&state, terminal::PromptJump::Next);
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::JumpNextPrompt,
        &action_jump_next_prompt,
    );

    // Search entry: update regex on text change
    {
        let state_for_search_changed = state.clone();
        search_entry.connect_search_changed(move |entry| {
            let text = entry.text();
            if let Some(terminal) = get_active_terminal(&state_for_search_changed) {
                if text.is_empty() {
                    terminal.search_set_regex(None::<&vte::Regex>, 0);
                } else {
                    // Escape the search text for use as a PCRE2 literal
                    let escaped = regex_escape(&text);
                    // Case-insensitive literal search with multiline support.
                    if let Ok(regex) =
                        vte::Regex::for_search(&escaped, terminal_search_regex_flags())
                    {
                        terminal.search_set_regex(Some(&regex), 0);
                        terminal.search_set_wrap_around(true);
                        terminal.search_find_previous();
                    }
                }
            }
        });
    }

    // Search entry: Enter = find next, Shift+Enter = find previous, Escape = close
    {
        let search_bar_for_key = search_bar.clone();
        let state_for_key = state.clone();
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.connect_key_pressed(move |_ctrl, key, _code, mods| {
            match key {
                gdk::Key::Escape => {
                    search_bar_for_key.set_visible(false);
                    // Clear search and refocus terminal
                    if let Some(terminal) = get_active_terminal(&state_for_key) {
                        terminal.search_set_regex(None::<&vte::Regex>, 0);
                        terminal.grab_focus();
                    }
                    return glib::Propagation::Stop;
                }
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    if let Some(terminal) = get_active_terminal(&state_for_key) {
                        if mods.contains(gdk::ModifierType::SHIFT_MASK) {
                            terminal.search_find_previous();
                        } else {
                            terminal.search_find_next();
                        }
                    }
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
            glib::Propagation::Proceed
        });
        search_entry.add_controller(key_ctrl);
    }

    // F004: Ctrl+Alt+J = jump to first attention tab
    let action_jump_attention = gio::SimpleAction::new("jump-attention", None);
    {
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        action_jump_attention.connect_activate(move |_, _| {
            let attention_target = {
                let st = state.borrow();
                views::first_attention_target(&st)
            };
            if let Some((tab_id, pane_id)) = attention_target {
                if !views::focus_attention_target(&state, &tab_list, &term_stack, tab_id, pane_id) {
                    show_error_toast("Attention target is no longer available");
                }
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::JumpAttention,
        &action_jump_attention,
    );

    // Notification activation target: focus a specific (tab, pane). Registered
    // on the application (not the window) because desktop notifications
    // activate `app.`-scoped actions. Parameter is a `"<tab>:<pane>"` string.
    let action_focus_pane = gio::SimpleAction::new("focus-pane", Some(glib::VariantTy::STRING));
    {
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        action_focus_pane.connect_activate(move |_, param| {
            let Some(target) = param.and_then(|p| p.get::<String>()) else {
                return;
            };
            let Some((tab_str, pane_str)) = target.split_once(':') else {
                return;
            };
            let (Ok(tab_id), Ok(pane_id)) = (tab_str.parse::<u32>(), pane_str.parse::<u32>())
            else {
                return;
            };
            window.present();
            if sidebar::activate_tab(&tab_list, &state, &term_stack, tab_id) {
                // Focus the exact pane inside the now-active tab, and clear its
                // consumed attention state so it can notify again later.
                let terminal = {
                    let mut st = state.borrow_mut();
                    st.find_tab_mut(tab_id).and_then(|tab| {
                        if tab.panes.leaf(pane_id).is_some() {
                            tab.focused_pane_id = pane_id;
                        }
                        tab.needs_attention = false;
                        tab.clear_pane_notification(pane_id);
                        tab.panes.leaf(pane_id).map(|leaf| leaf.terminal.clone())
                    })
                };
                if let Some(terminal) = terminal {
                    terminal.grab_focus();
                }
                sidebar::refresh_tab_row(&tab_list, &state, tab_id);
            }
        });
    }
    if let Some(app) = window.application() {
        app.add_action(&action_focus_pane);
    }

    // F007: Ctrl+Shift+P = command palette
    let action_palette = gio::SimpleAction::new("command-palette", None);
    {
        let cmd_palette = cmd_palette.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window_for_palette = window.clone();
        action_palette.connect_activate(move |_, _| {
            if cmd_palette.container.is_visible() {
                cmd_palette.container.set_visible(false);
            } else {
                palette::show_palette(
                    &cmd_palette,
                    &state,
                    &tab_list,
                    &term_stack,
                    &window_for_palette,
                );
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::CommandPalette,
        &action_palette,
    );

    let action_help = gio::SimpleAction::new("shortcut-help", None);
    {
        let cmd_palette = cmd_palette.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        action_help.connect_activate(move |_, _| {
            palette::show_keybinding_help(&cmd_palette, &state, &tab_list, &term_stack, &window);
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::ShortcutHelp,
        &action_help,
    );

    // Send to pane: open the target picker seeded from the current
    // selection/clipboard (or the active pane's recent output).
    let action_send_to_pane = gio::SimpleAction::new("send-to-pane", None);
    {
        let cmd_palette = cmd_palette.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window_for_send = window.clone();
        action_send_to_pane.connect_activate(move |_, _| {
            palette::open_send_to_pane_picker(
                &cmd_palette,
                &state,
                &tab_list,
                &term_stack,
                &window_for_send,
                false,
            );
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::SendToPane,
        &action_send_to_pane,
    );

    let action_send_last_output_to_pane = gio::SimpleAction::new("send-last-output-to-pane", None);
    {
        let cmd_palette = cmd_palette.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window_for_send = window.clone();
        action_send_last_output_to_pane.connect_activate(move |_, _| {
            palette::open_send_to_pane_picker(
                &cmd_palette,
                &state,
                &tab_list,
                &term_stack,
                &window_for_send,
                true,
            );
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::SendLastOutputToPane,
        &action_send_last_output_to_pane,
    );

    // Relay last message: open the send-to-pane picker preloaded with the active
    // pane's last agent message (falling back to recent output) for two-keystroke
    // agent-to-agent handoff (EXAMPLE-91).
    let action_relay_last_message = gio::SimpleAction::new("relay-last-message", None);
    {
        let cmd_palette = cmd_palette.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window_for_relay = window.clone();
        action_relay_last_message.connect_activate(move |_, _| {
            palette::open_relay_last_message_picker(
                &cmd_palette,
                &state,
                &tab_list,
                &term_stack,
                &window_for_relay,
            );
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::RelayLastMessage,
        &action_relay_last_message,
    );

    // Clipboard history: open the keyboard-first picker over the in-memory
    // clipboard "kill-ring" of taarof-initiated copies (EXAMPLE-88).
    let action_clipboard_history = gio::SimpleAction::new("clipboard-history", None);
    {
        let cmd_palette = cmd_palette.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window_for_clipboard = window.clone();
        action_clipboard_history.connect_activate(move |_, _| {
            palette::open_clipboard_history_picker(
                &cmd_palette,
                &state,
                &tab_list,
                &term_stack,
                &window_for_clipboard,
            );
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::ClipboardHistory,
        &action_clipboard_history,
    );

    // Recent files: open the keyboard-first picker over the files the focused
    // tab's agents created/edited, resolved against each pane's cwd (EXAMPLE-92).
    let action_recent_files = gio::SimpleAction::new("recent-files", None);
    {
        let cmd_palette = cmd_palette.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window_for_recent_files = window.clone();
        action_recent_files.connect_activate(move |_, _| {
            palette::open_recent_files_picker(
                &cmd_palette,
                &state,
                &tab_list,
                &term_stack,
                &window_for_recent_files,
            );
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::RecentFiles,
        &action_recent_files,
    );

    // Peek file: open the file referenced by the current selection in the
    // in-app overlay (EXAMPLE-93).
    let action_peek_file = gio::SimpleAction::new("peek-file", None);
    {
        let state = state.clone();
        action_peek_file.connect_activate(move |_, _| {
            crate::terminal::peek_active_selection(&state);
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::PeekFile,
        &action_peek_file,
    );

    let action_workspace_inspector = gio::SimpleAction::new("workspace-inspector", None);
    {
        let workspace_inspector = workspace_inspector.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let window_for_inspector = window.clone();
        action_workspace_inspector.connect_activate(move |_, _| {
            if workspace_inspector.container.is_visible() {
                workspace_inspector.hide(&state);
            } else {
                workspace_inspector.show(&state, &tab_list, &window_for_inspector);
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::WorkspaceInspector,
        &action_workspace_inspector,
    );

    let action_history_view = gio::SimpleAction::new("history-view", None);
    {
        let history_view = history_view.clone();
        action_history_view.connect_activate(move |_, _| {
            if history_view.container.is_visible() {
                history_view.hide();
            } else {
                history_view.show();
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::HistoryView,
        &action_history_view,
    );

    let action_paste_active_input = gio::SimpleAction::new("paste-active-input", None);
    {
        let state = state.clone();
        action_paste_active_input.connect_activate(move |_, _| {
            terminal::paste_clipboard_to_active_scope(&state);
            if let Some(terminal) = get_active_terminal(&state) {
                terminal.grab_focus();
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::Paste,
        &action_paste_active_input,
    );

    let action_toggle_selection_mode = gio::SimpleAction::new("toggle-selection-mode", None);
    {
        let state = state.clone();
        action_toggle_selection_mode.connect_activate(move |_, _| {
            let _ = terminal::toggle_active_terminal_input_mode(&state);
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::ToggleSelectionMode,
        &action_toggle_selection_mode,
    );
}

fn install_broadcast_actions(
    window: &adw::ApplicationWindow,
    action_ledger: &mut keybindings::WindowActionLedger,
    state: &Rc<RefCell<AppState>>,
    broadcast_indicator: &gtk::Box,
    broadcast_label: &gtk::Label,
) {
    // Ctrl+Alt+B = toggle broadcast input shortcut scope
    let action_toggle_broadcast = gio::SimpleAction::new("toggle-broadcast-input", None);
    {
        let state = state.clone();
        let indicator = broadcast_indicator.clone();
        let label = broadcast_label.clone();
        action_toggle_broadcast.connect_activate(move |_, _| {
            state.borrow_mut().toggle_broadcast_shortcut_scope();
            refresh_broadcast_indicator(&indicator, &label, &state);
            if let Some(terminal) = get_active_terminal(&state) {
                terminal.grab_focus();
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::ToggleBroadcastInput,
        &action_toggle_broadcast,
    );

    let action_broadcast_tab = gio::SimpleAction::new("broadcast-tab", None);
    {
        let state = state.clone();
        let indicator = broadcast_indicator.clone();
        let label = broadcast_label.clone();
        action_broadcast_tab.connect_activate(move |_, _| {
            state
                .borrow_mut()
                .set_broadcast_scope(Some(BroadcastScope::Tab));
            refresh_broadcast_indicator(&indicator, &label, &state);
            if let Some(terminal) = get_active_terminal(&state) {
                terminal.grab_focus();
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::BroadcastTab,
        &action_broadcast_tab,
    );

    let action_broadcast_workspace = gio::SimpleAction::new("broadcast-workspace", None);
    {
        let state = state.clone();
        let indicator = broadcast_indicator.clone();
        let label = broadcast_label.clone();
        action_broadcast_workspace.connect_activate(move |_, _| {
            state
                .borrow_mut()
                .set_broadcast_scope(Some(BroadcastScope::Workspace));
            refresh_broadcast_indicator(&indicator, &label, &state);
            if let Some(terminal) = get_active_terminal(&state) {
                terminal.grab_focus();
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::BroadcastWorkspace,
        &action_broadcast_workspace,
    );

    let action_broadcast_off = gio::SimpleAction::new("broadcast-off", None);
    {
        let state = state.clone();
        let indicator = broadcast_indicator.clone();
        let label = broadcast_label.clone();
        action_broadcast_off.connect_activate(move |_, _| {
            state.borrow_mut().set_broadcast_scope(None);
            refresh_broadcast_indicator(&indicator, &label, &state);
            if let Some(terminal) = get_active_terminal(&state) {
                terminal.grab_focus();
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::BroadcastOff,
        &action_broadcast_off,
    );
}

fn install_discovery_and_task_actions(
    window: &adw::ApplicationWindow,
    action_ledger: &mut keybindings::WindowActionLedger,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
) {
    // Ctrl+Shift+D = per-tab discovery
    let action_discover = gio::SimpleAction::new("discover-tab", None);
    {
        let state = state.clone();
        let tab_list = tab_list.clone();
        action_discover.connect_activate(move |_, _| {
            let tab_id = {
                let st = state.borrow();
                st.active_tab().map(|t| t.id)
            };
            if let Some(tab_id) = tab_id {
                task_panel::discover_tasks_explicitly(&state, &tab_list, tab_id);
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::DiscoverTab,
        &action_discover,
    );
}

#[allow(clippy::too_many_arguments)] // UI handles stay explicit at this installer boundary.
fn install_chord_overlay_actions(
    window: &adw::ApplicationWindow,
    action_ledger: &mut keybindings::WindowActionLedger,
    state: &Rc<RefCell<AppState>>,
    chord_overlay: &gtk::Box,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    keybindings: &keybindings::KeybindingConfig,
    chord_timeout: Duration,
) {
    // F026: generic leader-mode activation
    let action_leader_mode = gio::SimpleAction::new("leader-mode", None);
    {
        let state = state.clone();
        let chord_overlay = chord_overlay.clone();
        let window_for_leader = window.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let leader_map = keybindings.leader_map.clone();
        action_leader_mode.connect_activate(move |_, _| {
            if leader_map.is_empty() {
                show_error_toast("Leader mode is not configured");
                return;
            }
            let entries = leader_overlay_entries(&leader_map);
            show_chord_overlay(
                &chord_overlay,
                &state,
                ChordMode::Leader,
                &entries,
                "No leader bindings configured",
                &window_for_leader,
                &tab_list,
                &term_stack,
                chord_timeout,
            );
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::LeaderMode,
        &action_leader_mode,
    );

    // F012: Ctrl+F5 chord overlay for quick workspace actions
    let action_quick_action = gio::SimpleAction::new("quick-action", None);
    {
        let state = state.clone();
        let chord_overlay = chord_overlay.clone();
        let window_for_quick = window.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let quick_action_map = keybindings.quick_action_map.clone();
        action_quick_action.connect_activate(move |_, _| {
            let actions = {
                let st = state.borrow();
                st.active_tab()
                    .map(|tab| tab.discovered_actions.clone())
                    .unwrap_or_default()
            };
            let entries = quick_action_overlay_entries(&actions, &quick_action_map);
            show_chord_overlay(
                &chord_overlay,
                &state,
                ChordMode::QuickAction,
                &entries,
                "No actions available",
                &window_for_quick,
                &tab_list,
                &term_stack,
                chord_timeout,
            );
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::QuickAction,
        &action_quick_action,
    );

    // Chord follow-up key controller (Capture phase on the window so VTE
    // terminals cannot swallow the follow-up keypress).
    {
        let state = state.clone();
        let chord_overlay = chord_overlay.clone();
        let window_for_ctrl = window.clone();
        let window_for_keys = window_for_ctrl.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let quick_action_map = keybindings.quick_action_map.clone();
        let leader_map = keybindings.leader_map.clone();
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
        let overlay_for_keys = chord_overlay.clone();
        key_ctrl.connect_key_pressed(move |_ctrl, key, _code, _mods| {
            let chord_mode = state.borrow().chord_mode();
            if chord_mode.is_none() || !overlay_for_keys.is_visible() {
                return glib::Propagation::Proceed;
            }

            if key == gdk::Key::Escape {
                dismiss_chord_overlay(&overlay_for_keys, &state);
                return glib::Propagation::Stop;
            }

            match chord_mode {
                Some(ChordMode::QuickAction) => {
                    if let Some(action) = workspace_action_from_chord_key(key, &quick_action_map) {
                        execute_workspace_action(
                            &state,
                            &tab_list,
                            &term_stack,
                            &window_for_keys,
                            &overlay_for_keys,
                            action,
                        );
                    }
                }
                Some(ChordMode::Leader) => {
                    if let Some(action) = leader_action_from_chord_key(key, &leader_map) {
                        dismiss_chord_overlay(&overlay_for_keys, &state);
                        activate_keybinding_action(&window_for_keys, &state, action);
                    } else {
                        dismiss_chord_overlay(&overlay_for_keys, &state);
                    }
                }
                None => {}
            }
            glib::Propagation::Stop
        });
        // Attach to window, not the overlay Box — VTE terminals steal focus
        // from layout containers, so the overlay's EventControllerKey would
        // never see follow-up keypresses. The chord-mode + visibility guard
        // ensures we only intercept when a chord is active.
        window_for_ctrl.add_controller(key_ctrl);
    }
}

fn install_split_actions(
    window: &adw::ApplicationWindow,
    action_ledger: &mut keybindings::WindowActionLedger,
    state: &Rc<RefCell<AppState>>,
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
) {
    // F009: Ctrl+Shift+\ = split vertical
    let action_split_v = gio::SimpleAction::new("split-vertical", None);
    {
        let state = state.clone();
        let runtime = runtime.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        let window_for_split = window.clone();
        action_split_v.connect_activate(move |_, _| {
            let target_tab_id = {
                let st = state.borrow();
                st.active_tab().map(|tab| tab.id)
            };
            let do_split = {
                let runtime = runtime.clone();
                let term_stack = term_stack.clone();
                let tab_list = tab_list.clone();
                let window_for_split = window_for_split.clone();
                move || {
                    terminal::split_pane(
                        &runtime,
                        &term_stack,
                        pane::SplitDirection::Vertical,
                        &tab_list,
                        &window_for_split,
                    );
                }
            };
            if let Some(tab_id) = target_tab_id {
                terminal::validate_split_tmux_then(&state, &window_for_split, tab_id, do_split);
            } else {
                do_split();
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::SplitVertical,
        &action_split_v,
    );

    // F009: Ctrl+Shift+- = split horizontal
    let action_split_h = gio::SimpleAction::new("split-horizontal", None);
    {
        let state = state.clone();
        let runtime = runtime.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        let window_for_split = window.clone();
        action_split_h.connect_activate(move |_, _| {
            let target_tab_id = {
                let st = state.borrow();
                st.active_tab().map(|tab| tab.id)
            };
            let do_split = {
                let runtime = runtime.clone();
                let term_stack = term_stack.clone();
                let tab_list = tab_list.clone();
                let window_for_split = window_for_split.clone();
                move || {
                    terminal::split_pane(
                        &runtime,
                        &term_stack,
                        pane::SplitDirection::Horizontal,
                        &tab_list,
                        &window_for_split,
                    );
                }
            };
            if let Some(tab_id) = target_tab_id {
                terminal::validate_split_tmux_then(&state, &window_for_split, tab_id, do_split);
            } else {
                do_split();
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::SplitHorizontal,
        &action_split_h,
    );

    // F009: Ctrl+Shift+W = close focused pane
    let action_close_pane = gio::SimpleAction::new("close-pane", None);
    {
        let state = state.clone();
        let runtime = runtime.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        action_close_pane.connect_activate(move |_, _| {
            let (tab_id, pane_id) = {
                let st = state.borrow();
                let tab = match st.active_tab() {
                    Some(t) => t,
                    None => return,
                };
                (tab.id, tab.focused_pane_id)
            };
            let _ = terminal::close_pane(&runtime, &term_stack, Some(&tab_list), tab_id, pane_id);
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::ClosePane,
        &action_close_pane,
    );

    // F022: Ctrl+Shift+Z = toggle focused pane zoom
    let action_toggle_pane_zoom = gio::SimpleAction::new("toggle-pane-zoom", None);
    {
        let state = state.clone();
        action_toggle_pane_zoom.connect_activate(move |_, _| {
            terminal::toggle_pane_zoom(&state);
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::TogglePaneZoom,
        &action_toggle_pane_zoom,
    );

    // F009: Ctrl+Shift+H/J/K/L = focus pane (vim-style navigation)
    for (action, direction) in [
        (
            keybindings::Action::FocusPaneLeft,
            pane::PaneNavigationDirection::Left,
        ),
        (
            keybindings::Action::FocusPaneRight,
            pane::PaneNavigationDirection::Right,
        ),
        (
            keybindings::Action::FocusPaneUp,
            pane::PaneNavigationDirection::Up,
        ),
        (
            keybindings::Action::FocusPaneDown,
            pane::PaneNavigationDirection::Down,
        ),
    ] {
        let action_name = action
            .gaction_name()
            .strip_prefix("win.")
            .expect("focus actions are window actions");
        let handler = gio::SimpleAction::new(action_name, None);
        {
            let state = state.clone();
            let term_stack = term_stack.clone();
            let window_for_focus = window.clone();
            handler.connect_activate(move |_, _| {
                terminal::focus_pane_in_direction(
                    &state,
                    &term_stack,
                    direction,
                    &window_for_focus,
                );
            });
        }
        add_bindable_window_action(window, action_ledger, action, &handler);
    }
}

fn install_workspace_actions(
    window: &adw::ApplicationWindow,
    action_ledger: &mut keybindings::WindowActionLedger,
    state: &Rc<RefCell<AppState>>,
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
) {
    // Ctrl+Alt+N = new workspace
    let action_new_ws = gio::SimpleAction::new("new-workspace", None);
    {
        let state = state.clone();
        let runtime = runtime.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        let window_for_ws = window.clone();
        action_new_ws.connect_activate(move |_, _| {
            // Capture only immutable in-memory context. Repository discovery
            // follows on a worker so this GTK action creates a usable shell
            // immediately even when the focused path sits on slow storage.
            let cwd = {
                let st = state.borrow();
                crate::mise::discovery_dir(&st)
            };
            let ws_name = {
                let st = state.borrow();
                format!("Workspace {}", st.workspaces.len() + 1)
            };

            let ws_id = { runtime.create_workspace(&ws_name, None) };
            let ws_origin = state
                .borrow()
                .workspaces
                .iter()
                .find(|workspace| workspace.id == ws_id)
                .map(|workspace| workspace.work_origin.clone());
            sidebar::add_workspace_header(&tab_list, &state, &term_stack, ws_id, &ws_name);

            let tab_id =
                terminal::create_terminal(&runtime, &term_stack, "Shell", Some(&cwd), None);
            sidebar::add_tab_row(&tab_list, &state, &term_stack, tab_id, "Shell", true);
            terminal::wire_tab_terminals(&state, &term_stack, &tab_list, &window_for_ws, tab_id);

            let state_for_apply = state.clone();
            let tab_list_for_apply = tab_list.clone();
            let request_key = format!("discover:new-workspace:{ws_id}");
            let submission = git::spawn_async(
                request_key,
                move || git::discover(&cwd),
                move |git_info| {
                    let discovered_name = workspace_name_from_git_info(&git_info);
                    let header_name = {
                        let mut st = state_for_apply.borrow_mut();
                        let Some(workspace) = st.workspaces.iter_mut().find(|workspace| {
                            workspace.id == ws_id
                                && ws_origin.as_deref() == Some(workspace.work_origin.as_str())
                        }) else {
                            return;
                        };
                        apply_git_info_to_workspace(workspace, &git_info);
                        if let Some(name) = discovered_name.as_ref() {
                            workspace.name = name.clone();
                        }
                        workspace.name.clone()
                    };
                    sidebar::set_workspace_header_label(
                        &tab_list_for_apply,
                        &state_for_apply,
                        ws_id,
                        &workspace_header_label(&header_name, &git_info),
                    );
                },
            );
            if matches!(submission, git::GitAsyncSubmission::Saturated) {
                crate::show_toast(
                    "Git metadata refresh is busy; workspace details will refresh later",
                );
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::NewWorkspace,
        &action_new_ws,
    );

    let action_previous_ws = gio::SimpleAction::new("previous-workspace", None);
    {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        action_previous_ws.connect_activate(move |_, _| {
            if sidebar::activate_previous_workspace(&tab_list, &state, &term_stack) {
                if let Some(terminal) = get_active_terminal(&state) {
                    terminal.grab_focus();
                }
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::PreviousWorkspace,
        &action_previous_ws,
    );

    let action_next_ws = gio::SimpleAction::new("next-workspace", None);
    {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        action_next_ws.connect_activate(move |_, _| {
            if sidebar::activate_workspace_relative(&tab_list, &state, &term_stack, 1) {
                if let Some(terminal) = get_active_terminal(&state) {
                    terminal.grab_focus();
                }
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::NextWorkspace,
        &action_next_ws,
    );

    let action_prev_ws = gio::SimpleAction::new("prev-workspace", None);
    {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        action_prev_ws.connect_activate(move |_, _| {
            if sidebar::activate_workspace_relative(&tab_list, &state, &term_stack, -1) {
                if let Some(terminal) = get_active_terminal(&state) {
                    terminal.grab_focus();
                }
            }
        });
    }
    add_bindable_window_action(
        window,
        action_ledger,
        keybindings::Action::PrevWorkspace,
        &action_prev_ws,
    );
}

fn install_global_shortcuts(
    window: &adw::ApplicationWindow,
    keybindings: &keybindings::KeybindingConfig,
) {
    let ctrl = gtk::ShortcutController::new();
    ctrl.set_scope(gtk::ShortcutScope::Global);
    for (action, trigger) in keybindings.window_shortcuts() {
        if action == keybindings::Action::ShortcutHelp {
            continue;
        }
        ctrl.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string(trigger),
            Some(gtk::NamedAction::new(action.gaction_name())),
        ));
    }
    // Terminal-scope shortcuts (copy, paste, split, close-pane) are handled in
    // terminal.rs setup_keybindings — VTE consumes keys before ShortcutController.
    window.add_controller(ctrl);
}

fn help_key_matches(
    expected_key: gdk::Key,
    expected_modifiers: gdk::ModifierType,
    key: gdk::Key,
    modifiers: gdk::ModifierType,
    keycode: u32,
) -> bool {
    let direct_match = key == expected_key;
    let shifted_letter_match = expected_modifiers.contains(gdk::ModifierType::SHIFT_MASK)
        && key.to_upper() == expected_key.to_upper();
    let shifted_symbol_match = if !direct_match
        && !shifted_letter_match
        && expected_modifiers.contains(gdk::ModifierType::SHIFT_MASK)
    {
        use gdk::prelude::DisplayExtManual;
        gdk::Display::default()
            .and_then(|display| display.translate_key(keycode, gdk::ModifierType::empty(), 0))
            .is_some_and(|(base_key, _, _, _)| base_key == expected_key)
    } else {
        false
    };
    if !direct_match && !shifted_letter_match && !shifted_symbol_match {
        return false;
    }

    let accelerator_modifiers = gdk::ModifierType::SHIFT_MASK
        | gdk::ModifierType::CONTROL_MASK
        | gdk::ModifierType::ALT_MASK
        | gdk::ModifierType::SUPER_MASK
        | gdk::ModifierType::HYPER_MASK
        | gdk::ModifierType::META_MASK;
    let mut actual_modifiers = modifiers & accelerator_modifiers;
    let shifted_symbol = !expected_modifiers.contains(gdk::ModifierType::SHIFT_MASK)
        && key
            .to_unicode()
            .is_some_and(|character| !character.is_alphanumeric() && !character.is_whitespace());
    if shifted_symbol {
        actual_modifiers.remove(gdk::ModifierType::SHIFT_MASK);
    }

    actual_modifiers == expected_modifiers
}

fn help_trigger_matches(
    trigger: &str,
    key: gdk::Key,
    modifiers: gdk::ModifierType,
    keycode: u32,
) -> bool {
    let Some((expected_key, expected_modifiers)) = gtk::accelerator_parse(trigger) else {
        return false;
    };
    help_key_matches(expected_key, expected_modifiers, key, modifiers, keycode)
}

fn install_contextual_help_shortcut(
    window: &adw::ApplicationWindow,
    keybindings: &keybindings::KeybindingConfig,
) {
    let trigger = keybindings
        .trigger(keybindings::Action::ShortcutHelp)
        .to_string();
    if trigger.is_empty() {
        return;
    }
    let controller = gtk::EventControllerKey::new();
    let window_for_action = window.clone();
    controller.connect_key_pressed(move |_, key, keycode, modifiers| {
        if help_trigger_matches(&trigger, key, modifiers, keycode) {
            keybindings::activate(&window_for_action, keybindings::Action::ShortcutHelp);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    // Bubble phase is intentional: text-input widgets and VTE consume literal
    // `?` first, while ordinary app chrome lets it reach this help shortcut.
    window.add_controller(controller);
}

/// Wire the periodic poller timers for agent status, tmux metadata, host
/// probes, the broadcast indicator, and the auto-save session loop.
///
/// Returns the `Rc<RefCell<Option<glib::SourceId>>>` that holds the
/// auto-save source id so the shutdown path can cancel the periodic tick
/// before running the final save.
#[allow(clippy::too_many_arguments)] // Application setup keeps the explicit GTK and persistence handles at one wiring boundary.
fn install_periodic_pollers(
    state: &Rc<RefCell<AppState>>,
    session_writer: &Rc<session::SessionWriter>,
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    broadcast_indicator: &gtk::Box,
    broadcast_label: &gtk::Label,
) -> Rc<RefCell<Option<glib::SourceId>>> {
    // ── Agent status polling every 3s ──
    {
        let state = state.clone();
        let runtime = runtime.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        let window = window.clone();
        // Single-flight guard: a tick that fires while the probe build is still
        // in flight is coalesced away, never queued.
        let probe_in_flight = crate::runtime_probe::ProbeInFlight::default();
        let probe_interval_seconds =
            u32::try_from(crate::runtime_probe::PROBE_REFRESH_INTERVAL_MS / 1_000)
                .expect("probe refresh interval fits a u32 second count");
        glib::timeout_add_seconds_local(probe_interval_seconds, move || {
            app_runtime::update_agent_indicators(
                &runtime,
                &term_stack,
                &tab_list,
                &window,
                &probe_in_flight,
            );
            sidebar::update_workspace_status_dots(&tab_list, &state);
            glib::ControlFlow::Continue
        });
    }

    // ── Live agent transcript tailing every 1s ──
    // Reads each running pane agent's on-disk transcript incrementally, off the
    // main thread. A dedicated single-flight guard keeps overlapping polls from
    // piling up, and an empty-bindings early-out keeps the tick cheap at idle.
    {
        let runtime = runtime.clone();
        let tab_list = tab_list.clone();
        let tracker =
            std::sync::Arc::new(crate::agents::TranscriptTracker::with_default_adapters());
        let transcript_in_flight = crate::runtime_probe::ProbeInFlight::default();
        glib::timeout_add_seconds_local(1, move || {
            app_runtime::update_pane_transcripts(
                &runtime,
                &tracker,
                &transcript_in_flight,
                &tab_list,
            );
            glib::ControlFlow::Continue
        });
    }

    // ── Work-ledger observation every 2s ──
    // The probe contains only typed pane/activity/.plan facts. Consecutive
    // unchanged observations are folded by the ledger state machines.
    {
        let state = state.clone();
        let in_flight = crate::runtime_probe::ProbeInFlight::default();
        glib::timeout_add_seconds_local(2, move || {
            let Some(guard) = in_flight.try_begin() else {
                return glib::ControlFlow::Continue;
            };
            let probe = {
                let state = state.borrow();
                crate::work_ledger::collect_probe(&state)
            };
            let state = state.clone();
            glib::spawn_future_local(async move {
                let _guard = guard;
                let Ok(probe) =
                    gio::spawn_blocking(move || crate::work_ledger::enrich_plan_probe(probe)).await
                else {
                    return;
                };
                state.borrow_mut().observe_work_probe(probe);
            });
            glib::ControlFlow::Continue
        });
    }

    // ── tmux metadata polling every 5s ──
    {
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        glib::timeout_add_seconds_local(crate::tmux::TMUX_METADATA_POLL_SECONDS, move || {
            crate::terminal::poll_tmux_metadata(&state, &tab_list);
            crate::terminal::poll_dashboard_state(&state, &term_stack, &tab_list, &window);
            glib::ControlFlow::Continue
        });
    }

    // ── Host status polling every 15s ──
    {
        let state = state.clone();
        glib::timeout_add_seconds_local(15, move || {
            crate::terminal::poll_host_status(&state);
            glib::ControlFlow::Continue
        });
    }

    {
        let state = state.clone();
        let indicator = broadcast_indicator.clone();
        let label = broadcast_label.clone();
        glib::timeout_add_seconds_local(1, move || {
            // Skip the per-tick target scan and widget updates while broadcast
            // is off — the action paths already hide the indicator when scope
            // is cleared, so there is nothing for the tick to do.
            if state.borrow().broadcast_scope().is_some() {
                refresh_broadcast_indicator(&indicator, &label, &state);
            }
            glib::ControlFlow::Continue
        });
    }

    // ── Auto-save session every 60s ──
    // Track the SourceId so shutdown paths can cancel the periodic timer
    // before running the final save, preventing a stale periodic tick from
    // overwriting the authoritative shutdown snapshot.
    let auto_save_source: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    {
        let state = state.clone();
        let session_writer = session_writer.clone();
        let tab_list = tab_list.clone();
        let window = window.clone();
        let source_id = glib::timeout_add_seconds_local(60, move || {
            app_session::save_session_async(&session_writer, &state, &tab_list, &window);
            glib::ControlFlow::Continue
        });
        *auto_save_source.borrow_mut() = Some(source_id);
    }
    auto_save_source
}

fn install_update_watcher(update_pending_button: &gtk::Button) {
    // `spawn_refresh` is single-flight and performs all IO/hashing on a
    // blocking worker. The GTK tick only schedules work and paints cached data.
    crate::sidebar::refresh_update_pending_chip(update_pending_button);
    let button = update_pending_button.clone();
    glib::timeout_add_local(crate::update_watch::poll_interval(), move || {
        crate::update_watch::spawn_refresh();
        crate::sidebar::refresh_update_pending_chip(&button);
        glib::ControlFlow::Continue
    });
    update_watch::spawn_refresh();
}

fn build_ui(app: &adw::Application, resume_agents_after_reload: bool) {
    terminal::install_taarof_activity_termprops();

    let keybindings = Rc::new(keybindings::KeybindingConfig::load());
    keybindings::install(&keybindings);
    let runtime = RuntimeHandle::new();
    let state = runtime.shared_state();
    // Install the shared AppState handle used to label clipboard-history copy
    // sources (EXAMPLE-88). Must run before any copy can occur.
    terminal::install_clipboard_ring_state(&state);
    crate::diagnostics::record_lifecycle(
        "startup",
        "taarof session started",
        Some(serde_json::json!({
            "pid": std::process::id(),
            "session_name": crate::instance::session_name(),
        })),
    );

    // Terminal stack — holds one VTE widget per tab
    let term_stack = gtk::Stack::new();
    term_stack.set_hexpand(true);
    term_stack.set_vexpand(true);
    term_stack.add_css_class("terminal-area");

    // ── F002: Search overlay wrapping term_stack ──
    let search_bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    search_bar.add_css_class("search-bar");
    search_bar.set_halign(gtk::Align::End);
    search_bar.set_valign(gtk::Align::Start);
    search_bar.set_margin_top(8);
    search_bar.set_margin_end(8);
    search_bar.set_visible(false);

    let search_entry = gtk::SearchEntry::new();
    search_entry.set_hexpand(true);
    search_bar.append(&search_entry);

    // F007: Command palette overlay
    let cmd_palette = palette::build_command_palette();

    // Reusable overlay for quick-action and leader follow-up hints.
    let chord_overlay = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    chord_overlay.add_css_class("quick-action-overlay");
    chord_overlay.set_halign(gtk::Align::Center);
    chord_overlay.set_valign(gtk::Align::End);
    chord_overlay.set_margin_bottom(18);
    chord_overlay.set_focusable(true);
    chord_overlay.set_visible(false);

    let broadcast_indicator = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    broadcast_indicator.add_css_class("broadcast-indicator");
    broadcast_indicator.set_halign(gtk::Align::Center);
    broadcast_indicator.set_valign(gtk::Align::Start);
    broadcast_indicator.set_margin_top(18);
    broadcast_indicator.set_visible(false);

    let broadcast_label = gtk::Label::new(None);
    broadcast_label.add_css_class("broadcast-label");
    let broadcast_off_button = gtk::Button::with_label("Turn Off");
    broadcast_off_button.add_css_class("broadcast-button");
    broadcast_indicator.append(&broadcast_label);
    broadcast_indicator.append(&broadcast_off_button);

    let restore_legend = RestoreLegendUi::new(state.clone());
    let restore_legend_dismiss: Rc<dyn Fn(u32)> = {
        let restore_legend = restore_legend.clone();
        Rc::new(move |tab_id| {
            restore_legend.dismiss_tab(tab_id, false);
        })
    };

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&term_stack));
    overlay.add_overlay(&search_bar);
    overlay.add_overlay(&broadcast_indicator);
    overlay.add_overlay(&restore_legend.container);
    overlay.add_overlay(&cmd_palette.container);
    overlay.add_overlay(&chord_overlay);

    // Sidebar — tab list + session info
    let sb = sidebar::build_sidebar();
    sidebar::apply_compact_mode(&sb.container, config::sidebar_compact());
    let sidebar_box = sb.container;
    let tab_list = sb.tab_list;
    let tab_search = sb.tab_search;
    let update_pending_button = sb.update_pending_button;
    let tmux_button = sb.tmux_button;

    // Tab search (EXAMPLE-56): live-filter the tab list; Enter jumps to the top
    // match; Esc clears the filter (restoring the collapse-correct view).
    {
        let state = state.clone();
        let tab_list = tab_list.clone();
        tab_search.connect_search_changed(move |entry| {
            sidebar::apply_tab_search_filter(&tab_list, &state, entry.text().as_str());
        });
    }
    {
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        tab_search.connect_activate(move |entry| {
            if sidebar::activate_first_tab_match(
                &tab_list,
                &state,
                &term_stack,
                entry.text().as_str(),
            ) {
                if let Some(terminal) = get_active_terminal(&state) {
                    terminal.grab_focus();
                }
            }
        });
    }
    tab_search.connect_stop_search(|entry| {
        entry.set_text("");
    });

    let main_content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    main_content.set_hexpand(true);
    main_content.set_vexpand(true);
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
    main_content.append(&overlay);

    // Draggable paned layout: sidebar | terminal + task dock
    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    configure_sidebar_layout(&paned, &sidebar_box, &main_content);
    // Toast overlay wraps the main content for error notifications
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&paned));
    set_toast_overlay(toast_overlay.clone());

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("taarof")
        .default_width(1200)
        .default_height(800)
        .content(&toast_overlay)
        .build();
    window.set_icon_name(Some(DESKTOP_APP_ID));

    // The dock is built once so a runtime toggle is instant, but starts hidden
    // unless `[dock] visible = true`. Task and PR polling remain independently
    // controlled by `[tasks]`.
    let task_panel = task_panel::build_task_panel(
        state.clone(),
        tab_list.clone(),
        term_stack.clone(),
        window.clone(),
    );
    task_panel.container.set_visible(config::dock_visible());
    main_content.append(&task_panel.container);
    let live_config_watcher = install_live_config_watcher(LiveConfigUiHandles {
        state: state.clone(),
        tab_list: tab_list.clone(),
        content: paned.clone(),
        sidebar: sidebar_box.clone(),
        main_content: main_content.clone(),
        tmux_button: tmux_button.clone(),
        task_dock: task_panel.container.clone(),
    });

    let workspace_inspector = inspector::build_workspace_inspector(
        state.clone(),
        tab_list.clone(),
        term_stack.clone(),
        window.clone(),
    );
    overlay.add_overlay(&workspace_inspector.container);

    let history_view =
        history_view::ui::build_history_view(state.clone(), tab_list.clone(), window.clone());
    overlay.add_overlay(&history_view.container);

    // Peek overlay: instant in-app file viewer (self-registers into a
    // thread-local so terminal.rs / palette.rs can call peek::peek_path).
    let peek_overlay = peek::build_peek_overlay(state.clone());
    overlay.add_overlay(&peek_overlay.container);

    // Escape owns peek dismissal here rather than on the overlay widget: a
    // controller on the overlay only sees keys while the overlay subtree holds
    // focus, and clicking the sidebar or task panel takes focus away — which left
    // the overlay covering the pane with no way to close it (EXAMPLE-140). The
    // capture phase runs toplevel-down, so this sees Escape before any
    // overlay-level controller, and peek is the topmost overlay, so consuming
    // Escape while it is visible matches what the user sees.
    {
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.set_propagation_phase(gtk::PropagationPhase::Capture);
        key_ctrl.connect_key_pressed(move |_ctrl, key, _code, _mods| {
            if key == gdk::Key::Escape && peek::dismiss_if_visible() {
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(key_ctrl);
    }

    // Force dark theme
    let style_manager = adw::StyleManager::default();
    style_manager.set_color_scheme(adw::ColorScheme::ForceDark);

    {
        let state = state.clone();
        let indicator = broadcast_indicator.clone();
        let label = broadcast_label.clone();
        broadcast_off_button.connect_clicked(move |_| {
            state.borrow_mut().set_broadcast_scope(None);
            refresh_broadcast_indicator(&indicator, &label, &state);
            if let Some(terminal) = get_active_terminal(&state) {
                terminal.grab_focus();
            }
        });
    }
    refresh_broadcast_indicator(&broadcast_indicator, &broadcast_label, &state);

    // ── Restore previous session or create a default tab ──
    let eager_auto_resume_agents = terminal::SessionRestoreResumePolicy::eager(
        config::should_auto_resume_agents_on_session_restore(),
        resume_agents_after_reload,
    );
    let startup_session = session::load_v2_for_startup();
    let persistence_block = startup_session.writer_block_reason.clone();
    if let Some(diagnostic) = startup_session.diagnostic.as_deref() {
        diagnostics::record_session_recovery(diagnostic);
        show_toast(diagnostic);
    } else if startup_session.state.is_some() {
        show_toast(
            "Reopened layout. Display checkpoints are context only; live terminal and agent resume validate separately.",
        );
    }
    let saved = startup_session.state;
    if let Some(ref sess) = saved {
        window.set_default_size(sess.window_width, sess.window_height);
    }

    // Install the lazy-restore context so `sync_active_selection_ui` can build a
    // pending tab's real pane tree on first activation. Set before the restore
    // block runs any activation.
    set_lazy_restore_ctx(window.clone(), restore_legend_dismiss.clone());

    let saved_session_name = saved.as_ref().and_then(|s| s.session_name.clone());
    let saved_detached: Vec<dashboard::SavedDetachedSession> = saved
        .as_ref()
        .map(|s| s.detached_sessions.clone())
        .unwrap_or_default();
    let saved_bg_collapsed: bool = saved
        .as_ref()
        .and_then(|s| s.background_section_collapsed)
        .unwrap_or(true);

    // Git metadata is observational at restore time. Capture checkout paths
    // while rebuilding state, then refresh them after the first visible tab is
    // available instead of making launch wait on filesystem walks or Git.
    let mut pending_git_refreshes: Vec<(u32, String)> = Vec::new();

    if let Some(sess) = saved {
        if !sess.workspaces.is_empty() {
            runtime.clear_for_session_restore();

            // Track which workspace should be active after restore.
            let mut restored_active_ws_id: Option<u32> = None;
            let active_ws_idx = sess
                .active_workspace_index
                .min(sess.workspaces.len().saturating_sub(1));

            for (ws_idx, saved_ws) in sess.workspaces.iter().enumerate() {
                // Re-discover git info from the actual checkout path later, on
                // a worker. The saved metadata is enough to render and restore
                // the first usable tab without blocking startup.
                let git_discovery_path = workspace_git_discovery_path(saved_ws).map(str::to_string);

                let ws_id = {
                    let id = runtime.create_workspace(&saved_ws.name, saved_ws.repo_root.clone());
                    let mut st = state.borrow_mut();
                    st.apply_restored_workspace_work_origin(id, saved_ws.work_origin.as_deref());
                    if let Some(ws) = st.active_ws_mut() {
                        ws.repo_root = saved_ws.repo_root.clone();
                        ws.is_worktree = saved_ws.is_worktree;
                        ws.working_tree_path = saved_ws.working_tree_path.clone();
                        ws.linked_issue = saved_ws.linked_issue.clone();
                        ws.tmux_backed = saved_ws.tmux_backed;
                        ws.host_config_name = saved_ws.host_config_name.clone();
                        ws.branch_name = saved_ws.branch_name.clone();
                        ws.is_worktree |= saved_ws.is_worktree;
                        if ws.working_tree_path.is_none() {
                            ws.working_tree_path = saved_ws.working_tree_path.clone();
                        }
                        if ws.repo_root.is_none() {
                            ws.repo_root = saved_ws.repo_root.clone();
                        }
                    }
                    id
                };

                // Saved metadata is rendered immediately; fresh Git status is
                // applied asynchronously after the restore window.
                let ws_name = &saved_ws.name;
                let header_label = ws_name.clone();

                sidebar::add_workspace_header(&tab_list, &state, &term_stack, ws_id, &header_label);
                if let Some(path) = git_discovery_path {
                    pending_git_refreshes.push((ws_id, path));
                }

                // Lazy restore (EXAMPLE-84): only the active workspace's active tab
                // is fully built and spawned at startup. Every other saved tab is
                // registered as a lightweight `PaneNode::Empty` tab (sidebar row
                // still added) whose real pane tree and shells materialize on
                // first activation. This keeps startup cost off the total pane
                // count and avoids opening every tmux/SSH connection at once.
                let is_active_ws = ws_idx == active_ws_idx;
                let active_tab_idx = saved_ws
                    .active_tab_index
                    .min(saved_ws.tabs.len().saturating_sub(1));

                let mut ws_tab_ids = Vec::new();
                let mut eager_discovery_tab: Option<u32> = None;
                for (tab_idx, tab) in saved_ws.tabs.iter().enumerate() {
                    let tab_id = if is_active_ws && tab_idx == active_tab_idx {
                        // Eager path: build the visible tab now.
                        let tab_id = terminal::restore_tab(
                            &runtime,
                            &term_stack,
                            &tab.name,
                            tab.panes.as_ref(),
                            tab.cwd.as_deref(),
                            eager_auto_resume_agents,
                            true,
                        );
                        sidebar::add_tab_row(
                            &tab_list,
                            &state,
                            &term_stack,
                            tab_id,
                            &tab.name,
                            false,
                        );
                        terminal::wire_tab_terminals_with_restore_dismiss(
                            &state,
                            &term_stack,
                            &tab_list,
                            &window,
                            tab_id,
                            Some(restore_legend_dismiss.clone()),
                        );
                        if tab.discovery_cwd.is_some() {
                            eager_discovery_tab = Some(tab_id);
                        }
                        tab_id
                    } else {
                        // Lazy path: register a hidden tab; no stack child, no
                        // wiring, no spawn until it is first activated.
                        let tab_id = terminal::register_lazy_restored_tab(
                            &runtime,
                            &tab.name,
                            tab.panes.as_ref(),
                            tab.cwd.as_deref(),
                            true,
                        );
                        sidebar::add_tab_row(
                            &tab_list,
                            &state,
                            &term_stack,
                            tab_id,
                            &tab.name,
                            false,
                        );
                        tab_id
                    };
                    {
                        let mut st = state.borrow_mut();
                        st.apply_restored_tab_work_origin(tab_id, tab.work_origin.as_deref());
                        // Preserve discovery_cwd so it round-trips before a lazy
                        // tab opens; eager tabs also regain the same identity.
                        if let Some(live_tab) = st.find_tab_mut(tab_id) {
                            live_tab.discovery_cwd =
                                app_session::normalize_persisted_path(tab.discovery_cwd.as_deref());
                        }
                    }
                    ws_tab_ids.push(tab_id);
                }
                // Schedule deferred re-discovery for the eager tab only; lazy tabs
                // run discovery inside `materialize_pending_tab` on activation.
                if let Some(tab_id) = eager_discovery_tab {
                    let state = state.clone();
                    let tab_list = tab_list.clone();
                    glib::timeout_add_local_once(Duration::from_millis(500), move || {
                        // Restore-time discovery refreshes mise data / tool-version
                        // chips but does not reveal task UI; that stays opt-in.
                        sidebar::discover_for_tab(&state, &tab_list, tab_id, false);
                    });
                }

                // Restore active tab for this workspace.
                if let Some(&active_tab_id) = ws_tab_ids.get(active_tab_idx) {
                    let _ = runtime.set_workspace_active_tab(ws_id, active_tab_id);
                }

                // Apply collapse state (workspace starts expanded; toggle collapses it)
                if saved_ws.collapsed {
                    sidebar::toggle_workspace_collapse(&tab_list, &state, ws_id);
                }

                if ws_idx == active_ws_idx {
                    restored_active_ws_id = Some(ws_id);
                }
            }

            if let Some(active_ws_id) = restored_active_ws_id {
                sidebar::activate_workspace(&tab_list, &state, &term_stack, active_ws_id);
                sidebar::refresh_all_tab_rows(&tab_list, &state);
            }
            runtime.reset_navigation_history();
        } else {
            // Empty workspaces in saved session — create default
            app_session::restore_default_workspace(&runtime, &tab_list, &term_stack, &window);
        }
    } else {
        // No saved session — create default
        app_session::restore_default_workspace(&runtime, &tab_list, &term_stack, &window);
    }

    // This placement is intentionally after default/restore tab construction:
    // launch-to-first-usable-tab stays independent of Git filesystem latency.
    for (workspace_id, discovery_path) in pending_git_refreshes {
        let state_for_apply = state.clone();
        let tab_list_for_apply = tab_list.clone();
        let workspace_origin = state
            .borrow()
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .map(|workspace| workspace.work_origin.clone());
        let request_key = format!("discover:restore-workspace:{workspace_id}");
        let submission = git::spawn_async(
            request_key,
            move || git::discover(&discovery_path),
            move |git_info| {
                let mut st = state_for_apply.borrow_mut();
                let Some(workspace) = st.workspaces.iter_mut().find(|workspace| {
                    workspace.id == workspace_id
                        && workspace_origin.as_deref() == Some(workspace.work_origin.as_str())
                }) else {
                    return;
                };
                apply_git_info_to_workspace(workspace, &git_info);
                let workspace_name = workspace.name.clone();
                drop(st);
                // Repaint only after the identity check above; a removed/reused
                // workspace can never receive a late worker result.
                sidebar::set_workspace_header_label(
                    &tab_list_for_apply,
                    &state_for_apply,
                    workspace_id,
                    &workspace_header_label(&workspace_name, &git_info),
                );
            },
        );
        if matches!(submission, git::GitAsyncSubmission::Saturated) {
            crate::show_toast(
                "Git metadata refresh is busy; restored workspace details will refresh later",
            );
        }
    }

    // Restore detached sessions
    {
        let mut st = state.borrow_mut();
        for saved_ds in &saved_detached {
            st.detached_sessions
                .push(crate::dashboard::DetachedSession::from_saved(saved_ds));
        }
        st.background_section_collapsed = saved_bg_collapsed;
    }

    // Restore session name if saved
    if let Some(ref name) = saved_session_name {
        if let Some(label) = sidebar::session_label(&tab_list) {
            label.set_text(name);
        }
    }

    crate::terminal::poll_dashboard_state(&state, &term_stack, &tab_list, &window);

    {
        let restore_legend = restore_legend.clone();
        term_stack.connect_notify_local(Some("visible-child-name"), move |_, _| {
            restore_legend.refresh();
        });
    }
    restore_legend.refresh();

    let mut action_ledger = keybindings::WindowActionLedger::default();
    install_new_tab_actions(&window, &mut action_ledger, &state, &term_stack, &tab_list);
    install_search_palette_actions(
        &window,
        &mut action_ledger,
        &state,
        &term_stack,
        &tab_list,
        &search_bar,
        &search_entry,
        &cmd_palette,
        &workspace_inspector,
        &history_view,
    );
    install_broadcast_actions(
        &window,
        &mut action_ledger,
        &state,
        &broadcast_indicator,
        &broadcast_label,
    );
    install_dock_action(
        &window,
        &mut action_ledger,
        &state,
        &task_panel.container,
        &sidebar_box,
        &tab_list,
        &paned,
        &main_content,
    );
    install_discovery_and_task_actions(&window, &mut action_ledger, &state, &tab_list);

    let chord_timeout = Duration::from_secs(2);
    install_chord_overlay_actions(
        &window,
        &mut action_ledger,
        &state,
        &chord_overlay,
        &tab_list,
        &term_stack,
        &keybindings,
        chord_timeout,
    );
    install_split_actions(
        &window,
        &mut action_ledger,
        &state,
        &runtime,
        &term_stack,
        &tab_list,
    );
    install_workspace_actions(
        &window,
        &mut action_ledger,
        &state,
        &runtime,
        &term_stack,
        &tab_list,
    );
    validate_window_action_contract(&window, &action_ledger);
    install_global_shortcuts(&window, &keybindings);
    install_contextual_help_shortcut(&window, &keybindings);

    // Capture environment-derived agent-session roots on GTK before the writer
    // starts. Provider filesystem scans still happen only on the worker.
    crate::agent_sessions::initialize_default_catalog();

    // This is deliberately initialized after restore/default-tab creation: it
    // affects persistence after the first usable tab, never the launch window.
    let session_writer = Rc::new(
        match persistence_block {
            Some(reason) => session::SessionWriter::blocked(reason),
            None => session::SessionWriter::start(),
        }
        .expect("session persistence writer should start"),
    );
    let auto_save_source = install_periodic_pollers(
        &state,
        &session_writer,
        &runtime,
        &term_stack,
        &tab_list,
        &window,
        &broadcast_indicator,
        &broadcast_label,
    );
    install_update_watcher(&update_pending_button);

    // ── F008: Start Unix socket notification server ──
    let socket_path = socket::start_notify_server(
        state.clone(),
        term_stack.clone(),
        tab_list.clone(),
        window.clone(),
    );

    // ── Start HTTP API server (opt-in) ──
    let http_runtime_dir = socket_path
        .as_ref()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let http_config = config::http_config();
    let http_control_config = config::http_control_config();
    let http_runtime_dir_for_cleanup = http_runtime_dir.clone();
    let pane_dirty_for_http = state.borrow().pane_dirty.clone();
    if let Some((http_bridge_rx, http_event_tx)) = http_runtime_dir.as_ref().and_then(|dir| {
        http::start_http_server_with_control_config_dirty_and_history(
            dir,
            &http_config,
            &http_control_config,
            pane_dirty_for_http,
            state.borrow().history_reader.clone(),
        )
    }) {
        state
            .borrow_mut()
            .event_store
            .install_live_sink(http_event_tx);
        let state_for_http = state.clone();
        let term_stack_for_http = term_stack.clone();
        let tab_list_for_http = tab_list.clone();
        let window_for_http = window.clone();
        let runtime_for_http = runtime.clone();
        glib::MainContext::default().spawn_local(async move {
            let mut http_bridge_rx = http_bridge_rx;
            while let Some(request) = http_bridge_rx.recv().await {
                match request {
                    http::HttpBridgeRequest::QueryState { reply } => {
                        let snapshot = {
                            let st = state_for_http.borrow();
                            crate::api::build_state_snapshot(&st)
                        };
                        let _ = reply.send(snapshot);
                    }
                    http::HttpBridgeRequest::QueryStateProjection { projection, reply } => {
                        let snapshot = {
                            let st = state_for_http.borrow();
                            crate::api::build_state_projection(&st, projection)
                        };
                        let _ = reply.send(snapshot);
                    }
                    http::HttpBridgeRequest::QueryHealth { reply } => {
                        let snapshot = {
                            let st = state_for_http.borrow();
                            crate::api::build_health_snapshot(&st)
                        };
                        let _ = reply.send(snapshot);
                    }
                    http::HttpBridgeRequest::QueryEvents {
                        since_seq,
                        limit,
                        reply,
                    } => {
                        let snapshot = {
                            let st = state_for_http.borrow();
                            crate::api::build_events_snapshot(&st.event_store, since_seq, limit)
                        };
                        let _ = reply.send(snapshot);
                    }
                    http::HttpBridgeRequest::QueryAgentBindings { reply } => {
                        let bindings = {
                            let st = state_for_http.borrow();
                            crate::agent_sessions::build_live_agent_bindings(&st)
                        };
                        let _ = reply.send(bindings);
                    }
                    http::HttpBridgeRequest::ResolvePaneAttach {
                        tab_id,
                        pane_id,
                        reply,
                    } => {
                        let target = {
                            let st = state_for_http.borrow();
                            match tab_id {
                                Some(tab_id) => {
                                    http::resolve_pane_attach_target_in_tab(&st, tab_id, pane_id)
                                }
                                None => http::resolve_pane_attach_target(&st, pane_id),
                            }
                        };
                        let _ = reply.send(target);
                    }
                    http::HttpBridgeRequest::CaptureVtePaneSnapshot {
                        tab_id,
                        pane_id,
                        reply,
                    } => {
                        let result = {
                            let st = state_for_http.borrow();
                            http::capture_vte_pane_snapshot_sync(&st, tab_id, pane_id)
                        };
                        let _ = reply.send(result);
                    }
                    http::HttpBridgeRequest::ResolvePtyAdapter {
                        tab_id,
                        pane_id,
                        reply,
                    } => {
                        let resolution = {
                            let st = state_for_http.borrow();
                            http::resolve_pty_adapter_target(&st, tab_id, pane_id)
                        };
                        let _ = reply.send(resolution);
                    }
                    http::HttpBridgeRequest::DispatchPtyInput {
                        tab_id,
                        pane_id,
                        guard,
                        payload,
                        cancelled,
                        admission_lock,
                        reply,
                    } => {
                        let _admission = admission_lock.lock().unwrap();
                        if reply.is_closed() {
                            continue;
                        }
                        let result = {
                            let st = state_for_http.borrow();
                            http::dispatch_pty_input(
                                &st, tab_id, pane_id, &guard, payload, cancelled,
                            )
                        };
                        // Admission only: the async peer awaits actual delivery.
                        // If it disconnected, dropping the receipt cancels input.
                        let _ = reply.send(result);
                    }
                    http::HttpBridgeRequest::DispatchPtyResize {
                        tab_id,
                        pane_id,
                        guard,
                        cols,
                        rows,
                        reply,
                    } => {
                        let result = {
                            let st = state_for_http.borrow();
                            http::dispatch_pty_resize(&st, tab_id, pane_id, &guard, cols, rows)
                        };
                        let _ = reply.send(result);
                    }
                    http::HttpBridgeRequest::EmitEvent {
                        event_type,
                        payload,
                    } => {
                        runtime_for_http.emit_event(&event_type, payload);
                    }
                    http::HttpBridgeRequest::ControlAction {
                        action,
                        guard,
                        reply,
                    } => {
                        socket::handle_http_control_action_async_with_callback(
                            &state_for_http,
                            &term_stack_for_http,
                            &tab_list_for_http,
                            &window_for_http,
                            action,
                            guard,
                            move |result| {
                                let _ = reply.send(result);
                            },
                        );
                    }
                }
                glib::timeout_future(Duration::from_millis(0)).await;
            }
        });
    }

    let session_saved = Rc::new(Cell::new(false));
    let socket_cleaned = Rc::new(Cell::new(false));

    // ── Save session on window close + clean up socket ──
    {
        let state = state.clone();
        let session_writer = session_writer.clone();
        let tab_list = tab_list.clone();
        let window_ref = window.clone();
        let session_saved = session_saved.clone();
        let socket_cleaned = socket_cleaned.clone();
        let socket_path = socket_path.clone();
        let http_dir_for_close = http_runtime_dir_for_cleanup.clone();
        let auto_save_source_close = auto_save_source.clone();
        let live_config_watcher_close = live_config_watcher.clone();
        window.connect_close_request(move |_win| {
            if let Some(watcher) = &live_config_watcher_close {
                watcher.cancel();
            }
            // Cancel the periodic auto-save so it cannot race with the final
            // shutdown save below.
            if let Some(source_id) = auto_save_source_close.borrow_mut().take() {
                source_id.remove();
            }
            app_session::save_session_once(
                &session_saved,
                &session_writer,
                &state,
                &tab_list,
                &window_ref,
            );
            flush_history(&state);
            cleanup_socket_once(&socket_cleaned, socket_path.as_deref());
            if let Some(dir) = &http_dir_for_close {
                http::cleanup_token_file(dir);
            }
            glib::Propagation::Proceed
        });
    }

    {
        let runtime = runtime.clone();
        let socket_cleaned = socket_cleaned.clone();
        let socket_path = socket_path.clone();
        let http_dir_for_shutdown = http_runtime_dir_for_cleanup.clone();
        let live_config_watcher_shutdown = live_config_watcher.clone();
        app.connect_shutdown(move |_| {
            if let Some(watcher) = &live_config_watcher_shutdown {
                watcher.cancel();
            }
            runtime.emit_event(
                "session_stopping",
                serde_json::json!({
                    "session_name": crate::instance::session_name(),
                }),
            );
            crate::diagnostics::record_lifecycle(
                "shutdown",
                "taarof session stopping",
                Some(serde_json::json!({
                    "pid": std::process::id(),
                    "session_name": crate::instance::session_name(),
                })),
            );
            flush_history(&runtime.shared_state());
            cleanup_socket_once(&socket_cleaned, socket_path.as_deref());
            if let Some(dir) = &http_dir_for_shutdown {
                http::cleanup_token_file(dir);
            }
        });
    }

    app_session::install_signal_cleanup(
        app,
        &session_writer,
        &state,
        &tab_list,
        &window,
        &session_saved,
        &socket_cleaned,
        socket_path.as_deref(),
        &auto_save_source,
    );

    window.present();
}

fn cleanup_socket_once(cleaned: &Cell<bool>, socket_path: Option<&std::path::Path>) {
    if cleaned.replace(true) {
        return;
    }
    if let Some(path) = socket_path {
        socket::cleanup_socket(path);
    }
}

pub(crate) fn flush_history(state: &Rc<RefCell<AppState>>) {
    if let Err(error) = state.borrow().history.flush() {
        eprintln!("taarof: could not flush history during shutdown: {error}");
    }
}

/// Escape special PCRE2 regex characters in a string for literal matching.
fn regex_escape(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        match c {
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    escaped
}

fn terminal_search_regex_flags() -> u32 {
    PCRE2_CASELESS | PCRE2_MULTILINE
}

/// Get the focused pane's terminal for the currently active tab.
pub fn get_active_terminal(state: &Rc<RefCell<AppState>>) -> Option<vte::Terminal> {
    let st = state.borrow();
    let tab = st.active_tab()?;
    tab.panes.focused_terminal(tab.focused_pane_id).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn synthetic_agent_build_info(revision: &str, dirty: bool) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "taarof-agent-build-info-{}-{revision}",
            std::process::id()
        ));
        let value = serde_json::json!({
            "source_revision": revision,
            "source_dirty": dirty,
        });
        std::fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", value)).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[test]
    fn saved_resume_helper_rejects_a_replaced_channel_launcher() {
        let mut app = runtime_identity::build_identity();
        app.source_revision = Some("original-revision".into());
        app.source_dirty = Some(false);
        let matching = synthetic_agent_build_info("original-revision", false);
        assert_eq!(validate_agent_launcher_identity(&matching, &app), Ok(()));

        let replacement = synthetic_agent_build_info("replacement-revision", false);
        assert_eq!(
            validate_agent_launcher_identity(&replacement, &app),
            Err("the agent launcher does not match the running app build")
        );
        std::fs::remove_file(matching).unwrap();
        std::fs::remove_file(replacement).unwrap();
    }

    #[test]
    fn saved_resume_helper_rejects_replacement_app_build() {
        let original = runtime_identity::build_identity();
        let mut replacement = original.clone();
        replacement.build_id = Some("replacement-build".into());
        assert_eq!(
            validate_resume_helper_identity(&original, &original),
            Ok(())
        );
        assert_eq!(
            validate_resume_helper_identity(&replacement, &original),
            Err("the resume helper does not match the parent app build")
        );
    }

    #[test]
    fn app_build_info_reports_the_embedded_app_identity() {
        let value = app_build_info();
        assert_eq!(value["schema"], "taarof.app-build.v1");
        assert_eq!(value["app_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            value["build"],
            serde_json::to_value(runtime_identity::build_identity()).unwrap()
        );
    }

    #[test]
    fn left_sidebar_policy_allows_sidebar_to_shrink() {
        let policy = sidebar_paned_policy(config::SidebarPosition::Left, 220);

        assert_eq!(
            policy.divider_position,
            SidebarDividerPosition::FromStart(220)
        );
        assert!(policy.shrink_start_child);
        assert!(!policy.shrink_end_child);
        assert!(!policy.resize_start_child);
        assert!(policy.resize_end_child);
    }

    #[test]
    fn live_config_watch_coalesces_changes_during_an_inflight_reload() {
        let mut state = LiveConfigWatchState::default();
        state.note_change();
        let first = state.begin_reload().expect("first reload should start");
        state.note_change();
        assert_eq!(state.begin_reload(), None, "second reload should queue");
        assert_eq!(state.finish_reload(first), (false, true));

        let latest = state.begin_reload().expect("queued reload should start");
        assert_eq!(state.finish_reload(latest), (true, false));
    }

    #[test]
    fn live_config_watch_cancel_discards_inflight_results_and_future_events() {
        let mut state = LiveConfigWatchState::default();
        state.note_change();
        let generation = state.begin_reload().expect("reload should start");
        state.cancel();

        assert_eq!(state.finish_reload(generation), (false, false));
        state.note_change();
        assert_eq!(state.begin_reload(), None);
    }

    #[test]
    fn live_config_watch_deduplicates_invalid_write_errors_until_success() {
        let mut state = LiveConfigWatchState::default();
        assert!(state.should_report_error());
        assert!(!state.should_report_error());
        assert!(!state.should_report_error());
        state.clear_error();
        assert!(state.should_report_error());
    }

    #[test]
    fn dock_toggle_inverts_runtime_visibility() {
        assert!(toggled_dock_visibility(false));
        assert!(!toggled_dock_visibility(true));
    }

    #[test]
    fn compact_sidebar_policy_preserves_requested_width() {
        let policy = sidebar_paned_policy(config::SidebarPosition::Left, 64);
        assert_eq!(
            policy.divider_position,
            SidebarDividerPosition::FromStart(64)
        );
    }

    #[test]
    fn right_sidebar_policy_allows_sidebar_to_shrink() {
        let policy = sidebar_paned_policy(config::SidebarPosition::Right, 220);

        assert_eq!(
            policy.divider_position,
            SidebarDividerPosition::FromEnd(220)
        );
        assert!(!policy.shrink_start_child);
        assert!(policy.shrink_end_child);
        assert!(policy.resize_start_child);
        assert!(!policy.resize_end_child);
    }

    #[test]
    fn conventional_task_names_stay_typed_workspace_actions() {
        for action in WorkspaceAction::all() {
            assert_eq!(
                WorkspaceAction::from_task_name(action.task_name()),
                Some(*action),
                "conventional task names must resolve through WorkspaceAction"
            );
        }

        // `run-*` was an old GTK-handler vocabulary. Task names remain task
        // data, not action names; EXAMPLE-173 should route all task affordances
        // through the typed task-launch plan rather than revive that surface.
        assert_eq!(WorkspaceAction::from_task_name("run-dev"), None);
        assert!(keybindings::Action::ALL
            .iter()
            .all(|action| !action.gaction_name().starts_with("win.run-")));
    }

    #[test]
    fn appends_wp_presentation_to_gdk_wayland_disable() {
        assert_eq!(
            gdk_wayland_disable_with_presentation(""),
            Some("wp_presentation".to_string())
        );
        assert_eq!(
            gdk_wayland_disable_with_presentation("wl_seat"),
            Some("wl_seat,wp_presentation".to_string())
        );
        // Already present under any GDK-accepted separator: leave untouched.
        assert_eq!(
            gdk_wayland_disable_with_presentation("wp_presentation"),
            None
        );
        assert_eq!(
            gdk_wayland_disable_with_presentation("wl_seat:wp_presentation"),
            None
        );
        assert_eq!(
            gdk_wayland_disable_with_presentation("wl_seat wp_presentation"),
            None
        );
    }

    #[test]
    fn maps_chord_keys_to_workspace_actions() {
        let chord_map = keybindings::default_chord_map();
        assert_eq!(
            workspace_action_from_chord_key(gdk::Key::d, &chord_map),
            Some(WorkspaceAction::Dev)
        );
        assert_eq!(
            workspace_action_from_chord_key(gdk::Key::t, &chord_map),
            Some(WorkspaceAction::Test)
        );
        assert_eq!(
            workspace_action_from_chord_key(gdk::Key::b, &chord_map),
            Some(WorkspaceAction::Build)
        );
        assert_eq!(
            workspace_action_from_chord_key(gdk::Key::l, &chord_map),
            Some(WorkspaceAction::Lint)
        );
        assert_eq!(
            workspace_action_from_chord_key(gdk::Key::s, &chord_map),
            Some(WorkspaceAction::Setup)
        );
    }

    #[test]
    fn normalize_persisted_path_returns_absolute_canonical_path() {
        let unique = format!(
            "taarof-path-normalize-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).unwrap();

        let input = dir
            .parent()
            .unwrap()
            .join(".")
            .join(dir.file_name().unwrap())
            .to_string_lossy()
            .into_owned();

        let normalized = normalize_persisted_path(Some(&input)).unwrap();

        assert!(Path::new(&normalized).is_absolute());
        assert_eq!(normalized, dir.canonicalize().unwrap().to_string_lossy());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn contextual_help_trigger_matches_default_and_custom_modifier() {
        assert!(help_key_matches(
            gdk::Key::question,
            gdk::ModifierType::empty(),
            gdk::Key::question,
            gdk::ModifierType::SHIFT_MASK,
            0
        ));
        assert!(!help_key_matches(
            gdk::Key::question,
            gdk::ModifierType::CONTROL_MASK,
            gdk::Key::question,
            gdk::ModifierType::SHIFT_MASK,
            0
        ));
        assert!(help_key_matches(
            gdk::Key::question,
            gdk::ModifierType::CONTROL_MASK,
            gdk::Key::question,
            gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK,
            0
        ));
        assert!(!help_key_matches(
            gdk::Key::question,
            gdk::ModifierType::empty(),
            gdk::Key::question,
            gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK,
            0
        ));
        assert!(!help_key_matches(
            gdk::Key::question,
            gdk::ModifierType::CONTROL_MASK,
            gdk::Key::question,
            gdk::ModifierType::CONTROL_MASK
                | gdk::ModifierType::ALT_MASK
                | gdk::ModifierType::SHIFT_MASK,
            0
        ));
        assert!(help_key_matches(
            gdk::Key::h,
            gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK,
            gdk::Key::H,
            gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK,
            0
        ));
    }

    #[test]
    fn chord_parser_rejects_unknown_keys() {
        let chord_map = keybindings::default_chord_map();
        assert_eq!(
            workspace_action_from_chord_key(gdk::Key::x, &chord_map),
            None
        );
        assert_eq!(
            workspace_action_from_chord_key(gdk::Key::Escape, &chord_map),
            None
        );
    }

    #[test]
    fn worktree_restore_prefers_working_tree_path_for_git_refresh() {
        let saved = session::SavedWorkspace {
            id: 1,
            work_origin: Some("workspace-restore-feature".into()),
            name: "feature".into(),
            collapsed: false,
            repo_root: Some("/repo/main".into()),
            is_worktree: true,
            working_tree_path: Some("/repo-feature".into()),
            branch_name: Some("main".into()),
            linked_issue: None,
            tabs: Vec::new(),
            active_tab_index: 0,
            tmux_backed: false,
            host_config_name: None,
        };

        assert_eq!(workspace_git_discovery_path(&saved), Some("/repo-feature"));
    }

    #[test]
    fn restore_prefers_working_tree_path_when_present_even_without_flag() {
        let saved = session::SavedWorkspace {
            id: 1,
            work_origin: Some("workspace-restore-repo".into()),
            name: "repo".into(),
            collapsed: false,
            repo_root: Some("/repo/main".into()),
            is_worktree: false,
            working_tree_path: Some("/repo-feature".into()),
            branch_name: Some("main".into()),
            linked_issue: None,
            tabs: Vec::new(),
            active_tab_index: 0,
            tmux_backed: false,
            host_config_name: None,
        };

        assert_eq!(workspace_git_discovery_path(&saved), Some("/repo-feature"));
    }

    #[test]
    fn apply_git_info_marks_linked_worktrees_on_workspace_state() {
        let git_info = git::GitInfo {
            repo_root: Some("/repo/main".into()),
            checkout_root: Some("/repo-feature".into()),
            branch: Some("feature/demo".into()),
            is_dirty: false,
            upstream: None,
        };
        let mut workspace = Workspace {
            id: 1,
            work_origin: crate::workspace::new_workspace_work_origin(),
            name: "repo".into(),
            collapsed: false,
            repo_root: None,
            is_worktree: false,
            working_tree_path: None,
            branch_name: None,
            linked_issue: None,
            env_vars: HashMap::new(),
            run_status: WorkspaceStatus::default(),
            tabs: Vec::new(),
            active_tab: 0,
            last_active_tab: None,
            tmux_backed: false,
            host_config_name: None,
            host_status: crate::probe::ProbeSnapshot::default(),
        };

        apply_git_info_to_workspace(&mut workspace, &git_info);

        assert_eq!(workspace.repo_root.as_deref(), Some("/repo/main"));
        assert_eq!(workspace.branch_name.as_deref(), Some("feature/demo"));
        assert!(workspace.is_worktree);
        assert_eq!(
            workspace.working_tree_path.as_deref(),
            Some("/repo-feature")
        );
    }

    #[test]
    fn workspace_name_from_git_info_prefers_checkout_root() {
        let git_info = git::GitInfo {
            repo_root: Some("/repo/main".into()),
            checkout_root: Some("/repo-feature".into()),
            branch: Some("feature/demo".into()),
            is_dirty: false,
            upstream: None,
        };

        assert_eq!(
            workspace_name_from_git_info(&git_info),
            Some("repo-feature".into())
        );
    }

    #[test]
    fn async_workspace_header_label_restores_name_and_git_status() {
        let git_info = git::GitInfo {
            repo_root: Some("/repo".into()),
            checkout_root: Some("/repo".into()),
            branch: Some("feature/async".into()),
            is_dirty: true,
            upstream: Some("origin/feature/async".into()),
        };

        assert_eq!(
            workspace_header_label("Workspace", &git_info),
            "Workspace  feature/async * -> origin/feature/async"
        );
        assert_eq!(
            workspace_header_label("Workspace", &git::GitInfo::default()),
            "Workspace"
        );
    }

    #[test]
    fn chord_timeout_resets_active_state() {
        let mut state = AppState::new();
        let generation = state.activate_chord(ChordMode::Leader);
        assert_eq!(state.chord_mode(), Some(ChordMode::Leader));
        assert!(state.chord_active());
        assert!(state.expire_chord(generation));
        assert!(!state.chord_active());
        assert_eq!(state.chord_mode(), None);
    }

    #[test]
    fn leader_parser_maps_configured_follow_up_keys() {
        let mut leader_map = BTreeMap::new();
        leader_map.insert('c', keybindings::Action::NewTab);
        leader_map.insert('p', keybindings::Action::CommandPalette);

        assert_eq!(
            leader_action_from_chord_key(gdk::Key::c, &leader_map),
            Some(keybindings::Action::NewTab)
        );
        assert_eq!(
            leader_action_from_chord_key(gdk::Key::p, &leader_map),
            Some(keybindings::Action::CommandPalette)
        );
        assert_eq!(leader_action_from_chord_key(gdk::Key::x, &leader_map), None);
    }

    #[test]
    fn broadcast_shortcut_toggles_tab_mode_on_and_off() {
        let mut state = AppState::new();
        assert_eq!(state.broadcast_scope(), None);
        assert_eq!(
            state.toggle_broadcast_shortcut_scope(),
            Some(BroadcastScope::Tab)
        );
        assert_eq!(state.broadcast_scope(), Some(BroadcastScope::Tab));
        assert_eq!(state.toggle_broadcast_shortcut_scope(), None);
        assert_eq!(state.broadcast_scope(), None);
    }

    #[test]
    fn terminal_search_regex_flags_enable_caseless_and_multiline() {
        let flags = terminal_search_regex_flags();
        assert_eq!(flags & PCRE2_CASELESS, PCRE2_CASELESS);
        assert_eq!(flags & PCRE2_MULTILINE, PCRE2_MULTILINE);
    }

    #[test]
    fn background_section_starts_collapsed() {
        let state = AppState::new();
        assert!(state.background_section_collapsed);
    }

    /// Create a dummy Tab for state-only tests (no GTK/VTE needed).
    fn dummy_tab(state: &mut AppState, name: &str) -> Tab {
        let id = state.next_id;
        state.next_id += 1;
        let pane_id = state.next_id;
        state.next_id += 1;
        Tab {
            id,
            name: name.to_string(),
            work_origin: crate::workspace::new_tab_work_origin(),
            kind: workspace::TabKind::Terminal,
            panes: Box::new(pane::PaneNode::Stub { pane_id }),
            focused_pane_id: pane_id,
            next_pane_id: pane_id + 1,
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

    fn add_tab_to_workspace(state: &mut AppState, ws_id: u32, name: &str) -> u32 {
        let tab = dummy_tab(state, name);
        let tab_id = tab.id;
        let workspace = state
            .workspaces
            .iter_mut()
            .find(|ws| ws.id == ws_id)
            .expect("workspace should exist");
        workspace.tabs.push(tab);
        if workspace.active_tab == 0 {
            workspace.active_tab = tab_id;
        }
        tab_id
    }

    #[test]
    fn previous_tab_toggles_within_workspace() {
        let mut state = AppState::new();
        let ws_id = state.workspaces[0].id;
        let tab1 = add_tab_to_workspace(&mut state, ws_id, "Shell 1");
        let tab2 = add_tab_to_workspace(&mut state, ws_id, "Shell 2");
        state.workspaces[0].active_tab = tab1;
        state.reset_navigation_history();

        assert_eq!(state.activate_tab(tab2), Some(ws_id));
        let workspace = state.workspaces.iter().find(|ws| ws.id == ws_id).unwrap();
        assert_eq!(workspace.active_tab, tab2);
        assert_eq!(workspace.last_active_tab, Some(tab1));

        assert_eq!(state.activate_previous_tab(), Some(tab1));
        let workspace = state.workspaces.iter().find(|ws| ws.id == ws_id).unwrap();
        assert_eq!(workspace.active_tab, tab1);
        assert_eq!(workspace.last_active_tab, Some(tab2));
    }

    #[test]
    fn create_dashboard_tab_reuses_existing_dashboard_in_workspace() {
        let mut state = AppState::new();
        let ws_id = state.workspaces[0].id;
        let shell_tab = add_tab_to_workspace(&mut state, ws_id, "Shell 1");
        state.workspaces[0].active_tab = shell_tab;

        let dashboard_tab = state.create_dashboard_tab().expect("dashboard tab");
        assert_eq!(state.workspaces[0].active_tab, shell_tab);
        assert_eq!(state.presented_tab_id(), Some(dashboard_tab));
        assert_eq!(state.workspaces[0].last_active_tab, None);
        assert_eq!(
            state.activate_tab(dashboard_tab),
            None,
            "presentation-only tabs cannot become pane-command targets"
        );
        assert_eq!(
            state.workspaces[0]
                .tabs
                .iter()
                .filter(|tab| tab.kind == TabKind::Dashboard)
                .count(),
            1
        );

        let second_attempt = state.create_dashboard_tab();
        assert_eq!(second_attempt, None);
        assert_eq!(state.workspaces[0].active_tab, shell_tab);
        assert_eq!(state.presented_tab_id(), Some(dashboard_tab));
        assert_eq!(state.workspaces[0].last_active_tab, None);
        assert_eq!(
            state.workspaces[0]
                .tabs
                .iter()
                .filter(|tab| tab.kind == TabKind::Dashboard)
                .count(),
            1
        );
    }

    #[test]
    fn previous_workspace_toggles_between_workspaces() {
        let mut state = AppState::new();
        let ws_a = state.workspaces[0].id;
        let _tab_a = add_tab_to_workspace(&mut state, ws_a, "Shell 1");
        let ws_b = state.create_workspace("target", None);
        let _tab_b = add_tab_to_workspace(&mut state, ws_b, "Shell 2");
        state.reset_navigation_history();

        assert_eq!(state.activate_workspace(ws_a), Some(ws_a));
        assert_eq!(state.activate_workspace(ws_b), Some(ws_b));
        assert_eq!(state.activate_previous_workspace(), Some(ws_a));
        assert_eq!(state.active_workspace, ws_a);
        assert_eq!(state.activate_previous_workspace(), Some(ws_b));
        assert_eq!(state.active_workspace, ws_b);
    }

    #[test]
    fn workspace_relative_navigation_wraps_and_keeps_order() {
        let mut state = AppState::new();
        let ws_a = state.workspaces[0].id;
        let ws_b = state.create_workspace("two", None);
        let ws_c = state.create_workspace("three", None);
        state.reset_navigation_history();

        assert_eq!(state.activate_workspace(ws_a), Some(ws_a));
        assert_eq!(state.activate_workspace_relative(-1), Some(ws_c));
        assert_eq!(state.active_workspace, ws_c);
        assert_eq!(state.activate_workspace_relative(1), Some(ws_a));
        assert_eq!(state.active_workspace, ws_a);
        assert_eq!(
            state.workspaces.iter().map(|ws| ws.id).collect::<Vec<_>>(),
            vec![ws_a, ws_b, ws_c]
        );
    }

    #[test]
    fn move_tab_updates_workspace_history_safely() {
        let mut state = AppState::new();
        let ws_a = state.workspaces[0].id;
        let tab1 = add_tab_to_workspace(&mut state, ws_a, "Shell 1");
        let tab2 = add_tab_to_workspace(&mut state, ws_a, "Shell 2");
        state.workspaces[0].active_tab = tab2;
        state.workspaces[0].last_active_tab = Some(tab1);

        let ws_b = state.create_workspace("target", None);
        let tab3 = add_tab_to_workspace(&mut state, ws_b, "Shell 3");
        let target = state
            .workspaces
            .iter_mut()
            .find(|ws| ws.id == ws_b)
            .unwrap();
        target.active_tab = tab3;
        target.last_active_tab = None;
        state.reset_navigation_history();
        state.active_workspace = ws_a;

        assert!(state.move_tab_to_workspace(tab1, ws_b).is_ok());

        let source = state.workspaces.iter().find(|ws| ws.id == ws_a).unwrap();
        assert_eq!(source.last_active_tab, None);
        let target = state.workspaces.iter().find(|ws| ws.id == ws_b).unwrap();
        assert_eq!(target.active_tab, tab1);
        assert_eq!(target.last_active_tab, Some(tab3));
        assert_eq!(state.active_workspace, ws_b);
        assert_eq!(state.last_active_workspace(), Some(ws_a));
    }

    #[test]
    fn move_tab_to_different_workspace() {
        let mut state = AppState::new();
        let ws_a = state.workspaces[0].id; // default workspace from new()
        let ws_b = state.create_workspace("target", None);

        let tab = dummy_tab(&mut state, "Shell 1");
        let tab_id = tab.id;
        state.workspaces[0].tabs.push(tab);
        state.workspaces[0].active_tab = tab_id;

        assert!(state.move_tab_to_workspace(tab_id, ws_b).is_ok());

        // Tab moved to target workspace
        assert!(state
            .workspaces
            .iter()
            .find(|ws| ws.id == ws_a)
            .unwrap()
            .tabs
            .is_empty());
        assert_eq!(
            state
                .workspaces
                .iter()
                .find(|ws| ws.id == ws_b)
                .unwrap()
                .tabs
                .len(),
            1
        );
        assert_eq!(
            state
                .workspaces
                .iter()
                .find(|ws| ws.id == ws_b)
                .unwrap()
                .active_tab,
            tab_id
        );
        // Active workspace switched to target
        assert_eq!(state.active_workspace, ws_b);
        // Source workspace still exists (not deleted)
        assert!(state.workspaces.iter().any(|ws| ws.id == ws_a));
    }

    #[test]
    fn move_tab_fixes_source_active_tab() {
        let mut state = AppState::new();
        let ws_a = state.workspaces[0].id;
        let ws_b = state.create_workspace("target", None);

        let tab1 = dummy_tab(&mut state, "Shell 1");
        let tab1_id = tab1.id;
        let tab2 = dummy_tab(&mut state, "Shell 2");
        let tab2_id = tab2.id;
        state.workspaces[0].tabs.push(tab1);
        state.workspaces[0].tabs.push(tab2);
        state.workspaces[0].active_tab = tab1_id;

        // Move the active tab away
        assert!(state.move_tab_to_workspace(tab1_id, ws_b).is_ok());

        // Source workspace's active_tab falls back to remaining tab
        let source = state.workspaces.iter().find(|ws| ws.id == ws_a).unwrap();
        assert_eq!(source.active_tab, tab2_id);
    }

    #[test]
    fn move_tab_rejects_same_workspace() {
        let mut state = AppState::new();
        let ws_a = state.workspaces[0].id;

        let tab = dummy_tab(&mut state, "Shell 1");
        let tab_id = tab.id;
        state.workspaces[0].tabs.push(tab);
        state.workspaces[0].active_tab = tab_id;

        let result = state.move_tab_to_workspace(tab_id, ws_a);
        assert!(matches!(result, Err(MoveTabError::AlreadyInWorkspace)));
    }

    #[test]
    fn move_tab_empty_source_has_zero_active_tab() {
        let mut state = AppState::new();
        let ws_a = state.workspaces[0].id;
        let ws_b = state.create_workspace("target", None);

        let tab = dummy_tab(&mut state, "Shell 1");
        let tab_id = tab.id;
        state.workspaces[0].tabs.push(tab);
        state.workspaces[0].active_tab = tab_id;

        assert!(state.move_tab_to_workspace(tab_id, ws_b).is_ok());

        let source = state.workspaces.iter().find(|ws| ws.id == ws_a).unwrap();
        assert!(source.tabs.is_empty());
        assert_eq!(source.active_tab, 0);
    }

    #[test]
    fn move_tab_uncollapses_target() {
        let mut state = AppState::new();
        let ws_b = state.create_workspace("target", None);
        state
            .workspaces
            .iter_mut()
            .find(|ws| ws.id == ws_b)
            .unwrap()
            .collapsed = true;

        let tab = dummy_tab(&mut state, "Shell 1");
        let tab_id = tab.id;
        state.workspaces[0].tabs.push(tab);
        state.workspaces[0].active_tab = tab_id;

        assert!(state.move_tab_to_workspace(tab_id, ws_b).is_ok());

        let target = state.workspaces.iter().find(|ws| ws.id == ws_b).unwrap();
        assert!(!target.collapsed);
    }
}
