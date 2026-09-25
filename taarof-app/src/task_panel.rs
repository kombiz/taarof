use gtk::prelude::*;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::TaskPanelDefaultView;
use crate::tracking::{
    branch_pull_requests_command, classify_remote_git_identity, classify_remote_tasks_fetch,
    remote_git_identity_command, remote_tasks_fetch_command, BranchPullRequestsData,
    ParallelizationPlan, PlanTaskEntry, PlanTaskProjection, PlanTasksData, RemoteGitIdentityError,
    RemoteGitIdentityOutcome, RemoteTaskTarget, RemoteTasksError, RemoteTasksOutcome,
};
use crate::workspace::WorkspaceTaskButton;
use crate::AppState;

const TASK_PANEL_WIDTH: i32 = 260;
const TASK_PANEL_POLL_SECONDS: u32 = 1;
/// How long a remote-fetched `.plan/tasks.json` snapshot is reused before the
/// next SSH read, so the 1s poll loop does not spawn an SSH process every tick.
const REMOTE_TASKS_TTL_MS: u64 = 5_000;
/// Ceiling for the exponential backoff applied to failed remote fetches. A
/// host that rejects SSH settles at one attempt every two minutes instead of
/// one every TTL for the pane's lifetime. Successful and neutral no-source
/// outcomes keep the base TTL so a newly bootstrapped plan is noticed quickly.
const REMOTE_TASKS_FAILURE_TTL_CAP_MS: u64 = 120_000;
/// `gh pr list` can hit the network, so keep branch PR snapshots a little
/// longer than the task file poll interval while still making manual refreshes useful.
const PULL_REQUESTS_TTL_MS: u64 = 15_000;

/// Cache key for a remote task fetch: the exact bounded connection argv plus cwd.
/// This keeps identities, ports, jump hosts, and similarly named hosts isolated.
type RemoteKey = (Vec<String>, String);
/// Exact identity of one branch PR query. Repository identities are normalized
/// because GitHub treats owner/repository names case-insensitively.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PullRequestKey {
    checkout_root: PathBuf,
    remote_connection: Option<Vec<String>>,
    base_repository: String,
    head_owner: String,
    branch: String,
}

impl PullRequestKey {
    fn new(checkout_root: PathBuf, base_repository: &str, head_owner: &str, branch: &str) -> Self {
        Self {
            checkout_root,
            remote_connection: None,
            base_repository: base_repository.to_ascii_lowercase(),
            head_owner: head_owner.to_ascii_lowercase(),
            branch: branch.to_string(),
        }
    }

    fn for_target(target: &PullRequestTarget) -> Self {
        let mut key = Self::new(
            target.checkout_root.clone(),
            &target.base_repository,
            &target.head_owner,
            &target.branch,
        );
        key.remote_connection = target.remote_connection.clone();
        key
    }
}

/// A cached remote `.plan/tasks.json` fetch result with its capture time.
struct RemoteTasksEntry {
    fetched_at_ms: u64,
    outcome: RemoteTasksOutcome,
    /// Consecutive failed fetches for this key; drives exponential backoff.
    failure_streak: u32,
}

/// TTL before the next remote fetch attempt: the base TTL after a success,
/// doubling per consecutive failure up to the cap.
fn remote_tasks_ttl_ms(failure_streak: u32) -> u64 {
    REMOTE_TASKS_TTL_MS
        .saturating_mul(1u64 << failure_streak.min(16))
        .min(REMOTE_TASKS_FAILURE_TTL_CAP_MS.max(REMOTE_TASKS_TTL_MS))
}

fn next_remote_failure_streak(previous: u32, outcome: &RemoteTasksOutcome) -> u32 {
    if matches!(outcome, RemoteTasksOutcome::Error(_)) {
        previous.saturating_add(1)
    } else {
        0
    }
}

fn remote_cache_key(target: &RemoteTaskTarget) -> RemoteKey {
    let mut connection =
        remote_tasks_fetch_command(target.ssh_argv.as_deref(), &target.host, &target.cwd);
    let _ = connection.pop();
    (connection, target.cwd.clone())
}

/// A cached `gh pr list` result for the active local branch.
struct PullRequestsEntry {
    fetched_at_ms: u64,
    verified_at_ms: Option<u64>,
    verified: Option<BranchPullRequestsData>,
    error: Option<String>,
}

struct RemotePullRequestIdentityEntry {
    fetched_at_ms: u64,
    outcome: RemoteGitIdentityOutcome,
    failure_streak: u32,
}

fn remote_pull_request_identity_ttl_ms(failure_streak: u32) -> u64 {
    PULL_REQUESTS_TTL_MS
        .saturating_mul(1u64 << failure_streak.min(16))
        .min(REMOTE_TASKS_FAILURE_TTL_CAP_MS.max(PULL_REQUESTS_TTL_MS))
}

fn next_remote_git_failure_streak(previous: u32, outcome: &RemoteGitIdentityOutcome) -> u32 {
    if matches!(outcome, RemoteGitIdentityOutcome::Error(_)) {
        previous.saturating_add(1)
    } else {
        0
    }
}

