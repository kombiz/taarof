use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agents::AgentLifecycle;
use crate::pane::{PaneNode, SplitChild};
use crate::probe::ProbeSnapshot;

/// A workspace groups related tabs and metadata (e.g., repo root, branch).
/// In the initial migration, all tabs live in a single "default" workspace.
pub struct Workspace {
    pub id: u32,
    /// Stable work-ledger identity. Runtime ids may change after restore.
    pub work_origin: String,
    pub name: String,
    pub collapsed: bool,
    pub repo_root: Option<String>,
    pub is_worktree: bool,
    pub working_tree_path: Option<String>,
    pub branch_name: Option<String>,
    pub linked_issue: Option<String>,
    pub env_vars: HashMap<String, String>,
    pub run_status: WorkspaceStatus,
    pub tabs: Vec<Tab>,
    pub active_tab: u32,
    pub last_active_tab: Option<u32>,
    pub tmux_backed: bool,
    pub host_config_name: Option<String>,
    pub host_status: ProbeSnapshot<crate::host::HostStatus>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum WorkspaceStatus {
    #[default]
    Idle,
    Running,
    Errored,
}

pub const DONE_ACTIVITY_VISIBILITY: Duration = Duration::from_secs(5);
pub const EXPLICIT_ACTIVITY_FRESHNESS: Duration = Duration::from_secs(8);
pub const OUTPUT_SCAN_ACTIVITY_FRESHNESS: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentActivityState {
    Idle,
    Running,
    WaitingInput,
    Errored,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabPrimaryState {
    Idle,
    Running,
    Done,
    Alert,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentActivityOrigin {
    Socket,
    Termprop,
    OutputScan,
}

#[derive(Clone, Debug)]
pub struct AgentActivity {
    pub state: AgentActivityState,
    pub text: String,
    pub source: Option<String>,
    pub origin: AgentActivityOrigin,
    pub updated_at: Instant,
    /// Wall-clock time of the actual signal receipt. Rendering and polling
    /// must never advance this: it identifies when the evidence was verified.
    pub observed_at_unix_ms: u64,
}

/// Last provider-explicit observation for a pane, retained independently of
/// the short-lived activity badge. This is ordering evidence only: it cannot
/// create an attention row by itself.
#[derive(Clone, Debug)]
pub(crate) struct PaneExplicitObservation {
    pub state: AgentActivityState,
    pub source: Option<String>,
    pub origin: AgentActivityOrigin,
    pub updated_at: Instant,
    pub observed_at_unix_ms: u64,
}

impl PaneExplicitObservation {
    fn from_activity(activity: &AgentActivity) -> Option<Self> {
        matches!(
            activity.origin,
            AgentActivityOrigin::Socket | AgentActivityOrigin::Termprop
        )
        .then(|| Self {
            state: activity.state,
            source: activity.source.clone(),
            origin: activity.origin,
            updated_at: activity.updated_at,
            observed_at_unix_ms: activity.observed_at_unix_ms,
        })
    }

    pub(crate) fn is_fresh(&self) -> bool {
        self.updated_at.elapsed() < EXPLICIT_ACTIVITY_FRESHNESS
    }
}

impl AgentActivity {
    fn build(
        state: AgentActivityState,
        text: impl Into<String>,
        source: Option<String>,
        origin: AgentActivityOrigin,
    ) -> Option<Self> {
        if matches!(state, AgentActivityState::Idle) {
            return None;
        }

        let text = match text.into().trim() {
            "" if matches!(state, AgentActivityState::Running) => "working".to_string(),
            "" if matches!(state, AgentActivityState::WaitingInput) => {
                "waiting for input".to_string()
            }
            "" if matches!(state, AgentActivityState::Errored) => "errored".to_string(),
            "" if matches!(state, AgentActivityState::Done) => "done".to_string(),
            text => text.to_string(),
        };
        let source = source.and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        });

        Some(Self {
            state,
            text,
            source,
            origin,
            updated_at: Instant::now(),
            observed_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or_default(),
        })
    }

    pub fn socket(
        state: AgentActivityState,
        text: impl Into<String>,
        source: Option<String>,
    ) -> Option<Self> {
        Self::build(state, text, source, AgentActivityOrigin::Socket)
    }

    pub fn output_scan(
        state: AgentActivityState,
        text: impl Into<String>,
        source: Option<String>,
    ) -> Option<Self> {
        Self::build(state, text, source, AgentActivityOrigin::OutputScan)
    }

    pub fn termprop(
        state: AgentActivityState,
        text: impl Into<String>,
        source: Option<String>,
    ) -> Option<Self> {
        Self::build(state, text, source, AgentActivityOrigin::Termprop)
    }

    /// How long this signal stays authoritative, by origin.
    fn freshness_window(&self) -> Duration {
        match self.origin {
            AgentActivityOrigin::Socket | AgentActivityOrigin::Termprop => {
                EXPLICIT_ACTIVITY_FRESHNESS
            }
            AgentActivityOrigin::OutputScan => OUTPUT_SCAN_ACTIVITY_FRESHNESS,
        }
    }

    /// Whether this signal is still within its origin's freshness window,
    /// regardless of which state it reports.
    pub fn is_fresh(&self) -> bool {
        self.updated_at.elapsed() < self.freshness_window()
    }

    pub fn has_fresh_explicit_update(&self) -> bool {
        matches!(
            self.origin,
            AgentActivityOrigin::Socket | AgentActivityOrigin::Termprop
        ) && self.is_fresh()
    }

    pub fn is_fresh_running_signal(&self) -> bool {
        matches!(self.state, AgentActivityState::Running) && self.is_fresh()
    }
}

/// Conventional workspace actions mapped to mise task names.
/// Any repo that defines these mise tasks gets them as palette actions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WorkspaceAction {
    Setup, // mise run setup — one-time project bootstrap
    Dev,   // mise run dev   — start dev server with hot reload
    Test,  // mise run test  — run test suite
    Build, // mise run build — production build
    Lint,  // mise run lint  — code quality checks
}

