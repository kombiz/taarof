//! Shared per-pane agent projection for the left rail and right-hand Agents dock.
//!
//! Pane identity comes only from the tab's real pane tree. Probe/activity maps
//! enrich those panes but never create synthetic, unfocusable rows.

use crate::agents::AgentLifecycle;
use crate::AppState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentPaneProjection {
    pub(crate) workspace_name: String,
    pub(crate) tab_id: u32,
    pub(crate) pane_id: u32,
    pub(crate) title: String,
    pub(crate) agent_label: String,
    pub(crate) badge: crate::agents::AgentBadge,
    pub(crate) context: String,
    pub(crate) activity: String,
    pub(crate) state: AgentLifecycle,
    pub(crate) children: Vec<crate::agents::AgentInstance>,
}

fn activity_for_agent_pane(
    tab: &crate::workspace::Tab,
    pane_id: u32,
) -> Option<&crate::workspace::AgentActivity> {
    tab.pane_agent_activity(pane_id)
}

fn project_tab_agent_panes_for_ids(
    state: &AppState,
    workspace: &crate::workspace::Workspace,
    tab: &crate::workspace::Tab,
    pane_ids: Vec<u32>,
) -> Vec<AgentPaneProjection> {
    let now_unix_ms = crate::events::unix_time_ms();
    let ordered: Vec<(u32, crate::agents::AgentStatus)> = pane_ids
        .into_iter()
        .filter_map(|pane_id| {
            let activity = activity_for_agent_pane(tab, pane_id);
            let probe = state
                .runtime_probe
                .as_ref()
                .and_then(|snapshot| snapshot.pane_agents.get(&(tab.id, pane_id)))
                .filter(|status| status.running);
            if probe.is_none() && activity.is_none() {
                return None;
            }
            Some((
                pane_id,
                probe
                    .cloned()
                    .unwrap_or_else(|| crate::agents::AgentStatus {
                        agent_name: activity.and_then(|value| value.source.clone()),
                        session_id: None,
                        running: true,
                    }),
            ))
        })
        .collect();

    crate::runtime_probe::assign_agent_instance_labels(&ordered)
        .into_iter()
        .map(|instance| {
            let activity = activity_for_agent_pane(tab, instance.pane_id);
            // One canonical state machine: a detected-but-quiet agent process
            // resolves to IDLE, an open native turn to WORKING, and an
            // explicit socket/termprop signal outranks both.
            let projection_state = tab.pane_lifecycle_at(instance.pane_id, now_unix_ms);
            let agent_name = instance
                .agent_name
                .as_deref()
                .or_else(|| activity.and_then(|value| value.source.as_deref()))
                .unwrap_or("agent");
            let context = state
                .pane_task_binding(tab.id, instance.pane_id)
                .map(|binding| format!("Bound task · {} · {}", binding.task_id, binding.title))
                .or_else(|| workspace.branch_name.clone())
                .unwrap_or_else(|| workspace.name.clone());
            let parent_id = format!(
                "{}:{}",
                crate::agents::agent_badge(agent_name).name,
                state
                    .pane_transcripts
                    .get(&(tab.id, instance.pane_id))
                    .map(|transcript| transcript.session_id.as_str())
                    .unwrap_or("unknown")
            );
            let children = state
                .pane_transcripts
                .get(&(tab.id, instance.pane_id))
                .into_iter()
                .flat_map(|transcript| transcript.child_agents.iter())
                .filter(|child| child.parent_id == parent_id)
                .filter(|child| {
                    !matches!(child.state, AgentLifecycle::Done | AgentLifecycle::Errored)
                        || now_unix_ms.saturating_sub(child.updated_at_unix_ms) <= 5_000
                })
                .map(|child| crate::agents::AgentInstance {
                    stable_id: child.stable_id.clone(),
                    parent_id: Some(child.parent_id.clone()),
                    provider: child.provider.clone(),
                    label: child.label.clone(),
                    state: child.state,
                    activity: child.activity.clone(),
                    tab_id: tab.id,
                    pane_id: instance.pane_id,
                    headless: true,
                })
                .collect();
            AgentPaneProjection {
                workspace_name: workspace.name.clone(),
                tab_id: tab.id,
                pane_id: instance.pane_id,
                title: tab.name.clone(),
                agent_label: instance.instance_label,
                badge: crate::agents::agent_badge(agent_name),
                context,
                // Process presence is already communicated by the row itself.
                // Keep the detail line for meaningful activity only.
                activity: activity.map_or_else(String::new, |value| value.text.clone()),
                state: projection_state,
                children,
            }
        })
        .collect()
}

fn authoritative_pane_ids(tab: &crate::workspace::Tab) -> Vec<u32> {
    match tab.panes.as_ref() {
        crate::pane::PaneNode::Stub { pane_id } => vec![*pane_id],
        _ => tab
            .panes
            .leaves()
            .into_iter()
            .map(|leaf| leaf.pane_id)
            .collect(),
    }
}