fn record_pull_request_fetch(
    cache: &mut HashMap<PullRequestKey, PullRequestsEntry>,
    key: PullRequestKey,
    result: Result<BranchPullRequestsData, String>,
    fetched_at_ms: u64,
) {
    let previous = cache.remove(&key);
    let entry = match result {
        Ok(data) => PullRequestsEntry {
            fetched_at_ms,
            verified_at_ms: Some(fetched_at_ms),
            verified: Some(data),
            error: None,
        },
        Err(error) => PullRequestsEntry {
            fetched_at_ms,
            verified_at_ms: previous.as_ref().and_then(|entry| entry.verified_at_ms),
            verified: previous.and_then(|entry| entry.verified),
            error: Some(error),
        },
    };
    cache.insert(key, entry);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PullRequestFetchReason {
    Passive,
    ExplicitDiscovery,
}

/// Register one asynchronous refresh. An explicit request that arrives while
/// an older request is running queues one follow-up instead of treating the
/// older result as if it were captured after the operator's action.
fn register_pull_request_fetch<K: Clone + Eq + Hash>(
    inflight: &mut HashSet<K>,
    refresh_after_inflight: &mut HashSet<K>,
    key: &K,
    reason: PullRequestFetchReason,
) -> bool {
    if inflight.insert(key.clone()) {
        return true;
    }
    if reason == PullRequestFetchReason::ExplicitDiscovery {
        refresh_after_inflight.insert(key.clone());
    }
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PullRequestDiscoveryFeedback {
    Success(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskPanelMode {
    Session,
    Agents,
    Tasks,
    PullRequests,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskPanelIntent {
    Select(TaskPanelMode),
    DiscoverTasks,
}

#[derive(Debug, Clone)]
struct TaskPanelModeSelection {
    tasks_enabled: bool,
    pull_request_mode_enabled: bool,
    default_mode: TaskPanelMode,
    selected_modes: HashMap<String, TaskPanelMode>,
    global_mode: Option<TaskPanelMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TaskPanelRuntimePolicy {
    load_task_sources: bool,
    install_task_poll: bool,
}

fn task_panel_runtime_policy(tasks_enabled: bool) -> TaskPanelRuntimePolicy {
    TaskPanelRuntimePolicy {
        load_task_sources: tasks_enabled,
        install_task_poll: tasks_enabled,
    }
}

impl TaskPanelModeSelection {
    fn new(
        tasks_enabled: bool,
        pull_request_mode_enabled: bool,
        default_mode: TaskPanelMode,
    ) -> Self {
        Self {
            tasks_enabled,
            pull_request_mode_enabled,
            default_mode,
            selected_modes: HashMap::new(),
            global_mode: (!tasks_enabled).then_some(TaskPanelMode::Session),
        }
    }

    fn mode_for_context(&self, mode_key: &str) -> TaskPanelMode {
        if let Some(mode) = self.global_mode {
            return mode;
        }

        self.task_mode_for_context(mode_key)
    }

    fn task_mode_for_context(&self, mode_key: &str) -> TaskPanelMode {
        if !self.pull_request_mode_enabled {
            return TaskPanelMode::Tasks;
        }

        self.selected_modes
            .get(mode_key)
            .copied()
            .unwrap_or(self.default_mode)
    }

    fn apply_intent(&mut self, mode_key: &str, intent: TaskPanelIntent) {
        if let TaskPanelIntent::Select(mode @ (TaskPanelMode::Session | TaskPanelMode::Agents)) =
            intent
        {
            self.global_mode = Some(mode);
            return;
        }
        if !self.tasks_enabled {
            return;
        }

        let mode = match intent {
            TaskPanelIntent::Select(TaskPanelMode::Tasks) => TaskPanelMode::Tasks,
            TaskPanelIntent::Select(TaskPanelMode::PullRequests)
                if self.pull_request_mode_enabled =>
            {
                TaskPanelMode::PullRequests
            }
            TaskPanelIntent::Select(_) => return,
            TaskPanelIntent::DiscoverTasks => TaskPanelMode::Tasks,
        };
        self.global_mode = None;
        self.selected_modes.insert(mode_key.to_string(), mode);
    }
}

impl From<TaskPanelDefaultView> for TaskPanelMode {
    fn from(value: TaskPanelDefaultView) -> Self {
        match value {
            TaskPanelDefaultView::Tasks => Self::Tasks,
            TaskPanelDefaultView::PullRequests => Self::PullRequests,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PullRequestTarget {
    checkout_root: PathBuf,
    branch: String,
    head_repository: String,
    base_repository: String,
    head_owner: String,
    remote_connection: Option<Vec<String>>,
    pane_identity: crate::work_ledger::WorkIdentity,
    bound_task: Option<PullRequestBoundTask>,
    /// Computed by the immutable local probe. Remote PRs have no local loop
    /// runner surface and therefore carry `None`.
    loop_runner_availability: Option<LoopRunnerAvailability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PullRequestBoundTask {
    task_id: String,
    title: String,
    checkout_root: String,
    reporting_token: String,
}

/// GTK-side identity for an asynchronous local Git/plan probe. It deliberately
/// carries the same workspace/tab/candidate/report-token context as the task
/// snapshot; pane origin plus cwd alone lets a restored or moved task panel
/// coalesce with a semantically different request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LocalPullRequestProbeKey {
    workspace_origin: String,
    tab_origin: String,
    pane_origin: String,
    cwd: String,
    candidates: Vec<String>,
    reporting_token: Option<String>,
}

#[derive(Clone)]
struct LocalPullRequestProbeRequest {
    key: LocalPullRequestProbeKey,
    binding: Option<crate::task_binding::PaneTaskBinding>,
    pane_identity: crate::work_ledger::WorkIdentity,
}

#[derive(Clone)]
struct LocalPullRequestProbeResult {
    checkout_root: PathBuf,
    branch: String,
    repositories: crate::git::GitHubRepositoryIdentities,
    bound_task: Option<PullRequestBoundTask>,
    loop_runner_availability: Option<LoopRunnerAvailability>,
}

const LOCAL_PULL_REQUEST_CACHE_CAPACITY: usize = 16;
// Branch/ref state is discovered on the bounded worker, not GTK. Keep the
// identity short-lived so a branch switch in the same checkout cannot retain
// an earlier branch's PR target indefinitely.
const LOCAL_PULL_REQUEST_TTL_MS: u64 = 1_000;

#[derive(Clone)]
struct LocalPullRequestCacheEntry {
    fetched_at_ms: u64,
    result: Option<LocalPullRequestProbeResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LocalTaskProbeKey {
    workspace_origin: String,
    tab_origin: String,
    pane_origin: String,
    tab_id: u32,
    pane_id: u32,
    candidates: Vec<String>,
    cwd: Option<String>,
    cwd_host: Option<String>,
    remote_shell: bool,
    binding: Option<crate::task_binding::PaneTaskBinding>,
}

#[derive(Clone)]
struct LocalTaskProbeRequest {
    key: LocalTaskProbeKey,
    tab_id: u32,
    pane_id: u32,
    cwd: Option<String>,
    cwd_host: Option<String>,
    remote_shell: bool,
    binding: Option<crate::task_binding::PaneTaskBinding>,
}

#[derive(Clone)]
struct LocalTaskProbeResult {
    plan_root: Option<PathBuf>,
    projection: Option<PlanTaskProjection>,
    /// Exact task markdown paths observed by the filesystem worker. GTK row
    /// rendering consults this immutable set and never calls `Path::exists`.
    markdown_paths: HashSet<PathBuf>,
    load_error: Option<String>,
    binding_context: Option<TaskBindingContext>,
    current_task: Option<CurrentTaskSummary>,
    availability: Option<LoopRunnerAvailability>,
}

const LOCAL_TASK_CACHE_CAPACITY: usize = 16;
const LOCAL_TASK_TTL_MS: u64 = 1_000;

struct LocalTaskCacheEntry {
    fetched_at_ms: u64,
    result: LocalTaskProbeResult,
}

struct TaskPanelViewContext {
    mode: TaskPanelMode,
    show_task_modes: bool,
    show_pull_requests: bool,
    remote_target: Option<RemoteTaskTarget>,
    pull_request_target: Option<PullRequestTarget>,
}

use crate::agent_projection::{project_all_agent_panes, AgentPaneProjection};

fn build_agent_activity_row(
    card: &AgentPaneProjection,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);

    let button = gtk::Button::new();
    button.add_css_class("agent-activity-card");
    button.add_css_class(card.state.css_class());
    button.set_tooltip_text(Some("Focus this agent's exact tab and pane"));

    let content = gtk::Box::new(gtk::Orientation::Vertical, 5);
    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 7);

    let badge = gtk::Label::new(Some(&card.badge.short_label));
    badge.add_css_class("tab-agent-badge");
    badge.add_css_class(&format!("agent-badge-{}", card.badge.color_token));
    heading.append(&badge);

    let title = gtk::Label::new(Some(&card.title));
    title.add_css_class("agent-activity-title");
    title.set_halign(gtk::Align::Start);
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    heading.append(&title);

    let status = gtk::Label::new(Some(card.state.ui_label()));
    status.add_css_class("agent-activity-status");
    status.add_css_class(card.state.css_class());
    heading.append(&status);
    content.append(&heading);

    let identity = gtk::Label::new(Some(&format!(
        "{} · {} · pane {}",
        card.agent_label, card.workspace_name, card.pane_id
    )));
    identity.add_css_class("agent-activity-identity");
    identity.set_halign(gtk::Align::Start);
    identity.set_ellipsize(gtk::pango::EllipsizeMode::End);
    content.append(&identity);

    let context = gtk::Label::new(Some(&card.context));
    context.add_css_class("agent-activity-context");
    context.set_halign(gtk::Align::Start);
    context.set_wrap(true);
    content.append(&context);

    if !card.activity.is_empty() {
        let activity = gtk::Label::new(Some(&card.activity));
        activity.add_css_class("agent-activity-detail");
        activity.set_halign(gtk::Align::Start);
        activity.set_wrap(true);
        content.append(&activity);
    }
    if !card.children.is_empty() {
        let children = gtk::Box::new(gtk::Orientation::Vertical, 3);
        children.add_css_class("agent-headless-children");
        for child in &card.children {
            let child_row = gtk::Box::new(gtk::Orientation::Horizontal, 5);
            child_row.add_css_class("agent-headless-child");
            child_row.add_css_class(child.state.css_class());
            let marker = gtk::Label::new(Some("↳"));
            marker.add_css_class("agent-headless-marker");
            child_row.append(&marker);
            let label = gtk::Label::new(Some(&child.label));
            label.add_css_class("agent-headless-label");
            label.set_halign(gtk::Align::Start);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_tooltip_text(Some(&format!(
                "Headless {} subagent · inherits pane {}",
                child.provider, child.pane_id
            )));
            child_row.append(&label);
            let child_state = gtk::Label::new(Some(child.state.ui_label()));
            child_state.add_css_class("agent-headless-state");
            child_state.add_css_class(child.state.css_class());
            child_row.append(&child_state);
            children.append(&child_row);
        }
        content.append(&children);
    }
    button.set_child(Some(&content));

    let state = state.clone();
    let tab_list = tab_list.clone();
    let term_stack = term_stack.clone();
    let tab_id = card.tab_id;
    let pane_id = card.pane_id;
    button.connect_clicked(move |_| {
        if !crate::sidebar::focus_agent_pane(&state, &tab_list, &term_stack, tab_id, pane_id) {
            crate::show_error_toast("That agent pane is no longer available");
        }
    });
    row.set_child(Some(&button));
    row
}

#[derive(Clone)]
pub struct TaskPanel {
    pub container: gtk::Box,
    body: gtk::Box,
    session_content: gtk::Box,
    agents_content: gtk::Box,
    agents_summary: gtk::Label,
    agents_list: gtk::ListBox,
    agents_empty: gtk::Label,
    task_content: gtk::Box,
    revealer: gtk::Revealer,
    title: gtk::Label,
    chevron: gtk::Label,
    subtitle: gtk::Label,
    progress: gtk::Label,
    summary: gtk::Label,
    current_task_card: gtk::Box,
    current_task_label: gtk::Label,
    current_task_status: gtk::Label,
    current_task_pr: gtk::Button,
    current_task_clear: gtk::Button,
    list: gtk::ListBox,
    actions: gtk::FlowBox,
    empty: gtk::Label,
    mode_switch: gtk::Box,
    session_button: gtk::ToggleButton,
    agents_button: gtk::ToggleButton,
    tasks_button: gtk::ToggleButton,
    pull_requests_button: gtk::ToggleButton,
    expanded: Rc<Cell<bool>>,
    tasks_enabled: bool,
    pull_request_mode_enabled: bool,
    mode_selection: Rc<RefCell<TaskPanelModeSelection>>,
    syncing_mode_switch: Rc<Cell<bool>>,
    last_snapshot: Rc<RefCell<Option<TaskPanelSnapshot>>>,
    last_agent_cards: Rc<RefCell<Option<Vec<AgentPaneProjection>>>>,
    /// Per-(host, cwd) cache of remote task fetches (EXAMPLE-55).
    remote_cache: Rc<RefCell<HashMap<RemoteKey, RemoteTasksEntry>>>,
    /// Local `.plan` snapshots are loaded by the bounded Git/filesystem
    /// worker; GTK renders only this immutable cache.
    local_task_cache: Rc<RefCell<HashMap<LocalTaskProbeKey, LocalTaskCacheEntry>>>,
    local_task_inflight: Rc<RefCell<HashSet<LocalTaskProbeKey>>>,
    /// Local Git/config identity probes.  The cache is intentionally bounded:
    /// changing directories across many repositories must not become an
    /// unbounded collection of stale paths in a long-lived GTK process.
    local_pull_request_cache:
        Rc<RefCell<HashMap<LocalPullRequestProbeKey, LocalPullRequestCacheEntry>>>,
    local_pull_request_inflight: Rc<RefCell<HashSet<LocalPullRequestProbeKey>>>,
    /// Keys with an SSH fetch currently in flight, to avoid duplicate spawns.
    remote_inflight: Rc<RefCell<HashSet<RemoteKey>>>,
    /// Remote Git identity probes, keyed by the exact connection and pane cwd.
    remote_pull_request_cache: Rc<RefCell<HashMap<RemoteKey, RemotePullRequestIdentityEntry>>>,
    /// Remote Git identity probes currently in flight.
    remote_pull_request_inflight: Rc<RefCell<HashSet<RemoteKey>>>,
    /// Explicit identity refreshes that arrived behind an older in-flight probe.
    remote_pull_request_refresh_after_inflight: Rc<RefCell<HashSet<RemoteKey>>>,
    /// Per-(checkout root, branch) cache of `gh pr list` results.
    pull_request_cache: Rc<RefCell<HashMap<PullRequestKey, PullRequestsEntry>>>,
    /// Keys with a GitHub PR fetch currently in flight.
    pull_request_inflight: Rc<RefCell<HashSet<PullRequestKey>>>,
    /// Explicit GitHub refreshes that arrived behind an older in-flight query.
    pull_request_refresh_after_inflight: Rc<RefCell<HashSet<PullRequestKey>>>,
    /// Explicit discovery requests waiting for the current fetch result. A set
    /// lets repeated clicks join one in-flight query and receive one outcome.
    pull_request_feedback_pending: Rc<RefCell<HashSet<PullRequestKey>>>,
    /// Explicit remote discoveries waiting for identity resolution before the
    /// local `gh` refresh can begin.
    remote_pull_request_feedback_pending: Rc<RefCell<HashSet<RemoteKey>>>,
}

struct ExplicitDiscoveryController {
    panel: TaskPanel,
    term_stack: gtk::Stack,
    window: adw::ApplicationWindow,
}

thread_local! {
    static EXPLICIT_DISCOVERY_CONTROLLER: RefCell<Option<ExplicitDiscoveryController>> =
        const { RefCell::new(None) };
}

fn install_explicit_discovery_controller(controller: ExplicitDiscoveryController) {
    EXPLICIT_DISCOVERY_CONTROLLER.with(|slot| {
        *slot.borrow_mut() = Some(controller);
    });
}

fn select_tasks_for_explicit_discovery(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    tab_id: u32,
) {
    EXPLICIT_DISCOVERY_CONTROLLER.with(|slot| {
        let controller = slot.borrow();
        let Some(controller) = controller.as_ref() else {
            return;
        };
        controller.panel.select_mode_for_active_context(
            state,
            tab_list,
            &controller.term_stack,
            &controller.window,
            TaskPanelIntent::DiscoverTasks,
        );
        controller
            .panel
            .refresh_pull_requests_for_explicit_discovery(
                state,
                tab_list,
                tab_id,
                &controller.term_stack,
                &controller.window,
            );
    });
}

/// Run explicit task discovery through the one path that also moves the task
/// panel back to Tasks mode. Background discovery continues to call
/// `sidebar::discover_for_tab(..., false)` directly and does not change the
/// user's selected panel mode.
pub(crate) fn discover_tasks_explicitly(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    tab_id: u32,
) {
    crate::sidebar::discover_for_tab(state, tab_list, tab_id, true);
    select_tasks_for_explicit_discovery(state, tab_list, tab_id);
}

pub(crate) fn select_pull_requests_for_active_context(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
) {
    EXPLICIT_DISCOVERY_CONTROLLER.with(|slot| {
        let controller = slot.borrow();
        let Some(controller) = controller.as_ref() else {
            return;
        };
        controller.panel.select_mode_for_active_context(
            state,
            tab_list,
            &controller.term_stack,
            &controller.window,
            TaskPanelIntent::Select(TaskPanelMode::PullRequests),
        );
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskPanelSnapshot {
    subtitle: String,
    progress: String,
    summary: String,
    /// Dependency-aware "Ready now — N can run in parallel" line, shown above the
    /// task rows. `None` when there is nothing to schedule (PR view, remote, etc).
    parallelization_header: Option<String>,
    tasks: Vec<TaskRow>,
    action_labels: Vec<String>,
    current_task: Option<CurrentTaskSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CurrentTaskSummary {
    tab_id: u32,
    pane_id: u32,
    id: String,
    title: String,
    resolved: bool,
    pull_request: Option<CurrentTaskPullRequest>,
    truth: Option<crate::work_ledger::TaskTruth>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CurrentTaskPullRequest {
    label: String,
    url: Option<String>,
    ambiguous: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskRow {
    id: String,
    title: String,
    status: String,
    priority: Option<String>,
    note: Option<String>,
    is_next: bool,
    markdown_path: Option<PathBuf>,
    external_url: Option<String>,
    /// Loop-runner dispatch context for this row, when the feature is enabled and
    /// applicable. Drives the per-row "Build with loop runner" / PR review buttons.
    loop_runner: Option<LoopRunnerRowInfo>,
    binding: Option<TaskBindingRowInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskBindingRowInfo {
    tab_id: u32,
    pane_id: u32,
    is_current: bool,
    truth: Option<crate::work_ledger::TaskTruth>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskBindingContext {
    tab_id: u32,
    pane_id: u32,
    current_task_id: Option<String>,
}

/// Per-row loop-runner state. For a `.plan` task row this carries readiness and
/// unmet blockers; for a PR row it carries the PR number and open-ness. Either
/// way `checkout_root` is the `--repo` the runner is pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LoopRunnerRowInfo {
    checkout_root: PathBuf,
    kind: LoopRunnerRowKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LoopRunnerRowKind {
    /// A local `.plan` task. `ready` gates the build button; `blockers` names the
    /// unmet dependencies when it is not ready.
    Task { ready: bool, blockers: Vec<String> },
    /// An open PR that can be reviewed/merged.
    PullRequest { number: u64, open: bool },
}

/// Snapshot-wide loop-runner availability, computed once per refresh. The buttons
/// only appear when the feature is enabled AND the active checkout has a
/// `.loop/loops.yaml`; this struct caches that decision plus the checkout root.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LoopRunnerAvailability {
    checkout_root: PathBuf,
}

/// Decide whether loop-runner buttons should be offered for `checkout_root`.
/// Returns `None` (no buttons) when the feature is disabled or the repo has no
/// `.loop/loops.yaml`, so the panel degrades gracefully in repos without it.
fn loop_runner_availability(checkout_root: Option<&Path>) -> Option<LoopRunnerAvailability> {
    let config = crate::config::loop_runner_config();
    if !config.enabled {
        return None;
    }
    let root = checkout_root?;
    if !root.join(".loop/loops.yaml").exists() {
        return None;
    }
    Some(LoopRunnerAvailability {
        checkout_root: root.to_path_buf(),
    })
}

pub fn build_task_panel(
    state: Rc<RefCell<AppState>>,
    tab_list: gtk::Box,
    term_stack: gtk::Stack,
    window: adw::ApplicationWindow,
) -> TaskPanel {
    let tasks_config = crate::config::tasks_config();
    let runtime_policy = task_panel_runtime_policy(tasks_config.enabled);
    let default_mode = TaskPanelMode::from(tasks_config.default_view);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 8);
    container.add_css_class("task-panel");
    container.set_width_request(TASK_PANEL_WIDTH + 24);
    container.set_hexpand(false);
    container.set_margin_top(8);
    container.set_margin_bottom(8);
    container.set_margin_start(8);
    container.set_margin_end(8);
    container.set_vexpand(true);

    let header_button = gtk::Button::new();
    header_button.add_css_class("task-panel-toggle");
    header_button.set_halign(gtk::Align::Fill);
    header_button.set_width_request(TASK_PANEL_WIDTH);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let title_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    title_box.set_hexpand(true);

    let title = gtk::Label::new(Some(if tasks_config.enabled {
        "Tasks"
    } else {
        "Session"
    }));
    title.add_css_class("task-panel-title");
    title.set_halign(gtk::Align::Start);

    let subtitle = gtk::Label::new(Some("No .plan/tasks.json"));
    subtitle.add_css_class("task-panel-subtitle");
    subtitle.set_halign(gtk::Align::Start);
    subtitle.set_wrap(true);

    title_box.append(&title);
    title_box.append(&subtitle);

    let chevron = gtk::Label::new(Some("▾"));
    chevron.add_css_class("task-panel-chevron");

    header.append(&title_box);
    header.append(&chevron);
    header_button.set_child(Some(&header));
    container.append(&header_button);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    body.add_css_class("task-panel-body");
    body.set_width_request(TASK_PANEL_WIDTH);
    body.set_hexpand(false);
    body.set_vexpand(true);

    let mode_switch = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    mode_switch.add_css_class("task-panel-mode-switch");
    mode_switch.set_visible(true);

    let session_button = gtk::ToggleButton::with_label("Session");
    session_button.add_css_class("task-panel-mode-button");
    let agents_button = gtk::ToggleButton::with_label("Agents");
    agents_button.add_css_class("task-panel-mode-button");
    agents_button.set_group(Some(&session_button));
    let tasks_button = gtk::ToggleButton::with_label("Tasks");
    tasks_button.add_css_class("task-panel-mode-button");
    tasks_button.set_group(Some(&agents_button));
    tasks_button.set_visible(tasks_config.enabled);
    let pull_requests_button = gtk::ToggleButton::with_label("PRs");
    pull_requests_button.add_css_class("task-panel-mode-button");
    pull_requests_button.set_group(Some(&tasks_button));
    pull_requests_button.set_visible(tasks_config.enabled && tasks_config.pull_requests);
    if tasks_config.enabled {
        match default_mode {
            TaskPanelMode::Session | TaskPanelMode::Agents => session_button.set_active(true),
            TaskPanelMode::Tasks => tasks_button.set_active(true),
            TaskPanelMode::PullRequests => pull_requests_button.set_active(true),
        }
    } else {
        session_button.set_active(true);
    }
    mode_switch.append(&session_button);
    mode_switch.append(&agents_button);
    mode_switch.append(&tasks_button);
    mode_switch.append(&pull_requests_button);
    body.append(&mode_switch);

    let session_content = crate::sidebar::build_session_work_panel(
        state.clone(),
        tab_list.clone(),
        term_stack.clone(),
    );
    let agents_content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    agents_content.set_hexpand(true);
    agents_content.set_vexpand(true);

    let agents_summary = gtk::Label::new(Some("No live agent panes"));
    agents_summary.add_css_class("agent-activity-summary");
    agents_summary.set_halign(gtk::Align::Start);
    agents_summary.set_wrap(true);
    agents_content.append(&agents_summary);

    let agents_list = gtk::ListBox::new();
    agents_list.add_css_class("agent-activity-list");
    agents_list.set_selection_mode(gtk::SelectionMode::None);
    let agents_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_width(TASK_PANEL_WIDTH)
        .min_content_height(180)
        .child(&agents_list)
        .build();
    agents_scroll.set_vexpand(true);
    agents_content.append(&agents_scroll);

    let agents_empty = gtk::Label::new(Some(
        "No live agent activity right now. Agent panes appear here automatically.",
    ));
    agents_empty.add_css_class("task-panel-empty");
    agents_empty.set_halign(gtk::Align::Start);
    agents_empty.set_wrap(true);
    agents_content.append(&agents_empty);

    let task_content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    task_content.set_hexpand(true);
    task_content.set_vexpand(true);
    body.append(&session_content);
    body.append(&agents_content);
    body.append(&task_content);

    let current_task_card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    current_task_card.add_css_class("task-panel-row");
    current_task_card.set_visible(false);
    let current_task_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let current_task_label = gtk::Label::new(None);
    current_task_label.add_css_class("task-panel-task-title");
    current_task_label.set_halign(gtk::Align::Start);
    current_task_label.set_wrap(true);
    current_task_label.set_hexpand(true);
    let current_task_clear = gtk::Button::with_label("Clear");
    current_task_clear.add_css_class("task-panel-action-button");
    current_task_header.append(&current_task_label);
    current_task_header.append(&current_task_clear);
    let current_task_status = gtk::Label::new(None);
    current_task_status.add_css_class("task-panel-note");
    current_task_status.set_halign(gtk::Align::Start);
    current_task_status.set_wrap(true);
    let current_task_pr = gtk::Button::with_label("Open correlated PR");
    current_task_pr.add_css_class("task-panel-action-button");
    current_task_pr.set_halign(gtk::Align::Start);
    current_task_pr.set_visible(false);
    current_task_card.append(&current_task_header);
    current_task_card.append(&current_task_status);
    current_task_card.append(&current_task_pr);
    task_content.append(&current_task_card);

    let progress = gtk::Label::new(Some("No progress data"));
    progress.add_css_class("task-panel-progress");
    progress.set_halign(gtk::Align::Start);
    progress.set_wrap(true);
    task_content.append(&progress);

    let summary = gtk::Label::new(Some("Waiting for task data"));
    summary.add_css_class("task-panel-summary");
    summary.set_halign(gtk::Align::Start);
    summary.set_wrap(true);
    task_content.append(&summary);

    let list = gtk::ListBox::new();
    list.add_css_class("task-panel-list");
    list.set_selection_mode(gtk::SelectionMode::None);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_width(TASK_PANEL_WIDTH)
        .min_content_height(180)
        .child(&list)
        .build();
    scroll.set_vexpand(true);
    task_content.append(&scroll);

    let empty = gtk::Label::new(Some("No open tasks right now."));
    empty.add_css_class("task-panel-empty");
    empty.set_halign(gtk::Align::Start);
    task_content.append(&empty);

    let actions = gtk::FlowBox::new();
    actions.add_css_class("task-panel-actions");
    actions.set_selection_mode(gtk::SelectionMode::None);
    actions.set_max_children_per_line(2);
    actions.set_column_spacing(6);
    actions.set_row_spacing(6);
    task_content.append(&actions);

    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideLeft);
    revealer.set_transition_duration(180);
    revealer.set_reveal_child(true);
    revealer.set_child(Some(&body));
    container.append(&revealer);

    let panel = TaskPanel {
        container,
        body,
        session_content,
        agents_content,
        agents_summary,
        agents_list,
        agents_empty,
        task_content,
        revealer,
        title,
        chevron,
        subtitle,
        progress,
        summary,
        current_task_card,
        current_task_label,
        current_task_status,
        current_task_pr,
        current_task_clear,
        list,
        actions,
        empty,
        mode_switch,
        session_button,
        agents_button,
        tasks_button,
        pull_requests_button,
        expanded: Rc::new(Cell::new(true)),
        tasks_enabled: tasks_config.enabled,
        pull_request_mode_enabled: tasks_config.enabled && tasks_config.pull_requests,
        mode_selection: Rc::new(RefCell::new(TaskPanelModeSelection::new(
            tasks_config.enabled,
            tasks_config.pull_requests,
            default_mode,
        ))),
        syncing_mode_switch: Rc::new(Cell::new(false)),
        last_snapshot: Rc::new(RefCell::new(None)),
        last_agent_cards: Rc::new(RefCell::new(None)),
        remote_cache: Rc::new(RefCell::new(HashMap::new())),
        local_task_cache: Rc::new(RefCell::new(HashMap::new())),
        local_task_inflight: Rc::new(RefCell::new(HashSet::new())),
        local_pull_request_cache: Rc::new(RefCell::new(HashMap::new())),
        local_pull_request_inflight: Rc::new(RefCell::new(HashSet::new())),
        remote_inflight: Rc::new(RefCell::new(HashSet::new())),
        remote_pull_request_cache: Rc::new(RefCell::new(HashMap::new())),
        remote_pull_request_inflight: Rc::new(RefCell::new(HashSet::new())),
        remote_pull_request_refresh_after_inflight: Rc::new(RefCell::new(HashSet::new())),
        pull_request_cache: Rc::new(RefCell::new(HashMap::new())),
        pull_request_inflight: Rc::new(RefCell::new(HashSet::new())),
        pull_request_refresh_after_inflight: Rc::new(RefCell::new(HashSet::new())),
        pull_request_feedback_pending: Rc::new(RefCell::new(HashSet::new())),
        remote_pull_request_feedback_pending: Rc::new(RefCell::new(HashSet::new())),
    };

    {
        let panel = panel.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        let clear_button = panel.current_task_clear.clone();
        clear_button.connect_clicked(move |_| {
            let target = panel
                .last_snapshot
                .borrow()
                .as_ref()
                .and_then(|snapshot| snapshot.current_task.as_ref())
                .map(|current| (current.tab_id, current.pane_id));
            let Some((tab_id, pane_id)) = target else {
                return;
            };
            let cleared = clear_pane_task_mutation(&state, tab_id, pane_id);
            if cleared {
                crate::show_toast("Cleared current task");
                schedule_task_panel_refresh(&panel, &state, &tab_list, &term_stack, &window);
            }
        });
    }

    {
        let panel = panel.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        let button = panel.agents_button.clone();
        button.connect_toggled(move |button| {
            if button.is_active() {
                panel.select_mode_for_active_context(
                    &state,
                    &tab_list,
                    &term_stack,
                    &window,
                    TaskPanelIntent::Select(TaskPanelMode::Agents),
                );
            }
        });
    }

    {
        let panel = panel.clone();
        let button = panel.current_task_pr.clone();
        button.connect_clicked(move |_| {
            let url = panel
                .last_snapshot
                .borrow()
                .as_ref()
                .and_then(|snapshot| snapshot.current_task.as_ref())
                .and_then(|current| current.pull_request.as_ref())
                .and_then(|pr| pr.url.as_deref())
                .map(str::to_string);
            if let Some(url) = url {
                open_external_url(&url);
            }
        });
    }

    {
        let panel = panel.clone();
        header_button.connect_clicked(move |_| {
            let next = !panel.expanded.get();
            panel.expanded.set(next);
            panel.revealer.set_reveal_child(next);
            panel.chevron.set_text(if next { "▾" } else { "▸" });
            panel
                .body
                .set_width_request(if next { TASK_PANEL_WIDTH } else { -1 });
        });
    }

    {
        let panel = panel.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        let button = panel.session_button.clone();
        button.connect_toggled(move |button| {
            if button.is_active() {
                panel.select_mode_for_active_context(
                    &state,
                    &tab_list,
                    &term_stack,
                    &window,
                    TaskPanelIntent::Select(TaskPanelMode::Session),
                );
            }
        });
    }

    {
        let panel = panel.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        let button = panel.tasks_button.clone();
        button.connect_toggled(move |button| {
            if button.is_active() {
                panel.select_mode_for_active_context(
                    &state,
                    &tab_list,
                    &term_stack,
                    &window,
                    TaskPanelIntent::Select(TaskPanelMode::Tasks),
                );
            }
        });
    }

    if tasks_config.enabled {
        let panel = panel.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        let button = panel.pull_requests_button.clone();
        button.connect_toggled(move |button| {
            if button.is_active() {
                panel.select_mode_for_active_context(
                    &state,
                    &tab_list,
                    &term_stack,
                    &window,
                    TaskPanelIntent::Select(TaskPanelMode::PullRequests),
                );
            }
        });
    }

    if runtime_policy.install_task_poll {
        let panel = panel.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        glib::timeout_add_seconds_local(TASK_PANEL_POLL_SECONDS, move || {
            panel.refresh(&state, &tab_list, &term_stack, &window);
            glib::ControlFlow::Continue
        });
    } else {
        let panel = panel.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        glib::timeout_add_seconds_local(TASK_PANEL_POLL_SECONDS, move || {
            panel.refresh(&state, &tab_list, &term_stack, &window);
            glib::ControlFlow::Continue
        });
    }

    panel.refresh(&state, &tab_list, &term_stack, &window);
    install_explicit_discovery_controller(ExplicitDiscoveryController {
        panel: panel.clone(),
        term_stack,
        window,
    });
    panel
}

fn state_available_for_refresh(state: &Rc<RefCell<AppState>>) -> bool {
    state.try_borrow().is_ok()
}

impl TaskPanel {
    fn refresh(
        &self,
        state: &Rc<RefCell<AppState>>,
        tab_list: &gtk::Box,
        term_stack: &gtk::Stack,
        window: &adw::ApplicationWindow,
    ) {
        if self.try_refresh(state, tab_list, term_stack, window) {
            return;
        }

        let panel = self.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(5), move || {
            // One bounded retry only. The normal panel poll will recover if some
            // longer-running GTK callback still owns AppState after this tick.
            let _ = panel.try_refresh(&state, &tab_list, &term_stack, &window);
        });
    }

    fn try_refresh(
        &self,
        state: &Rc<RefCell<AppState>>,
        tab_list: &gtk::Box,
        term_stack: &gtk::Stack,
        window: &adw::ApplicationWindow,
    ) -> bool {
        if !state_available_for_refresh(state) {
            return false;
        }
        self.refresh_inner(state, tab_list, term_stack, window);
        true
    }

    fn refresh_inner(
        &self,
        state: &Rc<RefCell<AppState>>,
        tab_list: &gtk::Box,
        term_stack: &gtk::Stack,
        window: &adw::ApplicationWindow,
    ) {
        let agent_count = self.refresh_agent_cards(state, tab_list, term_stack);
        let context = self.view_context(state);
        self.sync_mode_switch(
            context.mode,
            context.show_task_modes,
            context.show_pull_requests,
        );
        self.session_content
            .set_visible(context.mode == TaskPanelMode::Session);
        self.agents_content
            .set_visible(context.mode == TaskPanelMode::Agents);
        self.task_content.set_visible(matches!(
            context.mode,
            TaskPanelMode::Tasks | TaskPanelMode::PullRequests
        ));
        let active_pr_target = context.pull_request_target.clone();
        let active_remote_target = context.remote_target.clone();
        if context.mode == TaskPanelMode::Tasks && active_remote_target.is_none() {
            self.ensure_local_task_snapshot(state, tab_list, term_stack, window);
        }
        if self.pull_request_mode_enabled {
            if let Some(remote) = active_remote_target.as_ref() {
                self.ensure_remote_pull_request_identity(
                    state,
                    tab_list,
                    term_stack,
                    window,
                    remote,
                    PullRequestFetchReason::Passive,
                );
                let remote_key = remote_cache_key(remote);
                let explicit_pending = self
                    .remote_pull_request_feedback_pending
                    .borrow()
                    .contains(&remote_key)
                    && !self
                        .remote_pull_request_inflight
                        .borrow()
                        .contains(&remote_key);
                if explicit_pending {
                    if let Some(target) = active_pr_target.as_ref() {
                        self.remote_pull_request_feedback_pending
                            .borrow_mut()
                            .remove(&remote_key);
                        self.pull_request_feedback_pending
                            .borrow_mut()
                            .insert(PullRequestKey::for_target(target));
                    } else if let Some(entry) =
                        self.remote_pull_request_cache.borrow().get(&remote_key)
                    {
                        if !matches!(entry.outcome, RemoteGitIdentityOutcome::GitHub(_)) {
                            let message = remote_pull_request_discovery_feedback(&entry.outcome);
                            self.remote_pull_request_feedback_pending
                                .borrow_mut()
                                .remove(&remote_key);
                            crate::show_error_toast(&message);
                        }
                    }
                }
            }
            if let Some(target) = active_pr_target.as_ref() {
                let reason = if self
                    .pull_request_feedback_pending
                    .borrow()
                    .contains(&PullRequestKey::for_target(target))
                {
                    PullRequestFetchReason::ExplicitDiscovery
                } else {
                    PullRequestFetchReason::Passive
                };
                self.ensure_pull_request_fetch(state, tab_list, term_stack, window, target, reason);
            }
        }

        let mut next = match context.mode {
            TaskPanelMode::Session => placeholder_snapshot(
                "Session work".to_string(),
                "Current session".to_string(),
                "Session work is shown in this dock.".to_string(),
            ),
            TaskPanelMode::Agents => placeholder_snapshot(
                "Agent Activity".to_string(),
                if agent_count == 1 {
                    "1 live agent pane".to_string()
                } else {
                    format!("{agent_count} live agent panes")
                },
                "Live observational state from terminal panes.".to_string(),
            ),
            TaskPanelMode::Tasks => match active_remote_target.clone() {
                Some(target) => {
                    // Remote pane: kick an async SSH fetch if the cache is stale,
                    // and render whatever the cache currently holds (loading/loaded/
                    // error). Never reads the remote file on this (GTK main) thread.
                    self.ensure_remote_fetch(state, tab_list, term_stack, window, &target);
                    self.remote_snapshot(&target)
                }
                None => self.snapshot_task_panel(state),
            },
            TaskPanelMode::PullRequests => match context.pull_request_target {
                Some(target) => self.pull_request_snapshot(&target),
                None => active_remote_target
                    .as_ref()
                    .map_or_else(unavailable_pull_request_snapshot, |remote| {
                        self.remote_pull_request_snapshot(remote)
                    }),
            },
        };
        next.current_task = self
            .tasks_enabled
            .then(|| {
                if active_remote_target.is_none() {
                    self.cached_local_task_current(state)
                } else {
                    remote_current_task_summary(&state.borrow())
                }
            })
            .flatten();
        if let (Some(current), Some(target)) =
            (next.current_task.as_mut(), active_pr_target.as_ref())
        {
            current.pull_request = self.correlated_current_task_pr(current, target);
        }
        {
            let st = state.borrow();
            // Build the shared pane metadata + truth inputs a single time for
            // this refresh. Both are O(panes × records); computing them per task
            // row would make the panel O(tasks × panes × records).
            let records = st.work_ledger.records();
            let metadata = crate::work_ledger::work_stream_pane_metadata(&st, &records);
            let inputs = crate::work_ledger::task_truth_inputs(&st, &metadata);
            for task in &mut next.tasks {
                if let Some(binding) = task.binding.as_mut() {
                    binding.truth = crate::work_ledger::task_truth_for_pane_task_with(
                        &st,
                        &metadata,
                        &inputs,
                        binding.tab_id,
                        binding.pane_id,
                        &task.id,
                        crate::work_ledger::CanonicalTaskState::from_status(&task.status),
                    );
                }
            }
            if let Some(current) = next.current_task.as_mut() {
                let canonical = next
                    .tasks
                    .iter()
                    .find(|task| task.id == current.id)
                    .map_or_else(
                        || {
                            if current.resolved {
                                crate::work_ledger::CanonicalTaskState::Unknown
                            } else {
                                crate::work_ledger::CanonicalTaskState::Absent
                            }
                        },
                        |task| crate::work_ledger::CanonicalTaskState::from_status(&task.status),
                    );
                current.truth = crate::work_ledger::task_truth_for_pane_task_with(
                    &st,
                    &metadata,
                    &inputs,
                    current.tab_id,
                    current.pane_id,
                    &current.id,
                    canonical,
                );
            }
        }
        let mut last = self.last_snapshot.borrow_mut();
        if *last == Some(next.clone()) {
            return;
        }
        *last = Some(next.clone());
        drop(last);

        self.subtitle.set_text(&next.subtitle);
        self.progress.set_text(&next.progress);
        self.summary.set_text(&next.summary);
        if let Some(current) = next.current_task.as_ref() {
            self.current_task_label.set_text(&format!(
                "Current work · {} — {}",
                current.id, current.title
            ));
            let resolution = if current.resolved {
                let base = "Resolved in this pane's pinned checkout";
                current
                    .pull_request
                    .as_ref()
                    .map_or_else(|| base.to_string(), |pr| format!("{base}\n{}", pr.label))
            } else {
                "Unresolved — pane checkout or .plan task no longer matches".to_string()
            };
            self.current_task_status.set_text(
                &current.truth.as_ref().map_or(resolution.clone(), |truth| {
                    format!("{resolution}\n{}", task_truth_label(truth))
                }),
            );
            self.current_task_pr.set_visible(
                current
                    .pull_request
                    .as_ref()
                    .is_some_and(|pr| pr.url.is_some() && !pr.ambiguous),
            );
            self.current_task_card.set_visible(true);
        } else {
            self.current_task_pr.set_visible(false);
            self.current_task_card.set_visible(false);
        }

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        if let Some(header) = next.parallelization_header.as_deref() {
            self.list.append(&build_parallelization_header_row(header));
        }
        let dispatch = RowDispatchContext {
            panel: self.clone(),
            state: state.clone(),
            tab_list: tab_list.clone(),
            term_stack: term_stack.clone(),
            window: window.clone(),
        };
        for task in &next.tasks {
            self.list.append(&build_task_row(task, &dispatch));
        }
        self.empty.set_visible(next.tasks.is_empty());

        while let Some(child) = self.actions.first_child() {
            self.actions.remove(&child);
        }
        populate_action_buttons(
            self,
            state,
            tab_list,
            term_stack,
            window,
            &next.action_labels,
        );
    }

    fn refresh_agent_cards(
        &self,
        state: &Rc<RefCell<AppState>>,
        tab_list: &gtk::Box,
        term_stack: &gtk::Stack,
    ) -> usize {
        let cards = project_all_agent_panes(&state.borrow());
        let count = cards.len();
        if self.last_agent_cards.borrow().as_ref() == Some(&cards) {
            return count;
        }
        *self.last_agent_cards.borrow_mut() = Some(cards.clone());
        self.last_snapshot.borrow_mut().take();

        let summary = if count == 1 {
            "1 agent pane · live observational state".to_string()
        } else if count == 0 {
            "No live agent panes".to_string()
        } else {
            format!("{count} agent panes · live observational state")
        };
        self.agents_summary.set_text(&summary);
        while let Some(child) = self.agents_list.first_child() {
            self.agents_list.remove(&child);
        }
        for card in &cards {
            self.agents_list
                .append(&build_agent_activity_row(card, state, tab_list, term_stack));
        }
        self.agents_empty.set_visible(cards.is_empty());
        count
    }

    fn correlated_current_task_pr(
        &self,
        current: &CurrentTaskSummary,
        target: &PullRequestTarget,
    ) -> Option<CurrentTaskPullRequest> {
        if !current.resolved {
            return None;
        }
        let bound = target.bound_task.as_ref()?;
        if bound.task_id != current.id || bound.title != current.title {
            return None;
        }
        let key = PullRequestKey::for_target(target);
        let cache = self.pull_request_cache.borrow();
        let entry = cache.get(&key)?;
        let data = entry.verified.as_ref()?;
        let stale = entry.error.is_some();
        match crate::tracking::correlate_pull_request(
            &current.id,
            &target.base_repository,
            &target.head_owner,
            &target.branch,
            data,
        ) {
            crate::tracking::PullRequestCorrelation::Linked(pr) => {
                let checks = match pr.checks {
                    crate::tracking::PullRequestChecks::None => "checks unknown",
                    crate::tracking::PullRequestChecks::Pending => "checks pending",
                    crate::tracking::PullRequestChecks::Failed => "checks failed",
                    crate::tracking::PullRequestChecks::Ready => "checks ready",
                };
                let stale_label = if stale { " · stale/error" } else { "" };
                Some(CurrentTaskPullRequest {
                    label: format!(
                        "PR #{} — {} · {} · {checks}{stale_label}",
                        pr.number,
                        pr.title,
                        pr.status_label()
                    ),
                    url: pr.url.clone(),
                    ambiguous: false,
                })
            }
            crate::tracking::PullRequestCorrelation::Ambiguous => Some(CurrentTaskPullRequest {
                label: "PR ambiguous — unlinked".to_string(),
                url: None,
                ambiguous: true,
            }),
            crate::tracking::PullRequestCorrelation::Unlinked => None,
            crate::tracking::PullRequestCorrelation::Incomplete => Some(CurrentTaskPullRequest {
                label: "PR query incomplete — unlinked".to_string(),
                url: None,
                ambiguous: true,
            }),
        }
    }

    fn cached_remote_pull_request_target(
        &self,
        state: &Rc<RefCell<AppState>>,
        remote: &RemoteTaskTarget,
    ) -> Option<PullRequestTarget> {
        let key = remote_cache_key(remote);
        if self
            .remote_pull_request_feedback_pending
            .borrow()
            .contains(&key)
            && self.remote_pull_request_inflight.borrow().contains(&key)
        {
            // An explicit refresh must not launch a GitHub query from the old
            // identity, but retain that entry for failure-streak accounting.
            return None;
        }
        let cache = self.remote_pull_request_cache.borrow();
        let RemoteGitIdentityOutcome::GitHub(identity) = &cache.get(&key)?.outcome else {
            return None;
        };
        let st = state.borrow();
        let tab = st.active_tab()?;
        let pane_id = tab.focused_pane_id;
        let binding = st.pane_task_binding(tab.id, pane_id);
        let pane_identity = crate::work_ledger::identity_for_pane(&st, tab.id, pane_id, binding)?;
        Some(PullRequestTarget {
            checkout_root: PathBuf::from(&identity.checkout_root),
            branch: identity.branch.clone(),
            head_repository: identity.head_repository.clone(),
            base_repository: identity.base_repository.clone(),
            head_owner: identity.head_owner.clone(),
            remote_connection: Some(key.0),
            pane_identity,
            // Remote task bindings cannot be revalidated from the controlling
            // host's filesystem. Keep the PR surface display-only; marker-based
            // rows remain visible, while ledger enrichment stays on its existing
            // local `gh` reconciliation path.
            bound_task: None,
            loop_runner_availability: None,
        })
    }

    fn ensure_local_pull_request_identity(&self, state: &Rc<RefCell<AppState>>) {
        let Some(request) = local_pull_request_probe_request(state) else {
            return;
        };
        let now = now_ms();
        if self
            .local_pull_request_cache
            .borrow()
            .get(&request.key)
            .is_some_and(|entry| local_pull_request_cache_entry_is_fresh(entry, now))
            || !self
                .local_pull_request_inflight
                .borrow_mut()
                .insert(request.key.clone())
        {
            return;
        }
        // A stale entry must not suppress the worker submission below, and it
        // must not occupy one of the bounded cache slots while the refresh is
        // in flight.
        self.local_pull_request_cache
            .borrow_mut()
            .remove(&request.key);

        let key = request.key.clone();
        let worker_request = request.clone();
        let panel = self.clone();
        let state_for_apply = state.clone();
        let submission = crate::git::spawn_async_result(
            format!(
                "task-panel-local-pr:{}:{}:{}:{}:{}:{}",
                key.workspace_origin,
                key.tab_origin,
                key.pane_origin,
                key.cwd,
                key.candidates.join("\u{1f}"),
                key.reporting_token.as_deref().unwrap_or("unbound"),
            ),
            move || Ok(probe_local_pull_request_target(&worker_request)),
            move |result| {
                panel.local_pull_request_inflight.borrow_mut().remove(&key);
                // A local path may have changed, a tab may have been restored,
                // or the binding may have been replaced while the worker was
                // blocked.  In all those cases, drop rather than apply.
                if !local_pull_request_probe_request(&state_for_apply)
                    .is_some_and(|current| current.key == key)
                {
                    return;
                }
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        // A join failure is not a valid "no pull request"
                        // observation. Leave the cache empty so a later panel
                        // refresh submits a real retry.
                        crate::show_error_toast(&format!(
                            "Pull-request identity worker failed: {error}"
                        ));
                        return;
                    }
                };
                let mut cache = panel.local_pull_request_cache.borrow_mut();
                if !cache.contains_key(&key) && cache.len() >= LOCAL_PULL_REQUEST_CACHE_CAPACITY {
                    if let Some(evicted) = cache.keys().next().cloned() {
                        cache.remove(&evicted);
                    }
                }
                cache.insert(
                    key,
                    LocalPullRequestCacheEntry {
                        fetched_at_ms: now_ms(),
                        result,
                    },
                );
            },
        );
        if !matches!(submission, crate::git::GitAsyncSubmission::Started) {
            self.local_pull_request_inflight
                .borrow_mut()
                .remove(&request.key);
            if matches!(submission, crate::git::GitAsyncSubmission::Saturated) {
                // Do not cache an unavailable worker as an empty, healthy
                // repository identity.  A later refresh gets a real retry.
                crate::show_toast("Git worker queue is busy; pull-request identity will retry");
            }
        }
    }

    fn cached_local_pull_request_target(
        &self,
        state: &Rc<RefCell<AppState>>,
    ) -> Option<PullRequestTarget> {
        let request = local_pull_request_probe_request(state)?;
        let result = local_pull_request_cache_result_for_request(
            &self.local_pull_request_cache.borrow(),
            &request,
            now_ms(),
        )?;
        Some(PullRequestTarget {
            checkout_root: result.checkout_root,
            branch: result.branch,
            head_owner: result.repositories.head.split_once('/')?.0.to_string(),
            head_repository: result.repositories.head,
            base_repository: result.repositories.base,
            remote_connection: None,
            pane_identity: request.pane_identity,
            bound_task: result.bound_task,
            loop_runner_availability: result.loop_runner_availability,
        })
    }

    fn invalidate_local_task_cache(&self) {
        self.local_task_cache.borrow_mut().clear();
        self.local_pull_request_cache.borrow_mut().clear();
        self.last_snapshot.borrow_mut().take();
    }

    fn cached_local_task_current(
        &self,
        state: &Rc<RefCell<AppState>>,
    ) -> Option<CurrentTaskSummary> {
        let request = local_task_probe_request(state)?;
        self.local_task_cache
            .borrow()
            .get(&request.key)
            .and_then(|entry| entry.result.current_task.clone())
    }

    fn ensure_local_task_snapshot(
        &self,
        state: &Rc<RefCell<AppState>>,
        tab_list: &gtk::Box,
        term_stack: &gtk::Stack,
        window: &adw::ApplicationWindow,
    ) {
        let Some(request) = local_task_probe_request(state) else {
            return;
        };
        if self
            .local_task_cache
            .borrow()
            .get(&request.key)
            .is_some_and(|entry| now_ms().saturating_sub(entry.fetched_at_ms) < LOCAL_TASK_TTL_MS)
            || !self
                .local_task_inflight
                .borrow_mut()
                .insert(request.key.clone())
        {
            return;
        }
        let key = request.key.clone();
        let worker_request = request.clone();
        let panel = self.clone();
        let state_for_apply = state.clone();
        let tab_list_for_apply = tab_list.clone();
        let term_stack_for_apply = term_stack.clone();
        let window_for_apply = window.clone();
        let submission = crate::git::spawn_async_result(
            local_task_probe_worker_key(&key),
            move || Ok(probe_local_task_snapshot(&worker_request)),
            move |result| {
                panel.local_task_inflight.borrow_mut().remove(&key);
                if !local_task_probe_is_current(&state_for_apply, &key) {
                    // `spawn_async` has already released the global ticket at
                    // this point, so refresh can submit the current generation
                    // without waiting behind this stale one.
                    panel.refresh(
                        &state_for_apply,
                        &tab_list_for_apply,
                        &term_stack_for_apply,
                        &window_for_apply,
                    );
                    return;
                }
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        // Do not cache a worker failure as an empty plan: the
                        // next one-second panel tick must be able to retry.
                        crate::show_error_toast(&format!("Task-panel worker failed: {error}"));
                        panel.refresh(
                            &state_for_apply,
                            &tab_list_for_apply,
                            &term_stack_for_apply,
                            &window_for_apply,
                        );
                        return;
                    }
                };
                let mut cache = panel.local_task_cache.borrow_mut();
                if !cache.contains_key(&key) && cache.len() >= LOCAL_TASK_CACHE_CAPACITY {
                    if let Some(evicted) = cache.keys().next().cloned() {
                        cache.remove(&evicted);
                    }
                }
                cache.insert(
                    key,
                    LocalTaskCacheEntry {
                        fetched_at_ms: now_ms(),
                        result,
                    },
                );
                drop(cache);
                panel.refresh(
                    &state_for_apply,
                    &tab_list_for_apply,
                    &term_stack_for_apply,
                    &window_for_apply,
                );
            },
        );
        if !matches!(submission, crate::git::GitAsyncSubmission::Started) {
            self.local_task_inflight.borrow_mut().remove(&request.key);
            if matches!(submission, crate::git::GitAsyncSubmission::Saturated) {
                crate::show_toast("Git worker queue is busy; task panel will retry");
            }
        }
    }

    fn view_context(&self, state: &Rc<RefCell<AppState>>) -> TaskPanelViewContext {
        let runtime_policy = task_panel_runtime_policy(self.tasks_enabled);
        if !runtime_policy.load_task_sources {
            let mode = self.mode_selection.borrow().mode_for_context("global");
            return TaskPanelViewContext {
                mode: if mode == TaskPanelMode::Agents {
                    TaskPanelMode::Agents
                } else {
                    TaskPanelMode::Session
                },
                show_task_modes: false,
                show_pull_requests: false,
                remote_target: None,
                pull_request_target: None,
            };
        }

        let remote_target = remote_task_target(state);
        if remote_target.is_none() && self.pull_request_mode_enabled {
            self.ensure_local_pull_request_identity(state);
        }
        let pull_request_target = remote_target
            .as_ref()
            .and_then(|target| self.cached_remote_pull_request_target(state, target))
            .or_else(|| {
                remote_target
                    .is_none()
                    .then(|| self.cached_local_pull_request_target(state))
                    .flatten()
            });
        let mode_key = active_mode_key(state, pull_request_target.as_ref(), remote_target.as_ref());
        let selected_mode = self.mode_selection.borrow().mode_for_context(&mode_key);

        if matches!(
            selected_mode,
            TaskPanelMode::Session | TaskPanelMode::Agents
        ) || !self.tasks_enabled
        {
            return TaskPanelViewContext {
                mode: selected_mode,
                show_task_modes: self.tasks_enabled,
                show_pull_requests: self.pull_request_mode_enabled,
                remote_target,
                pull_request_target,
            };
        }

        if remote_target.is_some() {
            return TaskPanelViewContext {
                mode: selected_mode,
                show_task_modes: true,
                show_pull_requests: self.pull_request_mode_enabled,
                remote_target,
                pull_request_target,
            };
        }

        TaskPanelViewContext {
            mode: selected_mode,
            show_task_modes: true,
            show_pull_requests: self.pull_request_mode_enabled,
            remote_target: None,
            pull_request_target,
        }
    }

    fn select_mode_for_active_context(
        &self,
        state: &Rc<RefCell<AppState>>,
        tab_list: &gtk::Box,
        term_stack: &gtk::Stack,
        window: &adw::ApplicationWindow,
        intent: TaskPanelIntent,
    ) {
        if self.syncing_mode_switch.get() {
            return;
        }
        if !self.tasks_enabled
            && !matches!(
                intent,
                TaskPanelIntent::Select(TaskPanelMode::Session | TaskPanelMode::Agents)
            )
        {
            return;
        }
        if !self.pull_request_mode_enabled
            && matches!(intent, TaskPanelIntent::Select(TaskPanelMode::PullRequests))
        {
            return;
        }
        let remote_target = remote_task_target(state);
        if remote_target.is_none() && self.pull_request_mode_enabled {
            self.ensure_local_pull_request_identity(state);
        }
        let target = remote_target
            .as_ref()
            .and_then(|remote| self.cached_remote_pull_request_target(state, remote))
            .or_else(|| {
                remote_target
                    .is_none()
                    .then(|| self.cached_local_pull_request_target(state))
                    .flatten()
            });
        let mode_key = active_mode_key(state, target.as_ref(), remote_target.as_ref());
        self.mode_selection
            .borrow_mut()
            .apply_intent(&mode_key, intent);
        self.last_snapshot.borrow_mut().take();
        self.refresh(state, tab_list, term_stack, window);
    }

    fn sync_mode_switch(
        &self,
        mode: TaskPanelMode,
        show_task_modes: bool,
        show_pull_requests: bool,
    ) {
        self.mode_switch.set_visible(true);
        self.tasks_button.set_visible(show_task_modes);
        self.pull_requests_button
            .set_visible(show_pull_requests && show_task_modes);
        self.title.set_text(match mode {
            TaskPanelMode::Session => "Session",
            TaskPanelMode::Agents => "Agent Activity",
            TaskPanelMode::Tasks => "Tasks",
            TaskPanelMode::PullRequests => "Pull Requests",
        });
        self.syncing_mode_switch.set(true);
        match mode {
            TaskPanelMode::Session => self.session_button.set_active(true),
            TaskPanelMode::Agents => self.agents_button.set_active(true),
            TaskPanelMode::Tasks => self.tasks_button.set_active(true),
            TaskPanelMode::PullRequests => self.pull_requests_button.set_active(true),
        }
        self.syncing_mode_switch.set(false);
    }

    /// Spawn an async SSH read of the remote `.plan/tasks.json` when the cache
    /// for this exact `(connection argv, cwd)` is missing or older than the TTL,
    /// unless a fetch is already in flight. The blocking SSH runs on a worker
    /// thread; on completion it updates the cache and re-renders the panel.
    fn ensure_remote_fetch(
        &self,
        state: &Rc<RefCell<AppState>>,
        tab_list: &gtk::Box,
        term_stack: &gtk::Stack,
        window: &adw::ApplicationWindow,
        target: &RemoteTaskTarget,
    ) {
        let key = remote_cache_key(target);
        let now = now_ms();
        if let Some(entry) = self.remote_cache.borrow().get(&key) {
            if now.saturating_sub(entry.fetched_at_ms) < remote_tasks_ttl_ms(entry.failure_streak) {
                return;
            }
        }
        // Atomic check-and-insert: bail if another fetch for this key is running.
        if !self.remote_inflight.borrow_mut().insert(key.clone()) {
            return;
        }

        let argv =
            remote_tasks_fetch_command(target.ssh_argv.as_deref(), &target.host, &target.cwd);
        let host = target.host.clone();
        let panel = self.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        glib::spawn_future_local(async move {
            let outcome = gio::spawn_blocking(move || {
                classify_remote_tasks_fetch(
                    crate::terminal::run_tmux_command_sync_result_without_diagnostics(&argv),
                )
            })
            .await
            .unwrap_or(RemoteTasksOutcome::Error(RemoteTasksError::Transport));

            let (previous_streak, previous_error) = panel
                .remote_cache
                .borrow()
                .get(&key)
                .map(|entry| {
                    let error = match &entry.outcome {
                        RemoteTasksOutcome::Error(error) => Some(*error),
                        _ => None,
                    };
                    (entry.failure_streak, error)
                })
                .unwrap_or((0, None));
            let failure_streak = next_remote_failure_streak(previous_streak, &outcome);
            if let RemoteTasksOutcome::Error(error) = &outcome {
                if previous_error != Some(*error) {
                    crate::diagnostics::record_command_failure(
                        "task-panel",
                        "remote-tasks-fetch",
                        format!("remote task fetch failed for host {host}"),
                        Some(serde_json::json!({
                            "host": host,
                            "kind": error.kind(),
                        })),
                    );
                }
            }
            panel.remote_cache.borrow_mut().insert(
                key.clone(),
                RemoteTasksEntry {
                    fetched_at_ms: now_ms(),
                    outcome,
                    failure_streak,
                },
            );
            panel.remote_inflight.borrow_mut().remove(&key);
            panel.refresh(&state, &tab_list, &term_stack, &window);
        });
    }

    fn ensure_remote_pull_request_identity(
        &self,
        state: &Rc<RefCell<AppState>>,
        tab_list: &gtk::Box,
        term_stack: &gtk::Stack,
        window: &adw::ApplicationWindow,
        target: &RemoteTaskTarget,
        reason: PullRequestFetchReason,
    ) {
        let key = remote_cache_key(target);
        let now = now_ms();
        if reason == PullRequestFetchReason::Passive {
            if let Some(entry) = self.remote_pull_request_cache.borrow().get(&key) {
                if now.saturating_sub(entry.fetched_at_ms)
                    < remote_pull_request_identity_ttl_ms(entry.failure_streak)
                {
                    return;
                }
            }
        }
        if !register_pull_request_fetch(
            &mut self.remote_pull_request_inflight.borrow_mut(),
            &mut self.remote_pull_request_refresh_after_inflight.borrow_mut(),
            &key,
            reason,
        ) {
            return;
        }
        let argv =
            remote_git_identity_command(target.ssh_argv.as_deref(), &target.host, &target.cwd);
        let retry_target = target.clone();
        let host = target.host.clone();
        let panel = self.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        glib::spawn_future_local(async move {
            let outcome = gio::spawn_blocking(move || {
                classify_remote_git_identity(
                    crate::terminal::run_tmux_command_sync_result_without_diagnostics(&argv),
                )
            })
            .await
            .unwrap_or(RemoteGitIdentityOutcome::Error(
                RemoteGitIdentityError::Transport,
            ));

            if panel
                .remote_pull_request_refresh_after_inflight
                .borrow_mut()
                .remove(&key)
            {
                panel.remote_pull_request_inflight.borrow_mut().remove(&key);
                panel.ensure_remote_pull_request_identity(
                    &state,
                    &tab_list,
                    &term_stack,
                    &window,
                    &retry_target,
                    PullRequestFetchReason::ExplicitDiscovery,
                );
                panel.refresh(&state, &tab_list, &term_stack, &window);
                return;
            }

            let (previous_streak, previous_error) = panel
                .remote_pull_request_cache
                .borrow()
                .get(&key)
                .map(|entry| {
                    let error = match entry.outcome {
                        RemoteGitIdentityOutcome::Error(error) => Some(error),
                        _ => None,
                    };
                    (entry.failure_streak, error)
                })
                .unwrap_or((0, None));
            let failure_streak = next_remote_git_failure_streak(previous_streak, &outcome);
            if let RemoteGitIdentityOutcome::Error(error) = &outcome {
                if previous_error != Some(*error) {
                    crate::diagnostics::record_command_failure(
                        "task-panel",
                        "remote-git-identity",
                        format!("remote Git identity probe failed for host {host}"),
                        Some(serde_json::json!({
                            "host": host,
                            "kind": error.kind(),
                        })),
                    );
                }
            }
            panel.remote_pull_request_cache.borrow_mut().insert(
                key.clone(),
                RemotePullRequestIdentityEntry {
                    fetched_at_ms: now_ms(),
                    outcome,
                    failure_streak,
                },
            );
            panel.remote_pull_request_inflight.borrow_mut().remove(&key);
            let still_active = remote_task_target(&state)
                .as_ref()
                .is_some_and(|target| remote_cache_key(target) == key);
            if !still_active {
                panel
                    .remote_pull_request_feedback_pending
                    .borrow_mut()
                    .remove(&key);
            }
            panel.refresh(&state, &tab_list, &term_stack, &window);
        });
    }

    fn ensure_pull_request_fetch(
        &self,
        state: &Rc<RefCell<AppState>>,
        tab_list: &gtk::Box,
        term_stack: &gtk::Stack,
        window: &adw::ApplicationWindow,
        target: &PullRequestTarget,
        reason: PullRequestFetchReason,
    ) {
        let key = PullRequestKey::for_target(target);
        let now = now_ms();
        if reason == PullRequestFetchReason::Passive {
            if let Some(entry) = self.pull_request_cache.borrow().get(&key) {
                if now.saturating_sub(entry.fetched_at_ms) < PULL_REQUESTS_TTL_MS {
                    return;
                }
            }
        }
        if !register_pull_request_fetch(
            &mut self.pull_request_inflight.borrow_mut(),
            &mut self.pull_request_refresh_after_inflight.borrow_mut(),
            &key,
            reason,
        ) {
            return;
        }

        let argv = branch_pull_requests_command(&target.base_repository, &target.branch);
        let retry_target = target.clone();
        let base_repository = target.base_repository.clone();
        let head_owner = target.head_owner.clone();
        let branch = target.branch.clone();
        let worker_head_owner = head_owner.clone();
        let worker_branch = branch.clone();
        let pane_identity = target.pane_identity.clone();
        let bound_task = target.bound_task.clone();
        let remote = target.remote_connection.is_some();
        let panel = self.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        glib::spawn_future_local(async move {
            let (result, verified_bound_task) = gio::spawn_blocking(move || {
                let result = run_local_gh_command(&argv).and_then(|output| {
                    BranchPullRequestsData::from_gh_json_str(&output)
                        .map(|data| data.into_exact_head(&worker_head_owner, &worker_branch))
                        .ok_or_else(|| "could not parse branch pull requests".to_string())
                });
                let verified = (!remote)
                    .then_some(bound_task)
                    .flatten()
                    .and_then(verify_pull_request_bound_task);
                (result, verified)
            })
            .await
            .unwrap_or_else(|_| {
                (
                    Err("branch pull request fetch did not complete".to_string()),
                    None,
                )
            });

            if panel
                .pull_request_refresh_after_inflight
                .borrow_mut()
                .remove(&key)
            {
                panel.pull_request_inflight.borrow_mut().remove(&key);
                panel.ensure_pull_request_fetch(
                    &state,
                    &tab_list,
                    &term_stack,
                    &window,
                    &retry_target,
                    PullRequestFetchReason::ExplicitDiscovery,
                );
                return;
            }
            let verified_for_ledger = result.as_ref().ok().cloned();

            let active_key = panel
                .view_context(&state)
                .pull_request_target
                .as_ref()
                .map(PullRequestKey::for_target);

            if panel
                .pull_request_feedback_pending
                .borrow_mut()
                .remove(&key)
                && active_key.as_ref() == Some(&key)
            {
                match pull_request_discovery_feedback(&result) {
                    PullRequestDiscoveryFeedback::Success(message) => crate::show_toast(&message),
                    PullRequestDiscoveryFeedback::Error(message) => {
                        crate::show_error_toast(&message)
                    }
                }
            }

            record_pull_request_fetch(
                &mut panel.pull_request_cache.borrow_mut(),
                key.clone(),
                result,
                now_ms(),
            );
            let current_local_target = (!remote)
                .then(|| panel.cached_local_pull_request_target(&state))
                .flatten()
                .filter(|target| target.pane_identity.tab_id == pane_identity.tab_id);
            let current_binding_matches = current_local_target.as_ref().is_some_and(|target| {
                PullRequestKey::for_target(target) == key
                    && target.pane_identity.pane_origin == pane_identity.pane_origin
                    && target.bound_task.as_ref() == verified_bound_task.as_ref()
            });
            if let (Some(data), Some(bound)) = (
                verified_for_ledger.as_ref(),
                verified_bound_task
                    .as_ref()
                    .filter(|_| current_binding_matches),
            ) {
                state.borrow_mut().observe_verified_pull_requests(
                    &crate::work_ledger::VerifiedPullRequestBinding {
                        identity: pane_identity,
                        task_id: bound.task_id.clone(),
                        title: bound.title.clone(),
                        checkout_root: bound.checkout_root.clone(),
                        reporting_token: bound.reporting_token.clone(),
                        base_repository,
                        head_owner,
                        branch,
                    },
                    data,
                );
            }
            panel.pull_request_inflight.borrow_mut().remove(&key);
            panel.refresh(&state, &tab_list, &term_stack, &window);
        });
    }

    fn refresh_pull_requests_for_explicit_discovery(
        &self,
        state: &Rc<RefCell<AppState>>,
        tab_list: &gtk::Box,
        tab_id: u32,
        term_stack: &gtk::Stack,
        window: &adw::ApplicationWindow,
    ) {
        if !self.pull_request_mode_enabled {
            return;
        }

        let requested_discovery_target = {
            let st = state.borrow();
            crate::mise::discovery_target_for_tab(&st, tab_id)
        };
        match requested_discovery_target {
            Some(crate::mise::DiscoveryTarget::Local { .. }) => {}
            Some(crate::mise::DiscoveryTarget::Remote {
                host,
                cwd,
                ssh_argv,
            }) => {
                let target = RemoteTaskTarget {
                    host,
                    cwd,
                    ssh_argv: Some(ssh_argv),
                };
                self.remote_pull_request_feedback_pending
                    .borrow_mut()
                    .insert(remote_cache_key(&target));
                self.ensure_remote_pull_request_identity(
                    state,
                    tab_list,
                    term_stack,
                    window,
                    &target,
                    PullRequestFetchReason::ExplicitDiscovery,
                );
                return;
            }
            None => {
                // Local discovery already explained why this tab could not be
                // resolved. Do not add an unrelated gh query or second toast.
                return;
            }
        }

        self.ensure_local_pull_request_identity(state);
        let Some(target) = self
            .cached_local_pull_request_target(state)
            .filter(|target| target.pane_identity.tab_id == tab_id)
        else {
            crate::show_toast("Resolving local GitHub checkout for pull-request refresh");
            return;
        };

        let key = PullRequestKey::for_target(&target);
        self.pull_request_feedback_pending.borrow_mut().insert(key);
        self.ensure_pull_request_fetch(
            state,
            tab_list,
            term_stack,
            window,
            &target,
            PullRequestFetchReason::ExplicitDiscovery,
        );
    }

    /// Build the panel snapshot for a remote target from the current cache:
    /// loading (no entry yet), the parsed tasks, or an explicit host-named error.
    fn remote_snapshot(&self, target: &RemoteTaskTarget) -> TaskPanelSnapshot {
        let subtitle = remote_subtitle(target);
        let key = remote_cache_key(target);
        let cache = self.remote_cache.borrow();
        match cache.get(&key) {
            None => placeholder_snapshot(
                subtitle,
                progress_for_unavailable("loading"),
                format!("Loading tasks from {}…", target.host),
            ),
            Some(entry) => match &entry.outcome {
                // Remote panes are display-only: no run buttons (action_labels
                // empty), no local markdown paths (markdown_root None), and no
                // loop-runner dispatch (remote dispatch is out of scope).
                RemoteTasksOutcome::Loaded {
                    checkout_root,
                    data,
                } => build_plan_snapshot(
                    remote_subtitle_for_root(target, checkout_root),
                    Vec::new(),
                    data,
                    TaskPanelRowContext::empty(),
                ),
                RemoteTasksOutcome::NoTaskSource { checkout_root } => placeholder_snapshot(
                    checkout_root
                        .as_deref()
                        .map(|root| remote_subtitle_for_root(target, root))
                        .unwrap_or(subtitle),
                    progress_for_unavailable("no_plan_root"),
                    if checkout_root.is_some() {
                        "This remote checkout has no .plan/tasks.json.".to_string()
                    } else {
                        "This remote directory is not inside a Git checkout.".to_string()
                    },
                ),
                RemoteTasksOutcome::Error(error) => placeholder_snapshot(
                    subtitle,
                    progress_for_unavailable("load_failed"),
                    remote_tasks_error_summary(&target.host, *error),
                ),
            },
        }
    }

    fn pull_request_snapshot(&self, target: &PullRequestTarget) -> TaskPanelSnapshot {
        let subtitle = pull_request_subtitle(target);
        let key = PullRequestKey::for_target(target);
        let cache = self.pull_request_cache.borrow();
        match cache.get(&key) {
            None => placeholder_snapshot(
                subtitle,
                progress_for_unavailable("pull_request_loading"),
                "Loading branch pull requests from GitHub…".to_string(),
            ),
            Some(entry) => match &entry.verified {
                Some(data) => {
                    let mut snapshot = build_pull_request_snapshot(
                        subtitle,
                        data,
                        target.loop_runner_availability.as_ref(),
                    );
                    if target.remote_connection.is_some() {
                        for row in &mut snapshot.tasks {
                            row.loop_runner = None;
                        }
                    }
                    if entry.error.is_some() {
                        snapshot.progress = format!("{} · stale", snapshot.progress);
                        snapshot.summary = format!(
                            "{}; showing last verified PR state",
                            pull_request_error_summary(entry.error.as_deref().unwrap_or_default())
                        );
                    }
                    snapshot
                }
                None => {
                    let error = entry.error.as_deref().unwrap_or_default();
                    placeholder_snapshot(
                        subtitle,
                        progress_for_unavailable("pull_request_load_failed"),
                        pull_request_error_summary(error),
                    )
                }
            },
        }
    }

    fn remote_pull_request_snapshot(&self, target: &RemoteTaskTarget) -> TaskPanelSnapshot {
        let subtitle = format!("{} · remote Git", target.host);
        let key = remote_cache_key(target);
        let cache = self.remote_pull_request_cache.borrow();
        match cache.get(&key).map(|entry| &entry.outcome) {
            None => placeholder_snapshot(
                subtitle,
                progress_for_unavailable("pull_request_loading"),
                "Reading the remote checkout identity over SSH…".to_string(),
            ),
            Some(RemoteGitIdentityOutcome::GitHub(_)) => placeholder_snapshot(
                subtitle,
                progress_for_unavailable("pull_request_loading"),
                "Loading branch pull requests from GitHub with local gh…".to_string(),
            ),
            Some(RemoteGitIdentityOutcome::NoRepository) => placeholder_snapshot(
                subtitle,
                "No Git checkout".to_string(),
                "The remote pane cwd is not inside a Git checkout.".to_string(),
            ),
            Some(RemoteGitIdentityOutcome::DetachedHead {
                checkout_root,
                short_commit,
            }) => placeholder_snapshot(
                format!(
                    "{}:{} · {short_commit}",
                    target.host,
                    path_basename(Path::new(checkout_root))
                        .unwrap_or_else(|| checkout_root.clone())
                ),
                "Detached HEAD".to_string(),
                "Check out a named branch before discovering pull requests.".to_string(),
            ),
            Some(RemoteGitIdentityOutcome::UnsupportedProvider) => placeholder_snapshot(
                subtitle,
                "Unsupported Git provider".to_string(),
                "This remote checkout's origin is not a supported GitHub remote.".to_string(),
            ),
            Some(RemoteGitIdentityOutcome::AmbiguousRemotes) => placeholder_snapshot(
                subtitle,
                "Ambiguous Git remotes".to_string(),
                "Set an unambiguous GitHub origin and upstream (or parent) remote.".to_string(),
            ),
            Some(RemoteGitIdentityOutcome::Error(error)) => placeholder_snapshot(
                subtitle,
                "Remote Git unavailable".to_string(),
                remote_git_identity_error_summary(&target.host, *error),
            ),
        }
    }

    fn snapshot_task_panel(&self, state: &Rc<RefCell<AppState>>) -> TaskPanelSnapshot {
        let st = state.borrow();
        let action_labels = active_task_buttons(&st)
            .into_iter()
            .map(|button| button.label)
            .collect::<Vec<_>>();
        drop(st);

        let Some(request) = local_task_probe_request(state) else {
            return TaskPanelSnapshot {
                action_labels,
                ..placeholder_snapshot(
                    "No .plan/tasks.json".to_string(),
                    progress_for_unavailable("no_plan_root"),
                    "Open a repo-backed workspace with .plan/tasks.json to see tasks here."
                        .to_string(),
                )
            };
        };

        let Some(result) = self
            .local_task_cache
            .borrow()
            .get(&request.key)
            .map(|entry| entry.result.clone())
        else {
            return TaskPanelSnapshot {
                action_labels,
                ..placeholder_snapshot(
                    "Loading .plan/tasks.json".to_string(),
                    progress_for_unavailable("loading"),
                    "Loading tasks without blocking the terminal UI…".to_string(),
                )
            };
        };

        let subtitle = result
            .plan_root
            .as_deref()
            .and_then(path_basename)
            .map(|name| format!("{name} · .plan/tasks.json"))
            .unwrap_or_else(|| "No .plan/tasks.json".to_string());
        let Some(plan_root) = result.plan_root.as_ref() else {
            return TaskPanelSnapshot {
                action_labels,
                ..placeholder_snapshot(
                    subtitle,
                    progress_for_unavailable("no_plan_root"),
                    "Open a repo-backed workspace with .plan/tasks.json to see tasks here."
                        .to_string(),
                )
            };
        };
        let Some(projection) = result.projection else {
            return TaskPanelSnapshot {
                action_labels,
                ..placeholder_snapshot(
                    subtitle,
                    progress_for_unavailable("load_failed"),
                    result
                        .load_error
                        .unwrap_or_else(|| "Could not read .plan/tasks.json".to_string()),
                )
            };
        };

        build_plan_snapshot_with_plan(
            subtitle,
            action_labels,
            &projection.data,
            &projection.plan,
            TaskPanelRowContext {
                markdown_root: Some(plan_root),
                markdown_paths: Some(&result.markdown_paths),
                loop_runner: result.availability.as_ref(),
                binding_context: result.binding_context.as_ref(),
            },
        )
    }
}

#[cfg(test)]
fn active_current_task_summary(state: &AppState) -> Option<CurrentTaskSummary> {
    let tab = state.active_tab()?;
    let pane_id = tab.focused_pane_id;
    let status = if let Some(pane) = state.headless_pane(tab.id, pane_id) {
        crate::task_binding::current_task_status(
            pane.location_state.cwd.as_deref(),
            pane.location_state.cwd_host.as_deref(),
            pane.process_state.remote_shell,
            pane.current_task.as_ref(),
        )
    } else if let Some(leaf) = tab.panes.leaf(pane_id) {
        crate::task_binding::current_task_status(
            leaf.location_state.cwd.as_deref(),
            leaf.location_state.cwd_host.as_deref(),
            leaf.process_state.remote_shell,
            leaf.current_task.as_ref(),
        )
    } else {
        state
            .pending_tab_restores
            .get(&tab.id)
            .and_then(|pending| {
                crate::terminal::plan_restored_spawns(
                    &pending.saved,
                    crate::config::should_auto_resume_agents_on_session_restore(),
                )
                .into_iter()
                .find(|pane| pane.pane_id == pane_id)
            })
            .and_then(|pane| {
                crate::task_binding::current_task_status(
                    pane.cwd.as_deref(),
                    pane.cwd_host.as_deref(),
                    pane.remote_shell,
                    pane.current_task.as_ref(),
                )
            })
    }?;

    Some(CurrentTaskSummary {
        tab_id: tab.id,
        pane_id,
        id: status.task_id,
        title: status.title,
        resolved: status.resolved,
        pull_request: None,
        truth: None,
    })
}

/// Remote panes do not read a local `.plan`; their task binding is displayed
/// as unresolved until the remote fetch reports canonical task data.
fn remote_current_task_summary(state: &AppState) -> Option<CurrentTaskSummary> {
    let tab = state.active_tab()?;
    let pane_id = tab.focused_pane_id;
    let binding = state
        .headless_pane(tab.id, pane_id)
        .and_then(|pane| pane.current_task.as_ref())
        .or_else(|| {
            tab.panes
                .leaf(pane_id)
                .and_then(|leaf| leaf.current_task.as_ref())
        })?;
    Some(CurrentTaskSummary {
        tab_id: tab.id,
        pane_id,
        id: binding.task_id.clone(),
        title: binding.title.clone(),
        resolved: false,
        pull_request: None,
        truth: None,
    })
}

/// Resolve the active tab's focused pane to a remote task target, or `None` when
/// the focused pane is local (so the existing local file path is used unchanged).
///
/// Requires an actual ssh process in the pane. A non-empty OSC 7 host alone is
/// not sufficient: a local shell can emit the machine's own hostname (e.g.
/// `file://your-linux-host/…`), which would otherwise misclassify a local pane as remote
/// and trigger a doomed SSH-to-self. The recorded ssh argv is also the reliable
/// fetch target, so gating on it avoids the guessed `ssh <host>` fallback.
fn remote_task_target(state: &Rc<RefCell<AppState>>) -> Option<RemoteTaskTarget> {
    let st = state.borrow();
    let tab = st.active_tab()?;
    let leaf = tab.panes.leaf(tab.focused_pane_id)?;
    let ssh_argv = leaf.ssh_command()?;
    let host = leaf
        .location_state
        .cwd_host
        .as_ref()
        .filter(|host| !host.trim().is_empty())?
        .clone();
    let cwd = leaf
        .location_state
        .cwd
        .as_ref()
        .filter(|cwd| !cwd.trim().is_empty())?
        .clone();
    Some(RemoteTaskTarget {
        host,
        cwd,
        ssh_argv: Some(ssh_argv),
    })
}

/// Subtitle for a remote target: `host:dir · .plan/tasks.json`, so it is obvious
/// the panel is showing a remote machine's tasks.
fn remote_subtitle(target: &RemoteTaskTarget) -> String {
    let dir = path_basename(Path::new(&target.cwd)).unwrap_or_else(|| target.cwd.clone());
    format!("{}:{dir} · .plan/tasks.json", target.host)
}

fn remote_subtitle_for_root(target: &RemoteTaskTarget, checkout_root: &str) -> String {
    let dir = path_basename(Path::new(checkout_root)).unwrap_or_else(|| checkout_root.to_string());
    format!("{}:{dir} · .plan/tasks.json", target.host)
}

fn remote_tasks_error_summary(host: &str, error: RemoteTasksError) -> String {
    let detail = match error {
        RemoteTasksError::Transport => "SSH transport failed",
        RemoteTasksError::Authentication => "SSH authentication failed",
        RemoteTasksError::Permission => "permission or ownership policy blocked task discovery",
        RemoteTasksError::RootDiscovery => "couldn't resolve the remote Git checkout",
        RemoteTasksError::MalformedJson => ".plan/tasks.json contains malformed JSON",
        RemoteTasksError::Schema => ".plan/tasks.json does not match the task schema",
        RemoteTasksError::Protocol => "remote task response was not understood",
    };
    format!("Remote host {host} — {detail}")
}

fn remote_git_identity_error_summary(host: &str, error: RemoteGitIdentityError) -> String {
    let detail = match error {
        RemoteGitIdentityError::Transport => "SSH transport failed",
        RemoteGitIdentityError::Authentication => "SSH authentication failed",
        RemoteGitIdentityError::Permission => {
            "Git permission or ownership policy blocked discovery"
        }
        RemoteGitIdentityError::RootDiscovery => "couldn't resolve the remote Git checkout",
        RemoteGitIdentityError::Protocol => "remote Git identity response was not understood",
    };
    format!("Remote host {host} — {detail}")
}

fn remote_pull_request_discovery_feedback(outcome: &RemoteGitIdentityOutcome) -> String {
    match outcome {
        RemoteGitIdentityOutcome::GitHub(_) => {
            "Tasks discovered; loading pull requests with local gh".to_string()
        }
        RemoteGitIdentityOutcome::NoRepository => {
            "Tasks discovered; the remote cwd is not inside a Git checkout".to_string()
        }
        RemoteGitIdentityOutcome::DetachedHead { .. } => {
            "Tasks discovered; check out a remote branch to refresh pull requests".to_string()
        }
        RemoteGitIdentityOutcome::UnsupportedProvider => {
            "Tasks discovered; the remote checkout is not GitHub-backed".to_string()
        }
        RemoteGitIdentityOutcome::AmbiguousRemotes => {
            "Tasks discovered; the remote checkout has ambiguous Git remotes".to_string()
        }
        RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Authentication) => {
            "Tasks discovered; SSH authentication failed while reading remote Git identity"
                .to_string()
        }
        RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Transport) => {
            "Tasks discovered; SSH failed while reading remote Git identity".to_string()
        }
        RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Permission) => {
            "Tasks discovered; remote Git permissions blocked pull-request discovery".to_string()
        }
        RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::RootDiscovery) => {
            "Tasks discovered; the remote Git checkout could not be resolved".to_string()
        }
        RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Protocol) => {
            "Tasks discovered; the remote Git identity response was not understood".to_string()
        }
    }
}

fn pull_request_subtitle(target: &PullRequestTarget) -> String {
    let repo = path_basename(&target.checkout_root)
        .unwrap_or_else(|| target.checkout_root.display().to_string());
    format!("{repo} · {}", target.branch)
}

fn pull_request_discovery_feedback(
    result: &Result<BranchPullRequestsData, String>,
) -> PullRequestDiscoveryFeedback {
    match result {
        Ok(data) => {
            if data.total == 0 {
                return PullRequestDiscoveryFeedback::Success(
                    "Tasks discovered; no pull requests found for this branch".to_string(),
                );
            }
            let noun = if data.total == 1 {
                "pull request"
            } else {
                "pull requests"
            };
            PullRequestDiscoveryFeedback::Success(format!(
                "Tasks discovered; refreshed {} {noun}",
                data.total
            ))
        }
        Err(error) => {
            let normalized = error.to_ascii_lowercase();
            let message = if normalized.contains("github cli is unavailable")
                || normalized.contains("failed to run gh")
                || normalized.contains("no such file or directory")
            {
                "Tasks discovered; install GitHub CLI (gh) to refresh pull requests"
            } else if normalized.contains("gh auth login")
                || normalized.contains("not logged into")
                || normalized.contains("authentication")
                || normalized.contains("authenticate")
            {
                "Tasks discovered; run gh auth login to refresh pull requests"
            } else if normalized.contains("could not resolve to a repository")
                || normalized.contains("no git remotes")
                || normalized.contains("none of the git remotes")
            {
                "Tasks discovered; this checkout has no supported GitHub remote"
            } else {
                "Tasks discovered; pull requests could not be refreshed with gh"
            };
            PullRequestDiscoveryFeedback::Error(message.to_string())
        }
    }
}

fn pull_request_error_summary(error: &str) -> String {
    let normalized = error.to_ascii_lowercase();
    if normalized.contains("cli is unavailable") {
        "Install GitHub CLI (gh) on the controlling host".to_string()
    } else if normalized.contains("authentication") {
        "Run gh auth login on the controlling host".to_string()
    } else if normalized.contains("resolve the selected repository") {
        "GitHub could not resolve the selected repository".to_string()
    } else if normalized.contains("parse") {
        "GitHub returned an unreadable pull-request response".to_string()
    } else {
        "GitHub pull-request query failed on the controlling host".to_string()
    }
}

fn active_mode_key(
    state: &Rc<RefCell<AppState>>,
    pull_request_target: Option<&PullRequestTarget>,
    remote_target: Option<&RemoteTaskTarget>,
) -> String {
    // Keep a remote pane's mode identity stable while its asynchronous Git
    // identity probe transitions from loading to resolved. Otherwise selecting
    // Pull Requests during loading can snap back to the default mode when the
    // checkout root and branch arrive.
    if let Some(target) = remote_target {
        let key = remote_cache_key(target);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        return format!("remote:{:016x}", hasher.finish());
    }
    if let Some(target) = pull_request_target {
        if let Some(connection) = target.remote_connection.as_ref() {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            connection.hash(&mut hasher);
            target.checkout_root.hash(&mut hasher);
            target.branch.hash(&mut hasher);
            return format!("remote:{:016x}", hasher.finish());
        }
        return format!("local:{}:{}", target.checkout_root.display(), target.branch);
    }

    // Mode selection is UI state.  Do not resolve `.plan` or Git paths while
    // merely choosing a mode; asynchronous refresh owns those filesystem
    // probes.  Stable runtime origins keep selection from leaking between
    // restored/reused tabs.
    let st = state.borrow();
    st.active_ws()
        .map(|workspace| {
            let tab = st.active_tab();
            format!(
                "local:{}:{}",
                workspace.work_origin,
                tab.map(|tab| tab.work_origin.as_str()).unwrap_or("no-tab")
            )
        })
        .unwrap_or_else(|| "no-repo".to_string())
}

fn local_pull_request_probe_request(
    state: &Rc<RefCell<AppState>>,
) -> Option<LocalPullRequestProbeRequest> {
    let st = state.borrow();
    let workspace = st.active_ws()?;
    let tab = st.active_tab()?;
    let tab_id = tab.id;
    let pane_id = tab.focused_pane_id;
    let cwd = st.pane_local_cwd(tab_id, pane_id).ok().or_else(|| {
        st.pending_tab_restores
            .get(&tab_id)
            .and_then(|pending| pending.saved.local_cwd_for_pane(pane_id))
    })?;
    let binding = st.pane_task_binding(tab_id, pane_id).or_else(|| {
        st.pending_tab_restores
            .get(&tab_id)
            .and_then(|pending| pending.saved.current_task_for_pane(pane_id))
    });
    let pane_identity = crate::work_ledger::identity_for_pane(&st, tab_id, pane_id, binding)?;
    let mut candidates = Vec::new();
    if let Some(path) = tab
        .discovery_cwd
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        candidates.push(path.to_string());
    }
    candidates.extend(
        [
            workspace.working_tree_path.as_deref(),
            workspace.repo_root.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string),
    );
    candidates.sort();
    candidates.dedup();
    Some(LocalPullRequestProbeRequest {
        key: LocalPullRequestProbeKey {
            workspace_origin: workspace.work_origin.clone(),
            tab_origin: tab.work_origin.clone(),
            pane_origin: pane_identity.pane_origin.clone(),
            cwd,
            candidates,
            reporting_token: binding.map(|binding| binding.reporting_token.clone()),
        },
        binding: binding.cloned(),
        pane_identity,
    })
}

/// Blocking half of local pull-request discovery.  It is called only by the
/// bounded Git worker; state identity is captured before spawn and rechecked
/// by the GTK apply phase.
fn probe_local_pull_request_target(
    request: &LocalPullRequestProbeRequest,
) -> Option<LocalPullRequestProbeResult> {
    let status = crate::task_binding::current_task_status(
        Some(&request.key.cwd),
        None,
        false,
        request.binding.as_ref(),
    );
    let pinned = status
        .as_ref()
        .filter(|status| status.resolved)
        .and_then(|status| status.checkout_root.as_deref());
    let discovery = crate::git::discover(pinned.unwrap_or(&request.key.cwd));
    let checkout_root = PathBuf::from(discovery.workspace_root()?);
    let branch = discovery
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let repositories = crate::git::github_repository_identities(&checkout_root)?;
    let bound_task = status.filter(|status| status.resolved).and_then(|status| {
        let binding = request.binding.as_ref()?;
        Some(PullRequestBoundTask {
            task_id: status.task_id,
            title: status.title,
            checkout_root: status.checkout_root?,
            reporting_token: binding.reporting_token.clone(),
        })
    });
    let loop_runner_availability = loop_runner_availability(Some(&checkout_root));
    Some(LocalPullRequestProbeResult {
        checkout_root,
        branch,
        repositories,
        bound_task,
        loop_runner_availability,
    })
}

fn local_task_probe_request(state: &Rc<RefCell<AppState>>) -> Option<LocalTaskProbeRequest> {
    let st = state.borrow();
    let workspace = st.active_ws()?;
    let tab = st.active_tab()?;
    let pane_id = tab.focused_pane_id;
    let leaf = tab.panes.leaf(pane_id);
    let (cwd, cwd_host, remote_shell, binding, pane_origin) =
        if let Some(pane) = st.headless_pane(tab.id, pane_id) {
            (
                pane.location_state.cwd.clone(),
                pane.location_state.cwd_host.clone(),
                pane.process_state.remote_shell,
                pane.current_task.clone(),
                pane.work_origin.clone(),
            )
        } else {
            let leaf = leaf?;
            (
                leaf.location_state.cwd.clone(),
                leaf.location_state.cwd_host.clone(),
                leaf.process_state.remote_shell,
                leaf.current_task.clone(),
                leaf.work_origin.clone(),
            )
        };
    let mut candidates = Vec::new();
    if let Some(path) = tab
        .discovery_cwd
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        candidates.push(path.to_string());
    }
    candidates.extend(
        [
            workspace.working_tree_path.as_deref(),
            workspace.repo_root.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string),
    );
    candidates.sort();
    candidates.dedup();
    Some(LocalTaskProbeRequest {
        key: LocalTaskProbeKey {
            workspace_origin: workspace.work_origin.clone(),
            tab_origin: tab.work_origin.clone(),
            pane_origin,
            tab_id: tab.id,
            pane_id,
            candidates,
            cwd: cwd.clone(),
            cwd_host: cwd_host.clone(),
            remote_shell,
            binding: binding.clone(),
        },
        tab_id: tab.id,
        pane_id,
        cwd,
        cwd_host,
        remote_shell,
        binding,
    })
}

/// The gate key carries the complete worker request identity.  This is kept
/// separate from the cache key only to name the worker in diagnostics; both
/// derive from the same `LocalTaskProbeKey`, so a changed cwd, host, remote
/// state, pane, or binding can never coalesce with an older probe.
fn local_task_probe_worker_key(key: &LocalTaskProbeKey) -> String {
    format!("task-panel-local-tasks:{key:?}")
}

fn local_task_probe_is_current(state: &Rc<RefCell<AppState>>, key: &LocalTaskProbeKey) -> bool {
    local_task_probe_request(state).is_some_and(|current| current.key == *key)
}

fn local_pull_request_cache_entry_is_fresh(
    entry: &LocalPullRequestCacheEntry,
    observed_at_ms: u64,
) -> bool {
    observed_at_ms.saturating_sub(entry.fetched_at_ms) < LOCAL_PULL_REQUEST_TTL_MS
}

fn local_pull_request_cache_result_for_request(
    cache: &HashMap<LocalPullRequestProbeKey, LocalPullRequestCacheEntry>,
    request: &LocalPullRequestProbeRequest,
    observed_at_ms: u64,
) -> Option<LocalPullRequestProbeResult> {
    cache
        .get(&request.key)
        .filter(|entry| local_pull_request_cache_entry_is_fresh(entry, observed_at_ms))
        .and_then(|entry| entry.result.clone())
}

/// Filesystem half of a local task-panel refresh.  This runs on the shared,
/// bounded worker gate, so the GTK refresh only consumes the result.
fn probe_local_task_snapshot(request: &LocalTaskProbeRequest) -> LocalTaskProbeResult {
    let plan_root = request
        .key
        .candidates
        .iter()
        .map(PathBuf::from)
        .find(|candidate| candidate.join(".plan/tasks.json").exists());
    let (projection, load_error) = match plan_root.as_ref() {
        Some(root) => match PlanTasksData::load(root) {
            Some(data) => (
                Some(PlanTaskProjection {
                    plan: data.parallelization_plan(),
                    data,
                }),
                None,
            ),
            None => (None, Some("Could not read .plan/tasks.json".to_string())),
        },
        None => (None, None),
    };
    let status = crate::task_binding::current_task_status(
        request.cwd.as_deref(),
        request.cwd_host.as_deref(),
        request.remote_shell,
        request.binding.as_ref(),
    );
    let binding_context = match (plan_root.as_ref(), status.as_ref()) {
        (Some(plan_root), Some(status)) if status.resolved => status
            .checkout_root
            .as_deref()
            .map(PathBuf::from)
            .filter(|checkout_root| checkout_root == plan_root)
            .map(|_| TaskBindingContext {
                tab_id: request.tab_id,
                pane_id: request.pane_id,
                current_task_id: Some(status.task_id.clone()),
            }),
        _ => None,
    };
    let current_task = status.map(|status| CurrentTaskSummary {
        tab_id: request.tab_id,
        pane_id: request.pane_id,
        id: status.task_id,
        title: status.title,
        resolved: status.resolved,
        pull_request: None,
        truth: None,
    });
    let markdown_paths = plan_root
        .as_ref()
        .zip(projection.as_ref())
        .map(|(root, projection)| {
            projection
                .data
                .tasks
                .iter()
                .filter_map(|task| task_markdown_path(root, &task.id))
                .filter(|path| path.exists())
                .collect()
        })
        .unwrap_or_default();
    let availability = loop_runner_availability(plan_root.as_deref());
    LocalTaskProbeResult {
        plan_root,
        projection,
        markdown_paths,
        load_error,
        binding_context,
        current_task,
        availability,
    }
}

#[cfg(test)]
fn pull_request_target_for_tab(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
) -> Option<PullRequestTarget> {
    let st = state.borrow();
    let (_, tab) = st.find_tab(tab_id)?;
    let pane_id = tab.focused_pane_id;
    let cwd = st.pane_local_cwd(tab_id, pane_id).ok().or_else(|| {
        st.pending_tab_restores
            .get(&tab_id)
            .and_then(|pending| pending.saved.local_cwd_for_pane(pane_id))
    })?;
    let binding = st.pane_task_binding(tab_id, pane_id).or_else(|| {
        st.pending_tab_restores
            .get(&tab_id)
            .and_then(|pending| pending.saved.current_task_for_pane(pane_id))
    });
    let status = crate::task_binding::current_task_status(Some(&cwd), None, false, binding);
    let pinned = status
        .as_ref()
        .filter(|status| status.resolved)
        .and_then(|status| status.checkout_root.as_deref());
    let discovery = crate::git::discover(pinned.unwrap_or(&cwd));
    let checkout_root = PathBuf::from(discovery.workspace_root()?);
    let branch = discovery
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let repositories = crate::git::github_repository_identities(&checkout_root)?;
    let head_owner = repositories.head.split_once('/')?.0.to_string();
    let pane_identity = crate::work_ledger::identity_for_pane(&st, tab_id, pane_id, binding)?;
    let bound_task = status.filter(|status| status.resolved).and_then(|status| {
        let binding = binding?;
        Some(PullRequestBoundTask {
            task_id: status.task_id,
            title: status.title,
            checkout_root: status.checkout_root?,
            reporting_token: binding.reporting_token.clone(),
        })
    });
    Some(PullRequestTarget {
        checkout_root,
        branch,
        head_repository: repositories.head,
        base_repository: repositories.base,
        head_owner,
        remote_connection: None,
        pane_identity,
        bound_task,
        loop_runner_availability: None,
    })
}

fn verify_pull_request_bound_task(bound: PullRequestBoundTask) -> Option<PullRequestBoundTask> {
    crate::task_binding::resolve_task_from_pane_cwd(&bound.checkout_root, &bound.task_id)
        .is_ok_and(|resolved| {
            resolved.task_id == bound.task_id
                && resolved.title == bound.title
                && resolved.checkout_root.as_deref() == Some(bound.checkout_root.as_str())
        })
        .then_some(bound)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Assemble a [`TaskPanelSnapshot`] from parsed [`PlanTasksData`]. Shared by the
/// local file path and the remote fetch. `markdown_root` enables per-task "open"
/// links only when the `.plan/tasks/*.md` files are reachable on the local disk
/// (i.e. `None` for remote panes).
#[derive(Clone, Copy)]
struct TaskPanelRowContext<'a> {
    markdown_root: Option<&'a Path>,
    markdown_paths: Option<&'a HashSet<PathBuf>>,
    loop_runner: Option<&'a LoopRunnerAvailability>,
    binding_context: Option<&'a TaskBindingContext>,
}

impl<'a> TaskPanelRowContext<'a> {
    fn empty() -> Self {
        Self {
            markdown_root: None,
            markdown_paths: None,
            loop_runner: None,
            binding_context: None,
        }
    }
}

fn build_plan_snapshot(
    subtitle: String,
    action_labels: Vec<String>,
    data: &PlanTasksData,
    row_context: TaskPanelRowContext<'_>,
) -> TaskPanelSnapshot {
    let plan = data.parallelization_plan();
    build_plan_snapshot_with_plan(subtitle, action_labels, data, &plan, row_context)
}

fn build_plan_snapshot_with_plan(
    subtitle: String,
    action_labels: Vec<String>,
    data: &PlanTasksData,
    plan: &ParallelizationPlan,
    row_context: TaskPanelRowContext<'_>,
) -> TaskPanelSnapshot {
    let progress = aggregate_progress_text(data);

    let next_task = data
        .tasks
        .iter()
        .find(|task| !task.is_done() && !task.is_blocked())
        .map(|task| task.id.clone())
        .or_else(|| {
            data.tasks
                .iter()
                .find(|task| !task.is_done())
                .map(|task| task.id.clone())
        });

    let summary = match next_task.as_deref() {
        Some(task_id) => format!(
            "Next: {task_id} · {} ready · {} active · {} blocked",
            data.ready, data.in_progress, data.blocked
        ),
        None => format!("All {} tasks are done", data.total),
    };

    let parallelization_header = parallelization_header_text(plan);
    let blockers: HashMap<String, Vec<String>> = plan.blockers.iter().cloned().collect();

    let tasks = visible_task_rows(
        data,
        row_context.markdown_root,
        row_context.markdown_paths,
        next_task.as_deref(),
        row_context.loop_runner,
        row_context.binding_context,
        &blockers,
    );

    TaskPanelSnapshot {
        subtitle,
        progress,
        summary,
        parallelization_header,
        tasks,
        action_labels,
        current_task: None,
    }
}

/// "Ready now — N can run in parallel" header, or `None` when nothing is ready.
fn parallelization_header_text(plan: &ParallelizationPlan) -> Option<String> {
    match plan.ready_now.len() {
        0 => None,
        1 => Some("Ready now — 1 task can run".to_string()),
        n => Some(format!("Ready now — {n} can run in parallel")),
    }
}

fn build_pull_request_snapshot(
    subtitle: String,
    data: &BranchPullRequestsData,
    availability: Option<&LoopRunnerAvailability>,
) -> TaskPanelSnapshot {
    let progress = if data.total == 0 {
        "No PRs for this branch".to_string()
    } else {
        format!("{} open · {} draft", data.open, data.draft)
    };
    let summary = if data.total == 0 {
        "No pull requests found for this branch".to_string()
    } else {
        format!(
            "Branch PRs: {} merged · {} closed",
            data.merged, data.closed
        )
    };
    let tasks = data
        .pull_requests
        .iter()
        .map(|pr| pull_request_row_from_entry(pr, data, availability))
        .collect();

    TaskPanelSnapshot {
        subtitle,
        progress,
        summary,
        parallelization_header: None,
        tasks,
        action_labels: Vec::new(),
        current_task: None,
    }
}

/// A snapshot with no task rows and no actions — used for every loading/error
/// placeholder state so the `parallelization_header` default lives in one place.
fn placeholder_snapshot(subtitle: String, progress: String, summary: String) -> TaskPanelSnapshot {
    TaskPanelSnapshot {
        subtitle,
        progress,
        summary,
        parallelization_header: None,
        tasks: Vec::new(),
        action_labels: Vec::new(),
        current_task: None,
    }
}

fn unavailable_pull_request_snapshot() -> TaskPanelSnapshot {
    placeholder_snapshot(
        "No git branch".to_string(),
        progress_for_unavailable("no_pull_request_target"),
        "Open a local GitHub-backed branch to see pull requests here.".to_string(),
    )
}

/// Format the aggregate "X of Y done" header from `PlanTasksData`.
///
/// Returns a stable, human-readable string for any combination of done/total
/// values so the panel header matches the rest of the snapshot. Tested in
/// `aggregate_progress_text_reports_done_and_total`.
fn aggregate_progress_text(data: &PlanTasksData) -> String {
    if data.total == 0 {
        return "No tasks tracked".to_string();
    }
    if data.done >= data.total {
        return format!("All {} of {} done", data.total, data.total);
    }
    format!("{} of {} done", data.done, data.total)
}

/// Header text used when the panel has no `PlanTasksData` to summarise.
///
/// `reason` is matched against the documented reasons so we can surface a
/// different placeholder for "no workspace / plan root" vs. "data failed to
/// load". Unknown reasons fall back to a generic placeholder.
fn progress_for_unavailable(reason: &str) -> String {
    match reason {
        "no_plan_root" => "No progress data".to_string(),
        "load_failed" => "Progress unavailable".to_string(),
        "loading" => "Fetching remote tasks…".to_string(),
        "pull_request_loading" => "Fetching branch PRs…".to_string(),
        "pull_request_load_failed" => "PRs unavailable".to_string(),
        "no_pull_request_target" => "No PR data".to_string(),
        _ => "No progress data".to_string(),
    }
}

fn visible_task_rows(
    data: &PlanTasksData,
    markdown_root: Option<&Path>,
    markdown_paths: Option<&HashSet<PathBuf>>,
    next_task_id: Option<&str>,
    loop_runner: Option<&LoopRunnerAvailability>,
    binding_context: Option<&TaskBindingContext>,
    blockers: &HashMap<String, Vec<String>>,
) -> Vec<TaskRow> {
    data.tasks
        .iter()
        .filter(|task| !task.is_done())
        .map(|task| {
            task_row_from_entry(
                task,
                markdown_root,
                markdown_paths,
                next_task_id,
                loop_runner,
                binding_context,
                blockers.get(&task.id).map(Vec::as_slice).unwrap_or(&[]),
            )
        })
        .collect()
}

fn task_row_from_entry(
    task: &PlanTaskEntry,
    markdown_root: Option<&Path>,
    markdown_paths: Option<&HashSet<PathBuf>>,
    next_task_id: Option<&str>,
    loop_runner: Option<&LoopRunnerAvailability>,
    binding_context: Option<&TaskBindingContext>,
    blocker_ids: &[String],
) -> TaskRow {
    // Prefer naming the actual unmet blockers; fall back to a count when the
    // graph only gave us a `blocked_by` total (e.g. blocked_by, not depends_on).
    let note = if !blocker_ids.is_empty() {
        Some(format!("blocked by {}", blocker_ids.join(", ")))
    } else if task.is_blocked() {
        Some(format!("blocked by {}", task.blocked_by))
    } else if task.depends_on > 0 {
        Some(format!("depends on {}", task.depends_on))
    } else {
        None
    };

    let markdown_path = markdown_root
        .and_then(|root| task_markdown_path(root, &task.id))
        .filter(|path| markdown_paths.is_some_and(|paths| paths.contains(path)));

    let loop_runner_info = loop_runner.map(|availability| LoopRunnerRowInfo {
        checkout_root: availability.checkout_root.clone(),
        kind: LoopRunnerRowKind::Task {
            ready: task.is_ready(),
            blockers: blocker_ids.to_vec(),
        },
    });

    TaskRow {
        id: task.id.clone(),
        title: task.title.clone(),
        status: task.status.clone(),
        priority: task.priority.clone(),
        note,
        is_next: next_task_id == Some(task.id.as_str()),
        markdown_path,
        external_url: None,
        loop_runner: loop_runner_info,
        binding: binding_context.map(|context| TaskBindingRowInfo {
            tab_id: context.tab_id,
            pane_id: context.pane_id,
            is_current: context.current_task_id.as_deref() == Some(task.id.as_str()),
            truth: None,
        }),
    }
}

fn pull_request_row_from_entry(
    pr: &crate::tracking::BranchPullRequestEntry,
    data: &BranchPullRequestsData,
    loop_runner: Option<&LoopRunnerAvailability>,
) -> TaskRow {
    let branch_note = match (
        pr.head_ref_name.trim().is_empty(),
        pr.base_ref_name.trim().is_empty(),
    ) {
        (false, false) => format!("{} -> {}", pr.head_ref_name, pr.base_ref_name),
        _ => "branch unavailable".to_string(),
    };
    let link_note = match pr.correlation.as_ref() {
        Some(metadata) => {
            let matching_markers = data
                .pull_requests
                .iter()
                .filter(|candidate| {
                    candidate
                        .correlation
                        .as_ref()
                        .is_some_and(|candidate_metadata| {
                            candidate_metadata.task_id == metadata.task_id
                                && candidate_metadata.repo == metadata.repo
                                && candidate_metadata.branch == metadata.branch
                                && candidate.head_ref_name == pr.head_ref_name
                                && candidate.head_repository_owner == pr.head_repository_owner
                        })
                })
                .count();
            if matching_markers == 1 {
                format!("linked: {}", metadata.task_id)
            } else {
                "unlinked (ambiguous marker)".to_string()
            }
        }
        None if pr.correlation_marker_present => "unlinked (invalid marker)".to_string(),
        None => "unlinked".to_string(),
    };
    let note = Some(format!("{link_note} · {branch_note}"));

    // Only offer review actions for PRs that are still open (not merged/closed).
    let open = pr.state == "open";
    let loop_runner_info = loop_runner.map(|availability| LoopRunnerRowInfo {
        checkout_root: availability.checkout_root.clone(),
        kind: LoopRunnerRowKind::PullRequest {
            number: pr.number,
            open,
        },
    });

    TaskRow {
        id: format!("#{}", pr.number),
        title: pr.title.clone(),
        status: pr.status_label(),
        priority: None,
        note,
        is_next: false,
        markdown_path: None,
        external_url: pr.url.clone(),
        loop_runner: loop_runner_info,
        binding: None,
    }
}

/// How long to wait for a loop-runner result file to appear before giving up on
/// the result toast. Agent runs are long, so this is generous; the live tab keeps
/// showing progress regardless of this timeout.
const LOOP_RUNNER_RESULT_TIMEOUT_SECS: u64 = 60 * 90;
/// How often the background poller checks for the result file.
const LOOP_RUNNER_RESULT_POLL_MS: u64 = 1_000;

/// GTK handles needed to dispatch a loop-runner run from a row button: spawn the
/// live tab, then poll its result file and toast/refresh on the main thread.
#[derive(Clone)]
struct RowDispatchContext {
    panel: TaskPanel,
    state: Rc<RefCell<AppState>>,
    tab_list: gtk::Box,
    term_stack: gtk::Stack,
    window: adw::ApplicationWindow,
}

/// Whether a parsed result is a dev-loop (task) or pr-loop result, so the right
/// status→toast mapping is used when the file is read.
#[derive(Clone, Copy)]
enum DispatchResultKind {
    DevLoop,
    PrLoop,
}

fn bind_pane_task_mutation(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
    task_id: &str,
) -> Result<crate::task_binding::PaneTaskBinding, crate::task_binding::PaneTaskBindingError> {
    let mut state = state.borrow_mut();
    state.bind_pane_to_task(tab_id, pane_id, task_id)
}

fn clear_pane_task_mutation(state: &Rc<RefCell<AppState>>, tab_id: u32, pane_id: u32) -> bool {
    let mut state = state.borrow_mut();
    state.clear_pane_task_binding(tab_id, pane_id)
}

fn schedule_task_panel_refresh(
    panel: &TaskPanel,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    let panel = panel.clone();
    let state = state.clone();
    let tab_list = tab_list.clone();
    let term_stack = term_stack.clone();
    let window = window.clone();
    glib::idle_add_local_once(move || {
        panel.last_snapshot.borrow_mut().take();
        panel.refresh(&state, &tab_list, &term_stack, &window);
    });
}

impl RowDispatchContext {
    fn schedule_refresh_rows(&self) {
        schedule_task_panel_refresh(
            &self.panel,
            &self.state,
            &self.tab_list,
            &self.term_stack,
            &self.window,
        );
    }

    fn bind_task(&self, tab_id: u32, pane_id: u32, task_id: &str) {
        let result = bind_pane_task_mutation(&self.state, tab_id, pane_id, task_id);
        match result {
            Ok(binding) => {
                crate::show_toast(&format!(
                    "Current task: {} - {}",
                    binding.task_id, binding.title
                ));
                self.schedule_refresh_rows();
            }
            Err(error) => {
                crate::show_error_toast(&format!("Could not bind task: {error:?}"));
            }
        }
    }

    fn clear_task(&self, tab_id: u32, pane_id: u32) {
        let cleared = clear_pane_task_mutation(&self.state, tab_id, pane_id);
        if cleared {
            crate::show_toast("Cleared current task");
            self.schedule_refresh_rows();
        }
    }

    /// Confirm, then dispatch a `dev-loop` build for `task_id`.
    fn confirm_and_dispatch_task(&self, checkout_root: &Path, task_id: &str) {
        // The id comes from `.plan/tasks.json` (agent-writable, sometimes synced),
        // so it is not trusted. Even though the command builder shell-quotes every
        // value, reject ids with unsafe characters up front for a clear error.
        if !crate::loop_runner::is_safe_issue_id(task_id) {
            crate::show_error_toast(&format!(
                "Refusing to dispatch task id with unsafe characters: {task_id}"
            ));
            return;
        }
        let config = crate::config::loop_runner_config();
        let command = crate::loop_runner::dev_command_line(
            &config.dev_command,
            &display_path(checkout_root),
            task_id,
            "dev",
        );
        if crate::loop_runner::split_command(&command).is_none() {
            crate::show_error_toast("Loop runner dev_command is empty");
            return;
        }
        let title = format!("Build {task_id} with loop runner");
        let body = format!("Run this command in a new tab?\n\n{command}");
        let this = self.clone();
        let task_id = task_id.to_string();
        let checkout_root = checkout_root.to_path_buf();
        self.confirm(&title, &body, move || {
            this.dispatch(
                &task_id,
                "task",
                &command,
                DispatchResultKind::DevLoop,
                Some((checkout_root.as_path(), task_id.as_str())),
            );
        });
    }

    /// Dispatch a `pr-loop` review/merge for `pr_number`. The merge-capable action
    /// always confirms first; "Review only" runs immediately (it never merges).
    fn dispatch_pr(
        &self,
        checkout_root: &Path,
        pr_number: u64,
        action: crate::loop_runner::LoopRunnerAction,
    ) {
        let config = crate::config::loop_runner_config();
        let loop_name =
            crate::loop_runner::pr_loop_name(action, &config.review_loop, &config.merge_loop);
        if loop_name.trim().is_empty() {
            crate::show_error_toast(
                "No loop configured for this action — set [loop_runner].review_loop / merge_loop",
            );
            return;
        }
        let pr = pr_number.to_string();
        let command = crate::loop_runner::pr_command_line(
            &config.pr_command,
            &display_path(checkout_root),
            &pr,
            loop_name,
        );
        if crate::loop_runner::split_command(&command).is_none() {
            crate::show_error_toast("Loop runner pr_command is empty");
            return;
        }
        let result_id = format!("pr-{pr_number}");
        let kind = DispatchResultKind::PrLoop;

        if matches!(action, crate::loop_runner::LoopRunnerAction::ReviewAndMerge) {
            let title = format!("Review & merge PR #{pr_number}");
            let body = format!(
                "This may merge the PR if the review passes and checks are green.\n\nRun this command in a new tab?\n\n{command}"
            );
            let this = self.clone();
            self.confirm(&title, &body, move || {
                this.dispatch(&result_id, "pr", &command, kind, None);
            });
        } else {
            self.dispatch(&result_id, "pr", &command, kind, None);
        }
    }

    /// Spawn the live tab and arrange to read the result file off the main thread.
    fn dispatch(
        &self,
        result_id: &str,
        kind_tag: &str,
        command: &str,
        kind: DispatchResultKind,
        bind_task: Option<(&Path, &str)>,
    ) {
        let Some(runtime_dir) = xdg_runtime_dir() else {
            crate::show_error_toast(
                "XDG_RUNTIME_DIR is not set — cannot capture loop runner result",
            );
            return;
        };
        let result_path = crate::loop_runner::result_file_path(&runtime_dir, kind_tag, result_id);
        let result_path_str = result_path.to_string_lossy().into_owned();
        let context_path = bind_task
            .map(|_| crate::work_reporting::context_file_path(Path::new(&runtime_dir), result_id));
        if let Some(path) = context_path.as_ref() {
            let _ = std::fs::remove_file(path);
        }
        let argv = match context_path.as_ref() {
            Some(path) => crate::loop_runner::task_bound_live_tab_argv(
                command,
                &result_path_str,
                &path.to_string_lossy(),
            ),
            None => crate::loop_runner::live_tab_argv(command, &result_path_str),
        };

        let tab_name = format!("loop: {result_id}");
        // Group loop runs in the dedicated "Loops" workspace (split panes, spilling
        // to a new tab when full) instead of a fresh top-level tab per dispatch.
        let working_dir = bind_task.map(|(checkout_root, _)| display_path(checkout_root));
        let destination = crate::palette::dispatch_loop_command(
            &self.state,
            &self.tab_list,
            &self.term_stack,
            &self.window,
            &tab_name,
            working_dir.as_deref(),
            argv,
        );
        let Some((tab_id, pane_id)) = destination else {
            if let Some(path) = context_path.as_ref() {
                let _ = std::fs::remove_file(path);
            }
            crate::show_error_toast(&format!(
                "Could not create a loop-runner pane for {result_id}"
            ));
            return;
        };
        if let Some((checkout_root, task_id)) = bind_task {
            if let Err(error) = self.state.borrow_mut().bind_pane_to_task_at_checkout(
                tab_id,
                pane_id,
                checkout_root,
                task_id,
            ) {
                if let Some(path) = context_path.as_ref() {
                    let _ = std::fs::remove_file(path);
                }
                crate::show_error_toast(&format!("Loop pane task binding failed: {error:?}"));
                return;
            }
            let context = match crate::work_reporting::context_for_pane(
                &self.state.borrow(),
                tab_id,
                pane_id,
            ) {
                Ok(context) => context,
                Err(error) => {
                    if let Some(path) = context_path.as_ref() {
                        let _ = std::fs::remove_file(path);
                    }
                    crate::show_error_toast(&format!(
                        "Loop pane reporting context failed: {error}"
                    ));
                    return;
                }
            };
            let Some(path) = context_path.as_ref() else {
                crate::show_error_toast("Loop pane reporting context path is missing");
                return;
            };
            if let Err(error) = crate::work_reporting::write_context_file(path, &context) {
                let _ = std::fs::remove_file(path);
                crate::show_error_toast(&format!(
                    "Loop pane reporting context could not be written: {error}"
                ));
                return;
            }
        }
        crate::show_toast(&format!("Dispatched loop runner for {result_id}"));

        self.watch_result(result_path, kind);
    }

    /// Poll the result file off the GTK main thread; when it appears, parse it and
    /// toast the outcome, then refresh the panel so a new PR/state shows up.
    fn watch_result(&self, result_path: PathBuf, kind: DispatchResultKind) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            let read_path = result_path.clone();
            let contents = gio::spawn_blocking(move || poll_result_file(&read_path))
                .await
                .unwrap_or(None);

            let toast = match contents {
                Some(json) => match kind {
                    DispatchResultKind::DevLoop => crate::loop_runner::dev_result_toast(&json),
                    DispatchResultKind::PrLoop => crate::loop_runner::pr_result_toast(&json),
                },
                None => crate::loop_runner::LoopRunnerToast::Error(
                    "Loop runner did not produce a result".to_string(),
                ),
            };

            match toast {
                crate::loop_runner::LoopRunnerToast::Success(m)
                | crate::loop_runner::LoopRunnerToast::Info(m) => crate::show_toast(&m),
                crate::loop_runner::LoopRunnerToast::Error(m) => crate::show_error_toast(&m),
            }

            // Drop the stale snapshot so the refresh re-renders (e.g. a new PR).
            this.panel.last_snapshot.borrow_mut().take();
            this.panel.invalidate_local_task_cache();
            this.panel
                .refresh(&this.state, &this.tab_list, &this.term_stack, &this.window);
            let _ = std::fs::remove_file(&result_path);
        });
    }

    /// Present a modal confirmation dialog; run `on_accept` only if accepted.
    fn confirm(&self, title: &str, body: &str, on_accept: impl Fn() + 'static) {
        let dialog = gtk::Dialog::builder()
            .transient_for(&self.window)
            .modal(true)
            .title(title)
            .build();
        dialog.add_button("Cancel", gtk::ResponseType::Cancel);
        dialog.add_button("Run", gtk::ResponseType::Accept);

        let content = dialog.content_area();
        content.set_spacing(8);
        content.set_margin_top(8);
        content.set_margin_bottom(8);
        content.set_margin_start(8);
        content.set_margin_end(8);
        let label = gtk::Label::new(Some(body));
        label.set_wrap(true);
        label.set_halign(gtk::Align::Start);
        label.set_selectable(true);
        content.append(&label);

        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                on_accept();
            }
            dialog.close();
        });
        dialog.present();
    }
}

