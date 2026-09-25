//! Tmux session attach/detach, dashboard targets, and session lifecycle.

use super::*;
#[cfg(test)]
use crate::terminal::restore::TmuxKillResult;
use crate::terminal::restore::{
    apply_dashboard_poll_results, apply_tmux_kill_result, run_tmux_command_sync_result,
    run_tmux_list_sessions_command_sync_result, tmux_kill_result_from_outcome,
    DashboardPollOutcome,
};
use crate::terminal::splits::{close_pane, split_pane_for_tab};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AttachedSessionPane {
    pub(super) tab_id: u32,
    pub(super) pane_id: u32,
    pub(super) session_name: String,
    pub(super) target: crate::tmux::TmuxTarget,
}

pub(super) fn resolve_attached_session_pane_in_list(
    session_name: &str,
    target: &crate::tmux::TmuxTarget,
    panes: &[AttachedSessionPane],
) -> Option<(u32, u32)> {
    panes
        .iter()
        .find(|pane| pane.session_name == session_name && &pane.target == target)
        .map(|pane| (pane.tab_id, pane.pane_id))
}

pub(super) fn collect_attached_session_panes(state: &crate::AppState) -> Vec<AttachedSessionPane> {
    state
        .workspaces
        .iter()
        .flat_map(|ws| ws.tabs.iter())
        .flat_map(|tab| {
            tab.panes.leaves().into_iter().filter_map(move |leaf| {
                let backing = leaf.tmux_backing.as_ref()?;
                Some(AttachedSessionPane {
                    tab_id: tab.id,
                    pane_id: leaf.pane_id,
                    session_name: backing.session_name.clone(),
                    target: backing.target.clone(),
                })
            })
        })
        .collect()
}

pub(crate) fn resolve_attached_session_pane(
    state: &crate::AppState,
    session_name: &str,
    target: &crate::tmux::TmuxTarget,
) -> Option<(u32, u32)> {
    resolve_attached_session_pane_in_list(
        session_name,
        target,
        &collect_attached_session_panes(state),
    )
}

pub fn detach_session_by_name(
    state: &Rc<RefCell<crate::AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    session_name: &str,
    target: &crate::tmux::TmuxTarget,
) -> Result<String, String> {
    let (tab_id, pane_id) = {
        let st = state.borrow();
        resolve_attached_session_pane(&st, session_name, target)
    }
    .ok_or_else(|| {
        format!(
            "Attached session not found: {session_name} on {}",
            tmux_target_label(target)
        )
    })?;

    detach_pane(state, term_stack, tab_list, tab_id, pane_id)
}

pub(super) fn resolve_session_target(
    detached: &[crate::dashboard::DetachedSession],
    live_targets: &[(String, crate::tmux::TmuxTarget)],
    session_name: &str,
    target_hint: Option<&crate::tmux::TmuxTarget>,
) -> crate::tmux::TmuxTarget {
    if let Some(target) = target_hint {
        return target.clone();
    }

    if let Some(target) = detached
        .iter()
        .find(|d| d.session_name == session_name)
        .map(|d| d.target.clone())
    {
        return target;
    }

    if let Some(target) = live_targets
        .iter()
        .find(|(name, _)| name == session_name)
        .map(|(_, target)| target.clone())
    {
        return target;
    }

    crate::tmux::TmuxTarget::Local
}

pub(super) fn collect_dashboard_targets(
    state: &crate::AppState,
    app_config: &crate::config::AppConfig,
) -> Vec<crate::tmux::TmuxTarget> {
    let mut targets = Vec::new();

    let mut push_unique = |target: crate::tmux::TmuxTarget| {
        if !targets.iter().any(|existing| existing == &target) {
            targets.push(target);
        }
    };

    for ws in &state.workspaces {
        let configured_remote_target =
            dashboard_remote_tmux_target(ws.host_config_name.as_deref(), app_config);

        if let Some(target) = configured_remote_target.clone() {
            push_unique(target);
        }

        if ws.tmux_backed {
            push_unique(configured_remote_target.unwrap_or(crate::tmux::TmuxTarget::Local));
        }

        for tab in &ws.tabs {
            for leaf in tab.panes.leaves() {
                if let Some(backing) = leaf.tmux_backing.as_ref() {
                    push_unique(backing.target.clone());
                }
            }
        }
    }

    for detached in &state.detached_sessions {
        push_unique(detached.target.clone());
    }

    targets
}

