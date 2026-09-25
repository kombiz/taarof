//! Pane split/close/detach operations and pane tree serialization.

use super::*;

#[allow(deprecated)] // VTE 0.78 deprecated current_directory_uri; termprop migration is future scope
pub fn save_pane_tree_for_tab(
    tab: &Tab,
    headless_metadata: impl Fn(u32) -> (Option<crate::task_binding::PaneTaskBinding>, Option<String>),
    agent_metadata: impl Fn(u32) -> (bool, Option<crate::session::SavedAgentSession>),
) -> crate::session::SavedPaneNode {
    if let Some(zoom_state) = tab.pane_zoom.as_ref() {
        save_pane_tree_with_zoom(
            &tab.panes,
            Some(zoom_state),
            &headless_metadata,
            &agent_metadata,
        )
    } else {
        save_pane_tree_with_zoom(&tab.panes, None, &headless_metadata, &agent_metadata)
    }
}

fn persisted_agent_session(
    tmux_backed: bool,
    detected_agent_running: bool,
    detected: Option<crate::session::SavedAgentSession>,
    restored: Option<&crate::session::SavedAgentSession>,
) -> Option<crate::session::SavedAgentSession> {
    if tmux_backed {
        None
    } else if detected_agent_running {
        // A live agent supersedes the restored identity even when its session
        // cannot yet be resolved. Retaining the old identity here would offer
        // an unrelated stale session after the next application restore.
        detected
    } else {
        detected.or_else(|| restored.cloned())
    }
}

fn persisted_tmux_metadata(
    backing: Option<&crate::pane::TmuxBacking>,
    restored: Option<&crate::pane::RestoredTmuxMetadata>,
) -> (Option<String>, Option<String>) {
    backing
        .map(|backing| {
            (
                Some(backing.session_name.clone()),
                backing.target.ssh_target_string(),
            )
        })
        .or_else(|| {
            restored.map(|saved| {
                (
                    Some(saved.session_name.clone()),
                    saved.target.ssh_target_string(),
                )
            })
        })
        .unwrap_or_default()
}

#[allow(deprecated)] // VTE 0.78 deprecated current_directory_uri; termprop migration is future scope
pub(super) fn save_pane_tree_with_zoom(
    node: &crate::pane::PaneNode,
    zoom_state: Option<&crate::workspace::PaneZoomState>,
    headless_metadata: &dyn Fn(
        u32,
    )
        -> (Option<crate::task_binding::PaneTaskBinding>, Option<String>),
    agent_metadata: &dyn Fn(u32) -> (bool, Option<crate::session::SavedAgentSession>),
) -> crate::session::SavedPaneNode {
    use crate::session::SavedPaneNode;
    match node {
        crate::pane::PaneNode::Stub { pane_id } => {
            let (current_task, work_origin) = headless_metadata(*pane_id);
            let (_, agent_session) = agent_metadata(*pane_id);
            SavedPaneNode::Leaf {
                work_origin: Some(work_origin.unwrap_or_else(crate::pane::new_pane_work_origin)),
                cwd: None,
                ssh_command: None,
                tmux_session: None,
                tmux_host: None,
                tmux_identity: None,
                current_task,
                agent_session,
            }
        }
        crate::pane::PaneNode::Empty => SavedPaneNode::Leaf {
            work_origin: Some(crate::pane::new_pane_work_origin()),
            cwd: None,
            ssh_command: None,
            tmux_session: None,
            tmux_host: None,
            tmux_identity: None,
            current_task: None,
            agent_session: None,
        },
        crate::pane::PaneNode::Leaf(leaf) => {
            let (tmux_session, tmux_host) =
                persisted_tmux_metadata(leaf.tmux_backing.as_ref(), leaf.restored_tmux.as_ref());
            SavedPaneNode::Leaf {
                work_origin: Some(leaf.work_origin.clone()),
                cwd: leaf.saved_cwd(),
                ssh_command: leaf.ssh_command(),
                tmux_session,
                tmux_host,
                tmux_identity: leaf
                    .tmux_backing
                    .as_ref()
                    .and_then(crate::pane::TmuxBacking::authoritative_generation),
                current_task: leaf.current_task.clone(),
                agent_session: {
                    let (detected_agent_running, detected) = agent_metadata(leaf.pane_id);
                    persisted_agent_session(
                        leaf.tmux_backing.is_some() || leaf.restored_tmux.is_some(),
                        detected_agent_running,
                        detected,
                        leaf.restored_agent_session.as_ref(),
                    )
                },
            }
        }
        crate::pane::PaneNode::Split {
            direction,
            first,
            second,
            widget,
        } => {
            let ratio = zoom_state
                .and_then(|zoom| {
                    zoom.ancestors
                        .iter()
                        .find(|ancestor| ancestor.widget == *widget)
                        .map(|ancestor| ancestor.ratio)
                })
                .unwrap_or_else(|| current_paned_ratio(widget));
            SavedPaneNode::Split {
                direction: match direction {
                    crate::pane::SplitDirection::Horizontal => "horizontal".into(),
                    crate::pane::SplitDirection::Vertical => "vertical".into(),
                },
                ratio,
                first: Box::new(save_pane_tree_with_zoom(
                    first,
                    zoom_state,
                    headless_metadata,
                    agent_metadata,
                )),
                second: Box::new(save_pane_tree_with_zoom(
                    second,
                    zoom_state,
                    headless_metadata,
                    agent_metadata,
                )),
            }
        }
    }
}