/// Block (on a worker thread) until the loop-runner result file has content or the
/// timeout elapses. Returns the file contents, or `None` on timeout.
fn poll_result_file(path: &Path) -> Option<String> {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(LOOP_RUNNER_RESULT_TIMEOUT_SECS);
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if !contents.trim().is_empty() {
                return Some(contents);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(LOOP_RUNNER_RESULT_POLL_MS));
    }
}

fn xdg_runtime_dir() -> Option<String> {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn build_task_row(task: &TaskRow, dispatch: &RowDispatchContext) -> gtk::ListBoxRow {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 6);
    container.add_css_class("task-panel-row");
    if task.markdown_path.is_some() || task.external_url.is_some() {
        container.add_css_class("task-panel-row-clickable");
    }

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    let id = gtk::Label::new(Some(&task.id));
    id.add_css_class("task-panel-id");
    id.set_halign(gtk::Align::Start);

    let title = gtk::Label::new(Some(&task.title));
    title.add_css_class("task-panel-task-title");
    title.set_halign(gtk::Align::Start);
    title.set_wrap(true);
    title.set_hexpand(true);

    top.append(&id);
    top.append(&title);
    if task.is_next {
        let badge = gtk::Label::new(Some("next"));
        badge.add_css_class("task-panel-badge");
        badge.add_css_class("task-panel-badge-next");
        top.append(&badge);
    }
    if task
        .binding
        .as_ref()
        .is_some_and(|binding| binding.is_current)
    {
        let badge = gtk::Label::new(Some("current"));
        badge.add_css_class("task-panel-badge");
        badge.add_css_class("task-panel-badge-next");
        top.append(&badge);
    }
    container.append(&top);

    let meta = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    meta.set_halign(gtk::Align::Start);

    let status = gtk::Label::new(Some(&task.status));
    status.add_css_class("task-panel-badge");
    status.add_css_class(task_status_class(&task.status));
    meta.append(&status);

    if let Some(priority) = task.priority.as_deref() {
        let badge = gtk::Label::new(Some(priority));
        badge.add_css_class("task-panel-badge");
        badge.add_css_class("task-panel-badge-priority");
        meta.append(&badge);
    }

    if let Some(note) = task.note.as_deref() {
        let note_label = gtk::Label::new(Some(note));
        note_label.add_css_class("task-panel-note");
        note_label.set_halign(gtk::Align::Start);
        meta.append(&note_label);
    }

    if task.markdown_path.is_some() || task.external_url.is_some() {
        let hint = gtk::Label::new(Some("open"));
        hint.add_css_class("task-panel-note");
        hint.add_css_class("task-panel-open-hint");
        hint.set_halign(gtk::Align::End);
        hint.set_hexpand(true);
        meta.append(&hint);
    }

    if let Some(path) = task.markdown_path.as_ref() {
        let tooltip_path = path.clone();
        let path_for_click = path.clone();
        let gesture = gtk::GestureClick::new();
        gesture.set_button(1);
        gesture.connect_released(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            crate::terminal::open_path_in_editor(&path_for_click);
        });
        container.add_controller(gesture);
        container.set_tooltip_text(Some(&format!("Open {}", tooltip_path.display())));
    } else if let Some(url) = task.external_url.as_ref() {
        let tooltip_url = url.clone();
        let url_for_click = url.clone();
        let gesture = gtk::GestureClick::new();
        gesture.set_button(1);
        gesture.connect_released(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            open_external_url(&url_for_click);
        });
        container.add_controller(gesture);
        container.set_tooltip_text(Some(&format!("Open {tooltip_url}")));
    }

    container.append(&meta);

    if let Some(truth) = task
        .binding
        .as_ref()
        .and_then(|binding| binding.truth.as_ref())
    {
        // Six independent truth badges cannot fit reliably in the narrow task
        // sidebar's existing horizontal metadata row. Flow them onto as many
        // lines as needed so verification and last-checked never disappear off
        // the right edge.
        let truth_badges = gtk::FlowBox::new();
        truth_badges.set_selection_mode(gtk::SelectionMode::None);
        truth_badges.set_halign(gtk::Align::Start);
        truth_badges.set_column_spacing(6);
        truth_badges.set_row_spacing(6);
        truth_badges.set_max_children_per_line(3);
        for label in task_truth_axis_labels(truth) {
            let is_mismatch = label.contains(" mismatch: ");
            let badge = gtk::Label::new(Some(&label));
            badge.add_css_class("task-panel-badge");
            badge.add_css_class("work-truth-binding");
            if is_mismatch {
                badge.add_css_class("work-truth-mismatch");
            }
            truth_badges.insert(&badge, -1);
        }
        container.append(&truth_badges);
    }

    if let Some(buttons) = build_loop_runner_buttons(task, dispatch) {
        container.append(&buttons);
    }
    if let Some(buttons) = build_task_binding_buttons(task, dispatch) {
        container.append(&buttons);
    }

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&container));
    row
}

