//! Terminal process lifecycle helpers.
//!
//! This module owns shell/tmux argv resolution, VTE child spawning, and
//! child-exit cleanup. It must not own VTE signal parsing, termprop handling,
//! or other long-lived terminal event adapters.

use super::*;

const FAILING_TEST_SNIPPET_LINES: u32 = 4;

/// How often the main loop polls the broker for a pane's child exit.
const BROKER_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(150);

pub(super) fn build_shell_argv(cfg: &GhosttyConfig) -> Vec<String> {
    if let Some(ref cmd) = cfg.shell_command {
        vec!["/bin/sh".into(), "-c".into(), cmd.clone()]
    } else {
        vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())]
    }
}

/// Determine spawn argv for a pane, considering tmux backing.
/// Returns `(argv, optional TmuxBacking to set on the leaf)`.
pub(super) fn pane_spawn_argv(
    cfg: &GhosttyConfig,
    tmux_backed: bool,
    session_name: Option<&str>,
    cwd: Option<&str>,
    host: Option<&crate::host::HostConfig>,
) -> (Vec<String>, Option<crate::pane::TmuxBacking>) {
    if tmux_backed {
        if let Some(name) = session_name {
            let target = super::tmux_target_for_host(host);
            let argv = crate::tmux::create_attach_command(
                &target,
                name,
                cwd,
                crate::config::tmux_session_style(),
            );
            let backing = crate::pane::TmuxBacking {
                session_name: name.to_string(),
                target,
                pane_info: ProbeSnapshot::default(),
            };
            return (argv, Some(backing));
        }
    }
    (build_shell_argv(cfg), None)
}

fn default_spawn_dir() -> String {
    std::env::current_dir()
        .ok()
        .map(|dir| dir.to_string_lossy().into_owned())
        .or_else(|| std::env::var("HOME").ok())
        .unwrap_or_else(|| ".".into())
}

pub(super) type SpawnResultCallback = Rc<dyn Fn(Result<glib::Pid, glib::Error>)>;
pub(super) type PaneSpawnResultCallback = Rc<dyn Fn(Result<glib::Pid, glib::Error>, u32, u32)>;

pub(super) fn spawn_terminal_process_with_callback(
    terminal: &vte::Terminal,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
    cwd: Option<&str>,
    argv_owned: Vec<String>,
    on_result: Option<SpawnResultCallback>,
) -> Result<(), glib::Error> {
    let fallback_dir = default_spawn_dir();
    let spawn_cwd = cwd.unwrap_or(&fallback_dir).to_string();

    let (cols, rows) = broker_grid_size(terminal);
    let spec = crate::pty_broker::SpawnSpec {
        argv: argv_owned,
        cwd: Some(std::path::PathBuf::from(spawn_cwd)),
        // Preserve shell compatibility without mutating the threaded parent's
        // environment. The broker applies these through the child sanitizer.
        env: vec![("VTE_VERSION".into(), "8203".into())],
        cols,
        rows,
    };

    match super::broker_pty::attach_broker(terminal, spec) {
        Ok(handle) => {
            let pid = handle.child_pid() as i32;
            let handle = Rc::new(handle);
            {
                let mut st = state.borrow_mut();
                if let Some(tab) = st.find_tab_mut(tab_id) {
                    if let Some(leaf) = tab.panes.leaf_mut(pane_id) {
                        leaf.shell_pid = Some(pid);
                        leaf.broker = Some(handle.clone());
                    }
                }
            }
            install_broker_resize_forwarding(terminal, &handle);
            if let Some(callback) = on_result.as_ref() {
                callback(Ok(glib::Pid(pid)));
            }
            Ok(())
        }
        Err(err) => {
            let message = format!("terminal broker spawn failed: {err}");
            if let Some(callback) = on_result.as_ref() {
                callback(Err(glib::Error::new(gio::IOErrorEnum::Failed, &message)));
            } else {
                // Without a caller-provided callback (e.g. session restore
                // paths), spawn errors would otherwise be silently dropped;
                // log to stderr so users see why a terminal failed to start.
                eprintln!("taarof: {message}");
            }
            Err(glib::Error::new(gio::IOErrorEnum::Failed, &message))
        }
    }
}

/// VTE's current grid size, falling back to a conventional 80x24 before the
/// widget's first allocation reports a real size.
fn broker_grid_size(terminal: &vte::Terminal) -> (u16, u16) {
    let clamp = |value: i64, fallback: u16| -> u16 {
        if value > 0 {
            value.min(u16::MAX as i64) as u16
        } else {
            fallback
        }
    };
    (
        clamp(terminal.column_count(), 80),
        clamp(terminal.row_count(), 24),
    )
}