impl WorkspaceAction {
    /// The mise task name this action maps to.
    pub fn task_name(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Dev => "dev",
            Self::Test => "test",
            Self::Build => "build",
            Self::Lint => "lint",
        }
    }

    /// Human-readable label for the palette.
    pub fn label(self) -> &'static str {
        match self {
            Self::Setup => "Setup",
            Self::Dev => "Dev Server",
            Self::Test => "Test",
            Self::Build => "Build",
            Self::Lint => "Lint",
        }
    }

    /// Compact label for buttons and chord overlays.
    pub fn compact_label(self) -> &'static str {
        match self {
            Self::Setup => "Setup",
            Self::Dev => "Dev",
            Self::Test => "Test",
            Self::Build => "Build",
            Self::Lint => "Lint",
        }
    }

    /// Single-key chord used by the quick action overlay.
    pub fn quick_key(self) -> char {
        match self {
            Self::Setup => 's',
            Self::Dev => 'd',
            Self::Test => 't',
            Self::Build => 'b',
            Self::Lint => 'l',
        }
    }

    /// All conventional actions in display order.
    pub fn all() -> &'static [WorkspaceAction] {
        &[Self::Dev, Self::Test, Self::Build, Self::Lint, Self::Setup]
    }

    /// Resolve the stable task vocabulary accepted in `keybindings.toml`.
    ///
    /// Chords keep this enum after parsing; task names are only an external
    /// configuration representation and never a GTK action name.
    pub fn from_task_name(name: &str) -> Option<Self> {
        Self::all()
            .iter()
            .copied()
            .find(|action| action.task_name() == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceTaskButton {
    pub label: String,
    pub task_name: String,
}

impl WorkspaceTaskButton {
    pub fn from_action(action: WorkspaceAction) -> Self {
        Self {
            label: action.compact_label().to_string(),
            task_name: action.task_name().to_string(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TabKind {
    #[default]
    Terminal,
    Dashboard,
}

pub struct Tab {
    pub id: u32,
    pub name: String,
    /// Stable, opaque identity for work-ledger correlation. Unlike a tab id,
    /// name, or CWD this survives restore and rename without collisions.
    pub work_origin: String,
    pub kind: TabKind,
    pub panes: Box<PaneNode>,
    pub focused_pane_id: u32,
    pub next_pane_id: u32,
    pub pane_zoom: Option<PaneZoomState>,
    pub close_on_exit: bool,
    pub respawn_on_exit: Option<ExitRespawn>,
    pub agent_running: bool,
    pub agent_name: Option<String>,
    pub agent_session_id: Option<String>,
    pub agent_pane_id: Option<u32>,
    pub listening_ports: Vec<u16>,
    pub listening_ports_updated_at_unix_ms: Option<u64>,
    pub socket_agent_activity: Option<AgentActivity>,
    pub pane_agent_activity: HashMap<u32, AgentActivity>,
    /// Provider-explicit ordering evidence that survives visible activity
    /// expiry. Pane removal or child/session replacement resets it explicitly.
    pub(crate) pane_explicit_observation: HashMap<u32, PaneExplicitObservation>,
    /// Native transcript turn evidence per pane, mirrored here from
    /// [`crate::AppState::pane_transcripts`] so tab-level state derivation
    /// needs nothing but the tab. See [`crate::agents::lifecycle`].
    pub(crate) pane_turn: HashMap<u32, crate::agents::PaneTurn>,
    pub agent_activity: Option<AgentActivity>,
    pub needs_attention: bool,
    pub notified: bool,
    pub notification_msg: Option<String>,
    /// Pane that produced the current `notification_msg`, so notification
    /// activation can focus the exact pane (not just the tab).
    pub notification_pane_id: Option<u32>,
    /// Last agent-activity state we fired a desktop notification for, per pane.
    /// Used to suppress re-firing on every poll for an unchanged (pane, state):
    /// a transition only notifies when the state actually changes.
    pub pane_last_notified: HashMap<u32, AgentActivityState>,
    /// If this tab is running a workspace action, tracks which one.
    pub workspace_action: Option<WorkspaceAction>,
    /// CWD used when the user triggered discovery for this tab.
    pub discovery_cwd: Option<String>,
    /// Mise standard actions discovered for this tab's CWD.
    pub discovered_actions: Vec<WorkspaceAction>,
    /// Task buttons surfaced for this tab's overlay and inspector UI.
    pub task_buttons: Vec<WorkspaceTaskButton>,
    /// Tracking data loaded from `.plan/features.json` at this tab's CWD.
    pub tracking_data: Option<crate::tracking::TrackingData>,
}

fn new_work_origin(prefix: &str) -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        static FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = FALLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let mixed = now ^ ((u128::from(std::process::id())) << 64) ^ u128::from(counter);
        bytes = mixed.to_le_bytes();
    }
    let mut rendered = String::with_capacity(prefix.len() + 33);
    rendered.push_str(prefix);
    rendered.push('-');
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(rendered, "{byte:02x}");
    }
    rendered
}

pub(crate) fn valid_work_origin(value: &str) -> bool {
    (5..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(crate) fn new_workspace_work_origin() -> String {
    new_work_origin("workspace")
}

pub(crate) fn new_tab_work_origin() -> String {
    new_work_origin("tab")
}

impl Tab {
    /// Rank a pane for "which agent speaks for this tab", using the same
    /// resolved lifecycle every surface renders rather than the raw signal.
    fn agent_pane_priority(&self, pane_id: u32, now_unix_ms: u64) -> u8 {
        match self.pane_lifecycle_at(pane_id, now_unix_ms) {
            AgentLifecycle::WaitingInput | AgentLifecycle::Errored => 3,
            AgentLifecycle::Working => 2,
            AgentLifecycle::Done => 1,
            AgentLifecycle::Idle => 0,
        }
    }

    pub fn primary_agent_activity(&self) -> Option<(u32, &AgentActivity)> {
        let now_unix_ms = crate::events::unix_time_ms();
        self.pane_agent_activity
            .iter()
            .max_by_key(|(pane_id, activity)| {
                (
                    self.agent_pane_priority(**pane_id, now_unix_ms),
                    activity.updated_at,
                )
            })
            .map(|(pane_id, activity)| (*pane_id, activity))
    }

    /// Record this pane's native transcript turn. Returns whether anything a
    /// surface renders actually moved, so callers can gate a redraw.
    pub(crate) fn set_pane_turn(&mut self, pane_id: u32, turn: crate::agents::PaneTurn) -> bool {
        self.pane_turn.insert(pane_id, turn) != Some(turn)
    }

    pub fn clear_pane_turn(&mut self, pane_id: u32) -> bool {
        self.pane_turn.remove(&pane_id).is_some()
    }

    /// Drop native turn evidence for every pane outside `pane_ids`. A vanished
    /// agent process cannot still be mid-turn.
    pub fn retain_pane_turns(&mut self, pane_ids: &[u32]) -> bool {
        let before = self.pane_turn.len();
        self.pane_turn
            .retain(|pane_id, _| pane_ids.contains(pane_id));
        self.pane_turn.len() != before
    }

    pub(crate) fn pane_turn(&self, pane_id: u32) -> Option<crate::agents::PaneTurn> {
        self.pane_turn.get(&pane_id).copied()
    }

    /// The canonical resolved lifecycle for one pane. Every surface that
    /// renders agent state goes through here.
    pub(crate) fn pane_lifecycle_at(&self, pane_id: u32, now_unix_ms: u64) -> AgentLifecycle {
        crate::agents::resolve_agent_lifecycle(
            self.pane_agent_activity.get(&pane_id),
            self.pane_turn(pane_id),
            now_unix_ms,
        )
    }

    pub(crate) fn pane_lifecycle(&self, pane_id: u32) -> AgentLifecycle {
        self.pane_lifecycle_at(pane_id, crate::events::unix_time_ms())
    }

    /// Resolved lifecycle for every pane this tab has any agent evidence for.
    fn pane_lifecycles_at(&self, now_unix_ms: u64) -> impl Iterator<Item = AgentLifecycle> + '_ {
        self.pane_agent_activity
            .keys()
            .chain(self.pane_turn.keys())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(move |pane_id| self.pane_lifecycle_at(*pane_id, now_unix_ms))
    }

    fn refresh_agent_activity_summary(&mut self) {
        self.agent_activity = self
            .primary_agent_activity()
            .map(|(_, activity)| activity.clone());
    }

    pub fn primary_agent_activity_pane_id(&self) -> Option<u32> {
        self.primary_agent_activity().map(|(pane_id, _)| pane_id)
    }

    pub fn set_socket_agent_activity(&mut self, activity: Option<AgentActivity>) {
        self.socket_agent_activity = activity;
        self.refresh_agent_activity_summary();
    }

    pub fn clear_socket_agent_activity_if(
        &mut self,
        should_clear: impl FnOnce(&AgentActivity) -> bool,
    ) -> bool {
        let matches = self
            .socket_agent_activity
            .as_ref()
            .is_some_and(should_clear);
        if matches {
            self.socket_agent_activity = None;
            self.refresh_agent_activity_summary();
        }
        matches
    }

    pub fn set_pane_agent_activity(&mut self, pane_id: u32, activity: Option<AgentActivity>) {
        if let Some(activity) = activity {
            if let Some(observation) = PaneExplicitObservation::from_activity(&activity) {
                self.pane_explicit_observation.insert(pane_id, observation);
            }
            self.pane_agent_activity.insert(pane_id, activity);
        } else {
            self.pane_agent_activity.remove(&pane_id);
        }
        self.refresh_agent_activity_summary();
    }

    pub fn pane_agent_activity(&self, pane_id: u32) -> Option<&AgentActivity> {
        self.pane_agent_activity.get(&pane_id)
    }

    pub(crate) fn pane_explicit_observation(
        &self,
        pane_id: u32,
    ) -> Option<&PaneExplicitObservation> {
        self.pane_explicit_observation.get(&pane_id)
    }

    /// Forget retirement evidence from a replaced provider session once its
    /// visible activity has already cleared. A current matching explicit
    /// signal may belong to the newly discovered session and stays intact.
    pub(crate) fn clear_retired_pane_explicit_observation(&mut self, pane_id: u32) -> bool {
        let backed_by_current_signal = self
            .pane_explicit_observation
            .get(&pane_id)
            .zip(self.pane_agent_activity.get(&pane_id))
            .is_some_and(|(observation, activity)| {
                activity.is_fresh()
                    && matches!(
                        activity.origin,
                        AgentActivityOrigin::Socket | AgentActivityOrigin::Termprop
                    )
                    && observation.state == activity.state
                    && observation.source == activity.source
                    && observation.origin == activity.origin
                    && observation.updated_at == activity.updated_at
                    && observation.observed_at_unix_ms == activity.observed_at_unix_ms
            });
        !backed_by_current_signal && self.pane_explicit_observation.remove(&pane_id).is_some()
    }

    /// Record an explicit idle signal before removing the visible activity.
    /// Clearing the badge must not erase the fact that this observation came
    /// after an older transcript error.
    pub(crate) fn note_explicit_pane_idle(
        &mut self,
        pane_id: u32,
        source: Option<String>,
        origin: AgentActivityOrigin,
    ) {
        debug_assert!(matches!(
            origin,
            AgentActivityOrigin::Socket | AgentActivityOrigin::Termprop
        ));
        self.pane_explicit_observation.insert(
            pane_id,
            PaneExplicitObservation {
                state: AgentActivityState::Idle,
                source,
                origin,
                updated_at: Instant::now(),
                observed_at_unix_ms: crate::events::unix_time_ms(),
            },
        );
    }

    /// Decide whether a desktop notification should fire for a pane entering
    /// `state`, and record it if so. Returns `true` only on a genuine state
    /// change for that pane; repeated identical scans (same pane, same state)
    /// return `false`, so a Codex "done" is not resuppressed by a Claude event
    /// and does not re-fire on every poll. `None` states (idle) never notify
    /// and clear the pane's remembered state.
    pub fn note_pane_notification(
        &mut self,
        pane_id: u32,
        state: Option<AgentActivityState>,
    ) -> bool {
        match state {
            None | Some(AgentActivityState::Idle) => {
                self.pane_last_notified.remove(&pane_id);
                false
            }
            Some(state) => {
                let changed = self.pane_last_notified.get(&pane_id) != Some(&state);
                if changed {
                    self.pane_last_notified.insert(pane_id, state);
                }
                changed
            }
        }
    }

    /// Forget a pane's last-notified state (e.g. when its agent exits) so a
    /// future re-entry into the same state notifies again.
    pub fn clear_pane_notification(&mut self, pane_id: u32) {
        self.pane_last_notified.remove(&pane_id);
    }

    pub fn clear_pane_agent_activity_if(
        &mut self,
        pane_id: u32,
        should_clear: impl FnOnce(&AgentActivity) -> bool,
    ) -> bool {
        let matches = self
            .pane_agent_activity
            .get(&pane_id)
            .is_some_and(should_clear);
        if matches {
            self.pane_agent_activity.remove(&pane_id);
            self.refresh_agent_activity_summary();
        }
        matches
    }

    pub(crate) fn prune_done_pane_agent_activity(
        &mut self,
        pane_id: u32,
        origin: AgentActivityOrigin,
        updated_at: Instant,
    ) -> bool {
        self.clear_pane_agent_activity_if(pane_id, |activity| {
            matches!(activity.state, AgentActivityState::Done)
                && activity.origin == origin
                && activity.updated_at == updated_at
        })
    }

    pub fn clear_pane_agent_activity(&mut self, pane_id: u32) -> bool {
        let removed = self.pane_agent_activity.remove(&pane_id).is_some();
        if removed {
            self.refresh_agent_activity_summary();
        }
        removed
    }

    /// Reset evidence when the pane identity ends or is replaced. Ordinary
    /// badge expiry deliberately uses `clear_pane_agent_activity` so the last
    /// explicit observation remains available for transcript ordering.
    pub(crate) fn reset_pane_agent_activity_evidence(&mut self, pane_id: u32) -> bool {
        let activity_removed = self.clear_pane_agent_activity(pane_id);
        let observation_removed = self.pane_explicit_observation.remove(&pane_id).is_some();
        let turn_removed = self.clear_pane_turn(pane_id);
        activity_removed || observation_removed || turn_removed
    }

    pub fn clear_pane_agent_activities_if(
        &mut self,
        should_clear: impl Fn(&AgentActivity) -> bool,
    ) -> bool {
        let before = self.pane_agent_activity.len();
        self.pane_agent_activity
            .retain(|_, activity| !should_clear(activity));
        let changed = self.pane_agent_activity.len() != before;
        if changed {
            self.refresh_agent_activity_summary();
        }
        changed
    }

    pub fn clear_output_scan_activities_for_panes(&mut self, pane_ids: &[u32]) -> bool {
        if pane_ids.is_empty() {
            return false;
        }

        let before = self.pane_agent_activity.len();
        self.pane_agent_activity.retain(|pane_id, activity| {
            !(pane_ids.contains(pane_id)
                && matches!(activity.origin, AgentActivityOrigin::OutputScan))
        });
        let changed = self.pane_agent_activity.len() != before;
        if changed {
            self.refresh_agent_activity_summary();
        }
        changed
    }

    pub fn clear_running_output_scan_activities_for_panes(&mut self, pane_ids: &[u32]) -> bool {
        if pane_ids.is_empty() {
            return false;
        }

        let before = self.pane_agent_activity.len();
        self.pane_agent_activity.retain(|pane_id, activity| {
            !(pane_ids.contains(pane_id)
                && matches!(activity.origin, AgentActivityOrigin::OutputScan)
                && matches!(activity.state, AgentActivityState::Running))
        });
        let changed = self.pane_agent_activity.len() != before;
        if changed {
            self.refresh_agent_activity_summary();
        }
        changed
    }

    /// Whether any pane in this tab currently owns an agent turn. Named for
    /// history; the answer now comes from the canonical state machine, so an
    /// agent thinking silently for minutes still counts as running.
    pub fn has_fresh_running_activity(&self) -> bool {
        let now_unix_ms = crate::events::unix_time_ms();
        self.pane_lifecycles_at(now_unix_ms)
            .any(AgentLifecycle::is_working)
    }

    pub fn is_running_now(&self) -> bool {
        self.has_fresh_running_activity()
    }

    pub fn primary_state(&self) -> TabPrimaryState {
        let now_unix_ms = crate::events::unix_time_ms();
        let strongest =
            crate::agents::strongest_agent_lifecycle(self.pane_lifecycles_at(now_unix_ms));
        if self.needs_attention || strongest.is_some_and(AgentLifecycle::needs_attention) {
            return TabPrimaryState::Alert;
        }
        match strongest {
            Some(AgentLifecycle::Working) => TabPrimaryState::Running,
            Some(AgentLifecycle::Done) => TabPrimaryState::Done,
            _ => {
                // The tab-level summary can still carry a Done from a socket
                // client that reported against the tab rather than a pane.
                if self
                    .agent_activity
                    .as_ref()
                    .is_some_and(|activity| matches!(activity.state, AgentActivityState::Done))
                {
                    TabPrimaryState::Done
                } else {
                    TabPrimaryState::Idle
                }
            }
        }
    }

    pub fn clear_running_activity(&mut self) -> bool {
        let socket_cleared = self.clear_socket_agent_activity_if(|activity| {
            matches!(activity.state, AgentActivityState::Running)
        });
        let pane_cleared = self.clear_pane_agent_activities_if(|activity| {
            matches!(activity.state, AgentActivityState::Running)
        });
        socket_cleared || pane_cleared
    }

    /// Path used for sidebar tab search: the focused pane's working directory.
    /// Remote panes yield a `file://host/path` form so a search can match either
    /// the host or the path; local panes yield the raw path. `None` when unknown.
    pub fn search_cwd(&self) -> Option<String> {
        self.panes
            .leaf(self.focused_pane_id)
            .and_then(|leaf| leaf.saved_cwd())
    }

    /// Returns the running command of the focused tmux-backed pane, if any.
    /// Returns None if the focused pane is not tmux-backed or has no pane info.
    pub fn tmux_running_command(&self) -> Option<&str> {
        let leaf = self.panes.leaf(self.focused_pane_id)?;
        let backing = leaf.tmux_backing.as_ref()?;
        let info = backing.pane_info.value()?;
        // Don't show shell names as "running commands" — they're the default
        if crate::tmux::is_shell_command(&info.current_command) {
            return None;
        }
        Some(&info.current_command)
    }
}

#[derive(Clone)]
pub struct PaneZoomState {
    pub pane_id: u32,
    pub ancestors: Vec<PaneZoomAncestor>,
}

#[derive(Clone)]
pub struct PaneZoomAncestor {
    pub widget: gtk::Paned,
    pub target_child: SplitChild,
    pub ratio: f64,
}

#[cfg(test)]
mod tests {
    use super::{
        AgentActivity, AgentActivityOrigin, AgentActivityState, Tab, TabKind, TabPrimaryState,
        EXPLICIT_ACTIVITY_FRESHNESS, OUTPUT_SCAN_ACTIVITY_FRESHNESS,
    };
    use crate::pane::PaneNode;
    use std::collections::HashMap;
    use std::time::Instant;

    fn stub_tab(id: u32) -> Tab {
        Tab {
            id,
            name: format!("Tab {id}"),
            work_origin: super::new_tab_work_origin(),
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

    fn activity(
        state: AgentActivityState,
        origin: AgentActivityOrigin,
        age_seconds: u64,
    ) -> AgentActivity {
        AgentActivity {
            state,
            text: "working".to_string(),
            source: Some("copilot".to_string()),
            origin,
            updated_at: Instant::now() - std::time::Duration::from_secs(age_seconds),
            observed_at_unix_ms: crate::events::unix_time_ms()
                .saturating_sub(age_seconds.saturating_mul(1_000)),
        }
    }

    fn set_activity(tab: &mut Tab, activity: AgentActivity) {
        tab.set_pane_agent_activity(tab.focused_pane_id, Some(activity));
    }

    #[test]
    fn note_pane_notification_dedups_per_pane_and_state() {
        let mut tab = stub_tab(1);

        // First transition into Done for pane 4 fires.
        assert!(tab.note_pane_notification(4, Some(AgentActivityState::Done)));
        // A second identical scan (same pane, same state) does NOT re-fire.
        assert!(!tab.note_pane_notification(4, Some(AgentActivityState::Done)));

        // A different pane is independent: pane 5 waiting still fires even
        // though pane 4 already notified.
        assert!(tab.note_pane_notification(5, Some(AgentActivityState::WaitingInput)));
        assert!(!tab.note_pane_notification(5, Some(AgentActivityState::WaitingInput)));

        // A genuine state CHANGE on pane 4 fires again.
        assert!(tab.note_pane_notification(4, Some(AgentActivityState::WaitingInput)));

        // Idle / None clears the pane so re-entry notifies again.
        assert!(!tab.note_pane_notification(4, None));
        assert!(tab.note_pane_notification(4, Some(AgentActivityState::WaitingInput)));

        // clear_pane_notification also forces the next notification.
        tab.clear_pane_notification(4);
        assert!(tab.note_pane_notification(4, Some(AgentActivityState::WaitingInput)));
    }

    #[test]
    fn primary_state_prefers_alert_over_running_and_done() {
        let mut tab = stub_tab(1);
        tab.needs_attention = true;
        tab.agent_running = true;
        tab.agent_activity = Some(activity(
            AgentActivityState::Done,
            AgentActivityOrigin::Socket,
            0,
        ));

        assert_eq!(tab.primary_state(), TabPrimaryState::Alert);
    }

    #[test]
    fn primary_state_uses_alert_for_waiting_input_activity() {
        let mut tab = stub_tab(1);
        set_activity(
            &mut tab,
            activity(
                AgentActivityState::WaitingInput,
                AgentActivityOrigin::Termprop,
                0,
            ),
        );

        assert_eq!(tab.primary_state(), TabPrimaryState::Alert);
    }

    #[test]
    fn primary_state_uses_alert_for_errored_activity() {
        let mut tab = stub_tab(1);
        set_activity(
            &mut tab,
            activity(AgentActivityState::Errored, AgentActivityOrigin::Socket, 0),
        );

        assert_eq!(tab.primary_state(), TabPrimaryState::Alert);
    }

    #[test]
    fn primary_state_uses_done_when_not_running() {
        let mut tab = stub_tab(1);
        set_activity(
            &mut tab,
            activity(AgentActivityState::Done, AgentActivityOrigin::Socket, 0),
        );

        assert_eq!(tab.primary_state(), TabPrimaryState::Done);
    }

    #[test]
    fn primary_state_ignores_stale_running_activity_without_live_runtime() {
        let mut tab = stub_tab(1);
        set_activity(
            &mut tab,
            AgentActivity {
                updated_at: Instant::now()
                    - EXPLICIT_ACTIVITY_FRESHNESS
                    - std::time::Duration::from_secs(1),
                ..activity(AgentActivityState::Running, AgentActivityOrigin::Socket, 0)
            },
        );

        assert_eq!(tab.primary_state(), TabPrimaryState::Idle);
    }

    #[test]
    fn primary_state_uses_fresh_explicit_running_activity() {
        let mut tab = stub_tab(1);
        set_activity(
            &mut tab,
            activity(AgentActivityState::Running, AgentActivityOrigin::Socket, 0),
        );

        assert_eq!(tab.primary_state(), TabPrimaryState::Running);
    }

    #[test]
    fn primary_state_uses_fresh_output_scan_activity() {
        let mut tab = stub_tab(1);
        set_activity(
            &mut tab,
            activity(
                AgentActivityState::Running,
                AgentActivityOrigin::OutputScan,
                0,
            ),
        );

        assert_eq!(tab.primary_state(), TabPrimaryState::Running);
    }

    #[test]
    fn primary_state_ignores_stale_output_scan_activity() {
        let mut tab = stub_tab(1);
        set_activity(
            &mut tab,
            AgentActivity {
                updated_at: Instant::now()
                    - OUTPUT_SCAN_ACTIVITY_FRESHNESS
                    - std::time::Duration::from_secs(1),
                ..activity(
                    AgentActivityState::Running,
                    AgentActivityOrigin::OutputScan,
                    0,
                )
            },
        );

        assert_eq!(tab.primary_state(), TabPrimaryState::Idle);
    }

    #[test]
    fn primary_state_ignores_agent_process_without_fresh_activity() {
        let mut tab = stub_tab(1);
        tab.agent_running = true;
        tab.agent_name = Some("claude".to_string());

        assert_eq!(tab.primary_state(), TabPrimaryState::Idle);
    }

    #[test]
    fn primary_state_reads_running_from_an_open_native_turn_alone() {
        // No socket, no termprop, no output-scan line for a full minute: the
        // transcript is the only thing that knows the agent is still thinking.
        let mut tab = stub_tab(1);
        let now = crate::events::unix_time_ms();
        tab.set_pane_turn(
            4,
            crate::agents::PaneTurn::new(crate::agents::TurnPhase::Active, now - 60_000),
        );

        assert!(tab.has_fresh_running_activity());
        assert_eq!(tab.primary_state(), TabPrimaryState::Running);
    }

    #[test]
    fn primary_state_returns_to_idle_when_the_native_turn_completes() {
        let mut tab = stub_tab(1);
        let now = crate::events::unix_time_ms();
        tab.set_pane_turn(
            4,
            crate::agents::PaneTurn::new(crate::agents::TurnPhase::Completed, now - 60_000),
        );

        assert!(!tab.has_fresh_running_activity());
        assert_eq!(tab.primary_state(), TabPrimaryState::Idle);
    }

    #[test]
    fn primary_state_ignores_a_native_turn_older_than_the_freshness_window() {
        let mut tab = stub_tab(1);
        let now = crate::events::unix_time_ms();
        tab.set_pane_turn(
            4,
            crate::agents::PaneTurn::new(
                crate::agents::TurnPhase::Active,
                now - (crate::agents::lifecycle::TRANSCRIPT_TURN_FRESHNESS.as_millis() as u64 + 1),
            ),
        );

        assert!(!tab.has_fresh_running_activity());
        assert_eq!(tab.primary_state(), TabPrimaryState::Idle);
    }

    #[test]
    fn a_pane_whose_agent_exited_loses_its_native_turn() {
        let mut tab = stub_tab(1);
        let now = crate::events::unix_time_ms();
        let active = crate::agents::PaneTurn::new(crate::agents::TurnPhase::Active, now);
        tab.set_pane_turn(4, active);
        tab.set_pane_turn(5, active);

        assert!(tab.retain_pane_turns(&[4]));
        assert_eq!(tab.pane_turn(4), Some(active));
        assert_eq!(tab.pane_turn(5), None);
        // Retaining the same set again is a no-op, so it never marks a redraw.
        assert!(!tab.retain_pane_turns(&[4]));
    }

    #[test]
    fn a_live_permission_prompt_still_beats_an_open_turn_at_the_tab_level() {
        // Requirement: the transcript never records a y/n prompt, so a fresh
        // terminal WAITING signal must keep the tab in Alert.
        let mut tab = stub_tab(1);
        let now = crate::events::unix_time_ms();
        tab.set_pane_turn(
            4,
            crate::agents::PaneTurn::new(crate::agents::TurnPhase::Active, now),
        );
        tab.set_pane_agent_activity(
            4,
            Some(activity(
                AgentActivityState::WaitingInput,
                AgentActivityOrigin::OutputScan,
                1,
            )),
        );

        assert_eq!(tab.primary_state(), TabPrimaryState::Alert);
    }

    #[test]
    fn aggregate_activity_prefers_actionable_state_over_newer_completion() {
        let mut tab = stub_tab(1);
        tab.set_pane_agent_activity(
            4,
            Some(activity(
                AgentActivityState::Running,
                AgentActivityOrigin::Socket,
                1,
            )),
        );
        tab.set_pane_agent_activity(
            5,
            Some(activity(
                AgentActivityState::Done,
                AgentActivityOrigin::Socket,
                0,
            )),
        );

        assert_eq!(tab.primary_agent_activity_pane_id(), Some(4));
        assert!(matches!(
            tab.agent_activity.as_ref().map(|activity| activity.state),
            Some(AgentActivityState::Running)
        ));
        assert_eq!(tab.primary_state(), TabPrimaryState::Running);

        tab.set_pane_agent_activity(
            6,
            Some(activity(
                AgentActivityState::WaitingInput,
                AgentActivityOrigin::Socket,
                2,
            )),
        );
        assert_eq!(tab.primary_agent_activity_pane_id(), Some(6));
        assert_eq!(tab.primary_state(), TabPrimaryState::Alert);
    }

    #[test]
    fn clear_running_activity_removes_socket_and_pane_running_entries() {
        let mut tab = stub_tab(1);
        tab.socket_agent_activity = Some(activity(
            AgentActivityState::Running,
            AgentActivityOrigin::Socket,
            0,
        ));
        tab.pane_agent_activity.insert(
            tab.focused_pane_id,
            activity(
                AgentActivityState::Running,
                AgentActivityOrigin::OutputScan,
                0,
            ),
        );
        tab.agent_activity = tab.socket_agent_activity.clone();

        assert!(tab.clear_running_activity());
        assert!(tab.socket_agent_activity.is_none());
        assert!(tab.pane_agent_activity.is_empty());
        assert!(tab.agent_activity.is_none());
    }

    #[test]
    fn clear_output_scan_activities_for_panes_only_removes_targeted_entries() {
        let mut tab = stub_tab(1);
        let local_pane_id = tab.focused_pane_id;
        let other_pane_id = local_pane_id + 1;
        tab.pane_agent_activity.insert(
            local_pane_id,
            activity(
                AgentActivityState::Running,
                AgentActivityOrigin::OutputScan,
                0,
            ),
        );
        tab.pane_agent_activity.insert(
            other_pane_id,
            activity(
                AgentActivityState::Running,
                AgentActivityOrigin::OutputScan,
                0,
            ),
        );
        tab.pane_agent_activity.insert(
            other_pane_id + 1,
            activity(AgentActivityState::Running, AgentActivityOrigin::Socket, 0),
        );
        tab.refresh_agent_activity_summary();

        assert!(tab.clear_output_scan_activities_for_panes(&[local_pane_id]));
        assert!(!tab.pane_agent_activity.contains_key(&local_pane_id));
        assert!(tab.pane_agent_activity.contains_key(&other_pane_id));
        assert!(tab.pane_agent_activity.contains_key(&(other_pane_id + 1)));
    }

    #[test]
    fn clear_running_output_scan_activities_keeps_waiting_input_alerts() {
        let mut tab = stub_tab(1);
        let pane_id = tab.focused_pane_id;
        tab.pane_agent_activity.insert(
            pane_id,
            activity(
                AgentActivityState::WaitingInput,
                AgentActivityOrigin::OutputScan,
                0,
            ),
        );
        tab.refresh_agent_activity_summary();

        assert!(!tab.clear_running_output_scan_activities_for_panes(&[pane_id]));
        assert!(matches!(
            tab.pane_agent_activity
                .get(&pane_id)
                .map(|activity| activity.state),
            Some(AgentActivityState::WaitingInput)
        ));
        assert_eq!(tab.primary_state(), TabPrimaryState::Alert);
    }
}

#[derive(Clone, Debug)]
pub struct ExitRespawn {
    pub working_dir: Option<String>,
    pub argv: Vec<String>,
}
