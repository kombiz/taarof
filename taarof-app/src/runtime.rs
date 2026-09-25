use glib::prelude::Cast;
use gtk::prelude::WidgetExt;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use crate::{
    dashboard, events, instance, pane,
    probe::ProbeSnapshot,
    session,
    workspace::{ExitRespawn, Tab, TabKind, Workspace, WorkspaceStatus},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastScope {
    Tab,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChordMode {
    QuickAction,
    Leader,
}

impl BroadcastScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tab => "Active Tab",
            Self::Workspace => "Active Workspace",
        }
    }
}

pub struct AppState {
    pub workspaces: Vec<Workspace>,
    pub active_workspace: u32,
    /// Tab currently presented in the content stack. Dashboard tabs are
    /// presentation-only and may be shown here without replacing the active
    /// terminal tab used by pane- and command-targeting APIs.
    presented_tab: Option<u32>,
    last_active_workspace: Option<u32>,
    pub next_id: u32,
    pub headless_panes: HashMap<(u32, u32), HeadlessPaneState>,
    /// Tabs restored lazily: their live `panes` is `PaneNode::Empty` and the
    /// original saved layout waits here until the tab is first activated, at
    /// which point it is built and its shells spawned. Keyed by tab id.
    pub pending_tab_restores: HashMap<u32, session::PendingTabRestore>,
    /// Explicit notifications raised through the control socket. These are
    /// kept separately from agent-derived attention so activity transitions
    /// cannot dismiss a user-facing alert before its tab is focused.
    pending_socket_notifications: HashMap<u32, String>,
    restored_legends: HashMap<u32, session::RestoredTabLegend>,
    chord_mode: Option<ChordMode>,
    chord_generation: u64,
    broadcast_scope: Option<BroadcastScope>,
    pub detached_sessions: Vec<dashboard::DetachedSession>,
    pub dashboard_state: dashboard::DashboardState,
    pub(crate) dashboard_poll_tracker: dashboard::DashboardPollTracker,
    pub background_section_collapsed: bool,
    pub event_store: events::EventStore,
    pub work_ledger: crate::work_ledger::WorkLedger,
    pub history: crate::history::HistoryHandle,
    pub history_reader: crate::history::HistoryReader,
    pub selected_dashboard_view: Option<String>,
    pub discovery_binary_paths: HashMap<u32, PathBuf>,
    /// The exact target and task set that produced each tab's task affordances.
    /// A launch must prove this snapshot still belongs to the tab before it can
    /// start a task, so a stale button cannot run a task from a previous CWD.
    pub(crate) task_discovery_snapshots: HashMap<u32, crate::task_launch::TaskDiscoverySnapshot>,
    pub(crate) runtime_probe: Option<crate::runtime_probe::RuntimeProbeSnapshot>,
    pub pane_dirty: Arc<crate::http::PaneDirtyRegistry>,
    /// Tabs whose task UI was explicitly revealed this session (via "Discover
    /// Tasks"). Transient and not persisted, so restarts start clean even when
    /// `[tasks] enabled` is off — the config flag governs automatic display.
    pub revealed_task_tabs: HashSet<u32>,
    /// Per-pane live agent transcript summaries, mirrored on the main thread
    /// from the off-thread transcript worker so `api.rs` can serialize them
    /// without locking. Keyed by `(tab_id, pane_id)`.
    pub(crate) pane_transcripts: HashMap<(u32, u32), crate::agents::TranscriptState>,
    /// Desktop-side state for the remote-device pairing confirmation channel.
    /// The control gateway owns the authoritative device records; this mirrors
    /// offers and operator decisions for the local socket surface.
    pub pairing: crate::socket::PairingCoordinator,
    /// Last rendered canonical Needs You projection. The periodic runtime poll
    /// compares against it so freshness expiry redraws GTK and invalidates web
    /// state even when no new terminal output arrives.
    attention_projection_signature: Vec<crate::attention::AttentionTarget>,
}

#[derive(Clone, Debug, Default)]
pub struct HeadlessPaneState {
    pub work_origin: String,
    pub shell_running: bool,
    pub tmux_backing: Option<pane::TmuxBacking>,
    pub location_state: pane::PaneLocationState,
    pub process_state: pane::PaneProcessState,
    pub current_task: Option<crate::task_binding::PaneTaskBinding>,
}

pub struct TerminalTabRegistration {
    pub name: String,
    pub panes: Box<pane::PaneNode>,
    pub focused_pane_id: u32,
    pub next_pane_id: u32,
    pub close_on_exit: bool,
    pub respawn_on_exit: Option<ExitRespawn>,
}

pub struct ClosedPaneState {
    pub focus_terminal: Option<vte::Terminal>,
    pub deferred_stack_widget: Option<gtk::Widget>,
    pub tmux_backing: Option<pane::TmuxBacking>,
    pub workspace_name: String,
}

#[derive(Clone)]
pub struct RuntimeHandle {
    state: Rc<RefCell<AppState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveTabError {
    TabNotFound,
    WorkspaceNotFound,
    AlreadyInWorkspace,
}

impl AppState {
    pub fn new() -> Self {
        let (history, history_reader) =
            crate::history::HistoryHandle::open(crate::config::history_config());
        let mut event_store = events::EventStore::default();
        event_store.install_history_sink(history.clone());
        let mut work_ledger = crate::work_ledger::WorkLedger::new();
        work_ledger.install_history_sink(history.clone());
        crate::diagnostics::install_history_sink(history.clone());
        let mut s = Self {
            workspaces: Vec::new(),
            active_workspace: 0,
            presented_tab: None,
            last_active_workspace: None,
            next_id: 1,
            headless_panes: HashMap::new(),
            pending_tab_restores: HashMap::new(),
            pending_socket_notifications: HashMap::new(),
            restored_legends: HashMap::new(),
            chord_mode: None,
            chord_generation: 0,
            broadcast_scope: None,
            detached_sessions: Vec::new(),
            dashboard_state: dashboard::DashboardState::default(),
            dashboard_poll_tracker: dashboard::DashboardPollTracker::default(),
            background_section_collapsed: true,
            event_store,
            work_ledger,
            history,
            history_reader,
            selected_dashboard_view: None,
            discovery_binary_paths: HashMap::new(),
            task_discovery_snapshots: HashMap::new(),
            runtime_probe: None,
            pane_dirty: Arc::new(crate::http::PaneDirtyRegistry::default()),
            revealed_task_tabs: HashSet::new(),
            pane_transcripts: HashMap::new(),
            pairing: crate::socket::PairingCoordinator::new(),
            attention_projection_signature: Vec::new(),
        };
        s.event_store.emit(
            "session_started",
            serde_json::json!({
                "session_name": instance::session_name(),
            }),
        );
        s.create_workspace("default", None);
        s
    }

    pub fn active_ws(&self) -> Option<&Workspace> {
        self.workspaces
            .iter()
            .find(|w| w.id == self.active_workspace)
    }

    pub fn active_ws_mut(&mut self) -> Option<&mut Workspace> {
        self.workspaces
            .iter_mut()
            .find(|w| w.id == self.active_workspace)
    }

    pub(crate) fn begin_dashboard_poll(
        &mut self,
        targets: &[crate::tmux::TmuxTarget],
    ) -> dashboard::DashboardPollContext {
        self.dashboard_poll_tracker.begin(targets)
    }

    pub(crate) fn invalidate_dashboard_session_snapshot(
        &mut self,
        target: &crate::tmux::TmuxTarget,
        session_name: &str,
    ) {
        self.dashboard_poll_tracker.invalidate_target(target);
        self.dashboard_state
            .sessions
            .retain(|session| !(session.target == *target && session.name == session_name));
        let host_name = match target {
            crate::tmux::TmuxTarget::Local => "localhost",
            crate::tmux::TmuxTarget::Remote { ssh_target } => ssh_target,
        };
        let session_count = self
            .dashboard_state
            .sessions
            .iter()
            .filter(|session| session.host == host_name)
            .count() as u32;
        if let Some(host) = self
            .dashboard_state
            .hosts
            .iter_mut()
            .find(|host| host.name == host_name)
        {
            host.session_count = session_count;
        }
    }