/// Forward VTE's grid size to the broker whenever it changes. A per-frame tick
/// callback is cheap because [`BrokerHandle::resize_to`] de-duplicates; the
/// callback stops itself once the pane (and thus the handle) is gone.
fn install_broker_resize_forwarding(terminal: &vte::Terminal, handle: &Rc<super::BrokerHandle>) {
    let weak = Rc::downgrade(handle);
    terminal.add_tick_callback(move |terminal, _clock| {
        let Some(handle) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let (cols, rows) = broker_grid_size(terminal);
        handle.resize_to(cols, rows);
        glib::ControlFlow::Continue
    });
}

pub(super) fn spawn_terminal_process(
    terminal: &vte::Terminal,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
    cwd: Option<&str>,
    argv_owned: Vec<String>,
) {
    let _ = spawn_terminal_process_with_callback(
        terminal, state, tab_id, pane_id, cwd, argv_owned, None,
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChildExitUiAction {
    None,
    ClosePane,
    CloseTab,
    CloseWindow,
}

pub(super) fn child_exit_ui_action(
    leaf_count: usize,
    close_on_exit: bool,
    total_tabs: usize,
) -> ChildExitUiAction {
    if leaf_count > 1 {
        ChildExitUiAction::ClosePane
    } else if !close_on_exit {
        ChildExitUiAction::None
    } else if total_tabs <= 1 {
        ChildExitUiAction::CloseWindow
    } else {
        ChildExitUiAction::CloseTab
    }
}

/// Watch a brokered pane's child for exit. VTE no longer owns the child (the
/// broker does), so instead of `connect_child_exited` we poll the broker's
/// reaped exit status from the main loop and run the same cleanup. The poller
/// stops itself when the pane, or its broker attachment, is gone.
pub(super) fn connect_child_exit_cleanup(
    terminal: &vte::Terminal,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    tab_id: u32,
    pane_id: u32,
) {
    let state = state.clone();
    let term_stack = term_stack.clone();
    let tab_list = tab_list.clone();
    let terminal = terminal.clone();
    glib::timeout_add_local(BROKER_EXIT_POLL_INTERVAL, move || {
        let exit_code = {
            let st = state.borrow();
            let Some((_, tab)) = st.find_tab(tab_id) else {
                return glib::ControlFlow::Break;
            };
            let Some(leaf) = tab.panes.leaf(pane_id) else {
                return glib::ControlFlow::Break;
            };
            let Some(handle) = leaf.broker.as_ref() else {
                return glib::ControlFlow::Break;
            };
            // Reap the direct child even if a descendant still owns its PTY.
            // Only UI cleanup waits for presentation EOF; try_wait caches the
            // status for the next tick after the final tail has been consumed.
            handle
                .pane()
                .try_exit_code()
                .filter(|_| handle.ready_for_exit_cleanup())
        };
        let Some(exit_code) = exit_code else {
            return glib::ControlFlow::Continue;
        };
        run_child_exit_cleanup(
            &state,
            &term_stack,
            &tab_list,
            Some(&terminal),
            tab_id,
            pane_id,
            exit_code,
        );
        glib::ControlFlow::Break
    });
}

/// Apply the UI/state consequences of a pane's child exiting: record a failing
/// `command_exited` event, update workspace run-status, respawn if configured,
/// otherwise close the pane/tab/window as appropriate. Extracted so the broker
/// exit poller can drive the exact behavior the old `connect_child_exited`
/// closure did, with a concrete exit code and terminal.
#[allow(clippy::too_many_arguments)] // Mirrors the historical child-exit closure's captured state.
fn run_child_exit_cleanup(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    terminal: Option<&vte::Terminal>,
    tab_id: u32,
    pane_id: u32,
    exit_code: i32,
) {
    let command_exit_event = if exit_code == 0 {
        None
    } else {
        let st = state.borrow();
        let Some((_, tab)) = st.find_tab(tab_id) else {
            return;
        };
        let Some(leaf) = tab.panes.leaf(pane_id) else {
            return;
        };
        leaf.launch_command.clone().map(|command| {
            let snippet = terminal
                .and_then(|terminal| {
                    super::capture_last_terminal_lines(terminal, FAILING_TEST_SNIPPET_LINES).ok()
                })
                .map(|(text, _)| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .unwrap_or_default();
            serde_json::json!({
                "tab_id": tab_id,
                "tab_name": tab.name.clone(),
                "pane_id": pane_id,
                "command": command,
                "exit_code": exit_code,
                "snippet": snippet,
            })
        })
    };

    let (ui_action, respawn_on_exit) = {
        let mut st = state.borrow_mut();
        let total_tabs = st.all_tabs().count();
        let Some(tab) = st.find_tab_mut(tab_id) else {
            return;
        };
        if let Some(leaf) = tab.panes.leaf_mut(pane_id) {
            leaf.shell_pid = None;
            leaf.was_busy = false;
            leaf.launch_command = None;
        }
        tab.clear_pane_agent_activity(pane_id);
        let leaf_count = tab.panes.leaf_count();
        let close_on_exit = tab.close_on_exit;
        let respawn_on_exit = if leaf_count <= 1 && !close_on_exit {
            tab.respawn_on_exit.take()
        } else {
            None
        };
        if respawn_on_exit.is_some() {
            tab.close_on_exit = true;
        }
        (
            child_exit_ui_action(leaf_count, close_on_exit, total_tabs),
            respawn_on_exit,
        )
    };

    {
        let st = state.borrow();
        let action_ws_id = st
            .find_tab(tab_id)
            .and_then(|(ws, tab)| tab.workspace_action.map(|_| ws.id));
        drop(st);

        if let Some(ws_id) = action_ws_id {
            let mut st = state.borrow_mut();
            let success = exit_code == 0;
            if let Some(ws) = st.workspaces.iter_mut().find(|w| w.id == ws_id) {
                ws.run_status = if success {
                    let others_running = ws
                        .tabs
                        .iter()
                        .any(|t| t.id != tab_id && t.workspace_action.is_some());
                    if others_running {
                        crate::workspace::WorkspaceStatus::Running
                    } else {
                        crate::workspace::WorkspaceStatus::Idle
                    }
                } else {
                    crate::workspace::WorkspaceStatus::Errored
                };
            }
        }
    }

    if let Some(payload) = command_exit_event {
        state
            .borrow_mut()
            .event_store
            .emit("command_exited", payload);
    }

    if let Some(respawn) = respawn_on_exit {
        if let Some(terminal) = terminal {
            spawn_terminal_process(
                terminal,
                state,
                tab_id,
                pane_id,
                respawn.working_dir.as_deref(),
                respawn.argv,
            );
            // The respawn is a fresh child under a new broker; re-arm the poller.
            connect_child_exit_cleanup(terminal, state, term_stack, tab_list, tab_id, pane_id);
        }
        return;
    }

    match ui_action {
        ChildExitUiAction::None => {}
        ChildExitUiAction::ClosePane => {
            let state = state.clone();
            let term_stack_weak = term_stack.downgrade();
            let tab_list_weak = tab_list.downgrade();
            glib::idle_add_local_once(move || {
                let Some(term_stack) = term_stack_weak.upgrade() else {
                    return;
                };
                let Some(tab_list) = tab_list_weak.upgrade() else {
                    return;
                };
                if term_stack.root().is_none() || tab_list.root().is_none() {
                    return;
                }
                let runtime = RuntimeHandle::from_shared_state(state.clone());
                let _ = close_pane(&runtime, &term_stack, Some(&tab_list), tab_id, pane_id);
            });
        }
        ChildExitUiAction::CloseTab => {
            let state = state.clone();
            let term_stack_weak = term_stack.downgrade();
            let tab_list_weak = tab_list.downgrade();
            glib::idle_add_local_once(move || {
                let Some(term_stack) = term_stack_weak.upgrade() else {
                    return;
                };
                let Some(tab_list) = tab_list_weak.upgrade() else {
                    return;
                };
                if term_stack.root().is_none() || tab_list.root().is_none() {
                    return;
                }
                let _ = crate::sidebar::close_tab_by_id(&tab_list, &state, &term_stack, tab_id);
            });
        }
        ChildExitUiAction::CloseWindow => {
            let term_stack_weak = term_stack.downgrade();
            glib::idle_add_local_once(move || {
                let Some(term_stack) = term_stack_weak.upgrade() else {
                    return;
                };
                let Some(root) = term_stack.root() else {
                    return;
                };
                if let Ok(window) = root.downcast::<gtk::Window>() {
                    window.close();
                }
            });
        }
    }
}
