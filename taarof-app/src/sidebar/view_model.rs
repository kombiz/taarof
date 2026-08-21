//! Sidebar-local typed row registry and monitor ownership.
//!
//! This module owns the sidebar's stable lookup layer keyed by workspace/tab
//! IDs. It must not perform `.plan` discovery or directly traverse the GTK
//! widget tree during routine refresh work.

use super::*;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};

thread_local! {
    static SIDEBAR_REGISTRIES: RefCell<HashMap<usize, Rc<SidebarRegistry>>> = RefCell::new(HashMap::new());
}

#[derive(Default)]
pub(super) struct SidebarRegistry {
    session_label: RefCell<Option<gtk::Label>>,
    workspace_rows: RefCell<HashMap<u32, WorkspaceRowHandle>>,
    tab_rows: RefCell<HashMap<u32, TabRowHandle>>,
    background_section: RefCell<Option<BackgroundSectionHandle>>,
    view_model: RefCell<SidebarViewModel>,
}

impl SidebarRegistry {
    fn teardown(&self) {
        for handle in self.tab_rows.borrow().values() {
            handle.clear_discovery_poll();
            handle.clear_plan_monitor();
        }
        self.tab_rows.borrow_mut().clear();
        self.workspace_rows.borrow_mut().clear();
        self.background_section.borrow_mut().take();
        self.session_label.borrow_mut().take();
        self.view_model.borrow_mut().teardown();
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct SidebarViewModel {
    pub(super) session_label_installed: bool,
    pub(super) workspace_ids: BTreeSet<u32>,
    pub(super) tab_ids: BTreeSet<u32>,
    pub(super) agent_tab_expansion: BTreeMap<u32, bool>,
    pub(super) background_section_installed: bool,
}

impl SidebarViewModel {
    pub(super) fn install_session_label(&mut self) {
        self.session_label_installed = true;
    }

    pub(super) fn register_workspace(&mut self, ws_id: u32) {
        self.workspace_ids.insert(ws_id);
    }

    pub(super) fn remove_workspace(&mut self, ws_id: u32) {
        self.workspace_ids.remove(&ws_id);
    }

    pub(super) fn register_tab(&mut self, tab_id: u32) {
        self.tab_ids.insert(tab_id);
    }

    pub(super) fn remove_tab(&mut self, tab_id: u32) {
        self.tab_ids.remove(&tab_id);
        self.agent_tab_expansion.remove(&tab_id);
    }

    pub(super) fn agent_tab_expanded(&self, tab_id: u32) -> bool {
        self.agent_tab_expansion
            .get(&tab_id)
            .copied()
            .unwrap_or(true)
    }

    pub(super) fn set_agent_tab_expanded(&mut self, tab_id: u32, expanded: bool) {
        self.agent_tab_expansion.insert(tab_id, expanded);
    }

    pub(super) fn set_background_section(&mut self, installed: bool) {
        self.background_section_installed = installed;
    }

    pub(super) fn teardown(&mut self) {
        self.session_label_installed = false;
        self.workspace_ids.clear();
        self.tab_ids.clear();
        self.agent_tab_expansion.clear();
        self.background_section_installed = false;
    }
}

#[derive(Clone)]
pub(super) struct WorkspaceRowHandle {
    pub(super) header: gtk::Box,
    pub(super) chevron: gtk::Label,
    pub(super) icon_label: gtk::Label,
    pub(super) tool_version_chip: gtk::Label,
    pub(super) worktree_badge: gtk::Label,
    pub(super) tab_count_badge: gtk::Label,
    pub(super) status_dot: gtk::Label,
}

#[derive(Clone)]
pub(super) struct PlanMonitorState {
    pub(super) _monitor: gio::FileMonitor,
    pub(super) pending: Rc<Cell<bool>>,
    pub(super) refresh_source: Rc<RefCell<Option<glib::SourceId>>>,
}

pub(super) fn clear_plan_monitor_slot(slot: &Rc<RefCell<Option<PlanMonitorState>>>) {
    if let Some(state) = slot.borrow_mut().take() {
        state.cancel_pending();
    }
}

impl PlanMonitorState {
    pub(super) fn cancel_pending(&self) {
        self.pending.set(false);
        if let Some(source_id) = self.refresh_source.borrow_mut().take() {
            source_id.remove();
        }
    }
}

#[derive(Clone)]
pub(super) struct BackgroundSectionHandle {
    pub(super) section: gtk::Box,
    pub(super) chevron: gtk::Label,
    pub(super) rows_box: gtk::Box,
}

type AgentChildrenSnapshot = (
    bool,
    bool,
    Vec<crate::agent_projection::AgentPaneProjection>,
);

#[derive(Clone)]
pub(super) struct TabRowHandle {
    pub(super) row: gtk::Box,
    pub(super) term_stack: gtk::Stack,
    pub(super) name_label: gtk::Label,
    pub(super) status_badge: gtk::Label,
    /// Container holding one small identity chip per detected agent kind.
    pub(super) agent_badges: gtk::Box,
    pub(super) agent_expand_button: gtk::Button,
    pub(super) agent_children: gtk::Box,
    pub(super) last_agent_children: Rc<RefCell<Option<AgentChildrenSnapshot>>>,
    pub(super) cwd_label: gtk::Label,
    pub(super) branch_label: gtk::Label,
    pub(super) ports_label: gtk::Label,
    pub(super) tmux_label: gtk::Label,
    pub(super) activity_label: gtk::Label,
    pub(super) agent_dot: gtk::Label,
    pub(super) actions_box: gtk::Box,
    pub(super) tracking_box: gtk::Box,
    /// Parsed by the bounded discovery worker; GTK redraws consume this
    /// snapshot instead of reopening `.plan/tasks.json`.
    pub(super) plan_tasks: Rc<RefCell<Option<crate::tracking::PlanTasksData>>>,
    /// Latest discovery submission for this row. A completion may only update
    /// GTK when it still owns this generation, even when the cwd is unchanged.
    pub(super) discovery_generation: Rc<Cell<u64>>,
    pub(super) discovery_poll_source: Rc<RefCell<Option<glib::SourceId>>>,
    pub(super) plan_monitor: Rc<RefCell<Option<PlanMonitorState>>>,
}

impl TabRowHandle {
    pub(super) fn clear_discovery_poll(&self) {
        if let Some(source_id) = self.discovery_poll_source.borrow_mut().take() {
            source_id.remove();
        }
    }

    pub(super) fn set_discovery_poll(&self, source_id: glib::SourceId) {
        self.clear_discovery_poll();
        self.discovery_poll_source.borrow_mut().replace(source_id);
    }

    pub(super) fn clear_plan_monitor(&self) {
        clear_plan_monitor_slot(&self.plan_monitor);
    }

    pub(super) fn set_plan_monitor(&self, monitor: PlanMonitorState) {
        self.clear_plan_monitor();
        self.plan_monitor.borrow_mut().replace(monitor);
    }
}

fn sidebar_key(tab_list: &gtk::Box) -> usize {
    tab_list.as_ptr() as usize
}

pub(super) fn sidebar_registry(tab_list: &gtk::Box) -> Rc<SidebarRegistry> {
    SIDEBAR_REGISTRIES.with(|registries| {
        let mut registries = registries.borrow_mut();
        registries
            .entry(sidebar_key(tab_list))
            .or_insert_with(|| Rc::new(SidebarRegistry::default()))
            .clone()
    })
}

pub(super) fn install_sidebar_registry(tab_list: &gtk::Box, session_label: &gtk::Label) {
    let registry = sidebar_registry(tab_list);
    registry
        .session_label
        .borrow_mut()
        .replace(session_label.clone());
    registry.view_model.borrow_mut().install_session_label();

    let key = sidebar_key(tab_list);
    tab_list.connect_destroy(move |_| {
        SIDEBAR_REGISTRIES.with(|registries| {
            if let Some(registry) = registries.borrow_mut().remove(&key) {
                registry.teardown();
            }
        });
    });
}

pub(crate) fn session_label(tab_list: &gtk::Box) -> Option<gtk::Label> {
    sidebar_registry(tab_list).session_label.borrow().clone()
}

pub(super) fn register_workspace_row(tab_list: &gtk::Box, ws_id: u32, handle: WorkspaceRowHandle) {
    let registry = sidebar_registry(tab_list);
    registry.workspace_rows.borrow_mut().insert(ws_id, handle);
    registry.view_model.borrow_mut().register_workspace(ws_id);
}

pub(super) fn workspace_row_handle(tab_list: &gtk::Box, ws_id: u32) -> Option<WorkspaceRowHandle> {
    sidebar_registry(tab_list)
        .workspace_rows
        .borrow()
        .get(&ws_id)
        .cloned()
}

pub(super) fn remove_workspace_row_handle(
    tab_list: &gtk::Box,
    ws_id: u32,
) -> Option<WorkspaceRowHandle> {
    let registry = sidebar_registry(tab_list);
    let handle = registry.workspace_rows.borrow_mut().remove(&ws_id);
    if handle.is_some() {
        registry.view_model.borrow_mut().remove_workspace(ws_id);
    }
    handle
}

pub(super) fn register_tab_row(tab_list: &gtk::Box, tab_id: u32, handle: TabRowHandle) {
    let registry = sidebar_registry(tab_list);
    registry.tab_rows.borrow_mut().insert(tab_id, handle);
    registry.view_model.borrow_mut().register_tab(tab_id);
}

pub(super) fn tab_row_handle(tab_list: &gtk::Box, tab_id: u32) -> Option<TabRowHandle> {
    sidebar_registry(tab_list)
        .tab_rows
        .borrow()
        .get(&tab_id)
        .cloned()
}

pub(super) fn agent_tab_expanded(tab_list: &gtk::Box, tab_id: u32) -> bool {
    sidebar_registry(tab_list)
        .view_model
        .borrow()
        .agent_tab_expanded(tab_id)
}

pub(super) fn set_agent_tab_expanded(tab_list: &gtk::Box, tab_id: u32, expanded: bool) {
    sidebar_registry(tab_list)
        .view_model
        .borrow_mut()
        .set_agent_tab_expanded(tab_id, expanded);
}

pub(super) fn remove_tab_row_handle(tab_list: &gtk::Box, tab_id: u32) -> Option<TabRowHandle> {
    let registry = sidebar_registry(tab_list);
    let handle = registry.tab_rows.borrow_mut().remove(&tab_id);
    if let Some(handle) = &handle {
        handle.clear_discovery_poll();
        handle.clear_plan_monitor();
        registry.view_model.borrow_mut().remove_tab(tab_id);
    }
    handle
}

pub(super) fn background_section_handle(tab_list: &gtk::Box) -> Option<BackgroundSectionHandle> {
    sidebar_registry(tab_list)
        .background_section
        .borrow()
        .clone()
}

pub(super) fn set_background_section_handle(
    tab_list: &gtk::Box,
    handle: Option<BackgroundSectionHandle>,
) {
    let registry = sidebar_registry(tab_list);
    registry
        .view_model
        .borrow_mut()
        .set_background_section(handle.is_some());
    *registry.background_section.borrow_mut() = handle;
}

pub(super) fn workspace_row_handle_for_header(header: &gtk::Box) -> Option<WorkspaceRowHandle> {
    SIDEBAR_REGISTRIES.with(|registries| {
        for registry in registries.borrow().values() {
            if let Some(handle) = registry
                .workspace_rows
                .borrow()
                .values()
                .find(|handle| handle.header.as_ptr() == header.as_ptr())
                .cloned()
            {
                return Some(handle);
            }
        }
        None
    })
}

pub(super) fn workspace_rows_snapshot(tab_list: &gtk::Box) -> Vec<(u32, WorkspaceRowHandle)> {
    sidebar_registry(tab_list)
        .workspace_rows
        .borrow()
        .iter()
        .map(|(ws_id, handle)| (*ws_id, handle.clone()))
        .collect()
}