    /// Active terminal used by implicit pane, clipboard, broadcast, task, and
    /// socket actions. Presentation-only tabs are intentionally excluded.
    pub fn active_tab(&self) -> Option<&Tab> {
        let ws = self.active_ws()?;
        ws.tabs
            .iter()
            .find(|tab| tab.id == ws.active_tab && tab.kind == TabKind::Terminal)
    }

    /// Mutable counterpart to [`Self::active_tab`].
    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        let ws = self.active_ws_mut()?;
        let at = ws.active_tab;
        ws.tabs
            .iter_mut()
            .find(|tab| tab.id == at && tab.kind == TabKind::Terminal)
    }

    pub fn all_tabs(&self) -> impl Iterator<Item = &Tab> {
        self.workspaces.iter().flat_map(|w| w.tabs.iter())
    }

    pub fn all_tabs_mut(&mut self) -> impl Iterator<Item = &mut Tab> {
        self.workspaces.iter_mut().flat_map(|w| w.tabs.iter_mut())
    }

    pub fn find_tab(&self, tab_id: u32) -> Option<(&Workspace, &Tab)> {
        for ws in &self.workspaces {
            if let Some(tab) = ws.tabs.iter().find(|t| t.id == tab_id) {
                return Some((ws, tab));
            }
        }
        None
    }

    pub fn find_tab_mut(&mut self, tab_id: u32) -> Option<&mut Tab> {
        self.workspaces
            .iter_mut()
            .flat_map(|w| w.tabs.iter_mut())
            .find(|t| t.id == tab_id)
    }

    /// Apply a persisted work origin while rejecting malformed or duplicate
    /// values. The tab's freshly generated token remains for legacy, template,
    /// corrupt, or colliding session data.
    pub(crate) fn apply_restored_tab_work_origin(
        &mut self,
        tab_id: u32,
        candidate: Option<&str>,
    ) -> String {
        let Some(candidate) = candidate
            .map(str::trim)
            .filter(|value| crate::workspace::valid_work_origin(value))
        else {
            return self
                .find_tab(tab_id)
                .map(|(_, tab)| tab.work_origin.clone())
                .unwrap_or_default();
        };
        let collision = self
            .all_tabs()
            .any(|tab| tab.id != tab_id && tab.work_origin == candidate);
        let Some(tab) = self.find_tab_mut(tab_id) else {
            return String::new();
        };
        if !collision {
            tab.work_origin = candidate.to_string();
        }
        tab.work_origin.clone()
    }

    pub(crate) fn apply_restored_workspace_work_origin(
        &mut self,
        workspace_id: u32,
        candidate: Option<&str>,
    ) -> String {
        let Some(candidate) = candidate
            .map(str::trim)
            .filter(|value| crate::workspace::valid_work_origin(value))
        else {
            return self
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .map(|workspace| workspace.work_origin.clone())
                .unwrap_or_default();
        };
        let collision = self
            .workspaces
            .iter()
            .any(|workspace| workspace.id != workspace_id && workspace.work_origin == candidate);
        let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
        else {
            return String::new();
        };
        if !collision {
            workspace.work_origin = candidate.to_string();
        }
        workspace.work_origin.clone()
    }

    pub fn create_workspace(&mut self, name: &str, repo_root: Option<String>) -> u32 {
        if self
            .workspaces
            .iter()
            .any(|ws| ws.id == self.active_workspace && ws.id != 0)
        {
            self.last_active_workspace = Some(self.active_workspace);
        }
        let id = self.next_id;
        self.next_id += 1;
        let repo_root_for_event = repo_root.clone();
        self.workspaces.push(Workspace {
            id,
            work_origin: crate::workspace::new_workspace_work_origin(),
            name: name.to_string(),
            collapsed: false,
            repo_root,
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
            host_status: ProbeSnapshot::default(),
        });
        self.active_workspace = id;
        self.presented_tab = None;
        self.event_store.emit(
            "workspace_created",
            serde_json::json!({
                "workspace_id": id,
                "name": name,
                "repo_root": repo_root_for_event,
            }),
        );
        id
    }

    fn has_workspace(&self, ws_id: u32) -> bool {
        self.workspaces.iter().any(|ws| ws.id == ws_id)
    }

    pub fn create_dashboard_tab(&mut self) -> Option<u32> {
        let existing_id = self
            .active_ws()?
            .tabs
            .iter()
            .find(|tab| tab.kind == TabKind::Dashboard)
            .map(|tab| tab.id);
        if let Some(id) = existing_id {
            let _ = self.present_tab(id);
            return None;
        }

        let tab_id = self.next_id;
        self.next_id += 1;

        let ws = self.active_ws_mut()?;
        ws.tabs.push(Tab {
            id: tab_id,
            name: "Dashboard".into(),
            work_origin: crate::workspace::new_tab_work_origin(),
            kind: TabKind::Dashboard,
            panes: Box::new(pane::PaneNode::Empty),
            focused_pane_id: 0,
            next_pane_id: 0,
            pane_zoom: None,
            close_on_exit: false,
            respawn_on_exit: None,
            agent_running: false,
            agent_name: None,
            agent_session_id: None,
            agent_pane_id: None,
            listening_ports: vec![],
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
            discovered_actions: vec![],
            task_buttons: vec![],
            tracking_data: None,
        });
        let _ = self.present_tab(tab_id);
        Some(tab_id)
    }

    fn sanitize_workspace_navigation(workspace: &mut Workspace) {
        if !workspace
            .tabs
            .iter()
            .any(|tab| tab.id == workspace.active_tab && tab.kind == TabKind::Terminal)
        {
            workspace.active_tab = workspace
                .tabs
                .iter()
                .find(|tab| tab.kind == TabKind::Terminal)
                .map(|tab| tab.id)
                .unwrap_or(0);
        }

        if workspace.last_active_tab == Some(workspace.active_tab)
            || workspace.last_active_tab.is_some_and(|tab_id| {
                !workspace
                    .tabs
                    .iter()
                    .any(|tab| tab.id == tab_id && tab.kind == TabKind::Terminal)
            })
        {
            workspace.last_active_tab = None;
        }
    }

    fn sanitize_navigation_state(&mut self) {
        for workspace in &mut self.workspaces {
            Self::sanitize_workspace_navigation(workspace);
        }

        if !self.has_workspace(self.active_workspace) {
            self.active_workspace = self.workspaces.first().map(|ws| ws.id).unwrap_or(0);
        }

        if self.last_active_workspace == Some(self.active_workspace)
            || self
                .last_active_workspace
                .is_some_and(|ws_id| !self.has_workspace(ws_id))
        {
            self.last_active_workspace = None;
        }

        if self.presented_tab.is_some_and(|tab_id| {
            self.find_tab(tab_id)
                .is_none_or(|(workspace, _)| workspace.id != self.active_workspace)
        }) {
            self.presented_tab = None;
        }
    }

    pub fn reset_navigation_history(&mut self) {
        self.last_active_workspace = None;
        for workspace in &mut self.workspaces {
            workspace.last_active_tab = None;
        }
    }

    fn switch_to_workspace(&mut self, ws_id: u32) -> Option<u32> {
        self.sanitize_navigation_state();
        if !self.has_workspace(ws_id) {
            return None;
        }

        if self.active_workspace != ws_id && self.has_workspace(self.active_workspace) {
            self.last_active_workspace = Some(self.active_workspace);
        }
        self.active_workspace = ws_id;
        self.sanitize_navigation_state();
        self.presented_tab = self
            .active_ws()
            .and_then(|ws| (ws.active_tab != 0).then_some(ws.active_tab));
        Some(ws_id)
    }

    pub fn activate_workspace(&mut self, ws_id: u32) -> Option<u32> {
        self.switch_to_workspace(ws_id)?;
        let active_tab_id = self.active_ws().map_or(0, |workspace| workspace.active_tab);
        if active_tab_id != 0 {
            self.consume_tab_attention(active_tab_id);
        }
        Some(ws_id)
    }

    pub fn activate_previous_workspace(&mut self) -> Option<u32> {
        self.sanitize_navigation_state();
        let ws_id = self.last_active_workspace?;
        self.activate_workspace(ws_id)
    }

