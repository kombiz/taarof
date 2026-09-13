//! Headless socket server variants: read-only and smoke-test servers.
//!
//! These serve the socket IPC surface without GTK widgets, suitable for
//! headless smoke tests and automation that only needs state inspection or
//! controlled session attach/detach.

use super::*;

pub(super) fn start_state_only_socket_server<F>(
    state: Rc<RefCell<AppState>>,
    runtime_dir: &Path,
    handler: F,
) -> Option<PathBuf>
where
    F: Fn(&Rc<RefCell<AppState>>, SocketMessage) -> SocketResponse + 'static,
{
    const SOCKET_REQUEST_QUEUE_CAPACITY: usize = 256;

    let (socket_path, listener) = bind_socket_listener(runtime_dir)?;
    let (tx, rx) = tokio_mpsc::channel::<(SocketMessage, mpsc::Sender<SocketResponse>)>(
        SOCKET_REQUEST_QUEUE_CAPACITY,
    );

    let history_reader = state.borrow().history_reader.clone();
    spawn_socket_listener_thread(listener, tx, history_reader);

    glib::MainContext::default().spawn_local(async move {
        let mut rx = rx;
        while let Some((msg, response_tx)) = rx.recv().await {
            let action = socket_message_action(&msg);
            let response = run_socket_handler_safely(action, || handler(&state, msg));
            let _ = response_tx.send(response);
            glib::timeout_future(Duration::from_millis(0)).await;
        }
    });

    Some(socket_path)
}

/// Start the shared production dispatcher with headless final-apply surfaces.
/// Only the GTK widget apply is adapted; close preparation, worker execution,
/// generation guards, Background preservation, and response handling remain
/// the exact `dispatch_socket_message` production path.
fn start_headless_dispatch_socket_server(
    state: Rc<RefCell<AppState>>,
    runtime_dir: &Path,
    worker: crate::tmux::TmuxWorker,
    close_behavior: crate::config::TmuxCloseBehavior,
    before_tab_removal: Option<BeforeTabRemovalObserver>,
) -> Option<(PathBuf, Rc<Cell<u64>>)> {
    const SOCKET_REQUEST_QUEUE_CAPACITY: usize = 256;

    let (socket_path, listener) = bind_socket_listener(runtime_dir)?;
    let (tx, rx) = tokio_mpsc::channel::<(SocketMessage, mpsc::Sender<SocketResponse>)>(
        SOCKET_REQUEST_QUEUE_CAPACITY,
    );
    let history_reader = state.borrow().history_reader.clone();
    spawn_socket_listener_thread(listener, tx, history_reader);
    let generation = Rc::new(Cell::new(0));
    let dispatch = SocketDispatchContext::headless(
        worker,
        close_behavior,
        generation.clone(),
        before_tab_removal,
    );
    spawn_socket_dispatch_loop(state, rx, dispatch);

    Some((socket_path, generation))
}

#[cfg(test)]
pub(crate) fn start_tmux_async_test_socket_server(
    state: Rc<RefCell<AppState>>,
    runtime_dir: &Path,
    worker: crate::tmux::TmuxWorker,
    before_tab_removal: Option<BeforeTabRemovalObserver>,
) -> Option<(PathBuf, Rc<Cell<u64>>)> {
    start_headless_dispatch_socket_server(
        state,
        runtime_dir,
        worker,
        crate::config::TmuxCloseBehavior::Close,
        before_tab_removal,
    )
}

/// Start a headless read-only socket server.
///
/// This serves the same read-only socket IPC surface (`query-state`,
/// `query-events`, `list-tabs`, `list-detached`, `dashboard-state`) without any
/// GTK widgets, which makes it suitable for headless smoke tests and future
/// automation that only needs state inspection.
pub fn start_read_only_socket_server(
    state: Rc<RefCell<AppState>>,
    runtime_dir: &Path,
) -> Option<PathBuf> {
    start_state_only_socket_server(state, runtime_dir, handle_read_only_socket_message)
}

/// Start a headless socket server for smoke automation.
///
/// This supports the read-only snapshot surfaces plus `detach-pane` and
/// `attach-session` by mutating `AppState` directly, without requiring GTK
/// widgets or a live tmux server.
#[doc(hidden)]
pub fn start_headless_smoke_socket_server(
    state: Rc<RefCell<AppState>>,
    runtime_dir: &Path,
) -> Option<PathBuf> {
    start_headless_smoke_socket_server_with_close_behavior(
        state,
        runtime_dir,
        crate::config::tmux_config().close_behavior,
    )
}

