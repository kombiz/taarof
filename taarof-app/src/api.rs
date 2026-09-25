use serde_json::{json, Value};
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};
use vte::prelude::*;

use crate::{
    dashboard::{DashboardHost, DashboardSession, DetachedSession, SessionStatus},
    diagnostics::DiagnosticSnapshot,
    events::{EventRecord, EventStore},
    host::HostStatus,
    pane::{PaneLeaf, PaneProcessState},
    probe::{ProbeSnapshot, ProbeState},
    tracking::TrackingData,
    workspace::{
        AgentActivity, AgentActivityOrigin, AgentActivityState, Tab, TabKind, Workspace,
        WorkspaceStatus,
    },
    AppState,
};

type RuntimeTabPidList = Vec<(u32, Vec<i32>)>;
type RuntimePanePidList = Vec<(u32, u32, i32)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateProjection {
    Workspaces,
    Tabs,
    Panes,
}

struct StateSnapshotIngredients {
    tab_pids: RuntimeTabPidList,
    pane_pids: RuntimePanePidList,
    live_pane_process_states: HashMap<(u32, u32), PaneProcessState>,
    live_pane_agents: HashMap<(u32, u32), crate::agents::AgentStatus>,
    pane_transcripts: HashMap<(u32, u32), crate::agents::TranscriptState>,
    attention_targets: Vec<crate::attention::AttentionTarget>,
    attention: HashMap<(u32, u32), crate::attention::AttentionEvidence>,
}

impl StateSnapshotIngredients {
    fn collect(state: &AppState) -> Self {
        let (tab_pids, pane_pids) = collect_runtime_probe_pids(state);
        let live_pane_process_states = collect_live_pane_process_states(state, &pane_pids);
        let live_pane_agents = collect_live_pane_agents(state, &pane_pids);
        let attention_targets = crate::attention::attention_targets_at(state, unix_time_ms());
        let attention = attention_targets
            .iter()
            .map(|target| ((target.tab_id, target.pane_id), target.evidence.clone()))
            .collect();
        Self {
            tab_pids,
            pane_pids,
            live_pane_process_states,
            live_pane_agents,
            pane_transcripts: state.pane_transcripts.clone(),
            attention_targets,
            attention,
        }
    }
}

pub fn build_list_tabs_snapshot(state: &AppState) -> Value {
    let ingredients = StateSnapshotIngredients::collect(state);
    let workspaces = build_workspaces_projection(state, &ingredients);

    json!({
        "active_workspace": state.active_workspace,
        "workspaces": workspaces,
    })
}

pub fn build_state_snapshot(state: &AppState) -> Value {
    crate::update_watch::spawn_refresh();
    let ingredients = StateSnapshotIngredients::collect(state);
    let workspaces = build_workspaces_projection(state, &ingredients);
    let active_ports = build_active_ports(state);
    let alerts = build_active_alerts(state, &ingredients);
    let recent_alerts = build_recent_alerts(&state.event_store, 20);
    let saved_views = build_saved_views_snapshot();
    let saved_templates = build_saved_templates_snapshot();
    let diagnostics = crate::diagnostics::snapshot();
    let history = state.history.status();
    let health = build_health_snapshot_from_parts(state, &diagnostics);
    let update = crate::update_watch::cached_status();
    // Identity is published alongside `update`, not folded into it: `update`
    // answers "is the running binary the installed one", identity additionally
    // answers "was it built from this source" (EXAMPLE-164). Collapsing the two is
    // the bug this field exists to prevent.
    let identity = crate::runtime_identity::cached();

    json!({
        "schema": "taarof.state.v1",
        "generated_at_unix_ms": unix_time_ms(),
        "session_name": crate::instance::session_name(),
        "active_workspace": state.active_workspace,
        "active_tab": state.active_ws().map(|workspace| workspace.active_tab),
        "capabilities": {
            "events": true,
            "agent_jobs": true,
            "history": history.enabled && history.available,
            "update_watch": true,
            "runtime_identity": true,
        },
        "health": health,
        "update": update,
        "identity": identity,
        "diagnostics": diagnostics_payload(&diagnostics),
        "workspaces": workspaces,
        "active_ports": active_ports,
        "alerts": alerts,
        "recent_alerts": recent_alerts,
        "saved_views": saved_views.items,
        "saved_views_error": saved_views.error,
        "saved_templates": saved_templates.items,
        "saved_templates_error": saved_templates.error,
        "agent_jobs": build_agent_jobs(state, &ingredients),
        "runtime_probe": runtime_probe_payload(state, &ingredients),
        "dashboard": {
            "probe_state": probe_state_label(state.dashboard_state.probe.state),
            "observed_at_unix_ms": state.dashboard_state.probe.observed_at_unix_ms,
            "checked_at_unix_ms": state.dashboard_state.probe.checked_at_unix_ms,
            "error": state.dashboard_state.probe.error.clone(),
            "sessions": state
                .dashboard_state
                .sessions
                .iter()
                .map(dashboard_session_payload)
                .collect::<Vec<_>>(),
            "hosts": state
                .dashboard_state
                .hosts
                .iter()
                .map(dashboard_host_payload)
                .collect::<Vec<_>>(),
        },
        "detached_sessions": state
            .detached_sessions
            .iter()
            .map(detached_session_payload)
            .collect::<Vec<_>>(),
        "events": {
            "high_watermark": state.event_store.high_watermark(),
            "next_seq": state.event_store.next_seq(),
            "stored": state.event_store.len(),
            "capacity": state.event_store.capacity(),
            "dropped": state.event_store.dropped(),
            "last_dropped_at_unix_ms": state.event_store.last_dropped_at_unix_ms(),
        },
        "history": history,
        "work_ledger": state.work_ledger.metadata_json(),
        "work": crate::work_ledger::work_stream_snapshot_json(state),
    })
}

pub fn build_state_projection(state: &AppState, projection: StateProjection) -> Value {
    let ingredients = StateSnapshotIngredients::collect(state);
    match projection {
        StateProjection::Workspaces => {
            Value::Array(build_workspaces_projection(state, &ingredients))
        }
        StateProjection::Tabs => Value::Array(build_tabs_projection(state, &ingredients)),
        StateProjection::Panes => Value::Array(build_panes_projection(state, &ingredients)),
    }
}

pub fn build_health_snapshot(state: &AppState) -> Value {
    let diagnostics = crate::diagnostics::snapshot();
    build_health_snapshot_from_parts(state, &diagnostics)
}

pub fn build_events_snapshot(
    store: &EventStore,
    since_seq: Option<u64>,
    limit: Option<usize>,
) -> Value {
    let query = store.query(since_seq, limit);
    let events: Vec<Value> = query.events.into_iter().map(event_payload).collect();

    json!({
        "schema": "taarof.events.v1",
        "since_seq": since_seq,
        "limit": query.limit,
        "next_seq": query.next_seq,
        "high_watermark": query.high_watermark,
        "oldest_seq": query.oldest_seq,
        "gap": query.gap,
        "gap_from": query.gap_from,
        "gap_to": query.gap_to,
        "resnapshot_required": query.resnapshot_required,
        "capacity": store.capacity(),
        "dropped": store.dropped(),
        "last_dropped_at_unix_ms": store.last_dropped_at_unix_ms(),
        "events": events,
    })
}

struct PersistedCollectionSnapshot {
    items: Vec<Value>,
    error: Option<String>,
}

fn build_saved_views_snapshot() -> PersistedCollectionSnapshot {
    match crate::views::list() {
        Ok(views) => PersistedCollectionSnapshot {
            items: views
                .into_iter()
                .map(|view| {
                    json!({
                        "name": view.name,
                        "preset": view.preset,
                        "limit": view.limit,
                        "effective_limit": view.effective_limit(),
                        "requires_unimplemented_data": view.preset.requires_unimplemented_data(),
                    })
                })
                .collect(),
            error: None,
        },
        Err(error) => {
            eprintln!("taarof: could not load saved views for query-state: {error}");
            PersistedCollectionSnapshot {
                items: Vec::new(),
                error: Some(error.to_string()),
            }
        }
    }
}

fn build_saved_templates_snapshot() -> PersistedCollectionSnapshot {
    match crate::templates::try_list() {
        Ok(templates) => PersistedCollectionSnapshot {
            items: templates
                .into_iter()
                .map(|template| match template {
                    crate::templates::TemplateRecord::Tab { name, tab } => json!({
                        "kind": "tab",
                        "name": name,
                        "tab_name": tab.name,
                        "cwd": tab.cwd,
                        "discovery_cwd": tab.discovery_cwd,
                        "pane_count": tab
                            .panes
                            .as_ref()
                            .map(saved_pane_count)
                            .unwrap_or(0),
                    }),
                    crate::templates::TemplateRecord::Workspace { name, workspace } => json!({
                        "kind": "workspace",
                        "name": name,
                        "tab_count": workspace.tabs.len(),
                        "active_tab_index": workspace.active_tab_index,
                        "tab_names": workspace
                            .tabs
                            .iter()
                            .map(|tab| tab.name.clone())
                            .collect::<Vec<_>>(),
                    }),
                })
                .collect(),
            error: None,
        },
        Err(error) => {
            eprintln!("taarof: could not load templates for query-state: {error}");
            PersistedCollectionSnapshot {
                items: Vec::new(),
                error: Some(error.to_string()),
            }
        }
    }
}

fn saved_pane_count(node: &crate::session::SavedPaneNode) -> usize {
    match node {
        crate::session::SavedPaneNode::Leaf { .. } => 1,
        crate::session::SavedPaneNode::Split { first, second, .. } => {
            saved_pane_count(first) + saved_pane_count(second)
        }
    }
}

fn build_workspaces_projection(
    state: &AppState,
    ingredients: &StateSnapshotIngredients,
) -> Vec<Value> {
    state
        .workspaces
        .iter()
        .map(|workspace| workspace_payload(state, workspace, ingredients))
        .collect()
}

fn build_tabs_projection(state: &AppState, ingredients: &StateSnapshotIngredients) -> Vec<Value> {
    state
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.tabs.iter().map(move |tab| {
                let mut payload = tab_payload(state, tab, ingredients);
                if let Some(object) = payload.as_object_mut() {
                    object.insert("workspace_id".into(), json!(workspace.id));
                    object.insert("workspace_name".into(), json!(workspace.name));
                }
                payload
            })
        })
        .collect()
}

fn build_panes_projection(state: &AppState, ingredients: &StateSnapshotIngredients) -> Vec<Value> {
    state
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.tabs.iter().flat_map(move |tab| {
                let mut panes: Vec<Value> = tab
                    .panes
                    .leaves()
                    .into_iter()
                    .map(|leaf| pane_payload(tab, leaf, ingredients))
                    .collect();
                if panes.is_empty() {
                    if let Some(headless) = state.headless_pane(tab.id, tab.focused_pane_id) {
                        panes.push(headless_pane_payload(tab.focused_pane_id, headless));
                    } else {
                        panes.extend(pending_pane_payloads(state, tab.id));
                    }
                }

                panes.into_iter().map(move |mut payload| {
                    attach_attention_payload(&mut payload, tab.id, ingredients);
                    if let Some(object) = payload.as_object_mut() {
                        object.insert("workspace_id".into(), json!(workspace.id));
                        object.insert("tab_id".into(), json!(tab.id));
                    }
                    payload
                })
            })
        })
        .collect()
}

fn workspace_payload(
    state: &AppState,
    workspace: &Workspace,
    ingredients: &StateSnapshotIngredients,
) -> Value {
    json!({
        "id": workspace.id,
        "name": workspace.name,
        "active_tab": workspace.active_tab,
        "collapsed": workspace.collapsed,
        "repo_root": workspace.repo_root,
        "working_tree_path": workspace.working_tree_path,
        "branch_name": workspace.branch_name,
        "tool_version_chip": workspace_tool_version_chip(state, workspace.id),
        "is_worktree": workspace.is_worktree,
        "linked_issue": workspace.linked_issue,
        "run_status": workspace_status_label(workspace.run_status),
        "tmux_backed": workspace.tmux_backed,
        "host_config_name": workspace.host_config_name,
        "host_status": workspace
            .host_config_name
            .as_ref()
            .map(|_| host_status_payload(&workspace.host_status)),
        "tab_count": workspace.tabs.len(),
        "tabs": workspace
            .tabs
            .iter()
            .map(|tab| tab_payload(state, tab, ingredients))
            .collect::<Vec<_>>(),
    })
}

fn workspace_tool_version_chip(state: &AppState, workspace_id: u32) -> Option<String> {
    crate::mise::discovery_target_for_workspace(state, workspace_id)
        .as_ref()
        .and_then(crate::mise::tool_version_chip_text_for_target)
}