    /// Rename a workspace by id, returning the trimmed name that was applied.
    ///
    /// This is the single source of truth for workspace-name mutation shared by
    /// the sidebar inline rename and the `rename-workspace` socket action, so
    /// name changes stay consistent regardless of the entry point. An empty or
    /// whitespace-only name is rejected.
    pub fn rename_workspace(&mut self, ws_id: u32, new_name: &str) -> Result<String, String> {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return Err("name cannot be empty".to_string());
        }
        let Some(ws) = self.workspaces.iter_mut().find(|w| w.id == ws_id) else {
            return Err("workspace not found".to_string());
        };
        ws.name = new_name.to_string();
        self.event_store.emit(
            "workspace_renamed",
            serde_json::json!({
                "workspace_id": ws_id,
                "name": new_name,
            }),
        );
        Ok(new_name.to_string())
    }

    #[cfg(test)]
    pub(crate) fn last_active_workspace(&self) -> Option<u32> {
        self.last_active_workspace
    }

    pub fn activate_workspace_relative(&mut self, offset: isize) -> Option<u32> {
        self.sanitize_navigation_state();
        if self.workspaces.len() < 2 {
            return None;
        }

        let current_idx = self
            .workspaces
            .iter()
            .position(|ws| ws.id == self.active_workspace)?;
        let len = self.workspaces.len() as isize;
        let next_idx = (current_idx as isize + offset).rem_euclid(len) as usize;
        let ws_id = self.workspaces[next_idx].id;
        self.activate_workspace(ws_id)
    }

    pub fn activate_tab(&mut self, tab_id: u32) -> Option<u32> {
        self.sanitize_navigation_state();
        let (workspace_idx, tab_idx) =
            self.workspaces
                .iter()
                .enumerate()
                .find_map(|(ws_idx, ws)| {
                    ws.tabs
                        .iter()
                        .position(|tab| tab.id == tab_id)
                        .map(|tab_idx| (ws_idx, tab_idx))
                })?;
        let workspace_id = self.workspaces[workspace_idx].id;
        if self.workspaces[workspace_idx].tabs[tab_idx].kind != TabKind::Terminal {
            return None;
        }

        if self.active_workspace != workspace_id && self.has_workspace(self.active_workspace) {
            self.last_active_workspace = Some(self.active_workspace);
        }

        let current_active = self.workspaces[workspace_idx].active_tab;
        if current_active != 0 && current_active != tab_id {
            self.workspaces[workspace_idx].last_active_tab = Some(current_active);
        }

        self.active_workspace = workspace_id;
        self.consume_tab_attention(tab_id);
        let workspace = &mut self.workspaces[workspace_idx];
        workspace.active_tab = tab_id;
        self.presented_tab = Some(tab_id);

        self.sanitize_navigation_state();
        Some(workspace_id)
    }

    /// Present a tab in the content stack and consume its pending attention.
    /// Terminal tabs also become the command target; dashboard tabs deliberately
    /// leave the active terminal unchanged so implicit socket and keyboard
    /// actions remain well-defined.
    pub fn present_tab(&mut self, tab_id: u32) -> Option<u32> {
        let (workspace_id, kind) = self
            .find_tab(tab_id)
            .map(|(workspace, tab)| (workspace.id, tab.kind))?;
        if kind == TabKind::Terminal {
            return self.activate_tab(tab_id);
        }
        if self.active_workspace != workspace_id {
            self.switch_to_workspace(workspace_id)?;
        }
        self.consume_tab_attention(tab_id);
        self.presented_tab = Some(tab_id);
        Some(workspace_id)
    }

    pub fn presented_tab_id(&self) -> Option<u32> {
        self.presented_tab
            .filter(|tab_id| self.find_tab(*tab_id).is_some())
            .or_else(|| {
                self.active_ws().and_then(|workspace| {
                    (workspace.active_tab != 0).then_some(workspace.active_tab)
                })
            })
    }

    pub fn activate_previous_tab(&mut self) -> Option<u32> {
        self.sanitize_navigation_state();
        let workspace_idx = self
            .workspaces
            .iter()
            .position(|ws| ws.id == self.active_workspace)?;
        let tab_id = self.workspaces[workspace_idx].last_active_tab?;
        self.activate_tab(tab_id).map(|_| tab_id)
    }

    pub fn remove_workspace(&mut self, ws_id: u32) {
        self.workspaces.retain(|ws| ws.id != ws_id);
        self.prune_tab_owned_facts();
        self.sanitize_navigation_state();
    }

    pub(crate) fn activate_chord(&mut self, mode: ChordMode) -> u64 {
        self.chord_generation += 1;
        self.chord_mode = Some(mode);
        self.chord_generation
    }

    pub(crate) fn dismiss_chord(&mut self) {
        self.chord_mode = None;
        self.chord_generation += 1;
    }

    pub(crate) fn expire_chord(&mut self, generation: u64) -> bool {
        if self.chord_mode.is_some() && self.chord_generation == generation {
            self.chord_mode = None;
            return true;
        }
        false
    }

    pub(crate) fn chord_mode(&self) -> Option<ChordMode> {
        self.chord_mode
    }

    #[cfg(test)]
    pub(crate) fn chord_active(&self) -> bool {
        self.chord_mode.is_some()
    }

    pub fn broadcast_scope(&self) -> Option<BroadcastScope> {
        self.broadcast_scope
    }

    pub fn set_broadcast_scope(&mut self, scope: Option<BroadcastScope>) {
        self.broadcast_scope = scope;
    }

    pub fn remember_restored_legend(&mut self, legend: session::RestoredTabLegend) {
        self.restored_legends.insert(legend.tab_id, legend);
    }

    pub fn restored_legend(&self, tab_id: u32) -> Option<&session::RestoredTabLegend> {
        self.restored_legends.get(&tab_id)
    }

    pub fn dismiss_restored_legend(&mut self, tab_id: u32) -> bool {
        self.restored_legends.remove(&tab_id).is_some()
    }

    pub fn toggle_broadcast_shortcut_scope(&mut self) -> Option<BroadcastScope> {
        self.broadcast_scope = match self.broadcast_scope {
            Some(_) => None,
            None => Some(BroadcastScope::Tab),
        };
        self.broadcast_scope
    }

    pub fn remove_tab(&mut self, tab_id: u32) {
        for ws in &mut self.workspaces {
            ws.tabs.retain(|t| t.id != tab_id);
            if ws.last_active_tab == Some(tab_id) {
                ws.last_active_tab = None;
            }
        }
        if self.workspaces.len() > 1 {
            self.workspaces.retain(|w| !w.tabs.is_empty());
        }
        self.prune_tab_owned_facts();
        self.sanitize_navigation_state();
    }

    /// Tab membership is the authority for every tab-owned cache. Keep one
    /// removal transition for direct tab close, workspace close and restore.
    fn prune_tab_owned_facts(&mut self) {
        let live: HashSet<u32> = self.all_tabs().map(|tab| tab.id).collect();
        self.headless_panes.retain(|(tab, _), _| live.contains(tab));
        self.pending_tab_restores
            .retain(|tab, _| live.contains(tab));
        self.pending_socket_notifications
            .retain(|tab, _| live.contains(tab));
        self.restored_legends.retain(|tab, _| live.contains(tab));
        self.discovery_binary_paths
            .retain(|tab, _| live.contains(tab));
        self.task_discovery_snapshots
            .retain(|tab, _| live.contains(tab));
        self.revealed_task_tabs.retain(|tab| live.contains(tab));
        self.pane_transcripts
            .retain(|(tab, _), _| live.contains(tab));
        self.pane_dirty.retain_tabs(&live);
        if let Some(snapshot) = &mut self.runtime_probe {
            snapshot.retain_tabs(&live);
        }
    }

    /// A worker can finish after its source tab was closed. Filter at delivery
    /// too, so completion/cancellation cannot recreate an orphan probe cache.
    pub(crate) fn install_runtime_probe(
        &mut self,
        mut snapshot: crate::runtime_probe::RuntimeProbeSnapshot,
    ) {
        snapshot.retain_tabs(&self.all_tabs().map(|tab| tab.id).collect());
        self.runtime_probe = Some(snapshot);
    }

    pub(crate) fn set_socket_notification(&mut self, tab_id: u32, message: String) {
        self.pending_socket_notifications.insert(tab_id, message);
    }

    pub(crate) fn clear_socket_notification(&mut self, tab_id: u32) {
        self.pending_socket_notifications.remove(&tab_id);
    }

    pub(crate) fn has_socket_notification(&self, tab_id: u32) -> bool {
        self.pending_socket_notifications.contains_key(&tab_id)
    }

    pub(crate) fn tab_needs_attention(&self, tab: &Tab) -> bool {
        tab.needs_attention || self.pending_socket_notifications.contains_key(&tab.id)
    }

    pub(crate) fn refresh_attention_projection(&mut self) -> bool {
        let next = crate::attention::attention_targets_at(self, crate::events::unix_time_ms());
        if self.attention_projection_signature == next {
            return false;
        }
        self.attention_projection_signature = next;
        true
    }

    pub(crate) fn tab_primary_state(&self, tab: &Tab) -> crate::workspace::TabPrimaryState {
        if self.tab_needs_attention(tab) {
            crate::workspace::TabPrimaryState::Alert
        } else {
            tab.primary_state()
        }
    }

    pub(crate) fn tab_notification_message<'a>(&'a self, tab: &'a Tab) -> Option<&'a str> {
        self.pending_socket_notifications
            .get(&tab.id)
            .map(String::as_str)
            .or(tab.notification_msg.as_deref())
    }

    fn consume_tab_attention(&mut self, tab_id: u32) {
        self.pending_socket_notifications.remove(&tab_id);
        if let Some(tab) = self.all_tabs_mut().find(|tab| tab.id == tab_id) {
            tab.needs_attention = false;
            tab.notified = false;
            tab.notification_msg = None;
        }
    }

    pub fn headless_pane(&self, tab_id: u32, pane_id: u32) -> Option<&HeadlessPaneState> {
        self.headless_panes.get(&(tab_id, pane_id))
    }

    pub fn headless_pane_mut(
        &mut self,
        tab_id: u32,
        pane_id: u32,
    ) -> Option<&mut HeadlessPaneState> {
        self.headless_panes.get_mut(&(tab_id, pane_id))
    }

    pub(crate) fn pane_local_cwd(
        &self,
        tab_id: u32,
        pane_id: u32,
    ) -> Result<String, crate::task_binding::PaneTaskBindingError> {
        if let Some(pane) = self.headless_pane(tab_id, pane_id) {
            if crate::terminal::is_remote_host(pane.location_state.cwd_host.as_deref())
                || pane.process_state.remote_shell
            {
                return Err(crate::task_binding::PaneTaskBindingError::NoLocalCwd);
            }
            return pane
                .location_state
                .cwd
                .as_ref()
                .map(|cwd| cwd.trim())
                .filter(|cwd| !cwd.is_empty())
                .map(str::to_string)
                .ok_or(crate::task_binding::PaneTaskBindingError::NoLocalCwd);
        }

        let Some((_, tab)) = self.find_tab(tab_id) else {
            return Err(crate::task_binding::PaneTaskBindingError::PaneNotFound);
        };
        let Some(leaf) = tab.panes.leaf(pane_id) else {
            return Err(crate::task_binding::PaneTaskBindingError::PaneNotFound);
        };
        if leaf.process_state.remote_shell
            || crate::terminal::is_remote_host(leaf.location_state.cwd_host.as_deref())
        {
            return Err(crate::task_binding::PaneTaskBindingError::NoLocalCwd);
        }
        leaf.location_state
            .cwd
            .as_ref()
            .map(|cwd| cwd.trim())
            .filter(|cwd| !cwd.is_empty())
            .map(str::to_string)
            .ok_or(crate::task_binding::PaneTaskBindingError::NoLocalCwd)
    }

    pub fn bind_pane_to_task(
        &mut self,
        tab_id: u32,
        pane_id: u32,
        task_id: &str,
    ) -> Result<crate::task_binding::PaneTaskBinding, crate::task_binding::PaneTaskBindingError>
    {
        let cwd = self.pane_local_cwd(tab_id, pane_id)?;
        let binding = crate::task_binding::resolve_task_from_pane_cwd(&cwd, task_id)?;
        let previous = self.pane_task_binding(tab_id, pane_id).cloned();
        if let Some(pane) = self.headless_pane_mut(tab_id, pane_id) {
            pane.current_task = Some(binding.clone());
            self.record_pane_binding_change(tab_id, pane_id, previous.as_ref(), Some(&binding));
            return Ok(binding);
        }
        let tab = self
            .find_tab_mut(tab_id)
            .ok_or(crate::task_binding::PaneTaskBindingError::PaneNotFound)?;
        let leaf = tab
            .panes
            .leaf_mut(pane_id)
            .ok_or(crate::task_binding::PaneTaskBindingError::PaneNotFound)?;
        leaf.current_task = Some(binding.clone());
        self.record_pane_binding_change(tab_id, pane_id, previous.as_ref(), Some(&binding));
        Ok(binding)
    }

    /// Bind a newly-dispatched local pane before its first OSC cwd update arrives.
    /// The checkout supplied here is also the command's working directory; task
    /// resolution still goes through the exact-checkout `.plan/tasks.json` path.
    pub fn bind_pane_to_task_at_checkout(
        &mut self,
        tab_id: u32,
        pane_id: u32,
        checkout_root: &std::path::Path,
        task_id: &str,
    ) -> Result<crate::task_binding::PaneTaskBinding, crate::task_binding::PaneTaskBindingError>
    {
        let cwd = checkout_root.to_string_lossy().into_owned();
        let binding = crate::task_binding::resolve_task_from_pane_cwd(&cwd, task_id)?;
        let previous = self.pane_task_binding(tab_id, pane_id).cloned();
        if let Some(pane) = self.headless_pane_mut(tab_id, pane_id) {
            if crate::terminal::is_remote_host(pane.location_state.cwd_host.as_deref())
                || pane.process_state.remote_shell
            {
                return Err(crate::task_binding::PaneTaskBindingError::NoLocalCwd);
            }
            pane.location_state.cwd = Some(cwd);
            pane.current_task = Some(binding.clone());
            self.record_pane_binding_change(tab_id, pane_id, previous.as_ref(), Some(&binding));
            return Ok(binding);
        }
        let tab = self
            .find_tab_mut(tab_id)
            .ok_or(crate::task_binding::PaneTaskBindingError::PaneNotFound)?;
        let leaf = tab
            .panes
            .leaf_mut(pane_id)
            .ok_or(crate::task_binding::PaneTaskBindingError::PaneNotFound)?;
        if crate::terminal::is_remote_host(leaf.location_state.cwd_host.as_deref())
            || leaf.process_state.remote_shell
        {
            return Err(crate::task_binding::PaneTaskBindingError::NoLocalCwd);
        }
        leaf.location_state.cwd = Some(cwd);
        leaf.current_task = Some(binding.clone());
        self.record_pane_binding_change(tab_id, pane_id, previous.as_ref(), Some(&binding));
        Ok(binding)
    }

    pub fn pane_task_binding(
        &self,
        tab_id: u32,
        pane_id: u32,
    ) -> Option<&crate::task_binding::PaneTaskBinding> {
        self.headless_pane(tab_id, pane_id)
            .and_then(|pane| pane.current_task.as_ref())
            .or_else(|| {
                self.find_tab(tab_id)
                    .and_then(|(_, tab)| tab.panes.leaf(pane_id))
                    .and_then(|leaf| leaf.current_task.as_ref())
            })
    }

    pub fn clear_pane_task_binding(&mut self, tab_id: u32, pane_id: u32) -> bool {
        if let Some(pane) = self.headless_pane_mut(tab_id, pane_id) {
            let removed = pane.current_task.take();
            if let Some(binding) = removed.as_ref() {
                self.record_pane_binding_change(tab_id, pane_id, Some(binding), None);
            }
            return removed.is_some();
        }
        if let Some(removed) = self
            .find_tab_mut(tab_id)
            .and_then(|tab| tab.panes.leaf_mut(pane_id))
            .and_then(|leaf| leaf.current_task.take())
        {
            self.record_pane_binding_change(tab_id, pane_id, Some(&removed), None);
            return true;
        }
        let previous = self
            .pending_tab_restores
            .get(&tab_id)
            .and_then(|pending| pending.saved.current_task_for_pane(pane_id))
            .cloned();
        let cleared = self
            .pending_tab_restores
            .get_mut(&tab_id)
            .is_some_and(|pending| pending.saved.clear_current_task_for_pane(pane_id));
        if cleared {
            self.record_pane_binding_change(tab_id, pane_id, previous.as_ref(), None);
        }
        cleared
    }

    pub fn reorder_tab(&mut self, tab_id: u32, new_index: usize) -> bool {
        let ws = match self
            .workspaces
            .iter_mut()
            .find(|ws| ws.tabs.iter().any(|t| t.id == tab_id))
        {
            Some(ws) => ws,
            None => return false,
        };
        let old_index = match ws.tabs.iter().position(|t| t.id == tab_id) {
            Some(i) => i,
            None => return false,
        };
        if old_index == new_index || new_index >= ws.tabs.len() {
            return false;
        }
        let tab = ws.tabs.remove(old_index);
        ws.tabs.insert(new_index, tab);
        true
    }

    pub fn move_tab_to_workspace(
        &mut self,
        tab_id: u32,
        target_ws_id: u32,
    ) -> Result<(), MoveTabError> {
        let source_idx = self
            .workspaces
            .iter()
            .position(|ws| ws.tabs.iter().any(|tab| tab.id == tab_id))
            .ok_or(MoveTabError::TabNotFound)?;
        let target_idx = self
            .workspaces
            .iter()
            .position(|ws| ws.id == target_ws_id)
            .ok_or(MoveTabError::WorkspaceNotFound)?;

        if self.workspaces[source_idx].id == target_ws_id {
            return Err(MoveTabError::AlreadyInWorkspace);
        }

        let moved_tab_idx = self.workspaces[source_idx]
            .tabs
            .iter()
            .position(|tab| tab.id == tab_id)
            .ok_or(MoveTabError::TabNotFound)?;
        let target_previous_active = self.workspaces[target_idx].active_tab;
        let tab = self.workspaces[source_idx].tabs.remove(moved_tab_idx);
        if self.workspaces[source_idx].last_active_tab == Some(tab_id) {
            self.workspaces[source_idx].last_active_tab = None;
        }

        let target_ws = &mut self.workspaces[target_idx];
        if target_previous_active != 0 && target_previous_active != tab_id {
            target_ws.last_active_tab = Some(target_previous_active);
        }
        target_ws.tabs.push(tab);
        target_ws.active_tab = tab_id;
        target_ws.collapsed = false;
        if self.active_workspace != target_ws_id && self.has_workspace(self.active_workspace) {
            self.last_active_workspace = Some(self.active_workspace);
        }
        self.active_workspace = target_ws_id;
        self.sanitize_navigation_state();
        Ok(())
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeHandle {
    pub fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(AppState::new())),
        }
    }

    pub fn from_shared_state(state: Rc<RefCell<AppState>>) -> Self {
        Self { state }
    }

    pub fn shared_state(&self) -> Rc<RefCell<AppState>> {
        self.state.clone()
    }

    pub fn clear_for_session_restore(&self) {
        let mut state = self.state.borrow_mut();
        state.workspaces.clear();
        state.active_workspace = 0;
        state.prune_tab_owned_facts();
        state.detached_sessions.clear();
        state.selected_dashboard_view = None;
        state.reset_navigation_history();
    }

    pub fn reset_navigation_history(&self) {
        self.state.borrow_mut().reset_navigation_history();
    }

    pub fn create_workspace(&self, name: &str, repo_root: Option<String>) -> u32 {
        self.state.borrow_mut().create_workspace(name, repo_root)
    }

    pub fn set_workspace_active_tab(&self, workspace_id: u32, tab_id: u32) -> bool {
        let mut state = self.state.borrow_mut();
        let is_terminal = state.find_tab(tab_id).is_some_and(|(workspace, tab)| {
            workspace.id == workspace_id && tab.kind == TabKind::Terminal
        });
        if !is_terminal {
            return false;
        }
        let Some(workspace) = state.workspaces.iter_mut().find(|ws| ws.id == workspace_id) else {
            return false;
        };
        workspace.active_tab = tab_id;
        if state.active_workspace == workspace_id {
            state.presented_tab = Some(tab_id);
        }
        true
    }

    pub fn remember_restored_legend(&self, legend: session::RestoredTabLegend) {
        self.state.borrow_mut().remember_restored_legend(legend);
    }

    pub fn register_terminal_tab(&self, registration: TerminalTabRegistration) -> u32 {
        let mut state = self.state.borrow_mut();
        let tab_id = state.next_id;
        state.next_id += 1;

        if state.active_ws_mut().is_none() {
            state.create_workspace("default", None);
        }

        let tab = build_terminal_tab(tab_id, registration);
        state
            .active_ws_mut()
            .expect("active workspace must exist before registering a tab")
            .tabs
            .push(tab);
        tab_id
    }

    pub fn reserve_pane_id_for_tab(&self, tab_id: u32) -> Option<u32> {
        let mut state = self.state.borrow_mut();
        let tab = state.find_tab_mut(tab_id)?;
        let pane_id = tab.next_pane_id;
        tab.next_pane_id += 1;
        Some(pane_id)
    }

    pub fn split_tab_with_node(
        &self,
        tab_id: u32,
        target_pane_id: u32,
        new_node: pane::PaneNode,
        direction: pane::SplitDirection,
        split_widget: &gtk::Paned,
    ) -> Option<u32> {
        let new_pane_id = first_pane_id(&new_node)?;
        let mut state = self.state.borrow_mut();
        let tab = state.find_tab_mut(tab_id)?;
        if !split_node_in_tree(
            &mut tab.panes,
            target_pane_id,
            new_node,
            direction,
            split_widget,
        ) {
            return None;
        }
        tab.focused_pane_id = new_pane_id;
        Some(new_pane_id)
    }

    pub fn close_pane_in_tab(
        &self,
        tab_id: u32,
        pane_id: u32,
        term_stack: &gtk::Stack,
        stack_name: &str,
    ) -> Option<ClosedPaneState> {
        let mut state = self.state.borrow_mut();
        let workspace_name = state.find_tab(tab_id)?.0.name.clone();
        let tab = state.find_tab_mut(tab_id)?;
        if tab.panes.leaf_count() <= 1 {
            return None;
        }

        let tmux_backing = tab
            .panes
            .leaf(pane_id)
            .and_then(|leaf| leaf.tmux_backing.clone());

        let (focus_terminal, deferred_stack_widget) =
            remove_pane_from_tree(&mut tab.panes, pane_id, term_stack, stack_name);
        tab.reset_pane_agent_activity_evidence(pane_id);
        tab.focused_pane_id = first_pane_id(&tab.panes).unwrap_or(0);

        Some(ClosedPaneState {
            focus_terminal,
            deferred_stack_widget,
            tmux_backing,
            workspace_name,
        })
    }

    pub fn emit_event(&self, event_type: impl Into<String>, payload: serde_json::Value) -> u64 {
        let event_type = event_type.into();
        let mut state = self.state.borrow_mut();
        let seq = state.event_store.emit(&event_type, payload.clone());
        if event_type == "agent_message" {
            state.project_agent_message(&payload);
        }
        seq
    }
}