pub(super) fn first_pane_id(node: &PaneNode) -> Option<u32> {
    match node {
        PaneNode::Leaf(leaf) => Some(leaf.pane_id),
        PaneNode::Split { first, second, .. } => {
            first_pane_id(first).or_else(|| first_pane_id(second))
        }
        PaneNode::Stub { pane_id } => Some(*pane_id),
        PaneNode::Empty => None,
    }
}

pub(super) fn resolved_target_pane_id(panes: &PaneNode, focused_pane_id: u32) -> Option<u32> {
    if panes.contains_pane(focused_pane_id) {
        Some(focused_pane_id)
    } else {
        first_pane_id(panes)
    }
}

/// Split the active tab's focused pane in the given direction.
/// Returns `Some(tab_id)` if the split succeeded, `None` if the tab doesn't exist.
/// Supports recursive splitting — any leaf can be split further.
#[allow(deprecated)] // VTE 0.78 deprecated current_directory_uri; termprop migration is future scope
pub fn split_pane(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    direction: crate::pane::SplitDirection,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
) -> Option<u32> {
    let state = runtime.shared_state();
    let tab_id = {
        let st = state.borrow();
        st.active_tab()?.id
    };
    split_pane_for_tab(
        runtime, term_stack, direction, tab_list, window, tab_id, None, None, None,
    )
    .map(|_| tab_id)
}

/// Split a specific tab and run a custom command in the new pane.
/// Returns the newly created pane_id on success.
#[allow(deprecated)] // VTE 0.78 deprecated current_directory_uri; termprop migration is future scope
#[allow(clippy::too_many_arguments)] // Split helpers are thin orchestration layers over explicit UI/runtime dependencies.
pub fn split_pane_with_command(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    tab_id: u32,
    direction: crate::pane::SplitDirection,
    argv: &[&str],
    working_dir: Option<&str>,
) -> Option<u32> {
    let argv_owned: Vec<String> = argv.iter().map(|arg| (*arg).to_string()).collect();
    split_pane_for_tab(
        runtime,
        term_stack,
        direction,
        tab_list,
        window,
        tab_id,
        Some(argv_owned),
        working_dir,
        None,
    )
}

/// Split a specific tab, optionally running a command. This is the entry point
/// the loopback control surface uses: `command_argv` `None` spawns the pane's
/// default shell (an interactive-style split), while `Some` runs that argv.
/// Returns the newly created pane_id on success.
#[allow(deprecated)] // VTE 0.78 deprecated current_directory_uri; termprop migration is future scope
#[allow(clippy::too_many_arguments)] // Split helpers are thin orchestration layers over explicit UI/runtime dependencies.
pub fn open_split_pane(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    tab_id: u32,
    direction: crate::pane::SplitDirection,
    command_argv: Option<Vec<String>>,
    working_dir: Option<&str>,
) -> Option<u32> {
    split_pane_for_tab(
        runtime,
        term_stack,
        direction,
        tab_list,
        window,
        tab_id,
        command_argv,
        working_dir,
        None,
    )
}