/// Start the headless smoke socket with an explicit close policy.
///
/// This deterministic variant exists for end-to-end protocol tests; production
/// callers should use [`start_headless_smoke_socket_server`].
#[doc(hidden)]
pub fn start_headless_smoke_socket_server_with_close_behavior(
    state: Rc<RefCell<AppState>>,
    runtime_dir: &Path,
    close_behavior: crate::config::TmuxCloseBehavior,
) -> Option<PathBuf> {
    start_headless_dispatch_socket_server(
        state,
        runtime_dir,
        crate::tmux::default_worker(),
        close_behavior,
        None,
    )
    .map(|(socket_path, _generation)| socket_path)
}

pub(super) fn handle_read_only_socket_message(
    state: &Rc<RefCell<AppState>>,
    msg: SocketMessage,
) -> SocketResponse {
    match msg {
        SocketMessage::ListTabs => handle_list_tabs(state),
        SocketMessage::WorkContext { tab, pane } => {
            handle_work_context_message(state, tab.as_ref(), pane)
        }
        SocketMessage::ListDetached => {
            let st = state.borrow();
            let mut resp = SocketResponse::ok();
            resp.data = Some(serde_json::json!(detached_session_list_payload(&st)));
            resp
        }
        SocketMessage::DashboardState => {
            let st = state.borrow();
            let sessions: Vec<serde_json::Value> = st
                .dashboard_state
                .sessions
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "host": s.host,
                        "status": format!("{:?}", s.status),
                        "command": s.command,
                        "started": s.started,
                        "is_detached": s.is_detached,
                        "is_taarof_managed": s.is_taarof_managed,
                    })
                })
                .collect();
            let hosts: Vec<serde_json::Value> = st
                .dashboard_state
                .hosts
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "name": h.name,
                        "cpu_percent": h.cpu_percent,
                        "memory_percent": h.memory_percent,
                        "session_count": h.session_count,
                    })
                })
                .collect();
            let mut resp = SocketResponse::ok();
            resp.data = Some(serde_json::json!({ "sessions": sessions, "hosts": hosts }));
            resp
        }
        SocketMessage::QueryState => handle_query_state(state),
        SocketMessage::QueryEvents { since_seq, limit } => {
            handle_query_events(state, since_seq, limit)
        }
        SocketMessage::QueryHistory { .. } => {
            SocketResponse::err("query-history must be handled by the socket listener thread")
        }
        _ => SocketResponse::err(
            "headless read-only socket server only supports read-only socket actions",
        ),
    }
}

pub(super) fn handle_headless_smoke_socket_message(
    state: &Rc<RefCell<AppState>>,
    msg: SocketMessage,
) -> SocketResponse {
    match msg {
        SocketMessage::WorkReport {
            milestone,
            note,
            session,
            workspace_origin,
            tab_origin,
            pane_origin,
            task_id,
            checkout_root,
            binding_token,
        } => handle_work_report_message(
            state,
            crate::work_reporting::WorkReport {
                milestone,
                note,
                context: crate::work_reporting::WorkContext {
                    session,
                    workspace_origin,
                    tab_origin,
                    pane_origin,
                    task_id,
                    task_title: String::new(),
                    checkout_root,
                    binding_token,
                    tab_id: 0,
                    pane_id: 0,
                },
            },
        ),
        SocketMessage::DetachPane { tab, pane } => {
            handle_headless_detach_pane(state, tab.as_ref(), pane)
        }
        SocketMessage::AttachSession {
            expected_agent: Some(_),
            ..
        } => SocketResponse::err("Exact live attach requires a running desktop pane."),
        SocketMessage::AttachSession {
            expected_agent: None,
            session_name,
            host,
            ssh_target,
        } => handle_headless_attach_session(
            state,
            &session_name,
            host.as_deref(),
            ssh_target.as_deref(),
        ),
        SocketMessage::CloseTab { .. } => {
            SocketResponse::err("close-tab must use asynchronous headless dispatch")
        }
        _ => handle_read_only_socket_message(state, msg),
    }
}

pub(super) fn detached_session_matches_selector(
    session: &crate::dashboard::DetachedSession,
    session_name: &str,
    host: Option<&str>,
    ssh_target: Option<&str>,
) -> bool {
    if session.session_name != session_name {
        return false;
    }
    if let Some(host) = host {
        if session.host != host {
            return false;
        }
    }
    if let Some(ssh_target) = ssh_target {
        if session.target.ssh_target_string().as_deref() != Some(ssh_target) {
            return false;
        }
    }
    true
}

