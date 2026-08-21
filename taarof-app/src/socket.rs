use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::os::unix::{
    fs::{MetadataExt, PermissionsExt},
    net::UnixListener,
};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc as tokio_mpsc;
use vte::prelude::*;

use crate::{
    agents, instance,
    pane::SplitDirection,
    sidebar, terminal,
    workspace::{AgentActivity, AgentActivityState, TabKind, DONE_ACTIVITY_VISIBILITY},
    AppState, RuntimeHandle,
};

mod headless;
mod pairing_channel;
mod protocol;
mod registry;
mod runtime_dir;

pub use self::pairing_channel::PairingCoordinator;

pub use self::headless::{
    start_headless_smoke_socket_server, start_headless_smoke_socket_server_with_close_behavior,
    start_read_only_socket_server,
};

use self::headless::detached_session_list_payload;
use self::headless::resolve_detached_session_for_attach;
use self::protocol::{
    bridge_queue_error, socket_message_action, socket_message_is_read_only, SocketActivityState,
    SocketMessage, SocketResponse, SOCKET_MAX_CAPTURE_SCROLLBACK_LINES,
};
use self::registry::{
    cleanup_socket_registry, cleanup_stale_socket_registry_in, write_socket_registry,
};

/// Republish runtime identity into the discovery registry. Called from the
/// update-watch blocking worker (EXAMPLE-164), never from a GTK callback.
pub(crate) use self::registry::refresh_registry_identity;
use self::runtime_dir::{runtime_dir_resolution, socket_path_in, validate_socket_path_length};

const SOCKET_MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

pub(crate) type BeforeTabRemovalObserver = Rc<dyn Fn(&AppState, u32)>;

/// Dependencies for the production socket dispatcher. GTK and headless smoke
/// surfaces share the same command preparation, tmux worker, completion guard,
/// cleanup ordering, and response path; only their final UI/state apply differs.
#[derive(Clone)]
enum SocketDispatchContext {
    Gtk {
        term_stack: gtk::Stack,
        tab_list: gtk::Box,
        window: adw::ApplicationWindow,
        worker: crate::tmux::TmuxWorker,
    },
    Headless {
        worker: crate::tmux::TmuxWorker,
        close_behavior: crate::config::TmuxCloseBehavior,
        generation: Rc<Cell<u64>>,
        before_tab_removal: Option<BeforeTabRemovalObserver>,
    },
}

impl SocketDispatchContext {
    fn gtk(term_stack: gtk::Stack, tab_list: gtk::Box, window: adw::ApplicationWindow) -> Self {
        Self::Gtk {
            term_stack,
            tab_list,
            window,
            worker: crate::tmux::default_worker(),
        }
    }

    fn headless(
        worker: crate::tmux::TmuxWorker,
        close_behavior: crate::config::TmuxCloseBehavior,
        generation: Rc<Cell<u64>>,
        before_tab_removal: Option<BeforeTabRemovalObserver>,
    ) -> Self {
        Self::Headless {
            worker,
            close_behavior,
            generation,
            before_tab_removal,
        }
    }

    fn worker(&self) -> crate::tmux::TmuxWorker {
        match self {
            Self::Gtk { worker, .. } | Self::Headless { worker, .. } => worker.clone(),
        }
    }

    fn close_behavior(&self) -> crate::config::TmuxCloseBehavior {
        match self {
            Self::Gtk { .. } => crate::config::tmux_config().close_behavior,
            Self::Headless { close_behavior, .. } => *close_behavior,
        }
    }
}

/// Agent-workspace commands retain their synchronous socket contract, while
/// their Git phase runs off the GTK thread.  Keep the number of distinct
/// outstanding worktree routes bounded as well as the shared Git worker gate.
const MAX_AGENT_WORKSPACE_OPERATIONS: usize = crate::git::MAX_GIT_ASYNC_IN_FLIGHT;
const DEFAULT_AGENT_WORKSPACE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_AGENT_WORKSPACE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[cfg(test)]
static AGENT_WORKSPACE_TEST_DELAY_MS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static AGENT_WORKSPACE_TEST_DELAY_STARTED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static AGENT_WORKSPACE_TEST_DELAY_FINISHED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static AGENT_WORKSPACE_TEST_WORKTREE_CREATED: AtomicBool = AtomicBool::new(false);

fn delay_agent_workspace_worker_for_test() {
    #[cfg(test)]
    {
        let delay_ms = AGENT_WORKSPACE_TEST_DELAY_MS.load(Ordering::SeqCst);
        if delay_ms > 0 {
            AGENT_WORKSPACE_TEST_DELAY_STARTED.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(delay_ms));
            AGENT_WORKSPACE_TEST_DELAY_FINISHED.store(true, Ordering::SeqCst);
        }
    }
}

struct AgentWorkspaceSubscriber {
    id: u64,
    source_workspace: Option<(u32, String)>,
    command: Option<String>,
    response_tx: mpsc::Sender<SocketResponse>,
}

#[derive(Clone)]
struct AgentWorkspaceDispatchContext {
    state: Rc<RefCell<AppState>>,
    term_stack: Option<gtk::Stack>,
    tab_list: Option<gtk::Box>,
    window: Option<adw::ApplicationWindow>,
}

thread_local! {
    static AGENT_WORKSPACE_SUBSCRIBERS: RefCell<HashMap<String, Vec<AgentWorkspaceSubscriber>>> = RefCell::new(HashMap::new());
    static NEXT_AGENT_WORKSPACE_SUBSCRIBER_ID: Cell<u64> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) use self::headless::start_tmux_async_test_socket_server;
#[cfg(test)]
use self::registry::{
    cleanup_stale_socket_registry_file, process_exists, registry_path_in, SocketRegistry,
};
#[cfg(test)]
use self::runtime_dir::{
    select_runtime_dir, validate_runtime_dir, validate_runtime_dir_attributes, RuntimeDirIssue,
};

/// Start the Unix socket notification server.
/// Binds the socket on the main thread (so errors are visible), then spawns
/// a listener thread. Returns the socket path for cleanup.
pub fn start_notify_server(
    state: Rc<RefCell<AppState>>,
    term_stack: gtk::Stack,
    tab_list: gtk::Box,
    window: adw::ApplicationWindow,
) -> Option<PathBuf> {
    const SOCKET_REQUEST_QUEUE_CAPACITY: usize = 256;

    let resolution = runtime_dir_resolution();
    for warning in &resolution.warnings {
        eprintln!("{warning}");
    }

    let runtime_dir = match resolution.dir {
        Some(path) => path,
        None => {
            eprintln!("taarof: socket server disabled: no safe runtime dir available");
            return None;
        }
    };

    let (socket_path, listener) = bind_socket_listener(&runtime_dir)?;

    let (tx, rx) = tokio_mpsc::channel::<(SocketMessage, mpsc::Sender<SocketResponse>)>(
        SOCKET_REQUEST_QUEUE_CAPACITY,
    );

    let history_reader = state.borrow().history_reader.clone();
    spawn_socket_listener_thread(listener, tx, history_reader);

    let dispatch = SocketDispatchContext::gtk(term_stack, tab_list, window);
    spawn_socket_dispatch_loop(state, rx, dispatch);

    Some(socket_path)
}

fn spawn_socket_dispatch_loop(
    state: Rc<RefCell<AppState>>,
    rx: tokio_mpsc::Receiver<(SocketMessage, mpsc::Sender<SocketResponse>)>,
    dispatch: SocketDispatchContext,
) {
    spawn_socket_dispatch_router(
        &glib::MainContext::default(),
        rx,
        move |msg, response_tx| {
            dispatch_socket_message(&state, &dispatch, msg, response_tx, None);
        },
        None,
    );
}

/// Run the main-context receive seam for socket requests. The listener only
/// parses and queues requests; this loop owns the yield between them so a
/// long-running request cannot starve GLib sources. Tests drive the same seam
/// with a narrower dispatcher, and `finished` lets them observe shutdown
/// without inventing their own receive loop.
fn spawn_socket_dispatch_router<D>(
    main_context: &glib::MainContext,
    rx: tokio_mpsc::Receiver<(SocketMessage, mpsc::Sender<SocketResponse>)>,
    dispatch_message: D,
    finished: Option<Rc<Cell<bool>>>,
) where
    D: Fn(SocketMessage, mpsc::Sender<SocketResponse>) + 'static,
{
    main_context.spawn_local(async move {
        let mut rx = rx;
        while let Some((msg, response_tx)) = rx.recv().await {
            dispatch_message(msg, response_tx);
            glib::timeout_future(Duration::from_millis(0)).await;
        }
        if let Some(finished) = finished {
            finished.set(true);
        }
    });
}

/// Dispatch operations which may wait on tmux/SSH without holding the socket
/// queue on GTK. The listener's per-request reply stays pending while the main
/// context remains free to serve read-only/control requests.
fn dispatch_socket_message(
    state: &Rc<RefCell<AppState>>,
    dispatch: &SocketDispatchContext,
    msg: SocketMessage,
    response_tx: mpsc::Sender<SocketResponse>,
    control_guard: Option<crate::http::HttpControlRequestGuard>,
) {
    if let SocketMessage::CloseTab { tab } = msg {
        record_socket_message_event(state, &SocketMessage::CloseTab { tab: tab.clone() });
        dispatch_close_tab_socket(state, dispatch, &tab, response_tx);
        return;
    }

    let SocketDispatchContext::Gtk {
        term_stack,
        tab_list,
        window,
        ..
    } = dispatch
    else {
        let action = socket_message_action(&msg);
        let response = run_socket_handler_safely(action, || {
            headless::handle_headless_smoke_socket_message(state, msg)
        });
        let _ = response_tx.send(response);
        return;
    };

    let msg = match msg {
        SocketMessage::AttachSession {
            session_name,
            host,
            ssh_target,
        } => {
            record_socket_message_event(
                state,
                &SocketMessage::AttachSession {
                    session_name: session_name.clone(),
                    host: host.clone(),
                    ssh_target: ssh_target.clone(),
                },
            );
            let detached = {
                let st = state.borrow();
                resolve_detached_session_for_attach(
                    &st.detached_sessions,
                    &session_name,
                    host.as_deref(),
                    ssh_target.as_deref(),
                )
            };
            match detached {
                Ok(detached) => crate::terminal::attach_session_async(
                    state,
                    term_stack,
                    tab_list,
                    window,
                    &detached.session_name,
                    &detached.target,
                    move |result| {
                        let response = match result {
                            Ok(()) => SocketResponse::ok(),
                            Err(error) => SocketResponse::err(error.to_string()),
                        };
                        let _ = response_tx.send(response);
                    },
                ),
                Err(error) => {
                    let _ = response_tx.send(SocketResponse::err(error));
                }
            }
            return;
        }
        SocketMessage::ClosePane { tab, pane } => {
            record_socket_message_event(
                state,
                &SocketMessage::ClosePane {
                    tab: tab.clone(),
                    pane,
                },
            );
            dispatch_close_pane_socket(
                state,
                term_stack,
                tab_list,
                tab.as_ref(),
                pane,
                response_tx,
            );
            return;
        }
        SocketMessage::SendKeys { tab, pane, keys } => {
            record_socket_message_event(
                state,
                &SocketMessage::SendKeys {
                    tab: tab.clone(),
                    pane,
                    keys: keys.clone(),
                },
            );
            dispatch_send_keys_socket(
                state,
                window,
                tab.as_ref(),
                pane,
                keys,
                ControlDispatchReply::new(response_tx, control_guard),
            );
            return;
        }
        SocketMessage::ResizePane {
            tab,
            pane,
            cols,
            rows,
        } => {
            record_socket_message_event(
                state,
                &SocketMessage::ResizePane {
                    tab: tab.clone(),
                    pane,
                    cols,
                    rows,
                },
            );
            dispatch_resize_pane_control(
                state,
                window,
                tab.as_ref(),
                pane,
                cols,
                rows,
                ControlDispatchReply::new(response_tx, control_guard),
            );
            return;
        }
        SocketMessage::CreateTmuxTab {
            name,
            host,
            working_dir,
        } => {
            record_socket_message_event(
                state,
                &SocketMessage::CreateTmuxTab {
                    name: name.clone(),
                    host: host.clone(),
                    working_dir: working_dir.clone(),
                },
            );
            dispatch_create_tmux_tab_socket(
                state,
                term_stack,
                tab_list,
                window,
                name,
                host,
                working_dir,
                response_tx,
            );
            return;
        }
        msg => msg,
    };

    if !try_begin_control_apply(control_guard.as_ref()) {
        let _ = response_tx.send(SocketResponse::err("HTTP control request timed out"));
        return;
    }
    // agent-workspace is socket-only, so the guard above is always `None` for
    // it; running the guard first still keeps a future HTTP verb from reaching
    // the Git worker without claiming its apply slot.
    let Some(msg) = route_agent_workspace_socket_message(
        state,
        Some(term_stack),
        Some(tab_list),
        Some(window),
        msg,
        &response_tx,
    ) else {
        return;
    };
    let action = socket_message_action(&msg);
    let response = run_socket_handler_safely(action, || {
        handle_socket_message(state, term_stack, tab_list, window, msg)
    });
    let _ = response_tx.send(response);
}

fn try_begin_control_apply(guard: Option<&crate::http::HttpControlRequestGuard>) -> bool {
    guard.is_none_or(crate::http::HttpControlRequestGuard::try_begin_apply)
}

struct ControlDispatchReply {
    sender: mpsc::Sender<SocketResponse>,
    guard: Option<crate::http::HttpControlRequestGuard>,
}

impl ControlDispatchReply {
    fn new(
        sender: mpsc::Sender<SocketResponse>,
        guard: Option<crate::http::HttpControlRequestGuard>,
    ) -> Self {
        Self { sender, guard }
    }

    fn try_begin_apply(&self) -> bool {
        try_begin_control_apply(self.guard.as_ref())
    }

    fn send(&self, response: SocketResponse) {
        let _ = self.sender.send(response);
    }
}

fn dispatch_close_pane_socket(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    tab_target: Option<&String>,
    pane_id: u32,
    response_tx: mpsc::Sender<SocketResponse>,
) {
    let tab_id = match resolve_tab_id_for_target(state, tab_target) {
        Ok(tab_id) => tab_id,
        Err(error) => {
            let _ = response_tx.send(SocketResponse::err(error));
            return;
        }
    };
    let can_close = state
        .borrow()
        .find_tab(tab_id)
        .is_some_and(|(_, tab)| tab.panes.contains_pane(pane_id) && tab.panes.leaf_count() > 1);
    if !can_close {
        let _ = response_tx.send(SocketResponse::err("pane not found or is the last pane"));
        return;
    }
    let runtime = RuntimeHandle::from_shared_state(state.clone());
    let send_result = response_tx.clone();
    if let Err(error) = terminal::close_pane_async(
        &runtime,
        term_stack,
        Some(tab_list),
        tab_id,
        pane_id,
        move |result| {
            let _ = send_result.send(close_pane_socket_response(result));
        },
    ) {
        let _ = response_tx.send(SocketResponse::err(error));
    }
}

fn strip_prepared_tmux_backings(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    backings: &[crate::pane::TmuxBacking],
) {
    let matches = |backing: &crate::pane::TmuxBacking| {
        backings.iter().any(|expected| {
            expected.session_name == backing.session_name && expected.target == backing.target
        })
    };
    let mut st = state.borrow_mut();
    if let Some(tab) = st.find_tab_mut(tab_id) {
        for leaf in tab.panes.leaves_mut() {
            if leaf.tmux_backing.as_ref().is_some_and(&matches) {
                leaf.tmux_backing = None;
            }
        }
    }
    for ((current_tab_id, _), pane) in &mut st.headless_panes {
        if *current_tab_id == tab_id && pane.tmux_backing.as_ref().is_some_and(&matches) {
            pane.tmux_backing = None;
        }
    }
}

enum SocketTabCloseTarget {
    Gtk {
        state: Rc<RefCell<AppState>>,
        term_stack: gtk::Stack,
        tab_list: gtk::Box,
        window: adw::ApplicationWindow,
    },
    Headless {
        state: Rc<RefCell<AppState>>,
        before_tab_removal: Option<BeforeTabRemovalObserver>,
    },
}

enum PendingSocketTabClose {
    Gtk {
        guard: crate::tmux::TmuxGtkApplyGuard,
        term_stack: gtk::Stack,
        tab_list: gtk::Box,
    },
    Headless {
        guard: crate::tmux::TmuxGtkApplyGuard,
        before_tab_removal: Option<BeforeTabRemovalObserver>,
    },
}

impl PendingSocketTabClose {
    fn upgrade(self, completion: &crate::tmux::TmuxCompletion) -> Option<SocketTabCloseTarget> {
        match self {
            Self::Gtk {
                guard,
                term_stack,
                tab_list,
            } => {
                let (state, window) = guard.upgrade(completion)?;
                Some(SocketTabCloseTarget::Gtk {
                    state,
                    term_stack,
                    tab_list,
                    window,
                })
            }
            Self::Headless {
                guard,
                before_tab_removal,
            } => Some(SocketTabCloseTarget::Headless {
                state: guard.upgrade_state(completion)?,
                before_tab_removal,
            }),
        }
    }
}

fn pending_socket_tab_close(
    state: &Rc<RefCell<AppState>>,
    dispatch: &SocketDispatchContext,
) -> PendingSocketTabClose {
    match dispatch {
        SocketDispatchContext::Gtk {
            term_stack,
            tab_list,
            window,
            ..
        } => PendingSocketTabClose::Gtk {
            guard: crate::tmux::TmuxGtkApplyGuard::new(state, window),
            term_stack: term_stack.clone(),
            tab_list: tab_list.clone(),
        },
        SocketDispatchContext::Headless {
            generation,
            before_tab_removal,
            ..
        } => PendingSocketTabClose::Headless {
            guard: crate::tmux::TmuxGtkApplyGuard::for_generation(state, generation),
            before_tab_removal: before_tab_removal.clone(),
        },
    }
}

fn immediate_socket_tab_close(
    state: &Rc<RefCell<AppState>>,
    dispatch: &SocketDispatchContext,
) -> SocketTabCloseTarget {
    match dispatch {
        SocketDispatchContext::Gtk {
            term_stack,
            tab_list,
            window,
            ..
        } => SocketTabCloseTarget::Gtk {
            state: state.clone(),
            term_stack: term_stack.clone(),
            tab_list: tab_list.clone(),
            window: window.clone(),
        },
        SocketDispatchContext::Headless {
            before_tab_removal, ..
        } => SocketTabCloseTarget::Headless {
            state: state.clone(),
            before_tab_removal: before_tab_removal.clone(),
        },
    }
}

fn finalize_socket_tab_close(
    target: SocketTabCloseTarget,
    tab_id: u32,
    prepared: PreparedTabClose,
    kill_errors: Vec<String>,
) -> SocketResponse {
    let state = match &target {
        SocketTabCloseTarget::Gtk { state, .. } | SocketTabCloseTarget::Headless { state, .. } => {
            state
        }
    };
    strip_prepared_tmux_backings(state, tab_id, &prepared.tmux_backings_to_close);

    if let SocketTabCloseTarget::Headless {
        before_tab_removal: Some(observer),
        ..
    } = &target
    {
        observer(&state.borrow(), tab_id);
    }

    match &target {
        SocketTabCloseTarget::Gtk {
            state,
            term_stack,
            tab_list,
            window,
        } => {
            apply_tab_close_plan(
                prepared.plan,
                || {
                    let _ = sidebar::close_tab_by_id(tab_list, state, term_stack, tab_id);
                },
                |workspace_id| {
                    let _ =
                        sidebar::close_workspace_by_id(tab_list, state, term_stack, workspace_id);
                },
            );
            if !prepared.detached_sessions.is_empty() || !kill_errors.is_empty() {
                sidebar::refresh_background_section(tab_list, state, term_stack, window);
            }
        }
        SocketTabCloseTarget::Headless { state, .. } => {
            apply_tab_close_plan(
                prepared.plan,
                || {
                    state.borrow_mut().remove_tab(tab_id);
                },
                |workspace_id| {
                    state.borrow_mut().remove_workspace(workspace_id);
                },
            );
        }
    }

    if kill_errors.is_empty() {
        SocketResponse::ok()
    } else {
        SocketResponse::err(kill_errors.join("; "))
    }
}

fn dispatch_close_tab_socket(
    state: &Rc<RefCell<AppState>>,
    dispatch: &SocketDispatchContext,
    tab_target: &str,
    response_tx: mpsc::Sender<SocketResponse>,
) {
    let owned_target = tab_target.to_string();
    let tab_id = match resolve_tab_id_for_target(state, Some(&owned_target)) {
        Ok(tab_id) => tab_id,
        Err(error) => {
            let _ = response_tx.send(SocketResponse::err(error));
            return;
        }
    };
    let prepared = match prepare_tab_close(state, tab_id, dispatch.close_behavior()) {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = response_tx.send(SocketResponse::err(error));
            return;
        }
    };
    if prepared.tmux_backings_to_close.is_empty() {
        let response = finalize_socket_tab_close(
            immediate_socket_tab_close(state, dispatch),
            tab_id,
            prepared,
            Vec::new(),
        );
        let _ = response_tx.send(response);
        return;
    }
    let workspace_name = state
        .borrow()
        .find_tab(tab_id)
        .map(|(workspace, _)| workspace.name.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let commands = prepared
        .tmux_backings_to_close
        .iter()
        .map(|backing| crate::tmux::kill_session_command(&backing.target, &backing.session_name))
        .collect();
    let pending_apply = pending_socket_tab_close(state, dispatch);
    let worker = dispatch.worker();
    glib::spawn_future_local(async move {
        let response = match worker
            .submit_coalesced(
                crate::tmux::TmuxJobKey::CloseTab(tab_id),
                commands,
                crate::tmux::TMUX_CLOSE_DEADLINE,
            )
            .await
        {
            Ok(completion) => match pending_apply.upgrade(&completion) {
                Some(target)
                    if {
                        let state = match &target {
                            SocketTabCloseTarget::Gtk { state, .. }
                            | SocketTabCloseTarget::Headless { state, .. } => state,
                        };
                        state.borrow().find_tab(tab_id).is_some()
                    } =>
                {
                    let state = match &target {
                        SocketTabCloseTarget::Gtk { state, .. }
                        | SocketTabCloseTarget::Headless { state, .. } => state,
                    };
                    let errors = crate::terminal::apply_tmux_kill_outcomes_before_removal(
                        state,
                        &prepared.tmux_backings_to_close,
                        completion.outcomes(),
                        &workspace_name,
                    );
                    finalize_socket_tab_close(target, tab_id, prepared, errors)
                }
                _ => SocketResponse::err("tab close was superseded"),
            },
            Err(error) => SocketResponse::err(error),
        };
        let _ = response_tx.send(response);
    });
}

fn dispatch_send_keys_socket(
    state: &Rc<RefCell<AppState>>,
    window: &adw::ApplicationWindow,
    tab_target: Option<&String>,
    pane_id: u32,
    keys: String,
    reply: ControlDispatchReply,
) {
    let tab_id = match resolve_tab_id_for_target(state, tab_target) {
        Ok(tab_id) => tab_id,
        Err(error) => {
            reply.send(SocketResponse::err(error));
            return;
        }
    };
    let target = state
        .borrow()
        .find_tab(tab_id)
        .and_then(|_| crate::terminal::resolve_pane_send_target(&state.borrow(), tab_id, pane_id));
    match target {
        Some(crate::terminal::PaneSendTarget::Vte(terminal)) => {
            if !reply.try_begin_apply() {
                reply.send(SocketResponse::err("HTTP control request timed out"));
                return;
            }
            terminal.feed_child(keys.as_bytes());
            reply.send(SocketResponse::ok());
        }
        Some(crate::terminal::PaneSendTarget::Tmux(backing)) => {
            if !reply.try_begin_apply() {
                reply.send(SocketResponse::err("HTTP control request timed out"));
                return;
            }
            let guard = crate::tmux::TmuxGtkApplyGuard::new(state, window);
            let worker = crate::tmux::default_worker();
            glib::spawn_future_local(async move {
                let response = match worker
                    .submit(
                        vec![crate::tmux::send_keys_command(
                            &backing.target,
                            &backing.session_name,
                            &keys,
                        )],
                        crate::tmux::TMUX_CONTROL_DEADLINE,
                    )
                    .await
                {
                    Ok(completion) if guard.upgrade(&completion).is_some() => completion
                        .outcomes()
                        .first()
                        .and_then(|outcome| outcome.result().err())
                        .map_or_else(SocketResponse::ok, |error| {
                            SocketResponse::err(format!("tmux send-keys failed: {error}"))
                        }),
                    Ok(_) => SocketResponse::err("send-keys was superseded"),
                    Err(error) => SocketResponse::err(error),
                };
                reply.send(response);
            });
        }
        None => {
            reply.send(SocketResponse::err("target pane no longer exists"));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_create_tmux_tab_socket(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    name: Option<String>,
    host_name: Option<String>,
    working_dir: Option<String>,
    response_tx: mpsc::Sender<SocketResponse>,
) {
    // Resolves through the shared seam so CLI/agents reach `~/.ssh/config`
    // aliases as well as `[hosts.*]` entries; unknown names still error.
    let host_config = match host_name.as_deref() {
        Some(host) => match crate::ssh_config::resolve(host) {
            Ok(config) => Some(config),
            Err(error) => {
                let _ = response_tx.send(SocketResponse::err(error));
                return;
            }
        },
        None => None,
    };
    let target = host_config
        .as_ref()
        .and_then(|host| host.ssh_target.clone())
        .map_or(crate::tmux::TmuxTarget::Local, |ssh_target| {
            crate::tmux::TmuxTarget::Remote { ssh_target }
        });
    let guard = crate::tmux::TmuxGtkApplyGuard::new(state, window);
    let worker = crate::tmux::default_worker();
    let term_stack = term_stack.clone();
    let tab_list = tab_list.clone();
    glib::spawn_future_local(async move {
        let response = match worker
            .submit_coalesced(
                crate::tmux::TmuxJobKey::ValidateTarget(target.clone()),
                vec![crate::tmux::version_command(&target)],
                crate::tmux::TMUX_CONTROL_DEADLINE,
            )
            .await
        {
            Ok(completion) => match guard.upgrade(&completion) {
                Some((state, window)) => match completion.outcomes().first() {
                    Some(outcome) if outcome.success => {
                        let runtime = RuntimeHandle::from_shared_state(state);
                        let tab_id = terminal::open_explicit_tmux_tab_after_validation(
                            &runtime,
                            &term_stack,
                            &tab_list,
                            &window,
                            name.as_deref().unwrap_or("tmux"),
                            working_dir.as_deref(),
                            host_config.as_ref(),
                        );
                        SocketResponse::ok_with_tab(tab_id)
                    }
                    _ => SocketResponse::err("tmux target is unavailable"),
                },
                None => SocketResponse::err("create-tmux-tab was superseded"),
            },
            Err(error) => SocketResponse::err(error),
        };
        let _ = response_tx.send(response);
    });
}

fn dispatch_resize_pane_control(
    state: &Rc<RefCell<AppState>>,
    window: &adw::ApplicationWindow,
    tab_target: Option<&String>,
    pane_id: u32,
    cols: u32,
    rows: u32,
    reply: ControlDispatchReply,
) {
    let tab_id = match resolve_tab_id_for_target(state, tab_target) {
        Ok(tab_id) => tab_id,
        Err(error) => {
            reply.send(SocketResponse::err(error));
            return;
        }
    };
    let (target, pane_dirty) = {
        let st = state.borrow();
        let Some((_, tab)) = st.find_tab(tab_id) else {
            reply.send(SocketResponse::err("tab not found"));
            return;
        };
        let target = if let Some(leaf) = tab
            .panes
            .leaves()
            .into_iter()
            .find(|leaf| leaf.pane_id == pane_id)
        {
            leaf.tmux_backing
                .clone()
                .map(PaneControlTarget::Tmux)
                .unwrap_or_else(|| PaneControlTarget::Vte(leaf.terminal.clone()))
        } else if let Some(backing) = st
            .headless_pane(tab_id, pane_id)
            .and_then(|pane| pane.tmux_backing.clone())
        {
            PaneControlTarget::Tmux(backing)
        } else {
            reply.send(SocketResponse::err("pane not found"));
            return;
        };
        (target, st.pane_dirty.clone())
    };

    match target {
        PaneControlTarget::Vte(terminal) => {
            if !reply.try_begin_apply() {
                reply.send(SocketResponse::err("HTTP control request timed out"));
                return;
            }
            terminal.set_size(cols.into(), rows.into());
            pane_dirty.mark_dirty(tab_id, pane_id);
            let mut response = SocketResponse::ok_with_data(serde_json::json!({
                "supported": true,
                "backend": "vte",
                "cols": cols,
                "rows": rows,
            }));
            response.tab_id = Some(tab_id);
            response.pane_id = Some(pane_id);
            reply.send(response);
        }
        PaneControlTarget::Tmux(backing) => {
            if !reply.try_begin_apply() {
                reply.send(SocketResponse::err("HTTP control request timed out"));
                return;
            }
            let guard = crate::tmux::TmuxGtkApplyGuard::new(state, window);
            let worker = crate::tmux::default_worker();
            glib::spawn_future_local(async move {
                let mut response = match worker
                    .submit_coalesced(
                        crate::tmux::TmuxJobKey::ResizePane(
                            backing.target.clone(),
                            backing.session_name.clone(),
                            cols,
                            rows,
                        ),
                        vec![crate::tmux::resize_pane_command(
                            &backing.target,
                            &backing.session_name,
                            cols,
                            rows,
                        )],
                        crate::tmux::TMUX_CONTROL_DEADLINE,
                    )
                    .await
                {
                    Ok(completion) if guard.upgrade(&completion).is_some() => completion
                        .outcomes()
                        .first()
                        .and_then(|outcome| outcome.result().err())
                        .map_or_else(
                            || {
                                pane_dirty.mark_dirty(tab_id, pane_id);
                                SocketResponse::ok_with_data(serde_json::json!({
                                    "supported": true,
                                    "backend": "tmux",
                                    "cols": cols,
                                    "rows": rows,
                                }))
                            },
                            |error| {
                                SocketResponse::err(format!("tmux resize-pane failed: {error}"))
                            },
                        ),
                    Ok(_) => SocketResponse::err("resize-pane was superseded"),
                    Err(error) => SocketResponse::err(error),
                };
                response.tab_id = Some(tab_id);
                response.pane_id = Some(pane_id);
                reply.send(response);
            });
        }
    }
}

/// Clean up the socket file on exit.
pub fn cleanup_socket(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    cleanup_socket_registry(path);
}

fn bind_socket_listener(runtime_dir: &Path) -> Option<(PathBuf, UnixListener)> {
    let socket_path = socket_path_in(runtime_dir);
    if let Err(error) = validate_socket_path_length(&socket_path) {
        eprintln!(
            "taarof: refusing socket path {}: {error}",
            socket_path.display()
        );
        return None;
    }
    cleanup_stale_socket_registry_in(runtime_dir);

    let _ = std::fs::remove_file(&socket_path);

    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!(
                "taarof: failed to bind socket {}: {error}",
                socket_path.display()
            );
            return None;
        }
    };

    // The control socket is a privileged same-user surface, so restrict it to
    // owner-only (0600) explicitly rather than relying on the process umask. If
    // we cannot tighten the permissions, refuse to expose an over-permissive
    // socket: drop the listener and remove the bound file before bailing out.
    if let Err(error) =
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
    {
        eprintln!(
            "taarof: failed to set socket permissions on {}: {error}",
            socket_path.display()
        );
        drop(listener);
        let _ = std::fs::remove_file(&socket_path);
        return None;
    }

    eprintln!("taarof: notify socket at {}", socket_path.display());
    eprintln!(
        "taarof: socket trust model: same-user local clients can control tabs, run commands, send raw keys, and capture pane text"
    );
    write_socket_registry(&socket_path);
    Some((socket_path, listener))
}

fn spawn_socket_listener_thread(
    listener: UnixListener,
    tx: tokio_mpsc::Sender<(SocketMessage, mpsc::Sender<SocketResponse>)>,
    history_reader: crate::history::HistoryReader,
) {
    const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(2);
    const SOCKET_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(conn) => {
                    let tx = tx.clone();
                    let history_reader = history_reader.clone();
                    std::thread::spawn(move || {
                        handle_socket_connection(
                            conn,
                            tx,
                            history_reader,
                            SOCKET_READ_TIMEOUT,
                            SOCKET_RESPONSE_TIMEOUT,
                        );
                    });
                }
                Err(error) => {
                    eprintln!("taarof: socket accept error: {error}");
                }
            }
        }
    });
}