fn tab_payload(state: &AppState, tab: &Tab, ingredients: &StateSnapshotIngredients) -> Value {
    let mut panes: Vec<Value> = tab
        .panes
        .leaves()
        .into_iter()
        .map(|leaf| pane_payload(tab, leaf, ingredients))
        .collect();
    if panes.is_empty() {
        if let Some(headless) = state.headless_pane(tab.id, tab.focused_pane_id) {
            panes.push(headless_pane_payload(tab.focused_pane_id, headless));
        } else {
            panes.extend(pending_pane_payloads(state, tab.id));
        }
    }
    for pane in &mut panes {
        attach_attention_payload(pane, tab.id, ingredients);
    }
    let agents = tab_agents_payload(tab, ingredients);
    let primary_activity = tab.primary_agent_activity();
    let primary_pane_id = primary_activity
        .map(|(pane_id, _)| pane_id)
        .or(tab.agent_pane_id);
    let primary_agent = primary_pane_id.and_then(|pane_id| {
        agents
            .iter()
            .find(|agent| agent["pane_id"] == serde_json::json!(pane_id))
    });

    json!({
        "tab_id": tab.id,
        "name": tab.name,
        "kind": tab_kind_label(tab.kind),
        "focused_pane": tab.focused_pane_id,
        "agent_running": tab.has_fresh_running_activity(),
        "agent_name": primary_agent
            .map(|agent| agent["agent_name"].clone())
            .unwrap_or_else(|| serde_json::json!(tab.agent_name)),
        "agent_session_id": primary_agent
            .map(|agent| agent["session_id"].clone())
            .unwrap_or_else(|| serde_json::json!(tab.agent_session_id)),
        "agent_pane_id": primary_pane_id,
        "agent_activity": primary_activity.map(|(_, activity)| agent_activity_payload(activity)),
        "agents": agents,
        "needs_attention": state.tab_needs_attention(tab),
        "notification_msg": state.tab_notification_message(tab),
        "workspace_action": tab.workspace_action.map(|action| action.task_name()),
        "discovery_cwd": tab.discovery_cwd,
        "listening_ports": tab.listening_ports,
        "ports_updated_at_unix_ms": tab.listening_ports_updated_at_unix_ms,
        "ports_label": crate::agents::format_ports_label(&tab.listening_ports),
        "tracking": tab.tracking_data.as_ref().map(tracking_payload),
        "panes": panes,
    })
}

/// One entry per pane with agent evidence, in pane order. This is the
/// multi-agent view; the flat `agent_*` tab fields describe only the primary.
/// Union of process-probe detection (local panes) and pane agent activity
/// (covers remote/termprop-only agents the probe cannot see).
fn tab_agents_payload(tab: &Tab, ingredients: &StateSnapshotIngredients) -> Vec<Value> {
    // Build the ordered per-pane agent list (union of process probe and
    // pane activity), then assign stable disambiguated labels so the web
    // Monitor can tell two same-kind agents apart the same way the native
    // sidebar does.
    let mut pane_ids: Vec<u32> = tab
        .panes
        .leaves()
        .into_iter()
        .map(|leaf| leaf.pane_id)
        .chain(tab.pane_agent_activity.keys().copied())
        .chain(
            ingredients
                .live_pane_agents
                .keys()
                .filter_map(|(tab_id, pane_id)| (*tab_id == tab.id).then_some(*pane_id)),
        )
        .collect();
    pane_ids.sort_unstable();
    pane_ids.dedup();
    let ordered: Vec<(u32, crate::agents::AgentStatus)> = pane_ids
        .into_iter()
        .filter_map(|pane_id| {
            let probe = ingredients
                .live_pane_agents
                .get(&(tab.id, pane_id))
                .filter(|status| status.running);
            let activity = tab.pane_agent_activity(pane_id);
            if probe.is_none() && activity.is_none() {
                return None;
            }
            let status = match probe {
                Some(status) => status.clone(),
                None => crate::agents::AgentStatus {
                    agent_name: activity.and_then(|activity| activity.source.clone()),
                    session_id: None,
                    running: tab.pane_lifecycle(pane_id).is_working(),
                },
            };
            Some((pane_id, status))
        })
        .collect();

    let instances = crate::runtime_probe::assign_agent_instance_labels(&ordered);
    instances
        .into_iter()
        .map(|instance| {
            let activity = tab.pane_agent_activity(instance.pane_id);
            let mut payload = agent_entry_payload(
                &instance,
                activity.map(agent_activity_payload),
                tab.pane_lifecycle(instance.pane_id),
            );
            if let Some(object) = payload.as_object_mut() {
                object.insert(
                    "attention".into(),
                    ingredients
                        .attention
                        .get(&(tab.id, instance.pane_id))
                        .map(attention_evidence_payload)
                        .unwrap_or(Value::Null),
                );
            }
            payload
        })
        .collect()
}

/// Serialize one resolved agent instance into the `agents[]` wire shape.
/// Pure so the label/field projection is unit-testable without a GTK pane tree.
///
/// `state` is the canonical lifecycle from [`crate::agents::lifecycle`] — the
/// same value the native sidebar badge renders — so web and socket clients
/// never have to re-derive it from `activity` and disagree with the desktop.
fn agent_entry_payload(
    instance: &crate::runtime_probe::AgentInstance,
    activity: Option<Value>,
    state: crate::agents::AgentLifecycle,
) -> Value {
    json!({
        "pane_id": instance.pane_id,
        "agent_name": instance.agent_name,
        "session_id": instance.session_id,
        "instance_label": instance.instance_label,
        "kind_index": instance.kind_index,
        "badge": agent_badge_payload(instance.agent_name.as_deref()),
        "activity": activity.unwrap_or(Value::Null),
        "state": state.wire(),
        "state_label": state.label(),
    })
}

/// Serialize the stable badge metadata for an agent name into the wire shape
/// web surfaces consume directly (no parallel TS table). Resolves via the
/// single source of truth in [`crate::agents::agent_badge`].
fn agent_badge_payload(agent_name: Option<&str>) -> Value {
    let badge = crate::agents::agent_badge(agent_name.unwrap_or(""));
    json!({
        "name": badge.name,
        "short_label": badge.short_label,
        "glyph": badge.glyph,
        "color_token": badge.color_token,
        "known": badge.known,
    })
}

/// Serialize a live pane transcript summary into the wire shape consumed by
/// query-state and external clients. Keeps `files_touched` (every file the agent
/// referenced via a tool) and adds `recent_files` (files it created/edited, with
/// operation and timestamp) for the recent-files feed.
fn transcript_payload(state: &crate::agents::TranscriptState) -> Value {
    json!({
        "last_message": state.last_message,
        "files_touched": state.files_touched,
        "recent_files": state
            .recent_files
            .iter()
            .map(|f| json!({
                "path": f.path,
                "op": f.op.wire(),
                "at_unix_ms": f.at_unix_ms,
            }))
            .collect::<Vec<_>>(),
        "message_count": state.message_count,
        "updated_at_unix_ms": state.updated_at_unix_ms,
        "turn": {
            "phase": match state.turn.phase {
                crate::agents::TurnPhase::Unknown => "unknown",
                crate::agents::TurnPhase::Active => "active",
                crate::agents::TurnPhase::Completed => "completed",
                crate::agents::TurnPhase::Errored => "errored",
            },
            "at_unix_ms": state.turn.at_unix_ms,
        },
        "recent_tool_calls": state
            .recent_tool_calls
            .iter()
            .map(|call| json!({ "tool": call.tool, "target": call.target }))
            .collect::<Vec<_>>(),
        "session_id": state.session_id,
    })
}

fn pane_payload(tab: &Tab, leaf: &PaneLeaf, ingredients: &StateSnapshotIngredients) -> Value {
    let live_pane_process_states = &ingredients.live_pane_process_states;
    let tmux_session = leaf
        .tmux_backing
        .as_ref()
        .map(|backing| backing.session_name.clone());
    let tmux_host = leaf
        .tmux_backing
        .as_ref()
        .and_then(|backing| backing.target.ssh_target_string());
    let tmux_probe = leaf
        .tmux_backing
        .as_ref()
        .map(|backing| tmux_probe_payload(&backing.pane_info));
    let process_state = pane_process_state_for_snapshot(
        tab.id,
        leaf.pane_id,
        leaf.shell_pid,
        &leaf.process_state,
        live_pane_process_states,
    );
    let (attach_supported, attach_kind) = live_pane_attach_metadata(
        leaf.tmux_backing.is_some(),
        leaf.restore_unavailable_reason.is_some(),
    );
    // Broker-owned panes expose the raw PTY adapter; every other live pane is
    // represented by the legacy snapshot capability.
    let pty_capability = if leaf.broker.is_some() {
        "raw_pty"
    } else {
        "legacy_snapshot"
    };
    let (cols, rows) = live_terminal_size(&leaf.terminal)
        .or_else(|| {
            leaf.tmux_backing
                .as_ref()
                .and_then(|backing| tmux_probe_size(&backing.pane_info))
        })
        .unwrap_or((None, None));

    let live_agent = ingredients
        .live_pane_agents
        .get(&(tab.id, leaf.pane_id))
        .filter(|status| status.running);

    let transcript = ingredients
        .pane_transcripts
        .get(&(tab.id, leaf.pane_id))
        .map(transcript_payload);

    pane_payload_from_data(PanePayloadData {
        pane_id: leaf.pane_id,
        shell_running: leaf.shell_pid.is_some(),
        has_child_process: process_state.has_child_process,
        remote_shell: process_state.remote_shell,
        cwd: leaf.location_state.cwd.clone(),
        cwd_host: leaf.location_state.cwd_host.clone(),
        location_updated_at_unix_ms: leaf.location_state.updated_at_unix_ms,
        probe_updated_at_unix_ms: process_state.updated_at_unix_ms,
        tmux_session,
        tmux_host,
        tmux_probe,
        attach_supported,
        attach_kind,
        attach_unavailable_reason: leaf.restore_unavailable_reason.clone().or_else(|| {
            (!attach_supported).then(|| {
                "Browser viewer reconnect unavailable: this live pane has no supported viewer target."
                    .to_string()
            })
        }),
        pty_capability,
        cols,
        rows,
        agent_name: live_agent.and_then(|status| status.agent_name.clone()),
        agent_session_id: live_agent.and_then(|status| status.session_id.clone()),
        transcript,
        current_task: crate::task_binding::current_task_payload(
            leaf.location_state.cwd.as_deref(),
            leaf.location_state.cwd_host.as_deref(),
            leaf.process_state.remote_shell,
            leaf.current_task.as_ref(),
        ),
    })
}

fn headless_pane_payload(pane_id: u32, pane: &crate::runtime::HeadlessPaneState) -> Value {
    let tmux_session = pane
        .tmux_backing
        .as_ref()
        .map(|backing| backing.session_name.clone());
    let tmux_host = pane
        .tmux_backing
        .as_ref()
        .and_then(|backing| backing.target.ssh_target_string());
    let tmux_probe = pane
        .tmux_backing
        .as_ref()
        .map(|backing| tmux_probe_payload(&backing.pane_info));
    let headless_kind = if pane.tmux_backing.is_some() {
        PaneAttachWireKind::Tmux
    } else {
        PaneAttachWireKind::Unsupported
    };
    let (attach_supported, attach_kind) = pane_attach_metadata(headless_kind);
    let (cols, rows) = pane
        .tmux_backing
        .as_ref()
        .and_then(|backing| tmux_probe_size(&backing.pane_info))
        .unwrap_or((None, None));

    pane_payload_from_data(PanePayloadData {
        pane_id,
        shell_running: pane.shell_running,
        has_child_process: pane.process_state.has_child_process,
        remote_shell: pane.process_state.remote_shell,
        cwd: pane.location_state.cwd.clone(),
        cwd_host: pane.location_state.cwd_host.clone(),
        location_updated_at_unix_ms: pane.location_state.updated_at_unix_ms,
        probe_updated_at_unix_ms: pane.process_state.updated_at_unix_ms,
        tmux_session,
        tmux_host,
        tmux_probe,
        attach_supported,
        attach_kind,
        attach_unavailable_reason: (!attach_supported).then(|| {
            "Browser viewer reconnect unavailable: this headless pane has no supported viewer target."
                .to_string()
        }),
        pty_capability: "legacy_snapshot",
        cols,
        rows,
        agent_name: None,
        agent_session_id: None,
        transcript: None,
        current_task: crate::task_binding::current_task_payload(
            pane.location_state.cwd.as_deref(),
            pane.location_state.cwd_host.as_deref(),
            pane.process_state.remote_shell,
            pane.current_task.as_ref(),
        ),
    })
}