impl Default for RuntimeHandle {
    fn default() -> Self {
        Self::new()
    }
}

fn build_terminal_tab(tab_id: u32, registration: TerminalTabRegistration) -> Tab {
    Tab {
        id: tab_id,
        name: registration.name,
        work_origin: crate::workspace::new_tab_work_origin(),
        kind: TabKind::Terminal,
        panes: registration.panes,
        focused_pane_id: registration.focused_pane_id,
        next_pane_id: registration.next_pane_id,
        pane_zoom: None,
        close_on_exit: registration.close_on_exit,
        respawn_on_exit: registration.respawn_on_exit,
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

pub(crate) fn first_pane_id(node: &pane::PaneNode) -> Option<u32> {
    match node {
        pane::PaneNode::Leaf(leaf) => Some(leaf.pane_id),
        pane::PaneNode::Split { first, second, .. } => {
            first_pane_id(first).or_else(|| first_pane_id(second))
        }
        pane::PaneNode::Stub { pane_id } => Some(*pane_id),
        pane::PaneNode::Empty => None,
    }
}

pub(crate) fn split_node_in_tree(
    node: &mut pane::PaneNode,
    target_pane_id: u32,
    new_node: pane::PaneNode,
    direction: pane::SplitDirection,
    split_widget: &gtk::Paned,
) -> bool {
    match node {
        pane::PaneNode::Leaf(leaf) if leaf.pane_id == target_pane_id => {
            let old_node = std::mem::replace(node, pane::PaneNode::Empty);
            *node = pane::PaneNode::Split {
                direction,
                first: Box::new(old_node),
                second: Box::new(new_node),
                widget: split_widget.clone(),
            };
            true
        }
        pane::PaneNode::Stub { pane_id } if *pane_id == target_pane_id => {
            let old_node = std::mem::replace(node, pane::PaneNode::Empty);
            *node = pane::PaneNode::Split {
                direction,
                first: Box::new(old_node),
                second: Box::new(new_node),
                widget: split_widget.clone(),
            };
            true
        }
        pane::PaneNode::Split { first, second, .. } => {
            if first.contains_pane(target_pane_id) {
                split_node_in_tree(first, target_pane_id, new_node, direction, split_widget)
            } else {
                split_node_in_tree(second, target_pane_id, new_node, direction, split_widget)
            }
        }
        _ => false,
    }
}

pub(crate) fn remove_pane_from_tree(
    node: &mut pane::PaneNode,
    target_pane_id: u32,
    term_stack: &gtk::Stack,
    stack_name: &str,
) -> (Option<vte::Terminal>, Option<gtk::Widget>) {
    let _ = (term_stack, stack_name);
    match node {
        pane::PaneNode::Stub { .. } => (None, None),
        pane::PaneNode::Empty => (None, None),
        pane::PaneNode::Split {
            first,
            second,
            widget: paned_widget,
            ..
        } => {
            let first_is_target = pane_node_matches_id(first.as_ref(), target_pane_id);
            let second_is_target = pane_node_matches_id(second.as_ref(), target_pane_id);

            if first_is_target || second_is_target {
                paned_widget.set_start_child(gtk::Widget::NONE);
                paned_widget.set_end_child(gtk::Widget::NONE);

                let surviving = if first_is_target {
                    std::mem::replace(second.as_mut(), pane::PaneNode::Empty)
                } else {
                    std::mem::replace(first.as_mut(), pane::PaneNode::Empty)
                };

                let focus_term = first_terminal(&surviving);
                let surviving_widget = root_widget_clone(&surviving);
                let deferred_stack_widget = if let Some(surviving_widget) = surviving_widget {
                    if let Some(parent) = paned_widget.parent() {
                        if let Ok(grandparent_paned) = parent.downcast::<gtk::Paned>() {
                            if grandparent_paned.start_child().as_ref()
                                == Some(paned_widget.upcast_ref::<gtk::Widget>())
                            {
                                grandparent_paned.set_start_child(gtk::Widget::NONE);
                                grandparent_paned.set_start_child(Some(&surviving_widget));
                            } else {
                                grandparent_paned.set_end_child(gtk::Widget::NONE);
                                grandparent_paned.set_end_child(Some(&surviving_widget));
                            }
                            None
                        } else {
                            Some(surviving_widget)
                        }
                    } else {
                        Some(surviving_widget)
                    }
                } else {
                    None
                };

                *node = surviving;
                (focus_term, deferred_stack_widget)
            } else if first.contains_pane(target_pane_id) {
                remove_pane_from_tree(first, target_pane_id, term_stack, stack_name)
            } else {
                remove_pane_from_tree(second, target_pane_id, term_stack, stack_name)
            }
        }
        pane::PaneNode::Leaf(_) => (None, None),
    }
}

fn pane_node_matches_id(node: &pane::PaneNode, target_pane_id: u32) -> bool {
    match node {
        pane::PaneNode::Leaf(leaf) => leaf.pane_id == target_pane_id,
        pane::PaneNode::Stub { pane_id } => *pane_id == target_pane_id,
        _ => false,
    }
}

fn first_terminal(node: &pane::PaneNode) -> Option<vte::Terminal> {
    match node {
        pane::PaneNode::Leaf(leaf) => Some(leaf.terminal.clone()),
        pane::PaneNode::Split { first, second, .. } => {
            first_terminal(first).or_else(|| first_terminal(second))
        }
        pane::PaneNode::Stub { .. } => None,
        pane::PaneNode::Empty => None,
    }
}

fn root_widget_clone(node: &pane::PaneNode) -> Option<gtk::Widget> {
    match node {
        pane::PaneNode::Leaf(leaf) => Some(leaf.container.clone().upcast()),
        pane::PaneNode::Split { widget, .. } => Some(widget.clone().upcast()),
        pane::PaneNode::Stub { .. } => None,
        pane::PaneNode::Empty => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{first_pane_id, RuntimeHandle, TerminalTabRegistration};
    use crate::dashboard::DetachedSession;
    use crate::pane::PaneNode;
    use crate::tmux::TmuxTarget;
    use crate::AppState;
    use std::time::Instant;

    fn write_tasks(root: &std::path::Path, tasks: &[(&str, &str)]) {
        std::fs::create_dir_all(root.join(".git")).expect("git dir should be created");
        let plan_dir = root.join(".plan");
        std::fs::create_dir_all(&plan_dir).expect("plan dir should be created");
        let tasks_json = tasks
            .iter()
            .map(|(id, title)| {
                format!(r#"{{"id":"{id}","title":"{title}","status":"todo","priority":"p1"}}"#)
            })
            .collect::<Vec<_>>()
            .join(",");
        std::fs::write(
            plan_dir.join("tasks.json"),
            format!(r#"{{"tasks":[{tasks_json}]}}"#),
        )
        .expect("tasks.json should be written");
    }

    fn unique_test_dir(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "taarof-runtime-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("test dir should be created");
        root
    }

    fn stub_tab_registration(
        root: PaneNode,
        focused_pane_id: u32,
        next_pane_id: u32,
    ) -> TerminalTabRegistration {
        TerminalTabRegistration {
            name: "Shell".to_string(),
            panes: Box::new(root),
            focused_pane_id,
            next_pane_id,
            close_on_exit: true,
            respawn_on_exit: None,
        }
    }

    #[test]
    fn runtime_register_terminal_tab_creates_default_workspace() {
        let runtime = RuntimeHandle::new();
        runtime.clear_for_session_restore();

        let tab_id = runtime.register_terminal_tab(stub_tab_registration(
            PaneNode::Stub { pane_id: 7 },
            7,
            8,
        ));
        let state = runtime.shared_state();
        let st = state.borrow();
        let workspace = st.active_ws().expect("default workspace should exist");

        assert_eq!(workspace.name, "default");
        assert_eq!(workspace.tabs.len(), 1);
        assert_eq!(workspace.tabs[0].id, tab_id);
        assert_eq!(workspace.tabs[0].focused_pane_id, 7);
        assert_eq!(first_pane_id(&workspace.tabs[0].panes), Some(7));
    }

    #[test]
    fn pane_task_binding_can_bind_change_and_clear_from_pane_cwd() {
        let root = unique_test_dir("pane-task-bind");
        write_tasks(
            &root,
            &[
                ("EXAMPLE-110", "Bind pane tasks"),
                ("EXAMPLE-111", "Change pane tasks"),
            ],
        );
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "work",
            crate::HeadlessPaneSeed {
                cwd: Some(root.join("src").to_string_lossy().into_owned()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless pane should be seeded");

        let bound = state
            .bind_pane_to_task(tab_id, pane_id, "EXAMPLE-110")
            .expect("task in pane-local plan should bind");
        assert_eq!(bound.task_id, "EXAMPLE-110");
        assert_eq!(bound.title, "Bind pane tasks");

        let changed = state
            .bind_pane_to_task(tab_id, pane_id, "EXAMPLE-111")
            .expect("second task should replace current binding");
        assert_eq!(changed.task_id, "EXAMPLE-111");
        assert_eq!(
            state
                .pane_task_binding(tab_id, pane_id)
                .expect("binding should be stored")
                .title,
            "Change pane tasks"
        );

        let before_noop = state.work_ledger.records().len();
        state
            .bind_pane_to_task(tab_id, pane_id, "EXAMPLE-111")
            .expect("rebinding the same task is allowed");
        assert_eq!(
            state.work_ledger.records().len(),
            before_noop,
            "same-binding probes do not append work"
        );

        assert!(state.clear_pane_task_binding(tab_id, pane_id));
        assert!(state.pane_task_binding(tab_id, pane_id).is_none());
        let binding_records: Vec<_> = state
            .work_ledger
            .records()
            .into_iter()
            .filter(|record| {
                matches!(
                    record.kind,
                    crate::work_ledger::WorkKind::PaneBound
                        | crate::work_ledger::WorkKind::PaneUnbound
                )
            })
            .collect();
        assert_eq!(binding_records.len(), 3);
        assert!(binding_records[1]
            .summary
            .contains("EXAMPLE-110 -> EXAMPLE-111"));
        assert_eq!(
            binding_records[2].identity.task_id.as_deref(),
            Some("EXAMPLE-111"),
            "unbind retains the cleared task snapshot"
        );
    }

    #[test]
    fn pane_task_binding_rejects_parent_plan_outside_exact_checkout() {
        let parent = unique_test_dir("pane-task-parent");
        write_tasks(&parent, &[("EXAMPLE-110", "Parent task must not bind")]);
        let nested = parent.join("nested-checkout");
        std::fs::create_dir_all(nested.join(".git")).expect("nested git dir should be created");

        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "work",
            crate::HeadlessPaneSeed {
                cwd: Some(nested.join("src").to_string_lossy().into_owned()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless pane should be seeded");

        assert_eq!(
            state.bind_pane_to_task(tab_id, pane_id, "EXAMPLE-110"),
            Err(crate::task_binding::PaneTaskBindingError::NoPlanFile)
        );
        assert!(state.pane_task_binding(tab_id, pane_id).is_none());
    }

    #[test]
    fn pane_task_binding_dispatch_helper_sets_checkout_cwd_and_binding() {
        let root = unique_test_dir("pane-task-dispatch");
        write_tasks(&root, &[("EXAMPLE-110", "Dispatch task")]);
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "loop",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("headless pane should be seeded");

        let binding = state
            .bind_pane_to_task_at_checkout(tab_id, pane_id, &root, "EXAMPLE-110")
            .expect("dispatch helper should bind from explicit checkout");

        assert_eq!(binding.title, "Dispatch task");
        assert_eq!(
            state
                .pane_task_binding(tab_id, pane_id)
                .map(|binding| binding.task_id.as_str()),
            Some("EXAMPLE-110")
        );
        let expected_cwd = root.to_string_lossy().into_owned();
        assert_eq!(
            state
                .headless_pane(tab_id, pane_id)
                .and_then(|pane| pane.location_state.cwd.as_deref()),
            Some(expected_cwd.as_str())
        );
    }

    fn seed_tab_owned_facts(state: &mut AppState, tab: u32, pane: u32) {
        state
            .pending_socket_notifications
            .insert(tab, "inert notification".into());
        state
            .discovery_binary_paths
            .insert(tab, "/inert/mise".into());
        state.task_discovery_snapshots.insert(
            tab,
            crate::task_launch::TaskDiscoverySnapshot::from_tasks(
                crate::mise::DiscoveryTarget::Local {
                    cwd: "/inert".into(),
                    binary_path: None,
                },
                &[],
            ),
        );
        state.revealed_task_tabs.insert(tab);
        state.restored_legends.insert(
            tab,
            crate::session::RestoredTabLegend {
                tab_id: tab,
                items: vec![],
            },
        );
        state.pending_tab_restores.insert(
            tab,
            crate::session::PendingTabRestore {
                saved: crate::session::SavedPaneNode::Leaf {
                    work_origin: None,
                    cwd: None,
                    ssh_command: None,
                    tmux_session: None,
                    tmux_host: None,
                    tmux_identity: None,
                    current_task: None,
                    agent_session: None,
                },
                cwd: None,
                show_restore_legend: false,
            },
        );
        state
            .pane_transcripts
            .insert((tab, pane), Default::default());
        let probe = state.runtime_probe.get_or_insert_with(|| {
            crate::runtime_probe::RuntimeProbeSnapshot::worker_failure(
                1,
                &[],
                &[],
                crate::runtime_probe::RuntimeProbeWorkerFailure::Panicked,
            )
        });
        probe.tab_pids.insert(tab, vec![42]);
        probe.pane_pids.insert((tab, pane), 42);
        probe
            .pane_process_states
            .insert((tab, pane), Default::default());
        probe.pane_agents.insert((tab, pane), Default::default());
        probe.tab_agents.insert(tab, Default::default());
        probe.tab_ports.insert(tab, vec![8080]);
    }

    fn tab_owned_fact_counts(state: &AppState) -> Vec<usize> {
        let mut counts = vec![
            state.headless_panes.len(),
            state.pending_tab_restores.len(),
            state.pending_socket_notifications.len(),
            state.restored_legends.len(),
            state.discovery_binary_paths.len(),
            state.task_discovery_snapshots.len(),
            state.revealed_task_tabs.len(),
            state.pane_transcripts.len(),
        ];
        if let Some(probe) = &state.runtime_probe {
            counts.extend([
                probe.tab_pids.len(),
                probe.pane_pids.len(),
                probe.pane_process_states.len(),
                probe.pane_agents.len(),
                probe.tab_agents.len(),
                probe.tab_ports.len(),
            ]);
        }
        counts
    }

    #[test]
    fn tab_cleanup_churn_reclaims_every_owned_fact_and_preserves_sibling() {
        let mut state = AppState::new();
        let stable_ws = state.active_workspace;
        let (stable_tab, stable_pane) =
            crate::seed_headless_terminal_tab(&mut state, stable_ws, "stable", Default::default())
                .unwrap();
        seed_tab_owned_facts(&mut state, stable_tab, stable_pane);
        let stable_signal = state.pane_dirty.signal_for(crate::http::PaneKey {
            tab_id: stable_tab,
            pane_id: stable_pane,
        });
        let baseline = tab_owned_fact_counts(&state);
        for n in 0..32 {
            let ws = state.create_workspace("churn", None);
            let (tab, pane) =
                crate::seed_headless_terminal_tab(&mut state, ws, "churn", Default::default())
                    .unwrap();
            seed_tab_owned_facts(&mut state, tab, pane);
            let signal = state.pane_dirty.signal_for(crate::http::PaneKey {
                tab_id: tab,
                pane_id: pane,
            });
            let late_probe = state.runtime_probe.as_ref().unwrap().clone();
            if n % 2 == 0 {
                state.remove_workspace(ws);
            } else {
                state.remove_tab(tab);
            }
            assert_eq!(
                tab_owned_fact_counts(&state),
                baseline,
                "tab-owned caches must return to baseline at iteration {n}"
            );
            assert!(
                !state.pane_dirty.mark_dirty(tab, pane),
                "late output callbacks must not recreate removed entries"
            );
            state.install_runtime_probe(late_probe);
            assert_eq!(
                tab_owned_fact_counts(&state),
                baseline,
                "a late worker probe cannot recreate closed-tab facts"
            );
            assert_eq!(
                std::sync::Arc::strong_count(&signal),
                1,
                "registry must release the removed pane signal"
            );
            assert_eq!(
                std::sync::Arc::strong_count(&stable_signal),
                2,
                "sibling registry entry must remain"
            );
            assert!(state.find_tab(stable_tab).is_some());
            assert!(
                state.pending_tab_restores.contains_key(&stable_tab),
                "unrelated lazy restore remains pending"
            );
        }
    }

    #[test]
    fn tab_cleanup_restore_reset_uses_the_same_ownership_transition() {
        let runtime = RuntimeHandle::new();
        let shared = runtime.shared_state();
        let signal = {
            let mut state = shared.borrow_mut();
            let ws = state.active_workspace;
            let (tab, pane) = crate::seed_headless_terminal_tab(
                &mut state,
                ws,
                "restore-reset",
                Default::default(),
            )
            .unwrap();
            seed_tab_owned_facts(&mut state, tab, pane);
            state.pane_dirty.signal_for(crate::http::PaneKey {
                tab_id: tab,
                pane_id: pane,
            })
        };
        runtime.clear_for_session_restore();
        assert!(tab_owned_fact_counts(&shared.borrow())
            .iter()
            .all(|count| *count == 0));
        assert_eq!(std::sync::Arc::strong_count(&signal), 1);
    }

    #[test]
    fn removing_tab_clears_headless_task_binding_state() {
        let root = unique_test_dir("pane-task-remove-tab");
        write_tasks(&root, &[("EXAMPLE-110", "Remove tab task")]);
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "work",
            crate::HeadlessPaneSeed {
                cwd: Some(root.to_string_lossy().into_owned()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless pane should be seeded");
        state
            .bind_pane_to_task(tab_id, pane_id, "EXAMPLE-110")
            .expect("task should bind before tab removal");

        state.remove_tab(tab_id);

        assert!(state.headless_pane(tab_id, pane_id).is_none());
        assert!(state.pane_task_binding(tab_id, pane_id).is_none());
    }

    #[test]
    fn pane_task_binding_rejects_remote_panes() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "remote",
            crate::HeadlessPaneSeed {
                cwd: Some("/tmp/remote-checkout".to_string()),
                cwd_host: Some("remote.example".to_string()),
                remote_shell: true,
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless pane should be seeded");

        assert_eq!(
            state.bind_pane_to_task(tab_id, pane_id, "EXAMPLE-110"),
            Err(crate::task_binding::PaneTaskBindingError::NoLocalCwd)
        );
        assert!(state.pane_task_binding(tab_id, pane_id).is_none());
    }

    #[test]
    fn pane_task_binding_accepts_local_hostname_and_rejects_remote_osc7_host() {
        let root = unique_test_dir("pane-task-host-semantics");
        write_tasks(&root, &[("EXAMPLE-110", "Host-aware task")]);
        let local_hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .expect("local hostname")
            .trim()
            .to_string();
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (local_tab, local_pane) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "local",
            crate::HeadlessPaneSeed {
                cwd: Some(root.to_string_lossy().into_owned()),
                cwd_host: Some(local_hostname),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("local pane");
        assert!(state
            .bind_pane_to_task(local_tab, local_pane, "EXAMPLE-110")
            .is_ok());

        let (remote_tab, remote_pane) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "remote-metadata",
            crate::HeadlessPaneSeed {
                cwd: Some(root.to_string_lossy().into_owned()),
                cwd_host: Some("remote.example".into()),
                remote_shell: false,
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("remote pane");
        assert_eq!(
            state.bind_pane_to_task(remote_tab, remote_pane, "EXAMPLE-110"),
            Err(crate::task_binding::PaneTaskBindingError::NoLocalCwd)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rename_workspace_updates_name_and_rejects_empty() {
        let runtime = RuntimeHandle::new();
        runtime.clear_for_session_restore();
        let ws_id = runtime.create_workspace("old-name", None);
        let state = runtime.shared_state();

        {
            let mut st = state.borrow_mut();
            let applied = st
                .rename_workspace(ws_id, "  new-name  ")
                .expect("rename should succeed");
            assert_eq!(applied, "new-name");
        }
        assert_eq!(
            state
                .borrow()
                .workspaces
                .iter()
                .find(|w| w.id == ws_id)
                .map(|w| w.name.as_str()),
            Some("new-name")
        );

        let mut st = state.borrow_mut();
        assert_eq!(
            st.rename_workspace(ws_id, "   "),
            Err("name cannot be empty".to_string())
        );
        assert_eq!(
            st.rename_workspace(9999, "x"),
            Err("workspace not found".to_string())
        );
    }

    #[test]
    fn present_background_dashboard_preserves_terminal_alert() {
        let mut state = AppState::new();
        let ws_a = state.active_workspace;
        let ws_b = state.create_workspace("background", None);
        let (term_id, _) = crate::seed_headless_terminal_tab(
            &mut state,
            ws_b,
            "work",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("background terminal tab should be seeded");
        let dash = state
            .create_dashboard_tab()
            .expect("background dashboard tab should be created");

        assert_eq!(
            state
                .workspaces
                .iter()
                .find(|workspace| workspace.id == ws_b)
                .map(|workspace| workspace.active_tab),
            Some(term_id),
            "creating the dashboard must leave the terminal active"
        );
        assert_eq!(state.activate_workspace(ws_a), Some(ws_a));
        assert_eq!(state.active_workspace, ws_a);
        let terminal = state
            .all_tabs_mut()
            .find(|tab| tab.id == term_id)
            .expect("background terminal tab should exist");
        terminal.needs_attention = true;
        terminal.notification_msg = Some("build done".into());
        terminal.notified = true;
        state.set_socket_notification(term_id, "build done".into());

        assert_eq!(state.present_tab(dash), Some(ws_b));

        let (_, terminal) = state
            .find_tab(term_id)
            .expect("background terminal tab should still exist");
        assert!(terminal.needs_attention);
        assert!(terminal.notified);
        assert_eq!(terminal.notification_msg.as_deref(), Some("build done"));
        assert!(state.has_socket_notification(term_id));
        assert_eq!(state.presented_tab_id(), Some(dash));

        assert_eq!(state.activate_tab(term_id), Some(ws_b));

        let (_, terminal) = state
            .find_tab(term_id)
            .expect("activated terminal tab should still exist");
        assert!(!terminal.needs_attention);
        assert!(!terminal.notified);
        assert!(terminal.notification_msg.is_none());
        assert!(!state.has_socket_notification(term_id));
    }

    #[test]
    fn runtime_clear_for_session_restore_resets_restore_state() {
        let runtime = RuntimeHandle::new();
        runtime.create_workspace("feature", Some("/tmp/repo".to_string()));
        {
            let state = runtime.shared_state();
            let mut st = state.borrow_mut();
            st.selected_dashboard_view = Some("detached".to_string());
            st.detached_sessions.push(DetachedSession {
                session_name: "taarof-test".to_string(),
                host: "local".to_string(),
                workspace: "feature".to_string(),
                target: TmuxTarget::Local,
                detached_at: Instant::now(),
                last_command: Some("cargo test".to_string()),
                finished: false,
            });
        }

        runtime.clear_for_session_restore();
        let state = runtime.shared_state();
        let st = state.borrow();

        assert!(st.workspaces.is_empty());
        assert_eq!(st.active_workspace, 0);
        assert!(st.detached_sessions.is_empty());
        assert!(st.selected_dashboard_view.is_none());
    }

    #[test]
    fn runtime_reserve_pane_id_for_tab_advances_counter() {
        let runtime = RuntimeHandle::new();
        runtime.clear_for_session_restore();
        let tab_id = runtime.register_terminal_tab(stub_tab_registration(
            PaneNode::Stub { pane_id: 7 },
            7,
            8,
        ));

        assert_eq!(runtime.reserve_pane_id_for_tab(tab_id), Some(8));
        assert_eq!(runtime.reserve_pane_id_for_tab(tab_id), Some(9));

        let state = runtime.shared_state();
        let st = state.borrow();
        let (_, tab) = st.find_tab(tab_id).expect("tab should exist");

        assert_eq!(tab.next_pane_id, 10);
    }

    #[test]
    fn runtime_set_workspace_active_tab_validates_tab_membership() {
        let runtime = RuntimeHandle::new();
        runtime.clear_for_session_restore();
        let workspace_id = runtime.create_workspace("feature", None);
        let tab_a = runtime.register_terminal_tab(stub_tab_registration(
            PaneNode::Stub { pane_id: 1 },
            1,
            2,
        ));
        let tab_b = runtime.register_terminal_tab(stub_tab_registration(
            PaneNode::Stub { pane_id: 3 },
            3,
            4,
        ));
        let other_workspace_id = runtime.create_workspace("other", None);
        let other_tab = runtime.register_terminal_tab(stub_tab_registration(
            PaneNode::Stub { pane_id: 5 },
            5,
            6,
        ));

        assert!(runtime.set_workspace_active_tab(workspace_id, tab_b));
        assert!(!runtime.set_workspace_active_tab(workspace_id, other_tab));
        assert!(!runtime.set_workspace_active_tab(other_workspace_id + 999, tab_a));

        let state = runtime.shared_state();
        let st = state.borrow();
        let workspace = st
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .expect("workspace should exist");
        assert_eq!(workspace.active_tab, tab_b);
    }

    #[test]
    fn runtime_emit_event_records_event_store_entries() {
        let runtime = RuntimeHandle::new();
        let event_id = runtime.emit_event("runtime_test_event", serde_json::json!({ "n": 1 }));

        let state = runtime.shared_state();
        let entries = state.borrow().event_store.entries();
        let event = entries.last().expect("event should be recorded");

        assert_eq!(event.seq, event_id);
        assert_eq!(event.event_type, "runtime_test_event");
        assert_eq!(event.payload["n"], serde_json::json!(1));
    }
}
