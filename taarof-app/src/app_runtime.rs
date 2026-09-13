//! App-level runtime polling and UI state fanout.
//!
//! This module owns periodic scans that reconcile runtime state back into the
//! sidebar and dashboard. It must not own startup wiring, session persistence,
//! or widget construction.

use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::{
    probe::{ProbeState, ProbeTransition},
    runtime_probe::{ProcProbeSource, RuntimeProbeSnapshot, RuntimeProbeWorkerFailure},
    sidebar, sound, RuntimeHandle,
};

type TabPidList = Vec<(u32, Vec<i32>)>;
type PanePidList = Vec<(u32, u32, i32)>;

/// Records a local-future cancellation while the blocking worker is pending.
/// Gio reports worker panics through its join result, but dropping the local
/// future is the cancellation path; without this guard that path would leave
/// the previous snapshot looking healthy forever.
struct RuntimeProbeCompletionGuard {
    completed: bool,
    runtime: RuntimeHandle,
    state: Rc<RefCell<crate::AppState>>,
    tab_list: gtk::Box,
    term_stack: gtk::Stack,
    tab_pids: TabPidList,
    pane_pids: PanePidList,
}

impl RuntimeProbeCompletionGuard {
    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for RuntimeProbeCompletionGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let failure = RuntimeProbeWorkerFailure::Cancelled;
        let snapshot_changed = {
            let mut state = self.state.borrow_mut();
            let snapshot = RuntimeProbeSnapshot::worker_failure(
                crate::events::unix_time_ms(),
                &self.tab_pids,
                &self.pane_pids,
                failure,
            )
            .reconcile(state.runtime_probe.as_ref());
            let changed = state
                .runtime_probe
                .as_ref()
                .is_none_or(|previous| !previous.renders_same_as(&snapshot));
            state.install_runtime_probe(snapshot);
            changed
        };
        self.runtime.emit_event(
            "runtime_probe_state_changed",
            serde_json::json!({
                "component": "worker",
                "current": "cancelled",
                "reason": failure.reason(),
            }),
        );
        let reason = failure.reason().to_string();
        // Dropping Gio's join handle detaches the already-scheduled task; the
        // diagnostic write must not block this GTK-thread cancellation path.
        drop(gio::spawn_blocking(move || {
            crate::diagnostics::record_probe_failure(
                "runtime-probe",
                "worker",
                reason,
                Some(serde_json::json!({ "current": "cancelled" })),
            );
        }));
        if snapshot_changed {
            sidebar::refresh_all_tab_rows(&self.tab_list, &self.state);
            crate::dashboard::refresh_dashboard_if_open(&self.state, &self.term_stack);
        }
    }
}

/// Build the notification default-action and id for focusing a specific
/// (tab, pane). Activating the notification triggers the `app.focus-pane`
/// action with a `"<tab>:<pane>"` string target so the handler can focus the
/// exact pane, not just the tab. When the driving pane is unknown, fall back
/// to the tab's focused pane and a tab-scoped notification id.
pub(crate) fn focus_pane_notification(
    tab_id: u32,
    target_pane: Option<u32>,
    focused_pane_id: u32,
) -> (String, String) {
    match target_pane {
        Some(pane) => (
            format!("app.focus-pane::{tab_id}:{pane}"),
            format!("tab-attention-{tab_id}-{pane}"),
        ),
        None => (
            format!("app.focus-pane::{tab_id}:{focused_pane_id}"),
            format!("tab-attention-{tab_id}"),
        ),
    }
}