#[allow(deprecated)] // VTE 0.78 deprecated current_directory_uri; termprop migration is future scope
#[allow(clippy::too_many_arguments)] // Split helpers are thin orchestration layers over explicit UI/runtime dependencies.
pub(super) fn split_pane_for_tab(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    direction: crate::pane::SplitDirection,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    tab_id: u32,
    command_argv: Option<Vec<String>>,
    working_dir_override: Option<&str>,
    on_spawn_result: Option<PaneSpawnResultCallback>,
) -> Option<u32> {
    let state = runtime.shared_state();
    clear_pane_zoom(&state, tab_id);

    // 1. Find the target tab and the focused pane's info
    let (
        tab_id,
        existing_cwd,
        existing_pane_id,
        existing_widget,
        tmux_backed,
        ws_name,
        host_config_name,
        pane_tmux_backing,
    ) = {
        let st = state.borrow();
        let (ws, tab) = st.find_tab(tab_id)?;
        let existing_pane_id = resolved_target_pane_id(&tab.panes, tab.focused_pane_id)?;
        let leaf = tab.panes.leaf(existing_pane_id)?;
        let existing_widget = leaf.container.clone().upcast::<gtk::Widget>();
        let pane_tmux_backing = leaf.tmux_backing.clone();
        let cwd = leaf.local_cwd();
        // A split is tmux-backed if the workspace says so OR the focused pane
        // itself has tmux backing, while the automatic tmux path is enabled.
        let tmux_backed = crate::config::tmux_inherits_backing(
            ws.tmux_backed,
            pane_tmux_backing.is_some(),
            crate::config::tmux_config().enabled,
        );
        let ws_name = ws.name.clone();
        // Inherit host from workspace, or from the focused pane's tmux target
        let host_config_name = ws.host_config_name.clone();
        (
            tab.id,
            cwd,
            existing_pane_id,
            existing_widget,
            tmux_backed,
            ws_name,
            host_config_name,
            pane_tmux_backing,
        )
    };

    let stack_name = tab_root_widget_name(tab_id);

    // 2. Create and configure the new terminal
    let cfg = crate::config::ghostty_config();
    let (new_terminal, new_container) = build_terminal(&cfg);
    let new_widget = new_container.clone().upcast::<gtk::Widget>();

    // 3. Build the gtk::Paned with the correct orientation
    //    SplitDirection::Vertical (side by side) -> gtk::Orientation::Horizontal
    //    SplitDirection::Horizontal (top/bottom) -> gtk::Orientation::Vertical
    let orientation = match direction {
        crate::pane::SplitDirection::Vertical => gtk::Orientation::Horizontal,
        crate::pane::SplitDirection::Horizontal => gtk::Orientation::Vertical,
    };
    let paned = gtk::Paned::new(orientation);
    paned.set_hexpand(true);
    paned.set_vexpand(true);

    // 4. GTK reparenting: find the existing pane wrapper and reparent it into the new paned
    // Determine the existing wrapper's parent to handle nested vs top-level reparenting
    let parent = existing_widget.parent();
    if let Some(parent_paned) = parent.as_ref().and_then(|p| p.downcast_ref::<gtk::Paned>()) {
        // Nested: wrapper is inside an existing Paned — replace it with the new paned
        if parent_paned.start_child().as_ref() == Some(&existing_widget) {
            parent_paned.set_start_child(gtk::Widget::NONE);
            paned.set_start_child(Some(&existing_widget));
            paned.set_end_child(Some(&new_widget));
            parent_paned.set_start_child(Some(&paned));
        } else {
            parent_paned.set_end_child(gtk::Widget::NONE);
            paned.set_start_child(Some(&existing_widget));
            paned.set_end_child(Some(&new_widget));
            parent_paned.set_end_child(Some(&paned));
        }
    } else {
        // Top-level: wrapper is directly in the stack
        if let Some(child) = term_stack.child_by_name(&stack_name) {
            term_stack.remove(&child);
        }
        paned.set_start_child(Some(&existing_widget));
        paned.set_end_child(Some(&new_widget));
        term_stack.add_named(&paned, Some(&stack_name));
        term_stack.set_visible_child_name(&stack_name);
    }

    // Set initial divider position to 50% (after allocation)
    schedule_paned_ratio(&paned, orientation, 0.5);

    // 5. Assign a new pane_id and update the pane tree
    let new_pane_id = runtime.reserve_pane_id_for_tab(tab_id)?;
    runtime.split_tab_with_node(
        tab_id,
        existing_pane_id,
        PaneNode::Leaf(build_pane_leaf(
            new_pane_id,
            &new_terminal,
            &new_container,
            None,
        )),
        direction,
        &paned,
    )?;

    // 6. Spawn the shell (or replicate SSH session) in the new terminal.
    //    If the workspace is tmux_backed, use tmux argv (takes priority over SSH replication).
    //    Otherwise, if the focused pane has an SSH/mosh process, replicate that command.
    //    A remote pane's OSC 7 cwd is replayed as a remote `cd` so the split
    //    opens beside the focused pane rather than in the remote home dir.
    let ssh_cmd = if !tmux_backed {
        let st = state.borrow();
        st.find_tab(tab_id).and_then(|(_, t)| {
            let leaf = t
                .panes
                .leaves()
                .into_iter()
                .find(|l| l.pane_id == existing_pane_id)?;
            let ssh_command = leaf.ssh_command()?;
            Some(remote_split_respawn(&ssh_command, &leaf.location_state))
        })
    } else {
        None
    };

    let argv_owned: Vec<String>;
    let spawn_cwd: Option<String>;
    let tmux_backing: Option<crate::pane::TmuxBacking>;

    let host_config = host_config_name
        .as_deref()
        .and_then(crate::config::host_config);

    if let Some(command_argv) = command_argv {
        argv_owned = command_argv;
        spawn_cwd = Some(
            working_dir_override
                .map(|s| s.to_string())
                .or(existing_cwd)
                .unwrap_or_else(|| ".".to_string()),
        );
        tmux_backing = None;
    } else if tmux_backed {
        // tmux path: use new_pane_id for the session name
        let tmux_cwd = working_dir_override
            .map(|s| s.to_string())
            .or_else(|| existing_cwd.clone());
        let session_name = crate::tmux::session_name(
            &crate::config::tmux_config().session_prefix,
            &ws_name,
            tab_id,
            new_pane_id,
        );

        // If no workspace-level host but the focused pane has a remote tmux target,
        // inherit it so the split pane connects to the same host.
        let inherited_target = if host_config.is_none() {
            pane_tmux_backing.as_ref().map(|b| &b.target)
        } else {
            None
        };

        let (argv, backing) = if let Some(target) = inherited_target {
            let argv = crate::tmux::create_attach_command(
                target,
                &session_name,
                tmux_cwd.as_deref(),
                crate::config::tmux_session_style(),
            );
            let backing = crate::pane::TmuxBacking {
                session_name: session_name.clone(),
                target: target.clone(),
                expected_generation: None,
                pane_info: ProbeSnapshot::default(),
            };
            (argv, Some(backing))
        } else {
            pane_spawn_argv(
                &cfg,
                true,
                Some(&session_name),
                tmux_cwd.as_deref(),
                host_config.as_ref(),
            )
        };

        argv_owned = argv;
        spawn_cwd = None; // tmux handles cwd via -c flag
        tmux_backing = backing;
    } else if let Some(ref ssh_args) = ssh_cmd {
        // Replicate the SSH/mosh session
        argv_owned = ssh_args.clone();
        spawn_cwd = Some(existing_cwd.unwrap_or_else(|| ".".to_string()));
        tmux_backing = None;
    } else {
        // Normal shell
        let (shell_argv, _) = pane_spawn_argv(&cfg, false, None, None, None);
        argv_owned = shell_argv;
        spawn_cwd = Some(existing_cwd.unwrap_or_else(|| ".".to_string()));
        tmux_backing = None;
    }

    let on_spawn_result = on_spawn_result.map(|callback| {
        let callback = callback.clone();
        Rc::new(move |result| callback(result, tab_id, new_pane_id)) as SpawnResultCallback
    });

    let _ = spawn_terminal_process_with_callback(
        &new_terminal,
        &state,
        tab_id,
        new_pane_id,
        spawn_cwd.as_deref(),
        argv_owned,
        on_spawn_result,
    );

    // Set tmux_backing on the new leaf after spawn
    if let Some(backing) = tmux_backing {
        let mut st = state.borrow_mut();
        if let Some(tab) = st.find_tab_mut(tab_id) {
            if let Some(leaf) = tab.panes.leaf_mut(new_pane_id) {
                leaf.tmux_backing = Some(backing);
            }
        }
    }

    // 7. Focus the new terminal
    new_terminal.grab_focus();

    wire_pane_terminal(&state, term_stack, tab_list, window, tab_id, new_pane_id);

    Some(new_pane_id)
}