fn pending_pane_payloads(state: &AppState, tab_id: u32) -> Vec<Value> {
    let Some(pending) = state.pending_tab_restores.get(&tab_id) else {
        return Vec::new();
    };
    crate::terminal::plan_restored_spawns(
        &pending.saved,
        crate::config::should_auto_resume_agents_on_session_restore(),
    )
    .into_iter()
    .map(|pane| {
        let attach_unavailable_reason = if pane.tmux_session.is_some() && pane.spawn_cmd.is_none() {
            "Browser viewer reconnect unavailable: native Reattach live terminal lacks the exact saved tmux generation."
        } else {
            "Browser viewer reconnect unavailable until the native saved pane is materialized."
        };
        pane_payload_from_data(PanePayloadData {
            pane_id: pane.pane_id,
            shell_running: false,
            has_child_process: false,
            remote_shell: pane.remote_shell,
            cwd: pane.cwd.clone(),
            cwd_host: pane.cwd_host.clone(),
            location_updated_at_unix_ms: None,
            probe_updated_at_unix_ms: None,
            tmux_session: pane.tmux_session,
            tmux_host: pane.tmux_host,
            tmux_probe: None,
            // Pending panes have no live resolver target until materialized.
            attach_supported: false,
            attach_kind: "unsupported",
            attach_unavailable_reason: Some(attach_unavailable_reason.to_string()),
            pty_capability: "legacy_snapshot",
            cols: None,
            rows: None,
            agent_name: None,
            agent_session_id: None,
            transcript: None,
            current_task: crate::task_binding::current_task_payload(
                pane.cwd.as_deref(),
                pane.cwd_host.as_deref(),
                pane.remote_shell,
                pane.current_task.as_ref(),
            ),
        })
    })
    .collect()
}

fn collect_live_pane_process_states(
    state: &AppState,
    pane_pids: &RuntimePanePidList,
) -> HashMap<(u32, u32), PaneProcessState> {
    let source = crate::runtime_probe::ProcProbeSource;
    crate::runtime_probe::pane_process_states_from_cached_probe(
        &source,
        state.runtime_probe.as_ref(),
        unix_time_ms(),
        pane_pids,
        crate::runtime_probe::PROBE_TTL_MS,
    )
}

fn collect_live_pane_agents(
    state: &AppState,
    pane_pids: &RuntimePanePidList,
) -> HashMap<(u32, u32), crate::agents::AgentStatus> {
    let source = crate::runtime_probe::ProcProbeSource;
    crate::runtime_probe::pane_agents_from_cached_probe(
        &source,
        state.runtime_probe.as_ref(),
        unix_time_ms(),
        pane_pids,
        crate::runtime_probe::PROBE_TTL_MS,
    )
}

fn collect_runtime_probe_pids(state: &AppState) -> (RuntimeTabPidList, RuntimePanePidList) {
    let mut tab_pids = Vec::new();
    let mut pane_pids = Vec::new();
    let now_ms = crate::events::unix_time_ms();
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            tab_pids.push((tab.id, tab.panes.collect_pids()));
            pane_pids.extend(tab.panes.leaves().into_iter().filter_map(|leaf| {
                crate::runtime_probe::pane_process_root(leaf, now_ms)
                    .map(|pid| (tab.id, leaf.pane_id, pid))
            }));
        }
    }
    (tab_pids, pane_pids)
}

fn pane_process_state_for_snapshot(
    tab_id: u32,
    pane_id: u32,
    shell_pid: Option<i32>,
    cached_process_state: &PaneProcessState,
    live_pane_process_states: &HashMap<(u32, u32), PaneProcessState>,
) -> PaneProcessState {
    if shell_pid.is_some() {
        live_pane_process_states
            .get(&(tab_id, pane_id))
            .cloned()
            .unwrap_or_else(|| cached_process_state.clone())
    } else {
        cached_process_state.clone()
    }
}

#[derive(Debug, Clone)]
struct PanePayloadData {
    pane_id: u32,
    shell_running: bool,
    has_child_process: bool,
    remote_shell: bool,
    cwd: Option<String>,
    cwd_host: Option<String>,
    location_updated_at_unix_ms: Option<u64>,
    probe_updated_at_unix_ms: Option<u64>,
    tmux_session: Option<String>,
    tmux_host: Option<String>,
    tmux_probe: Option<Value>,
    attach_supported: bool,
    attach_kind: &'static str,
    attach_unavailable_reason: Option<String>,
    pty_capability: &'static str,
    cols: Option<u32>,
    rows: Option<u32>,
    agent_name: Option<String>,
    agent_session_id: Option<String>,
    transcript: Option<Value>,
    current_task: Value,
}

fn pane_payload_from_data(data: PanePayloadData) -> Value {
    // The process probe's `remote_shell` bit is best-effort. A remote tmux
    // backing is authoritative even when the local SSH process was missed, so
    // never project the local process-tree observation as remote command state.
    let remote_process_state_unknown = data.remote_shell || data.tmux_host.is_some();
    let has_child_process = (!remote_process_state_unknown).then_some(data.has_child_process);
    json!({
        "pane_id": data.pane_id,
        "shell_running": data.shell_running,
        "has_child_process": has_child_process,
        "remote_shell": data.remote_shell,
        "cwd": data.cwd,
        "cwd_host": data.cwd_host,
        "location_updated_at_unix_ms": data.location_updated_at_unix_ms,
        "probe_updated_at_unix_ms": data.probe_updated_at_unix_ms,
        "tmux_session": data.tmux_session,
        "tmux_host": data.tmux_host,
        "tmux_probe": data.tmux_probe,
        "attach_supported": data.attach_supported,
        "attach_kind": data.attach_kind,
        "attach_unavailable_reason": data.attach_unavailable_reason,
        "pty_capability": data.pty_capability,
        "cols": data.cols,
        "rows": data.rows,
        "agent_name": data.agent_name,
        "agent_session_id": data.agent_session_id,
        "current_task": data.current_task,
        "transcript": data.transcript.unwrap_or(Value::Null),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaneAttachWireKind {
    Tmux,
    Vte,
    Unsupported,
}

fn pane_attach_metadata(kind: PaneAttachWireKind) -> (bool, &'static str) {
    match kind {
        PaneAttachWireKind::Tmux => (true, "tmux"),
        PaneAttachWireKind::Vte => (true, "vte"),
        PaneAttachWireKind::Unsupported => (false, "unsupported"),
    }
}

fn live_pane_attach_metadata(
    has_tmux_backing: bool,
    restore_unavailable: bool,
) -> (bool, &'static str) {
    let kind = if restore_unavailable {
        PaneAttachWireKind::Unsupported
    } else if has_tmux_backing {
        PaneAttachWireKind::Tmux
    } else {
        PaneAttachWireKind::Vte
    };
    pane_attach_metadata(kind)
}

fn live_terminal_size(terminal: &vte::Terminal) -> Option<(Option<u32>, Option<u32>)> {
    let cols = terminal.column_count();
    let rows = terminal.row_count();
    if cols > 0 && rows > 0 {
        Some((Some(cols as u32), Some(rows as u32)))
    } else {
        None
    }
}

fn tmux_probe_size(
    status: &ProbeSnapshot<crate::tmux::TmuxPaneInfo>,
) -> Option<(Option<u32>, Option<u32>)> {
    status
        .value()
        .map(|value| (Some(value.width), Some(value.height)))
}

fn agent_activity_payload(activity: &AgentActivity) -> Value {
    json!({
        "state": agent_activity_state_label(activity.state),
        "text": activity.text,
        "source": activity.source,
        "origin": agent_activity_origin_label(activity.origin),
        "observed_at_unix_ms": activity.observed_at_unix_ms,
    })
}

fn attention_evidence_payload(evidence: &crate::attention::AttentionEvidence) -> Value {
    json!({
        "reason": evidence.reason.wire(),
        "provider": evidence.provider,
        "provenance": evidence.provenance,
        "authority": evidence.authority.wire(),
        "freshness": evidence.freshness.wire(),
        "last_verified_unix_ms": evidence.last_verified_unix_ms,
    })
}

fn attach_attention_payload(
    payload: &mut Value,
    tab_id: u32,
    ingredients: &StateSnapshotIngredients,
) {
    let pane_id = payload
        .get("pane_id")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    let attention = pane_id
        .and_then(|pane_id| ingredients.attention.get(&(tab_id, pane_id)))
        .map(attention_evidence_payload)
        .unwrap_or(Value::Null);
    if let Some(object) = payload.as_object_mut() {
        object.insert("attention".into(), attention);
    }
}

fn tracking_payload(tracking: &TrackingData) -> Value {
    json!({
        "total": tracking.total,
        "done": tracking.done,
        "in_progress": tracking.in_progress,
        "queued": tracking.queued,
        "percent_complete": tracking.percent_complete(),
        "features": tracking
            .features
            .iter()
            .map(|feature| {
                json!({
                    "id": feature.id,
                    "title": feature.title,
                    "status": feature.status,
                })
            })
            .collect::<Vec<_>>(),
    })
}

fn build_agent_jobs(state: &AppState, ingredients: &StateSnapshotIngredients) -> Vec<Value> {
    state
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.tabs.iter().flat_map(move |tab| {
                tab_agents_payload(tab, ingredients)
                    .into_iter()
                    .filter(|agent| {
                        agent["pane_id"]
                            .as_u64()
                            .is_some_and(|pane_id| tab.pane_lifecycle(pane_id as u32).is_working())
                    })
                    .map(move |agent| {
                        json!({
                            "workspace_id": workspace.id,
                            "workspace_name": workspace.name,
                            "tab_id": tab.id,
                            "tab_name": tab.name,
                            "pane_id": agent["pane_id"],
                            "agent_name": agent["agent_name"],
                            "session_id": agent["session_id"],
                            "instance_label": agent["instance_label"],
                            "activity": agent["activity"],
                        })
                    })
            })
        })
        .collect()
}

fn build_active_ports(state: &AppState) -> Vec<Value> {
    state
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.tabs.iter().flat_map(move |tab| {
                tab.listening_ports.iter().map(move |port| {
                    json!({
                        "workspace_id": workspace.id,
                        "workspace_name": workspace.name,
                        "tab_id": tab.id,
                        "tab_name": tab.name,
                        "ports_updated_at_unix_ms": tab.listening_ports_updated_at_unix_ms,
                        "port": port,
                    })
                })
            })
        })
        .collect()
}

fn build_active_alerts(state: &AppState, ingredients: &StateSnapshotIngredients) -> Vec<Value> {
    ingredients
        .attention_targets
        .iter()
        .filter_map(|target| {
            let (_, tab) = state.find_tab(target.tab_id)?;
            Some(json!({
                "workspace_id": target.workspace_id,
                "workspace_name": target.workspace_name,
                "repository": target.repository,
                "worktree": target.worktree,
                "machine": target.machine,
                "tab_id": target.tab_id,
                "tab_name": target.tab_name,
                "pane_id": target.pane_id,
                "message": active_alert_message(state, tab, target),
                "agent_running": tab.pane_lifecycle(target.pane_id).is_working(),
                "agent_activity": tab.pane_agent_activity(target.pane_id).map(agent_activity_payload),
                "attention": attention_evidence_payload(&target.evidence),
            }))
        })
        .collect()
}

fn active_alert_message<'a>(
    state: &'a AppState,
    tab: &'a Tab,
    target: &crate::attention::AttentionTarget,
) -> &'a str {
    let verified_signal = (target.evidence.freshness
        == crate::attention::AttentionFreshness::Fresh
        && target.evidence.reason != crate::attention::AttentionReason::Unknown
        && matches!(
            target.evidence.authority,
            crate::attention::AttentionAuthority::ProviderExplicit
                | crate::attention::AttentionAuthority::TerminalHeuristic
        ))
    .then(|| tab.pane_agent_activity(target.pane_id))
    .flatten()
    .map(|activity| activity.text.as_str());
    let exact_notification = (target.evidence.authority
        == crate::attention::AttentionAuthority::System)
        .then(|| {
            let notification_pane_id = tab.notification_pane_id.unwrap_or(tab.focused_pane_id);
            (notification_pane_id == target.pane_id)
                .then(|| state.tab_notification_message(tab))
                .flatten()
        })
        .flatten();
    verified_signal.or(exact_notification).unwrap_or(&tab.name)
}

fn runtime_probe_payload(state: &AppState, ingredients: &StateSnapshotIngredients) -> Value {
    let now = unix_time_ms();
    let truth = crate::runtime_probe::runtime_probe_truth(
        state.runtime_probe.as_ref(),
        &ingredients.tab_pids,
        &ingredients.pane_pids,
        now,
        crate::runtime_probe::PROBE_TTL_MS,
    );
    runtime_probe_payload_from_truth(&truth, state.runtime_probe.as_ref())
}