#[cfg(test)]
pub(crate) fn query_history_request_for_tests(
    raw: &str,
    history_reader: &crate::history::HistoryReader,
) -> Result<serde_json::Value, String> {
    let message = serde_json::from_str::<SocketMessage>(raw).map_err(|error| error.to_string())?;
    let SocketMessage::QueryHistory {
        since_id,
        limit,
        filters,
    } = message
    else {
        return Err("expected query-history".to_string());
    };
    let page = history_reader.query(since_id, limit, filters)?;
    serde_json::to_value(page).map_err(|error| error.to_string())
}

fn handle_socket_connection(
    mut conn: std::os::unix::net::UnixStream,
    tx: tokio_mpsc::Sender<(SocketMessage, mpsc::Sender<SocketResponse>)>,
    history_reader: crate::history::HistoryReader,
    read_timeout: Duration,
    response_timeout: Duration,
) {
    if let Err(error) = conn.set_read_timeout(Some(read_timeout)) {
        let _ = write_json_response(
            &mut conn,
            SocketResponse::err(format!("failed to configure socket read timeout: {error}")),
        );
        return;
    }

    let buf = match read_socket_request(&mut conn) {
        Ok(buf) => buf,
        Err(response) => {
            let _ = write_json_response(&mut conn, response);
            return;
        }
    };

    match serde_json::from_str::<SocketMessage>(&buf) {
        Ok(msg) => {
            if let SocketMessage::QueryHistory {
                since_id,
                limit,
                filters,
            } = msg
            {
                let response = history_reader
                    .query(since_id, limit, filters)
                    .and_then(|page| serde_json::to_value(page).map_err(|error| error.to_string()))
                    .map(SocketResponse::ok_with_data)
                    .unwrap_or_else(SocketResponse::err);
                let _ = write_json_response(&mut conn, response);
                return;
            }
            let read_only = socket_message_is_read_only(&msg);
            let (response_tx, response_rx) = mpsc::channel::<SocketResponse>();
            if let Err(error) = tx.try_send((msg, response_tx)) {
                let _ = write_json_response(&mut conn, bridge_queue_error(error));
                return;
            }
            let response = if read_only {
                response_rx
                    .recv_timeout(response_timeout)
                    .unwrap_or_else(|_| {
                        SocketResponse::err("timeout waiting for runtime query response")
                    })
            } else {
                response_rx
                    .recv()
                    .unwrap_or_else(|_| SocketResponse::err("runtime command channel closed"))
            };
            let _ = write_json_response(&mut conn, response);
        }
        Err(error) => {
            eprintln!("taarof: invalid notify message: {error}");
            let _ = write_json_response(
                &mut conn,
                SocketResponse::err(format!("invalid JSON message: {error}")),
            );
        }
    }
}

fn read_socket_request(
    conn: &mut std::os::unix::net::UnixStream,
) -> Result<String, SocketResponse> {
    let mut bytes = Vec::new();
    let read_limit = (SOCKET_MAX_REQUEST_BYTES + 1) as u64;
    match Read::by_ref(conn).take(read_limit).read_to_end(&mut bytes) {
        Ok(_) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            return Err(SocketResponse::err("timeout reading request"));
        }
        Err(error) => {
            return Err(SocketResponse::err(format!(
                "failed to read request: {error}"
            )));
        }
    }

    if bytes.len() > SOCKET_MAX_REQUEST_BYTES {
        return Err(SocketResponse::err(format!(
            "request too large: max {SOCKET_MAX_REQUEST_BYTES} bytes"
        )));
    }

    String::from_utf8(bytes)
        .map_err(|error| SocketResponse::err(format!("invalid UTF-8 request: {error}")))
}

fn resolve_tab_id_for_target_in_state(
    state: &AppState,
    target: Option<&str>,
) -> Result<u32, String> {
    let Some(target) = target else {
        return state
            .active_tab()
            .map(|tab| tab.id)
            .ok_or_else(|| "tab not found".to_string());
    };

    if target.eq_ignore_ascii_case("current") {
        return state
            .active_tab()
            .map(|tab| tab.id)
            .ok_or_else(|| "tab not found".to_string());
    }

    if let Ok(id) = target.parse::<u32>() {
        return state
            .all_tabs()
            .find(|tab| tab.id == id)
            .map(|tab| tab.id)
            .ok_or_else(|| "tab not found".to_string());
    }

    let matches: Vec<(u32, &str)> = state
        .workspaces
        .iter()
        .flat_map(|workspace| {
            workspace
                .tabs
                .iter()
                .filter(|tab| tab.name.eq_ignore_ascii_case(target))
                .map(move |tab| (tab.id, workspace.name.as_str()))
        })
        .collect();

    match matches.as_slice() {
        [] => Err("tab not found".to_string()),
        [(tab_id, _workspace_name)] => Ok(*tab_id),
        _ => {
            let details = matches
                .iter()
                .map(|(tab_id, workspace_name)| {
                    format!("tab_id {tab_id} (workspace \"{workspace_name}\")")
                })
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "ambiguous tab target \"{target}\": matched {details}; use numeric tab_id from list-tabs"
            ))
        }
    }
}

fn resolve_tab_id_for_target(
    state: &Rc<RefCell<AppState>>,
    target: Option<&String>,
) -> Result<u32, String> {
    let st = state.borrow();
    resolve_tab_id_for_target_in_state(&st, target.map(String::as_str))
}

/// Resolve a `switch-workspace` / `rename-workspace` target to a workspace id.
///
/// Resolution is deterministic: a numeric target is matched against workspace
/// ids first, otherwise the target is matched case-insensitively against
/// workspace names. A name that matches multiple workspaces is rejected with a
/// message pointing at the numeric ids, mirroring how `resolve_tab_id_for_target`
/// disambiguates duplicate tab names.
fn resolve_workspace_id_for_target_in_state(state: &AppState, target: &str) -> Result<u32, String> {
    if let Ok(id) = target.parse::<u32>() {
        return state
            .workspaces
            .iter()
            .find(|ws| ws.id == id)
            .map(|ws| ws.id)
            .ok_or_else(|| "workspace not found".to_string());
    }

    let matches: Vec<u32> = state
        .workspaces
        .iter()
        .filter(|ws| ws.name.eq_ignore_ascii_case(target))
        .map(|ws| ws.id)
        .collect();

    match matches.as_slice() {
        [] => Err("workspace not found".to_string()),
        [ws_id] => Ok(*ws_id),
        _ => {
            let details = matches
                .iter()
                .map(|ws_id| format!("workspace_id {ws_id}"))
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "ambiguous workspace target \"{target}\": matched {details}; use the numeric workspace id"
            ))
        }
    }
}

fn resolve_workspace_id_for_target(
    state: &Rc<RefCell<AppState>>,
    target: &str,
) -> Result<u32, String> {
    let st = state.borrow();
    resolve_workspace_id_for_target_in_state(&st, target)
}

fn run_socket_handler_safely(
    action: &'static str,
    handler: impl FnOnce() -> SocketResponse,
) -> SocketResponse {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(handler)) {
        Ok(response) => response,
        Err(_) => {
            eprintln!(
                "taarof: socket request handler panicked while handling {action}; keeping socket bridge alive"
            );
            SocketResponse::err(format!(
                "runtime request handler panicked while handling {action}"
            ))
        }
    }
}

fn http_control_action_name(action: &crate::http::HttpControlAction) -> &'static str {
    match action {
        crate::http::HttpControlAction::SendKeys { .. } => "send-keys",
        crate::http::HttpControlAction::RunInPane { .. } => "run-in-pane",
        crate::http::HttpControlAction::ResizePane { .. } => "resize-pane",
        crate::http::HttpControlAction::SwitchTab { .. } => "switch-tab",
        crate::http::HttpControlAction::CreateTab { .. } => "create-tab",
        crate::http::HttpControlAction::SplitPane { .. } => "split-pane",
    }
}

fn http_control_action_to_socket_message(action: crate::http::HttpControlAction) -> SocketMessage {
    match action {
        crate::http::HttpControlAction::SendKeys { tab, pane, keys } => {
            SocketMessage::SendKeys { tab, pane, keys }
        }
        crate::http::HttpControlAction::RunInPane { tab, pane, command } => {
            SocketMessage::RunInPane { tab, pane, command }
        }
        crate::http::HttpControlAction::ResizePane {
            tab,
            pane,
            cols,
            rows,
        } => SocketMessage::ResizePane {
            tab,
            pane,
            cols,
            rows,
        },
        crate::http::HttpControlAction::SwitchTab { tab } => SocketMessage::SwitchTab { tab },
        crate::http::HttpControlAction::CreateTab {
            name,
            working_dir,
            command,
        } => SocketMessage::CreateTab {
            name,
            working_dir,
            command,
        },
        crate::http::HttpControlAction::SplitPane {
            tab,
            direction,
            command,
            working_dir,
        } => SocketMessage::SplitPane {
            tab,
            direction,
            command,
            working_dir,
        },
    }
}

fn record_http_control_action_event(
    state: &Rc<RefCell<AppState>>,
    action: &crate::http::HttpControlAction,
    response: &SocketResponse,
) {
    let mut payload = serde_json::json!({
        "action": http_control_action_name(action),
    });

    if let Some(map) = payload.as_object_mut() {
        match action {
            crate::http::HttpControlAction::SendKeys { tab, pane, .. }
            | crate::http::HttpControlAction::RunInPane { tab, pane, .. }
            | crate::http::HttpControlAction::ResizePane { tab, pane, .. } => {
                if let Some(tab) = tab {
                    map.insert("tab_target".to_string(), serde_json::json!(tab));
                }
                map.insert("pane_id".to_string(), serde_json::json!(pane));
            }
            crate::http::HttpControlAction::SwitchTab { tab } => {
                map.insert("tab_target".to_string(), serde_json::json!(tab));
            }
            crate::http::HttpControlAction::CreateTab { .. } => {}
            crate::http::HttpControlAction::SplitPane { tab, direction, .. } => {
                if let Some(tab) = tab {
                    map.insert("tab_target".to_string(), serde_json::json!(tab));
                }
                if let Some(direction) = direction {
                    map.insert("direction".to_string(), serde_json::json!(direction));
                }
            }
        }

        if let crate::http::HttpControlAction::ResizePane { cols, rows, .. } = action {
            map.insert("cols".to_string(), serde_json::json!(cols));
            map.insert("rows".to_string(), serde_json::json!(rows));
        }

        if let Some(tab_id) = response.tab_id {
            map.insert("response_tab_id".to_string(), serde_json::json!(tab_id));
        }
        if let Some(pane_id) = response.pane_id {
            map.insert("response_pane_id".to_string(), serde_json::json!(pane_id));
        }
    }

    RuntimeHandle::from_shared_state(state.clone()).emit_event("http_control_action", payload);
}

pub(crate) fn handle_http_control_action_async_with_callback(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    action: crate::http::HttpControlAction,
    control_guard: crate::http::HttpControlRequestGuard,
    callback: impl FnOnce(Result<serde_json::Value, String>) + 'static,
) {
    let action_name = http_control_action_name(&action);
    let callback = Rc::new(RefCell::new(
        Some(Box::new(callback) as HttpControlCallback),
    ));
    let panic_callback = callback.clone();
    let panic_guard = control_guard.clone();
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_http_control_action_inner(
            state,
            term_stack,
            tab_list,
            window,
            action,
            control_guard,
            callback,
        );
    }))
    .is_err()
    {
        eprintln!(
            "taarof: HTTP control handler panicked while handling {action_name}; keeping HTTP bridge alive"
        );
        panic_guard.finish();
        complete_http_control_callback(
            &panic_callback,
            Err(format!(
                "runtime request handler panicked while handling {action_name}"
            )),
        );
    }
}

type HttpControlCallback = Box<dyn FnOnce(Result<serde_json::Value, String>)>;

fn complete_http_control_callback(
    callback: &Rc<RefCell<Option<HttpControlCallback>>>,
    result: Result<serde_json::Value, String>,
) {
    let callback = callback.borrow_mut().take();
    if let Some(callback) = callback {
        callback(result);
    }
}

fn handle_http_control_action_inner(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    action: crate::http::HttpControlAction,
    control_guard: crate::http::HttpControlRequestGuard,
    callback: Rc<RefCell<Option<HttpControlCallback>>>,
) {
    let dispatch = SocketDispatchContext::gtk(term_stack.clone(), tab_list.clone(), window.clone());
    let (response_tx, response_rx) = mpsc::channel();
    dispatch_socket_message(
        state,
        &dispatch,
        http_control_action_to_socket_message(action.clone()),
        response_tx,
        Some(control_guard.clone()),
    );
    let state = state.clone();
    glib::spawn_future_local(async move {
        let response = gio::spawn_blocking(move || response_rx.recv())
            .await
            .map_err(|error| format!("control reply task failed: {error:?}"))
            .and_then(|response| response.map_err(|_| "control reply channel closed".to_string()));
        let result =
            response.and_then(|response| finish_http_control_response(&state, &action, response));
        control_guard.finish();
        complete_http_control_callback(&callback, result);
    });
}

fn finish_http_control_response(
    state: &Rc<RefCell<AppState>>,
    action: &crate::http::HttpControlAction,
    response: SocketResponse,
) -> Result<serde_json::Value, String> {
    if response.ok {
        record_http_control_action_event(state, action, &response);
        serde_json::to_value(response)
            .map_err(|error| format!("failed to serialize HTTP control response: {error}"))
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "control action failed".to_string()))
    }
}

enum PaneControlTarget {
    Vte(vte::Terminal),
    Tmux(crate::pane::TmuxBacking),
}

fn record_socket_message_event(state: &Rc<RefCell<AppState>>, msg: &SocketMessage) {
    if matches!(
        msg,
        SocketMessage::QueryState
            | SocketMessage::QueryEvents { .. }
            | SocketMessage::QueryHistory { .. }
            | SocketMessage::QueryAgentSessions
            | SocketMessage::ListTabs
    ) {
        return;
    }

    RuntimeHandle::from_shared_state(state.clone()).emit_event(
        "socket_message_received",
        serde_json::json!({
            "action": socket_message_action(msg),
        }),
    );
}

fn handle_socket_message(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    msg: SocketMessage,
) -> SocketResponse {
    record_socket_message_event(state, &msg);

    match msg {
        SocketMessage::Notify { tab, message } => {
            handle_notify_message(state, term_stack, tab_list, tab.as_ref(), message.as_ref())
        }
        SocketMessage::AgentStatus {
            tab,
            pane,
            state: activity_state,
            text,
            source,
        } => handle_agent_status_message(
            state,
            term_stack,
            tab_list,
            AgentStatusInput {
                tab_target: tab.as_ref(),
                pane_target: pane,
                activity_state,
                text: text.as_deref(),
                source: source.as_deref(),
            },
        ),
        SocketMessage::WorkContext { tab, pane } => {
            handle_work_context_message(state, tab.as_ref(), pane)
        }
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
        SocketMessage::OpenPane {
            tab,
            command,
            direction,
            working_dir,
        } => handle_open_pane(
            state,
            term_stack,
            tab_list,
            window,
            tab.as_ref(),
            &command,
            direction.as_deref(),
            working_dir.as_deref(),
        ),
        SocketMessage::RunInPane { tab, pane, command } => {
            handle_run_in_pane(state, tab.as_ref(), pane, &command)
        }
        SocketMessage::ClosePane { .. } => {
            SocketResponse::err("close-pane must use asynchronous socket dispatch")
        }
        // ── F010: Full scripting API ──
        SocketMessage::CreateTab {
            name,
            working_dir,
            command,
        } => handle_create_tab(
            state,
            term_stack,
            tab_list,
            window,
            name.as_deref(),
            working_dir.as_deref(),
            command.as_deref(),
        ),
        SocketMessage::CloseTab { .. } => {
            SocketResponse::err("close-tab must use asynchronous socket dispatch")
        }
        SocketMessage::SwitchTab { tab } => handle_switch_tab(state, term_stack, tab_list, &tab),
        SocketMessage::RenameTab { tab, name } => handle_rename_tab(state, tab_list, &tab, &name),
        SocketMessage::ReorderTab { tab, index } => {
            handle_reorder_tab(state, tab_list, &tab, index)
        }
        SocketMessage::CreateWorkspace { name } => {
            handle_create_workspace(state, term_stack, tab_list, window, name.as_deref())
        }
        SocketMessage::SwitchWorkspace { workspace } => {
            handle_switch_workspace(state, term_stack, tab_list, &workspace)
        }
        SocketMessage::RenameWorkspace { workspace, name } => {
            handle_rename_workspace(state, tab_list, &workspace, &name)
        }
        SocketMessage::ListTabs => handle_list_tabs(state),
        SocketMessage::SendKeys { .. } => {
            SocketResponse::err("send-keys must use asynchronous socket dispatch")
        }
        SocketMessage::ResizePane { .. } => {
            SocketResponse::err("resize-pane must use asynchronous socket dispatch")
        }
        SocketMessage::SplitPane {
            tab,
            direction,
            command,
            working_dir,
        } => handle_split_pane(
            state,
            term_stack,
            tab_list,
            window,
            tab.as_ref(),
            direction.as_deref(),
            command.as_deref(),
            working_dir.as_deref(),
        ),
        SocketMessage::GetText {
            tab,
            pane,
            scrollback,
            logical,
        } => handle_get_text(
            state,
            tab.as_ref(),
            pane,
            scrollback.unwrap_or(0),
            logical.unwrap_or(true),
        ),
        SocketMessage::CreateTmuxTab {
            name: _,
            host: _,
            working_dir: _,
        } => SocketResponse::err("create-tmux-tab must use asynchronous socket dispatch"),
        SocketMessage::OpenDashboard => {
            if let Some(tab_id) =
                crate::dashboard::open_dashboard(state, term_stack, tab_list, window)
            {
                SocketResponse::ok_with_tab(tab_id)
            } else {
                SocketResponse::ok()
            }
        }
        SocketMessage::DetachPane { tab, pane } => {
            match resolve_tab_id_for_target(state, tab.as_ref()) {
                Ok(tab_id) => {
                    match crate::terminal::detach_pane(state, term_stack, tab_list, tab_id, pane) {
                        Ok(name) => {
                            sidebar::refresh_background_section(
                                tab_list, state, term_stack, window,
                            );
                            let mut resp = SocketResponse::ok();
                            resp.data = Some(serde_json::json!({ "session": name }));
                            resp
                        }
                        Err(e) => SocketResponse::err(e),
                    }
                }
                Err(err) => SocketResponse::err(err),
            }
        }
        SocketMessage::AttachSession {
            session_name: _,
            host: _,
            ssh_target: _,
        } => SocketResponse::err("attach-session must use asynchronous socket dispatch"),
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
        SocketMessage::QueryAgentSessions => handle_query_agent_sessions(state),
        SocketMessage::AgentWorkspace {
            branch,
            command,
            repo,
            ..
        } => handle_agent_workspace(
            state,
            term_stack,
            tab_list,
            window,
            &branch,
            command.as_deref(),
            repo.as_deref(),
        ),
        SocketMessage::PairingOffer => handle_pairing_offer(state),
        SocketMessage::PairingPending => handle_pairing_pending(state),
        SocketMessage::PairingConfirm { offer_id } => handle_pairing_confirm(state, &offer_id),
        SocketMessage::PairingReject { offer_id } => handle_pairing_reject(state, &offer_id),
        SocketMessage::DeviceRevoke { device_id } => handle_device_revoke(state, &device_id),
    }
}

/// Current Unix time in milliseconds, saturating to 0 before the epoch.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A fresh random UUID for offer and device identifiers, reusing the broker's
/// canonical v4 UUID generator.
fn new_pairing_id() -> Result<String, String> {
    crate::pty_broker::BrokerEpoch::new()
        .map(|epoch| epoch.to_string())
        .map_err(|e| format!("could not generate identifier: {e}"))
}

fn handle_pairing_offer(state: &Rc<RefCell<AppState>>) -> SocketResponse {
    let offer_id = match new_pairing_id() {
        Ok(id) => id,
        Err(e) => return SocketResponse::err(e),
    };
    let now = now_unix_ms();
    let expires_at_ms = state
        .borrow_mut()
        .pairing
        .create_offer(offer_id.clone(), now);
    SocketResponse::ok_with_data(serde_json::json!({
        "offer_id": offer_id,
        "expires_at_ms": expires_at_ms,
    }))
}

fn handle_pairing_pending(state: &Rc<RefCell<AppState>>) -> SocketResponse {
    let now = now_unix_ms();
    let pending: Vec<serde_json::Value> = state
        .borrow()
        .pairing
        .pending(now)
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "offer_id": p.offer_id,
                "device_name": p.device_name,
                "key_fingerprint": p.key_fingerprint,
            })
        })
        .collect();
    SocketResponse::ok_with_data(serde_json::json!({ "pending": pending }))
}

fn handle_pairing_confirm(state: &Rc<RefCell<AppState>>, offer_id: &str) -> SocketResponse {
    let device_id = match new_pairing_id() {
        Ok(id) => id,
        Err(e) => return SocketResponse::err(e),
    };
    let now = now_unix_ms();
    match state.borrow_mut().pairing.confirm(offer_id, device_id, now) {
        Ok(device_id) => {
            SocketResponse::ok_with_data(serde_json::json!({ "device_id": device_id }))
        }
        Err(e) => SocketResponse::err(e.message()),
    }
}

fn handle_pairing_reject(state: &Rc<RefCell<AppState>>, offer_id: &str) -> SocketResponse {
    match state.borrow_mut().pairing.reject(offer_id) {
        Ok(()) => SocketResponse::ok(),
        Err(e) => SocketResponse::err(e.message()),
    }
}

fn handle_device_revoke(state: &Rc<RefCell<AppState>>, device_id: &str) -> SocketResponse {
    match state.borrow_mut().pairing.revoke(device_id) {
        Ok(()) => SocketResponse::ok(),
        Err(e) => SocketResponse::err(e.message()),
    }
}

fn handle_notify_message(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    tab_target: Option<&String>,
    message: Option<&String>,
) -> SocketResponse {
    let change = match dispatch_notify_message(state, tab_target, message) {
        Ok(change) => change,
        Err(response) => return response,
    };
    let Some(change) = change else {
        return SocketResponse::ok();
    };

    sidebar::refresh_tab_row(tab_list, state, change.tab_id);
    crate::show_toast(&change.toast_message);
    crate::dashboard::refresh_dashboard_if_open(state, term_stack);
    SocketResponse::ok()
}