/// Recursively find a leaf in the pane tree by pane_id and replace it with a Split node.
/// Close a single pane in a multi-pane tab. The surviving sibling takes its place.
/// If the tab has only one pane, this is a no-op (create_terminal's handler closes the tab).
pub fn close_pane(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    tab_list: Option<&gtk::Box>,
    tab_id: u32,
    pane_id: u32,
) -> Result<(), String> {
    close_pane_async(runtime, term_stack, tab_list, tab_id, pane_id, |result| {
        if let Err(error) = result {
            crate::show_error_toast(&error);
        }
    })
}

pub(crate) fn close_pane_async(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    tab_list: Option<&gtk::Box>,
    tab_id: u32,
    pane_id: u32,
    callback: impl FnOnce(Result<(), String>) + 'static,
) -> Result<(), String> {
    let state = runtime.shared_state();
    let backing_snapshot = {
        let st = state.borrow();
        st.find_tab(tab_id).and_then(|(workspace, tab)| {
            tab.panes.leaf(pane_id).and_then(|leaf| {
                leaf.tmux_backing
                    .clone()
                    .map(|backing| (backing, workspace.name.clone()))
            })
        })
    };

    let Some((backing, workspace_name)) = backing_snapshot else {
        let result = finalize_close_pane(runtime, term_stack, tab_list, tab_id, pane_id);
        callback(result.clone());
        return result;
    };
    let window = tab_list
        .and_then(|tab_list| tab_list.root())
        .and_then(|root| root.downcast::<adw::ApplicationWindow>().ok())
        .ok_or_else(|| "could not close tmux-backed pane without a live window".to_string())?;
    let guard = crate::tmux::TmuxGtkApplyGuard::new(&state, &window);
    let worker = crate::tmux::default_worker();
    let runtime = RuntimeHandle::from_shared_state(state);
    let term_stack = term_stack.clone();
    let tab_list = tab_list.cloned();
    glib::spawn_future_local(async move {
        let completion = worker
            .submit_coalesced(
                crate::tmux::TmuxJobKey::ClosePane { tab_id, pane_id },
                vec![crate::tmux::kill_backing_command(&backing)],
                std::time::Duration::from_secs(10),
            )
            .await;
        let result = match completion {
            Ok(completion) => match guard.upgrade(&completion) {
                Some((state, _window)) => {
                    let still_same = state
                        .borrow()
                        .find_tab(tab_id)
                        .and_then(|(_, tab)| tab.panes.leaf(pane_id))
                        .and_then(|leaf| leaf.tmux_backing.as_ref())
                        .is_some_and(|live| live.same_execution_target(&backing));
                    if !still_same {
                        Err("pane close was superseded".to_string())
                    } else {
                        let errors = super::restore::apply_tmux_kill_outcomes_before_removal(
                            &state,
                            std::slice::from_ref(&backing),
                            completion.outcomes(),
                            &workspace_name,
                        );
                        let finalized = finalize_close_pane(
                            &runtime,
                            &term_stack,
                            tab_list.as_ref(),
                            tab_id,
                            pane_id,
                        );
                        match (finalized, errors.is_empty()) {
                            (Err(error), _) => Err(error),
                            (Ok(()), true) => Ok(()),
                            (Ok(()), false) => Err(errors.join("; ")),
                        }
                    }
                }
                None => Err("pane close was superseded".to_string()),
            },
            Err(error) => Err(error),
        };
        if result.is_err() {
            if let (Some(tab_list), Some(window)) = (
                tab_list.as_ref(),
                tab_list.as_ref().and_then(|list| {
                    list.root()
                        .and_then(|root| root.downcast::<adw::ApplicationWindow>().ok())
                }),
            ) {
                crate::sidebar::refresh_background_section(
                    tab_list,
                    &runtime.shared_state(),
                    &term_stack,
                    &window,
                );
            }
        }
        callback(result);
    });
    Ok(())
}