pub(super) fn dashboard_remote_tmux_target(
    host_config_name: Option<&str>,
    app_config: &crate::config::AppConfig,
) -> Option<crate::tmux::TmuxTarget> {
    let host_name = host_config_name?;
    app_config
        .hosts
        .iter()
        .find(|host| host.name == host_name)
        .and_then(|host| host.ssh_target.clone())
        .map(|ssh_target| crate::tmux::TmuxTarget::Remote { ssh_target })
}

/// Kill a tmux session by name.
/// For detached sessions: uses the stored target and removes from detached_sessions.
/// For attached sessions: resolves the target from the live pane backing.
/// Falls back to local tmux only when no better target is known.
#[cfg(test)]
pub(super) fn kill_session_by_name_with_hint_using(
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
    session_name: &str,
    target_hint: Option<&crate::tmux::TmuxTarget>,
    kill: impl FnOnce(&crate::tmux::TmuxTarget, &str) -> TmuxKillResult,
) -> Result<(), String> {
    let kill_target = {
        let st = state.borrow();
        let live_targets: Vec<(String, crate::tmux::TmuxTarget)> = st
            .workspaces
            .iter()
            .flat_map(|ws| ws.tabs.iter())
            .flat_map(|tab| tab.panes.leaves())
            .filter_map(|leaf| {
                let backing = leaf.tmux_backing.as_ref()?;
                Some((backing.session_name.clone(), backing.target.clone()))
            })
            .collect();
        resolve_session_target(
            &st.detached_sessions,
            &live_targets,
            session_name,
            target_hint,
        )
    };
    let backing = crate::pane::TmuxBacking {
        session_name: session_name.to_string(),
        target: kill_target.clone(),
        expected_generation: None,
        pane_info: crate::probe::ProbeSnapshot::default(),
    };
    let result = kill(&kill_target, session_name);
    apply_tmux_kill_result(state, &backing, result)
}

pub fn kill_session_by_name_with_hint_async(
    state: &std::rc::Rc<std::cell::RefCell<crate::AppState>>,
    window: &adw::ApplicationWindow,
    session_name: &str,
    target_hint: Option<&crate::tmux::TmuxTarget>,
    callback: impl FnOnce(Result<(), String>) + 'static,
) {
    let kill_target = {
        let st = state.borrow();
        let live_targets = st
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.tabs.iter())
            .flat_map(|tab| tab.panes.leaves())
            .filter_map(|leaf| {
                let backing = leaf.tmux_backing.as_ref()?;
                Some((backing.session_name.clone(), backing.target.clone()))
            })
            .collect::<Vec<_>>();
        resolve_session_target(
            &st.detached_sessions,
            &live_targets,
            session_name,
            target_hint,
        )
    };
    let backing = crate::pane::TmuxBacking {
        session_name: session_name.to_string(),
        target: kill_target.clone(),
        expected_generation: None,
        pane_info: crate::probe::ProbeSnapshot::default(),
    };
    let guard = crate::tmux::TmuxGtkApplyGuard::new(state, window);
    let worker = crate::tmux::default_worker();
    glib::spawn_future_local(async move {
        let completion = worker
            .submit_coalesced(
                crate::tmux::TmuxJobKey::KillSession(
                    kill_target.clone(),
                    backing.session_name.clone(),
                ),
                vec![crate::tmux::kill_session_command(
                    &kill_target,
                    &backing.session_name,
                )],
                Duration::from_secs(10),
            )
            .await;
        let result = match completion {
            Ok(completion) => match guard.upgrade(&completion) {
                Some((state, _window)) => completion
                    .outcomes()
                    .first()
                    .map(tmux_kill_result_from_outcome)
                    .map_or_else(
                        || Err("tmux kill returned no result".to_string()),
                        |outcome| apply_tmux_kill_result(&state, &backing, outcome),
                    ),
                None => Err("tmux kill result was superseded".to_string()),
            },
            Err(error) => Err(error),
        };
        callback(result);
    });
}