/// Resolve and apply the state/event portion of the public `notify` socket
/// action. UI binding is performed by [`handle_notify_message`] after this
/// succeeds, keeping the actual dispatch path testable without a display.
fn dispatch_notify_message(
    state: &Rc<RefCell<AppState>>,
    tab_target: Option<&String>,
    message: Option<&String>,
) -> Result<Option<NotifyStateChange>, SocketResponse> {
    let target_tab_id = match tab_target {
        Some(_) => match resolve_tab_id_for_target(state, tab_target) {
            Ok(tab_id) => Some(tab_id),
            Err(err) => return Err(SocketResponse::err(err)),
        },
        None => None,
    };

    let change = apply_notify_state(
        &mut state.borrow_mut(),
        target_tab_id,
        message.map(String::as_str),
    );
    if let Some(payload) = change
        .as_ref()
        .and_then(|change| change.alert_payload.clone())
    {
        RuntimeHandle::from_shared_state(state.clone()).emit_event("alert_raised", payload);
    }
    Ok(change)
}

#[derive(Debug, PartialEq)]
struct NotifyStateChange {
    tab_id: u32,
    toast_message: String,
    alert_payload: Option<serde_json::Value>,
}

/// Apply the state portion of a socket notification.
///
/// A notification aimed at a background tab raises persistent attention that
/// [`AppState::present_tab`] consumes when the user presents that tab. A tab
/// that is already focused needs only the in-app toast; manufacturing an alert
/// there would leave a badge that cannot be consumed without switching away
/// and back.
fn apply_notify_state(
    state: &mut AppState,
    target_tab_id: Option<u32>,
    message: Option<&str>,
) -> Option<NotifyStateChange> {
    let active_tab_id = state
        .active_ws()
        .map_or(0, |workspace| workspace.active_tab);
    // If no target was supplied, prefer a background terminal while preserving
    // the legacy fallback to any background tab when no terminal is available.
    let tab_id = target_tab_id.or_else(|| {
        state
            .all_tabs()
            .find(|tab| tab.id != active_tab_id && tab.kind == TabKind::Terminal)
            .or_else(|| state.all_tabs().find(|tab| tab.id != active_tab_id))
            .map(|tab| tab.id)
    })?;
    let is_focused = tab_id == active_tab_id;

    if is_focused {
        state.clear_socket_notification(tab_id);
        clear_tab_attention(state.all_tabs_mut().find(|tab| tab.id == tab_id)?);
    } else {
        let notification = message.map(str::to_string).or_else(|| {
            state
                .find_tab(tab_id)
                .map(|(_, tab)| tab_alert_message(tab))
        })?;
        state.set_socket_notification(tab_id, notification);
        let tab = state.all_tabs_mut().find(|tab| tab.id == tab_id)?;
        tab.needs_attention = true;
        if let Some(body) = message {
            tab.notification_msg = Some(body.to_string());
        }
    }

    let tab = state.find_tab(tab_id)?.1;
    let toast_message = message
        .map(str::to_string)
        .or_else(|| state.tab_notification_message(tab).map(str::to_string))
        .unwrap_or_else(|| tab.name.clone());
    let alert_payload =
        (!is_focused).then(|| alert_event_payload(tab, toast_message.clone(), "socket-notify"));
    Some(NotifyStateChange {
        tab_id,
        toast_message,
        alert_payload,
    })
}

fn tab_alert_message(tab: &crate::Tab) -> String {
    tab.notification_msg
        .clone()
        .unwrap_or_else(|| tab.name.clone())
}

fn alert_event_payload(tab: &crate::Tab, message: String, source: &str) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "tab_id": tab.id,
        "tab_name": tab.name,
        "message": message,
        "source": source,
    });
    if let Some(pane_id) = tab.notification_pane_id {
        payload["pane_id"] = serde_json::json!(pane_id);
    }
    payload
}

fn tab_alert_event(
    event_type: &'static str,
    tab: &crate::Tab,
    message: String,
    source: &str,
) -> (&'static str, serde_json::Value) {
    (event_type, alert_event_payload(tab, message, source))
}

fn clear_tab_attention(tab: &mut crate::Tab) -> Option<String> {
    let had_attention = tab.needs_attention || tab.notification_msg.is_some();
    let message = had_attention.then(|| tab_alert_message(tab));
    tab.needs_attention = false;
    tab.notified = false;
    tab.notification_msg = None;
    tab.notification_pane_id = None;
    message
}

fn clear_tab_attention_event(
    tab: &mut crate::Tab,
    source: &str,
) -> Option<(&'static str, serde_json::Value)> {
    let message =
        (tab.needs_attention || tab.notification_msg.is_some()).then(|| tab_alert_message(tab))?;
    let event = tab_alert_event("alert_cleared", tab, message, source);
    clear_tab_attention(tab);
    Some(event)
}

fn retarget_tab_attention_to_remaining_activity(tab: &mut crate::Tab) -> bool {
    let candidate = tab
        .pane_agent_activity
        .iter()
        .filter(|(_, activity)| {
            matches!(
                activity.state,
                AgentActivityState::WaitingInput
                    | AgentActivityState::Errored
                    | AgentActivityState::Done
            )
        })
        .max_by_key(|(_, activity)| {
            let priority = match activity.state {
                AgentActivityState::WaitingInput | AgentActivityState::Errored => 2,
                AgentActivityState::Done => 1,
                AgentActivityState::Idle | AgentActivityState::Running => 0,
            };
            (priority, activity.updated_at)
        })
        .map(|(pane_id, activity)| (*pane_id, activity.text.clone(), activity.source.clone()));
    let Some((pane_id, text, source)) = candidate else {
        return false;
    };
    tab.notification_pane_id = Some(pane_id);
    tab.notification_msg = Some(agents::format_agent_notification(
        source.as_deref(),
        Some(pane_id),
        &text,
    ));
    true
}

fn socket_activity_state_label(state: SocketActivityState) -> &'static str {
    match state {
        SocketActivityState::Idle => "idle",
        SocketActivityState::Running => "running",
        SocketActivityState::WaitingInput => "waiting-input",
        SocketActivityState::Errored => "errored",
        SocketActivityState::Done => "done",
    }
}

fn handle_work_context_message(
    state: &Rc<RefCell<AppState>>,
    tab_target: Option<&String>,
    pane_id: u32,
) -> SocketResponse {
    let tab_id = match resolve_tab_id_for_target(state, tab_target) {
        Ok(tab_id) => tab_id,
        Err(error) => return SocketResponse::err(error),
    };
    match crate::work_reporting::context_for_pane(&state.borrow(), tab_id, pane_id) {
        Ok(context) => {
            let mut data = serde_json::to_value(context).expect("work context is serializable");
            data["instructions"] = serde_json::json!(crate::work_reporting::REPORTING_INSTRUCTIONS);
            SocketResponse::ok_with_data(data)
        }
        Err(error) => SocketResponse::err(error),
    }
}

fn handle_work_report_message(
    state: &Rc<RefCell<AppState>>,
    report: crate::work_reporting::WorkReport,
) -> SocketResponse {
    let target = match crate::work_reporting::validate_report(&state.borrow(), &report) {
        Ok(target) => target,
        Err(error) => return SocketResponse::err(error),
    };
    let (tab_id, pane_id) = target;
    let milestone = report.milestone;
    if !state
        .borrow_mut()
        .record_agent_report(tab_id, pane_id, milestone, report.note.as_deref())
    {
        return SocketResponse::err("stale pane context");
    }
    RuntimeHandle::from_shared_state(state.clone()).emit_event(
        "agent_work_reported",
        serde_json::json!({
            "tab_id": tab_id,
            "pane_id": pane_id,
            "task_id": report.context.task_id,
            "milestone": milestone.label(),
        }),
    );
    let mut response = SocketResponse::ok_with_data(serde_json::json!({
        "task_id": report.context.task_id,
        "milestone": milestone.label(),
        "recorded": true,
    }));
    response.tab_id = Some(tab_id);
    response.pane_id = Some(pane_id);
    response
}

fn resolve_agent_status_pane_target(
    requested: Option<u32>,
    candidate_panes: &[u32],
) -> Result<u32, String> {
    let mut panes = candidate_panes.to_vec();
    panes.sort_unstable();
    panes.dedup();

    if let Some(pane_id) = requested {
        return panes
            .contains(&pane_id)
            .then_some(pane_id)
            .ok_or_else(|| format!("pane {pane_id} not found in target tab"));
    }

    match panes.as_slice() {
        [pane_id] => Ok(*pane_id),
        [] => Err("target tab has no addressable pane".to_string()),
        _ => Err("agent-status requires pane targeting for a multi-pane tab".to_string()),
    }
}

fn validate_agent_status_source(
    source: Option<&str>,
    detected_agent: Option<&str>,
    pane_id: u32,
) -> Result<(), String> {
    let (Some(source), Some(detected_agent)) = (source, detected_agent) else {
        return Ok(());
    };
    if crate::agent_sessions::normalize_agent_name(source)
        == crate::agent_sessions::normalize_agent_name(detected_agent)
    {
        Ok(())
    } else {
        Err(format!(
            "agent-status source {source:?} does not match pane {pane_id} agent {detected_agent:?}"
        ))
    }
}

struct AgentStatusInput<'a> {
    tab_target: Option<&'a String>,
    pane_target: Option<u32>,
    activity_state: SocketActivityState,
    text: Option<&'a str>,
    source: Option<&'a str>,
}

fn handle_agent_status_message(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    input: AgentStatusInput<'_>,
) -> SocketResponse {
    let AgentStatusInput {
        tab_target,
        pane_target,
        activity_state,
        text,
        source,
    } = input;
    let tab_id = match resolve_tab_id_for_target(state, tab_target) {
        Ok(tab_id) => tab_id,
        Err(err) => return SocketResponse::err(err),
    };
    let candidate_panes = {
        let st = state.borrow();
        let Some((_, tab)) = st.find_tab(tab_id) else {
            return SocketResponse::err("tab not found");
        };
        let mut pane_ids: Vec<u32> = match tab.panes.as_ref() {
            crate::pane::PaneNode::Stub { pane_id } => vec![*pane_id],
            _ => tab
                .panes
                .leaves()
                .into_iter()
                .map(|leaf| leaf.pane_id)
                .collect(),
        };
        pane_ids.extend(tab.pane_agent_activity.keys().copied());
        pane_ids
    };
    let pane_id = match resolve_agent_status_pane_target(pane_target, &candidate_panes) {
        Ok(pane_id) => pane_id,
        Err(err) => return SocketResponse::err(err),
    };
    let detected_agent = {
        let st = state.borrow();
        crate::runtime_probe::runtime_process_truth_is_fresh(&st)
            .then(|| {
                st.runtime_probe
                    .as_ref()
                    .and_then(|probe| probe.pane_agents.get(&(tab_id, pane_id)))
                    .and_then(|status| status.agent_name.as_deref())
                    .map(str::to_string)
            })
            .flatten()
    };
    if let Err(error) = validate_agent_status_source(source, detected_agent.as_deref(), pane_id) {
        return SocketResponse::err(error);
    }

    let mut clear_at = None;
    let mut pending_event = None;
    let activity_event;
    {
        let mut st = state.borrow_mut();
        let active_tab_id = st.active_ws().map_or(0, |ws| ws.active_tab);
        let has_socket_notification = st.has_socket_notification(tab_id);
        let Some(tab) = st.find_tab_mut(tab_id) else {
            return SocketResponse::err("tab not found");
        };
        let should_clear_tab_attention =
            tab.notification_pane_id.is_none() || tab.notification_pane_id == Some(pane_id);

        match activity_state {
            SocketActivityState::Idle => {
                tab.set_pane_agent_activity(pane_id, None);
                tab.clear_pane_notification(pane_id);
                if should_clear_tab_attention && !retarget_tab_attention_to_remaining_activity(tab)
                {
                    let cleared = clear_tab_attention_event(tab, "socket-agent-status");
                    if !has_socket_notification {
                        pending_event = cleared;
                    }
                }
            }
            SocketActivityState::Running => {
                let activity = AgentActivity::socket(
                    AgentActivityState::Running,
                    agents::summarize_activity_text(text.unwrap_or("working")),
                    source.map(str::to_string),
                );
                tab.set_pane_agent_activity(pane_id, activity);
                tab.clear_pane_notification(pane_id);
                if should_clear_tab_attention && !retarget_tab_attention_to_remaining_activity(tab)
                {
                    let cleared = clear_tab_attention_event(tab, "socket-agent-status");
                    if !has_socket_notification {
                        pending_event = cleared;
                    }
                }
            }
            SocketActivityState::WaitingInput
            | SocketActivityState::Errored
            | SocketActivityState::Done => {
                let (state, fallback) = match activity_state {
                    SocketActivityState::WaitingInput => {
                        (AgentActivityState::WaitingInput, "waiting for input")
                    }
                    SocketActivityState::Errored => (AgentActivityState::Errored, "errored"),
                    SocketActivityState::Done => (AgentActivityState::Done, "done"),
                    SocketActivityState::Idle | SocketActivityState::Running => unreachable!(),
                };
                let summary = agents::summarize_activity_text(text.unwrap_or(fallback));
                let activity = AgentActivity::socket(state, &summary, source.map(str::to_string));
                if matches!(activity_state, SocketActivityState::Done) {
                    clear_at = activity.as_ref().map(|activity| activity.updated_at);
                }
                tab.set_pane_agent_activity(pane_id, activity);
                if tab.note_pane_notification(pane_id, Some(state)) {
                    tab.notified = false;
                }
                tab.notification_msg = Some(agents::format_agent_notification(
                    source,
                    Some(pane_id),
                    &summary,
                ));
                tab.notification_pane_id = Some(pane_id);
                if tab_id != active_tab_id {
                    tab.needs_attention = true;
                    pending_event = Some(tab_alert_event(
                        "alert_raised",
                        tab,
                        summary,
                        "socket-agent-status",
                    ));
                }
            }
        }

        activity_event = serde_json::json!({
            "tab_id": tab.id,
            "tab_name": tab.name,
            "pane_id": pane_id,
            "state": socket_activity_state_label(activity_state),
            "source": source,
        });
    }

    RuntimeHandle::from_shared_state(state.clone())
        .emit_event("agent_activity_changed", activity_event);
    if let Some((event_type, payload)) = pending_event {
        RuntimeHandle::from_shared_state(state.clone()).emit_event(event_type, payload);
    }

    sidebar::refresh_tab_row(tab_list, state, tab_id);
    crate::dashboard::refresh_dashboard_if_open(state, term_stack);

    if let Some(done_at) = clear_at {
        schedule_done_activity_clear(state.clone(), tab_list.clone(), tab_id, pane_id, done_at);
    }

    let mut response = SocketResponse::ok();
    response.tab_id = Some(tab_id);
    response.pane_id = Some(pane_id);
    response
}

fn schedule_done_activity_clear(
    state: Rc<RefCell<AppState>>,
    tab_list: gtk::Box,
    tab_id: u32,
    pane_id: u32,
    done_at: std::time::Instant,
) {
    glib::timeout_add_local_once(DONE_ACTIVITY_VISIBILITY, move || {
        let should_refresh = {
            let mut st = state.borrow_mut();
            let Some(tab) = st.find_tab_mut(tab_id) else {
                return;
            };
            tab.clear_pane_agent_activity_if(pane_id, |activity| {
                matches!(activity.state, AgentActivityState::Done)
                    && matches!(
                        activity.origin,
                        crate::workspace::AgentActivityOrigin::Socket
                    )
                    && activity.updated_at == done_at
            })
        };

        if should_refresh {
            sidebar::refresh_tab_row(&tab_list, &state, tab_id);
        }
    });
}

fn validate_socket_command_input<'a>(action: &str, command: &'a str) -> Result<&'a str, String> {
    if command.trim().is_empty() {
        return Err(format!("{action} command cannot be empty"));
    }
    if command.chars().any(|c| c == '\0') {
        return Err(format!("{action} command must not contain NUL bytes"));
    }
    // Reject embedded carriage returns. A bare '\r' lets an authenticated
    // run-in-pane/create-tab/open-pane caller inject an extra command line into
    // the spawned shell (CR line-confusion / command injection). Nothing
    // legitimately needs '\r' in these command strings.
    if command.chars().any(|c| c == '\r') {
        return Err(format!(
            "{action} command must not contain carriage returns"
        ));
    }
    // '\n' stays allowed: callers legitimately pass multi-line shell scripts
    // (e.g. "printf 'hi'\nexit 0") that run as a single `/bin/sh -c` argument.
    // '\t' stays allowed: tabs are harmless inside arguments.
    if command
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err(format!(
            "{action} command contains unsupported control characters"
        ));
    }
    Ok(command)
}

fn validate_get_text_scrollback(scrollback_lines: u32) -> Result<u32, String> {
    if scrollback_lines > SOCKET_MAX_CAPTURE_SCROLLBACK_LINES {
        return Err(format!(
            "scrollback exceeds max {} lines",
            SOCKET_MAX_CAPTURE_SCROLLBACK_LINES
        ));
    }
    Ok(scrollback_lines)
}

#[allow(clippy::too_many_arguments)] // Socket handler forwards the already-parsed request fields directly into terminal orchestration.
fn handle_open_pane(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    tab_target: Option<&String>,
    command: &str,
    direction: Option<&str>,
    working_dir: Option<&str>,
) -> SocketResponse {
    let tab_id = match resolve_tab_id_for_target(state, tab_target) {
        Ok(tab_id) => tab_id,
        Err(err) => return SocketResponse::err(err),
    };
    let command = match validate_socket_command_input("open-pane", command) {
        Ok(command) => command,
        Err(err) => return SocketResponse::err(err),
    };
    let split_direction = match direction.unwrap_or("vertical") {
        "vertical" => SplitDirection::Vertical,
        "horizontal" => SplitDirection::Horizontal,
        other => return SocketResponse::err(format!("invalid direction: {other}")),
    };
    let argv_owned = ["/bin/sh".to_string(), "-c".to_string(), command.to_string()];
    let argv_refs: Vec<&str> = argv_owned.iter().map(String::as_str).collect();
    let runtime = RuntimeHandle::from_shared_state(state.clone());
    match terminal::split_pane_with_command(
        &runtime,
        term_stack,
        tab_list,
        window,
        tab_id,
        split_direction,
        &argv_refs,
        working_dir,
    ) {
        Some(pane_id) => SocketResponse::ok_with_pane(pane_id),
        None => SocketResponse::err("failed to split pane"),
    }
}

/// Split the focused pane of a tab via the HTTP control surface. Unlike
/// `open-pane`, the command is optional: `None` spawns the pane's default shell,
/// mirroring an interactive split, while `Some` runs a validated command.
#[allow(clippy::too_many_arguments)] // Socket handler forwards already-parsed request fields into terminal orchestration.
fn handle_split_pane(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    tab_target: Option<&String>,
    direction: Option<&str>,
    command: Option<&str>,
    working_dir: Option<&str>,
) -> SocketResponse {
    let tab_id = match resolve_tab_id_for_target(state, tab_target) {
        Ok(tab_id) => tab_id,
        Err(err) => return SocketResponse::err(err),
    };
    let split_direction = match direction.unwrap_or("vertical") {
        "vertical" => SplitDirection::Vertical,
        "horizontal" => SplitDirection::Horizontal,
        other => return SocketResponse::err(format!("invalid direction: {other}")),
    };
    let command_argv = match command {
        Some(command) => {
            let command = match validate_socket_command_input("split-pane", command) {
                Ok(command) => command,
                Err(err) => return SocketResponse::err(err),
            };
            Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                command.to_string(),
            ])
        }
        None => None,
    };
    let runtime = RuntimeHandle::from_shared_state(state.clone());
    match terminal::open_split_pane(
        &runtime,
        term_stack,
        tab_list,
        window,
        tab_id,
        split_direction,
        command_argv,
        working_dir,
    ) {
        Some(pane_id) => SocketResponse::ok_with_pane(pane_id),
        None => SocketResponse::err("failed to split pane"),
    }
}

fn handle_run_in_pane(
    state: &Rc<RefCell<AppState>>,
    tab_target: Option<&String>,
    pane_id: u32,
    command: &str,
) -> SocketResponse {
    // Resolve tab + pane and clone the terminal handle, then drop the
    // borrow *before* calling feed_child.  VTE's feed_child can
    // synchronously emit signals that re-borrow AppState on the same
    // thread, which would panic if we still held borrow_mut.
    let command = match validate_socket_command_input("run-in-pane", command) {
        Ok(command) => command,
        Err(err) => return SocketResponse::err(err),
    };
    let tab_id = match resolve_tab_id_for_target(state, tab_target) {
        Ok(tab_id) => tab_id,
        Err(err) => return SocketResponse::err(err),
    };
    let terminal = {
        let mut st = state.borrow_mut();
        let Some(tab) = st.find_tab_mut(tab_id) else {
            return SocketResponse::err("tab not found");
        };
        let Some(leaf) = tab.panes.leaf_mut(pane_id) else {
            return SocketResponse::err("pane not found");
        };
        leaf.terminal.clone()
    }; // borrow_mut released here — safe to call feed_child now
    let payload = format!("{command}\n");
    terminal.feed_child(payload.as_bytes());
    SocketResponse::ok()
}

fn close_pane_socket_response(result: Result<(), String>) -> SocketResponse {
    match result {
        Ok(()) => SocketResponse::ok(),
        Err(error) => SocketResponse::err(error),
    }
}

// ── F010: Full scripting API handlers ──

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseTabPlan {
    CloseTab,
    CloseWorkspace { workspace_id: u32 },
}

#[derive(Debug)]
pub(crate) struct PreparedTabClose {
    pub(crate) plan: CloseTabPlan,
    pub(crate) detached_sessions: Vec<String>,
    pub(crate) tmux_backings_to_close: Vec<crate::pane::TmuxBacking>,
}

fn tab_tmux_backings(state: &AppState, tab_id: u32) -> Vec<crate::pane::TmuxBacking> {
    let mut backings = state
        .find_tab(tab_id)
        .into_iter()
        .flat_map(|(_, tab)| tab.panes.leaves())
        .filter_map(|leaf| leaf.tmux_backing.clone())
        .chain(
            state
                .headless_panes
                .iter()
                .filter(|((current_tab_id, _), _)| *current_tab_id == tab_id)
                .filter_map(|(_, pane)| pane.tmux_backing.clone()),
        )
        .collect::<Vec<_>>();
    let mut seen = std::collections::HashSet::new();
    backings.retain(|backing| seen.insert((backing.target.clone(), backing.session_name.clone())));
    backings
}

fn close_tab_plan(
    total_tabs: usize,
    workspace_id: u32,
    workspace_tab_count: usize,
    is_worktree: bool,
) -> Result<CloseTabPlan, &'static str> {
    if total_tabs <= 1 {
        return Err("cannot close last tab");
    }

    if workspace_tab_count == 1 && is_worktree {
        Ok(CloseTabPlan::CloseWorkspace { workspace_id })
    } else {
        Ok(CloseTabPlan::CloseTab)
    }
}

pub(crate) fn prepare_tab_close(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    behavior: crate::config::TmuxCloseBehavior,
) -> Result<PreparedTabClose, String> {
    let plan = {
        let st = state.borrow();
        let total_tabs: usize = st
            .workspaces
            .iter()
            .map(|workspace| workspace.tabs.len())
            .sum();
        let (workspace, _) = st
            .find_tab(tab_id)
            .ok_or_else(|| "tab not found".to_string())?;
        close_tab_plan(
            total_tabs,
            workspace.id,
            workspace.tabs.len(),
            workspace.is_worktree,
        )
        .map_err(str::to_string)?
    };

    let tmux_backings_to_close = match behavior {
        crate::config::TmuxCloseBehavior::Close => tab_tmux_backings(&state.borrow(), tab_id),
        crate::config::TmuxCloseBehavior::Detach => Vec::new(),
    };
    let detached_sessions = match behavior {
        crate::config::TmuxCloseBehavior::Close => Vec::new(),
        crate::config::TmuxCloseBehavior::Detach => {
            crate::terminal::register_detached_tab(state, tab_id)?
        }
    };

    Ok(PreparedTabClose {
        plan,
        detached_sessions,
        tmux_backings_to_close,
    })
}

pub(crate) fn apply_tab_close_plan<T>(
    plan: CloseTabPlan,
    close_tab: impl FnOnce() -> T,
    close_workspace: impl FnOnce(u32) -> T,
) -> T {
    match plan {
        CloseTabPlan::CloseTab => close_tab(),
        CloseTabPlan::CloseWorkspace { workspace_id } => close_workspace(workspace_id),
    }
}

fn handle_create_tab(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    name: Option<&str>,
    working_dir: Option<&str>,
    command: Option<&str>,
) -> SocketResponse {
    let tab_name = name.unwrap_or("Shell");
    let runtime = RuntimeHandle::from_shared_state(state.clone());
    let tab_id = if let Some(cmd) = command {
        let cmd = match validate_socket_command_input("create-tab", cmd) {
            Ok(command) => command,
            Err(err) => return SocketResponse::err(err),
        };
        let argv = vec!["/bin/sh".to_string(), "-c".to_string(), cmd.to_string()];
        terminal::create_terminal_with_command(&runtime, term_stack, tab_name, working_dir, argv)
    } else {
        terminal::create_terminal(&runtime, term_stack, tab_name, working_dir, None)
    };
    sidebar::add_tab_row(tab_list, state, term_stack, tab_id, tab_name, true);
    terminal::wire_tab_terminals(state, term_stack, tab_list, window, tab_id);
    SocketResponse::ok_with_tab(tab_id)
}

fn resolve_agent_workspace_repo_path(
    explicit_repo: Option<&str>,
    workspace_context: Option<&str>,
    tab_context: Option<&str>,
) -> Result<PathBuf, String> {
    let Some((source, raw_path)) = [
        ("repo", explicit_repo),
        ("active workspace", workspace_context),
        ("active tab", tab_context),
    ]
    .into_iter()
    .find_map(|(source, path)| path.map(|path| (source, path))) else {
        return Err(
            "no repo context: specify \"repo\" or switch to a workspace in a git repository"
                .to_string(),
        );
    };

    let path = Path::new(raw_path);
    if !path.is_dir() {
        return Err(format!("{source} path does not exist: {raw_path}"));
    }

    let normalized = crate::git::normalize_path(raw_path);
    let git_info = crate::git::discover(&normalized);
    // Agent-workspace is scoped to the checkout supplied by the caller.  A
    // linked worktree shares its Git database with a primary checkout, but
    // reporting or creating relative to that primary checkout would violate
    // the socket contract and strand cleanup outside the requested path.
    let Some(repo_root) = git_info.workspace_root() else {
        return Err(match source {
            "repo" => format!("repo path is not inside a git repository: {raw_path}"),
            _ => format!("{source} is not inside a git repository: {raw_path}"),
        });
    };

    Ok(PathBuf::from(repo_root))
}

/// Repo-context candidates for an agent-workspace request, captured on the main
/// context before the Git work is handed to the worker: the explicit `repo`
/// argument, the active workspace path, the active tab's cwd, and the source
/// workspace `(id, work_origin)` used to validate the apply phase.
type AgentWorkspaceRepoContext = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<(u32, String)>,
);

fn resolve_agent_workspace_repo(
    state: &Rc<RefCell<AppState>>,
    explicit_repo: Option<&str>,
) -> AgentWorkspaceRepoContext {
    let (workspace_context, tab_context, source_workspace) = {
        let st = state.borrow();
        let workspace_context = st.active_ws().and_then(|ws| {
            ws.working_tree_path
                .as_deref()
                .or(ws.repo_root.as_deref())
                .map(str::to_string)
        });
        let tab_context = st.active_tab().and_then(|tab| {
            tab.panes
                .leaf(tab.focused_pane_id)
                .or_else(|| tab.panes.leaves().into_iter().next())
                .and_then(|leaf| leaf.local_cwd())
                .or_else(|| tab.discovery_cwd.clone())
        });
        let source_workspace = st
            .active_ws()
            .map(|workspace| (workspace.id, workspace.work_origin.clone()));
        (workspace_context, tab_context, source_workspace)
    };
    (
        explicit_repo.map(str::to_string),
        workspace_context,
        tab_context,
        source_workspace,
    )
}