fn finalize_close_pane(
    runtime: &RuntimeHandle,
    term_stack: &gtk::Stack,
    _tab_list: Option<&gtk::Box>,
    tab_id: u32,
    pane_id: u32,
) -> Result<(), String> {
    let state = runtime.shared_state();
    clear_pane_zoom(&state, tab_id);

    let stack_name = tab_root_widget_name(tab_id);
    let Some(closed) = runtime.close_pane_in_tab(tab_id, pane_id, term_stack, &stack_name) else {
        return Ok(());
    };
    let focus_terminal = closed.focus_terminal;
    let deferred_stack_widget = closed.deferred_stack_widget;

    // Perform deferred Stack reparenting now that the borrow is released.
    // These GTK ops fire notify::visible-child-name which re-enters state.
    if let Some(widget) = deferred_stack_widget {
        if let Some(child) = term_stack.child_by_name(&stack_name) {
            term_stack.remove(&child);
        }
        term_stack.add_named(&widget, Some(&stack_name));
        term_stack.set_visible_child_name(&stack_name);
    }

    if let Some(term) = focus_terminal {
        term.grab_focus();
    }
    Ok(())
}

/// Register a tmux-backed pane as detached and strip its backing before any UI
/// teardown can run. Explicit pane detach and tab-close detach both use this
/// primitive so every surviving session reaches the detached-session tracker.
pub(crate) fn register_detached_pane(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
) -> Result<String, String> {
    let mut registered = register_detached_panes(state, tab_id, &[pane_id])?;
    registered
        .pop()
        .ok_or_else(|| "Pane is not tmux-backed".to_string())
}