pub(super) fn resolve_detached_session_for_attach(
    detached_sessions: &[crate::dashboard::DetachedSession],
    session_name: &str,
    host: Option<&str>,
    ssh_target: Option<&str>,
) -> Result<crate::dashboard::DetachedSession, String> {
    let matches: Vec<_> = detached_sessions
        .iter()
        .filter(|session| {
            detached_session_matches_selector(session, session_name, host, ssh_target)
        })
        .cloned()
        .collect();

    match matches.len() {
        0 => Err("Session not found in detached list".to_string()),
        1 => Ok(matches
            .into_iter()
            .next()
            .expect("match count should be one")),
        _ => Err(format!(
            "Detached session '{session_name}' is ambiguous; retry with host or ssh_target"
        )),
    }
}

pub(super) fn detached_session_list_payload(state: &AppState) -> Vec<serde_json::Value> {
    state
        .detached_sessions
        .iter()
        .map(|session| {
            serde_json::json!({
                "session_name": session.session_name,
                "host": session.host,
                "workspace": session.workspace,
                "ssh_target": session.target.ssh_target_string(),
                "finished": session.finished,
                "last_command": session.last_command,
                "is_detached": true,
            })
        })
        .collect()
}

pub(super) fn handle_headless_detach_pane(
    state: &Rc<RefCell<AppState>>,
    tab_target: Option<&String>,
    pane_id: u32,
) -> SocketResponse {
    let tab_id = match resolve_tab_id_for_target(state, tab_target) {
        Ok(tab_id) => tab_id,
        Err(err) => return SocketResponse::err(err),
    };
    match crate::terminal::register_detached_pane(state, tab_id, pane_id) {
        Ok(session_name) => {
            SocketResponse::ok_with_data(serde_json::json!({ "session": session_name }))
        }
        Err(error) => SocketResponse::err(error),
    }
}

pub(super) fn headless_attach_target(state: &AppState, tab: &crate::Tab) -> Option<u32> {
    if tab.kind != crate::workspace::TabKind::Terminal {
        return None;
    }

    if tab.panes.contains_pane(tab.focused_pane_id) {
        return Some(tab.focused_pane_id);
    }

    if state.headless_pane(tab.id, tab.focused_pane_id).is_some() {
        return Some(tab.focused_pane_id);
    }

    tab.panes.leaves().first().map(|leaf| leaf.pane_id)
}

pub(super) fn handle_headless_attach_session(
    state: &Rc<RefCell<AppState>>,
    session_name: &str,
    host: Option<&str>,
    ssh_target: Option<&str>,
) -> SocketResponse {
    let detached = {
        let st = state.borrow();
        resolve_detached_session_for_attach(&st.detached_sessions, session_name, host, ssh_target)
    };
    let detached = match detached {
        Ok(detached) => detached,
        Err(err) => return SocketResponse::err(err),
    };

    let mut st = state.borrow_mut();
    let (target_tab_id, target_pane_id) = {
        let Some(workspace) = st.active_ws() else {
            return SocketResponse::err("no active workspace");
        };
        let Some((target_tab_id, target_pane_id)) = workspace
            .tabs
            .iter()
            .find_map(|tab| headless_attach_target(&st, tab).map(|pane_id| (tab.id, pane_id)))
        else {
            return SocketResponse::err("no terminal tab available in active workspace");
        };
        (target_tab_id, target_pane_id)
    };

    {
        let backing = crate::pane::TmuxBacking {
            session_name: detached.session_name.clone(),
            target: detached.target.clone(),
            pane_info: crate::probe::ProbeSnapshot::default(),
        };

        // Verify pane exists (and apply backing) BEFORE mutating active_tab,
        // so a pane-not-found error does not leave the UI pointing at the
        // target tab with no attached pane.
        let has_leaf = {
            let Some(workspace) = st.active_ws_mut() else {
                return SocketResponse::err("no active workspace");
            };
            let Some(tab) = workspace
                .tabs
                .iter_mut()
                .find(|tab| tab.id == target_tab_id)
            else {
                return SocketResponse::err("tab not found");
            };
            if let Some(leaf) = tab.panes.leaf_mut(target_pane_id) {
                leaf.tmux_backing = Some(backing.clone());
                true
            } else {
                false
            }
        };

        if !has_leaf {
            if let Some(headless) = st.headless_pane_mut(target_tab_id, target_pane_id) {
                headless.tmux_backing = Some(backing.clone());
            } else {
                return SocketResponse::err("pane not found");
            }
        }

        // Only commit the active-tab change after the pane has been located.
        if let Some(workspace) = st.active_ws_mut() {
            workspace.active_tab = target_tab_id;
        }
    }
    st.detached_sessions
        .retain(|session| !session.matches_target(session_name, &detached.target));
    st.invalidate_dashboard_session_snapshot(&detached.target, session_name);

    SocketResponse::ok_with_data(serde_json::json!({
        "session": session_name,
        "tab_id": target_tab_id,
        "pane_id": target_pane_id,
    }))
}