fn ensure_agent_worktree(repo_root: &Path, branch: &str) -> Result<(String, bool), String> {
    let repo_root = crate::git::normalize_path(&repo_root.to_string_lossy());
    if let Some(existing) = crate::git::list_worktrees_checked(Path::new(&repo_root))?
        .into_iter()
        .find(|worktree| worktree.branch.as_deref() == Some(branch))
    {
        let existing_path = crate::git::normalize_path(&existing.path);
        if existing_path == repo_root {
            return Err(format!(
                "branch {branch} is already checked out in the primary checkout at {repo_root}"
            ));
        }
        return Ok((existing_path, false));
    }

    crate::git::create_worktree(Path::new(&repo_root), branch)
        .map(|path| (crate::git::normalize_path(&path), true))
}

fn preferred_agent_workspace_tab_id(workspace: &crate::Workspace) -> Option<u32> {
    workspace
        .tabs
        .iter()
        .find(|tab| {
            tab.kind == TabKind::Terminal
                && tab.name == "Shell"
                && tab.workspace_action.is_none()
                && tab.panes.leaf_count() == 1
        })
        .map(|tab| tab.id)
}

fn ensure_agent_workspace_command_tab(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    workspace_id: u32,
    worktree_path: &str,
) -> u32 {
    let existing_tab = {
        let st = state.borrow();
        st.workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .and_then(preferred_agent_workspace_tab_id)
    };

    if let Some(tab_id) = existing_tab {
        let _ = sidebar::activate_tab(tab_list, state, term_stack, tab_id);
        return tab_id;
    }

    let runtime = RuntimeHandle::from_shared_state(state.clone());
    let tab_id =
        terminal::create_terminal(&runtime, term_stack, "Shell", Some(worktree_path), None);
    sidebar::add_tab_row(tab_list, state, term_stack, tab_id, "Shell", true);
    terminal::wire_tab_terminals(state, term_stack, tab_list, window, tab_id);
    tab_id
}

/// Kept for direct/headless handler callers.  The live socket bridge uses
/// [`dispatch_agent_workspace`] so it can keep processing other GTK requests
/// while the client waits for the completed worktree response.
fn handle_agent_workspace(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    branch: &str,
    command: Option<&str>,
    repo: Option<&str>,
) -> SocketResponse {
    let _ = (state, term_stack, tab_list, window, branch, command, repo);
    SocketResponse::err("agent-workspace requires the asynchronous socket dispatcher")
}

fn agent_workspace_route_key(request_identity: &str, branch: &str) -> String {
    format!("agent-workspace:{request_identity}:{branch}")
}

fn agent_workspace_response(
    repo_root: &Path,
    worktree_path: &str,
    workspace_id: u32,
    tab_id: u32,
    created_worktree: bool,
    ran_command: bool,
    branch: &str,
) -> SocketResponse {
    let mut response = SocketResponse::ok_with_tab(tab_id);
    response.data = Some(serde_json::json!({
        "branch": branch,
        "repo_root": repo_root.display().to_string(),
        "worktree_path": worktree_path,
        "workspace_id": workspace_id,
        "tab_id": tab_id,
        "created_worktree": created_worktree,
        "ran_command": ran_command,
    }));
    response
}

fn respond_agent_workspace_subscribers(
    subscribers: Vec<AgentWorkspaceSubscriber>,
    response: SocketResponse,
) {
    for subscriber in subscribers {
        let _ = subscriber.response_tx.send(response.clone());
    }
}

fn take_agent_workspace_subscribers(key: &str) -> Vec<AgentWorkspaceSubscriber> {
    AGENT_WORKSPACE_SUBSCRIBERS
        .with(|operations| operations.borrow_mut().remove(key).unwrap_or_default())
}

fn next_agent_workspace_subscriber_id() -> u64 {
    NEXT_AGENT_WORKSPACE_SUBSCRIBER_ID.with(|next| {
        let id = next.get().wrapping_add(1);
        next.set(id);
        id
    })
}

fn agent_workspace_response_timeout(timeout_seconds: Option<f64>) -> Result<Duration, String> {
    let seconds = timeout_seconds.unwrap_or(DEFAULT_AGENT_WORKSPACE_RESPONSE_TIMEOUT.as_secs_f64());
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err("agent-workspace timeout_seconds must be a positive finite number".to_string());
    }
    let timeout = Duration::try_from_secs_f64(seconds)
        .map_err(|_| "agent-workspace timeout_seconds is out of range".to_string())?;
    if timeout > MAX_AGENT_WORKSPACE_RESPONSE_TIMEOUT {
        return Err(format!(
            "agent-workspace timeout_seconds may not exceed {} seconds",
            MAX_AGENT_WORKSPACE_RESPONSE_TIMEOUT.as_secs()
        ));
    }
    Ok(timeout)
}

/// A CLI timeout is a request-lifecycle deadline, not merely a local socket
/// read timeout. Removing the subscriber before apply prevents an abandoned
/// synchronous request from unexpectedly opening a workspace later. If it was
/// the last subscriber, `apply_agent_workspace_operation` rolls a created
/// worktree back through the bounded worker.
fn schedule_agent_workspace_subscriber_expiry(
    operation_key: String,
    subscriber_id: u64,
    timeout: Duration,
) {
    // Use the thread-default context rather than the process-default source.
    // The socket receiver is main-context-bound, and its lifecycle timer must
    // stay with that same context (including isolated runtime/test contexts).
    glib::spawn_future_local(async move {
        glib::timeout_future(timeout).await;
        let expired = AGENT_WORKSPACE_SUBSCRIBERS.with(|operations| {
            let mut operations = operations.borrow_mut();
            let subscribers = operations.get_mut(&operation_key)?;
            let index = subscribers
                .iter()
                .position(|subscriber| subscriber.id == subscriber_id)?;
            let subscriber = subscribers.remove(index);
            if subscribers.is_empty() {
                operations.remove(&operation_key);
            }
            Some(subscriber)
        });
        if let Some(subscriber) = expired {
            let _ = subscriber.response_tx.send(SocketResponse::err(
                "agent-workspace response deadline expired before apply; request was abandoned",
            ));
        }
    });
}

fn source_workspace_is_current(
    state: &Rc<RefCell<AppState>>,
    source_workspace: Option<&(u32, String)>,
) -> bool {
    source_workspace.is_none_or(|(workspace_id, work_origin)| {
        state
            .borrow()
            .workspaces
            .iter()
            .any(|workspace| workspace.id == *workspace_id && workspace.work_origin == *work_origin)
    })
}

fn schedule_orphaned_agent_worktree_rollback(
    repo_root: PathBuf,
    worktree_path: String,
    stale_subscribers: Vec<AgentWorkspaceSubscriber>,
    abandonment_reason: &'static str,
) {
    let worktree_for_worker = worktree_path.clone();
    let worktree_for_response = worktree_path.clone();
    let subscribers = Arc::new(Mutex::new(Some(stale_subscribers)));
    let subscribers_for_apply = subscribers.clone();
    let submission = crate::git::spawn_async_result(
        format!("agent-workspace-rollback:{worktree_path}"),
        move || {
            crate::git::remove_worktree_from_repo(Some(&repo_root), Path::new(&worktree_for_worker))
        },
        move |result| {
            let subscribers = subscribers_for_apply
                .lock()
                .expect("agent-workspace rollback subscribers lock")
                .take()
                .unwrap_or_default();
            match result {
            Ok(()) => respond_agent_workspace_subscribers(
                subscribers,
                SocketResponse::err(format!(
                    "agent-workspace {abandonment_reason}; removed unopened worktree {worktree_for_response}"
                )),
            ),
            Err(error) => {
                let message = format!(
                    "agent-workspace {abandonment_reason} and rollback failed for {worktree_for_response}: {error}"
                );
                crate::show_error_toast(&message);
                respond_agent_workspace_subscribers(subscribers, SocketResponse::err(message));
            }
        }
        },
    );
    if !matches!(submission, crate::git::GitAsyncSubmission::Started) {
        let message = format!(
            "agent-workspace {abandonment_reason}; rollback could not be scheduled for {worktree_path}"
        );
        crate::show_error_toast(&message);
        let subscribers = subscribers
            .lock()
            .expect("agent-workspace rollback subscribers lock")
            .take()
            .unwrap_or_default();
        respond_agent_workspace_subscribers(subscribers, SocketResponse::err(message));
    }
}

fn apply_agent_workspace_operation(
    state: &Rc<RefCell<AppState>>,
    term_stack: Option<&gtk::Stack>,
    tab_list: Option<&gtk::Box>,
    window: Option<&adw::ApplicationWindow>,
    operation_key: &str,
    branch: &str,
    result: Result<(PathBuf, String, bool), String>,
) {
    let subscribers = take_agent_workspace_subscribers(operation_key);
    match result {
        Err(error) => {
            let message = format!("failed to create worktree: {error}");
            crate::show_error_toast(&format!("Could not create worktree: {error}"));
            respond_agent_workspace_subscribers(subscribers, SocketResponse::err(message));
        }
        Ok((repo_root, worktree_path, created_worktree)) => {
            let (current, stale): (Vec<_>, Vec<_>) =
                subscribers.into_iter().partition(|subscriber| {
                    source_workspace_is_current(state, subscriber.source_workspace.as_ref())
                });
            if current.is_empty() {
                let abandonment_reason = if stale.is_empty() {
                    "response deadline expired before apply"
                } else {
                    "source workspace closed before apply"
                };
                crate::show_toast(
                    "Worktree finished after its request was abandoned; result was not opened",
                );
                if created_worktree {
                    schedule_orphaned_agent_worktree_rollback(
                        repo_root,
                        worktree_path,
                        stale,
                        abandonment_reason,
                    );
                } else {
                    respond_agent_workspace_subscribers(
                        stale,
                        SocketResponse::err("agent-workspace request became stale before apply"),
                    );
                }
                return;
            }

            let (Some(term_stack), Some(tab_list), Some(window)) = (term_stack, tab_list, window)
            else {
                let message = "agent-workspace runtime UI disappeared before apply";
                if created_worktree {
                    schedule_orphaned_agent_worktree_rollback(
                        repo_root,
                        worktree_path,
                        current.into_iter().chain(stale).collect(),
                        "runtime UI disappeared before apply",
                    );
                } else {
                    respond_agent_workspace_subscribers(
                        current.into_iter().chain(stale).collect(),
                        SocketResponse::err(message),
                    );
                }
                return;
            };

            let (workspace_id, mut tab_id) = crate::palette::open_worktree_workspace_scoped_to_repo(
                &worktree_path,
                state,
                tab_list,
                term_stack,
                window,
                Some(&repo_root.to_string_lossy()),
            );

            // One Git/worktree operation may have multiple waiting callers.
            // Group exact command identities so duplicate socket retries run a
            // command once, while distinct commands are never silently lost.
            let mut command_responses: HashMap<Option<String>, SocketResponse> = HashMap::new();
            for command in current.iter().map(|subscriber| subscriber.command.clone()) {
                if command_responses.contains_key(&command) {
                    continue;
                }
                let response = if let Some(command) = command.as_deref() {
                    tab_id = ensure_agent_workspace_command_tab(
                        state,
                        term_stack,
                        tab_list,
                        window,
                        workspace_id,
                        &worktree_path,
                    );
                    let tab_target = tab_id.to_string();
                    let mut response = handle_run_in_pane(state, Some(&tab_target), 0, command);
                    response.tab_id = Some(tab_id);
                    response.data = Some(serde_json::json!({
                        "branch": branch,
                        "repo_root": repo_root.display().to_string(),
                        "worktree_path": worktree_path,
                        "workspace_id": workspace_id,
                        "tab_id": tab_id,
                        "created_worktree": created_worktree,
                        "ran_command": response.ok,
                    }));
                    if !response.ok {
                        crate::show_error_toast(&format!(
                            "Worktree ready but command could not start: {}",
                            response.error.as_deref().unwrap_or("unknown error")
                        ));
                    }
                    response
                } else {
                    agent_workspace_response(
                        &repo_root,
                        &worktree_path,
                        workspace_id,
                        tab_id,
                        created_worktree,
                        false,
                        branch,
                    )
                };
                command_responses.insert(command, response);
            }

            let any_command_ran = command_responses.values().any(|response| {
                response.ok
                    && response.data.as_ref().is_some_and(|data| {
                        data.get("ran_command").and_then(serde_json::Value::as_bool) == Some(true)
                    })
            });
            crate::show_toast(&format!(
                "Worktree ready: {}{}",
                worktree_path,
                if any_command_ran {
                    " (command started)"
                } else {
                    ""
                }
            ));
            RuntimeHandle::from_shared_state(state.clone()).emit_event(
                "agent_workspace_ready",
                serde_json::json!({
                    "repo_root": repo_root.display().to_string(),
                    "worktree_path": worktree_path,
                    "workspace_id": workspace_id,
                    "tab_id": tab_id,
                    "created_worktree": created_worktree,
                    "ran_command": any_command_ran,
                }),
            );

            for subscriber in current {
                let response = command_responses
                    .get(&subscriber.command)
                    .cloned()
                    .expect("every agent-workspace command has a completion response");
                let _ = subscriber.response_tx.send(response);
            }
            respond_agent_workspace_subscribers(
                stale,
                SocketResponse::err("agent-workspace request became stale before apply"),
            );
        }
    }
}

/// The one place that recognises the long-running agent-workspace verb and
/// hands it to the bounded Git worker instead of `handle_socket_message`.
/// Returns the message untouched when it is a normal GTK action, so no caller
/// can grow a second routing match that drifts from this one.
fn route_agent_workspace_socket_message(
    state: &Rc<RefCell<AppState>>,
    term_stack: Option<&gtk::Stack>,
    tab_list: Option<&gtk::Box>,
    window: Option<&adw::ApplicationWindow>,
    msg: SocketMessage,
    response_tx: &mpsc::Sender<SocketResponse>,
) -> Option<SocketMessage> {
    let SocketMessage::AgentWorkspace {
        branch,
        command,
        repo,
        timeout_seconds,
    } = msg
    else {
        return Some(msg);
    };
    record_socket_message_event(
        state,
        &SocketMessage::AgentWorkspace {
            branch: branch.clone(),
            command: command.clone(),
            repo: repo.clone(),
            timeout_seconds,
        },
    );
    dispatch_agent_workspace(
        &AgentWorkspaceDispatchContext {
            state: state.clone(),
            term_stack: term_stack.cloned(),
            tab_list: tab_list.cloned(),
            window: window.cloned(),
        },
        branch,
        command,
        repo,
        timeout_seconds,
        response_tx.clone(),
    );
    None
}

/// Start or join a worktree operation without blocking GTK.  The Unix-socket
/// connection still receives the *completed* response from
/// `apply_agent_workspace_operation`, preserving the long-standing CLI
/// contract while allowing subsequent socket requests and GLib sources to run.
fn dispatch_agent_workspace(
    context: &AgentWorkspaceDispatchContext,
    branch: String,
    command: Option<String>,
    repo: Option<String>,
    timeout_seconds: Option<f64>,
    response_tx: mpsc::Sender<SocketResponse>,
) {
    let branch = branch.trim();
    if branch.is_empty() {
        let _ = response_tx.send(SocketResponse::err("branch is required"));
        return;
    }
    let command = match command {
        Some(command) => match validate_socket_command_input("agent-workspace", &command) {
            Ok(command) => Some(command.to_string()),
            Err(error) => {
                let _ = response_tx.send(SocketResponse::err(error));
                return;
            }
        },
        None => None,
    };
    let response_timeout = match agent_workspace_response_timeout(timeout_seconds) {
        Ok(timeout) => timeout,
        Err(error) => {
            let _ = response_tx.send(SocketResponse::err(error));
            return;
        }
    };

    let (explicit_repo, workspace_context, tab_context, source_workspace) =
        resolve_agent_workspace_repo(&context.state, repo.as_deref());
    let request_identity = explicit_repo
        .as_deref()
        .or(workspace_context.as_deref())
        .or(tab_context.as_deref())
        .unwrap_or("missing-repo-context");
    let request_key = agent_workspace_route_key(request_identity, branch);
    let subscriber = AgentWorkspaceSubscriber {
        id: next_agent_workspace_subscriber_id(),
        source_workspace,
        command,
        response_tx,
    };
    let subscriber_id = subscriber.id;
    let should_start = AGENT_WORKSPACE_SUBSCRIBERS.with(|operations| {
        let mut operations = operations.borrow_mut();
        if let Some(subscribers) = operations.get_mut(&request_key) {
            subscribers.push(subscriber);
            return Some(false);
        }
        if operations.len() >= MAX_AGENT_WORKSPACE_OPERATIONS {
            let _ = subscriber.response_tx.send(SocketResponse::err(
                "too many agent-workspace operations are running; retry later",
            ));
            return None;
        }
        operations.insert(request_key.clone(), vec![subscriber]);
        Some(true)
    });
    let Some(should_start) = should_start else {
        return;
    };
    schedule_agent_workspace_subscriber_expiry(
        request_key.clone(),
        subscriber_id,
        response_timeout,
    );
    if !should_start {
        return;
    }

    let branch_for_worker = branch.to_string();
    let context_for_apply = context.clone();
    let key_for_apply = request_key.clone();
    let key_for_failure = key_for_apply.clone();
    let branch_for_apply = branch.to_string();
    let submission = crate::git::spawn_async_result(
        request_key,
        move || {
            delay_agent_workspace_worker_for_test();
            let repo_root = resolve_agent_workspace_repo_path(
                explicit_repo.as_deref(),
                workspace_context.as_deref(),
                tab_context.as_deref(),
            )?;
            let (worktree_path, created_worktree) =
                ensure_agent_worktree(&repo_root, &branch_for_worker)?;
            #[cfg(test)]
            if created_worktree {
                AGENT_WORKSPACE_TEST_WORKTREE_CREATED.store(true, Ordering::SeqCst);
            }
            Ok::<_, String>((repo_root, worktree_path, created_worktree))
        },
        move |result| {
            apply_agent_workspace_operation(
                &context_for_apply.state,
                context_for_apply.term_stack.as_ref(),
                context_for_apply.tab_list.as_ref(),
                context_for_apply.window.as_ref(),
                &key_for_apply,
                &branch_for_apply,
                result,
            );
        },
    );
    if !matches!(submission, crate::git::GitAsyncSubmission::Started) {
        let subscribers = take_agent_workspace_subscribers(&key_for_failure);
        let message = match submission {
            crate::git::GitAsyncSubmission::Coalesced => {
                "agent-workspace operation was already registered without subscribers"
            }
            crate::git::GitAsyncSubmission::Saturated => {
                "Git worker queue is busy; retry agent-workspace shortly"
            }
            crate::git::GitAsyncSubmission::Started => unreachable!(),
        };
        respond_agent_workspace_subscribers(subscribers, SocketResponse::err(message));
    }
}

fn handle_switch_tab(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    tab_target: &str,
) -> SocketResponse {
    let owned_target = tab_target.to_string();
    let tab_id = match resolve_tab_id_for_target(state, Some(&owned_target)) {
        Ok(tab_id) => tab_id,
        Err(err) => return SocketResponse::err(err),
    };
    if sidebar::activate_tab(tab_list, state, term_stack, tab_id) {
        SocketResponse::ok()
    } else {
        SocketResponse::err("tab UI not found")
    }
}

fn handle_rename_tab(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    tab_target: &str,
    new_name: &str,
) -> SocketResponse {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return SocketResponse::err("name cannot be empty");
    }
    let owned_target = tab_target.to_string();
    let tab_id = match resolve_tab_id_for_target(state, Some(&owned_target)) {
        Ok(tab_id) => tab_id,
        Err(err) => return SocketResponse::err(err),
    };
    if sidebar::rename_tab(tab_list, state, tab_id, new_name) {
        SocketResponse::ok()
    } else {
        SocketResponse::err("tab not found")
    }
}

fn handle_reorder_tab(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    tab_target: &str,
    index: usize,
) -> SocketResponse {
    let owned_target = tab_target.to_string();
    // Resolving the target is the genuine not-found error, exactly like rename-tab.
    let tab_id = match resolve_tab_id_for_target(state, Some(&owned_target)) {
        Ok(tab_id) => tab_id,
        Err(err) => return SocketResponse::err(err),
    };
    // reorder_tab_to_index clamps out-of-range indices and treats an
    // already-in-place tab as success, so a sort caller can re-run safely.
    match sidebar::reorder_tab_to_index(tab_list, state, tab_id, index) {
        Ok(()) => SocketResponse::ok(),
        Err(err) => SocketResponse::err(err),
    }
}

fn handle_create_workspace(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    name: Option<&str>,
) -> SocketResponse {
    // Auto-name to match the GUI "New Workspace" fallback: "Workspace {n}".
    let ws_name = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => name.to_string(),
        None => {
            let count = state.borrow().workspaces.len();
            format!("Workspace {}", count + 1)
        }
    };

    let runtime = RuntimeHandle::from_shared_state(state.clone());
    let ws_id = runtime.create_workspace(&ws_name, None);
    sidebar::add_workspace_header(tab_list, state, term_stack, ws_id, &ws_name);

    // Give the workspace an initial shell tab, matching the GUI which never
    // leaves a new workspace empty. This also makes the workspace visible and
    // usable through query-state right away.
    let tab_id = terminal::create_terminal(&runtime, term_stack, "Shell", None, None);
    sidebar::add_tab_row(tab_list, state, term_stack, tab_id, "Shell", true);
    terminal::wire_tab_terminals(state, term_stack, tab_list, window, tab_id);

    SocketResponse::ok_with_data(serde_json::json!({
        "workspace_id": ws_id,
        "name": ws_name,
    }))
}

fn handle_switch_workspace(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    workspace_target: &str,
) -> SocketResponse {
    let ws_id = match resolve_workspace_id_for_target(state, workspace_target) {
        Ok(ws_id) => ws_id,
        Err(err) => return SocketResponse::err(err),
    };
    if sidebar::activate_workspace(tab_list, state, term_stack, ws_id) {
        SocketResponse::ok_with_data(serde_json::json!({ "workspace_id": ws_id }))
    } else {
        SocketResponse::err("workspace not found")
    }
}

fn handle_rename_workspace(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    workspace_target: &str,
    new_name: &str,
) -> SocketResponse {
    let ws_id = match resolve_workspace_id_for_target(state, workspace_target) {
        Ok(ws_id) => ws_id,
        Err(err) => return SocketResponse::err(err),
    };
    match sidebar::rename_workspace(tab_list, state, ws_id, new_name) {
        Ok(applied) => SocketResponse::ok_with_data(serde_json::json!({
            "workspace_id": ws_id,
            "name": applied,
        })),
        Err(err) => SocketResponse::err(err),
    }
}

fn handle_query_state(state: &Rc<RefCell<AppState>>) -> SocketResponse {
    let st = state.borrow();
    SocketResponse::ok_with_data(crate::api::build_state_snapshot(&st))
}

fn handle_query_events(
    state: &Rc<RefCell<AppState>>,
    since_seq: Option<u64>,
    limit: Option<usize>,
) -> SocketResponse {
    let st = state.borrow();
    SocketResponse::ok_with_data(crate::api::build_events_snapshot(
        &st.event_store,
        since_seq,
        limit,
    ))
}

fn handle_query_agent_sessions(state: &Rc<RefCell<AppState>>) -> SocketResponse {
    let live_bindings = {
        let st = state.borrow();
        crate::agent_sessions::build_live_agent_bindings(&st)
    };
    let snapshot = crate::agent_sessions::default_catalog().snapshot_blocking(live_bindings);
    match serde_json::to_value(snapshot) {
        Ok(data) => SocketResponse::ok_with_data(data),
        Err(error) => SocketResponse::err(format!(
            "could not serialize agent session snapshot: {error}"
        )),
    }
}

fn handle_list_tabs(state: &Rc<RefCell<AppState>>) -> SocketResponse {
    let st = state.borrow();
    SocketResponse::ok_with_data(crate::api::build_list_tabs_snapshot(&st))
}

