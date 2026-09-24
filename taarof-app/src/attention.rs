use crate::agents::TurnPhase;
use crate::workspace::{AgentActivity, AgentActivityOrigin, AgentActivityState, Tab};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttentionReason {
    WaitingInput,
    Error,
    Unknown,
}

impl AttentionReason {
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::WaitingInput => "waiting_input",
            Self::Error => "error",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::WaitingInput => "Waiting for input",
            Self::Error => "Error",
            Self::Unknown => "Unknown reason",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttentionAuthority {
    ProviderExplicit,
    ProviderNative,
    TerminalHeuristic,
    System,
}

impl AttentionAuthority {
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::ProviderExplicit => "provider_explicit",
            Self::ProviderNative => "provider_native",
            Self::TerminalHeuristic => "terminal_heuristic",
            Self::System => "system",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttentionFreshness {
    Fresh,
    Stale,
    Conflicting,
    Unknown,
}

impl AttentionFreshness {
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Conflicting => "conflicting",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttentionEvidence {
    pub reason: AttentionReason,
    pub provider: Option<String>,
    pub provenance: &'static str,
    pub authority: AttentionAuthority,
    pub freshness: AttentionFreshness,
    pub last_verified_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttentionTarget {
    pub workspace_id: u32,
    pub workspace_name: String,
    pub repository: String,
    pub worktree: String,
    pub machine: String,
    pub tab_id: u32,
    pub tab_name: String,
    pub pane_id: u32,
    pub evidence: AttentionEvidence,
}

fn verified_time(observed_at_unix_ms: u64, now_unix_ms: u64) -> Option<u64> {
    (observed_at_unix_ms > 0 && observed_at_unix_ms <= now_unix_ms).then_some(observed_at_unix_ms)
}

fn signal_reason(signal: &AgentActivity) -> Option<AttentionReason> {
    match signal.state {
        AgentActivityState::WaitingInput => Some(AttentionReason::WaitingInput),
        AgentActivityState::Errored => Some(AttentionReason::Error),
        AgentActivityState::Idle | AgentActivityState::Running | AgentActivityState::Done => None,
    }
}

fn signal_authority(origin: AgentActivityOrigin) -> AttentionAuthority {
    match origin {
        AgentActivityOrigin::Socket | AgentActivityOrigin::Termprop => {
            AttentionAuthority::ProviderExplicit
        }
        AgentActivityOrigin::OutputScan => AttentionAuthority::TerminalHeuristic,
    }
}

fn signal_provenance(origin: AgentActivityOrigin) -> &'static str {
    match origin {
        AgentActivityOrigin::Socket => "socket",
        AgentActivityOrigin::Termprop => "termprop",
        AgentActivityOrigin::OutputScan => "output_scan",
    }
}

pub(crate) fn pane_attention_evidence_at(
    tab: &Tab,
    pane_id: u32,
    now_unix_ms: u64,
) -> Option<AttentionEvidence> {
    let signal = tab
        .pane_agent_activity(pane_id)
        .filter(|signal| signal_reason(signal).is_some());
    let turn = tab
        .pane_turn(pane_id)
        .filter(|turn| matches!(turn.phase, TurnPhase::Errored));

    let signal_evidence = signal.map(|signal| {
        let verified = verified_time(signal.observed_at_unix_ms, now_unix_ms);
        let freshness = if verified.is_none() {
            AttentionFreshness::Unknown
        } else if signal.is_fresh() {
            AttentionFreshness::Fresh
        } else {
            AttentionFreshness::Stale
        };
        AttentionEvidence {
            reason: if freshness == AttentionFreshness::Fresh {
                signal_reason(signal).unwrap_or(AttentionReason::Unknown)
            } else {
                AttentionReason::Unknown
            },
            provider: signal.source.clone(),
            provenance: signal_provenance(signal.origin),
            authority: signal_authority(signal.origin),
            freshness,
            last_verified_unix_ms: verified,
        }
    });

    let turn_evidence = turn.map(|turn| {
        let verified = verified_time(turn.at_unix_ms, now_unix_ms);
        let freshness = if verified.is_none() {
            AttentionFreshness::Unknown
        } else if turn.is_fresh(now_unix_ms) {
            AttentionFreshness::Fresh
        } else {
            AttentionFreshness::Stale
        };
        AttentionEvidence {
            reason: if freshness == AttentionFreshness::Fresh {
                AttentionReason::Error
            } else {
                AttentionReason::Unknown
            },
            provider: None,
            provenance: "native_transcript",
            authority: AttentionAuthority::ProviderNative,
            freshness,
            last_verified_unix_ms: verified,
        }
    });

    match (signal_evidence, turn_evidence) {
        (Some(signal), Some(turn))
            if signal.freshness == AttentionFreshness::Fresh
                && turn.freshness == AttentionFreshness::Fresh
                && signal.reason != turn.reason =>
        {
            Some(AttentionEvidence {
                reason: AttentionReason::Unknown,
                provider: signal.provider,
                provenance: "conflicting",
                authority: signal.authority,
                freshness: AttentionFreshness::Conflicting,
                last_verified_unix_ms: signal.last_verified_unix_ms.max(turn.last_verified_unix_ms),
            })
        }
        (Some(signal), Some(turn)) => {
            if signal.freshness == AttentionFreshness::Fresh {
                Some(signal)
            } else if turn.freshness == AttentionFreshness::Fresh {
                Some(turn)
            } else {
                Some(signal)
            }
        }
        (Some(signal), None) => Some(signal),
        (None, Some(turn)) => Some(turn),
        (None, None) => None,
    }
}

fn pane_machine(workspace: &crate::Workspace, tab: &Tab, pane_id: u32) -> String {
    tab.panes
        .leaf(pane_id)
        .and_then(|leaf| {
            leaf.location_state.cwd_host.clone().or_else(|| {
                leaf.tmux_backing
                    .as_ref()
                    .and_then(|backing| backing.target.ssh_target_string())
            })
        })
        .or_else(|| workspace.host_config_name.clone())
        .unwrap_or_else(|| glib::host_name().to_string())
}

fn provider_hint(state: &crate::AppState, tab: &Tab, pane_id: u32) -> Option<String> {
    state
        .runtime_probe
        .as_ref()
        .and_then(|probe| probe.pane_agents.get(&(tab.id, pane_id)))
        .and_then(|status| status.agent_name.clone())
}

pub(crate) fn attention_targets_at(
    state: &crate::AppState,
    now_unix_ms: u64,
) -> Vec<AttentionTarget> {
    let mut targets = Vec::new();
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            let mut pane_ids = tab
                .pane_agent_activity
                .keys()
                .chain(tab.pane_turn.keys())
                .copied()
                .collect::<std::collections::BTreeSet<_>>();
            pane_ids.retain(|pane_id| tab.panes.contains_pane(*pane_id));

            let target_start = targets.len();
            for pane_id in pane_ids {
                let Some(mut evidence) = pane_attention_evidence_at(tab, pane_id, now_unix_ms)
                else {
                    continue;
                };
                if evidence.provider.is_none() {
                    evidence.provider = provider_hint(state, tab, pane_id);
                }
                targets.push(AttentionTarget {
                    workspace_id: workspace.id,
                    workspace_name: workspace.name.clone(),
                    repository: workspace
                        .repo_root
                        .clone()
                        .unwrap_or_else(|| workspace.name.clone()),
                    worktree: workspace
                        .working_tree_path
                        .clone()
                        .or_else(|| workspace.repo_root.clone())
                        .unwrap_or_else(|| workspace.name.clone()),
                    machine: pane_machine(workspace, tab, pane_id),
                    tab_id: tab.id,
                    tab_name: tab.name.clone(),
                    pane_id,
                    evidence,
                });
            }

            if targets.len() == target_start && state.tab_needs_attention(tab) {
                let pane_id = tab.notification_pane_id.unwrap_or(tab.focused_pane_id);
                if tab.panes.contains_pane(pane_id) {
                    targets.push(AttentionTarget {
                        workspace_id: workspace.id,
                        workspace_name: workspace.name.clone(),
                        repository: workspace
                            .repo_root
                            .clone()
                            .unwrap_or_else(|| workspace.name.clone()),
                        worktree: workspace
                            .working_tree_path
                            .clone()
                            .or_else(|| workspace.repo_root.clone())
                            .unwrap_or_else(|| workspace.name.clone()),
                        machine: pane_machine(workspace, tab, pane_id),
                        tab_id: tab.id,
                        tab_name: tab.name.clone(),
                        pane_id,
                        evidence: AttentionEvidence {
                            reason: AttentionReason::Unknown,
                            provider: None,
                            provenance: "system_notification",
                            authority: AttentionAuthority::System,
                            freshness: AttentionFreshness::Unknown,
                            last_verified_unix_ms: None,
                        },
                    });
                }
            }
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::{
        attention_targets_at, pane_attention_evidence_at, AttentionAuthority, AttentionFreshness,
        AttentionReason,
    };
    use crate::agents::{PaneTurn, TurnPhase};
    use crate::pane::PaneNode;
    use crate::workspace::{AgentActivity, AgentActivityState, Tab, TabKind};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    const NOW: u64 = 1_800_000_000_000;

    fn tab_with(id: u32, pane_id: u32) -> Tab {
        Tab {
            id,
            name: "agents".into(),
            work_origin: crate::workspace::new_tab_work_origin(),
            kind: TabKind::Terminal,
            panes: Box::new(PaneNode::Stub { pane_id }),
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
            pane_turn: HashMap::new(),
            agent_activity: None,
            needs_attention: false,
            notified: false,
            notification_msg: None,
            notification_pane_id: None,
            pane_last_notified: HashMap::new(),
            workspace_action: None,
            discovery_cwd: None,
            discovered_actions: Vec::new(),
            task_buttons: Vec::new(),
            tracking_data: None,
        }
    }

    #[test]
    fn waiting_pane_stays_visible_beside_a_running_sibling() {
        let mut tab = tab_with(7, 41);
        tab.set_pane_agent_activity(
            41,
            AgentActivity::socket(AgentActivityState::Running, "working", Some("codex".into())),
        );
        tab.set_pane_agent_activity(
            42,
            AgentActivity::termprop(
                AgentActivityState::WaitingInput,
                "waiting for input",
                Some("claude".into()),
            ),
        );

        assert!(pane_attention_evidence_at(&tab, 41, NOW).is_none());
        let waiting = pane_attention_evidence_at(&tab, 42, NOW).expect("waiting pane evidence");
        assert_eq!(waiting.reason, AttentionReason::WaitingInput);
        assert_eq!(waiting.authority, AttentionAuthority::ProviderExplicit);
        assert_eq!(waiting.freshness, AttentionFreshness::Fresh);
        assert_eq!(waiting.provider.as_deref(), Some("claude"));
    }

    #[test]
    fn fresh_disagreeing_attention_reasons_are_conflicting_not_confident() {
        let mut tab = tab_with(7, 41);
        tab.set_pane_agent_activity(
            41,
            AgentActivity::socket(
                AgentActivityState::WaitingInput,
                "waiting for input",
                Some("codex".into()),
            ),
        );
        tab.set_pane_turn(41, PaneTurn::new(TurnPhase::Errored, NOW));

        let evidence = pane_attention_evidence_at(&tab, 41, NOW).expect("conflicting evidence");
        assert_eq!(evidence.reason, AttentionReason::Unknown);
        assert_eq!(evidence.freshness, AttentionFreshness::Conflicting);
    }

    #[test]
    fn expired_waiting_signal_is_stale_and_loses_its_confident_reason() {
        let mut tab = tab_with(7, 41);
        let mut signal = AgentActivity::output_scan(
            AgentActivityState::WaitingInput,
            "waiting for input",
            Some("claude".into()),
        )
        .expect("non-idle activity");
        signal.updated_at = Instant::now() - Duration::from_secs(9);
        signal.observed_at_unix_ms = NOW - 9_000;
        tab.set_pane_agent_activity(41, Some(signal));

        let evidence =
            pane_attention_evidence_at(&tab, 41, NOW).expect("stale evidence remains visible");
        assert_eq!(evidence.reason, AttentionReason::Unknown);
        assert_eq!(evidence.freshness, AttentionFreshness::Stale);
        assert_eq!(evidence.last_verified_unix_ms, Some(NOW - 9_000));
    }

    #[test]
    fn future_wall_clock_timestamp_is_not_reported_as_verified() {
        let mut tab = tab_with(7, 41);
        let mut signal =
            AgentActivity::socket(AgentActivityState::Errored, "errored", Some("codex".into()))
                .expect("non-idle activity");
        signal.observed_at_unix_ms = NOW + 1;
        tab.set_pane_agent_activity(41, Some(signal));

        let evidence = pane_attention_evidence_at(&tab, 41, NOW).expect("error evidence");
        assert_eq!(evidence.reason, AttentionReason::Unknown);
        assert_eq!(evidence.freshness, AttentionFreshness::Unknown);
        assert_eq!(evidence.last_verified_unix_ms, None);
    }

    #[test]
    fn target_projection_uses_workspace_tab_pane_order_and_drops_removed_panes() {
        let mut state = crate::AppState::new();
        let workspace = &mut state.workspaces[0];
        workspace.repo_root = Some("/src/taarof".into());
        workspace.working_tree_path = Some("/worktrees/TAAROF-27".into());
        workspace.host_config_name = Some("workstation".into());

        let mut first = tab_with(20, 8);
        first.set_pane_agent_activity(
            8,
            AgentActivity::socket(AgentActivityState::Errored, "errored", Some("codex".into())),
        );
        // Evidence can outlive a closed pane for a short poll interval. It
        // must not create an unfocusable target.
        first.set_pane_agent_activity(
            999,
            AgentActivity::socket(
                AgentActivityState::WaitingInput,
                "waiting for input",
                Some("claude".into()),
            ),
        );

        let mut second = tab_with(10, 3);
        second.set_pane_agent_activity(
            3,
            AgentActivity::termprop(
                AgentActivityState::WaitingInput,
                "waiting for input",
                Some("claude".into()),
            ),
        );
        workspace.tabs.extend([first, second]);

        for index in 0..20 {
            state
                .event_store
                .emit("output", serde_json::json!({ "index": index }));
        }
        let targets = attention_targets_at(&state, crate::events::unix_time_ms());

        assert_eq!(
            targets
                .iter()
                .map(|target| (target.tab_id, target.pane_id))
                .collect::<Vec<_>>(),
            vec![(20, 8), (10, 3)]
        );
        assert_eq!(targets[0].repository, "/src/taarof");
        assert_eq!(targets[0].worktree, "/worktrees/TAAROF-27");
        assert_eq!(targets[0].machine, "workstation");
        assert_eq!(targets[0].evidence.provider.as_deref(), Some("codex"));
    }

    #[test]
    fn projection_signature_changes_once_when_fresh_attention_expires() {
        let mut state = crate::AppState::new();
        let mut waiting = tab_with(20, 8);
        waiting.set_pane_agent_activity(
            8,
            AgentActivity::output_scan(
                AgentActivityState::WaitingInput,
                "waiting for input",
                Some("claude".into()),
            ),
        );
        state.workspaces[0].tabs.push(waiting);

        assert!(state.refresh_attention_projection());
        assert!(!state.refresh_attention_projection());

        let signal = state
            .find_tab_mut(20)
            .expect("waiting tab")
            .pane_agent_activity
            .get_mut(&8)
            .expect("waiting evidence");
        signal.updated_at = Instant::now() - Duration::from_secs(9);
        signal.observed_at_unix_ms = crate::events::unix_time_ms() - 9_000;

        assert!(state.refresh_attention_projection());
        assert!(!state.refresh_attention_projection());
    }
}