fn build_task_binding_buttons(task: &TaskRow, dispatch: &RowDispatchContext) -> Option<gtk::Box> {
    let info = task.binding.as_ref()?;
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("task-panel-row-actions");
    row.set_halign(gtk::Align::Start);

    if info.is_current {
        let current = gtk::Button::with_label("Current");
        current.add_css_class("task-panel-action-button");
        current.set_sensitive(false);
        row.append(&current);

        let clear = gtk::Button::with_label("Clear");
        clear.add_css_class("task-panel-action-button");
        let dispatch = dispatch.clone();
        let tab_id = info.tab_id;
        let pane_id = info.pane_id;
        clear.connect_clicked(move |_| dispatch.clear_task(tab_id, pane_id));
        row.append(&clear);
    } else {
        let bind = gtk::Button::with_label("Bind");
        bind.add_css_class("task-panel-action-button");
        let dispatch = dispatch.clone();
        let tab_id = info.tab_id;
        let pane_id = info.pane_id;
        let task_id = task.id.clone();
        bind.connect_clicked(move |_| dispatch.bind_task(tab_id, pane_id, &task_id));
        row.append(&bind);
    }

    Some(row)
}

/// A non-interactive "Ready now — N can run in parallel" header row above the
/// ready tasks. Rendered as a disabled list row so it scrolls with the list.
fn build_parallelization_header_row(text: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("task-panel-section-label");
    label.set_halign(gtk::Align::Start);
    label.set_margin_top(2);
    label.set_margin_bottom(2);

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&label));
    row.set_selectable(false);
    row.set_activatable(false);
    row
}