pub(crate) fn project_tab_agent_panes(state: &AppState, tab_id: u32) -> Vec<AgentPaneProjection> {
    let Some((workspace, tab)) = state.find_tab(tab_id) else {
        return Vec::new();
    };
    project_tab_agent_panes_for_ids(state, workspace, tab, authoritative_pane_ids(tab))
}

pub(crate) fn project_all_agent_panes(state: &AppState) -> Vec<AgentPaneProjection> {
    state
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace.tabs.iter().flat_map(|tab| {
                project_tab_agent_panes_for_ids(state, workspace, tab, authoritative_pane_ids(tab))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_activity_cards_keep_same_kind_agents_separate_and_remove_closed_panes() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, first_pane) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "Agents",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("tab should seed");
        let second_pane = first_pane + 1;
        let third_pane = first_pane + 2;
        let tab = state.find_tab_mut(tab_id).expect("tab should exist");
        for pane_id in [first_pane, second_pane] {
            tab.pane_agent_activity.insert(
                pane_id,
                crate::workspace::AgentActivity::socket(
                    crate::workspace::AgentActivityState::Running,
                    format!("working in pane {pane_id}"),
                    Some("codex".into()),
                )
                .expect("running activity"),
            );
        }
        tab.pane_agent_activity.insert(
            third_pane,
            crate::workspace::AgentActivity::socket(
                crate::workspace::AgentActivityState::WaitingInput,
                "reviewing",
                Some("claude".into()),
            )
            .expect("waiting activity"),
        );

        let project = |state: &AppState, pane_ids| {
            let (workspace, tab) = state.find_tab(tab_id).expect("tab should exist");
            project_tab_agent_panes_for_ids(state, workspace, tab, pane_ids)
        };
        let panes = project(&state, vec![first_pane, second_pane, third_pane]);
        assert_eq!(
            panes
                .iter()
                .map(|pane| (pane.pane_id, pane.agent_label.as_str(), pane.state))
                .collect::<Vec<_>>(),
            vec![
                (first_pane, "codex #1", AgentLifecycle::Working),
                (second_pane, "codex #2", AgentLifecycle::Working),
                (third_pane, "claude", AgentLifecycle::WaitingInput),
            ]
        );

        let after_close = project(&state, vec![first_pane, third_pane]);
        assert_eq!(
            after_close
                .iter()
                .map(|pane| (pane.pane_id, pane.agent_label.as_str()))
                .collect::<Vec<_>>(),
            vec![(first_pane, "codex"), (third_pane, "claude")]
        );
    }

    #[test]
    fn stale_activity_ids_cannot_create_unfocusable_agent_rows() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "Agents",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("tab should seed");
        let tab = state.find_tab_mut(tab_id).expect("tab should exist");
        tab.pane_agent_activity.insert(
            pane_id,
            crate::workspace::AgentActivity::socket(
                crate::workspace::AgentActivityState::Running,
                "working",
                Some("codex".into()),
            )
            .expect("running activity"),
        );
        tab.pane_agent_activity.insert(
            pane_id + 99,
            crate::workspace::AgentActivity::socket(
                crate::workspace::AgentActivityState::Running,
                "stale",
                Some("claude".into()),
            )
            .expect("running activity"),
        );

        let panes = project_tab_agent_panes(&state, tab_id);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, pane_id);
    }

    /// Seed a tab with `count` panes and a running-agent probe entry for each,
    /// so projections exercise the detected-process path rather than requiring
    /// a terminal signal to exist at all.
    fn seed_agent_panes(state: &mut AppState, count: u32, agent: &str) -> (u32, Vec<u32>) {
        let workspace_id = state.active_workspace;
        let (tab_id, first_pane) = crate::seed_headless_terminal_tab(
            state,
            workspace_id,
            "Agents",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("tab should seed");
        let pane_ids: Vec<u32> = (0..count).map(|offset| first_pane + offset).collect();

        state.runtime_probe = Some(crate::runtime_probe::RuntimeProbeSnapshot {
            probed_at_unix_ms: 1,
            process_observed_at_unix_ms: Some(1),
            ports_observed_at_unix_ms: Some(1),
            tab_pids: std::collections::BTreeMap::new(),
            pane_pids: std::collections::BTreeMap::new(),
            pane_process_states: std::collections::HashMap::new(),
            pane_exact_agents: std::collections::HashMap::new(),
            pane_agents: pane_ids
                .iter()
                .map(|pane_id| {
                    (
                        (tab_id, *pane_id),
                        crate::agents::AgentStatus {
                            agent_name: Some(agent.to_string()),
                            session_id: None,
                            running: true,
                        },
                    )
                })
                .collect(),
            tab_agents: std::collections::HashMap::new(),
            tab_ports: std::collections::HashMap::new(),
            process_probe: crate::probe::ProbeState::Ok,
            ports_probe: crate::probe::ProbeState::Ok,
            process_error: None,
            ports_error: None,
        });
        (tab_id, pane_ids)
    }

    fn project_panes(state: &AppState, tab_id: u32, pane_ids: &[u32]) -> Vec<AgentPaneProjection> {
        let (workspace, tab) = state.find_tab(tab_id).expect("tab should exist");
        project_tab_agent_panes_for_ids(state, workspace, tab, pane_ids.to_vec())
    }

    #[test]
    fn a_detected_agent_process_alone_projects_as_idle_not_working() {
        let mut state = AppState::new();
        let (tab_id, panes) = seed_agent_panes(&mut state, 1, "claude");

        let cards = project_panes(&state, tab_id, &panes);

        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].state, AgentLifecycle::Idle);
        assert!(cards[0].activity.is_empty());
    }

    #[test]
    fn two_claude_panes_project_thinking_and_waiting_from_their_own_turns() {
        // Both panes have a live claude process and neither has scrolled a
        // Read/Edit/Bash line recently. Only the native turn distinguishes
        // them, and it must stay scoped to the pane that owns it.
        let mut state = AppState::new();
        let (tab_id, panes) = seed_agent_panes(&mut state, 2, "claude");
        let now = crate::events::unix_time_ms();
        let tab = state.find_tab_mut(tab_id).expect("tab should exist");
        // Thirty seconds of silence: far past the old eight-second window.
        tab.set_pane_turn(
            panes[0],
            crate::agents::PaneTurn::new(crate::agents::TurnPhase::Active, now - 30_000),
        );
        tab.set_pane_turn(
            panes[1],
            crate::agents::PaneTurn::new(crate::agents::TurnPhase::Completed, now - 30_000),
        );
        tab.set_pane_agent_activity(
            panes[1],
            crate::workspace::AgentActivity::output_scan(
                crate::workspace::AgentActivityState::WaitingInput,
                "waiting for input",
                Some("claude".into()),
            ),
        );

        let cards = project_panes(&state, tab_id, &panes);

        assert_eq!(
            cards
                .iter()
                .map(|card| (card.pane_id, card.agent_label.as_str(), card.state))
                .collect::<Vec<_>>(),
            vec![
                (panes[0], "claude #1", AgentLifecycle::Working),
                (panes[1], "claude #2", AgentLifecycle::WaitingInput),
            ]
        );
    }

    #[test]
    fn a_finished_turn_returns_a_pane_to_idle_and_the_tab_stops_reading_as_running() {
        let mut state = AppState::new();
        let (tab_id, panes) = seed_agent_panes(&mut state, 1, "codex");
        let now = crate::events::unix_time_ms();
        let tab = state.find_tab_mut(tab_id).expect("tab should exist");

        tab.set_pane_turn(
            panes[0],
            crate::agents::PaneTurn::new(crate::agents::TurnPhase::Active, now - 60_000),
        );
        assert!(tab.has_fresh_running_activity());
        assert_eq!(
            project_panes(&state, tab_id, &panes)[0].state,
            AgentLifecycle::Working
        );

        let tab = state.find_tab_mut(tab_id).expect("tab should exist");
        tab.set_pane_turn(
            panes[0],
            crate::agents::PaneTurn::new(crate::agents::TurnPhase::Completed, now - 60_000),
        );
        assert!(!tab.has_fresh_running_activity());
        assert_eq!(tab.primary_state(), crate::workspace::TabPrimaryState::Idle);
        assert_eq!(
            project_panes(&state, tab_id, &panes)[0].state,
            AgentLifecycle::Idle
        );
    }

    #[test]
    fn turn_evidence_for_a_closed_pane_never_leaks_onto_a_live_one() {
        let mut state = AppState::new();
        let (tab_id, panes) = seed_agent_panes(&mut state, 1, "pi");
        let now = crate::events::unix_time_ms();
        let tab = state.find_tab_mut(tab_id).expect("tab should exist");
        tab.set_pane_turn(
            panes[0] + 99,
            crate::agents::PaneTurn::new(crate::agents::TurnPhase::Active, now),
        );

        let cards = project_panes(&state, tab_id, &panes);

        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].state, AgentLifecycle::Idle);
    }

    #[test]
    fn headless_children_inherit_the_exact_parent_pane_without_becoming_rows() {
        let mut state = AppState::new();
        let (tab_id, panes) = seed_agent_panes(&mut state, 1, "claude");
        state.pane_transcripts.insert(
            (tab_id, panes[0]),
            crate::agents::TranscriptState {
                agent: "claude".into(),
                session_id: "parent".into(),
                child_agents: vec![crate::agents::HeadlessAgentEvidence {
                    stable_id: "claude:tool-a".into(),
                    parent_id: "claude:parent".into(),
                    provider: "claude".into(),
                    label: "researcher".into(),
                    state: AgentLifecycle::Working,
                    activity: "inspect parser".into(),
                    updated_at_unix_ms: crate::events::unix_time_ms(),
                }],
                ..Default::default()
            },
        );

        let cards = project_panes(&state, tab_id, &panes);
        assert_eq!(cards.len(), 1, "a headless child is not a pane row");
        assert_eq!(cards[0].children.len(), 1);
        let child = &cards[0].children[0];
        assert!(child.headless);
        assert_eq!((child.tab_id, child.pane_id), (tab_id, panes[0]));
        assert_eq!(child.parent_id.as_deref(), Some("claude:parent"));
    }
}