pub(super) fn resolve_attach_target_tab_id_in_workspace(
    active_tab_id: u32,
    last_active_tab_id: Option<u32>,
    tabs: &[(u32, crate::workspace::TabKind)],
) -> Option<u32> {
    let is_terminal = |tab_id| {
        tabs.iter()
            .any(|(id, kind)| *id == tab_id && matches!(kind, crate::workspace::TabKind::Terminal))
    };

    if is_terminal(active_tab_id) {
        return Some(active_tab_id);
    }

    if let Some(tab_id) = last_active_tab_id.filter(|tab_id| is_terminal(*tab_id)) {
        return Some(tab_id);
    }

    tabs.iter()
        .find(|(_, kind)| matches!(kind, crate::workspace::TabKind::Terminal))
        .map(|(id, _)| *id)
}

pub(super) fn resolve_attach_target_tab_id(state: &crate::AppState) -> Option<u32> {
    let workspace = state.active_ws()?;
    let tabs: Vec<(u32, crate::workspace::TabKind)> = workspace
        .tabs
        .iter()
        .map(|tab| (tab.id, tab.kind))
        .collect();
    resolve_attach_target_tab_id_in_workspace(
        workspace.active_tab,
        workspace.last_active_tab,
        &tabs,
    )
}

pub(super) fn tmux_target_label(target: &crate::tmux::TmuxTarget) -> String {
    match target {
        crate::tmux::TmuxTarget::Local => "localhost".to_string(),
        crate::tmux::TmuxTarget::Remote { ssh_target } => ssh_target.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachSessionError {
    NoTerminalTabAvailable,
    SessionUnavailable {
        session_name: String,
        target: String,
    },
    CouldNotCreateSplitPane,
    RequestSuperseded,
}

impl std::fmt::Display for AttachSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTerminalTabAvailable => {
                write!(f, "no terminal tab available in active workspace")
            }
            Self::SessionUnavailable {
                session_name,
                target,
            } => write!(
                f,
                "tmux session {} is not available on {}",
                session_name, target
            ),
            Self::CouldNotCreateSplitPane => write!(f, "could not create split pane"),
            Self::RequestSuperseded => write!(f, "attach request was superseded"),
        }
    }
}

impl std::error::Error for AttachSessionError {}