/// Build the per-row loop-runner action buttons, or `None` when the feature is
/// unavailable for this row. Tasks get "Build with loop runner"; PRs get
/// "Review only" / "Review & merge" plus an "Open PR" affordance.
fn build_loop_runner_buttons(task: &TaskRow, dispatch: &RowDispatchContext) -> Option<gtk::Box> {
    let info = task.loop_runner.as_ref()?;
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("task-panel-row-actions");
    row.set_halign(gtk::Align::Start);

    match &info.kind {
        LoopRunnerRowKind::Task { ready, blockers } => {
            let button = gtk::Button::with_label("Build with loop runner");
            button.add_css_class("task-panel-action-button");
            if *ready {
                let dispatch = dispatch.clone();
                let checkout_root = info.checkout_root.clone();
                let task_id = task.id.clone();
                button.connect_clicked(move |_| {
                    dispatch.confirm_and_dispatch_task(&checkout_root, &task_id);
                });
            } else {
                button.set_sensitive(false);
                let reason = if blockers.is_empty() {
                    "Task is not ready to build".to_string()
                } else {
                    format!("Blocked by {}", blockers.join(", "))
                };
                button.set_tooltip_text(Some(&reason));
            }
            row.append(&button);
        }
        LoopRunnerRowKind::PullRequest { number, open } => {
            if !*open {
                // Merged/closed PRs are not actionable; show nothing.
                return None;
            }
            let review = gtk::Button::with_label("Review only");
            review.add_css_class("task-panel-action-button");
            {
                let dispatch = dispatch.clone();
                let checkout_root = info.checkout_root.clone();
                let number = *number;
                review.connect_clicked(move |_| {
                    dispatch.dispatch_pr(
                        &checkout_root,
                        number,
                        crate::loop_runner::LoopRunnerAction::ReviewOnly,
                    );
                });
            }
            row.append(&review);

            let merge = gtk::Button::with_label("Review & merge");
            merge.add_css_class("task-panel-action-button");
            {
                let dispatch = dispatch.clone();
                let checkout_root = info.checkout_root.clone();
                let number = *number;
                merge.connect_clicked(move |_| {
                    dispatch.dispatch_pr(
                        &checkout_root,
                        number,
                        crate::loop_runner::LoopRunnerAction::ReviewAndMerge,
                    );
                });
            }
            row.append(&merge);

            if let Some(url) = task.external_url.as_ref() {
                let open_pr = gtk::Button::with_label("Open PR");
                open_pr.add_css_class("task-panel-action-button");
                let url = url.clone();
                open_pr.connect_clicked(move |_| open_external_url(&url));
                row.append(&open_pr);
            }
        }
    }

    Some(row)
}