/// Poll process trees, update agent dots, and detect busy→idle transitions.
/// Also sends desktop notifications when the window is unfocused.
pub(crate) fn update_agent_indicators(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    in_flight: &crate::runtime_probe::ProbeInFlight,
) {
    // Coalesce: if a probe build is still in flight, drop this tick rather than
    // queue an overlapping /proc walk. The guard clears the flag on drop.
    let guard = match in_flight.try_begin() {
        Some(guard) => guard,
        None => return,
    };

    let state = runtime.shared_state();
    let (tab_pids, pane_pids): (TabPidList, PanePidList) = {
        let st = state.borrow();
        let mut tab_pids = Vec::new();
        let mut pane_pids = Vec::new();
        let now_ms = crate::events::unix_time_ms();
        for tab in st.all_tabs() {
            let pids = tab.panes.collect_pids();
            pane_pids.extend(tab.panes.leaves().into_iter().filter_map(|leaf| {
                crate::runtime_probe::pane_process_root(leaf, now_ms)
                    .map(|pid| (tab.id, leaf.pane_id, pid))
            }));
            tab_pids.push((tab.id, pids));
        }
        (tab_pids, pane_pids)
    };

    let runtime = runtime.clone();
    let term_stack = term_stack.clone();
    let tab_list = tab_list.clone();
    let window = window.clone();

    glib::spawn_future_local(async move {
        // Keep the in-flight guard alive for the whole probe; it clears the flag
        // when this future ends (including early return or a dropped future).
        let _guard = guard;
        let mut completion = RuntimeProbeCompletionGuard {
            completed: false,
            runtime: runtime.clone(),
            state: state.clone(),
            tab_list: tab_list.clone(),
            term_stack: term_stack.clone(),
            tab_pids: tab_pids.clone(),
            pane_pids: pane_pids.clone(),
        };

        // The only heavy work — the /proc walk and listen-table parse — runs on
        // a blocking worker. Only the finished (Send) snapshot crosses back.
        let worker_tab_pids = tab_pids.clone();
        let worker_pane_pids = pane_pids.clone();
        let snapshot = match gio::spawn_blocking(move || {
            let probe_source = ProcProbeSource;
            RuntimeProbeSnapshot::build(
                &probe_source,
                crate::events::unix_time_ms(),
                &worker_tab_pids,
                &worker_pane_pids,
            )
        })
        .await
        {
            Ok(snapshot) => snapshot,
            Err(_) => RuntimeProbeSnapshot::worker_failure(
                crate::events::unix_time_ms(),
                &tab_pids,
                &pane_pids,
                RuntimeProbeWorkerFailure::Panicked,
            ),
        };
        completion.complete();

        let (dirty, dashboard_dirty, pending_events, probe_failures) = {
            let mut st = state.borrow_mut();
            let previous_process_state = st
                .runtime_probe
                .as_ref()
                .map_or(ProbeState::Unknown, |probe| probe.process_probe);
            let previous_ports_state = st
                .runtime_probe
                .as_ref()
                .map_or(ProbeState::Unknown, |probe| probe.ports_probe);
            let previous_process_error = st
                .runtime_probe
                .as_ref()
                .and_then(|probe| probe.process_error.clone());
            let previous_ports_error = st
                .runtime_probe
                .as_ref()
                .and_then(|probe| probe.ports_error.clone());
            let snapshot = snapshot.reconcile(st.runtime_probe.as_ref());
            // Whether the freshly built snapshot renders differently from the
            // previous one. Read before overwriting `st.runtime_probe`.
            let snapshot_changed = st
                .runtime_probe
                .as_ref()
                .is_none_or(|prev| !prev.renders_same_as(&snapshot));
            let active_tab_id = st.active_ws().map_or(0, |w| w.active_tab);
            let window_focused = window.is_active();
            let mut rows_dirty = false;
            let mut pending_events: Vec<(&'static str, serde_json::Value)> = Vec::new();
            let mut probe_failures = Vec::new();
            for (component, previous, current, previous_error, error) in [
                (
                    "process",
                    previous_process_state,
                    snapshot.process_probe,
                    previous_process_error,
                    snapshot.process_error.clone(),
                ),
                (
                    "ports",
                    previous_ports_state,
                    snapshot.ports_probe,
                    previous_ports_error,
                    snapshot.ports_error.clone(),
                ),
            ] {
                let transition = ProbeTransition { previous, current };
                let reason_changed = previous_error != error;
                if transition.changed() || reason_changed {
                    pending_events.push((
                        "runtime_probe_state_changed",
                        serde_json::json!({
                            "component": component,
                            "previous": previous.label(),
                            "current": current.label(),
                            "reason": error,
                        }),
                    ));
                }
                if current.is_degraded() && (transition.entered_degraded() || reason_changed) {
                    probe_failures.push((
                        component,
                        error.unwrap_or_else(|| format!("runtime {component} probe degraded")),
                        previous.label(),
                        current.label(),
                    ));
                }
            }
            let process_projection_available =
                matches!(snapshot.process_probe, ProbeState::Ok | ProbeState::Stale);
            let process_projection_fresh = matches!(snapshot.process_probe, ProbeState::Ok);
            for tab in st.all_tabs_mut() {
                if process_projection_available {
                    for leaf in tab.panes.leaves_mut() {
                        if let Some(process_state) =
                            snapshot.pane_process_states.get(&(tab.id, leaf.pane_id))
                        {
                            leaf.update_process_state(process_state.clone());
                        } else if matches!(snapshot.process_probe, ProbeState::Ok) {
                            leaf.update_process_state(crate::pane::PaneProcessState::default());
                        }
                    }
                }

                let previous_agent_running = tab.agent_running;
                let previous_agent_name = tab.agent_name.clone();
                let previous_ports = tab.listening_ports.clone();
                let previous_attention = tab.needs_attention;
                let previous_notification = tab.notification_msg.clone();
                let previous_primary_state = tab.primary_state();

                let local_pane_ids: Vec<u32> = tab
                    .panes
                    .leaves()
                    .into_iter()
                    .filter(|leaf| !leaf.process_state.remote_shell)
                    .map(|leaf| leaf.pane_id)
                    .collect();
                let agents_view = crate::runtime_probe::resolve_tab_agents(
                    &snapshot,
                    tab.id,
                    &local_pane_ids,
                    tab.focused_pane_id,
                );
                let local_agent_status = agents_view.primary.clone();
                let pane_busy = tab.panes.any_busy();
                if process_projection_fresh {
                    let agent_process_present = local_agent_status.is_some();
                    // A vanished agent process cannot still be mid-turn: drop
                    // native turn evidence for panes with no live agent before any
                    // state is derived from it. A degraded process projection skips
                    // this block, preserving last-known identity and activity.
                    let agent_pane_ids: Vec<u32> = agents_view
                        .agents
                        .iter()
                        .map(|(pane_id, _)| *pane_id)
                        .collect();
                    rows_dirty |= tab.retain_pane_turns(&agent_pane_ids);
                    let explicitly_signaled = tab.has_fresh_running_activity();
                    tab.agent_running = explicitly_signaled;
                    if !agent_process_present && !explicitly_signaled {
                        rows_dirty |= tab.clear_output_scan_activities_for_panes(&local_pane_ids);
                    } else if !tab.agent_running {
                        rows_dirty |=
                            tab.clear_running_output_scan_activities_for_panes(&local_pane_ids);
                    }
                    if !tab.agent_running && !pane_busy {
                        rows_dirty |= tab.clear_running_activity();
                    }
                    tab.agent_name = local_agent_status
                        .as_ref()
                        .and_then(|(_, status)| status.agent_name.clone())
                        .or_else(|| {
                            tab.agent_activity
                                .as_ref()
                                .and_then(|activity| activity.source.clone())
                        });
                    tab.agent_session_id = local_agent_status
                        .as_ref()
                        .and_then(|(_, status)| status.session_id.clone());
                    tab.agent_pane_id = local_agent_status.as_ref().map(|(pane_id, _)| *pane_id);
                }
                if matches!(snapshot.ports_probe, ProbeState::Ok | ProbeState::Stale) {
                    tab.listening_ports =
                        snapshot.tab_ports.get(&tab.id).cloned().unwrap_or_default();
                    tab.listening_ports_updated_at_unix_ms = snapshot.ports_observed_at_unix_ms;
                } else if let Some(partial_ports) = snapshot.tab_ports.get(&tab.id) {
                    // A partially readable /proc/net source can prove these
                    // ports exist, but cannot prove omitted ports disappeared.
                    // Surface the positive data without replacing last-good
                    // timestamps or clearing unrelated retained ports.
                    tab.listening_ports.extend(partial_ports);
                    tab.listening_ports.sort_unstable();
                    tab.listening_ports.dedup();
                }
                if process_projection_fresh {
                    if let Some(activity) = tab.agent_activity.as_mut() {
                        if activity.source.is_none() {
                            activity.source = tab.agent_name.clone();
                        }
                    }
                }

                let any_transitioned = tab.panes.update_busy_transitions();
                if any_transitioned && tab.id != active_tab_id {
                    tab.needs_attention = true;
                    // The pane whose event drove notification_msg, if known; else
                    // the tab's primary agent pane. This is the pane the
                    // notification will focus when activated.
                    let target_pane = tab.notification_pane_id.or(tab.agent_pane_id);
                    // Per-pane dedup: only fire when this pane's notified state
                    // actually changed to Done, so repeated identical scans do not
                    // re-fire and one agent's event cannot resuppress another's.
                    let dedup_pane = target_pane.unwrap_or(tab.focused_pane_id);
                    let should_fire = tab.note_pane_notification(
                        dedup_pane,
                        Some(crate::workspace::AgentActivityState::Done),
                    );
                    if !window_focused && should_fire {
                        tab.notified = true;
                        if let Some(app) = window.application() {
                            let notification = gio::Notification::new("taarof — Tab ready");
                            let body = tab.notification_msg.as_deref().unwrap_or(&tab.name);
                            notification.set_body(Some(body));
                            // Target the exact tab + pane so activation focuses the
                            // right pane, not just the tab.
                            let (action, notif_id) =
                                focus_pane_notification(tab.id, target_pane, tab.focused_pane_id);
                            notification.set_default_action(&action);
                            app.send_notification(Some(&notif_id), &notification);
                            sound::play_attention_sound();
                        }
                    }
                }
                if !tab.needs_attention {
                    tab.notified = false;
                    tab.notification_msg = None;
                    // Attention was consumed (tab focused / activity cleared): let
                    // the pane notify again on its next transition.
                    if let Some(pane) = tab.notification_pane_id.take() {
                        tab.clear_pane_notification(pane);
                    }
                }

                if previous_agent_running != tab.agent_running
                    || previous_agent_name != tab.agent_name
                {
                    pending_events.push((
                        "agent_state_changed",
                        serde_json::json!({
                            "tab_id": tab.id,
                            "tab_name": tab.name,
                            "running": tab.agent_running,
                            "agent_name": tab.agent_name,
                            "agents": agents_view
                                .agents
                                .iter()
                                .map(|(pane_id, status)| serde_json::json!({
                                    "pane_id": pane_id,
                                    "agent_name": status.agent_name,
                                    "session_id": status.session_id,
                                }))
                                .collect::<Vec<_>>(),
                        }),
                    ));
                }
                if previous_ports != tab.listening_ports {
                    pending_events.push((
                        "ports_changed",
                        serde_json::json!({
                            "tab_id": tab.id,
                            "tab_name": tab.name,
                            "ports": tab.listening_ports,
                        }),
                    ));
                }
                if !previous_attention && tab.needs_attention {
                    pending_events.push((
                        "alert_raised",
                        serde_json::json!({
                            "tab_id": tab.id,
                            "tab_name": tab.name,
                            "message": tab.notification_msg.clone().unwrap_or_else(|| tab.name.clone()),
                            "source": "activity-poll",
                        }),
                    ));
                } else if previous_attention && !tab.needs_attention {
                    pending_events.push((
                        "alert_cleared",
                        serde_json::json!({
                            "tab_id": tab.id,
                            "tab_name": tab.name,
                            "message": previous_notification.clone().unwrap_or_else(|| tab.name.clone()),
                            "source": "activity-poll",
                        }),
                    ));
                }

                // A rendered tab field changed this tick if any persisted
                // sidebar-facing field moved. Combined with `snapshot_changed`
                // and dashboard events into the `dirty` gate below.
                rows_dirty |= previous_agent_running != tab.agent_running
                    || previous_agent_name != tab.agent_name
                    || previous_ports != tab.listening_ports
                    || previous_attention != tab.needs_attention
                    || previous_notification != tab.notification_msg
                    // Transcript-driven transitions (a finished turn ageing out
                    // of DONE, say) move the rendered state without touching any
                    // of the fields above.
                    || previous_primary_state != tab.primary_state();
            }

            let dashboard_dirty = !pending_events.is_empty();
            st.install_runtime_probe(snapshot);
            let dirty = snapshot_changed || rows_dirty || dashboard_dirty;
            (dirty, dashboard_dirty, pending_events, probe_failures)
        };
        // Socket/web clients must keep receiving these regardless of the sidebar
        // render gate, so the emit loop stays unconditional.
        for (event_type, payload) in pending_events {
            runtime.emit_event(event_type, payload);
        }
        if !probe_failures.is_empty() {
            glib::spawn_future_local(async move {
                let _ = gio::spawn_blocking(move || {
                    for (component, message, previous, current) in probe_failures {
                        crate::diagnostics::record_probe_failure(
                            "runtime-probe",
                            component,
                            message,
                            Some(serde_json::json!({
                                "previous": previous,
                                "current": current,
                            })),
                        );
                    }
                })
                .await;
            });
        }

        if dirty {
            sidebar::refresh_all_tab_rows(&tab_list, &state);
        }
        if dashboard_dirty {
            crate::dashboard::refresh_dashboard_if_open(&state, &term_stack);
        }
    });
}

/// Poll each running pane agent's on-disk transcript for newly appended
/// assistant messages, mirror the folded [`crate::agents::TranscriptState`]
/// into `AppState`, and emit an `agent_message` event per pane that saw new
/// messages. All disk IO runs on a blocking worker; the main thread only builds
/// bindings from in-memory probe state and writes the finished results back.
///
/// Structurally mirrors [`update_agent_indicators`]: a single-flight guard
/// coalesces overlapping ticks, and the heavy work crosses to `gio::spawn_blocking`.
pub(crate) fn update_pane_transcripts(
    runtime: &RuntimeHandle,
    tracker: &Arc<crate::agents::TranscriptTracker>,
    in_flight: &crate::runtime_probe::ProbeInFlight,
    tab_list: &gtk::Box,
) {
    // Coalesce: drop this tick if a previous poll is still running.
    let guard = match in_flight.try_begin() {
        Some(guard) => guard,
        None => return,
    };

    let state = runtime.shared_state();
    let (bindings, transcripts_empty) = {
        let st = state.borrow();
        (
            crate::agents::collect_transcript_bindings(&st),
            st.pane_transcripts.is_empty(),
        )
    };
    // Nothing to poll and nothing to clear: skip the worker entirely. The guard
    // drops here, freeing the next tick.
    if bindings.is_empty() && transcripts_empty && !tracker.has_tracked_panes() {
        return;
    }

    let tracker = tracker.clone();
    let runtime = runtime.clone();
    let tab_list = tab_list.clone();

    glib::spawn_future_local(async move {
        // Hold the guard for the whole poll; it clears on drop even if the
        // future is cancelled or the worker errors.
        let _guard = guard;

        let mut result = match gio::spawn_blocking(move || tracker.sync_and_poll(&bindings)).await {
            Ok(result) => result,
            Err(_) => return,
        };

        // Apply results and collect events under one borrow, then drop it before
        // emitting (emit_event re-borrows the shared state mutably).
        let ui_dirty = !result.removed.is_empty() || !result.ticks.is_empty();
        let (events, transition_events) = {
            let mut st = state.borrow_mut();
            result.retain_tabs(&st.all_tabs().map(|tab| tab.id).collect());
            let mut transition_events = Vec::new();
            for key in result.removed {
                st.pane_transcripts.remove(&key);
                // An unbound pane has no native turn to speak for it any more.
                if let Some(tab) = st.find_tab_mut(key.0) {
                    let before = tab.pane_lifecycle(key.1);
                    tab.clear_pane_turn(key.1);
                    let after = tab.pane_lifecycle(key.1);
                    if let Some(payload) = crate::events::agent_activity_transition_payload(
                        key.0, key.1, before, after, None,
                    ) {
                        transition_events.push(payload);
                    }
                }
            }
            let mut events = Vec::new();
            for tick in result.ticks {
                // Mirror the native turn onto the tab so every surface — the
                // sidebar rows, the agent cards, the socket and HTTP APIs —
                // resolves the same canonical state from the same evidence.
                // This happens for baseline ticks too: the turn carries the
                // transcript record's own timestamp, so replayed history is
                // already stale on arrival and cannot fake live work.
                let turn = tick.state.turn;
                if let Some(tab) = st.find_tab_mut(tick.key.0) {
                    let before = tab.pane_lifecycle(tick.key.1);
                    tab.set_pane_turn(tick.key.1, turn);
                    let after = tab.pane_lifecycle(tick.key.1);
                    if let Some(mut payload) = crate::events::agent_activity_transition_payload(
                        tick.key.0,
                        tick.key.1,
                        before,
                        after,
                        Some(&tick.state.agent),
                    ) {
                        payload["native_turn"] = serde_json::json!(tick.state.native_turn_id);
                        transition_events.push(payload);
                    }
                }
                let previous = st.pane_transcripts.get(&tick.key);
                // The tracker marks first discovery and agent-session switches
                // explicitly. Cache those folded states, including an empty
                // transcript, but never replay their history as current work.
                let same_session = !tick.baseline
                    && previous
                        .is_some_and(|previous| previous.session_id == tick.state.session_id);
                let work_events = if same_session {
                    tick.work_events
                        .iter()
                        .map(|event| match event {
                            crate::agents::TranscriptWorkEvent::AssistantCompleted { ordinal } => {
                                serde_json::json!({
                                    "kind": "assistant_completed",
                                    "ordinal": ordinal,
                                })
                            }
                            crate::agents::TranscriptWorkEvent::FileOperation {
                                path,
                                op,
                                at_unix_ms,
                                ordinal,
                            } => serde_json::json!({
                                "kind": "file_operation",
                                "path": path,
                                "op": op.wire(),
                                "at_unix_ms": at_unix_ms,
                                "ordinal": ordinal,
                            }),
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                let new_messages = if same_session {
                    tick.new_messages as u64
                } else {
                    0
                };
                let recent_files = if same_session {
                    tick.work_events
                        .iter()
                        .filter_map(|event| {
                            let crate::agents::TranscriptWorkEvent::FileOperation {
                                path,
                                op,
                                at_unix_ms,
                                ordinal,
                            } = event
                            else {
                                return None;
                            };
                            Some(serde_json::json!({
                                "path": path,
                                "op": op.wire(),
                                "at_unix_ms": at_unix_ms,
                                "ordinal": ordinal,
                            }))
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                if new_messages > 0 || !recent_files.is_empty() {
                    events.push(serde_json::json!({
                        "tab_id": tick.key.0,
                        "pane_id": tick.key.1,
                        "agent_name": tick.state.agent,
                        "session_id": tick.state.session_id,
                        "last_message": tick.state.last_message,
                        "message_count": tick.state.message_count,
                        "new_messages": new_messages,
                        "work_events": work_events,
                        "files_touched": tick.state.files_touched,
                        "recent_files": recent_files,
                        "updated_at_unix_ms": tick.state.updated_at_unix_ms,
                    }));
                }
                st.pane_transcripts.insert(tick.key, tick.state);
            }
            (events, transition_events)
        };

        for payload in transition_events {
            runtime.emit_event("agent_activity_changed", payload);
        }
        for payload in events {
            runtime.emit_event("agent_message", payload);
        }
        if ui_dirty {
            crate::sidebar::refresh_all_tab_rows(&tab_list, &state);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::focus_pane_notification;

    #[test]
    fn focus_pane_notification_targets_the_driving_pane() {
        // A pane-scoped event (e.g. codex #2 waiting in pane 5 of tab 3)
        // targets that exact pane in both the action and the notification id,
        // so activation focuses tab 3 AND pane 5.
        let (action, notif_id) = focus_pane_notification(3, Some(5), 1);
        assert_eq!(action, "app.focus-pane::3:5");
        assert_eq!(notif_id, "tab-attention-3-5");
    }

    #[test]
    fn focus_pane_notification_falls_back_to_focused_pane() {
        // Unknown driving pane: fall back to the tab's focused pane and a
        // tab-scoped id (preserves the pre-EXAMPLE-78 id shape for that case).
        let (action, notif_id) = focus_pane_notification(3, None, 8);
        assert_eq!(action, "app.focus-pane::3:8");
        assert_eq!(notif_id, "tab-attention-3");
    }
}