/// Attach to a detached tmux session by adding it as a split pane in the
/// current workspace context. If the active tab is non-terminal (for example
/// Dashboard), taarof falls back to the last active terminal tab in the active
/// workspace.
///
/// The detached session is removed only after spawn succeeds. If spawn fails,
/// the provisional pane is closed again and the detached session remains.
fn attach_session_after_validation(
    state: &Rc<RefCell<crate::AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    session_name: &str,
    target: &crate::tmux::TmuxTarget,
) -> Result<(), AttachSessionError> {
    let target_tab_id = {
        let st = state.borrow();
        resolve_attach_target_tab_id(&st)
    }
    .ok_or(AttachSessionError::NoTerminalTabAvailable)?;
    let runtime = RuntimeHandle::from_shared_state(state.clone());

    let _ = crate::sidebar::activate_tab(tab_list, state, term_stack, target_tab_id);

    let state_for_spawn = state.clone();
    let term_stack_for_spawn = term_stack.clone();
    let tab_list_for_spawn = tab_list.clone();
    let window_for_spawn = window.clone();
    let session_name_owned = session_name.to_string();
    let target_owned = target.clone();
    let on_spawn_result: PaneSpawnResultCallback =
        Rc::new(move |result, tab_id, pane_id| match result {
            Ok(_) => {
                let state = state_for_spawn.clone();
                let term_stack = term_stack_for_spawn.clone();
                let tab_list = tab_list_for_spawn.clone();
                let window = window_for_spawn.clone();
                let session_name = session_name_owned.clone();
                let target = target_owned.clone();
                glib::timeout_add_local_once(ATTACH_SESSION_CONFIRM_DELAY, move || {
                    let should_finalize = {
                        let st = state.borrow();
                        st.find_tab(tab_id)
                            .and_then(|(_, tab)| tab.panes.leaf(pane_id))
                            .and_then(|leaf| leaf.shell_pid)
                            .is_some()
                    };

                    if !should_finalize {
                        return;
                    }

                    {
                        let mut st = state.borrow_mut();
                        if let Some(tab) = st.find_tab_mut(tab_id) {
                            if let Some(leaf) = tab.panes.leaf_mut(pane_id) {
                                leaf.tmux_backing = Some(crate::pane::TmuxBacking {
                                    session_name: session_name.clone(),
                                    target: target.clone(),
                                    expected_generation: None,
                                    pane_info: ProbeSnapshot::default(),
                                });
                            }
                        }
                        st.detached_sessions
                            .retain(|d| !d.matches_target(&session_name, &target));
                        st.invalidate_dashboard_session_snapshot(&target, &session_name);
                    }

                    crate::sidebar::refresh_background_section(
                        &tab_list,
                        &state,
                        &term_stack,
                        &window,
                    );
                    crate::dashboard::refresh_dashboard_if_open(&state, &term_stack);
                });
            }
            Err(err) => {
                crate::diagnostics::record_command_failure(
                    "terminal",
                    "attach-session",
                    format!("attach_session spawn failed for {}", session_name_owned),
                    Some(serde_json::json!({
                        "session_name": session_name_owned,
                        "error": err.to_string(),
                    })),
                );

                let state = state_for_spawn.clone();
                let term_stack = term_stack_for_spawn.clone();
                let tab_list = tab_list_for_spawn.clone();
                glib::idle_add_local_once(move || {
                    let runtime = RuntimeHandle::from_shared_state(state.clone());
                    let _ = close_pane(&runtime, &term_stack, Some(&tab_list), tab_id, pane_id);
                });
            }
        });

    let argv = crate::tmux::attach_command(target, session_name);
    split_pane_for_tab(
        &runtime,
        term_stack,
        crate::pane::SplitDirection::Vertical,
        tab_list,
        window,
        target_tab_id,
        Some(argv),
        None,
        Some(on_spawn_result),
    )
    .ok_or(AttachSessionError::CouldNotCreateSplitPane)?;

    Ok(())
}

/// Check a tmux/SSH target on the bounded worker, then apply the attach only
/// when the originating window and request generation are still current.
pub fn attach_session_async(
    state: &Rc<RefCell<crate::AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    session_name: &str,
    target: &crate::tmux::TmuxTarget,
    callback: impl FnOnce(Result<(), AttachSessionError>) + 'static,
) {
    let guard = crate::tmux::TmuxGtkApplyGuard::new(state, window);
    let worker = crate::tmux::default_worker();
    let session_name = session_name.to_string();
    let target = target.clone();
    let term_stack = term_stack.clone();
    let tab_list = tab_list.clone();

    glib::spawn_future_local(async move {
        let completion = worker
            .submit_coalesced(
                crate::tmux::TmuxJobKey::HasSession(target.clone(), session_name.clone()),
                vec![crate::tmux::has_session_command_batch(
                    &target,
                    &session_name,
                )],
                Duration::from_secs(10),
            )
            .await;
        let result = match completion {
            Ok(completion) => match guard.upgrade(&completion) {
                Some((state, window)) => {
                    if !completion
                        .outcomes()
                        .first()
                        .is_some_and(|outcome| outcome.success)
                    {
                        Err(AttachSessionError::SessionUnavailable {
                            session_name: session_name.clone(),
                            target: tmux_target_label(&target),
                        })
                    } else {
                        attach_session_after_validation(
                            &state,
                            &term_stack,
                            &tab_list,
                            &window,
                            &session_name,
                            &target,
                        )
                    }
                }
                None => Err(AttachSessionError::RequestSuperseded),
            },
            Err(_) => Err(AttachSessionError::SessionUnavailable {
                session_name: session_name.clone(),
                target: tmux_target_label(&target),
            }),
        };
        callback(result);
    });
}