fn task_status_class(status: &str) -> &'static str {
    if matches!(status, "done" | "completed" | "closed" | "merged") {
        "task-panel-badge-done"
    } else if status == "draft" {
        "task-panel-badge-todo"
    } else if matches!(
        status,
        "in-progress" | "in_progress" | "doing" | "active" | "started" | "open"
    ) || status.starts_with("open ·")
    {
        "task-panel-badge-active"
    } else {
        "task-panel-badge-todo"
    }
}

fn task_truth_axis_labels(truth: &crate::work_ledger::TaskTruth) -> Vec<String> {
    let canonical = match truth.canonical {
        crate::work_ledger::CanonicalTaskState::Todo => "Todo",
        crate::work_ledger::CanonicalTaskState::InProgress => "In progress",
        crate::work_ledger::CanonicalTaskState::Blocked => "Blocked",
        crate::work_ledger::CanonicalTaskState::Done => "Done",
        crate::work_ledger::CanonicalTaskState::Cancelled => "Cancelled",
        crate::work_ledger::CanonicalTaskState::Absent => "Absent",
        crate::work_ledger::CanonicalTaskState::Unknown => "Unknown",
    };
    let binding = match truth.binding {
        crate::work_ledger::BindingState::Bound => "Bound",
        crate::work_ledger::BindingState::Unbound => "Unbound",
    };
    let execution = match truth.execution {
        crate::work_ledger::ExecutionState::Running => "Running",
        crate::work_ledger::ExecutionState::Idle => "Idle",
        crate::work_ledger::ExecutionState::Unknown => "Execution unknown",
    };
    let origin = match truth.origin {
        crate::work_ledger::WorkStreamOriginState::Live => "Live",
        crate::work_ledger::WorkStreamOriginState::Lazy => "Lazy",
        crate::work_ledger::WorkStreamOriginState::Historical => "Historical",
    };
    let verification = match truth.verification {
        crate::work_ledger::WorkReconciliationStatus::Pending => "Pending",
        crate::work_ledger::WorkReconciliationStatus::Verified => "Verified",
        crate::work_ledger::WorkReconciliationStatus::Stale => "Stale",
        crate::work_ledger::WorkReconciliationStatus::Unverified => "Unverified",
    };
    let checked = truth.last_checked_unix_ms.map_or_else(
        || "Never checked".to_string(),
        |checked| {
            format!(
                "Checked {}",
                crate::work_ledger::format_age(checked, crate::events::unix_time_ms())
            )
        },
    );
    let mut labels = vec![
        canonical.to_string(),
        binding.to_string(),
        execution.to_string(),
        origin.to_string(),
        format!("{verification} ({})", truth.verification_source),
        checked,
    ];
    if let Some(mismatch) = truth.mismatch.as_ref() {
        labels.push(format!("{} mismatch: {}", mismatch.source, mismatch.detail));
    }
    labels
}

fn task_truth_label(truth: &crate::work_ledger::TaskTruth) -> String {
    task_truth_axis_labels(truth).join(" · ")
}

pub(crate) fn open_external_url(url: &str) {
    crate::browser::open_url(url);
}