fn register_detached_panes(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_ids: &[u32],
) -> Result<Vec<String>, String> {
    let detached = {
        let st = state.borrow();
        let (workspace, tab) = st
            .find_tab(tab_id)
            .ok_or_else(|| "Tab not found".to_string())?;
        let mut detached = Vec::new();
        for pane_id in pane_ids {
            let Some(backing) = tab
                .panes
                .leaf(*pane_id)
                .and_then(|leaf| leaf.tmux_backing.as_ref())
                .or_else(|| {
                    st.headless_pane(tab_id, *pane_id)
                        .and_then(|pane| pane.tmux_backing.as_ref())
                })
            else {
                continue;
            };
            let host = match &backing.target {
                crate::tmux::TmuxTarget::Local => "localhost".to_string(),
                crate::tmux::TmuxTarget::Remote { ssh_target } => ssh_target.clone(),
            };
            let last_command = backing.pane_info.value().and_then(|info| {
                let command = info.current_command.as_str();
                if command.is_empty() || crate::tmux::is_shell_command(command) {
                    None
                } else {
                    Some(command.to_string())
                }
            });
            let candidate = crate::dashboard::DetachedSession {
                session_name: backing.session_name.clone(),
                host,
                workspace: workspace.name.clone(),
                target: backing.target.clone(),
                detached_at: std::time::Instant::now(),
                last_command,
                finished: false,
            };
            if !detached
                .iter()
                .any(|existing: &crate::dashboard::DetachedSession| {
                    existing.matches_target(&candidate.session_name, &candidate.target)
                })
            {
                detached.push(candidate);
            }
        }
        detached
    };
    let session_names = detached
        .iter()
        .map(|session| session.session_name.clone())
        .collect();

    let mut st = state.borrow_mut();
    for candidate in detached {
        if let Some(existing) = st
            .detached_sessions
            .iter_mut()
            .find(|existing| existing.matches_target(&candidate.session_name, &candidate.target))
        {
            *existing = candidate;
        } else {
            st.detached_sessions.push(candidate);
        }
    }
    for pane_id in pane_ids {
        if let Some(tab) = st.find_tab_mut(tab_id) {
            if let Some(leaf) = tab.panes.leaf_mut(*pane_id) {
                leaf.tmux_backing = None;
            }
        }
        if let Some(pane) = st.headless_pane_mut(tab_id, *pane_id) {
            pane.tmux_backing = None;
        }
    }

    Ok(session_names)
}