fn handle_get_text(
    state: &Rc<RefCell<AppState>>,
    tab_target: Option<&String>,
    pane_id: u32,
    scrollback_lines: u32,
    logical: bool,
) -> SocketResponse {
    let scrollback_lines = match validate_get_text_scrollback(scrollback_lines) {
        Ok(scrollback_lines) => scrollback_lines,
        Err(err) => return SocketResponse::err(err),
    };
    let tab_id = match resolve_tab_id_for_target(state, tab_target) {
        Ok(tab_id) => tab_id,
        Err(err) => return SocketResponse::err(err),
    };
    let terminal = {
        let st = state.borrow();
        let Some((_, tab)) = st.find_tab(tab_id) else {
            return SocketResponse::err("tab not found");
        };
        let Some(leaf) = tab
            .panes
            .leaves()
            .into_iter()
            .find(|l| l.pane_id == pane_id)
        else {
            return SocketResponse::err("pane not found");
        };
        leaf.terminal.clone()
    };
    let (_cursor_col, cursor_row) = terminal.cursor_position();
    let cols = terminal.column_count();
    let row_count = terminal.row_count();
    if cols <= 0 || row_count <= 0 {
        return SocketResponse::ok_with_data(serde_json::json!({
            "text": "",
            "rows": 0,
        }));
    }
    // Visible area ends at cursor_row, starts row_count above.
    // Scrollback extends further up.
    let visible_start = (cursor_row - row_count + 1).max(0);
    let start_row = if scrollback_lines > 0 {
        (visible_start - scrollback_lines as libc::c_long).max(0)
    } else {
        visible_start
    };
    // Row-wise capture so the caller can choose logical (soft-wrap-rejoined)
    // lines or raw one-line-per-grid-row output. `logical` defaults on; the
    // logical join reproduces VTE's whole-range `text_range_format` byte-for-byte.
    let rows = crate::terminal::capture_grid_rows(&terminal, start_row, cursor_row, cols);
    let text = if logical {
        crate::terminal::join_logical_lines(&rows)
    } else {
        rows.iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    SocketResponse::ok_with_data(serde_json::json!({
        "text": text,
        "rows": cursor_row - start_row + 1,
    }))
}

fn write_json_response(
    conn: &mut std::os::unix::net::UnixStream,
    response: SocketResponse,
) -> std::io::Result<()> {
    let payload = serde_json::to_vec(&response)
        .unwrap_or_else(|_| br#"{"ok":false,"error":"failed to serialize response"}"#.to_vec());
    conn.write_all(&payload)?;
    conn.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::{
        alert_event_payload, apply_notify_state, bind_socket_listener, cleanup_socket,
        cleanup_stale_socket_registry_file, clear_tab_attention, close_tab_plan,
        detached_session_list_payload, dispatch_notify_message, dispatch_socket_message,
        ensure_agent_worktree, handle_device_revoke, handle_list_tabs, handle_pairing_confirm,
        handle_pairing_offer, handle_pairing_pending, handle_pairing_reject, handle_query_events,
        handle_query_state, handle_socket_connection, preferred_agent_workspace_tab_id,
        prepare_tab_close, process_exists, record_socket_message_event, registry_path_in,
        resolve_agent_status_pane_target, resolve_agent_workspace_repo_path,
        resolve_detached_session_for_attach, resolve_tab_id_for_target_in_state,
        resolve_workspace_id_for_target_in_state, retarget_tab_attention_to_remaining_activity,
        route_agent_workspace_socket_message, run_socket_handler_safely, select_runtime_dir,
        socket_message_action, socket_message_is_read_only, socket_path_in,
        spawn_socket_dispatch_router, spawn_socket_listener_thread, validate_agent_status_source,
        validate_get_text_scrollback, validate_runtime_dir, validate_runtime_dir_attributes,
        validate_socket_command_input, validate_socket_path_length, CloseTabPlan, RuntimeDirIssue,
        SocketActivityState, SocketDispatchContext, SocketMessage, SocketRegistry, SocketResponse,
        AGENT_WORKSPACE_TEST_DELAY_FINISHED, AGENT_WORKSPACE_TEST_DELAY_MS,
        AGENT_WORKSPACE_TEST_DELAY_STARTED, AGENT_WORKSPACE_TEST_WORKTREE_CREATED,
        SOCKET_MAX_CAPTURE_SCROLLBACK_LINES,
    };
    use crate::{
        palette::{ensure_worktree_workspace_state, WorktreeWorkspaceState},
        pane::PaneNode,
        runtime::{RuntimeHandle, TerminalTabRegistration},
        workspace::{AgentActivity, AgentActivityState, TabKind, WorkspaceAction, WorkspaceStatus},
        AppState, Tab, Workspace,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::io::{Read as _, Write as _};
    use std::net::Shutdown;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::rc::Rc;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Mutex,
    };
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::sync::mpsc as tokio_mpsc;

    static SESSION_ENV_LOCK: Mutex<()> = Mutex::new(());
    static PANIC_NEXT_SOCKET_HANDLER_REQUEST: AtomicBool = AtomicBool::new(false);

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "taarof-socket-tests-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    fn send_socket_request_for_tests(
        socket_path: &Path,
        request: serde_json::Value,
    ) -> mpsc::Receiver<String> {
        let (response_sender, response_receiver) = mpsc::channel();
        std::thread::spawn({
            let socket_path = socket_path.to_path_buf();
            move || {
                let mut stream =
                    UnixStream::connect(&socket_path).expect("socket client should connect");
                stream
                    .write_all(
                        serde_json::to_string(&request)
                            .expect("socket request should serialize")
                            .as_bytes(),
                    )
                    .expect("socket request should be written");
                stream
                    .shutdown(Shutdown::Write)
                    .expect("socket write side should close");
                let mut response = String::new();
                stream
                    .read_to_string(&mut response)
                    .expect("socket response should be readable");
                response_sender
                    .send(response)
                    .expect("socket response should be forwarded");
            }
        });

        response_receiver
    }

    fn dispatch_socket_request_for_tests<F>(
        socket_path: &Path,
        request: serde_json::Value,
        dispatch_rx: &mut tokio_mpsc::Receiver<(SocketMessage, mpsc::Sender<SocketResponse>)>,
        handler: F,
    ) -> serde_json::Value
    where
        F: FnOnce(SocketMessage) -> SocketResponse,
    {
        let response_receiver = send_socket_request_for_tests(socket_path, request);
        let (message, response_tx) = dispatch_rx
            .blocking_recv()
            .expect("listener should forward the socket request");
        let action = socket_message_action(&message);
        let _ = response_tx.send(run_socket_handler_safely(action, || handler(message)));
        let response = response_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("socket response should arrive");

        serde_json::from_str(response.trim()).expect("socket response should be json")
    }

    fn panic_once_socket_handler(
        state: &Rc<RefCell<AppState>>,
        msg: SocketMessage,
    ) -> SocketResponse {
        if PANIC_NEXT_SOCKET_HANDLER_REQUEST.swap(false, Ordering::SeqCst) {
            panic!("intentional socket bridge liveness panic");
        }

        match msg {
            SocketMessage::ListTabs => handle_list_tabs(state),
            SocketMessage::QueryState => handle_query_state(state),
            SocketMessage::OpenDashboard => {
                SocketResponse::err("open-dashboard requires a GTK runtime in this test")
            }
            _ => SocketResponse::err("unsupported test socket action"),
        }
    }

    #[test]
    fn socket_bridge_liveness_survives_request_handler_panic() {
        let _guard = SESSION_ENV_LOCK
            .lock()
            .expect("session env lock should work");
        PANIC_NEXT_SOCKET_HANDLER_REQUEST.store(true, Ordering::SeqCst);

        let runtime_dir = unique_temp_dir("socket-bridge-liveness");
        let state = Rc::new(RefCell::new(stub_app_state(
            vec![stub_workspace_with_id(
                11,
                "default",
                vec![stub_terminal_tab(101, "Shell", None)],
                101,
            )],
            11,
        )));
        let (socket_path, listener) =
            bind_socket_listener(&runtime_dir).expect("state-only socket server should start");
        let (dispatch_tx, mut dispatch_rx) =
            tokio_mpsc::channel::<(SocketMessage, mpsc::Sender<SocketResponse>)>(8);
        spawn_socket_listener_thread(
            listener,
            dispatch_tx,
            crate::history::HistoryReader::disabled(),
        );

        let panic_response = dispatch_socket_request_for_tests(
            &socket_path,
            serde_json::json!({ "action": "open-dashboard" }),
            &mut dispatch_rx,
            |message| panic_once_socket_handler(&state, message),
        );
        assert_eq!(panic_response["ok"], serde_json::json!(false));
        assert!(
            panic_response["error"]
                .as_str()
                .expect("panic response should include an error")
                .contains("runtime request handler panicked"),
            "unexpected panic response: {panic_response}"
        );

        let tabs_response = dispatch_socket_request_for_tests(
            &socket_path,
            serde_json::json!({ "action": "list-tabs" }),
            &mut dispatch_rx,
            |message| panic_once_socket_handler(&state, message),
        );
        assert_eq!(tabs_response["ok"], serde_json::json!(true));
        assert_eq!(
            tabs_response["data"]["workspaces"][0]["tabs"][0]["tab_id"],
            serde_json::json!(101),
            "list-tabs must still work after a failed request"
        );

        let state_response = dispatch_socket_request_for_tests(
            &socket_path,
            serde_json::json!({ "action": "query-state" }),
            &mut dispatch_rx,
            |message| panic_once_socket_handler(&state, message),
        );
        assert_eq!(state_response["ok"], serde_json::json!(true));
        assert_eq!(
            state_response["data"]["active_tab"],
            serde_json::json!(101),
            "query-state must still work after a failed request"
        );

        let dashboard_response = dispatch_socket_request_for_tests(
            &socket_path,
            serde_json::json!({ "action": "open-dashboard" }),
            &mut dispatch_rx,
            |message| panic_once_socket_handler(&state, message),
        );
        assert_eq!(dashboard_response["ok"], serde_json::json!(false));
        let dashboard_error = dashboard_response["error"]
            .as_str()
            .expect("dashboard response should include a runtime error");
        assert!(dashboard_error.contains("GTK runtime"));
        assert!(
            !dashboard_error.contains("runtime request queue is closed"),
            "mutating actions should fail for runtime reasons, not because the bridge died"
        );

        cleanup_socket(&socket_path);
        let _ = std::fs::remove_dir_all(runtime_dir);
    }

    #[test]
    fn socket_listener_serves_second_client_while_first_is_idle() {
        let dir = unique_temp_dir("listener-idle-client");
        let socket_path = dir.join("taarof-test.sock");
        let listener = UnixListener::bind(&socket_path).expect("test listener should bind");
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<(SocketMessage, mpsc::Sender<SocketResponse>)>(8);

        spawn_socket_listener_thread(listener, tx, crate::history::HistoryReader::disabled());

        let handler = std::thread::spawn(move || {
            let (message, response_tx) = rx
                .blocking_recv()
                .expect("listener should forward the second request");
            assert!(matches!(message, SocketMessage::ListTabs));
            response_tx
                .send(SocketResponse::ok())
                .expect("response should send");
        });

        let _idle_client = UnixStream::connect(&socket_path).expect("idle client should connect");
        let mut active_client =
            UnixStream::connect(&socket_path).expect("active client should connect");
        active_client
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("read timeout should be set");
        active_client
            .write_all(br#"{"action":"list-tabs"}"#)
            .expect("request should write");
        active_client
            .shutdown(Shutdown::Write)
            .expect("client write side should close");

        let mut response = String::new();
        active_client
            .read_to_string(&mut response)
            .expect("response should arrive while first client is still idle");
        assert!(
            response.contains(r#""ok":true"#),
            "unexpected response: {response}"
        );

        handler.join().expect("handler thread should finish");
    }

    #[test]
    fn bind_socket_listener_restricts_socket_to_owner_only() {
        let runtime_dir = unique_temp_dir("bind-perms-0600");
        // Loosen the process umask so an unrestricted bind would otherwise leave
        // group/other bits set, proving the explicit chmod is what tightens it.
        let previous_umask = unsafe { libc::umask(0) };

        let (socket_path, listener) =
            bind_socket_listener(&runtime_dir).expect("socket should bind in temp runtime dir");

        let mode = std::fs::symlink_metadata(&socket_path)
            .expect("bound socket file should exist")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "control socket must be owner-only (0600), got {:04o}",
            mode & 0o777
        );

        unsafe {
            libc::umask(previous_umask);
        }
        drop(listener);
        cleanup_socket(&socket_path);
        let _ = std::fs::remove_dir_all(&runtime_dir);
    }

    #[test]
    fn socket_connection_times_out_read_only_queries() {
        let (server_conn, mut client_conn) =
            UnixStream::pair().expect("socket pair should be created");
        let (tx, _rx) =
            tokio::sync::mpsc::channel::<(SocketMessage, mpsc::Sender<SocketResponse>)>(8);

        let handler = std::thread::spawn(move || {
            handle_socket_connection(
                server_conn,
                tx,
                crate::history::HistoryReader::disabled(),
                Duration::from_millis(100),
                Duration::from_millis(20),
            );
        });

        client_conn
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("read timeout should be set");
        client_conn
            .write_all(br#"{"action":"list-tabs"}"#)
            .expect("request should write");
        client_conn
            .shutdown(Shutdown::Write)
            .expect("client write side should close");

        let mut response = String::new();
        client_conn
            .read_to_string(&mut response)
            .expect("query timeout response should arrive");
        assert!(
            response.contains("timeout waiting for runtime query response"),
            "unexpected response: {response}"
        );

        handler.join().expect("handler thread should finish");
    }

    #[test]
    fn socket_connection_waits_for_mutating_commands_without_timeout() {
        let (server_conn, mut client_conn) =
            UnixStream::pair().expect("socket pair should be created");
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<(SocketMessage, mpsc::Sender<SocketResponse>)>(8);

        let handler = std::thread::spawn(move || {
            handle_socket_connection(
                server_conn,
                tx,
                crate::history::HistoryReader::disabled(),
                Duration::from_millis(100),
                Duration::from_millis(20),
            );
        });
        let responder = std::thread::spawn(move || {
            let (message, response_tx) = rx
                .blocking_recv()
                .expect("listener should forward the mutating request");
            assert!(matches!(message, SocketMessage::AgentWorkspace { .. }));
            std::thread::sleep(Duration::from_millis(75));
            response_tx
                .send(SocketResponse::ok())
                .expect("response should send");
        });

        client_conn
            .set_read_timeout(Some(Duration::from_millis(5000)))
            .expect("read timeout should be set");
        client_conn
            .write_all(br#"{"action":"agent-workspace","branch":"feature/test"}"#)
            .expect("request should write");
        client_conn
            .shutdown(Shutdown::Write)
            .expect("client write side should close");

        let mut response = String::new();
        client_conn
            .read_to_string(&mut response)
            .expect("mutating response should arrive");
        assert!(
            response.contains(r#""ok":true"#),
            "unexpected response: {response}"
        );

        handler.join().expect("handler thread should finish");
        responder.join().expect("responder thread should finish");
    }

    #[test]
    fn delayed_agent_workspace_keeps_glib_and_dispatched_query_responsive() {
        let _glib_guard = crate::glib_main_context_test_guard();
        let context = glib::MainContext::default();
        let _acquire = context.acquire().expect("test owns the main context");
        let state = Rc::new(RefCell::new(stub_app_state(
            vec![stub_workspace_with_id(
                11,
                "default",
                vec![stub_terminal_tab(101, "Shell", None)],
                101,
            )],
            11,
        )));
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<(SocketMessage, mpsc::Sender<SocketResponse>)>(8);
        let receiver_done = Rc::new(Cell::new(false));
        let receiver_done_for_task = receiver_done.clone();
        let (fake_started_tx, fake_started_rx) = mpsc::channel();
        let state_for_dispatch = state.clone();

        // This is the same socket bridge shape used by start_notify_server:
        // the delayed fake adapter is independently spawned, so the receiver
        // can dispatch a real QueryState before it completes.
        glib::MainContext::default().spawn_local(async move {
            while let Some((message, response_tx)) = rx.recv().await {
                match message {
                    SocketMessage::AgentWorkspace { .. } => {
                        let fake_started_tx = fake_started_tx.clone();
                        glib::spawn_future_local(async move {
                            let response = gio::spawn_blocking(move || {
                                fake_started_tx
                                    .send(())
                                    .expect("fake adapter start should be observed");
                                std::thread::sleep(Duration::from_millis(150));
                                SocketResponse::ok()
                            })
                            .await
                            .unwrap_or_else(|_| SocketResponse::err("fake adapter failed"));
                            let _ = response_tx.send(response);
                        });
                    }
                    SocketMessage::QueryState => {
                        let _ = response_tx.send(handle_query_state(&state_for_dispatch));
                    }
                    _ => {
                        let _ = response_tx.send(SocketResponse::err("unexpected test action"));
                    }
                }
                glib::timeout_future(Duration::from_millis(0)).await;
            }
            receiver_done_for_task.set(true);
        });

        let heartbeat = Rc::new(std::cell::Cell::new(0));
        let heartbeat_for_source = heartbeat.clone();
        glib::timeout_add_local_once(Duration::from_millis(1), move || {
            heartbeat_for_source.set(heartbeat_for_source.get() + 1)
        });

        let (agent_server, mut agent_client) = UnixStream::pair().expect("agent socket pair");
        let agent_tx = tx.clone();
        let agent_handler = std::thread::spawn(move || {
            handle_socket_connection(
                agent_server,
                agent_tx,
                crate::history::HistoryReader::disabled(),
                Duration::from_millis(100),
                Duration::from_millis(500),
            );
        });
        agent_client
            .write_all(br#"{"action":"agent-workspace","branch":"feature/delayed"}"#)
            .expect("agent request should write");
        agent_client
            .shutdown(Shutdown::Write)
            .expect("agent write side should close");
        let (agent_response_tx, agent_response_rx) = mpsc::channel();
        let agent_reader = std::thread::spawn(move || {
            let mut response = String::new();
            agent_client
                .read_to_string(&mut response)
                .expect("agent response should read");
            agent_response_tx
                .send(response)
                .expect("agent response should forward");
        });

        let started_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while fake_started_rx.try_recv().is_err() {
            assert!(
                std::time::Instant::now() < started_deadline,
                "delayed fake adapter did not start"
            );
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }

        let (query_server, mut query_client) = UnixStream::pair().expect("query socket pair");
        let query_handler_tx = tx.clone();
        let query_handler = std::thread::spawn(move || {
            handle_socket_connection(
                query_server,
                query_handler_tx,
                crate::history::HistoryReader::disabled(),
                Duration::from_millis(100),
                Duration::from_millis(200),
            );
        });
        query_client
            .write_all(br#"{"action":"query-state"}"#)
            .expect("query request should write");
        query_client
            .shutdown(Shutdown::Write)
            .expect("query write side should close");
        let (query_response_tx, query_response_rx) = mpsc::channel();
        let query_reader = std::thread::spawn(move || {
            let mut response = String::new();
            query_client
                .read_to_string(&mut response)
                .expect("query response should read");
            query_response_tx
                .send(response)
                .expect("query response should forward");
        });

        let query_deadline = std::time::Instant::now() + Duration::from_millis(100);
        let query_response = loop {
            if let Ok(response) = query_response_rx.try_recv() {
                break response;
            }
            assert!(
                std::time::Instant::now() < query_deadline,
                "query-state was starved by the delayed agent adapter"
            );
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        };
        assert!(query_response.contains(r#""ok":true"#));
        assert!(
            heartbeat.get() > 0,
            "a real GLib timeout source must run while the adapter is delayed"
        );
        assert!(
            agent_response_rx.try_recv().is_err(),
            "the delayed mutation should not have completed before query-state"
        );

        let completion_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while agent_response_rx.try_recv().is_err() {
            assert!(
                std::time::Instant::now() < completion_deadline,
                "delayed mutation did not complete"
            );
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(tx);
        agent_handler.join().expect("agent handler should finish");
        agent_reader.join().expect("agent reader should finish");
        query_handler.join().expect("query handler should finish");
        query_reader.join().expect("query reader should finish");
        let cleanup_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !receiver_done.get() {
            assert!(
                std::time::Instant::now() < cleanup_deadline,
                "delayed agent socket receiver did not release its GLib source"
            );
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn real_agent_workspace_socket_worker_rolls_back_abandoned_apply_without_starving_queries() {
        struct ResetAgentWorkspaceDelay;
        impl Drop for ResetAgentWorkspaceDelay {
            fn drop(&mut self) {
                AGENT_WORKSPACE_TEST_DELAY_MS.store(0, Ordering::SeqCst);
                AGENT_WORKSPACE_TEST_DELAY_STARTED.store(false, Ordering::SeqCst);
                AGENT_WORKSPACE_TEST_DELAY_FINISHED.store(false, Ordering::SeqCst);
                AGENT_WORKSPACE_TEST_WORKTREE_CREATED.store(false, Ordering::SeqCst);
            }
        }

        let _delay_reset = ResetAgentWorkspaceDelay;
        let _glib_guard = crate::glib_main_context_test_guard();
        // A real socket receiver normally runs on the process default context.
        // Give this integration test its own context so it cannot drain a
        // local source that a parallel unit test created on the global one.
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let _acquire = context.acquire().expect("test owns the main context");
                let repo = TempRepo::new("agent-workspace-real-socket");
                let branch = "feature/real-socket-rollback";
                let state = Rc::new(RefCell::new(stub_app_state(
                    vec![stub_workspace_with_id(
                        11,
                        "default",
                        vec![stub_terminal_tab(101, "Shell", None)],
                        101,
                    )],
                    11,
                )));
                let socket_dir = unique_temp_dir("agent-workspace-real-socket");
                let socket_path = socket_dir.join("taarof-test.sock");
                let listener = UnixListener::bind(&socket_path).expect("test listener should bind");
                let (tx, rx) =
                    tokio_mpsc::channel::<(SocketMessage, mpsc::Sender<SocketResponse>)>(8);
                // Use the production connection handler over a real Unix listener,
                // but bound the test listener to its two expected connections.  The
                // production listener intentionally owns a sender forever; retaining
                // that here would keep the GTK receiver future alive past this test,
                // allowing a GLib source to be finalized by a later test thread.
                let listener_tx = tx.clone();
                let listener_thread = std::thread::spawn(move || {
                    let mut handlers = Vec::new();
                    for _ in 0..2 {
                        let (connection, _) = listener
                            .accept()
                            .expect("test listener should accept a real client");
                        let connection_tx = listener_tx.clone();
                        handlers.push(std::thread::spawn(move || {
                            handle_socket_connection(
                                connection,
                                connection_tx,
                                crate::history::HistoryReader::disabled(),
                                Duration::from_secs(2),
                                Duration::from_secs(5),
                            );
                        }));
                    }
                    drop(listener_tx);
                    for handler in handlers {
                        handler
                            .join()
                            .expect("socket connection handler should finish");
                    }
                });

                let receiver_done = Rc::new(Cell::new(false));
                let state_for_executor = state.clone();
                // Drive the production receive seam and the production
                // agent-workspace routing seam. The test deliberately exercises
                // the real post-worker apply abandonment path, so it passes no
                // GTK surfaces: the actual created worktree must be rolled back
                // before subscribers get their terminal response.
                spawn_socket_dispatch_router(
                    &context,
                    rx,
                    move |message, response_tx| {
                        let Some(message) = route_agent_workspace_socket_message(
                            &state_for_executor,
                            None,
                            None,
                            None,
                            message,
                            &response_tx,
                        ) else {
                            return;
                        };
                        let action = socket_message_action(&message);
                        let response = run_socket_handler_safely(action, || match message {
                            SocketMessage::QueryState => handle_query_state(&state_for_executor),
                            unexpected => SocketResponse::err(format!(
                                "unexpected test action: {}",
                                socket_message_action(&unexpected)
                            )),
                        });
                        let _ = response_tx.send(response);
                    },
                    Some(receiver_done.clone()),
                );

                let heartbeat = Rc::new(Cell::new(0));
                let heartbeat_for_source = heartbeat.clone();
                context.spawn_local(async move {
                    glib::timeout_future(Duration::from_millis(1)).await;
                    heartbeat_for_source.set(heartbeat_for_source.get() + 1)
                });
                AGENT_WORKSPACE_TEST_DELAY_STARTED.store(false, Ordering::SeqCst);
                AGENT_WORKSPACE_TEST_DELAY_FINISHED.store(false, Ordering::SeqCst);
                AGENT_WORKSPACE_TEST_WORKTREE_CREATED.store(false, Ordering::SeqCst);
                AGENT_WORKSPACE_TEST_DELAY_MS.store(300, Ordering::SeqCst);

                let agent_response = send_socket_request_for_tests(
                    &socket_path,
                    serde_json::json!({
                        "action": "agent-workspace",
                        "branch": branch,
                        "repo": repo.root,
                        "timeout_seconds": 0.2,
                    }),
                );
                let start_deadline = std::time::Instant::now() + Duration::from_secs(1);
                while !AGENT_WORKSPACE_TEST_DELAY_STARTED.load(Ordering::SeqCst) {
                    assert!(
                        std::time::Instant::now() < start_deadline,
                        "real agent-workspace worker did not start"
                    );
                    context.iteration(false);
                    std::thread::sleep(Duration::from_millis(1));
                }

                let query_response = send_socket_request_for_tests(
                    &socket_path,
                    serde_json::json!({"action": "query-state"}),
                );
                let query_deadline = std::time::Instant::now() + Duration::from_millis(100);
                let query = loop {
                    if let Ok(response) = query_response.try_recv() {
                        break response;
                    }
                    assert!(
                        std::time::Instant::now() < query_deadline,
                        "query-state was starved by the real delayed worktree worker"
                    );
                    context.iteration(false);
                    std::thread::sleep(Duration::from_millis(1));
                };
                assert!(query.contains(r#""ok":true"#));
                assert!(
                    heartbeat.get() > 0,
                    "GLib heartbeat should run during Git work"
                );
                assert!(
                    agent_response.try_recv().is_err(),
                    "request completed before its deadline"
                );

                let completion_deadline = std::time::Instant::now() + Duration::from_secs(5);
                let response = loop {
                    if let Ok(response) = agent_response.try_recv() {
                        break response;
                    }
                    assert!(
                        std::time::Instant::now() < completion_deadline,
                        "agent-workspace subscriber did not receive rollback completion"
                    );
                    context.iteration(false);
                    std::thread::sleep(Duration::from_millis(1));
                };
                let response: serde_json::Value =
                    serde_json::from_str(response.trim()).expect("response should be JSON");
                assert_eq!(response["ok"], serde_json::json!(false));
                assert!(response["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("response deadline expired before apply")));
                let worker_deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !AGENT_WORKSPACE_TEST_DELAY_FINISHED.load(Ordering::SeqCst) {
                    assert!(
                        std::time::Instant::now() < worker_deadline,
                        "the delayed worker did not reach the real worktree/apply phase"
                    );
                    context.iteration(false);
                    std::thread::sleep(Duration::from_millis(5));
                }
                let creation_deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !AGENT_WORKSPACE_TEST_WORKTREE_CREATED.load(Ordering::SeqCst) {
                    assert!(
                        std::time::Instant::now() < creation_deadline,
                        "the real worker did not create its worktree before rollback"
                    );
                    context.iteration(false);
                    std::thread::sleep(Duration::from_millis(5));
                }
                let rollback_deadline = std::time::Instant::now() + Duration::from_secs(5);
                while crate::git::list_worktrees(&repo.root).len() != 1 {
                    assert!(
                        std::time::Instant::now() < rollback_deadline,
                        "the real worktree must be removed by the worker rollback"
                    );
                    context.iteration(false);
                    std::thread::sleep(Duration::from_millis(5));
                }

                drop(tx);
                let receiver_deadline = std::time::Instant::now() + Duration::from_secs(1);
                while !receiver_done.get() {
                    assert!(
                        std::time::Instant::now() < receiver_deadline,
                        "test socket receiver did not release its GLib source"
                    );
                    context.iteration(false);
                    std::thread::sleep(Duration::from_millis(1));
                }
                listener_thread
                    .join()
                    .expect("bounded test socket listener should finish");
                cleanup_socket(&socket_path);
                let _ = std::fs::remove_dir_all(socket_dir);
            })
            .expect("test context should become thread-default");
    }

    #[test]
    fn socket_connection_accepts_large_send_keys_payloads() {
        let (server_conn, mut client_conn) =
            UnixStream::pair().expect("socket pair should be created");
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<(SocketMessage, mpsc::Sender<SocketResponse>)>(8);

        let handler = std::thread::spawn(move || {
            handle_socket_connection(
                server_conn,
                tx,
                crate::history::HistoryReader::disabled(),
                Duration::from_millis(100),
                Duration::from_millis(20),
            );
        });
        let responder = std::thread::spawn(move || {
            let (message, response_tx) = rx
                .blocking_recv()
                .expect("listener should forward the large request");
            match message {
                SocketMessage::SendKeys { pane, keys, .. } => {
                    assert_eq!(pane, 0);
                    assert_eq!(keys.len(), 8192);
                    assert!(keys.chars().all(|ch| ch == 'x'));
                }
                other => panic!("expected send-keys request, got {other:?}"),
            }
            response_tx
                .send(SocketResponse::ok())
                .expect("response should send");
        });

        let keys = "x".repeat(8192);
        let request = serde_json::json!({
            "action": "send-keys",
            "pane": 0,
            "keys": keys,
        })
        .to_string();

        client_conn
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("read timeout should be set");
        client_conn
            .write_all(request.as_bytes())
            .expect("request should write");
        client_conn
            .shutdown(Shutdown::Write)
            .expect("client write side should close");

        let mut response = String::new();
        client_conn
            .read_to_string(&mut response)
            .expect("large request response should arrive");
        assert!(
            response.contains(r#""ok":true"#),
            "unexpected response: {response}"
        );

        handler.join().expect("handler thread should finish");
        responder.join().expect("responder thread should finish");
    }

    struct TempRepo {
        root: PathBuf,
    }

    impl TempRepo {
        fn new(name: &str) -> Self {
            let root = unique_temp_dir(name);
            run_git(
                root.parent().expect("temp repo should have a parent"),
                &[
                    "init",
                    "-b",
                    "main",
                    root.to_str().expect("utf-8 temp path"),
                ],
            );
            run_git(&root, &["config", "user.email", "taarof@example.com"]);
            run_git(&root, &["config", "user.name", "taarof"]);
            std::fs::write(root.join("README.md"), "hello\n").expect("seed file should be written");
            run_git(&root, &["add", "README.md"]);
            run_git(&root, &["commit", "-m", "init"]);
            Self { root }
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git command should start");
        assert!(
            output.status.success(),
            "git {:?} failed in {}: {}",
            args,
            cwd.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn stub_terminal_tab(id: u32, name: &str, workspace_action: Option<WorkspaceAction>) -> Tab {
        Tab {
            id,
            name: name.to_string(),
            work_origin: crate::workspace::new_tab_work_origin(),
            kind: TabKind::Terminal,
            panes: Box::new(PaneNode::Stub { pane_id: 0 }),
            focused_pane_id: 0,
            next_pane_id: 1,
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
            agent_activity: None,
            needs_attention: false,
            notified: false,
            notification_msg: None,
            notification_pane_id: None,
            pane_last_notified: std::collections::HashMap::new(),
            pane_turn: std::collections::HashMap::new(),
            workspace_action,
            discovery_cwd: None,
            discovered_actions: Vec::new(),
            task_buttons: Vec::new(),
            tracking_data: None,
        }
    }

    fn stub_dashboard_tab(id: u32) -> Tab {
        Tab {
            id,
            name: "Dashboard".to_string(),
            work_origin: crate::workspace::new_tab_work_origin(),
            kind: TabKind::Dashboard,
            panes: Box::new(PaneNode::Empty),
            focused_pane_id: 0,
            next_pane_id: 0,
            pane_zoom: None,
            close_on_exit: false,
            respawn_on_exit: None,
            agent_running: false,
            agent_name: None,
            agent_session_id: None,
            agent_pane_id: None,
            listening_ports: Vec::new(),
            listening_ports_updated_at_unix_ms: None,
            socket_agent_activity: None,
            pane_agent_activity: HashMap::new(),
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

    fn stub_workspace_with_id(id: u32, name: &str, tabs: Vec<Tab>, active_tab: u32) -> Workspace {
        Workspace {
            id,
            work_origin: crate::workspace::new_workspace_work_origin(),
            name: name.to_string(),
            collapsed: false,
            repo_root: Some("/tmp/repo".to_string()),
            is_worktree: true,
            working_tree_path: Some("/tmp/repo-feature".to_string()),
            branch_name: Some("feature/reuse".to_string()),
            linked_issue: None,
            env_vars: HashMap::new(),
            run_status: WorkspaceStatus::Idle,
            tabs,
            active_tab,
            last_active_tab: None,
            tmux_backed: false,
            host_config_name: None,
            host_status: crate::probe::ProbeSnapshot::default(),
        }
    }

    fn stub_workspace(tabs: Vec<Tab>, active_tab: u32) -> Workspace {
        stub_workspace_with_id(7, "worktree", tabs, active_tab)
    }

    fn stub_app_state(workspaces: Vec<Workspace>, active_workspace: u32) -> AppState {
        let mut state = AppState::new();
        state.workspaces = workspaces;
        state.active_workspace = active_workspace;
        state.event_store = crate::events::EventStore::default();
        state
    }

    fn register_stub_terminal_tab(
        state: &Rc<RefCell<AppState>>,
        workspace_id: u32,
        name: &str,
    ) -> u32 {
        let runtime = RuntimeHandle::from_shared_state(state.clone());
        let _ = state.borrow_mut().activate_workspace(workspace_id);
        let tab_id = runtime.register_terminal_tab(TerminalTabRegistration {
            name: name.to_string(),
            panes: Box::new(PaneNode::Stub { pane_id: 0 }),
            focused_pane_id: 0,
            next_pane_id: 1,
            close_on_exit: true,
            respawn_on_exit: None,
        });
        assert!(runtime.set_workspace_active_tab(workspace_id, tab_id));
        tab_id
    }

    fn open_worktree_workspace_headless(
        state: &Rc<RefCell<AppState>>,
        worktree_path: &str,
    ) -> (u32, u32, bool) {
        match ensure_worktree_workspace_state(state, worktree_path) {
            WorktreeWorkspaceState::Existing {
                workspace_id,
                tab_id,
            } => (workspace_id, tab_id, false),
            WorktreeWorkspaceState::Created { workspace_id, .. } => {
                // The production path applies this discovery from the Git
                // worker. The headless test helper has no GTK loop, so mirror
                // that completed apply synchronously after construction.
                let git_info = crate::git::discover(worktree_path);
                let mut st = state.borrow_mut();
                let workspace = st
                    .workspaces
                    .iter_mut()
                    .find(|workspace| workspace.id == workspace_id)
                    .expect("new worktree workspace should exist");
                crate::apply_git_info_to_workspace(workspace, &git_info);
                workspace.is_worktree = true;
                workspace.working_tree_path = Some(worktree_path.to_string());
                drop(st);
                let tab_id = register_stub_terminal_tab(state, workspace_id, "Shell");
                (workspace_id, tab_id, true)
            }
        }
    }

    struct TempWorktree {
        repo_root: PathBuf,
        path: PathBuf,
        branch: String,
    }

    impl TempWorktree {
        fn new(repo_root: &Path, path: impl Into<PathBuf>, branch: &str) -> Self {
            Self {
                repo_root: repo_root.to_path_buf(),
                path: path.into(),
                branch: branch.to_string(),
            }
        }
    }

    impl Drop for TempWorktree {
        fn drop(&mut self) {
            let _ = crate::git::remove_worktree_from_repo(Some(&self.repo_root), &self.path);
            let _ = Command::new("git")
                .args(["branch", "-D", &self.branch])
                .current_dir(&self.repo_root)
                .output();
        }
    }

    #[test]
    fn resolve_tab_target_returns_unique_name_match() {
        let workspace_a = stub_workspace_with_id(
            11,
            "default",
            vec![
                stub_terminal_tab(101, "Shell", None),
                stub_terminal_tab(102, "Logs", None),
            ],
            101,
        );
        let workspace_b = stub_workspace_with_id(
            12,
            "feature",
            vec![stub_terminal_tab(201, "Shell", None)],
            201,
        );
        let state = stub_app_state(vec![workspace_a, workspace_b], 11);

        assert_eq!(
            resolve_tab_id_for_target_in_state(&state, Some("Logs")),
            Ok(102)
        );
    }

    #[test]
    fn resolve_tab_target_rejects_duplicate_name_matches_across_workspaces() {
        let workspace_a = stub_workspace_with_id(
            11,
            "default",
            vec![stub_terminal_tab(101, "Shell", None)],
            101,
        );
        let workspace_b = stub_workspace_with_id(
            12,
            "feature",
            vec![stub_terminal_tab(201, "shell", None)],
            201,
        );
        let state = stub_app_state(vec![workspace_a, workspace_b], 11);

        let err = resolve_tab_id_for_target_in_state(&state, Some("Shell"))
            .expect_err("duplicate tab names should be rejected");

        assert!(
            err.contains("ambiguous tab target \"Shell\""),
            "unexpected error: {err}"
        );
        assert!(err.contains("tab_id 101"), "unexpected error: {err}");
        assert!(err.contains("tab_id 201"), "unexpected error: {err}");
        assert!(
            err.contains("use numeric tab_id from list-tabs"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_tab_target_allows_current_and_numeric_ids_when_names_collide() {
        let workspace_a = stub_workspace_with_id(
            11,
            "default",
            vec![stub_terminal_tab(101, "Shell", None)],
            101,
        );
        let workspace_b = stub_workspace_with_id(
            12,
            "feature",
            vec![stub_terminal_tab(201, "Shell", None)],
            201,
        );
        let state = stub_app_state(vec![workspace_a, workspace_b], 12);

        assert_eq!(
            resolve_tab_id_for_target_in_state(&state, Some("current")),
            Ok(201)
        );
        assert_eq!(
            resolve_tab_id_for_target_in_state(&state, Some("101")),
            Ok(101)
        );
    }

    #[test]
    fn implicit_socket_actions_keep_targeting_terminal_while_dashboard_is_presented() {
        let workspace = stub_workspace_with_id(
            11,
            "default",
            vec![stub_terminal_tab(101, "Shell", None)],
            101,
        );
        let mut state = stub_app_state(vec![workspace], 11);
        let dashboard_id = state.create_dashboard_tab().expect("dashboard tab");

        assert_eq!(state.presented_tab_id(), Some(dashboard_id));
        assert!(matches!(
            state
                .find_tab(dashboard_id)
                .map(|(_, tab)| tab.panes.as_ref()),
            Some(PaneNode::Empty)
        ));
        assert_eq!(state.active_tab().map(|tab| tab.id), Some(101));
        // open-pane, run-in-pane, and send-keys all enter through this shared
        // resolver when --tab is omitted or set to "current".
        assert_eq!(resolve_tab_id_for_target_in_state(&state, None), Ok(101));
        assert_eq!(
            resolve_tab_id_for_target_in_state(&state, Some("current")),
            Ok(101)
        );
        assert_eq!(
            crate::api::build_state_snapshot(&state)["active_tab"],
            serde_json::json!(101)
        );
    }

    #[test]
    fn list_tabs_uses_tab_id_field_for_tabs() {
        let workspace = stub_workspace_with_id(
            11,
            "default",
            vec![stub_terminal_tab(101, "Shell", None)],
            101,
        );
        let state = Rc::new(RefCell::new(stub_app_state(vec![workspace], 11)));

        let response = handle_list_tabs(&state);
        assert!(response.ok);

        let data = response.data.expect("list-tabs should return data");
        let workspace = &data["workspaces"][0];
        let tab = &workspace["tabs"][0];

        assert_eq!(workspace["id"], serde_json::json!(11));
        assert_eq!(tab["tab_id"], serde_json::json!(101));
        assert_eq!(tab["name"], serde_json::json!("Shell"));
        assert!(
            tab.get("id").is_none(),
            "list-tabs tab entries should expose tab_id instead of id"
        );
    }

    #[test]
    fn query_state_includes_ports_alerts_and_capabilities() {
        let mut tab = stub_terminal_tab(101, "Shell", None);
        tab.agent_running = true;
        tab.agent_name = Some("claude".to_string());
        tab.set_pane_agent_activity(
            tab.focused_pane_id,
            AgentActivity::socket(
                AgentActivityState::Running,
                "editing socket.rs",
                Some("claude".to_string()),
            ),
        );
        tab.listening_ports = vec![3000, 8080];
        tab.needs_attention = true;
        tab.notification_msg = Some("Build done".to_string());

        let workspace = stub_workspace_with_id(11, "default", vec![tab], 101);
        let state = Rc::new(RefCell::new(stub_app_state(vec![workspace], 11)));
        state.borrow_mut().event_store.emit(
            "alert_raised",
            serde_json::json!({
                "tab_id": 101,
                "tab_name": "Shell",
                "message": "Build done",
            }),
        );

        let response = handle_query_state(&state);
        assert!(response.ok);

        let data = response.data.expect("query-state should return data");
        assert_eq!(data["schema"], serde_json::json!("taarof.state.v1"));
        assert_eq!(data["capabilities"]["events"], serde_json::json!(true));
        assert_eq!(data["capabilities"]["history"], serde_json::json!(false));
        assert_eq!(data["history"]["enabled"], serde_json::json!(false));
        assert_eq!(data["capabilities"]["agent_jobs"], serde_json::json!(true));
        let jobs = data["agent_jobs"]
            .as_array()
            .expect("agent_jobs should be array");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["tab_id"], serde_json::json!(101));
        assert_eq!(jobs[0]["agent_name"], serde_json::json!("claude"));
        assert_eq!(data["events"]["high_watermark"], serde_json::json!(1));
        assert_eq!(data["events"]["next_seq"], serde_json::json!(2));
        assert_eq!(data["events"]["capacity"], serde_json::json!(512));
        assert_eq!(data["events"]["dropped"], serde_json::json!(0));
        assert_eq!(data["active_ports"][0]["port"], serde_json::json!(3000));
        assert_eq!(data["alerts"][0]["tab_id"], serde_json::json!(101));
        assert_eq!(
            data["recent_alerts"][0]["event_type"],
            serde_json::json!("alert_raised")
        );
        assert!(data["health"].is_object());
        assert!(data["diagnostics"].is_object());
    }

    #[test]
    fn query_state_populates_agent_jobs_from_active_tabs() {
        let mut tab1 = stub_terminal_tab(101, "Agent tab", None);
        tab1.agent_running = true;
        tab1.agent_name = Some("claude".to_string());
        tab1.set_pane_agent_activity(
            tab1.focused_pane_id,
            AgentActivity::socket(
                AgentActivityState::Running,
                "editing main.rs",
                Some("claude".to_string()),
            ),
        );
        let tab2 = stub_terminal_tab(102, "Shell", None);

        let workspace = stub_workspace_with_id(11, "default", vec![tab1, tab2], 101);
        let state = Rc::new(RefCell::new(stub_app_state(vec![workspace], 11)));

        let response = handle_query_state(&state);
        let data = response.data.expect("query-state should return data");

        assert_eq!(data["capabilities"]["agent_jobs"], serde_json::json!(true));

        let jobs = data["agent_jobs"]
            .as_array()
            .expect("agent_jobs should be an array");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["tab_id"], serde_json::json!(101));
        assert_eq!(jobs[0]["tab_name"], serde_json::json!("Agent tab"));
        assert_eq!(jobs[0]["workspace_id"], serde_json::json!(11));
        assert_eq!(jobs[0]["agent_name"], serde_json::json!("claude"));
        assert_eq!(jobs[0]["activity"]["state"], serde_json::json!("running"));
        assert_eq!(
            jobs[0]["activity"]["text"],
            serde_json::json!("editing main.rs")
        );
    }

    #[test]
    fn query_events_respects_since_seq_and_limit() {
        let workspace = stub_workspace_with_id(
            11,
            "default",
            vec![stub_terminal_tab(101, "Shell", None)],
            101,
        );
        let state = Rc::new(RefCell::new(stub_app_state(vec![workspace], 11)));
        {
            let mut st = state.borrow_mut();
            st.event_store.emit("one", serde_json::json!({"n": 1}));
            st.event_store.emit("two", serde_json::json!({"n": 2}));
            st.event_store.emit("three", serde_json::json!({"n": 3}));
        }

        let response = handle_query_events(&state, Some(1), Some(1));
        assert!(response.ok);

        let data = response.data.expect("query-events should return data");
        assert_eq!(data["schema"], serde_json::json!("taarof.events.v1"));
        assert_eq!(data["limit"], serde_json::json!(1));
        assert_eq!(data["events"].as_array().map(Vec::len), Some(1));
        assert_eq!(data["events"][0]["event_type"], serde_json::json!("two"));
        assert_eq!(data["next_seq"], serde_json::json!(2));
        assert_eq!(data["high_watermark"], serde_json::json!(3));
    }

    #[test]
    fn query_actions_do_not_record_socket_events() {
        let workspace = stub_workspace_with_id(
            11,
            "default",
            vec![stub_terminal_tab(101, "Shell", None)],
            101,
        );
        let state = Rc::new(RefCell::new(stub_app_state(vec![workspace], 11)));

        record_socket_message_event(&state, &SocketMessage::QueryState);
        record_socket_message_event(
            &state,
            &SocketMessage::QueryEvents {
                since_seq: Some(1),
                limit: Some(10),
            },
        );
        record_socket_message_event(&state, &SocketMessage::ListTabs);

        let st = state.borrow();
        assert_eq!(st.event_store.len(), 0);
        assert_eq!(st.event_store.next_seq(), 1);
    }

    #[test]
    fn query_events_exposes_oldest_seq_for_stale_cursor_detection() {
        let workspace = stub_workspace_with_id(
            11,
            "default",
            vec![stub_terminal_tab(101, "Shell", None)],
            101,
        );
        let state = Rc::new(RefCell::new(stub_app_state(vec![workspace], 11)));
        {
            let mut st = state.borrow_mut();
            // Use a small-capacity store so overflow is easy to trigger
            st.event_store = crate::events::EventStore::with_capacity(2);
            st.event_store.emit("one", serde_json::json!({"n": 1}));
            st.event_store.emit("two", serde_json::json!({"n": 2}));
            st.event_store.emit("three", serde_json::json!({"n": 3}));
            // "one" (seq 1) was evicted; oldest retained is "two" (seq 2)
        }

        let response = handle_query_events(&state, Some(0), None);
        let data = response.data.expect("query-events should return data");

        // oldest_seq lets clients detect that seq 1 was lost
        assert_eq!(data["oldest_seq"], serde_json::json!(2));
        assert_eq!(data["high_watermark"], serde_json::json!(3));
        assert_eq!(data["gap"], serde_json::json!(true));
        assert_eq!(data["gap_from"], serde_json::json!(1));
        assert_eq!(data["gap_to"], serde_json::json!(1));
        assert_eq!(data["resnapshot_required"], serde_json::json!(true));

        // A client resuming from since_seq=0 only gets seq 2+
        let events = data["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["seq"], serde_json::json!(2));
    }

    #[test]
    fn clearing_one_pane_retargets_attention_to_another_alerting_pane() {
        let mut tab = stub_terminal_tab(101, "Shell", None);
        tab.set_pane_agent_activity(
            4,
            AgentActivity::socket(
                AgentActivityState::WaitingInput,
                "first review",
                Some("codex".to_string()),
            ),
        );
        tab.set_pane_agent_activity(
            5,
            AgentActivity::socket(
                AgentActivityState::WaitingInput,
                "second review",
                Some("claude".to_string()),
            ),
        );
        tab.needs_attention = true;
        tab.notification_pane_id = Some(5);
        tab.notification_msg = Some("claude (pane 5): second review".to_string());

        tab.set_pane_agent_activity(
            5,
            AgentActivity::socket(
                AgentActivityState::Running,
                "resumed",
                Some("claude".to_string()),
            ),
        );
        assert!(retarget_tab_attention_to_remaining_activity(&mut tab));
        assert!(tab.needs_attention);
        assert_eq!(tab.notification_pane_id, Some(4));
        assert_eq!(
            tab.notification_msg.as_deref(),
            Some("codex (pane 4): first review")
        );
    }

    #[test]
    fn clear_tab_attention_resets_alert_state_and_returns_message() {
        let mut tab = stub_terminal_tab(101, "Shell", None);
        tab.needs_attention = true;
        tab.notified = true;
        tab.notification_msg = Some("Build done".to_string());
        tab.notification_pane_id = Some(7);

        let event = alert_event_payload(&tab, "Build done".to_string(), "socket-agent-status");
        let message = clear_tab_attention(&mut tab);

        assert_eq!(message.as_deref(), Some("Build done"));
        assert!(!tab.needs_attention);
        assert!(!tab.notified);
        assert!(tab.notification_msg.is_none());
        assert!(tab.notification_pane_id.is_none());
        assert_eq!(event["pane_id"], serde_json::json!(7));
    }

    #[test]
    fn notify_background_tab_sets_sidebar_alert_state_until_focus() {
        let workspace = stub_workspace_with_id(
            11,
            "default",
            vec![
                stub_terminal_tab(101, "Focused", None),
                stub_terminal_tab(102, "Target", None),
            ],
            101,
        );
        let mut state = stub_app_state(vec![workspace], 11);

        let change = apply_notify_state(&mut state, Some(102), Some("Build done"))
            .expect("target tab should be notified");

        assert_eq!(change.tab_id, 102);
        assert_eq!(change.toast_message, "Build done");
        assert!(change.alert_payload.is_some());
        let (_, target) = state.find_tab(102).expect("target tab");
        assert!(target.needs_attention);
        assert_eq!(target.notification_msg.as_deref(), Some("Build done"));
        assert_eq!(
            target.primary_state(),
            crate::workspace::TabPrimaryState::Alert
        );

        state
            .activate_tab(101)
            .expect("focused tab remains activatable");
        // Agent activity owns the legacy fields and may update them while the
        // tab remains in the background. The socket notification itself must
        // nevertheless stay projected as an alert until focus consumes it.
        let target = state
            .all_tabs_mut()
            .find(|tab| tab.id == 102)
            .expect("target tab");
        target.needs_attention = false;
        target.notification_msg = None;
        let target = state.find_tab(102).expect("target tab").1;
        assert!(state.tab_needs_attention(target));
        assert_eq!(state.tab_notification_message(target), Some("Build done"));
        state.activate_tab(102).expect("target tab should activate");
        let (_, target) = state.find_tab(102).expect("target tab");
        assert!(!state.tab_needs_attention(target));
        assert!(state.tab_notification_message(target).is_none());
    }

    #[test]
    fn notify_dashboard_attention_is_consumed_when_presented() {
        let workspace = stub_workspace_with_id(
            11,
            "default",
            vec![
                stub_terminal_tab(101, "Focused", None),
                stub_dashboard_tab(102),
            ],
            101,
        );
        let mut state = stub_app_state(vec![workspace], 11);

        let change = apply_notify_state(&mut state, Some(102), Some("Build done"))
            .expect("dashboard should be notified");

        assert_eq!(change.tab_id, 102);
        let dashboard = state.find_tab(102).expect("dashboard tab").1;
        assert!(state.tab_needs_attention(dashboard));
        assert_eq!(
            state
                .all_tabs()
                .find(|tab| state.tab_needs_attention(tab))
                .map(|tab| tab.id),
            Some(102)
        );

        state
            .present_tab(102)
            .expect("dashboard should be presented");

        let dashboard = state.find_tab(102).expect("dashboard tab").1;
        assert!(!state.tab_needs_attention(dashboard));
        assert!(state.tab_notification_message(dashboard).is_none());
        assert!(state
            .all_tabs()
            .find(|tab| state.tab_needs_attention(tab))
            .is_none());
    }

    #[test]
    fn untargeted_notify_prefers_background_terminal_over_dashboard() {
        let workspace = stub_workspace_with_id(
            11,
            "default",
            vec![
                stub_terminal_tab(101, "Focused", None),
                stub_dashboard_tab(102),
                stub_terminal_tab(103, "Background", None),
            ],
            101,
        );
        let mut state = stub_app_state(vec![workspace], 11);

        let change = apply_notify_state(&mut state, None, Some("Build done"))
            .expect("background tab should be notified");

        assert_eq!(change.tab_id, 103);
        let dashboard = state.find_tab(102).expect("dashboard tab").1;
        assert!(!state.tab_needs_attention(dashboard));
        assert!(state.tab_notification_message(dashboard).is_none());
    }

    #[test]
    fn notify_focused_tab_is_toast_only() {
        let workspace = stub_workspace_with_id(
            11,
            "default",
            vec![stub_terminal_tab(101, "Focused", None)],
            101,
        );
        let state = Rc::new(RefCell::new(stub_app_state(vec![workspace], 11)));
        let target = "101".to_string();
        let message = "Look here".to_string();

        let change = match dispatch_notify_message(&state, Some(&target), Some(&message)) {
            Ok(Some(change)) => change,
            Ok(None) => panic!("focused tab should still receive a toast"),
            Err(_) => panic!("dispatch should resolve"),
        };

        assert_eq!(change.toast_message, "Look here");
        assert!(change.alert_payload.is_none());
        let state = state.borrow();
        let (_, tab) = state.find_tab(101).expect("focused tab");
        assert!(!tab.needs_attention);
        assert!(tab.notification_msg.is_none());
    }

    #[test]
    fn notify_dispatch_emits_background_alert_and_workspace_focus_consumes_it() {
        let workspace_a = stub_workspace_with_id(
            11,
            "default",
            vec![stub_terminal_tab(101, "Focused", None)],
            101,
        );
        let workspace_b = stub_workspace_with_id(
            12,
            "feature",
            vec![stub_terminal_tab(201, "Target", None)],
            201,
        );
        let state = Rc::new(RefCell::new(stub_app_state(
            vec![workspace_a, workspace_b],
            11,
        )));
        let target = "201".to_string();
        let message = "Review ready".to_string();

        let change = match dispatch_notify_message(&state, Some(&target), Some(&message)) {
            Ok(Some(change)) => change,
            Ok(None) => panic!("target should be notified"),
            Err(_) => panic!("dispatch should resolve"),
        };

        assert_eq!(change.tab_id, 201);
        assert_eq!(change.toast_message, "Review ready");
        assert!(state
            .borrow()
            .event_store
            .entries()
            .iter()
            .any(|event| event.event_type == "alert_raised"
                && event.payload["tab_id"] == serde_json::json!(201)));
        // Simulate agent-status clearing the legacy fields: every consumer is
        // still bound to the persistent socket projection.
        {
            let mut st = state.borrow_mut();
            let tab = st.find_tab_mut(201).expect("target tab");
            tab.needs_attention = false;
            tab.notification_msg = None;
        }
        {
            let st = state.borrow();
            let tab = st.find_tab(201).expect("target tab").1;
            assert!(st.tab_needs_attention(tab));
            assert_eq!(st.tab_notification_message(tab), Some("Review ready"));
        }
        let query = handle_query_state(&state);
        assert!(query.ok);
        assert!(query
            .data
            .as_ref()
            .is_some_and(|data| data["alerts"].as_array().is_some_and(|alerts| alerts
                .iter()
                .any(|alert| {
                    alert["tab_id"] == serde_json::json!(201)
                        && alert["message"] == serde_json::json!("Review ready")
                }))));

        state
            .borrow_mut()
            .activate_workspace(12)
            .expect("target workspace should activate");
        let st = state.borrow();
        let tab = st.find_tab(201).expect("target tab").1;
        assert!(!st.tab_needs_attention(tab));
        assert!(st.tab_notification_message(tab).is_none());
    }

    #[test]
    fn test_registry_path_in_runtime_dir() {
        let _guard = SESSION_ENV_LOCK
            .lock()
            .expect("env lock should not be poisoned");
        let previous = std::env::var_os("TAAROF_SESSION");
        std::env::remove_var("TAAROF_SESSION");

        let dir = PathBuf::from("/tmp/taarof-tests");
        assert_eq!(registry_path_in(&dir), dir.join("taarof-current.json"));

        match previous {
            Some(value) => std::env::set_var("TAAROF_SESSION", value),
            None => std::env::remove_var("TAAROF_SESSION"),
        }
    }

    #[test]
    fn test_registry_path_in_named_session_runtime_dir() {
        let _guard = SESSION_ENV_LOCK
            .lock()
            .expect("env lock should not be poisoned");
        let previous = std::env::var_os("TAAROF_SESSION");
        std::env::set_var("TAAROF_SESSION", "dev/tools");

        let dir = PathBuf::from("/tmp/taarof-tests");
        let key = crate::instance::session_storage_key().unwrap();
        assert_eq!(
            registry_path_in(&dir),
            dir.join(format!("taarof-current-{key}.json"))
        );

        match previous {
            Some(value) => std::env::set_var("TAAROF_SESSION", value),
            None => std::env::remove_var("TAAROF_SESSION"),
        }
    }

    #[test]
    fn long_named_session_socket_is_fixed_size_and_overlong_runtime_dir_fails_closed() {
        let _guard = SESSION_ENV_LOCK
            .lock()
            .expect("env lock should not be poisoned");
        let previous = std::env::var_os("TAAROF_SESSION");
        std::env::set_var("TAAROF_SESSION", "A".repeat(1_000));

        let standard = socket_path_in(Path::new("/run/user/1000"));
        let basename = standard.file_name().unwrap().to_string_lossy();
        assert_eq!(
            basename,
            format!(
                "taarof-s-c2e686823489ced2017f6059b8b23931-{}.sock",
                std::process::id()
            )
        );
        assert!(basename.len() <= 64);
        assert_eq!(validate_socket_path_length(&standard), Ok(()));

        let max_dir_len = 107usize - 1 - basename.len();
        let max_dir = PathBuf::from(format!("/{}", "r".repeat(max_dir_len - 1)));
        let boundary = socket_path_in(&max_dir);
        assert_eq!(boundary.as_os_str().as_encoded_bytes().len(), 107);
        assert_eq!(validate_socket_path_length(&boundary), Ok(()));

        let too_long_dir = PathBuf::from(format!("/{}", "r".repeat(max_dir_len)));
        let too_long = socket_path_in(&too_long_dir);
        assert_eq!(too_long.as_os_str().as_encoded_bytes().len(), 108);
        assert!(validate_socket_path_length(&too_long).is_err());
        assert!(
            bind_socket_listener(&too_long_dir).is_none(),
            "binding must fail closed before touching an overlong socket path"
        );

        match previous {
            Some(value) => std::env::set_var("TAAROF_SESSION", value),
            None => std::env::remove_var("TAAROF_SESSION"),
        }
    }

    #[test]
    fn runtime_dir_selection_refuses_missing_candidates() {
        assert_eq!(select_runtime_dir(false, None, None), None);
    }

    #[test]
    fn runtime_dir_selection_prefers_xdg_runtime_dir() {
        let xdg = PathBuf::from("/secure/runtime");
        let fallback = PathBuf::from("/run/user/1000");
        assert_eq!(
            select_runtime_dir(true, Some(xdg.clone()), Some(fallback)),
            Some(xdg)
        );
    }

    #[test]
    fn runtime_dir_selection_uses_fallback_when_xdg_is_unset() {
        let fallback = PathBuf::from("/run/user/1000");
        assert_eq!(
            select_runtime_dir(false, None, Some(fallback.clone())),
            Some(fallback)
        );
    }

    #[test]
    fn runtime_dir_selection_does_not_fallback_when_xdg_is_invalid() {
        let fallback = PathBuf::from("/run/user/1000");
        assert_eq!(select_runtime_dir(true, None, Some(fallback)), None);
    }

    #[test]
    fn runtime_dir_validation_rejects_tmp() {
        assert_eq!(
            validate_runtime_dir_attributes(Path::new("/tmp"), 1000, 0o700, true, 1000),
            Err(RuntimeDirIssue::InsecureTempDir)
        );
    }

    #[test]
    fn runtime_dir_validation_rejects_wrong_owner() {
        assert_eq!(
            validate_runtime_dir_attributes(Path::new("/run/user/1000"), 2000, 0o700, true, 1000),
            Err(RuntimeDirIssue::WrongOwner {
                expected: 1000,
                actual: 2000,
            })
        );
    }

    #[test]
    fn runtime_dir_validation_rejects_symlink_path() {
        let dir = unique_temp_dir("runtime-dir-symlink-target");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("permissions should be updated");
        let link = dir.with_file_name(format!(
            "{}-link",
            dir.file_name()
                .expect("temp dir should have a file name")
                .to_string_lossy()
        ));
        symlink(&dir, &link).expect("symlink should be created");

        let current_uid = unsafe { libc::getuid() } as u32;
        let reason = validate_runtime_dir(&link, current_uid)
            .expect_err("symlink runtime dir should be rejected");
        assert!(
            reason.contains("must not be a symlink"),
            "unexpected reason: {reason}"
        );

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn runtime_dir_validation_rejects_group_or_world_access() {
        assert_eq!(
            validate_runtime_dir_attributes(Path::new("/run/user/1000"), 1000, 0o755, true, 1000),
            Err(RuntimeDirIssue::PermissionsTooOpen { mode: 0o755 })
        );
    }

    #[test]
    fn runtime_dir_validation_rejects_missing_owner_execute() {
        assert_eq!(
            validate_runtime_dir_attributes(Path::new("/run/user/1000"), 1000, 0o600, true, 1000),
            Err(RuntimeDirIssue::OwnerPermissionsInsufficient { mode: 0o600 })
        );
    }

    #[test]
    fn runtime_dir_validation_rejects_missing_owner_write() {
        assert_eq!(
            validate_runtime_dir_attributes(Path::new("/run/user/1000"), 1000, 0o500, true, 1000),
            Err(RuntimeDirIssue::OwnerPermissionsInsufficient { mode: 0o500 })
        );
    }

    #[test]
    fn runtime_dir_validation_accepts_private_current_user_directory() {
        let dir = unique_temp_dir("runtime-dir-private");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("permissions should be updated");

        let current_uid = unsafe { libc::getuid() } as u32;
        assert_eq!(validate_runtime_dir(&dir, current_uid), Ok(()));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn runtime_dir_validation_rejects_non_private_filesystem_directory() {
        let dir = unique_temp_dir("runtime-dir-open");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("permissions should be updated");

        let current_uid = unsafe { libc::getuid() } as u32;
        let reason = validate_runtime_dir(&dir, current_uid)
            .expect_err("non-private runtime dir should be rejected");
        assert!(reason.contains("too open"), "unexpected reason: {reason}");

        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cleanup_stale_socket_registry_removes_dead_pid_file() {
        let dir = unique_temp_dir("stale-registry");
        let path = dir.join("taarof-current.json");
        let registry = SocketRegistry {
            pid: u32::MAX,
            socket_path: "/tmp/taarof-dead.sock".to_string(),
            identity: None,
        };
        std::fs::write(
            &path,
            serde_json::to_string(&registry).expect("registry should serialize"),
        )
        .expect("registry should be written");

        cleanup_stale_socket_registry_file(&path);

        assert!(!path.exists(), "stale registry should be removed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cleanup_stale_socket_registry_keeps_live_pid_file() {
        let dir = unique_temp_dir("live-registry");
        let path = dir.join("taarof-current.json");
        let registry = SocketRegistry {
            pid: std::process::id(),
            socket_path: "/tmp/taarof-live.sock".to_string(),
            identity: None,
        };
        std::fs::write(
            &path,
            serde_json::to_string(&registry).expect("registry should serialize"),
        )
        .expect("registry should be written");

        cleanup_stale_socket_registry_file(&path);

        assert!(path.exists(), "live registry should be preserved");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn process_exists_accepts_current_pid() {
        assert!(process_exists(std::process::id()));
    }

    #[test]
    fn validate_socket_command_input_accepts_shell_text() {
        let command = validate_socket_command_input("open-pane", "printf 'hi'\nexit 0")
            .expect("multi-line command should be allowed");
        assert_eq!(command, "printf 'hi'\nexit 0");
    }

    #[test]
    fn validate_socket_command_input_rejects_empty_payload() {
        let error =
            validate_socket_command_input("run-in-pane", "   ").expect_err("empty input must fail");
        assert_eq!(error, "run-in-pane command cannot be empty");
    }

    #[test]
    fn validate_socket_command_input_rejects_control_characters() {
        let error = validate_socket_command_input("create-tab", "echo hi\u{1b}[31m")
            .expect_err("escape sequences must fail");
        assert_eq!(
            error,
            "create-tab command contains unsupported control characters"
        );
    }

    #[test]
    fn validate_socket_command_input_rejects_carriage_return() {
        // A bare CR could inject an extra command line into the spawned shell.
        let error = validate_socket_command_input("run-in-pane", "echo hi\rrm -rf /")
            .expect_err("carriage return must fail");
        assert_eq!(
            error,
            "run-in-pane command must not contain carriage returns"
        );
    }

    #[test]
    fn validate_socket_command_input_rejects_crlf() {
        // CRLF must also be rejected (the CR is the injection vector).
        let error = validate_socket_command_input("open-pane", "echo hi\r\nmalicious")
            .expect_err("CRLF must fail");
        assert_eq!(error, "open-pane command must not contain carriage returns");
    }

    #[test]
    fn validate_get_text_scrollback_rejects_large_capture_requests() {
        let error = validate_get_text_scrollback(SOCKET_MAX_CAPTURE_SCROLLBACK_LINES + 1)
            .expect_err("oversized scrollback must fail");
        assert_eq!(
            error,
            format!(
                "scrollback exceeds max {} lines",
                SOCKET_MAX_CAPTURE_SCROLLBACK_LINES
            )
        );
    }

    #[test]
    fn socket_message_deserializes_notify() {
        let raw = r#"{"action":"notify","tab":"Shell 1","message":"Build done"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse notify");
        match parsed {
            SocketMessage::Notify { tab, message } => {
                assert_eq!(tab.as_deref(), Some("Shell 1"));
                assert_eq!(message.as_deref(), Some("Build done"));
            }
            _ => panic!("expected notify"),
        }
    }

    #[test]
    fn socket_message_deserializes_open_pane_full() {
        let raw = r#"{"action":"open-pane","tab":"Shell 1","command":"echo hi","direction":"horizontal","working_dir":"/tmp"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse open-pane");
        match parsed {
            SocketMessage::OpenPane {
                tab,
                command,
                direction,
                working_dir,
            } => {
                assert_eq!(tab.as_deref(), Some("Shell 1"));
                assert_eq!(command, "echo hi");
                assert_eq!(direction.as_deref(), Some("horizontal"));
                assert_eq!(working_dir.as_deref(), Some("/tmp"));
            }
            _ => panic!("expected open-pane"),
        }
    }

    #[test]
    fn socket_message_deserializes_open_pane_required_only() {
        let raw = r#"{"action":"open-pane","command":"echo hi"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse open-pane");
        match parsed {
            SocketMessage::OpenPane {
                tab,
                command,
                direction,
                working_dir,
            } => {
                assert!(tab.is_none());
                assert_eq!(command, "echo hi");
                assert!(direction.is_none());
                assert!(working_dir.is_none());
            }
            _ => panic!("expected open-pane"),
        }
    }

    #[test]
    fn agent_status_targeting_requires_explicit_pane_when_legacy_target_is_ambiguous() {
        assert_eq!(resolve_agent_status_pane_target(None, &[7]), Ok(7));
        assert_eq!(resolve_agent_status_pane_target(Some(8), &[7, 8]), Ok(8));
        assert_eq!(
            resolve_agent_status_pane_target(None, &[7, 8]),
            Err("agent-status requires pane targeting for a multi-pane tab".to_string())
        );
        assert_eq!(
            resolve_agent_status_pane_target(Some(9), &[7, 8]),
            Err("pane 9 not found in target tab".to_string())
        );
    }

    #[test]
    fn agent_status_source_must_match_the_targeted_detected_agent() {
        assert!(validate_agent_status_source(Some("codex"), Some("codex"), 7).is_ok());
        assert!(validate_agent_status_source(None, Some("codex"), 7).is_ok());
        assert_eq!(
            validate_agent_status_source(Some("claude"), Some("codex"), 7),
            Err("agent-status source \"claude\" does not match pane 7 agent \"codex\"".to_string())
        );
    }

    #[test]
    fn socket_message_deserializes_agent_status() {
        let raw = r#"{"action":"agent-status","tab":"current","pane":7,"state":"running","text":"Editing terminal.rs","source":"claude"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse agent-status");
        match parsed {
            SocketMessage::AgentStatus {
                tab,
                pane,
                state,
                text,
                source,
            } => {
                assert_eq!(tab.as_deref(), Some("current"));
                assert_eq!(pane, Some(7));
                assert!(matches!(state, SocketActivityState::Running));
                assert_eq!(text.as_deref(), Some("Editing terminal.rs"));
                assert_eq!(source.as_deref(), Some("claude"));
            }
            _ => panic!("expected agent-status"),
        }
    }

    #[test]
    fn socket_message_deserializes_waiting_input_agent_status() {
        let raw =
            r#"{"action":"agent-status","state":"waiting-input","text":"Waiting for user input"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse agent-status");
        match parsed {
            SocketMessage::AgentStatus { state, .. } => {
                assert!(matches!(state, SocketActivityState::WaitingInput));
            }
            _ => panic!("expected agent-status"),
        }
    }

    #[test]
    fn socket_message_deserializes_structured_work_reporting_contract() {
        let context: SocketMessage =
            serde_json::from_str(r#"{"action":"work-context","tab":"current","pane":3}"#).unwrap();
        assert!(matches!(
            context,
            SocketMessage::WorkContext {
                tab: Some(tab),
                pane: 3
            } if tab == "current"
        ));
        let report: SocketMessage = serde_json::from_str(
            r#"{"action":"work-report","milestone":"progress","note":"Parser tests passed","session":"default","workspace_origin":"workspace-a","tab_origin":"tab-a","pane_origin":"pane-a","task_id":"EXAMPLE-112","checkout_root":"/repo","binding_token":"ctx-11111111111111111111111111111111"}"#,
        )
        .unwrap();
        assert!(matches!(
            report,
            SocketMessage::WorkReport {
                milestone: crate::work_reporting::WorkMilestone::Progress,
                task_id,
                note: Some(note),
                ..
            } if task_id == "EXAMPLE-112" && note == "Parser tests passed"
        ));
    }

    #[test]
    fn socket_message_deserializes_agent_workspace_full() {
        let raw = r#"{"action":"agent-workspace","branch":"fix/issue-42","command":"claude-code 'fix the bug'","repo":"."}"#;
        let parsed: SocketMessage =
            serde_json::from_str(raw).expect("should parse agent-workspace");
        match parsed {
            SocketMessage::AgentWorkspace {
                branch,
                command,
                repo,
                timeout_seconds,
            } => {
                assert_eq!(branch, "fix/issue-42");
                assert_eq!(command.as_deref(), Some("claude-code 'fix the bug'"));
                assert_eq!(repo.as_deref(), Some("."));
                assert!(timeout_seconds.is_none());
            }
            _ => panic!("expected agent-workspace"),
        }
    }

    #[test]
    fn socket_message_deserializes_agent_workspace_minimal() {
        let raw = r#"{"action":"agent-workspace","branch":"feat/new-feature"}"#;
        let parsed: SocketMessage =
            serde_json::from_str(raw).expect("should parse agent-workspace");
        match parsed {
            SocketMessage::AgentWorkspace {
                branch,
                command,
                repo,
                timeout_seconds,
            } => {
                assert_eq!(branch, "feat/new-feature");
                assert!(command.is_none());
                assert!(repo.is_none());
                assert!(timeout_seconds.is_none());
            }
            _ => panic!("expected agent-workspace"),
        }
    }

    #[test]
    fn socket_message_agent_workspace_requires_branch() {
        let raw = r#"{"action":"agent-workspace"}"#;
        let err = serde_json::from_str::<SocketMessage>(raw).expect_err("branch is required");
        assert!(err.to_string().contains("branch"));
    }

    #[test]
    fn socket_message_agent_workspace_branch_with_slashes() {
        let raw = r#"{"action":"agent-workspace","branch":"feat/deep/nested/branch"}"#;
        let parsed: SocketMessage =
            serde_json::from_str(raw).expect("should parse agent-workspace");
        match parsed {
            SocketMessage::AgentWorkspace { branch, .. } => {
                assert_eq!(branch, "feat/deep/nested/branch");
            }
            _ => panic!("expected agent-workspace"),
        }
    }

    #[test]
    fn socket_message_unknown_action_errors() {
        let raw = r#"{"action":"do-thing","message":"nope"}"#;
        let err = serde_json::from_str::<SocketMessage>(raw).expect_err("must fail");
        assert!(err.to_string().contains("unknown variant"));
    }

    #[test]
    fn socket_message_malformed_json_errors() {
        let raw = r#"{"action":"notify","message":"oops""#;
        let err = serde_json::from_str::<SocketMessage>(raw).expect_err("must fail");
        // Truncated JSON yields an EOF error, not a syntax error; either way it must not parse.
        assert!(err.is_syntax() || err.is_eof());
    }

    #[test]
    fn socket_message_deserializes_close_pane() {
        let raw = r#"{"action":"close-pane","tab":"dev","pane":3}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse close-pane");
        match parsed {
            SocketMessage::ClosePane { tab, pane } => {
                assert_eq!(tab.as_deref(), Some("dev"));
                assert_eq!(pane, 3);
            }
            _ => panic!("expected close-pane"),
        }
    }

    #[test]
    fn socket_close_pane_surfaces_transient_kill_failure() {
        let response = super::close_pane_socket_response(Err(
            "Could not kill tmux session preserved-pane; preserved it in Background: ssh timeout"
                .to_string(),
        ));

        assert!(!response.ok);
        assert!(response
            .error
            .as_deref()
            .is_some_and(|error| error.contains("ssh timeout")));
    }

    #[test]
    fn socket_message_deserializes_run_in_pane() {
        let raw = r#"{"action":"run-in-pane","tab":"dev","pane":4,"command":"ls -la"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse run-in-pane");
        match parsed {
            SocketMessage::RunInPane { tab, pane, command } => {
                assert_eq!(tab.as_deref(), Some("dev"));
                assert_eq!(pane, 4);
                assert_eq!(command, "ls -la");
            }
            _ => panic!("expected run-in-pane"),
        }
    }

    #[test]
    fn socket_message_notify_backward_compatible() {
        let raw = r#"{"action":"notify","message":"still works"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse notify");
        match parsed {
            SocketMessage::Notify { tab, message } => {
                assert!(tab.is_none());
                assert_eq!(message.as_deref(), Some("still works"));
            }
            _ => panic!("expected notify"),
        }
    }

    // ── F010: Scripting API message tests ──

    #[test]
    fn socket_message_deserializes_create_tab_full() {
        let raw = r#"{"action":"create-tab","name":"dev","working_dir":"/tmp","command":"vim"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse create-tab");
        match parsed {
            SocketMessage::CreateTab {
                name,
                working_dir,
                command,
            } => {
                assert_eq!(name.as_deref(), Some("dev"));
                assert_eq!(working_dir.as_deref(), Some("/tmp"));
                assert_eq!(command.as_deref(), Some("vim"));
            }
            _ => panic!("expected create-tab"),
        }
    }

    #[test]
    fn socket_message_deserializes_create_tab_minimal() {
        let raw = r#"{"action":"create-tab"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse create-tab");
        match parsed {
            SocketMessage::CreateTab {
                name,
                working_dir,
                command,
            } => {
                assert!(name.is_none());
                assert!(working_dir.is_none());
                assert!(command.is_none());
            }
            _ => panic!("expected create-tab"),
        }
    }

    #[test]
    fn socket_message_deserializes_close_tab() {
        let raw = r#"{"action":"close-tab","tab":"Shell 1"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse close-tab");
        match parsed {
            SocketMessage::CloseTab { tab } => {
                assert_eq!(tab, "Shell 1");
            }
            _ => panic!("expected close-tab"),
        }
    }

    #[test]
    fn socket_message_deserializes_switch_tab() {
        let raw = r#"{"action":"switch-tab","tab":"3"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse switch-tab");
        match parsed {
            SocketMessage::SwitchTab { tab } => {
                assert_eq!(tab, "3");
            }
            _ => panic!("expected switch-tab"),
        }
    }

    #[test]
    fn socket_message_deserializes_rename_tab() {
        let raw = r#"{"action":"rename-tab","tab":"current","name":"my-tab"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse rename-tab");
        match parsed {
            SocketMessage::RenameTab { tab, name } => {
                assert_eq!(tab, "current");
                assert_eq!(name, "my-tab");
            }
            _ => panic!("expected rename-tab"),
        }
    }

    #[test]
    fn socket_message_deserializes_reorder_tab() {
        let raw = r#"{"action":"reorder-tab","tab":"3","index":0}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse reorder-tab");
        match parsed {
            SocketMessage::ReorderTab { tab, index } => {
                assert_eq!(tab, "3");
                assert_eq!(index, 0);
            }
            _ => panic!("expected reorder-tab"),
        }
    }

    #[test]
    fn socket_message_deserializes_create_workspace() {
        let raw = r#"{"action":"create-workspace","name":"feature"}"#;
        let parsed: SocketMessage =
            serde_json::from_str(raw).expect("should parse create-workspace");
        assert_eq!(socket_message_action(&parsed), "create-workspace");
        match parsed {
            SocketMessage::CreateWorkspace { name } => {
                assert_eq!(name.as_deref(), Some("feature"));
            }
            _ => panic!("expected create-workspace"),
        }
    }

    #[test]
    fn socket_message_deserializes_create_workspace_without_name() {
        let raw = r#"{"action":"create-workspace"}"#;
        let parsed: SocketMessage =
            serde_json::from_str(raw).expect("should parse create-workspace");
        match parsed {
            SocketMessage::CreateWorkspace { name } => assert!(name.is_none()),
            _ => panic!("expected create-workspace"),
        }
    }

    #[test]
    fn socket_message_deserializes_switch_workspace() {
        let raw = r#"{"action":"switch-workspace","workspace":"2"}"#;
        let parsed: SocketMessage =
            serde_json::from_str(raw).expect("should parse switch-workspace");
        assert_eq!(socket_message_action(&parsed), "switch-workspace");
        match parsed {
            SocketMessage::SwitchWorkspace { workspace } => assert_eq!(workspace, "2"),
            _ => panic!("expected switch-workspace"),
        }
    }

    #[test]
    fn socket_message_deserializes_rename_workspace() {
        let raw = r#"{"action":"rename-workspace","workspace":"feature","name":"renamed"}"#;
        let parsed: SocketMessage =
            serde_json::from_str(raw).expect("should parse rename-workspace");
        assert_eq!(socket_message_action(&parsed), "rename-workspace");
        match parsed {
            SocketMessage::RenameWorkspace { workspace, name } => {
                assert_eq!(workspace, "feature");
                assert_eq!(name, "renamed");
            }
            _ => panic!("expected rename-workspace"),
        }
    }

    #[test]
    fn workspace_socket_actions_are_not_read_only() {
        for raw in [
            r#"{"action":"create-workspace"}"#,
            r#"{"action":"switch-workspace","workspace":"1"}"#,
            r#"{"action":"rename-workspace","workspace":"1","name":"x"}"#,
        ] {
            let parsed: SocketMessage = serde_json::from_str(raw).expect("parse");
            assert!(
                !socket_message_is_read_only(&parsed),
                "workspace mutation must not be read-only: {raw}"
            );
        }
    }

    #[test]
    fn resolve_workspace_target_matches_numeric_id() {
        let workspace_a = stub_workspace_with_id(
            11,
            "default",
            vec![stub_terminal_tab(101, "Shell", None)],
            101,
        );
        let workspace_b = stub_workspace_with_id(
            12,
            "feature",
            vec![stub_terminal_tab(201, "Shell", None)],
            201,
        );
        let state = stub_app_state(vec![workspace_a, workspace_b], 11);

        assert_eq!(
            resolve_workspace_id_for_target_in_state(&state, "12"),
            Ok(12)
        );
        assert_eq!(
            resolve_workspace_id_for_target_in_state(&state, "feature"),
            Ok(12)
        );
    }

    #[test]
    fn resolve_workspace_target_rejects_unknown() {
        let workspace = stub_workspace_with_id(
            11,
            "default",
            vec![stub_terminal_tab(101, "Shell", None)],
            101,
        );
        let state = stub_app_state(vec![workspace], 11);

        assert_eq!(
            resolve_workspace_id_for_target_in_state(&state, "nope"),
            Err("workspace not found".to_string())
        );
        assert_eq!(
            resolve_workspace_id_for_target_in_state(&state, "99"),
            Err("workspace not found".to_string())
        );
    }

    #[test]
    fn resolve_workspace_target_rejects_duplicate_names() {
        let workspace_a =
            stub_workspace_with_id(11, "dev", vec![stub_terminal_tab(101, "Shell", None)], 101);
        let workspace_b =
            stub_workspace_with_id(12, "DEV", vec![stub_terminal_tab(201, "Shell", None)], 201);
        let state = stub_app_state(vec![workspace_a, workspace_b], 11);

        let err = resolve_workspace_id_for_target_in_state(&state, "dev")
            .expect_err("duplicate workspace names should be rejected");
        assert!(
            err.contains("ambiguous workspace target \"dev\""),
            "unexpected error: {err}"
        );
        assert!(err.contains("workspace_id 11"), "unexpected error: {err}");
        assert!(err.contains("workspace_id 12"), "unexpected error: {err}");
        assert!(
            err.contains("use the numeric workspace id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn socket_message_deserializes_list_tabs() {
        let raw = r#"{"action":"list-tabs"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse list-tabs");
        assert!(matches!(parsed, SocketMessage::ListTabs));
    }

    #[test]
    fn socket_message_deserializes_query_state() {
        let raw = r#"{"action":"query-state"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse query-state");
        assert!(matches!(parsed, SocketMessage::QueryState));
    }

    #[test]
    fn socket_message_deserializes_workspace_state_alias() {
        let raw = r#"{"action":"workspace-state"}"#;
        let parsed: SocketMessage =
            serde_json::from_str(raw).expect("should parse workspace-state alias");
        assert!(matches!(parsed, SocketMessage::QueryState));
    }

    #[test]
    fn socket_message_deserializes_query_events() {
        let raw = r#"{"action":"query-events","since_seq":41,"limit":25}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse query-events");
        match parsed {
            SocketMessage::QueryEvents { since_seq, limit } => {
                assert_eq!(since_seq, Some(41));
                assert_eq!(limit, Some(25));
            }
            _ => panic!("expected query-events"),
        }
    }

    #[test]
    fn socket_message_deserializes_and_classifies_query_history() {
        let raw = r#"{"action":"query-history","since_id":41,"limit":25,"record_type":"work","task":"EXAMPLE-133","severity":"warn","text":"blocked","order":"desc","scan_budget":1234}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse query-history");
        match &parsed {
            SocketMessage::QueryHistory {
                since_id,
                limit,
                filters,
            } => {
                assert_eq!(*since_id, Some(41));
                assert_eq!(*limit, Some(25));
                assert_eq!(filters.record_type.as_deref(), Some("work"));
                assert_eq!(filters.task.as_deref(), Some("EXAMPLE-133"));
                assert_eq!(filters.severity.as_deref(), Some("warn"));
                assert_eq!(filters.text.as_deref(), Some("blocked"));
                assert_eq!(filters.order, crate::history::HistoryOrder::Desc);
                assert_eq!(filters.scan_budget, Some(1234));
            }
            _ => panic!("expected query-history"),
        }
        assert!(socket_message_is_read_only(&parsed));
        assert_eq!(socket_message_action(&parsed), "query-history");
    }

    #[test]
    fn socket_message_deserializes_send_keys() {
        let raw = r#"{"action":"send-keys","pane":0,"keys":"hello\n"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse send-keys");
        match parsed {
            SocketMessage::SendKeys { tab, pane, keys } => {
                assert!(tab.is_none());
                assert_eq!(pane, 0);
                assert_eq!(keys, "hello\n");
            }
            _ => panic!("expected send-keys"),
        }
    }

    #[test]
    fn send_keys_preserves_raw_control_bytes() {
        // send-keys is intentionally byte-transparent: handle_send_keys forwards
        // `keys.as_bytes()` verbatim (no validate_socket_command_input gate) so
        // control characters like Ctrl-C (0x03) and CR (0x0d) can drive TUIs.
        // This guards against anyone sneaking sanitization into the send-keys
        // path. We assert the bytes handed to terminal.feed_child(keys.as_bytes())
        // survive the message layer untouched, including the exact control bytes
        // the command validator now rejects on run-in-pane/create-tab/open-pane.
        let raw = r#"{"action":"send-keys","pane":0,"keys":"\u0003abc\r\n"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse send-keys");
        match parsed {
            SocketMessage::SendKeys { keys, .. } => {
                assert_eq!(keys.as_bytes(), b"\x03abc\r\n");
            }
            _ => panic!("expected send-keys"),
        }
    }

    #[test]
    fn socket_message_deserializes_get_text() {
        let raw = r#"{"action":"get-text","tab":"dev","pane":0,"scrollback":100}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse get-text");
        match parsed {
            SocketMessage::GetText {
                tab,
                pane,
                scrollback,
                logical,
            } => {
                assert_eq!(tab.as_deref(), Some("dev"));
                assert_eq!(pane, 0);
                assert_eq!(scrollback, Some(100));
                // Omitted `logical` stays `None`; the dispatcher defaults it on.
                assert!(logical.is_none());
            }
            _ => panic!("expected get-text"),
        }
    }

    #[test]
    fn socket_message_deserializes_get_text_no_scrollback() {
        let raw = r#"{"action":"get-text","pane":0}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse get-text");
        match parsed {
            SocketMessage::GetText {
                tab,
                pane,
                scrollback,
                logical,
            } => {
                assert!(tab.is_none());
                assert_eq!(pane, 0);
                assert!(scrollback.is_none());
                assert!(logical.is_none());
            }
            _ => panic!("expected get-text"),
        }
    }

    #[test]
    fn socket_message_deserializes_get_text_logical_false() {
        let raw = r#"{"action":"get-text","pane":0,"logical":false}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse get-text");
        match parsed {
            SocketMessage::GetText { pane, logical, .. } => {
                assert_eq!(pane, 0);
                assert_eq!(logical, Some(false));
            }
            _ => panic!("expected get-text"),
        }
    }

    #[test]
    fn close_tab_plan_rejects_last_tab() {
        let result = close_tab_plan(1, 7, 1, false);
        assert_eq!(result, Err("cannot close last tab"));
    }

    #[test]
    fn close_tab_plan_closes_worktree_workspace_non_interactively() {
        let result = close_tab_plan(3, 9, 1, true);
        assert_eq!(result, Ok(CloseTabPlan::CloseWorkspace { workspace_id: 9 }));
    }

    #[test]
    fn close_tab_plan_closes_normal_tab_directly() {
        let result = close_tab_plan(3, 9, 1, false);
        assert_eq!(result, Ok(CloseTabPlan::CloseTab));
    }

    #[test]
    fn tab_close_detach_registers_session_before_teardown() {
        let state = Rc::new(RefCell::new(AppState::new()));
        let workspace_id = state.borrow().active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "close-detach",
            crate::HeadlessPaneSeed {
                tmux_session: Some("taarof-close-detach".to_string()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless tmux tab");
        crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "survivor",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("survivor tab");

        let prepared = prepare_tab_close(&state, tab_id, crate::config::TmuxCloseBehavior::Detach)
            .expect("shared tab-close preparation");

        assert_eq!(prepared.plan, CloseTabPlan::CloseTab);
        assert_eq!(prepared.detached_sessions, vec!["taarof-close-detach"]);
        assert!(prepared.tmux_backings_to_close.is_empty());
        let st = state.borrow();
        assert!(st
            .headless_pane(tab_id, pane_id)
            .is_some_and(|pane| pane.tmux_backing.is_none()));
        let payload = detached_session_list_payload(&st);
        assert_eq!(payload.len(), 1);
        assert_eq!(payload[0]["session_name"], "taarof-close-detach");
        assert_eq!(payload[0]["is_detached"], true);
        let snapshot = crate::api::build_state_snapshot(&st);
        assert_eq!(
            snapshot["detached_sessions"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(
            snapshot["detached_sessions"][0]["session_name"],
            "taarof-close-detach"
        );
        let dashboard = crate::dashboard::aggregate_dashboard_state(
            &[],
            &st.detached_sessions,
            &[],
            &HashMap::new(),
            "taarof",
        );
        assert_eq!(dashboard.sessions.len(), 1);
        assert!(dashboard.sessions[0].is_detached);
        assert_eq!(
            dashboard.sessions[0].status,
            crate::dashboard::SessionStatus::Detached
        );
    }

    #[test]
    fn tab_close_close_mode_shared_path_preserves_tracker_and_backing() {
        let state = Rc::new(RefCell::new(AppState::new()));
        let workspace_id = state.borrow().active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "close-mode",
            crate::HeadlessPaneSeed {
                tmux_session: Some("taarof-close-mode".to_string()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless tmux tab");
        crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "survivor",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("survivor tab");

        let prepared = prepare_tab_close(&state, tab_id, crate::config::TmuxCloseBehavior::Close)
            .expect("shared tab-close preparation");

        assert_eq!(prepared.plan, CloseTabPlan::CloseTab);
        assert!(prepared.detached_sessions.is_empty());
        assert_eq!(prepared.tmux_backings_to_close.len(), 1);
        assert_eq!(
            prepared.tmux_backings_to_close[0].session_name,
            "taarof-close-mode"
        );
        let st = state.borrow();
        assert!(st.detached_sessions.is_empty());
        assert_eq!(
            st.headless_pane(tab_id, pane_id)
                .and_then(|pane| pane.tmux_backing.as_ref())
                .map(|backing| backing.session_name.as_str()),
            Some("taarof-close-mode")
        );
    }

    #[test]
    fn socket_close_preflight_captures_backing_without_tombstoning() {
        let state = Rc::new(RefCell::new(AppState::new()));
        let workspace_id = state.borrow().active_workspace;
        let (tab_id, _) = crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "socket-close-inflight",
            crate::HeadlessPaneSeed {
                tmux_session: Some("taarof-socket-close-inflight".to_string()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless tmux tab");
        crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "survivor",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("survivor tab");
        let poll_context = state
            .borrow_mut()
            .begin_dashboard_poll(&[crate::tmux::TmuxTarget::Local]);
        let prepared = prepare_tab_close(&state, tab_id, crate::config::TmuxCloseBehavior::Close)
            .expect("socket close preparation");

        assert_eq!(prepared.tmux_backings_to_close.len(), 1);
        assert!(state
            .borrow()
            .dashboard_poll_tracker
            .result_is_current(&poll_context, &crate::tmux::TmuxTarget::Local));
    }

    struct ImmediateSuccessfulTmuxAdapter;

    impl crate::tmux::TmuxCommandAdapter for ImmediateSuccessfulTmuxAdapter {
        fn execute(&self, _argv: &[String], _timeout: Duration) -> crate::tmux::TmuxCommandOutcome {
            crate::tmux::TmuxCommandOutcome {
                success: true,
                status: Some("exit status: 0".to_string()),
                stdout: String::new(),
                stderr: String::new(),
                error: None,
            }
        }
    }

    fn dispatch_headless_close_through_production(
        state: &Rc<RefCell<AppState>>,
        tab_id: u32,
        close_behavior: crate::config::TmuxCloseBehavior,
    ) -> SocketResponse {
        let _glib_guard = crate::glib_main_context_test_guard();
        let worker = crate::tmux::TmuxWorker::with_adapter(
            std::sync::Arc::new(ImmediateSuccessfulTmuxAdapter),
            1,
            8,
        );
        let dispatch =
            SocketDispatchContext::headless(worker, close_behavior, Rc::new(Cell::new(0)), None);
        let (response_tx, response_rx) = mpsc::channel();
        dispatch_socket_message(
            state,
            &dispatch,
            SocketMessage::CloseTab {
                tab: tab_id.to_string(),
            },
            response_tx,
            None,
        );

        let context = glib::MainContext::default();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            while context.pending() {
                context.iteration(false);
            }
            if let Ok(response) = response_rx.try_recv() {
                return response;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "production close dispatch did not reply"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn headless_close_policy_tombstones_inflight_dashboard_poll() {
        let state = Rc::new(RefCell::new(AppState::new()));
        let workspace_id = state.borrow().active_workspace;
        let (tab_id, _) = crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "close-inflight",
            crate::HeadlessPaneSeed {
                tmux_session: Some("taarof-close-inflight".to_string()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless tmux tab");
        crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "survivor",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("survivor tab");
        state.borrow_mut().dashboard_state = crate::dashboard::aggregate_dashboard_state(
            &[],
            &[],
            &[(
                crate::tmux::TmuxTarget::Local,
                vec![("taarof-close-inflight".to_string(), 1, false, 1)],
            )],
            &HashMap::new(),
            "taarof",
        );
        let poll_context = state
            .borrow_mut()
            .begin_dashboard_poll(&[crate::tmux::TmuxTarget::Local]);

        let response = dispatch_headless_close_through_production(
            &state,
            tab_id,
            crate::config::TmuxCloseBehavior::Close,
        );

        assert!(response.ok, "headless close failed: {:?}", response.error);
        let st = state.borrow();
        assert!(st.find_tab(tab_id).is_none());
        assert!(st.dashboard_state.sessions.is_empty());
        assert!(!st
            .dashboard_poll_tracker
            .result_is_current(&poll_context, &crate::tmux::TmuxTarget::Local));
    }

    #[test]
    fn headless_detach_policy_does_not_invalidate_or_kill_poll_target() {
        let state = Rc::new(RefCell::new(AppState::new()));
        let workspace_id = state.borrow().active_workspace;
        let (tab_id, _) = crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "detach-inflight",
            crate::HeadlessPaneSeed {
                tmux_session: Some("taarof-detach-inflight".to_string()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless tmux tab");
        crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "survivor",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("survivor tab");
        let poll_context = state
            .borrow_mut()
            .begin_dashboard_poll(&[crate::tmux::TmuxTarget::Local]);

        let response = dispatch_headless_close_through_production(
            &state,
            tab_id,
            crate::config::TmuxCloseBehavior::Detach,
        );

        assert!(response.ok, "headless detach failed: {:?}", response.error);
        let st = state.borrow();
        assert!(st.find_tab(tab_id).is_none());
        assert_eq!(st.detached_sessions.len(), 1);
        assert_eq!(
            st.detached_sessions[0].session_name,
            "taarof-detach-inflight"
        );
        assert!(st
            .dashboard_poll_tracker
            .result_is_current(&poll_context, &crate::tmux::TmuxTarget::Local));
    }

    #[test]
    fn tab_close_detach_deduplicates_same_session_target_across_panes() {
        let state = Rc::new(RefCell::new(AppState::new()));
        let workspace_id = state.borrow().active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "duplicate-close-detach",
            crate::HeadlessPaneSeed {
                tmux_session: Some("shared-pane-session".to_string()),
                tmux_ssh_target: Some("builder@example".to_string()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless tmux tab");
        let duplicate_pane_id = pane_id + 1000;
        let duplicate = state
            .borrow()
            .headless_pane(tab_id, pane_id)
            .expect("seeded pane")
            .clone();
        state
            .borrow_mut()
            .headless_panes
            .insert((tab_id, duplicate_pane_id), duplicate);

        let registered = crate::terminal::register_detached_tab(&state, tab_id)
            .expect("tab-close detach registration");

        assert_eq!(registered, vec!["shared-pane-session"]);
        let st = state.borrow();
        assert_eq!(st.detached_sessions.len(), 1);
        assert!(st
            .headless_pane(tab_id, pane_id)
            .is_some_and(|pane| pane.tmux_backing.is_none()));
        assert!(st
            .headless_pane(tab_id, duplicate_pane_id)
            .is_some_and(|pane| pane.tmux_backing.is_none()));
        assert!(super::headless::resolve_detached_session_for_attach(
            &st.detached_sessions,
            "shared-pane-session",
            None,
            None,
        )
        .is_ok());
    }

    #[test]
    fn explicit_detach_pane_still_registers_session() {
        let state = Rc::new(RefCell::new(AppState::new()));
        let workspace_id = state.borrow().active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "explicit-detach",
            crate::HeadlessPaneSeed {
                tmux_session: Some("taarof-explicit-detach".to_string()),
                tmux_ssh_target: Some("dev.example".to_string()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless tmux tab");

        let response = super::headless::handle_headless_detach_pane(
            &state,
            Some(&tab_id.to_string()),
            pane_id,
        );

        assert!(response.ok, "explicit detach failed: {:?}", response.error);
        let st = state.borrow();
        assert_eq!(st.detached_sessions.len(), 1);
        assert_eq!(
            st.detached_sessions[0].session_name,
            "taarof-explicit-detach"
        );
        assert_eq!(st.detached_sessions[0].host, "dev.example");
        assert!(st
            .headless_pane(tab_id, pane_id)
            .is_some_and(|pane| pane.tmux_backing.is_none()));
    }

    #[test]
    fn resolve_agent_workspace_repo_prefers_explicit_repo() {
        let repo = TempRepo::new("resolve-explicit");
        let nested = repo.root.join("src").join("nested");
        std::fs::create_dir_all(&nested).expect("nested dir should be created");

        let resolved = resolve_agent_workspace_repo_path(
            Some(nested.to_str().expect("utf-8 nested path")),
            None,
            None,
        )
        .expect("explicit repo path should resolve");

        assert_eq!(resolved, repo.root);
    }

    #[test]
    fn resolve_agent_workspace_repo_rejects_missing_explicit_path() {
        let missing = unique_temp_dir("resolve-missing").join("does-not-exist");
        let err = resolve_agent_workspace_repo_path(
            Some(missing.to_str().expect("utf-8 missing path")),
            None,
            None,
        )
        .expect_err("missing repo path should fail");

        assert!(err.contains("does not exist"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_agent_workspace_repo_uses_workspace_context() {
        let repo = TempRepo::new("resolve-workspace");

        let resolved = resolve_agent_workspace_repo_path(
            None,
            Some(repo.root.to_str().expect("utf-8 repo path")),
            None,
        )
        .expect("workspace context should resolve");

        assert_eq!(resolved, repo.root);
    }

    #[test]
    fn resolve_agent_workspace_repo_keeps_worktree_context() {
        let repo = TempRepo::new("resolve-worktree");
        let worktree_path = crate::git::create_worktree(&repo.root, "feature/socket-api")
            .expect("worktree should be created");

        let resolved = resolve_agent_workspace_repo_path(None, Some(&worktree_path), None)
            .expect("worktree context should resolve to that checkout");

        assert_eq!(resolved, PathBuf::from(worktree_path));
    }

    #[test]
    fn resolve_agent_workspace_repo_requires_some_context() {
        let err = resolve_agent_workspace_repo_path(None, None, None)
            .expect_err("missing repo context should fail");

        assert!(err.contains("no repo context"), "unexpected error: {err}");
    }

    #[test]
    fn ensure_agent_worktree_reuses_existing_branch_worktree() {
        let repo = TempRepo::new("ensure-worktree-reuse");

        let (first_path, first_created) = ensure_agent_worktree(&repo.root, "feature/reuse")
            .expect("first worktree creation should succeed");
        let (second_path, second_created) = ensure_agent_worktree(&repo.root, "feature/reuse")
            .expect("second worktree lookup should succeed");

        assert!(first_created);
        assert!(!second_created);
        assert_eq!(first_path, second_path);
    }

    #[test]
    fn ensure_agent_worktree_rejects_primary_checkout_branch() {
        let repo = TempRepo::new("ensure-worktree-primary-checkout");
        let err = ensure_agent_worktree(&repo.root, "main")
            .expect_err("primary checkout branch should not be reused as a worktree");

        assert!(err.contains("primary checkout"), "unexpected error: {err}");
    }

    #[test]
    fn preferred_agent_workspace_tab_ignores_active_non_shell_tab() {
        let workspace = stub_workspace(
            vec![stub_terminal_tab(11, "Shell", None), stub_dashboard_tab(12)],
            12,
        );

        assert_eq!(preferred_agent_workspace_tab_id(&workspace), Some(11));
    }

    #[test]
    fn preferred_agent_workspace_tab_skips_workspace_action_tabs() {
        let workspace = stub_workspace(
            vec![stub_terminal_tab(11, "Shell", Some(WorkspaceAction::Dev))],
            11,
        );

        assert_eq!(preferred_agent_workspace_tab_id(&workspace), None);
    }

    #[test]
    fn agent_workspace_headless_harness_covers_create_reuse_and_cleanup() {
        let repo = TempRepo::new("agent-workspace-headless");
        let state = Rc::new(RefCell::new(AppState::new()));

        let default_workspace_id = state.borrow().active_workspace;
        let default_tab_id = register_stub_terminal_tab(&state, default_workspace_id, "Main");

        let repo_root = resolve_agent_workspace_repo_path(
            Some(repo.root.to_str().expect("utf-8 repo path")),
            None,
            None,
        )
        .expect("explicit repo path should resolve");
        let branch = "feature/agent-workspace-headless";
        let (worktree_path, created_worktree) =
            ensure_agent_worktree(&repo_root, branch).expect("worktree should be created");
        assert!(created_worktree);
        let worktree_guard = TempWorktree::new(&repo.root, &worktree_path, branch);

        let (workspace_id, tab_id, created_workspace) =
            open_worktree_workspace_headless(&state, &worktree_path);
        assert!(created_workspace);
        assert_ne!(workspace_id, default_workspace_id);
        assert_ne!(tab_id, default_tab_id);

        {
            let st = state.borrow();
            let workspace = st
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .expect("worktree workspace should exist");
            assert_eq!(
                workspace.repo_root.as_deref(),
                Some(repo.root.to_str().expect("utf-8 repo path"))
            );
            assert_eq!(
                workspace.working_tree_path.as_deref(),
                Some(worktree_path.as_str())
            );
            assert_eq!(workspace.branch_name.as_deref(), Some(branch));
            assert!(workspace.is_worktree);
            assert_eq!(workspace.tabs.len(), 1);
            assert_eq!(workspace.active_tab, tab_id);
            assert_eq!(st.active_workspace, workspace_id);
        }

        let query_state = handle_query_state(&state);
        assert!(query_state.ok);
        let snapshot_workspace = query_state
            .data
            .as_ref()
            .and_then(|data| data["workspaces"].as_array())
            .and_then(|workspaces| {
                workspaces
                    .iter()
                    .find(|workspace| workspace["id"] == serde_json::json!(workspace_id))
            })
            .expect("query-state should include the worktree workspace");
        assert_eq!(
            snapshot_workspace["working_tree_path"],
            serde_json::json!(worktree_path)
        );
        assert_eq!(snapshot_workspace["branch_name"], serde_json::json!(branch));
        assert_eq!(snapshot_workspace["tab_count"], serde_json::json!(1));
        assert_eq!(snapshot_workspace["active_tab"], serde_json::json!(tab_id));

        let (reused_path, reused_worktree_created) =
            ensure_agent_worktree(&repo_root, branch).expect("worktree should be reused");
        assert_eq!(reused_path, worktree_path);
        assert!(!reused_worktree_created);

        let (reused_workspace_id, reused_tab_id, created_workspace_again) =
            open_worktree_workspace_headless(&state, &reused_path);
        assert!(!created_workspace_again);
        assert_eq!(reused_workspace_id, workspace_id);
        assert_eq!(reused_tab_id, tab_id);

        let total_tabs = {
            let st = state.borrow();
            st.workspaces
                .iter()
                .map(|workspace| workspace.tabs.len())
                .sum()
        };
        let close_plan = {
            let st = state.borrow();
            let workspace = st
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .expect("worktree workspace should still exist");
            close_tab_plan(
                total_tabs,
                workspace.id,
                workspace.tabs.len(),
                workspace.is_worktree,
            )
            .expect("worktree workspace should close non-interactively")
        };
        assert_eq!(close_plan, CloseTabPlan::CloseWorkspace { workspace_id });

        state.borrow_mut().remove_workspace(workspace_id);
        assert!(
            worktree_guard.path.exists(),
            "worktree directory should remain on disk"
        );

        let query_state_after_close = handle_query_state(&state);
        assert!(query_state_after_close.ok);
        let remaining_workspaces = query_state_after_close
            .data
            .as_ref()
            .and_then(|data| data["workspaces"].as_array())
            .expect("query-state should include workspaces");
        assert!(remaining_workspaces
            .iter()
            .all(|workspace| workspace["id"] != serde_json::json!(workspace_id)));
        assert_eq!(state.borrow().active_workspace, default_workspace_id);
    }

    #[test]
    fn test_parse_create_tmux_tab_minimal() {
        let raw = r#"{"action":"create-tmux-tab"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse");
        match parsed {
            SocketMessage::CreateTmuxTab {
                name,
                host,
                working_dir,
            } => {
                assert!(name.is_none());
                assert!(host.is_none());
                assert!(working_dir.is_none());
            }
            _ => panic!("expected CreateTmuxTab"),
        }
    }

    #[test]
    fn test_parse_create_tmux_tab_with_host() {
        let raw = r#"{"action":"create-tmux-tab","name":"dev","host":"devbox-1","working_dir":"/tmp/user"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse");
        match parsed {
            SocketMessage::CreateTmuxTab {
                name,
                host,
                working_dir,
            } => {
                assert_eq!(name.as_deref(), Some("dev"));
                assert_eq!(host.as_deref(), Some("devbox-1"));
                assert_eq!(working_dir.as_deref(), Some("/tmp/user"));
            }
            _ => panic!("expected CreateTmuxTab"),
        }
    }

    #[test]
    fn test_parse_attach_session_with_target_identity() {
        let raw = r#"{"action":"attach-session","session_name":"taarof-smoke","host":"ci-box","ssh_target":"builder@ci-box"}"#;
        let parsed: SocketMessage = serde_json::from_str(raw).expect("should parse");
        match parsed {
            SocketMessage::AttachSession {
                session_name,
                host,
                ssh_target,
            } => {
                assert_eq!(session_name, "taarof-smoke");
                assert_eq!(host.as_deref(), Some("ci-box"));
                assert_eq!(ssh_target.as_deref(), Some("builder@ci-box"));
            }
            _ => panic!("expected AttachSession"),
        }
    }

    #[test]
    fn resolve_detached_session_for_attach_rejects_ambiguous_session_names() {
        let detached_sessions = vec![
            crate::dashboard::DetachedSession {
                session_name: "shared".to_string(),
                host: "localhost".to_string(),
                workspace: "default".to_string(),
                target: crate::tmux::TmuxTarget::Local,
                detached_at: std::time::Instant::now(),
                last_command: None,
                finished: false,
            },
            crate::dashboard::DetachedSession {
                session_name: "shared".to_string(),
                host: "ci-box".to_string(),
                workspace: "remote".to_string(),
                target: crate::tmux::TmuxTarget::Remote {
                    ssh_target: "builder@ci-box".to_string(),
                },
                detached_at: std::time::Instant::now(),
                last_command: None,
                finished: false,
            },
        ];

        let error = resolve_detached_session_for_attach(&detached_sessions, "shared", None, None)
            .expect_err("ambiguous session should be rejected");
        assert!(error.contains("ambiguous"));

        let resolved = resolve_detached_session_for_attach(
            &detached_sessions,
            "shared",
            Some("ci-box"),
            Some("builder@ci-box"),
        )
        .expect("fully-qualified target should resolve");
        assert_eq!(resolved.host, "ci-box");
        assert_eq!(
            resolved.target,
            crate::tmux::TmuxTarget::Remote {
                ssh_target: "builder@ci-box".to_string()
            }
        );
    }

    #[test]
    fn query_state_workspace_tab_pane_shape_contract() {
        let workspace = stub_workspace_with_id(
            11,
            "default",
            vec![stub_terminal_tab(101, "Shell", None)],
            101,
        );
        let state = Rc::new(RefCell::new(stub_app_state(vec![workspace], 11)));

        let response = handle_query_state(&state);
        let data = response.data.expect("query-state should return data");

        // Top-level keys
        assert!(data.get("schema").is_some());
        assert!(data.get("generated_at_unix_ms").is_some());
        assert!(data.get("session_name").is_some());
        assert!(data.get("active_workspace").is_some());
        assert!(data.get("active_tab").is_some());
        assert!(data.get("capabilities").is_some());
        assert!(data.get("health").is_some());
        assert!(data.get("diagnostics").is_some());
        assert!(data.get("workspaces").is_some());
        assert!(data.get("active_ports").is_some());
        assert!(data.get("alerts").is_some());
        assert!(data.get("recent_alerts").is_some());
        assert!(data.get("agent_jobs").is_some());
        assert!(data.get("dashboard").is_some());
        assert!(data.get("detached_sessions").is_some());
        assert!(data.get("events").is_some());

        // Workspace shape
        let ws = &data["workspaces"][0];
        for key in &[
            "id",
            "name",
            "active_tab",
            "collapsed",
            "repo_root",
            "branch_name",
            "is_worktree",
            "run_status",
            "tmux_backed",
            "tab_count",
            "tabs",
        ] {
            assert!(ws.get(key).is_some(), "workspace missing key: {key}");
        }

        // Tab shape
        let tab = &ws["tabs"][0];
        for key in &[
            "tab_id",
            "name",
            "kind",
            "focused_pane",
            "agent_running",
            "agent_name",
            "needs_attention",
            "notification_msg",
            "listening_ports",
            "panes",
        ] {
            assert!(tab.get(key).is_some(), "tab missing key: {key}");
        }

        // Pane shape — PaneNode::Stub returns no leaves in tests (requires GTK
        // widget for real panes), so we only verify the "panes" key is an array.
        assert!(tab["panes"].is_array(), "tab should have a panes array");
    }

    #[test]
    fn query_events_response_shape_contract() {
        let workspace = stub_workspace_with_id(
            11,
            "default",
            vec![stub_terminal_tab(101, "Shell", None)],
            101,
        );
        let state = Rc::new(RefCell::new(stub_app_state(vec![workspace], 11)));
        state
            .borrow_mut()
            .event_store
            .emit("test_event", serde_json::json!({"key": "value"}));

        let response = handle_query_events(&state, None, None);
        let data = response.data.expect("query-events should return data");

        // Top-level keys
        for key in &[
            "schema",
            "since_seq",
            "limit",
            "next_seq",
            "high_watermark",
            "oldest_seq",
            "gap",
            "gap_from",
            "gap_to",
            "resnapshot_required",
            "events",
        ] {
            assert!(
                data.get(key).is_some(),
                "events response missing key: {key}"
            );
        }

        // Event record shape
        let event = &data["events"][0];
        for key in &["seq", "ts_unix_ms", "event_type", "payload"] {
            assert!(event.get(key).is_some(), "event record missing key: {key}");
        }
    }

    #[test]
    fn pairing_messages_parse_and_classify() {
        let cases = [
            (
                serde_json::json!({"action": "pairing-offer"}),
                "pairing-offer",
                false,
            ),
            (
                serde_json::json!({"action": "pairing-pending"}),
                "pairing-pending",
                true,
            ),
            (
                serde_json::json!({"action": "pairing-confirm", "offer_id": "o1"}),
                "pairing-confirm",
                false,
            ),
            (
                serde_json::json!({"action": "pairing-reject", "offer_id": "o1"}),
                "pairing-reject",
                false,
            ),
            (
                serde_json::json!({"action": "device-revoke", "device_id": "d1"}),
                "device-revoke",
                false,
            ),
        ];
        for (json, action, read_only) in cases {
            let msg: SocketMessage =
                serde_json::from_value(json).expect("pairing message should deserialize");
            assert_eq!(socket_message_action(&msg), action);
            assert_eq!(
                socket_message_is_read_only(&msg),
                read_only,
                "unexpected read-only classification for {action}"
            );
        }
    }

    #[test]
    fn pairing_offer_pending_confirm_revoke_round_trip() {
        let state = Rc::new(RefCell::new(AppState::new()));

        // Operator creates an offer.
        let offer = handle_pairing_offer(&state);
        assert!(offer.ok, "offer creation should succeed");
        let offer_id = offer.data.as_ref().unwrap()["offer_id"]
            .as_str()
            .unwrap()
            .to_string();

        // Gateway surfaces a pending device against that offer.
        state
            .borrow_mut()
            .pairing
            .attach_pending(
                &offer_id,
                super::pairing_channel::PendingDeviceInfo {
                    device_name: "Pixel".to_string(),
                    key_fingerprint: format!("sha256:{}", "0".repeat(64)),
                },
            )
            .expect("attach pending device");

        // It shows up in the pending list.
        let pending = handle_pairing_pending(&state);
        let list = pending.data.as_ref().unwrap()["pending"]
            .as_array()
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["device_name"], "Pixel");

        // Operator confirms; a device id is returned and it is active.
        let confirm = handle_pairing_confirm(&state, &offer_id);
        assert!(confirm.ok, "confirm should succeed");
        let device_id = confirm.data.as_ref().unwrap()["device_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(state.borrow().pairing.is_active_device(&device_id));

        // Confirmed offer no longer pending.
        let pending = handle_pairing_pending(&state);
        assert!(pending.data.as_ref().unwrap()["pending"]
            .as_array()
            .unwrap()
            .is_empty());

        // Operator revokes the device.
        let revoke = handle_device_revoke(&state, &device_id);
        assert!(revoke.ok, "revoke should succeed");
        assert!(!state.borrow().pairing.is_active_device(&device_id));
    }

    #[test]
    fn pairing_confirm_and_revoke_errors_are_reported() {
        let state = Rc::new(RefCell::new(AppState::new()));

        let confirm = handle_pairing_confirm(&state, "ghost-offer");
        assert!(!confirm.ok);
        assert_eq!(confirm.error.as_deref(), Some("no such pairing offer"));

        let reject = handle_pairing_reject(&state, "ghost-offer");
        assert!(!reject.ok);
        assert_eq!(reject.error.as_deref(), Some("no such pairing offer"));

        let revoke = handle_device_revoke(&state, "ghost-device");
        assert!(!revoke.ok);
        assert_eq!(revoke.error.as_deref(), Some("no such device"));
    }
}