fn run_local_gh_command(argv: &[String]) -> Result<String, String> {
    let Some(program) = argv.first().filter(|program| program.as_str() == "gh") else {
        return Err("GitHub CLI command was not safe to run locally".to_string());
    };
    let output = Command::new(program)
        .args(&argv[1..])
        .output()
        .map_err(|_| "GitHub CLI is unavailable on the controlling host".to_string())?;
    if !output.status.success() {
        return Err(classify_local_gh_failure(&String::from_utf8_lossy(
            &output.stderr,
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn classify_local_gh_failure(stderr: &str) -> String {
    let normalized = stderr.to_ascii_lowercase();
    if normalized.contains("gh auth login")
        || normalized.contains("not logged into")
        || normalized.contains("authentication")
        || normalized.contains("authenticate")
        || normalized.contains("http 401")
    {
        "GitHub authentication is required on the controlling host".to_string()
    } else if normalized.contains("could not resolve to a repository")
        || normalized.contains("repository not found")
    {
        "GitHub could not resolve the selected repository".to_string()
    } else {
        // Never retain raw `gh` stderr: it may include request URLs, user names,
        // or credential-helper details. The actionable category is sufficient.
        "GitHub pull-request query failed on the controlling host".to_string()
    }
}

fn populate_action_buttons(
    panel: &TaskPanel,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
    action_labels: &[String],
) {
    for label in action_labels {
        let button = gtk::Button::with_label(label);
        button.add_css_class("task-panel-action-button");
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        let label = label.clone();
        button.connect_clicked(move |_| {
            let task_name = {
                let st = state.borrow();
                active_task_buttons(&st)
                    .into_iter()
                    .find(|button| button.label == label)
                    .map(|button| button.task_name)
            };
            let Some(task_name) = task_name else {
                crate::show_error_toast("Task action is no longer available");
                return;
            };
            let request = {
                let st = state.borrow();
                let source_tab_id = st.active_tab().map(|tab| tab.id).unwrap_or_default();
                task_panel_task_launch_request(&st, source_tab_id, task_name.clone())
            };
            if let Err(error) =
                crate::palette::launch_task(&request, &state, &tab_list, &term_stack, &window)
            {
                crate::show_error_toast(&error.to_string());
            }
        });
        panel.actions.insert(&button, -1);
    }

    let discover = gtk::Button::with_label("Discover Tasks");
    discover.add_css_class("task-panel-action-button");
    {
        let state = state.clone();
        let tab_list = tab_list.clone();
        discover.connect_clicked(move |_| {
            let Some(tab_id) = state.borrow().active_tab().map(|tab| tab.id) else {
                crate::show_error_toast("No active tab to discover tasks for");
                return;
            };
            discover_tasks_explicitly(&state, &tab_list, tab_id);
        });
    }
    panel.actions.insert(&discover, -1);

    let refresh = gtk::Button::with_label("Refresh");
    refresh.add_css_class("task-panel-action-button");
    {
        let panel = panel.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        refresh.connect_clicked(move |_| {
            panel.invalidate_local_task_cache();
            panel.refresh(&state, &tab_list, &term_stack, &window);
        });
    }
    panel.actions.insert(&refresh, -1);
}

/// GTK-free half of the task-panel action callback.
pub(crate) fn task_panel_task_launch_request(
    state: &AppState,
    source_tab_id: u32,
    task_name: impl Into<String>,
) -> crate::task_launch::TaskLaunchRequest {
    crate::task_launch::TaskLaunchRequest::for_discovered_tab(
        crate::task_launch::TaskLaunchSurface::TaskPanel,
        state,
        source_tab_id,
        task_name,
        None,
    )
}

fn active_task_buttons(state: &AppState) -> Vec<WorkspaceTaskButton> {
    let Some(active_tab) = state.active_tab() else {
        return Vec::new();
    };

    if !active_tab.task_buttons.is_empty() {
        return active_tab.task_buttons.clone();
    }

    active_tab
        .discovered_actions
        .iter()
        .copied()
        .map(WorkspaceTaskButton::from_action)
        .collect()
}

fn task_markdown_path(plan_root: &Path, task_id: &str) -> Option<PathBuf> {
    let task_id = task_id.trim();
    if !is_safe_task_id(task_id) {
        return None;
    }
    Some(plan_root.join(".plan/tasks").join(format!("{task_id}.md")))
}

fn is_safe_task_id(task_id: &str) -> bool {
    if task_id.is_empty() || task_id.contains('\\') {
        return false;
    }

    let mut components = Path::new(task_id).components();
    let is_single_normal_component = matches!(
        components.next(),
        Some(Component::Normal(name)) if name.to_str() == Some(task_id)
    );
    is_single_normal_component && components.next().is_none()
}

fn path_basename(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::{
        active_current_task_summary, aggregate_progress_text, bind_pane_task_mutation,
        build_pull_request_snapshot, classify_local_gh_failure, clear_pane_task_mutation,
        local_pull_request_cache_result_for_request, local_pull_request_probe_request,
        local_task_probe_is_current, local_task_probe_request, next_remote_failure_streak,
        next_remote_git_failure_streak, probe_local_pull_request_target, progress_for_unavailable,
        project_all_agent_panes, pull_request_discovery_feedback, pull_request_target_for_tab,
        record_pull_request_fetch, register_pull_request_fetch, remote_cache_key,
        remote_pull_request_identity_ttl_ms, remote_tasks_error_summary, remote_tasks_ttl_ms,
        state_available_for_refresh, task_markdown_path, task_panel_runtime_policy,
        task_row_from_entry, task_truth_axis_labels, verify_pull_request_bound_task,
        visible_task_rows, LocalPullRequestCacheEntry, PullRequestBoundTask,
        PullRequestDiscoveryFeedback, PullRequestKey, TaskBindingContext, TaskPanelIntent,
        TaskPanelMode, TaskPanelModeSelection, TaskPanelRuntimePolicy, LOCAL_PULL_REQUEST_TTL_MS,
        PULL_REQUESTS_TTL_MS, REMOTE_TASKS_FAILURE_TTL_CAP_MS, REMOTE_TASKS_TTL_MS,
    };
    use crate::tracking::{
        BranchPullRequestEntry, BranchPullRequestsData, PlanTaskEntry, PlanTasksData,
        RemoteGitIdentityError, RemoteGitIdentityOutcome, RemoteTaskTarget, RemoteTasksError,
        RemoteTasksOutcome,
    };
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn task_truth_axis_labels_keep_execution_origin_and_verification_separate() {
        let truth = crate::work_ledger::project_task_truth(&crate::work_ledger::TaskTruthInput {
            pane_origin: "pane".into(),
            task_id: Some("EXAMPLE-131".into()),
            origin: crate::work_ledger::WorkStreamOriginState::Live,
            bound_now: true,
            execution_known: true,
            agent_running: true,
            canonical: crate::work_ledger::CanonicalTaskState::InProgress,
            reconciliation: crate::work_ledger::WorkReconciliation {
                status: crate::work_ledger::WorkReconciliationStatus::Verified,
                source: "plan".into(),
                reason: "Current .plan status verified".into(),
                origin_status: "live".into(),
                current_value: Some("in_progress".into()),
                checked_at_unix_ms: Some(crate::events::unix_time_ms()),
            },
            pull_request: None,
        });
        let labels = task_truth_axis_labels(&truth);
        assert_eq!(
            &labels[..5],
            ["In progress", "Bound", "Running", "Live", "Verified (plan)"]
        );
        assert!(labels[5].starts_with("Checked "));
    }

    fn test_git_checkout(label: &str, branch: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "taarof-task-panel-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join(".git")).expect("create test git directory");
        fs::write(
            root.join(".git/HEAD"),
            format!("ref: refs/heads/{branch}\n"),
        )
        .expect("write test HEAD");
        fs::write(
            root.join(".git/config"),
            format!("[remote \"origin\"]\n\turl = git@github.com:owner/{label}.git\n"),
        )
        .expect("write test git config");
        root
    }

    #[test]
    fn local_task_probe_transition_during_flight_rejects_every_changed_worker_input() {
        let first_root = test_git_checkout("probe-flight-first", "main");
        let second_root = test_git_checkout("probe-flight-second", "feature/next");
        let state = Rc::new(RefCell::new(crate::AppState::new()));
        let (tab_id, pane_id) = {
            let mut st = state.borrow_mut();
            let workspace_id = st.active_workspace;
            let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
                &mut st,
                workspace_id,
                "probe",
                crate::HeadlessPaneSeed {
                    cwd: Some(first_root.to_string_lossy().into_owned()),
                    cwd_host: Some("local".into()),
                    ..crate::HeadlessPaneSeed::default()
                },
            )
            .expect("headless pane");
            st.active_ws_mut().expect("active workspace").repo_root =
                Some(first_root.to_string_lossy().into_owned());
            st.find_tab_mut(tab_id).expect("seeded tab").discovery_cwd =
                Some(first_root.to_string_lossy().into_owned());
            (tab_id, pane_id)
        };
        let in_flight = local_task_probe_request(&state).expect("initial probe request");

        {
            let mut st = state.borrow_mut();
            let second = second_root.to_string_lossy().into_owned();
            let workspace = st.active_ws_mut().expect("active workspace");
            workspace.repo_root = Some(second.clone());
            workspace.working_tree_path = Some(second.clone());
            st.find_tab_mut(tab_id).expect("seeded tab").discovery_cwd = Some(second.clone());
            let pane = st
                .headless_pane_mut(tab_id, pane_id)
                .expect("headless pane remains live");
            pane.location_state.cwd = Some(second.clone());
            pane.location_state.cwd_host = Some("remote-builder".into());
            pane.process_state.remote_shell = true;
            pane.current_task = Some(crate::task_binding::PaneTaskBinding {
                task_id: "EXAMPLE-166".into(),
                title: "Moved probe".into(),
                checkout_root: Some(second.into_boxed_str()),
                reporting_token: "ctx-next".into(),
            });
        }

        let current = local_task_probe_request(&state).expect("replacement probe request");
        assert_ne!(in_flight.key, current.key);
        assert!(
            !local_task_probe_is_current(&state, &in_flight.key),
            "a completion captured before cwd/host/remote/binding/candidate transition must not apply"
        );
        assert!(local_task_probe_is_current(&state, &current.key));

        let _ = fs::remove_dir_all(first_root);
        let _ = fs::remove_dir_all(second_root);
    }

    #[test]
    fn local_pull_request_cache_refreshes_branch_after_ttl_in_same_checkout() {
        let checkout = test_git_checkout("pr-cache-branch-switch", "main");
        let state = Rc::new(RefCell::new(crate::AppState::new()));
        {
            let mut st = state.borrow_mut();
            let workspace_id = st.active_workspace;
            let (tab_id, _) = crate::seed_headless_terminal_tab(
                &mut st,
                workspace_id,
                "pr",
                crate::HeadlessPaneSeed {
                    cwd: Some(checkout.to_string_lossy().into_owned()),
                    ..crate::HeadlessPaneSeed::default()
                },
            )
            .expect("headless pane");
            st.active_ws_mut().expect("active workspace").repo_root =
                Some(checkout.to_string_lossy().into_owned());
            st.find_tab_mut(tab_id).expect("seeded tab").discovery_cwd =
                Some(checkout.to_string_lossy().into_owned());
        }
        let request = local_pull_request_probe_request(&state).expect("local checkout request");
        let main_result =
            probe_local_pull_request_target(&request).expect("main branch should be discovered");
        assert_eq!(main_result.branch, "main");
        let fetched_at_ms = 100;
        let mut cache = HashMap::from([(
            request.key.clone(),
            LocalPullRequestCacheEntry {
                fetched_at_ms,
                result: Some(main_result),
            },
        )]);

        assert_eq!(
            local_pull_request_cache_result_for_request(
                &cache,
                &request,
                fetched_at_ms + LOCAL_PULL_REQUEST_TTL_MS - 1,
            )
            .as_ref()
            .map(|result| result.branch.as_str()),
            Some("main")
        );
        assert!(
            local_pull_request_cache_result_for_request(
                &cache,
                &request,
                fetched_at_ms + LOCAL_PULL_REQUEST_TTL_MS,
            )
            .is_none(),
            "the unchanged checkout key must request a fresh off-thread branch probe at TTL"
        );

        // Git's branch/ref state is intentionally discovered by the worker,
        // rather than read by GTK. The checkout and cache key stay the same
        // while HEAD moves, so expiry must expose this new worker result.
        fs::write(checkout.join(".git/HEAD"), "ref: refs/heads/feature/next\n")
            .expect("switch same checkout to feature branch");
        let same_checkout_request =
            local_pull_request_probe_request(&state).expect("same checkout request");
        assert_eq!(same_checkout_request.key, request.key);
        let refreshed_result = probe_local_pull_request_target(&same_checkout_request)
            .expect("feature branch should be rediscovered off-thread");
        assert_eq!(refreshed_result.branch, "feature/next");

        cache.insert(
            request.key.clone(),
            LocalPullRequestCacheEntry {
                fetched_at_ms: fetched_at_ms + LOCAL_PULL_REQUEST_TTL_MS,
                result: Some(refreshed_result),
            },
        );
        assert_eq!(
            local_pull_request_cache_result_for_request(
                &cache,
                &request,
                fetched_at_ms + LOCAL_PULL_REQUEST_TTL_MS,
            )
            .as_ref()
            .map(|result| result.branch.as_str()),
            Some("feature/next"),
            "same checkout must expose the newly discovered branch after refresh"
        );

        let _ = fs::remove_dir_all(checkout);
    }

    #[test]
    fn remote_fetch_backoff_doubles_and_caps() {
        assert_eq!(remote_tasks_ttl_ms(0), REMOTE_TASKS_TTL_MS);
        assert_eq!(remote_tasks_ttl_ms(1), REMOTE_TASKS_TTL_MS * 2);
        assert_eq!(remote_tasks_ttl_ms(3), REMOTE_TASKS_TTL_MS * 8);
        assert_eq!(remote_tasks_ttl_ms(5), REMOTE_TASKS_FAILURE_TTL_CAP_MS);
        // Large streaks must neither overflow nor exceed the cap.
        assert_eq!(
            remote_tasks_ttl_ms(u32::MAX),
            REMOTE_TASKS_FAILURE_TTL_CAP_MS
        );
    }

    #[test]
    fn remote_fetch_backoff_only_advances_for_real_errors() {
        let no_plan = RemoteTasksOutcome::NoTaskSource {
            checkout_root: Some("/srv/repo".to_string()),
        };
        let no_repo = RemoteTasksOutcome::NoTaskSource {
            checkout_root: None,
        };
        let error = RemoteTasksOutcome::Error(RemoteTasksError::Transport);

        assert_eq!(next_remote_failure_streak(4, &no_plan), 0);
        assert_eq!(next_remote_failure_streak(4, &no_repo), 0);
        assert_eq!(next_remote_failure_streak(4, &error), 5);
    }

    #[test]
    fn remote_pull_request_identity_failures_back_off() {
        assert_eq!(remote_pull_request_identity_ttl_ms(0), PULL_REQUESTS_TTL_MS);
        assert_eq!(
            remote_pull_request_identity_ttl_ms(1),
            PULL_REQUESTS_TTL_MS * 2
        );
        assert_eq!(
            remote_pull_request_identity_ttl_ms(u32::MAX),
            REMOTE_TASKS_FAILURE_TTL_CAP_MS
        );
        assert_eq!(
            next_remote_git_failure_streak(
                2,
                &RemoteGitIdentityOutcome::Error(RemoteGitIdentityError::Transport)
            ),
            3
        );
        assert_eq!(
            next_remote_git_failure_streak(2, &RemoteGitIdentityOutcome::NoRepository),
            0
        );
    }

    #[test]
    fn explicit_refresh_queues_a_post_click_fetch_behind_stale_inflight_work() {
        let key = "remote-branch".to_string();
        let mut inflight = HashSet::new();
        let mut refresh_after_inflight = HashSet::new();
        assert!(register_pull_request_fetch(
            &mut inflight,
            &mut refresh_after_inflight,
            &key,
            super::PullRequestFetchReason::Passive,
        ));
        assert!(!register_pull_request_fetch(
            &mut inflight,
            &mut refresh_after_inflight,
            &key,
            super::PullRequestFetchReason::ExplicitDiscovery,
        ));
        assert!(refresh_after_inflight.contains(&key));

        // Passive polling still coalesces without creating an endless retry.
        refresh_after_inflight.clear();
        assert!(!register_pull_request_fetch(
            &mut inflight,
            &mut refresh_after_inflight,
            &key,
            super::PullRequestFetchReason::Passive,
        ));
        assert!(refresh_after_inflight.is_empty());
    }

    #[test]
    fn remote_cache_key_is_stable_for_connection_and_checkout_location() {
        let target = |remote_command: &str, identity: &str| RemoteTaskTarget {
            host: "dev".to_string(),
            cwd: "/srv/repo/src".to_string(),
            ssh_argv: Some(vec![
                "ssh".to_string(),
                "-i".to_string(),
                identity.to_string(),
                "-t".to_string(),
                "dev".to_string(),
                remote_command.to_string(),
            ]),
        };
        assert_eq!(
            remote_cache_key(&target("exec bash", "/keys/a")),
            remote_cache_key(&target("exec zsh", "/keys/a"))
        );
        assert_ne!(
            remote_cache_key(&target("exec bash", "/keys/a")),
            remote_cache_key(&target("exec bash", "/keys/b"))
        );
    }

    #[test]
    fn remote_error_summaries_remain_distinct() {
        let errors = [
            RemoteTasksError::Transport,
            RemoteTasksError::Authentication,
            RemoteTasksError::Permission,
            RemoteTasksError::RootDiscovery,
            RemoteTasksError::MalformedJson,
            RemoteTasksError::Schema,
            RemoteTasksError::Protocol,
        ];
        let summaries: std::collections::HashSet<_> = errors
            .into_iter()
            .map(|error| remote_tasks_error_summary("dev", error))
            .collect();
        assert_eq!(summaries.len(), errors.len());
    }

    #[test]
    fn explicit_task_discovery_overrides_default_and_remembered_pull_request_modes() {
        let context = "repo:/workspace#main";
        let mut selection = TaskPanelModeSelection::new(true, true, TaskPanelMode::PullRequests);

        assert_eq!(
            selection.mode_for_context(context),
            TaskPanelMode::PullRequests
        );
        selection.apply_intent(context, TaskPanelIntent::DiscoverTasks);
        assert_eq!(selection.mode_for_context(context), TaskPanelMode::Tasks);

        selection.apply_intent(
            context,
            TaskPanelIntent::Select(TaskPanelMode::PullRequests),
        );
        assert_eq!(
            selection.mode_for_context(context),
            TaskPanelMode::PullRequests
        );
        selection.apply_intent(context, TaskPanelIntent::DiscoverTasks);
        assert_eq!(selection.mode_for_context(context), TaskPanelMode::Tasks);
    }

    #[test]
    fn session_mode_preserves_checkout_task_preferences_and_discovery_returns_to_tasks() {
        let first = "repo:/workspace#main";
        let second = "repo:/other#feature";
        let mut selection = TaskPanelModeSelection::new(true, true, TaskPanelMode::PullRequests);

        selection.apply_intent(first, TaskPanelIntent::Select(TaskPanelMode::Tasks));
        selection.apply_intent(first, TaskPanelIntent::Select(TaskPanelMode::Session));
        assert_eq!(selection.mode_for_context(first), TaskPanelMode::Session);
        assert_eq!(selection.mode_for_context(second), TaskPanelMode::Session);

        selection.apply_intent(second, TaskPanelIntent::DiscoverTasks);
        assert_eq!(selection.mode_for_context(second), TaskPanelMode::Tasks);
        assert_eq!(selection.task_mode_for_context(first), TaskPanelMode::Tasks);
        assert_eq!(
            selection.task_mode_for_context(second),
            TaskPanelMode::Tasks
        );

        let disabled = TaskPanelModeSelection::new(false, false, TaskPanelMode::Tasks);
        assert_eq!(disabled.mode_for_context(first), TaskPanelMode::Session);
    }

    #[test]
    fn agents_mode_is_global_and_available_without_task_sources() {
        let first = "repo:/workspace#main";
        let second = "repo:/other#feature";
        let mut selection = TaskPanelModeSelection::new(false, false, TaskPanelMode::Tasks);

        selection.apply_intent(first, TaskPanelIntent::Select(TaskPanelMode::Agents));

        assert_eq!(selection.mode_for_context(first), TaskPanelMode::Agents);
        assert_eq!(selection.mode_for_context(second), TaskPanelMode::Agents);
    }

    #[test]
    fn agent_activity_cards_project_live_panes_with_identity_activity_and_context() {
        let mut state = crate::AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "Implementation",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("agent pane should seed");
        state.runtime_probe = Some(crate::runtime_probe::RuntimeProbeSnapshot {
            probed_at_unix_ms: 1,
            process_observed_at_unix_ms: Some(1),
            ports_observed_at_unix_ms: Some(1),
            tab_pids: std::collections::BTreeMap::new(),
            pane_pids: std::collections::BTreeMap::new(),
            pane_process_states: HashMap::new(),
            pane_exact_agents: std::collections::HashMap::new(),
            pane_agents: HashMap::from([(
                (tab_id, pane_id),
                crate::agents::AgentStatus {
                    agent_name: Some("codex".into()),
                    session_id: Some("session-1".into()),
                    running: true,
                },
            )]),
            tab_agents: HashMap::new(),
            tab_ports: HashMap::new(),
            process_probe: crate::probe::ProbeState::Ok,
            ports_probe: crate::probe::ProbeState::Ok,
            process_error: None,
            ports_error: None,
        });
        state
            .find_tab_mut(tab_id)
            .expect("seeded tab")
            .set_pane_agent_activity(
                pane_id,
                crate::workspace::AgentActivity::socket(
                    crate::workspace::AgentActivityState::Running,
                    "editing task_panel.rs",
                    Some("codex".into()),
                ),
            );

        let cards = project_all_agent_panes(&state);

        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].title, "Implementation");
        assert_eq!(cards[0].agent_label, "codex");
        assert_eq!(cards[0].activity, "editing task_panel.rs");
        assert_eq!(cards[0].state.label(), "WORKING");
        assert_eq!((cards[0].tab_id, cards[0].pane_id), (tab_id, pane_id));
    }

    #[test]
    fn agent_activity_cards_report_idle_when_only_a_process_is_detected() {
        let mut state = crate::AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "Idle agent",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("agent pane should seed");
        state.runtime_probe = Some(crate::runtime_probe::RuntimeProbeSnapshot {
            probed_at_unix_ms: 1,
            process_observed_at_unix_ms: Some(1),
            ports_observed_at_unix_ms: Some(1),
            tab_pids: std::collections::BTreeMap::new(),
            pane_pids: std::collections::BTreeMap::new(),
            pane_process_states: HashMap::new(),
            pane_exact_agents: std::collections::HashMap::new(),
            pane_agents: HashMap::from([(
                (tab_id, pane_id),
                crate::agents::AgentStatus {
                    agent_name: Some("claude".into()),
                    session_id: None,
                    running: true,
                },
            )]),
            tab_agents: HashMap::new(),
            tab_ports: HashMap::new(),
            process_probe: crate::probe::ProbeState::Ok,
            ports_probe: crate::probe::ProbeState::Ok,
            process_error: None,
            ports_error: None,
        });

        let cards = project_all_agent_panes(&state);

        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].state, crate::agents::AgentLifecycle::Idle);
        assert_eq!(cards[0].state.label(), "IDLE");
    }

    #[test]
    fn agent_activity_cards_report_idle_when_the_running_signal_is_stale() {
        let mut state = crate::AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "Stale agent",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("agent pane should seed");
        let stale = crate::workspace::AgentActivity {
            updated_at: std::time::Instant::now()
                - crate::workspace::EXPLICIT_ACTIVITY_FRESHNESS
                - std::time::Duration::from_secs(1),
            ..crate::workspace::AgentActivity::socket(
                crate::workspace::AgentActivityState::Running,
                "editing task_panel.rs",
                Some("claude".into()),
            )
            .expect("running activity")
        };
        state
            .find_tab_mut(tab_id)
            .expect("seeded tab")
            .set_pane_agent_activity(pane_id, Some(stale));

        let cards = project_all_agent_panes(&state);

        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].state, crate::agents::AgentLifecycle::Idle);
        assert_eq!(cards[0].activity, "editing task_panel.rs");
    }

    #[test]
    fn agent_activity_cards_keep_waiting_done_and_error_states_distinct() {
        let mut state = crate::AppState::new();
        let workspace_id = state.active_workspace;
        let cases = [
            (
                "Architecture review",
                "kimi",
                crate::workspace::AgentActivityState::WaitingInput,
                "WAITING",
            ),
            (
                "Test matrix",
                "pi",
                crate::workspace::AgentActivityState::Done,
                "DONE",
            ),
            (
                "Release notes",
                "opencode",
                crate::workspace::AgentActivityState::Errored,
                "ERRORED",
            ),
        ];
        for (title, agent, activity_state, _) in cases {
            let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
                &mut state,
                workspace_id,
                title,
                crate::HeadlessPaneSeed::default(),
            )
            .expect("activity pane should seed");
            state
                .find_tab_mut(tab_id)
                .expect("seeded tab")
                .set_pane_agent_activity(
                    pane_id,
                    crate::workspace::AgentActivity::socket(
                        activity_state,
                        "status detail",
                        Some(agent.into()),
                    ),
                );
        }

        let cards = project_all_agent_panes(&state);

        assert_eq!(
            cards
                .iter()
                .map(|card| (card.title.as_str(), card.state.label()))
                .collect::<Vec<_>>(),
            cases
                .iter()
                .map(|(title, _, _, label)| (*title, *label))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn agent_activity_cards_do_not_guess_a_pane_for_legacy_tab_scoped_activity() {
        let mut state = crate::AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, _pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "Release notes",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("agent pane should seed");
        state
            .find_tab_mut(tab_id)
            .expect("seeded tab")
            .set_socket_agent_activity(crate::workspace::AgentActivity::socket(
                crate::workspace::AgentActivityState::Errored,
                "documentation failed",
                Some("opencode".into()),
            ));

        let cards = project_all_agent_panes(&state);

        assert!(cards.is_empty());
    }

    #[test]
    fn task_disabled_mode_never_loads_or_polls_task_and_pr_sources() {
        assert_eq!(
            task_panel_runtime_policy(false),
            TaskPanelRuntimePolicy {
                load_task_sources: false,
                install_task_poll: false,
            }
        );
        assert_eq!(
            task_panel_runtime_policy(true),
            TaskPanelRuntimePolicy {
                load_task_sources: true,
                install_task_poll: true,
            }
        );
    }

    #[test]
    fn pr_ledger_event_intent_selects_existing_pull_request_action_surface() {
        let context = "local:/workspace:agent/work";
        let mut selection = TaskPanelModeSelection::new(true, true, TaskPanelMode::Tasks);
        assert_eq!(selection.mode_for_context(context), TaskPanelMode::Tasks);
        selection.apply_intent(
            context,
            TaskPanelIntent::Select(TaskPanelMode::PullRequests),
        );
        assert_eq!(
            selection.mode_for_context(context),
            TaskPanelMode::PullRequests
        );
    }

    #[test]
    fn pull_request_target_uses_requested_tabs_focused_pane_not_discovery_context() {
        let first_root = test_git_checkout("requested-first", "feature/first");
        let second_root = test_git_checkout("requested-second", "feature/second");
        let mut app = crate::AppState::new();
        let workspace = app.active_workspace;
        let (first_tab, _first_pane) = crate::seed_headless_terminal_tab(
            &mut app,
            workspace,
            "split",
            crate::HeadlessPaneSeed {
                cwd: Some(first_root.to_string_lossy().into_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let (second_tab, second_pane) = crate::seed_headless_terminal_tab(
            &mut app,
            workspace,
            "temporary",
            crate::HeadlessPaneSeed {
                cwd: Some(second_root.to_string_lossy().into_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let second_state = app
            .headless_panes
            .remove(&(second_tab, second_pane))
            .expect("second pane state");
        app.headless_panes
            .insert((first_tab, second_pane), second_state);
        {
            let tab = app.find_tab_mut(first_tab).expect("split tab");
            tab.focused_pane_id = second_pane;
            tab.discovery_cwd = Some(first_root.to_string_lossy().into_owned());
        }
        let state = Rc::new(RefCell::new(app));

        let target = pull_request_target_for_tab(&state, first_tab).expect("focused PR target");

        assert_eq!(target.checkout_root, second_root);
        assert_eq!(target.branch, "feature/second");
        assert_eq!(target.head_repository, "owner/requested-second");

        let _ = fs::remove_dir_all(first_root);
        let _ = fs::remove_dir_all(second_root);
    }

    #[test]
    fn pull_request_target_is_identical_for_eager_and_lazy_restore() {
        let root = test_git_checkout("restored", "agent/restored");
        let cwd = root.to_string_lossy().into_owned();

        let mut eager = crate::AppState::new();
        let eager_workspace = eager.active_workspace;
        let (eager_tab, _) = crate::seed_headless_terminal_tab(
            &mut eager,
            eager_workspace,
            "eager",
            crate::HeadlessPaneSeed {
                cwd: Some(cwd.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let eager = Rc::new(RefCell::new(eager));

        let mut lazy = crate::AppState::new();
        let lazy_workspace = lazy.active_workspace;
        let lazy_tab = crate::seed_pending_restore_tab(
            &mut lazy,
            lazy_workspace,
            "lazy",
            crate::session::SavedPaneNode::Leaf {
                work_origin: Some("pane-lazy-pr-target".into()),
                cwd: Some(cwd.clone()),
                ssh_command: None,
                tmux_session: None,
                tmux_host: None,
                tmux_identity: None,
                current_task: None,
                agent_session: None,
            },
            Some(cwd),
        )
        .unwrap();
        let lazy = Rc::new(RefCell::new(lazy));

        let eager_target = pull_request_target_for_tab(&eager, eager_tab).unwrap();
        let lazy_target = pull_request_target_for_tab(&lazy, lazy_tab).unwrap();
        assert_eq!(eager_target.checkout_root, root);
        assert_eq!(lazy_target.checkout_root, root);
        assert_eq!(eager_target.branch, lazy_target.branch);
        assert_eq!(eager_target.head_repository, lazy_target.head_repository);
        assert_eq!(eager_target.base_repository, lazy_target.base_repository);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn background_pr_verification_rejects_removed_or_renamed_plan_task() {
        let root = test_git_checkout("binding-verification", "agent/work");
        fs::create_dir_all(root.join(".plan")).unwrap();
        let write_task = |id: &str, title: &str| {
            fs::write(
                root.join(".plan/tasks.json"),
                format!(r#"{{"tasks":[{{"id":"{id}","title":"{title}","status":"todo"}}]}}"#),
            )
            .unwrap();
        };
        write_task("TASK-1", "Original title");
        let bound = PullRequestBoundTask {
            task_id: "TASK-1".into(),
            title: "Original title".into(),
            checkout_root: root.canonicalize().unwrap().to_string_lossy().into_owned(),
            reporting_token: crate::task_binding::new_reporting_token(),
        };
        assert!(verify_pull_request_bound_task(bound.clone()).is_some());

        write_task("TASK-2", "Replacement");
        assert!(verify_pull_request_bound_task(bound.clone()).is_none());
        write_task("TASK-1", "Renamed title");
        assert!(verify_pull_request_bound_task(bound).is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discover_tasks_refreshes_branch_pull_requests_without_duplicates() {
        let key = PullRequestKey::new(
            PathBuf::from("/repo"),
            "owner/repo",
            "owner",
            "feature/discovery",
        );
        let initial = BranchPullRequestsData {
            total: 1,
            open: 1,
            draft: 0,
            merged: 0,
            closed: 0,
            pull_requests: Vec::new(),
            query_complete: true,
        };
        let refreshed = BranchPullRequestsData {
            total: 2,
            open: 2,
            draft: 0,
            merged: 0,
            closed: 0,
            pull_requests: Vec::new(),
            query_complete: true,
        };
        let mut cache = HashMap::new();
        record_pull_request_fetch(&mut cache, key.clone(), Ok(initial), 10);
        record_pull_request_fetch(&mut cache, key.clone(), Ok(refreshed), 20);

        assert_eq!(cache.len(), 1, "a refresh must replace the branch entry");
        let entry = cache.get(&key).expect("refreshed branch entry");
        assert_eq!(entry.fetched_at_ms, 20);
        assert_eq!(
            entry.verified.as_ref().expect("successful refresh").total,
            2
        );
        assert_eq!(
            pull_request_discovery_feedback(&Ok(entry.verified.clone().unwrap())),
            PullRequestDiscoveryFeedback::Success(
                "Tasks discovered; refreshed 2 pull requests".to_string()
            )
        );
    }

    #[test]
    fn remote_pull_request_cache_replaces_same_connection_branch_without_cross_host_reuse() {
        let mut first = PullRequestKey::new(
            PathBuf::from("/srv/repo"),
            "owner/repo",
            "fork",
            "agent/work",
        );
        first.remote_connection = Some(vec!["ssh".into(), "dev-a".into()]);
        let mut second = first.clone();
        second.remote_connection = Some(vec!["ssh".into(), "dev-b".into()]);
        let data = |total| BranchPullRequestsData {
            total,
            open: total,
            draft: 0,
            merged: 0,
            closed: 0,
            pull_requests: Vec::new(),
            query_complete: true,
        };
        let mut cache = HashMap::new();
        record_pull_request_fetch(&mut cache, first.clone(), Ok(data(1)), 10);
        record_pull_request_fetch(&mut cache, first.clone(), Ok(data(2)), 20);
        record_pull_request_fetch(&mut cache, second.clone(), Ok(data(3)), 30);

        assert_eq!(cache.len(), 2);
        assert_eq!(cache[&first].verified.as_ref().unwrap().total, 2);
        assert_eq!(cache[&second].verified.as_ref().unwrap().total, 3);
    }

    #[test]
    fn no_matching_pull_request_is_a_successful_empty_result() {
        let empty = BranchPullRequestsData {
            total: 0,
            open: 0,
            draft: 0,
            merged: 0,
            closed: 0,
            pull_requests: Vec::new(),
            query_complete: true,
        };
        assert_eq!(
            pull_request_discovery_feedback(&Ok(empty)),
            PullRequestDiscoveryFeedback::Success(
                "Tasks discovered; no pull requests found for this branch".into()
            )
        );
    }

    #[test]
    fn github_failure_classification_never_returns_secret_bearing_stderr() {
        let error = classify_local_gh_failure(
            "request failed for https://token:super-secret@api.github.com/graphql",
        );
        assert_eq!(
            error,
            "GitHub pull-request query failed on the controlling host"
        );
        assert!(!error.contains("super-secret"));
    }

    #[test]
    fn failed_pr_refresh_preserves_last_verified_state_and_marks_error() {
        let key = PullRequestKey::new(PathBuf::from("/repo"), "owner/repo", "owner", "agent/work");
        let verified = BranchPullRequestsData {
            total: 1,
            open: 1,
            draft: 0,
            merged: 0,
            closed: 0,
            pull_requests: Vec::new(),
            query_complete: true,
        };
        let mut cache = HashMap::new();
        record_pull_request_fetch(&mut cache, key.clone(), Ok(verified), 10);
        record_pull_request_fetch(&mut cache, key.clone(), Err("offline".into()), 20);
        let entry = cache.get(&key).unwrap();
        assert_eq!(entry.fetched_at_ms, 20);
        assert_eq!(entry.verified_at_ms, Some(10));
        assert_eq!(entry.verified.as_ref().unwrap().total, 1);
        assert_eq!(entry.error.as_deref(), Some("offline"));
    }

    #[test]
    fn failed_pr_refresh_never_reuses_verified_data_from_another_base_repository() {
        let old_key = PullRequestKey::new(
            PathBuf::from("/repo"),
            "owner/old-repo",
            "fork-owner",
            "agent/work",
        );
        let new_key = PullRequestKey::new(
            PathBuf::from("/repo"),
            "owner/new-repo",
            "fork-owner",
            "agent/work",
        );
        let verified = BranchPullRequestsData {
            total: 1,
            open: 1,
            draft: 0,
            merged: 0,
            closed: 0,
            pull_requests: Vec::new(),
            query_complete: true,
        };
        let mut cache = HashMap::new();
        record_pull_request_fetch(&mut cache, old_key.clone(), Ok(verified), 10);
        record_pull_request_fetch(&mut cache, new_key.clone(), Err("offline".into()), 20);

        assert_eq!(cache.len(), 2);
        assert!(cache.get(&old_key).unwrap().verified.is_some());
        let new_entry = cache.get(&new_key).unwrap();
        assert!(new_entry.verified.is_none());
        assert_eq!(new_entry.error.as_deref(), Some("offline"));
    }

    #[test]
    fn failed_pr_refresh_never_reuses_verified_data_from_another_head_owner() {
        let old_key = PullRequestKey::new(
            PathBuf::from("/repo"),
            "Owner/Repo",
            "old-fork",
            "agent/work",
        );
        let new_key = PullRequestKey::new(
            PathBuf::from("/repo"),
            "owner/repo",
            "new-fork",
            "agent/work",
        );
        let verified = BranchPullRequestsData {
            total: 1,
            open: 1,
            draft: 0,
            merged: 0,
            closed: 0,
            pull_requests: Vec::new(),
            query_complete: true,
        };
        let mut cache = HashMap::new();
        record_pull_request_fetch(&mut cache, old_key.clone(), Ok(verified), 10);
        record_pull_request_fetch(&mut cache, new_key.clone(), Err("offline".into()), 20);

        assert_eq!(old_key.base_repository, "owner/repo");
        assert_eq!(
            new_key,
            PullRequestKey::new(
                PathBuf::from("/repo"),
                "Owner/Repo",
                "New-Fork",
                "agent/work",
            )
        );
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&old_key).unwrap().verified.is_some());
        assert!(cache.get(&new_key).unwrap().verified.is_none());
    }

    #[test]
    fn discover_tasks_preserves_local_results_when_pull_request_query_fails() {
        let context = "repo:/workspace#main";
        let mut selection = TaskPanelModeSelection::new(true, true, TaskPanelMode::PullRequests);
        selection.apply_intent(context, TaskPanelIntent::DiscoverTasks);

        let cases = [
            (
                "failed to run gh: No such file or directory".to_string(),
                "Tasks discovered; install GitHub CLI (gh) to refresh pull requests",
            ),
            (
                "To get started with GitHub CLI, please run: gh auth login".to_string(),
                "Tasks discovered; run gh auth login to refresh pull requests",
            ),
            (
                "none of the git remotes configured for this repository point to a known GitHub host"
                    .to_string(),
                "Tasks discovered; this checkout has no supported GitHub remote",
            ),
            (
                "network request timed out".to_string(),
                "Tasks discovered; pull requests could not be refreshed with gh",
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(
                selection.mode_for_context(context),
                TaskPanelMode::Tasks,
                "PR feedback must not move explicit discovery away from local tasks"
            );
            assert_eq!(
                pull_request_discovery_feedback(&Err(error)),
                PullRequestDiscoveryFeedback::Error(expected.to_string())
            );
        }
    }

    fn task(id: &str, status: &str, blocked_by: usize) -> PlanTaskEntry {
        PlanTaskEntry {
            id: id.into(),
            title: format!("Task {id}"),
            status: status.into(),
            priority: Some("p1".into()),
            blocked_by,
            depends_on: 0,
            unresolved_depends_on: 0,
            depends_on_ids: Vec::new(),
            assigned_model: None,
        }
    }

    fn plan_data(done: usize, total: usize) -> PlanTasksData {
        PlanTasksData {
            total,
            done,
            in_progress: 0,
            ready: total.saturating_sub(done),
            blocked: 0,
            tasks: Vec::new(),
        }
    }

    #[test]
    fn visible_task_rows_hides_done_tasks() {
        let data = PlanTasksData {
            total: 3,
            done: 1,
            in_progress: 1,
            ready: 1,
            blocked: 0,
            tasks: vec![
                task("EXAMPLE-1", "in-progress", 0),
                task("EXAMPLE-2", "todo", 0),
                task("EXAMPLE-3", "done", 0),
            ],
        };

        let rows = visible_task_rows(
            &data,
            Some(Path::new("/repo")),
            None,
            Some("EXAMPLE-2"),
            None,
            None,
            &HashMap::new(),
        );
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["EXAMPLE-1", "EXAMPLE-2"]);
    }

    #[test]
    fn task_row_marks_next_task() {
        let row = task_row_from_entry(
            &task("EXAMPLE-2", "todo", 0),
            Some(Path::new("/repo")),
            None,
            Some("EXAMPLE-2"),
            None,
            None,
            &[],
        );
        assert!(row.is_next);
    }

    #[test]
    fn task_row_marks_blocked_note() {
        let row = task_row_from_entry(
            &task("EXAMPLE-9", "todo", 2),
            Some(Path::new("/repo")),
            None,
            None,
            None,
            None,
            &[],
        );
        assert_eq!(row.note.as_deref(), Some("blocked by 2"));
    }

    #[test]
    fn task_row_names_unmet_blockers_when_known() {
        // When the parallelization plan supplies the actual blocker IDs, the row
        // note names them rather than showing a bare count.
        let blockers = vec!["EXAMPLE-1".to_string(), "EXAMPLE-2".to_string()];
        let row = task_row_from_entry(
            &task("EXAMPLE-9", "todo", 2),
            Some(Path::new("/repo")),
            None,
            None,
            None,
            None,
            &blockers,
        );
        assert_eq!(row.note.as_deref(), Some("blocked by EXAMPLE-1, EXAMPLE-2"));
    }

    #[test]
    fn local_task_rows_expose_bind_and_current_metadata() {
        let context = TaskBindingContext {
            tab_id: 10,
            pane_id: 20,
            current_task_id: Some("EXAMPLE-2".to_string()),
        };
        let rows = visible_task_rows(
            &PlanTasksData {
                total: 2,
                done: 0,
                in_progress: 0,
                ready: 2,
                blocked: 0,
                tasks: vec![task("EXAMPLE-1", "todo", 0), task("EXAMPLE-2", "todo", 0)],
            },
            Some(Path::new("/repo")),
            None,
            None,
            None,
            Some(&context),
            &HashMap::new(),
        );

        assert_eq!(
            rows[0].binding.as_ref().map(|info| info.is_current),
            Some(false)
        );
        assert_eq!(
            rows[1].binding.as_ref().map(|info| info.is_current),
            Some(true)
        );
        assert_eq!(rows[1].binding.as_ref().map(|info| info.tab_id), Some(10));
        assert_eq!(rows[1].binding.as_ref().map(|info| info.pane_id), Some(20));
    }

    #[test]
    fn cross_checkout_same_id_row_remains_bindable_not_current() {
        let first = test_git_checkout("row-current-first", "main");
        let second = test_git_checkout("row-current-second", "main");
        for (root, title) in [(&first, "First title"), (&second, "Second title")] {
            fs::create_dir_all(root.join(".plan")).expect("plan dir");
            fs::write(
                root.join(".plan/tasks.json"),
                format!(
                    r#"{{"tasks":[{{"id":"EXAMPLE-110","title":"{title}","status":"todo"}}]}}"#
                ),
            )
            .expect("tasks file");
        }
        let mut state = crate::AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "work",
            crate::HeadlessPaneSeed {
                cwd: Some(first.to_string_lossy().into_owned()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless pane");
        state
            .bind_pane_to_task(tab_id, pane_id, "EXAMPLE-110")
            .expect("bind first checkout");
        state
            .headless_pane_mut(tab_id, pane_id)
            .expect("pane")
            .location_state
            .cwd = Some(second.to_string_lossy().into_owned());

        let context = TaskBindingContext {
            tab_id,
            pane_id,
            current_task_id: None,
        };
        assert_eq!(context.current_task_id, None);
        let rows = visible_task_rows(
            &PlanTasksData {
                total: 1,
                done: 0,
                in_progress: 0,
                ready: 1,
                blocked: 0,
                tasks: vec![task("EXAMPLE-110", "todo", 0)],
            },
            Some(&second),
            None,
            None,
            None,
            Some(&context),
            &HashMap::new(),
        );
        assert_eq!(
            rows[0].binding.as_ref().map(|binding| binding.is_current),
            Some(false)
        );
        let _ = fs::remove_dir_all(first);
        let _ = fs::remove_dir_all(second);
    }

    #[test]
    fn current_work_summary_survives_done_and_deleted_task_rows() {
        let root = test_git_checkout("current-work-card", "main");
        fs::create_dir_all(root.join(".plan")).expect("plan dir");
        fs::write(
            root.join(".plan/tasks.json"),
            r#"{"tasks":[{"id":"EXAMPLE-110","title":"Pinned work","status":"done"}]}"#,
        )
        .expect("tasks file");
        let mut state = crate::AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "work",
            crate::HeadlessPaneSeed {
                cwd: Some(root.to_string_lossy().into_owned()),
                cwd_host: Some("localhost".into()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless pane");
        state
            .bind_pane_to_task(tab_id, pane_id, "EXAMPLE-110")
            .expect("done task can still be current work");

        let current = active_current_task_summary(&state).expect("current card summary");
        assert_eq!(current.id, "EXAMPLE-110");
        assert_eq!(current.title, "Pinned work");
        assert!(current.resolved);

        fs::write(root.join(".plan/tasks.json"), r#"{"tasks":[]}"#).expect("remove task");
        let unresolved = active_current_task_summary(&state).expect("card remains visible");
        assert_eq!(unresolved.title, "Pinned work");
        assert!(!unresolved.resolved);

        assert!(state.clear_pane_task_binding(tab_id, pane_id));
        assert!(active_current_task_summary(&state).is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task_mutation_helpers_release_state_borrow_before_refresh_can_reborrow() {
        let root = test_git_checkout("binding-borrow", "main");
        fs::create_dir_all(root.join(".plan")).expect("plan dir");
        fs::write(
            root.join(".plan/tasks.json"),
            r#"{"tasks":[{"id":"EXAMPLE-110","title":"Borrow-safe","status":"todo"}]}"#,
        )
        .expect("tasks file");
        let state = std::rc::Rc::new(std::cell::RefCell::new(crate::AppState::new()));
        let (tab_id, pane_id) = {
            let mut state = state.borrow_mut();
            let workspace_id = state.active_workspace;
            crate::seed_headless_terminal_tab(
                &mut state,
                workspace_id,
                "work",
                crate::HeadlessPaneSeed {
                    cwd: Some(root.to_string_lossy().into_owned()),
                    ..crate::HeadlessPaneSeed::default()
                },
            )
            .expect("headless pane")
        };

        {
            let _busy = state.borrow_mut();
            assert!(!state_available_for_refresh(&state));
        }
        assert!(state_available_for_refresh(&state));

        bind_pane_task_mutation(&state, tab_id, pane_id, "EXAMPLE-110").expect("bind task");
        assert!(state.try_borrow().is_ok(), "bind borrow must be released");
        assert!(clear_pane_task_mutation(&state, tab_id, pane_id));
        assert!(state.try_borrow().is_ok(), "clear borrow must be released");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn display_only_task_rows_do_not_expose_binding_metadata() {
        let row = task_row_from_entry(
            &task("EXAMPLE-1", "todo", 0),
            None,
            None,
            None,
            None,
            None,
            &[],
        );

        assert!(row.binding.is_none());
    }

    #[test]
    fn task_markdown_path_rejects_path_like_task_ids() {
        let root = Path::new("/repo");
        assert_eq!(
            task_markdown_path(root, "EXAMPLE-1").as_deref(),
            Some(Path::new("/repo/.plan/tasks/EXAMPLE-1.md"))
        );
        assert!(task_markdown_path(root, "../secret").is_none());
        assert!(task_markdown_path(root, "KMUX/1").is_none());
        assert!(task_markdown_path(root, "/tmp/EXAMPLE-1").is_none());
        assert!(task_markdown_path(root, r"KMUX\1").is_none());
    }

    #[test]
    fn aggregate_progress_text_reports_done_and_total() {
        assert_eq!(aggregate_progress_text(&plan_data(0, 7)), "0 of 7 done");
        assert_eq!(aggregate_progress_text(&plan_data(1, 7)), "1 of 7 done");
        assert_eq!(aggregate_progress_text(&plan_data(3, 7)), "3 of 7 done");
        assert_eq!(aggregate_progress_text(&plan_data(7, 7)), "All 7 of 7 done");
    }

    #[test]
    fn aggregate_progress_text_handles_empty_and_complete() {
        assert_eq!(
            aggregate_progress_text(&plan_data(0, 0)),
            "No tasks tracked"
        );
        // Defensive: done > total would be a corrupt snapshot, but the header
        // should still resolve to the "all done" branch rather than print a
        // misleading negative count.
        assert_eq!(aggregate_progress_text(&plan_data(9, 7)), "All 7 of 7 done");
    }

    #[test]
    fn aggregate_progress_text_handles_single_task() {
        assert_eq!(aggregate_progress_text(&plan_data(0, 1)), "0 of 1 done");
        assert_eq!(aggregate_progress_text(&plan_data(1, 1)), "All 1 of 1 done");
    }

    #[test]
    fn progress_for_unavailable_reports_state_specific_placeholders() {
        assert_eq!(progress_for_unavailable("no_plan_root"), "No progress data");
        assert_eq!(
            progress_for_unavailable("load_failed"),
            "Progress unavailable"
        );
        // Unknown reasons fall back to the generic placeholder rather than
        // panicking or surfacing the raw reason string to the user.
        assert_eq!(progress_for_unavailable("???"), "No progress data");
    }

    #[test]
    fn pull_request_snapshot_reports_branch_counts_and_rows() {
        let mut data = BranchPullRequestsData {
            total: 3,
            open: 2,
            draft: 1,
            merged: 1,
            closed: 0,
            pull_requests: vec![
                BranchPullRequestEntry {
                    number: 41,
                    title: "Ready branch work".into(),
                    state: "open".into(),
                    is_draft: false,
                    review_decision: Some("APPROVED".into()),
                    url: Some("https://github.com/Example-Org/Example-Repo/pull/41".into()),
                    head_repository_owner: "example-org".into(),
                    head_ref_name: "feature/task-panel".into(),
                    base_ref_name: "main".into(),
                    updated_at: Some("2026-06-19T15:00:00Z".into()),
                    checks: crate::tracking::PullRequestChecks::Ready,
                    correlation: Some(crate::tracking::PullRequestCorrelationMetadata {
                        task_id: "EXAMPLE-130".into(),
                        repo: "example-org/example-repo".into(),
                        branch: "feature/task-panel".into(),
                        dispatch_source: "agent".into(),
                    }),
                    correlation_marker_present: true,
                },
                BranchPullRequestEntry {
                    number: 42,
                    title: "Draft branch work".into(),
                    state: "open".into(),
                    is_draft: true,
                    review_decision: None,
                    url: Some("https://github.com/Example-Org/Example-Repo/pull/42".into()),
                    head_repository_owner: "example-org".into(),
                    head_ref_name: "feature/task-panel".into(),
                    base_ref_name: "main".into(),
                    updated_at: Some("2026-06-18T15:00:00Z".into()),
                    checks: crate::tracking::PullRequestChecks::Pending,
                    correlation: None,
                    correlation_marker_present: false,
                },
                BranchPullRequestEntry {
                    number: 40,
                    title: "Old branch work".into(),
                    state: "merged".into(),
                    is_draft: false,
                    review_decision: None,
                    url: Some("https://github.com/Example-Org/Example-Repo/pull/40".into()),
                    head_repository_owner: "example-org".into(),
                    head_ref_name: "feature/task-panel".into(),
                    base_ref_name: "main".into(),
                    updated_at: Some("2026-06-17T15:00:00Z".into()),
                    checks: crate::tracking::PullRequestChecks::None,
                    correlation: None,
                    correlation_marker_present: false,
                },
            ],
            query_complete: true,
        };

        let snapshot =
            build_pull_request_snapshot("legacy-app · feature/task-panel".into(), &data, None);

        assert_eq!(snapshot.progress, "2 open · 1 draft");
        assert_eq!(snapshot.summary, "Branch PRs: 1 merged · 0 closed");
        assert_eq!(snapshot.tasks.len(), 3);
        assert_eq!(snapshot.tasks[0].id, "#41");
        assert_eq!(snapshot.tasks[0].status, "open · approved");
        assert_eq!(
            snapshot.tasks[0].note.as_deref(),
            Some("linked: EXAMPLE-130 · feature/task-panel -> main")
        );
        assert_eq!(
            snapshot.tasks[1].note.as_deref(),
            Some("unlinked · feature/task-panel -> main")
        );
        assert_eq!(
            snapshot.tasks[0].external_url.as_deref(),
            Some("https://github.com/Example-Org/Example-Repo/pull/41")
        );

        let mut duplicate = data.pull_requests[0].clone();
        duplicate.number = 43;
        data.pull_requests.push(duplicate);
        data.total += 1;
        data.open += 1;
        let ambiguous =
            build_pull_request_snapshot("legacy-app · feature/task-panel".into(), &data, None);
        assert_eq!(
            ambiguous.tasks[0].note.as_deref(),
            Some("unlinked (ambiguous marker) · feature/task-panel -> main")
        );
        assert_eq!(
            ambiguous.tasks[3].note.as_deref(),
            Some("unlinked (ambiguous marker) · feature/task-panel -> main")
        );
    }
}