/// Poll detached session status, update finished state, and rebuild dashboard_state.
pub fn poll_dashboard_state(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
) {
    let app_config = crate::config::app_config();
    let (detached_targets, dashboard_targets, poll_context) = {
        let mut st = state.borrow_mut();
        let detached_targets = st
            .detached_sessions
            .iter()
            .map(|d| (d.session_name.clone(), d.target.clone(), d.detached_at))
            .collect::<Vec<_>>();
        let dashboard_targets = collect_dashboard_targets(&st, &app_config);
        let poll_context = st.begin_dashboard_poll(&dashboard_targets);
        (detached_targets, dashboard_targets, poll_context)
    };
    let detached_target_count = detached_targets.len();
    let dashboard_target_count = dashboard_targets.len();
    let polled_detached_sessions = detached_targets
        .iter()
        .map(|(name, target, detached_at)| {
            (
                crate::dashboard::detached_session_command_key(target, name),
                *detached_at,
            )
        })
        .collect::<Vec<_>>();
    let session_prefix = crate::config::tmux_config().session_prefix.clone();

    let state = state.clone();
    let term_stack = term_stack.clone();
    let tab_list = tab_list.clone();
    let window = window.clone();
    glib::spawn_future_local(async move {
        type BlockResult = (
            Vec<(
                crate::dashboard::DetachedSessionCommandKey,
                Result<String, String>,
            )>,
            Vec<(crate::tmux::TmuxTarget, Vec<(String, u64, bool, u32)>)>,
            Vec<(Option<crate::tmux::TmuxTarget>, String)>,
        );
        let (results, live_sessions, errors): BlockResult = gio::spawn_blocking(move || {
            let mut errors = Vec::new();

            let cmds = detached_targets
                .into_iter()
                .map(|(name, target, _)| {
                    let key = crate::dashboard::detached_session_command_key(&target, &name);
                    let argv = crate::tmux::pane_current_command(&target, &name);
                    match run_tmux_command_sync_result(&argv) {
                        Ok(output) => (key, Ok(output.trim().to_string())),
                        Err(err) => (
                            key,
                            Err(format!(
                                "detached session probe for {} on {} failed: {}",
                                name,
                                tmux_target_label(&target),
                                err
                            )),
                        ),
                    }
                })
                .collect();

            let sessions = dashboard_targets
                .into_iter()
                .filter_map(|target| {
                    let argv = crate::tmux::list_sessions_dashboard_command(&target);
                    match run_tmux_list_sessions_command_sync_result(&argv) {
                        Ok(output) => match crate::tmux::parse_list_sessions_tuples(&output) {
                            Ok(sessions) => Some((target, sessions)),
                            Err(err) => {
                                errors.push((
                                    Some(target.clone()),
                                    format!(
                                        "dashboard tmux probe for {} returned invalid output: {}",
                                        tmux_target_label(&target),
                                        err
                                    ),
                                ));
                                None
                            }
                        },
                        Err(err) => {
                            errors.push((
                                Some(target.clone()),
                                format!(
                                    "dashboard tmux probe for {} failed: {}",
                                    tmux_target_label(&target),
                                    err
                                ),
                            ));
                            None
                        }
                    }
                })
                .collect();

            (cmds, sessions, errors)
        })
        .await
        .unwrap_or_else(|err| {
            (
                Vec::new(),
                Vec::new(),
                vec![(None, format!("dashboard probe task failed: {err:?}"))],
            )
        });

        let current_commands: std::collections::HashMap<
            crate::dashboard::DetachedSessionCommandKey,
            String,
        > = results
            .iter()
            .filter_map(|(key, result)| {
                result
                    .as_ref()
                    .ok()
                    .map(|command| (key.clone(), command.clone()))
            })
            .collect();
        let detached_probe_errors = results
            .into_iter()
            .filter_map(|(key, result)| result.err().map(|error| (key, error)))
            .collect::<Vec<_>>();

        {
            let mut st = state.borrow_mut();
            apply_dashboard_poll_results(
                &mut st,
                DashboardPollOutcome {
                    current_commands: &current_commands,
                    polled_detached_sessions: &polled_detached_sessions,
                    live_sessions: &live_sessions,
                    detached_probe_errors: &detached_probe_errors,
                    dashboard_errors: &errors,
                    dashboard_target_count,
                    detached_target_count,
                    session_prefix: &session_prefix,
                    poll_context: &poll_context,
                },
            );
        }

        crate::sidebar::refresh_background_section(&tab_list, &state, &term_stack, &window);
        crate::dashboard::refresh_dashboard_if_open(&state, &term_stack);
    });
}