/// Register every tmux-backed pane in a tab before the tab is torn down.
pub(crate) fn register_detached_tab(
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
) -> Result<Vec<String>, String> {
    let pane_ids = {
        let st = state.borrow();
        let (_, tab) = st
            .find_tab(tab_id)
            .ok_or_else(|| "Tab not found".to_string())?;
        let mut pane_ids: Vec<u32> = tab
            .panes
            .leaves()
            .into_iter()
            .filter(|leaf| leaf.tmux_backing.is_some())
            .map(|leaf| leaf.pane_id)
            .collect();
        pane_ids.extend(st.headless_panes.iter().filter_map(
            |(&(headless_tab_id, pane_id), pane)| {
                (headless_tab_id == tab_id && pane.tmux_backing.is_some()).then_some(pane_id)
            },
        ));
        pane_ids.sort_unstable();
        pane_ids.dedup();
        pane_ids
    };

    register_detached_panes(state, tab_id, &pane_ids)
}

/// Detach a tmux-backed pane: register the session, strip the tmux_backing so
/// the exit handler won't kill the tmux session, then close the VTE pane
/// normally.
///
/// Returns the tmux session name on success, or an error message.
pub fn detach_pane(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    tab_id: u32,
    pane_id: u32,
) -> Result<String, String> {
    let session_name = register_detached_pane(state, tab_id, pane_id)?;

    // Close the VTE pane. For a multi-pane tab this uses close_pane; for a
    // single-pane tab we close the entire tab via close_tab_by_id (since
    // close_pane bails out for leaf_count <= 1 and nothing else terminates the
    // VTE child process).
    let leaf_count = {
        let st = state.borrow();
        st.find_tab(tab_id)
            .map(|(_, tab)| tab.panes.leaf_count())
            .unwrap_or(0)
    };
    if leaf_count > 1 {
        let runtime = RuntimeHandle::from_shared_state(state.clone());
        let _ = close_pane(&runtime, term_stack, Some(tab_list), tab_id, pane_id);
    } else {
        let _ = crate::sidebar::close_tab_by_id(tab_list, state, term_stack, tab_id);
    }

    Ok(session_name)
}

#[cfg(test)]
mod persistence_tests {
    use super::{persisted_agent_session, persisted_tmux_metadata};
    use crate::pane::RestoredTmuxMetadata;
    use crate::session::{SavedAgentSession, SavedAgentSessionSource};
    use crate::tmux::TmuxTarget;

    fn saved(session_id: &str) -> SavedAgentSession {
        SavedAgentSession {
            agent_name: "codex".into(),
            session_id: session_id.into(),
            host_identity: Some("build.ts".into()),
            source: SavedAgentSessionSource::Argv,
        }
    }

    #[test]
    fn restored_agent_session_survives_bare_shell_autosave() {
        let restored = saved("restored-session");
        assert_eq!(
            persisted_agent_session(false, false, None, Some(&restored)),
            Some(restored)
        );
    }

    #[test]
    fn live_agent_session_supersedes_restored_metadata_and_tmux_saves_none() {
        let restored = saved("restored-session");
        let detected = saved("live-session");
        assert_eq!(
            persisted_agent_session(false, true, Some(detected.clone()), Some(&restored)),
            Some(detected.clone())
        );
        assert_eq!(
            persisted_agent_session(true, true, Some(detected), Some(&restored)),
            None
        );
    }

    #[test]
    fn unresolved_live_agent_clears_unrelated_restored_metadata() {
        let restored = saved("restored-session");
        assert_eq!(
            persisted_agent_session(false, true, None, Some(&restored)),
            None
        );
    }

    #[test]
    fn legacy_tmux_display_metadata_survives_materialized_autosave() {
        let restored = RestoredTmuxMetadata {
            session_name: "saved-name".into(),
            target: TmuxTarget::Remote {
                ssh_target: "builder@host".into(),
            },
        };
        assert_eq!(
            persisted_tmux_metadata(None, Some(&restored)),
            (Some("saved-name".into()), Some("builder@host".into()))
        );
    }
}