fn runtime_probe_payload_from_truth(
    truth: &crate::runtime_probe::RuntimeProbeTruth,
    snapshot: Option<&crate::runtime_probe::RuntimeProbeSnapshot>,
) -> Value {
    json!({
        "state": truth.state.label(),
        "degraded": truth.state.is_degraded(),
        "process_probe_state": probe_state_label(truth.process_state),
        "ports_probe_state": probe_state_label(truth.ports_state),
        "probed_at_unix_ms": truth.checked_at_unix_ms,
        "checked_at_unix_ms": truth.checked_at_unix_ms,
        "cache_ttl_ms": crate::runtime_probe::PROBE_TTL_MS,
        "cache_fresh": matches!(truth.state, crate::runtime_probe::RuntimeProbeState::Ok),
        "tab_pid_count": snapshot.map_or(0, |snapshot| snapshot.tab_pids.len()),
        "pane_pid_count": snapshot.map_or(0, |snapshot| snapshot.pane_pids.len()),
        "process_error": truth.process_error,
        "ports_error": truth.ports_error,
        "process": {
            "state": probe_state_label(truth.process_state),
            "observed_at_unix_ms": truth.process_observed_at_unix_ms,
            "checked_at_unix_ms": truth.checked_at_unix_ms,
            "age_ms": truth.process_age_ms,
            "error": truth.process_error,
        },
        "ports": {
            "state": probe_state_label(truth.ports_state),
            "observed_at_unix_ms": truth.ports_observed_at_unix_ms,
            "checked_at_unix_ms": truth.checked_at_unix_ms,
            "age_ms": truth.ports_age_ms,
            "error": truth.ports_error,
        },
    })
}

fn build_recent_alerts(store: &EventStore, limit: usize) -> Vec<Value> {
    let mut events: Vec<EventRecord> = store
        .entries()
        .into_iter()
        .rev()
        .filter(|event| event.event_type.starts_with("alert_"))
        .take(limit)
        .collect();
    events.reverse();
    events.into_iter().map(event_payload).collect()
}

fn diagnostics_payload(snapshot: &DiagnosticSnapshot) -> Value {
    json!({
        "log_path": snapshot.log_path,
        "archive_path": snapshot.archive_path,
        "retention": {
            "recent_record_limit": snapshot.retention.recent_record_limit,
            "log_max_bytes": snapshot.retention.log_max_bytes,
            "archive_count": snapshot.retention.archive_count,
        },
        "counters": {
            "total_records": snapshot.counters.total_records,
            "warning_records": snapshot.counters.warning_records,
            "error_records": snapshot.counters.error_records,
            "command_failures": snapshot.counters.command_failures,
            "probe_failures": snapshot.counters.probe_failures,
            "event_drops": snapshot.counters.event_drops,
            "lifecycle_events": snapshot.counters.lifecycle_events,
            "write_failures": snapshot.counters.write_failures,
        },
        "last_failure_at_unix_ms": snapshot.last_failure_at_unix_ms,
        "last_event_drop_at_unix_ms": snapshot.last_event_drop_at_unix_ms,
        "recent": snapshot.recent,
    })
}

fn build_health_snapshot_from_parts(state: &AppState, diagnostics: &DiagnosticSnapshot) -> Value {
    let event_retention = state.event_store.retention_health();
    let (tab_pids, pane_pids) = collect_runtime_probe_pids(state);
    let runtime_probe_truth = crate::runtime_probe::runtime_probe_truth(
        state.runtime_probe.as_ref(),
        &tab_pids,
        &pane_pids,
        unix_time_ms(),
        crate::runtime_probe::PROBE_TTL_MS,
    );
    let components = build_degraded_components(state, &event_retention, &runtime_probe_truth);
    let last_component_error_at = components
        .iter()
        .filter_map(|component| component["checked_at_unix_ms"].as_u64())
        .max();
    let last_error_at_unix_ms = [last_component_error_at, diagnostics.last_failure_at_unix_ms]
        .into_iter()
        .flatten()
        .max();
    let update = crate::update_watch::cached_status();

    json!({
        "state": if components.is_empty() { "ok" } else { "degraded" },
        "degraded": !components.is_empty(),
        "degraded_components": components.len(),
        "components": components,
        "probe_failures": diagnostics.counters.probe_failures,
        "command_failures": diagnostics.counters.command_failures,
        "events": event_retention,
        "runtime_probe": runtime_probe_payload_from_truth(
            &runtime_probe_truth,
            state.runtime_probe.as_ref(),
        ),
        "event_drops": state.event_store.dropped(),
        "last_error_at_unix_ms": last_error_at_unix_ms,
        "last_event_drop_at_unix_ms": state.event_store.last_dropped_at_unix_ms(),
        "update": {
            "state": update.state,
            "reason": update.reason,
            "checked_at_unix_ms": update.checked_at_unix_ms,
        },
    })
}

fn build_degraded_components(
    state: &AppState,
    event_retention: &crate::events::EventRetentionHealth,
    runtime_probe_truth: &crate::runtime_probe::RuntimeProbeTruth,
) -> Vec<Value> {
    let mut components = runtime_probe_health_components(runtime_probe_truth);

    let history = state.history.status();
    let history_state_failed =
        history.state == "misconfigured" || (history.enabled && history.state != "ok");
    let maintenance_failed = !history_state_failed && history.maintenance.last_result == "failed";
    if history_state_failed || maintenance_failed {
        components.push(json!({
            "kind": "history",
            "label": "sqlite-history",
            "state": if maintenance_failed { "maintenance_failed" } else { &history.state },
            "error": if maintenance_failed {
                history.maintenance.last_error.as_ref()
            } else {
                history.reason.as_ref()
            },
            "checked_at_unix_ms": if maintenance_failed {
                history.maintenance.last_run_at_unix_ms
            } else {
                history.last_commit_at_unix_ms
            },
            "observed_at_unix_ms": if maintenance_failed {
                history.maintenance.last_run_at_unix_ms
            } else {
                history.last_commit_at_unix_ms
            },
        }));
    }

    if state.dashboard_state.probe.state.is_degraded() {
        components.push(json!({
            "kind": "dashboard",
            "label": "dashboard-state",
            "state": probe_state_label(state.dashboard_state.probe.state),
            "error": state.dashboard_state.probe.error,
            "checked_at_unix_ms": state.dashboard_state.probe.checked_at_unix_ms,
            "observed_at_unix_ms": state.dashboard_state.probe.observed_at_unix_ms,
        }));
    }

    for workspace in &state.workspaces {
        if workspace.host_status.state.is_degraded() {
            components.push(json!({
                "kind": "host",
                "label": workspace.host_config_name.as_deref().unwrap_or(&workspace.name),
                "workspace_id": workspace.id,
                "workspace_name": workspace.name,
                "state": probe_state_label(workspace.host_status.state),
                "error": workspace.host_status.error,
                "checked_at_unix_ms": workspace.host_status.checked_at_unix_ms,
                "observed_at_unix_ms": workspace.host_status.observed_at_unix_ms,
            }));
        }

        for tab in &workspace.tabs {
            for leaf in tab.panes.leaves() {
                let Some(backing) = leaf.tmux_backing.as_ref() else {
                    continue;
                };
                if !backing.pane_info.state.is_degraded() {
                    continue;
                }
                components.push(json!({
                    "kind": "tmux-pane",
                    "label": backing.session_name,
                    "workspace_id": workspace.id,
                    "workspace_name": workspace.name,
                    "tab_id": tab.id,
                    "tab_name": tab.name,
                    "pane_id": leaf.pane_id,
                    "state": probe_state_label(backing.pane_info.state),
                    "error": backing.pane_info.error,
                    "checked_at_unix_ms": backing.pane_info.checked_at_unix_ms,
                    "observed_at_unix_ms": backing.pane_info.observed_at_unix_ms,
                    "ssh_target": backing.target.ssh_target_string(),
                }));
            }
        }
    }

    if event_retention.state == "degraded" {
        let observed_at_unix_ms = if event_retention.active_cursor_loss {
            event_retention.last_cursor_gap_at_unix_ms
        } else {
            event_retention.last_dropped_at_unix_ms
        };
        components.push(json!({
            "kind": "events",
            "label": "event-store",
            "state": "degraded",
            "error": event_retention.reason.as_deref(),
            "checked_at_unix_ms": observed_at_unix_ms,
            "observed_at_unix_ms": observed_at_unix_ms,
        }));
    }

    components
}

fn runtime_probe_health_components(truth: &crate::runtime_probe::RuntimeProbeTruth) -> Vec<Value> {
    if matches!(truth.state, crate::runtime_probe::RuntimeProbeState::Absent) {
        return Vec::new();
    }

    [
        (
            "process-source",
            truth.process_state,
            truth.process_error.as_deref(),
            truth.process_observed_at_unix_ms,
            truth.process_age_ms,
        ),
        (
            "port-source",
            truth.ports_state,
            truth.ports_error.as_deref(),
            truth.ports_observed_at_unix_ms,
            truth.ports_age_ms,
        ),
    ]
    .into_iter()
    .filter(|(_, state, _, _, _)| !matches!(state, ProbeState::Ok))
    .map(|(label, state, error, observed_at, observed_age)| {
        json!({
            "kind": "runtime-probe",
            "label": label,
            "state": probe_state_label(state),
            "error": error,
            "checked_at_unix_ms": truth.checked_at_unix_ms,
            "observed_at_unix_ms": observed_at,
            "observed_age_ms": observed_age,
        })
    })
    .collect()
}

fn detached_session_payload(session: &DetachedSession) -> Value {
    json!({
        "session_name": session.session_name,
        "host": session.host,
        "workspace": session.workspace,
        "ssh_target": session.target.ssh_target_string(),
        "last_command": session.last_command,
        "finished": session.finished,
        "is_detached": true,
    })
}

fn dashboard_session_payload(session: &DashboardSession) -> Value {
    json!({
        "name": session.name,
        "host": session.host,
        "status": session_status_label(&session.status),
        "command": session.command,
        "started": session.started,
        "pid": session.pid,
        "output_preview": session.output_preview,
        "is_detached": session.is_detached,
        "is_taarof_managed": session.is_taarof_managed,
        "ssh_target": session.target.ssh_target_string(),
    })
}

fn dashboard_host_payload(host: &DashboardHost) -> Value {
    json!({
        "name": host.name,
        "cpu_percent": host.cpu_percent,
        "memory_percent": host.memory_percent,
        "session_count": host.session_count,
        "probe_state": probe_state_label(host.probe.state),
        "observed_at_unix_ms": host.probe.observed_at_unix_ms,
        "checked_at_unix_ms": host.probe.checked_at_unix_ms,
        "error": host.probe.error.clone(),
    })
}

fn host_status_payload(status: &ProbeSnapshot<HostStatus>) -> Value {
    json!({
        "state": probe_state_label(status.state),
        "observed_at_unix_ms": status.observed_at_unix_ms,
        "checked_at_unix_ms": status.checked_at_unix_ms,
        "error": status.error.clone(),
        "session_count": status.value().map(|value| value.session_count),
        "cpu_load_percent": status.value().map(|value| value.cpu_load_percent),
        "memory_used_percent": status.value().map(|value| value.memory_used_percent),
    })
}

fn tmux_probe_payload(status: &ProbeSnapshot<crate::tmux::TmuxPaneInfo>) -> Value {
    json!({
        "state": probe_state_label(status.state),
        "observed_at_unix_ms": status.observed_at_unix_ms,
        "checked_at_unix_ms": status.checked_at_unix_ms,
        "error": status.error.clone(),
        "current_command": status.value().map(|value| value.current_command.clone()),
        "cwd": status.value().map(|value| value.cwd.clone()),
        "pid": status.value().map(|value| value.pid),
        "width": status.value().map(|value| value.width),
        "height": status.value().map(|value| value.height),
    })
}

fn event_payload(event: EventRecord) -> Value {
    json!({
        "seq": event.seq,
        "ts_unix_ms": event.ts_unix_ms,
        "event_type": event.event_type,
        "payload": event.payload,
    })
}

fn workspace_status_label(status: WorkspaceStatus) -> &'static str {
    match status {
        WorkspaceStatus::Idle => "idle",
        WorkspaceStatus::Running => "running",
        WorkspaceStatus::Errored => "errored",
    }
}

fn tab_kind_label(kind: TabKind) -> &'static str {
    match kind {
        TabKind::Terminal => "terminal",
        TabKind::Dashboard => "dashboard",
    }
}

fn agent_activity_state_label(state: AgentActivityState) -> &'static str {
    match state {
        AgentActivityState::Idle => "idle",
        AgentActivityState::Running => "running",
        AgentActivityState::WaitingInput => "waiting-input",
        AgentActivityState::Errored => "errored",
        AgentActivityState::Done => "done",
    }
}

fn agent_activity_origin_label(origin: AgentActivityOrigin) -> &'static str {
    match origin {
        AgentActivityOrigin::Socket => "socket",
        AgentActivityOrigin::Termprop => "termprop",
        AgentActivityOrigin::OutputScan => "output-scan",
    }
}

fn session_status_label(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Running => "running",
        SessionStatus::Idle => "idle",
        SessionStatus::Detached => "detached",
        SessionStatus::Finished => "finished",
    }
}

fn probe_state_label(state: ProbeState) -> &'static str {
    state.label()
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::{
        agent_badge_payload, agent_entry_payload, build_active_alerts,
        build_health_snapshot_from_parts, host_status_payload, pane_attach_metadata,
        pane_payload_from_data, pane_process_state_for_snapshot, runtime_probe_health_components,
        runtime_probe_payload_from_truth, tab_payload, tmux_probe_payload, transcript_payload,
        PaneAttachWireKind, PanePayloadData, StateSnapshotIngredients,
    };
    use crate::agents::{FileOp, TouchedFile, TranscriptState};
    use crate::{
        diagnostics::DiagnosticSnapshot,
        host::HostStatus,
        mise::{
            clear_tool_version_cache_for_test, clear_tool_version_test_probe,
            install_tool_version_test_probe, task_discovery_test_guard, tool_version_test_entry,
            wait_for_tool_version_chip, DiscoveryTarget,
        },
        pane::{PaneNode, PaneProcessState},
        probe::{ProbeSnapshot, ProbeState},
        tmux::TmuxPaneInfo,
        workspace::{AgentActivity, AgentActivityState, Tab, TabKind},
        AppState, HeadlessPaneSeed,
    };
    use std::collections::HashMap;
    use std::{fs, path::PathBuf};

    fn empty_ingredients() -> StateSnapshotIngredients {
        StateSnapshotIngredients {
            tab_pids: Vec::new(),
            pane_pids: Vec::new(),
            live_pane_process_states: HashMap::new(),
            live_pane_agents: HashMap::new(),
            pane_transcripts: HashMap::new(),
            attention_targets: Vec::new(),
            attention: HashMap::new(),
        }
    }

    #[test]
    fn runtime_probe_failure_api_distinguishes_partial_stale_truth() {
        let truth = crate::runtime_probe::RuntimeProbeTruth {
            state: crate::runtime_probe::RuntimeProbeState::Partial,
            process_state: ProbeState::Stale,
            ports_state: ProbeState::Ok,
            process_observed_at_unix_ms: Some(500),
            ports_observed_at_unix_ms: Some(1_500),
            process_age_ms: Some(1_500),
            ports_age_ms: Some(500),
            process_error: Some("process source unreadable".to_string()),
            ports_error: None,
            checked_at_unix_ms: Some(2_000),
        };

        let payload = runtime_probe_payload_from_truth(&truth, None);

        assert_eq!(payload["state"], serde_json::json!("partial"));
        assert_eq!(payload["process"]["state"], serde_json::json!("stale"));
        assert_eq!(payload["process"]["age_ms"], serde_json::json!(1_500));
        assert_eq!(payload["ports"]["state"], serde_json::json!("ok"));
        assert_eq!(payload["ports"]["age_ms"], serde_json::json!(500));
        assert_eq!(
            payload["process"]["error"],
            serde_json::json!("process source unreadable")
        );
    }

    #[test]
    fn runtime_probe_failure_health_names_each_degraded_source() {
        let truth = crate::runtime_probe::RuntimeProbeTruth {
            state: crate::runtime_probe::RuntimeProbeState::Partial,
            process_state: ProbeState::Stale,
            ports_state: ProbeState::Ok,
            process_observed_at_unix_ms: Some(500),
            ports_observed_at_unix_ms: Some(1_500),
            process_age_ms: Some(1_500),
            ports_age_ms: Some(500),
            process_error: Some("process source unreadable".to_string()),
            ports_error: None,
            checked_at_unix_ms: Some(2_000),
        };

        let components = runtime_probe_health_components(&truth);

        assert_eq!(components.len(), 1);
        assert_eq!(components[0]["kind"], serde_json::json!("runtime-probe"));
        assert_eq!(components[0]["label"], serde_json::json!("process-source"));
        assert_eq!(components[0]["state"], serde_json::json!("stale"));
        assert_eq!(components[0]["observed_age_ms"], serde_json::json!(1_500));
    }

    #[test]
    fn runtime_probe_partial_port_source_is_independent_in_api_and_health() {
        let truth = crate::runtime_probe::RuntimeProbeTruth {
            state: crate::runtime_probe::RuntimeProbeState::Partial,
            process_state: ProbeState::Ok,
            ports_state: ProbeState::Stale,
            process_observed_at_unix_ms: Some(2_000),
            ports_observed_at_unix_ms: Some(500),
            process_age_ms: Some(0),
            ports_age_ms: Some(1_500),
            process_error: None,
            ports_error: Some("3 of 4 /proc/net tables unreadable".to_string()),
            checked_at_unix_ms: Some(2_000),
        };

        let payload = runtime_probe_payload_from_truth(&truth, None);
        assert_eq!(payload["state"], serde_json::json!("partial"));
        assert_eq!(payload["process"]["state"], serde_json::json!("ok"));
        assert_eq!(payload["ports"]["state"], serde_json::json!("stale"));
        assert_eq!(payload["ports"]["age_ms"], serde_json::json!(1_500));
        assert_eq!(
            payload["ports"]["error"],
            serde_json::json!("3 of 4 /proc/net tables unreadable")
        );

        let components = runtime_probe_health_components(&truth);
        assert_eq!(components.len(), 1);
        assert_eq!(components[0]["label"], serde_json::json!("port-source"));
        assert_eq!(components[0]["state"], serde_json::json!("stale"));
        assert_eq!(components[0]["observed_age_ms"], serde_json::json!(1_500));
    }

    #[test]
    fn health_snapshot_keeps_historical_event_drops_without_permanent_degradation() {
        let mut state = AppState::new();
        state.event_store = crate::events::EventStore::with_capacity(2);
        state.event_store.emit("one", serde_json::json!({}));
        state.event_store.emit("two", serde_json::json!({}));
        state.event_store.emit("three", serde_json::json!({}));

        let snapshot = build_health_snapshot_from_parts(&state, &DiagnosticSnapshot::default());
        assert_eq!(snapshot["state"], serde_json::json!("ok"));
        assert_eq!(snapshot["events"]["state"], serde_json::json!("ok"));
        assert_eq!(snapshot["events"]["dropped_total"], 1);
        assert_eq!(snapshot["events"]["drops_in_window"], 1);
        assert_eq!(snapshot["events"]["cursor_gaps_total"], 0);
        assert!(snapshot["components"]
            .as_array()
            .unwrap()
            .iter()
            .all(|component| component["kind"] != "events"));
    }

    #[test]
    fn health_snapshot_degrades_for_an_active_cursor_gap_with_actionable_reason() {
        let mut state = AppState::new();
        state.event_store = crate::events::EventStore::with_capacity(2);
        state.event_store.emit("one", serde_json::json!({}));
        state.event_store.emit("two", serde_json::json!({}));
        state.event_store.emit("three", serde_json::json!({}));
        assert!(state.event_store.query(Some(0), None).gap);

        let snapshot = build_health_snapshot_from_parts(&state, &DiagnosticSnapshot::default());
        assert_eq!(snapshot["state"], serde_json::json!("degraded"));
        assert_eq!(
            snapshot["events"]["cursor_gaps_total"],
            serde_json::json!(1)
        );
        let component = snapshot["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|component| component["kind"] == "events")
            .expect("event retention component");
        assert_eq!(
            component["error"],
            serde_json::json!("consumer cursor fell behind retained history")
        );
    }

    #[test]
    fn health_snapshot_degrades_for_misconfigured_history_even_when_disabled() {
        let mut state = AppState::new();
        let config = crate::history::HistoryConfig {
            config_error: Some(
                "history.max_records is 42; expected 0 (unbounded) or 1000..=10000000".to_string(),
            ),
            ..crate::history::HistoryConfig::default()
        };
        state.history = crate::history::HistoryHandle::open(config).0;

        let snapshot = build_health_snapshot_from_parts(&state, &DiagnosticSnapshot::default());
        assert_eq!(snapshot["state"], serde_json::json!("degraded"));
        let component = snapshot["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|component| component["kind"] == "history")
            .expect("history health component");
        assert_eq!(component["state"], serde_json::json!("misconfigured"));
        assert!(component["error"]
            .as_str()
            .is_some_and(|error| error.contains("history.max_records is 42")));
    }

    #[test]
    fn transcript_payload_exposes_files_touched_and_recent_files() {
        let state = TranscriptState {
            files_touched: vec!["/a/x".to_string()],
            recent_files: vec![TouchedFile {
                path: "/a/x".to_string(),
                op: FileOp::Created,
                at_unix_ms: 5,
            }],
            ..Default::default()
        };

        let payload = transcript_payload(&state);

        // The existing files_touched key is still present.
        assert_eq!(
            payload["files_touched"],
            serde_json::json!(["/a/x"]),
            "files_touched stays exposed"
        );
        // recent_files carries the wire shape { path, op, at_unix_ms }.
        assert_eq!(
            payload["recent_files"][0],
            serde_json::json!({ "path": "/a/x", "op": "write", "at_unix_ms": 5 })
        );
        // An unbound transcript reports an unknown turn rather than omitting it.
        assert_eq!(
            payload["turn"],
            serde_json::json!({ "phase": "unknown", "at_unix_ms": 0 })
        );
    }

    #[test]
    fn transcript_payload_exposes_the_native_turn_phase() {
        let state = TranscriptState {
            turn: crate::agents::PaneTurn::new(crate::agents::TurnPhase::Active, 1_700),
            ..Default::default()
        };

        assert_eq!(
            transcript_payload(&state)["turn"],
            serde_json::json!({ "phase": "active", "at_unix_ms": 1_700 })
        );
    }

    #[test]
    fn agent_entry_payload_carries_the_canonical_lifecycle_state() {
        // Web and socket clients read the same resolved state the native
        // sidebar badge renders, rather than re-deriving it from `activity`.
        let instance = crate::runtime_probe::AgentInstance {
            pane_id: 4,
            agent_name: Some("claude".into()),
            session_id: None,
            instance_label: "claude".into(),
            kind_index: 1,
        };

        let payload = agent_entry_payload(&instance, None, crate::agents::AgentLifecycle::Working);

        assert_eq!(payload["state"], serde_json::json!("working"));
        assert_eq!(payload["state_label"], serde_json::json!("WORKING"));
        assert_eq!(payload["activity"], serde_json::Value::Null);
    }

    #[test]
    fn state_snapshot_projects_one_canonical_attention_object_to_pane_agent_and_alert() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "waiting agent",
            HeadlessPaneSeed::default(),
        )
        .expect("headless tab should be seeded");
        state
            .find_tab_mut(tab_id)
            .expect("seeded tab")
            .set_pane_agent_activity(
                pane_id,
                AgentActivity::termprop(
                    AgentActivityState::WaitingInput,
                    "waiting for input",
                    Some("claude".into()),
                ),
            );

        let snapshot = super::build_state_snapshot(&state);
        let tab = snapshot["workspaces"][0]["tabs"]
            .as_array()
            .expect("tabs")
            .iter()
            .find(|tab| tab["tab_id"] == tab_id)
            .expect("waiting tab");
        let pane_attention = &tab["panes"][0]["attention"];
        let agent_attention = &tab["agents"][0]["attention"];
        let alert = snapshot["alerts"]
            .as_array()
            .expect("alerts")
            .iter()
            .find(|alert| alert["pane_id"] == pane_id)
            .expect("pane alert");

        assert_eq!(pane_attention["reason"], "waiting_input");
        assert_eq!(pane_attention["provider"], "claude");
        assert_eq!(pane_attention["provenance"], "termprop");
        assert_eq!(pane_attention["authority"], "provider_explicit");
        assert_eq!(pane_attention["freshness"], "fresh");
        assert!(pane_attention["last_verified_unix_ms"].as_u64().is_some());
        assert_eq!(agent_attention, pane_attention);
        assert_eq!(alert["attention"], *pane_attention);
        assert_eq!(
            tab["agent_activity"]["observed_at_unix_ms"],
            pane_attention["last_verified_unix_ms"]
        );
    }

    #[test]
    fn active_alert_messages_stay_scoped_to_their_exact_panes() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, first_pane) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "two requesting agents",
            HeadlessPaneSeed::default(),
        )
        .expect("headless tab should be seeded");
        let second_pane = first_pane + 1;
        let tab = state.find_tab_mut(tab_id).expect("seeded tab");
        tab.set_pane_agent_activity(
            first_pane,
            AgentActivity::socket(
                AgentActivityState::WaitingInput,
                "first pane needs input",
                Some("codex".into()),
            ),
        );
        tab.set_pane_agent_activity(
            second_pane,
            AgentActivity::socket(
                AgentActivityState::Errored,
                "second pane failed",
                Some("claude".into()),
            ),
        );
        tab.notification_msg = Some("most recent sibling message".into());

        let target = |pane_id, reason, provider: &str| crate::attention::AttentionTarget {
            workspace_id,
            workspace_name: "default".into(),
            repository: "repository".into(),
            worktree: "worktree".into(),
            machine: "machine".into(),
            tab_id,
            tab_name: "two requesting agents".into(),
            pane_id,
            evidence: crate::attention::AttentionEvidence {
                reason,
                provider: Some(provider.into()),
                provenance: "socket",
                authority: crate::attention::AttentionAuthority::ProviderExplicit,
                freshness: crate::attention::AttentionFreshness::Fresh,
                last_verified_unix_ms: Some(crate::events::unix_time_ms()),
            },
        };
        let ingredients = StateSnapshotIngredients {
            attention_targets: vec![
                target(
                    first_pane,
                    crate::attention::AttentionReason::WaitingInput,
                    "codex",
                ),
                target(
                    second_pane,
                    crate::attention::AttentionReason::Error,
                    "claude",
                ),
            ],
            ..empty_ingredients()
        };

        let alerts = build_active_alerts(&state, &ingredients);
        assert_eq!(alerts[0]["message"], "first pane needs input");
        assert_eq!(alerts[1]["message"], "second pane failed");
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "taarof-api-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("test temp dir should be created");
        root
    }

    #[test]
    fn agents_payload_disambiguates_same_kind_and_carries_instance_label() {
        use crate::agents::AgentStatus;
        use crate::runtime_probe::assign_agent_instance_labels;

        // Two codex (panes 4, 5) plus one claude (pane 6), in pane order.
        let ordered = vec![
            (
                4,
                AgentStatus {
                    agent_name: Some("codex".into()),
                    session_id: Some("c1".into()),
                    running: true,
                },
            ),
            (
                5,
                AgentStatus {
                    agent_name: Some("codex".into()),
                    session_id: Some("c2".into()),
                    running: true,
                },
            ),
            (
                6,
                AgentStatus {
                    agent_name: Some("claude".into()),
                    session_id: Some("cl".into()),
                    running: true,
                },
            ),
        ];

        let payload: Vec<serde_json::Value> = assign_agent_instance_labels(&ordered)
            .iter()
            .map(|instance| {
                agent_entry_payload(instance, None, crate::agents::AgentLifecycle::Idle)
            })
            .collect();

        // Same-kind codex agents are distinguishable by instance_label; the
        // lone claude keeps its bare name. Pane attribution is preserved.
        assert_eq!(payload[0]["pane_id"], serde_json::json!(4));
        assert_eq!(payload[0]["instance_label"], serde_json::json!("codex #1"));
        assert_eq!(payload[0]["kind_index"], serde_json::json!(1));
        assert_eq!(payload[0]["session_id"], serde_json::json!("c1"));

        assert_eq!(payload[1]["pane_id"], serde_json::json!(5));
        assert_eq!(payload[1]["instance_label"], serde_json::json!("codex #2"));
        assert_eq!(payload[1]["kind_index"], serde_json::json!(2));
        assert_eq!(payload[1]["session_id"], serde_json::json!("c2"));

        assert_eq!(payload[2]["pane_id"], serde_json::json!(6));
        assert_eq!(payload[2]["instance_label"], serde_json::json!("claude"));
        assert_eq!(payload[2]["agent_name"], serde_json::json!("claude"));

        // Each entry carries stable badge metadata so web surfaces resolve the
        // same identity model straight from the payload.
        assert_eq!(payload[0]["badge"]["short_label"], serde_json::json!("CDX"));
        assert_eq!(
            payload[0]["badge"]["color_token"],
            serde_json::json!("agent-codex")
        );
        assert_eq!(payload[0]["badge"]["known"], serde_json::json!(true));
        assert_eq!(payload[2]["badge"]["short_label"], serde_json::json!("CLD"));
        assert_eq!(
            payload[2]["badge"]["color_token"],
            serde_json::json!("agent-claude")
        );
    }

    #[test]
    fn agent_badge_payload_degrades_to_generic_for_unknown_and_missing() {
        let unknown = agent_badge_payload(Some("MysteryBot"));
        assert_eq!(unknown["known"], serde_json::json!(false));
        assert_eq!(unknown["short_label"], serde_json::json!("MYST"));
        assert_eq!(unknown["color_token"], serde_json::json!("agent-generic"));

        let missing = agent_badge_payload(None);
        assert_eq!(missing["known"], serde_json::json!(false));
        assert_eq!(missing["short_label"], serde_json::json!("AGENT"));
        assert_eq!(missing["color_token"], serde_json::json!("agent-generic"));
    }

    #[test]
    fn pane_payload_serializes_remote_child_process_as_unknown() {
        let payload = pane_payload_from_data(PanePayloadData {
            pane_id: 7,
            shell_running: true,
            has_child_process: true,
            remote_shell: true,
            cwd: Some("/tmp/project".into()),
            cwd_host: Some("devbox".into()),
            location_updated_at_unix_ms: Some(10),
            probe_updated_at_unix_ms: Some(20),
            tmux_session: Some("sess".into()),
            tmux_host: Some("host".into()),
            tmux_probe: None,
            attach_supported: true,
            attach_kind: "tmux",
            attach_unavailable_reason: None,
            pty_capability: "legacy_snapshot",
            cols: Some(120),
            rows: Some(40),
            agent_name: Some("claude".into()),
            agent_session_id: Some("sess-live".into()),
            transcript: None,
            current_task: serde_json::Value::Null,
        });

        assert_eq!(payload["pane_id"], serde_json::json!(7));
        assert!(payload["has_child_process"].is_null());
        assert_eq!(payload["remote_shell"], serde_json::json!(true));
        assert_eq!(payload["cwd"], serde_json::json!("/tmp/project"));
        assert_eq!(payload["cwd_host"], serde_json::json!("devbox"));
        assert_eq!(
            payload["location_updated_at_unix_ms"],
            serde_json::json!(10)
        );
        assert_eq!(payload["probe_updated_at_unix_ms"], serde_json::json!(20));
        assert_eq!(payload["attach_supported"], serde_json::json!(true));
        assert_eq!(payload["attach_kind"], serde_json::json!("tmux"));
        assert_eq!(payload["cols"], serde_json::json!(120));
        assert_eq!(payload["rows"], serde_json::json!(40));
    }

    #[test]
    fn pane_payload_handles_missing_cached_metadata() {
        let payload = pane_payload_from_data(PanePayloadData {
            pane_id: 9,
            shell_running: false,
            has_child_process: false,
            remote_shell: false,
            cwd: None,
            cwd_host: None,
            location_updated_at_unix_ms: None,
            probe_updated_at_unix_ms: None,
            tmux_session: None,
            tmux_host: None,
            tmux_probe: None,
            attach_supported: false,
            attach_kind: "unsupported",
            attach_unavailable_reason: Some("concrete unavailable reason".into()),
            pty_capability: "legacy_snapshot",
            cols: None,
            rows: None,
            agent_name: None,
            agent_session_id: None,
            transcript: None,
            current_task: serde_json::Value::Null,
        });

        assert_eq!(payload["pane_id"], serde_json::json!(9));
        assert_eq!(payload["shell_running"], serde_json::json!(false));
        assert_eq!(payload["has_child_process"], serde_json::json!(false));
        assert_eq!(payload["remote_shell"], serde_json::json!(false));
        assert!(payload["cwd"].is_null());
        assert!(payload["cwd_host"].is_null());
        assert!(payload["location_updated_at_unix_ms"].is_null());
        assert!(payload["probe_updated_at_unix_ms"].is_null());
        assert_eq!(payload["attach_supported"], serde_json::json!(false));
        assert_eq!(payload["attach_kind"], serde_json::json!("unsupported"));
        assert_eq!(
            payload["attach_unavailable_reason"],
            serde_json::json!("concrete unavailable reason")
        );
        assert!(payload["cols"].is_null());
        assert!(payload["rows"].is_null());
    }

    #[test]
    fn pane_payload_preserves_stale_cached_timestamps() {
        let payload = pane_payload_from_data(PanePayloadData {
            pane_id: 11,
            shell_running: true,
            has_child_process: false,
            remote_shell: false,
            cwd: Some("/stale/project".into()),
            cwd_host: None,
            location_updated_at_unix_ms: Some(1),
            probe_updated_at_unix_ms: Some(2),
            tmux_session: None,
            tmux_host: None,
            tmux_probe: None,
            attach_supported: false,
            attach_kind: "unsupported",
            attach_unavailable_reason: None,
            pty_capability: "legacy_snapshot",
            cols: Some(80),
            rows: Some(24),
            agent_name: None,
            agent_session_id: None,
            transcript: None,
            current_task: serde_json::Value::Null,
        });

        assert_eq!(payload["cwd"], serde_json::json!("/stale/project"));
        assert_eq!(payload["location_updated_at_unix_ms"], serde_json::json!(1));
        assert_eq!(payload["probe_updated_at_unix_ms"], serde_json::json!(2));
        assert_eq!(payload["cols"], serde_json::json!(80));
        assert_eq!(payload["rows"], serde_json::json!(24));
    }

    #[test]
    fn pane_attach_metadata_classifies_all_kinds() {
        assert_eq!(
            pane_attach_metadata(PaneAttachWireKind::Tmux),
            (true, "tmux")
        );
        assert_eq!(pane_attach_metadata(PaneAttachWireKind::Vte), (true, "vte"));
        assert_eq!(
            pane_attach_metadata(PaneAttachWireKind::Unsupported),
            (false, "unsupported")
        );
    }

    #[test]
    fn failed_exact_restore_is_not_projected_as_a_live_tmux_viewer_target() {
        assert_eq!(
            super::live_pane_attach_metadata(true, true),
            (false, "unsupported")
        );
        assert_eq!(
            super::live_pane_attach_metadata(true, false),
            (true, "tmux")
        );
    }

    #[test]
    fn build_state_snapshot_treats_remote_tmux_process_state_as_unknown_when_probe_misses_ssh() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "tmux tab",
            HeadlessPaneSeed {
                cwd: Some("/tmp/project".into()),
                cwd_host: Some("devbox".into()),
                shell_running: true,
                has_child_process: true,
                // The process probe is best-effort and may miss the SSH
                // client. The authoritative remote tmux target must still
                // make local child-process state unknown on the wire.
                remote_shell: false,
                ssh_command: None,
                tmux_session: Some("sess".into()),
                tmux_ssh_target: Some("devbox".into()),
            },
        )
        .expect("headless tab should be seeded");

        let backing = state
            .headless_pane_mut(tab_id, pane_id)
            .expect("headless pane should exist")
            .tmux_backing
            .as_mut()
            .expect("headless pane should be tmux-backed");
        backing.pane_info.record_success(TmuxPaneInfo {
            current_command: "vim".into(),
            cwd: "/tmp/project".into(),
            pid: 42,
            width: 132,
            height: 41,
            session_id: "$1".into(),
            session_created: 1,
            continuity_id: Some("11".repeat(16)),
        });

        let snapshot = super::build_state_snapshot(&state);
        let pane = &snapshot["workspaces"][0]["tabs"][0]["panes"][0];

        assert_eq!(pane["pane_id"], serde_json::json!(pane_id));
        assert_eq!(pane["attach_supported"], serde_json::json!(true));
        assert_eq!(pane["attach_kind"], serde_json::json!("tmux"));
        assert_eq!(pane["cols"], serde_json::json!(132));
        assert_eq!(pane["rows"], serde_json::json!(41));
        assert_eq!(pane["remote_shell"], serde_json::json!(false));
        assert_eq!(pane["tmux_host"], serde_json::json!("devbox"));
        assert!(pane["has_child_process"].is_null());

        let panes = super::build_state_projection(&state, super::StateProjection::Panes);
        let projected = panes
            .as_array()
            .unwrap()
            .iter()
            .find(|pane| pane["pane_id"] == serde_json::json!(pane_id))
            .expect("remote pane should be present in /api/v1/panes projection");
        assert_eq!(projected["remote_shell"], serde_json::json!(false));
        assert_eq!(projected["tmux_host"], serde_json::json!("devbox"));
        assert!(projected["has_child_process"].is_null());
    }

    #[test]
    fn build_state_snapshot_exposes_resolved_and_unresolved_pane_task_binding() {
        let root = unique_test_dir("pane-task-query-state");
        fs::create_dir_all(root.join(".git")).expect(".git dir should be created");
        fs::create_dir_all(root.join(".plan")).expect(".plan dir should be created");
        fs::write(
            root.join(".plan/tasks.json"),
            r#"{"tasks":[{"id":"EXAMPLE-110","title":"Bind panes","status":"todo"}]}"#,
        )
        .expect("tasks.json should be written");

        let mut state = AppState::new();
        state.workspaces[0].repo_root = Some("/unrelated/checkout".into());
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "runner",
            HeadlessPaneSeed {
                cwd: Some(root.to_string_lossy().into_owned()),
                cwd_host: None,
                shell_running: true,
                has_child_process: true,
                remote_shell: false,
                ssh_command: None,
                tmux_session: None,
                tmux_ssh_target: None,
            },
        )
        .expect("headless tab should be seeded");
        state
            .bind_pane_to_task(tab_id, pane_id, "EXAMPLE-110")
            .expect("task should bind");
        state.observe_work_probe(crate::work_ledger::enrich_plan_probe(
            crate::work_ledger::collect_probe(&state),
        ));

        let snapshot = super::build_state_snapshot(&state);
        let pane = &snapshot["workspaces"][0]["tabs"][0]["panes"][0];
        assert_eq!(pane["current_task"]["id"], serde_json::json!("EXAMPLE-110"));
        assert_eq!(
            pane["current_task"]["title"],
            serde_json::json!("Bind panes")
        );
        assert_eq!(pane["current_task"]["resolved"], serde_json::json!(true));
        assert_eq!(snapshot["work"]["schema"], "taarof.work-stream.v1");
        assert_eq!(snapshot["work"]["legend"][0]["marker"], "P1");
        assert_eq!(snapshot["work"]["legend"][0]["task_id"], "EXAMPLE-110");
        let truth = &snapshot["work"]["truth"][0];
        assert_eq!(truth["canonical"], "todo");
        assert_eq!(truth["binding"], "bound");
        assert_eq!(
            truth["execution"], "unknown",
            "a headless pane without a runtime observation must not look known-idle"
        );
        assert_eq!(truth["origin"], "live");
        assert_eq!(truth["verification"], "verified");
        assert!(!truth["verification_source"].as_str().unwrap().is_empty());
        assert!(truth.get("last_checked_unix_ms").is_some());
        assert_eq!(truth["counts_as_live_open"], true);
        assert_eq!(snapshot["work"]["counts"]["live_open"], 1);
        assert_eq!(snapshot["work"]["counts"]["historical"], 0);
        assert_eq!(snapshot["work"]["counts"]["mismatch"], 0);
        let entry_truth = &snapshot["work"]["entries"][0]["truth"];
        assert_eq!(
            snapshot["work"]["all_entries"][0]["truth"],
            entry_truth.clone(),
            "the complete web chronology must expose the same per-record truth"
        );
        for field in [
            "canonical",
            "binding",
            "execution",
            "origin",
            "verification",
            "verification_source",
            "last_checked_unix_ms",
            "counts_as_live_open",
        ] {
            assert!(
                entry_truth.get(field).is_some(),
                "missing truth field {field}"
            );
        }
        assert_eq!(snapshot["work_ledger"]["schema"], "taarof.work-ledger.v3");
        let flat = super::build_state_projection(&state, super::StateProjection::Panes);
        assert_eq!(pane["current_task"], flat[0]["current_task"]);

        fs::write(root.join(".plan/tasks.json"), r#"{"tasks":[]}"#)
            .expect("tasks.json should be replaceable");
        let snapshot = super::build_state_snapshot(&state);
        let pane = &snapshot["workspaces"][0]["tabs"][0]["panes"][0];
        assert_eq!(pane["current_task"]["id"], serde_json::json!("EXAMPLE-110"));
        assert_eq!(pane["current_task"]["resolved"], serde_json::json!(false));
        let flat = super::build_state_projection(&state, super::StateProjection::Panes);
        assert_eq!(pane["current_task"], flat[0]["current_task"]);
    }

    #[test]
    fn pending_lazy_restore_exposes_stable_bound_panes_in_nested_and_flat_state() {
        let root = unique_test_dir("pending-pane-task-query-state");
        fs::create_dir_all(root.join(".git")).expect(".git dir should be created");
        fs::create_dir_all(root.join(".plan")).expect(".plan dir should be created");
        fs::write(
            root.join(".plan/tasks.json"),
            r#"{"tasks":[{"id":"EXAMPLE-110","title":"Pending task","status":"todo"}]}"#,
        )
        .expect("tasks.json should be written");
        let binding =
            crate::task_binding::resolve_task_from_pane_cwd(&root.to_string_lossy(), "EXAMPLE-110")
                .expect("saved binding should resolve");
        let saved = crate::session::SavedPaneNode::Split {
            direction: "vertical".into(),
            ratio: 0.5,
            first: Box::new(crate::session::SavedPaneNode::Leaf {
                work_origin: None,
                cwd: Some(root.to_string_lossy().into_owned()),
                ssh_command: None,
                tmux_session: None,
                tmux_host: None,
                tmux_identity: None,
                current_task: Some(binding),
                agent_session: None,
            }),
            second: Box::new(crate::session::SavedPaneNode::Leaf {
                work_origin: None,
                cwd: Some(root.to_string_lossy().into_owned()),
                ssh_command: None,
                tmux_session: Some("pending-tmux".into()),
                tmux_host: None,
                tmux_identity: None,
                current_task: None,
                agent_session: None,
            }),
        };

        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let tab_id = crate::seed_pending_restore_tab(
            &mut state,
            workspace_id,
            "lazy",
            saved,
            Some(root.to_string_lossy().into_owned()),
        )
        .expect("pending tab should seed");

        let snapshot = super::build_state_snapshot(&state);
        let tab = snapshot["workspaces"][0]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tab| tab["tab_id"] == serde_json::json!(tab_id))
            .expect("pending tab should be projected");
        assert_eq!(tab["panes"][0]["pane_id"], serde_json::json!(0));
        assert_eq!(tab["panes"][1]["pane_id"], serde_json::json!(1));
        assert_eq!(
            tab["panes"][1]["tmux_session"],
            serde_json::json!("pending-tmux")
        );
        assert_eq!(
            tab["panes"][1]["attach_supported"],
            serde_json::json!(false)
        );
        assert_eq!(
            tab["panes"][1]["attach_kind"],
            serde_json::json!("unsupported")
        );
        assert_eq!(
            tab["panes"][1]["attach_unavailable_reason"],
            serde_json::json!("Browser viewer reconnect unavailable: native Reattach live terminal lacks the exact saved tmux generation.")
        );
        assert_eq!(
            tab["panes"][0]["current_task"]["id"],
            serde_json::json!("EXAMPLE-110")
        );
        assert_eq!(
            tab["panes"][0]["current_task"]["resolved"],
            serde_json::json!(true)
        );

        let flat = super::build_state_projection(&state, super::StateProjection::Panes);
        let pending: Vec<_> = flat
            .as_array()
            .unwrap()
            .iter()
            .filter(|pane| pane["tab_id"] == serde_json::json!(tab_id))
            .collect();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0]["pane_id"], serde_json::json!(0));
        assert_eq!(pending[1]["pane_id"], serde_json::json!(1));
        assert_eq!(pending[1]["attach_supported"], serde_json::json!(false));
        assert_eq!(pending[1]["attach_kind"], serde_json::json!("unsupported"));
        assert_eq!(pending[0]["current_task"], tab["panes"][0]["current_task"]);
    }

    #[test]
    fn move_tab_workspace_state_appears_in_query_state() {
        let mut state = AppState::new();
        let ws_a = state.active_workspace;
        let ws_b = state.create_workspace("target", None);

        let (moved_tab, moved_pane) = crate::seed_headless_terminal_tab(
            &mut state,
            ws_a,
            "runner",
            HeadlessPaneSeed {
                cwd: Some("/tmp/user/project".into()),
                cwd_host: None,
                shell_running: true,
                has_child_process: true,
                remote_shell: false,
                ssh_command: None,
                tmux_session: Some("kmux60".into()),
                tmux_ssh_target: None,
            },
        )
        .expect("headless tab should seed");

        // Decorate the tab so we can prove agent metadata relocates with it.
        state.workspaces[0]
            .tabs
            .iter_mut()
            .find(|t| t.id == moved_tab)
            .expect("moved tab should exist")
            .set_pane_agent_activity(
                moved_pane,
                AgentActivity::socket(
                    AgentActivityState::Running,
                    "moving workspace",
                    Some("claude".into()),
                ),
            );

        state
            .move_tab_to_workspace(moved_tab, ws_b)
            .expect("move should succeed");

        let snapshot = super::build_state_snapshot(&state);

        // Active workspace/tab followed the move.
        assert_eq!(snapshot["active_workspace"], serde_json::json!(ws_b));
        assert_eq!(snapshot["active_tab"], serde_json::json!(moved_tab));

        let workspaces = snapshot["workspaces"]
            .as_array()
            .expect("workspaces should be an array");
        let source = workspaces
            .iter()
            .find(|ws| ws["id"] == serde_json::json!(ws_a))
            .expect("source workspace should be present");
        let target = workspaces
            .iter()
            .find(|ws| ws["id"] == serde_json::json!(ws_b))
            .expect("target workspace should be present");

        // The moved tab is under the target workspace and gone from the source.
        assert!(source["tabs"]
            .as_array()
            .expect("source tabs array")
            .iter()
            .all(|t| t["tab_id"] != serde_json::json!(moved_tab)));
        let moved = target["tabs"]
            .as_array()
            .expect("target tabs array")
            .iter()
            .find(|t| t["tab_id"] == serde_json::json!(moved_tab))
            .expect("moved tab should be under target workspace");

        // Agent state, cwd, and tmux target survived the relocation intact.
        assert_eq!(moved["agent_name"], serde_json::json!("claude"));
        assert_eq!(moved["agent_pane_id"], serde_json::json!(moved_pane));
        assert_eq!(moved["agents"][0]["pane_id"], serde_json::json!(moved_pane));
        assert_eq!(target["active_tab"], serde_json::json!(moved_tab));
        let pane = &moved["panes"][0];
        assert_eq!(pane["cwd"], serde_json::json!("/tmp/user/project"));
        assert_eq!(pane["tmux_session"], serde_json::json!("kmux60"));
    }

    #[test]
    fn build_state_snapshot_includes_workspace_tool_version_chip() {
        let _guard = task_discovery_test_guard();
        clear_tool_version_cache_for_test();
        let root = unique_test_dir("workspace-tool-version-chip");
        fs::write(root.join(".mise.toml"), "[tools]\nnode = \"22.1.0\"\n")
            .expect("test mise config should be written");
        install_tool_version_test_probe(Some(vec![
            tool_version_test_entry("node", "22.1.0", &root),
            tool_version_test_entry("rust", "1.81.0", &root),
        ]));

        let mut state = AppState::new();
        state
            .active_ws_mut()
            .expect("default workspace should exist")
            .repo_root = Some(root.to_string_lossy().into_owned());

        // Tool-version discovery is now non-blocking on the snapshot caller;
        // warm the cache so the snapshot sees the discovered chip.
        wait_for_tool_version_chip(&DiscoveryTarget::Local {
            cwd: root.to_string_lossy().into_owned(),
            binary_path: None,
        });

        let snapshot = super::build_state_snapshot(&state);
        let workspace = &snapshot["workspaces"][0];

        assert_eq!(
            workspace["tool_version_chip"],
            serde_json::json!("node 22.1 • rust 1.81")
        );

        clear_tool_version_test_probe();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn running_activity_drives_tabs_jobs_and_alert_payloads_even_with_stale_cached_flag() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "alerting runner",
            HeadlessPaneSeed {
                cwd: None,
                cwd_host: None,
                shell_running: true,
                has_child_process: true,
                remote_shell: false,
                ssh_command: None,
                tmux_session: None,
                tmux_ssh_target: None,
            },
        )
        .expect("headless tab should be seeded");

        let tab = state.find_tab_mut(tab_id).expect("seeded tab should exist");
        tab.agent_running = false;
        tab.agent_name = Some("codex".to_string());
        tab.agent_pane_id = Some(pane_id);
        tab.set_pane_agent_activity(
            pane_id,
            AgentActivity::socket(
                AgentActivityState::Running,
                "running cargo test",
                Some("codex".to_string()),
            ),
        );
        tab.needs_attention = true;
        tab.notification_msg = Some("review needed".to_string());

        let snapshot = super::build_state_snapshot(&state);
        let tab_payload = snapshot["workspaces"][0]["tabs"]
            .as_array()
            .expect("tabs should be an array")
            .iter()
            .find(|tab| tab["tab_id"] == serde_json::json!(tab_id))
            .expect("seeded tab payload should exist");
        assert_eq!(tab_payload["agent_running"], serde_json::json!(true));

        let agent_job = snapshot["agent_jobs"]
            .as_array()
            .expect("agent_jobs should be an array")
            .iter()
            .find(|job| job["tab_id"] == serde_json::json!(tab_id))
            .expect("fresh running tab should be included in agent_jobs");
        assert_eq!(agent_job["tab_name"], serde_json::json!("alerting runner"));

        let alert = snapshot["alerts"]
            .as_array()
            .expect("alerts should be an array")
            .iter()
            .find(|alert| alert["tab_id"] == serde_json::json!(tab_id))
            .expect("alerting tab should be included in active alerts");
        assert_eq!(alert["agent_running"], serde_json::json!(true));
    }

    #[test]
    fn multi_agent_targeted_activity_stays_consistent_across_tab_agents_and_jobs() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, first_pane) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "three agents",
            HeadlessPaneSeed::default(),
        )
        .expect("headless tab should be seeded");
        let second_pane = first_pane + 1;
        let third_pane = first_pane + 2;
        let tab = state.find_tab_mut(tab_id).expect("seeded tab should exist");
        for (pane_id, source, text) in [
            (first_pane, "codex", "editing api.rs"),
            (second_pane, "codex", "running tests"),
            (third_pane, "claude", "reviewing changes"),
        ] {
            tab.set_pane_agent_activity(
                pane_id,
                AgentActivity::socket(AgentActivityState::Running, text, Some(source.to_string())),
            );
        }

        let snapshot = super::build_state_snapshot(&state);
        let tab_payload = snapshot["workspaces"][0]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tab| tab["tab_id"] == tab_id)
            .unwrap();
        let agents = tab_payload["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 3);
        assert_eq!(agents[0]["instance_label"], "codex #1");
        assert_eq!(agents[1]["instance_label"], "codex #2");
        assert_eq!(agents[2]["instance_label"], "claude");
        assert_eq!(snapshot["agent_jobs"].as_array().unwrap().len(), 3);
        assert_eq!(tab_payload["agent_running"], true);

        state
            .find_tab_mut(tab_id)
            .unwrap()
            .set_pane_agent_activity(second_pane, None);
        let snapshot = super::build_state_snapshot(&state);
        let tab_payload = snapshot["workspaces"][0]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tab| tab["tab_id"] == tab_id)
            .unwrap();
        assert_eq!(tab_payload["agents"].as_array().unwrap().len(), 2);
        assert_eq!(snapshot["agent_jobs"].as_array().unwrap().len(), 2);
        assert_eq!(tab_payload["agent_running"], true);

        let tab = state.find_tab_mut(tab_id).unwrap();
        tab.pane_agent_activity
            .get_mut(&first_pane)
            .unwrap()
            .updated_at = std::time::Instant::now() - std::time::Duration::from_secs(60);
        tab.set_pane_agent_activity(
            third_pane,
            AgentActivity::socket(
                AgentActivityState::Done,
                "review complete",
                Some("claude".to_string()),
            ),
        );
        let snapshot = super::build_state_snapshot(&state);
        let tab_payload = snapshot["workspaces"][0]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tab| tab["tab_id"] == tab_id)
            .unwrap();
        assert_eq!(snapshot["agent_jobs"].as_array().unwrap().len(), 0);
        assert_eq!(tab_payload["agent_running"], false);
        assert_eq!(tab_payload["agent_pane_id"], third_pane);
        assert_eq!(tab_payload["agent_activity"]["state"], "done");
        assert_eq!(tab_payload["agent_activity"]["source"], "claude");
    }

    #[test]
    fn tab_agent_identity_falls_back_to_detected_process_without_activity() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "detected agent",
            HeadlessPaneSeed::default(),
        )
        .unwrap();
        let tab = state.find_tab_mut(tab_id).unwrap();
        tab.agent_name = Some("codex".to_string());
        tab.agent_session_id = Some("session-7".to_string());
        tab.agent_pane_id = Some(pane_id);

        let snapshot = super::build_state_snapshot(&state);
        let tab_payload = snapshot["workspaces"][0]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tab| tab["tab_id"] == tab_id)
            .unwrap();
        assert_eq!(tab_payload["agent_name"], "codex");
        assert_eq!(tab_payload["agent_session_id"], "session-7");
        assert_eq!(tab_payload["agent_pane_id"], pane_id);
        assert!(tab_payload["agent_activity"].is_null());
    }

    #[test]
    fn pane_process_state_snapshot_prefers_live_scan_results() {
        let cached = PaneProcessState {
            has_child_process: false,
            remote_shell: false,
            ssh_command: None,
            updated_at_unix_ms: Some(10),
        };
        let live = PaneProcessState {
            has_child_process: true,
            remote_shell: true,
            ssh_command: Some(vec!["ssh".into(), "devbox".into()]),
            updated_at_unix_ms: Some(20),
        };
        let mut live_pane_process_states = HashMap::new();
        live_pane_process_states.insert((7, 3), live.clone());

        let resolved =
            pane_process_state_for_snapshot(7, 3, Some(1234), &cached, &live_pane_process_states);

        assert_eq!(resolved.has_child_process, live.has_child_process);
        assert_eq!(resolved.remote_shell, live.remote_shell);
        assert_eq!(resolved.ssh_command, live.ssh_command);
        assert_eq!(resolved.updated_at_unix_ms, live.updated_at_unix_ms);
    }

    #[test]
    fn pane_process_state_snapshot_falls_back_to_cached_state_without_shell_pid() {
        let cached = PaneProcessState {
            has_child_process: false,
            remote_shell: true,
            ssh_command: Some(vec!["ssh".into(), "cached-host".into()]),
            updated_at_unix_ms: Some(30),
        };

        let resolved = pane_process_state_for_snapshot(7, 3, None, &cached, &HashMap::new());

        assert_eq!(resolved.has_child_process, cached.has_child_process);
        assert_eq!(resolved.remote_shell, cached.remote_shell);
        assert_eq!(resolved.ssh_command, cached.ssh_command);
        assert_eq!(resolved.updated_at_unix_ms, cached.updated_at_unix_ms);
    }

    #[test]
    fn host_status_payload_includes_probe_state_and_error() {
        let mut status = ProbeSnapshot::default();
        status.record_success(HostStatus {
            session_count: 2,
            cpu_load_percent: 33.0,
            memory_used_percent: 44.0,
        });
        status.record_failure("timeout");

        let payload = host_status_payload(&status);

        assert_eq!(payload["state"], serde_json::json!("stale"));
        assert_eq!(payload["error"], serde_json::json!("timeout"));
        assert_eq!(payload["session_count"], serde_json::json!(2));
        assert_eq!(payload["cpu_load_percent"], serde_json::json!(33.0));
        assert_eq!(payload["memory_used_percent"], serde_json::json!(44.0));
    }

    #[test]
    fn tmux_probe_payload_includes_probe_metadata() {
        let mut probe = ProbeSnapshot::default();
        probe.record_success(TmuxPaneInfo {
            current_command: "vim".into(),
            cwd: "/tmp/project".into(),
            pid: 42,
            width: 120,
            height: 40,
            session_id: "$1".into(),
            session_created: 1,
            continuity_id: Some("11".repeat(16)),
        });
        probe.record_failure("probe failed");

        let payload = tmux_probe_payload(&probe);

        assert_eq!(
            payload["state"],
            serde_json::json!(ProbeState::Stale.label())
        );
        assert_eq!(payload["error"], serde_json::json!("probe failed"));
        assert_eq!(payload["current_command"], serde_json::json!("vim"));
        assert_eq!(payload["cwd"], serde_json::json!("/tmp/project"));
        assert_eq!(payload["pid"], serde_json::json!(42));
        assert_eq!(payload["width"], serde_json::json!(120));
        assert_eq!(payload["height"], serde_json::json!(40));
    }

    #[test]
    fn tab_payload_includes_ports_cache_freshness() {
        let payload = tab_payload(
            &AppState::new(),
            &Tab {
                id: 5,
                name: "tab".into(),
                work_origin: crate::workspace::new_tab_work_origin(),
                kind: TabKind::Terminal,
                panes: Box::new(PaneNode::Empty),
                focused_pane_id: 0,
                next_pane_id: 0,
                pane_zoom: None,
                close_on_exit: true,
                respawn_on_exit: None,
                agent_running: false,
                agent_name: None,
                agent_session_id: None,
                agent_pane_id: None,
                listening_ports: vec![3000],
                listening_ports_updated_at_unix_ms: Some(99),
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
            },
            &empty_ingredients(),
        );

        assert_eq!(payload["listening_ports"], serde_json::json!([3000]));
        assert_eq!(payload["ports_updated_at_unix_ms"], serde_json::json!(99));
        assert_eq!(payload["agents"], serde_json::json!([]));
    }

    #[test]
    fn state_and_health_expose_cached_update_without_degradation() {
        use crate::update_watch::{
            set_cached_status_for_test, BinaryIdentity, UpdateReason, UpdateState, UpdateStatus,
        };

        let identity = |path: &str, hash: &str| BinaryIdentity {
            path: Some(path.to_string()),
            resolved_path: Some(path.to_string()),
            symlink: false,
            deleted: false,
            size_bytes: Some(4),
            sha256: Some(hash.to_string()),
            build_id: None,
            readable: true,
            error: None,
        };
        set_cached_status_for_test(UpdateStatus {
            schema: "taarof.update.v1",
            state: UpdateState::UpdatePending,
            reason: Some(UpdateReason::ContentMismatch),
            running: identity("/proc/self/exe", &"a".repeat(64)),
            installed: identity("/usr/local/bin/taarof-app", &"b".repeat(64)),
            content_matches: Some(false),
            checked_at_unix_ms: Some(123),
            restart_required: true,
            restart_policy: "operator",
        });

        let state = AppState::new();
        let snapshot = super::build_state_snapshot(&state);
        assert_eq!(snapshot["capabilities"]["update_watch"], true);
        assert_eq!(snapshot["capabilities"]["runtime_identity"], true);
        assert_eq!(snapshot["update"]["schema"], "taarof.update.v1");
        assert_eq!(snapshot["update"]["state"], "update_pending");
        assert_eq!(snapshot["health"]["state"], "ok");
        assert_eq!(snapshot["health"]["degraded_components"], 0);
        assert_eq!(snapshot["health"]["update"]["state"], "update_pending");
    }

    /// EXAMPLE-164: the state snapshot, the sidebar chip and the registry all read
    /// the same cache, so a byte-identical binary with unprovable provenance
    /// must read as `current` on the binary axis and `unknown` on the source
    /// axis everywhere at once.
    #[test]
    fn runtime_identity_surfaces_share_one_cached_verdict() {
        use crate::runtime_identity::{RuntimeIdentity, SourceState};
        use crate::update_watch::{BinaryIdentity, UpdateState};

        let hash = "a".repeat(64);
        let binary = |path: &str| BinaryIdentity {
            path: Some(path.to_string()),
            resolved_path: Some(path.to_string()),
            symlink: false,
            deleted: false,
            size_bytes: Some(4),
            sha256: Some(hash.clone()),
            build_id: None,
            readable: true,
            error: None,
        };
        let mut identity = RuntimeIdentity::not_checked();
        identity.running = binary("/proc/self/exe");
        identity.installed = binary("/tmp/user/.local/bin/taarof-app");
        identity.running_matches_installed = Some(true);
        identity.binary_state = UpdateState::Current;
        identity.build.source_revision = None;
        identity.source_state = SourceState::Unknown;
        crate::runtime_identity::store(identity.clone());

        let state = AppState::new();
        let snapshot = super::build_state_snapshot(&state);
        let published = &snapshot["identity"];

        assert_eq!(
            published["schema"],
            crate::runtime_identity::IDENTITY_SCHEMA
        );
        assert_eq!(published["binary_state"], "current");
        assert_eq!(published["running_matches_installed"], true);
        // The whole point: identical binaries, unprovable source.
        assert_eq!(published["source_state"], "unknown");
        assert_eq!(
            published,
            &serde_json::to_value(&identity).expect("identity should serialize")
        );

        // The visible chip says the same thing the payload does.
        let label = crate::sidebar::version_label_text(&identity);
        assert!(label.contains("source unknown"), "chip said: {label}");
        let tooltip = crate::sidebar::version_tooltip_text(&identity);
        assert!(tooltip.contains("Source match: unknown"), "{tooltip}");
        assert!(
            tooltip.contains("Running is installed binary: yes"),
            "{tooltip}"
        );

        crate::runtime_identity::store(RuntimeIdentity::not_checked());
    }
}
