//! VTE signal adapters and activity tracking.
//!
//! This module owns translating terminal-local signals into taarof pane/tab
//! state. It must not spawn child processes or own respawn/exit policy.

use super::*;

/// Known termprops that change frequently and should NOT trigger notifications.
const NOISY_TERMPROPS: &[&str] = &[
    "vte.cwd",
    "vte.cwf",
    "xterm.title",
    "vte.shell.precmd",
    "vte.shell.preexec",
    "vte.shell.postexec",
    "vte.container.name",
    "vte.container.runtime",
    "vte.container.uid",
];

const OUTPUT_SCAN_DEBOUNCE: Duration = Duration::from_millis(350);
const OUTPUT_SCAN_MAX_LINES: usize = 48;

fn emit_agent_activity_transition(state: &Rc<RefCell<AppState>>, tab_id: u32, pane_id: u32) {
    let evidence = state.borrow().find_tab(tab_id).map(|(_, tab)| {
        (
            tab.pane_lifecycle(pane_id),
            tab.pane_agent_activity(pane_id)
                .and_then(|activity| activity.source.clone()),
        )
    });
    if let Some((lifecycle, source)) = evidence {
        crate::runtime::RuntimeHandle::from_shared_state(state.clone()).emit_event(
            "agent_activity_changed",
            serde_json::json!({
                "tab_id": tab_id,
                "pane_id": pane_id,
                "state": crate::agents::turn_lifecycle_label(lifecycle),
                "source": source,
            }),
        );
    }
}

pub(super) fn connect_pane_focus_tracking(
    terminal: &vte::Terminal,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
    labels: &crate::sidebar::TabLabels,
) {
    let state_for_focus = state.clone();
    let terminal_for_focus = terminal.clone();
    let labels_for_focus = labels.clone();
    let focus_ctrl = gtk::EventControllerFocus::new();
    focus_ctrl.connect_enter(move |_| {
        let mut st = state_for_focus.borrow_mut();
        if let Some(tab) = st.find_tab_mut(tab_id) {
            tab.focused_pane_id = pane_id;
        }
        drop(st);
        super::update_tab_labels_from_terminal(
            &terminal_for_focus,
            &labels_for_focus.cwd_label,
            &labels_for_focus.branch_label,
        );
    });
    terminal.add_controller(focus_ctrl);
}

/// Connect CWD tracking: OSC 7 (primary) plus terminal title fallback.
#[allow(deprecated)] // VTE 0.78 deprecated these; termprop migration is future scope
pub(super) fn connect_cwd_tracking(
    terminal: &vte::Terminal,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
    tab_list: &gtk::Box,
    cwd_label: &gtk::Label,
    branch_label: &gtk::Label,
) {
    let label = cwd_label.clone();
    let blabel = branch_label.clone();
    let state_for_osc7 = state.clone();
    let tab_list_for_osc7 = tab_list.clone();
    terminal.connect_current_directory_uri_changed(move |term| {
        if let Some(uri) = term.current_directory_uri() {
            let title = term.window_title().map(|s| s.to_string());
            let (cwd, cwd_host) =
                super::location_metadata_from_terminal_state(Some(&uri), title.as_deref());
            let mut st = state_for_osc7.borrow_mut();
            let active_tab = st.active_ws().map_or(0, |w| w.active_tab);
            if let Some(tab) = st.find_tab_mut(tab_id) {
                let is_focused_pane = tab.focused_pane_id == pane_id;
                if is_focused_pane {
                    label.set_text(&super::format_cwd_display(
                        cwd_host.as_deref(),
                        cwd.as_deref().unwrap_or(""),
                    ));
                    super::update_branch_label(&blabel, Some(&uri), title.as_deref());
                }
                if active_tab != tab_id && tab.panes.any_busy() {
                    tab.needs_attention = true;
                }
                if let Some(leaf) = tab.panes.leaf_mut(pane_id) {
                    leaf.was_busy = true;
                    leaf.update_location_cache(cwd, cwd_host);
                }
            }
            drop(st);
            crate::sidebar::invalidate_and_rediscover_if_target_changed(
                &state_for_osc7,
                &tab_list_for_osc7,
                tab_id,
            );
        }
    });

    let label = cwd_label.clone();
    let blabel = branch_label.clone();
    let state_for_title = state.clone();
    let tab_list_for_title = tab_list.clone();
    terminal.connect_window_title_changed(move |term| {
        if let Some(title) = term.window_title() {
            let uri = term.current_directory_uri().map(|s| s.to_string());
            let (cwd, cwd_host) =
                super::location_metadata_from_terminal_state(uri.as_deref(), Some(title.as_str()));

            let is_focused_pane = {
                let mut st = state_for_title.borrow_mut();
                let Some(tab) = st.find_tab_mut(tab_id) else {
                    return;
                };
                if let Some(leaf) = tab.panes.leaf_mut(pane_id) {
                    leaf.update_location_cache(cwd.clone(), cwd_host.clone());
                }
                tab.focused_pane_id == pane_id
            };

            crate::sidebar::invalidate_and_rediscover_if_target_changed(
                &state_for_title,
                &tab_list_for_title,
                tab_id,
            );

            if !is_focused_pane {
                return;
            }

            super::update_branch_label(&blabel, uri.as_deref(), Some(title.as_str()));
            if uri.is_none() {
                if let Some(path) = cwd {
                    label.set_text(&super::shorten_path(&path));
                }
            }
        }
    });
}

/// Connect bell and termprop signals for agent-triggered notifications.
pub(super) fn connect_notifications(
    terminal: &vte::Terminal,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    window: &adw::ApplicationWindow,
) {
    let state_for_bell = state.clone();
    let window_for_bell = window.clone();
    terminal.connect_bell(move |_term| {
        let mut st = state_for_bell.borrow_mut();
        let active_tab_id = st.active_ws().map_or(0, |w| w.active_tab);
        if active_tab_id != tab_id {
            if let Some(tab) = st.find_tab_mut(tab_id) {
                tab.needs_attention = true;
            }
        }
        if !window_for_bell.is_active() {
            crate::sound::play_attention_sound();
        }
    });

    let state_for_tp = state.clone();
    let window_for_tp = window.clone();
    terminal.connect_termprop_changed(None, move |term, prop_name| {
        if NOISY_TERMPROPS.contains(&prop_name) || is_taarof_activity_termprop(prop_name) {
            return;
        }

        let mut st = state_for_tp.borrow_mut();
        let active_tab_id = st.active_ws().map_or(0, |w| w.active_tab);
        let mut should_sound = false;
        if active_tab_id != tab_id {
            if let Some(tab) = st.find_tab_mut(tab_id) {
                let (msg, _) = term.dup_termprop_string(prop_name);
                if let Some(body) = msg {
                    let body = body.to_string();
                    if !body.is_empty() {
                        tab.needs_attention = true;
                        tab.notification_msg = Some(body);
                        should_sound = true;
                    }
                }
            }
        } else {
            let (msg, _) = term.dup_termprop_string(prop_name);
            should_sound = msg.is_some_and(|m| !m.to_string().is_empty());
        }
        if should_sound && !window_for_tp.is_active() {
            crate::sound::play_attention_sound();
        }
    });
}

pub(super) fn is_taarof_activity_termprop(prop_name: &str) -> bool {
    super::TAAROF_ACTIVITY_TERMPROPS.contains(&prop_name)
}

fn read_termprop_string(terminal: &vte::Terminal, prop_name: &str) -> Option<String> {
    let (value, _) = terminal.dup_termprop_string(prop_name);
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

pub(super) fn parse_termprop_activity_state(
    value: Option<&str>,
) -> Option<crate::workspace::AgentActivityState> {
    match value?.trim() {
        value if value.eq_ignore_ascii_case("idle") => {
            Some(crate::workspace::AgentActivityState::Idle)
        }
        value if value.eq_ignore_ascii_case("running") => {
            Some(crate::workspace::AgentActivityState::Running)
        }
        value
            if value.eq_ignore_ascii_case("waiting-input")
                || value.eq_ignore_ascii_case("waiting")
                || value.eq_ignore_ascii_case("needs-input") =>
        {
            Some(crate::workspace::AgentActivityState::WaitingInput)
        }
        value if value.eq_ignore_ascii_case("errored") || value.eq_ignore_ascii_case("error") => {
            Some(crate::workspace::AgentActivityState::Errored)
        }
        value if value.eq_ignore_ascii_case("done") => {
            Some(crate::workspace::AgentActivityState::Done)
        }
        _ => None,
    }
}

fn schedule_termprop_done_clear(
    state: Rc<RefCell<AppState>>,
    tab_list: gtk::Box,
    tab_id: u32,
    pane_id: u32,
    done_at: std::time::Instant,
) {
    glib::timeout_add_local_once(crate::workspace::DONE_ACTIVITY_VISIBILITY, move || {
        let (should_refresh, lifecycle_changed) = {
            let mut st = state.borrow_mut();
            let Some(tab) = st.find_tab_mut(tab_id) else {
                return;
            };
            let before = tab.pane_lifecycle(pane_id);
            let changed = tab.clear_pane_agent_activity_if(pane_id, |activity| {
                matches!(activity.state, crate::workspace::AgentActivityState::Done)
                    && matches!(
                        activity.origin,
                        crate::workspace::AgentActivityOrigin::Termprop
                    )
                    && activity.updated_at == done_at
            });
            (changed, before != tab.pane_lifecycle(pane_id))
        };

        if should_refresh {
            crate::sidebar::refresh_tab_row(&tab_list, &state, tab_id);
        }
        if lifecycle_changed {
            emit_agent_activity_transition(&state, tab_id, pane_id);
        }
    });
}

fn sync_agent_activity_from_termprops(
    terminal: &vte::Terminal,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    tab_id: u32,
    pane_id: u32,
    changed_prop: &str,
) {
    let raw_state = read_termprop_string(terminal, super::TAAROF_AGENT_STATE_TERMPROP);
    let raw_text = read_termprop_string(terminal, super::TAAROF_AGENT_TEXT_TERMPROP);
    let raw_source = read_termprop_string(terminal, super::TAAROF_AGENT_SOURCE_TERMPROP);
    let parsed_state = parse_termprop_activity_state(raw_state.as_deref());

    if parsed_state.is_none() && changed_prop != super::TAAROF_AGENT_STATE_TERMPROP {
        return;
    }

    let mut clear_at = None;
    let (should_refresh, lifecycle_changed) = {
        let mut st = state.borrow_mut();
        let active_tab_id = st.active_ws().map_or(0, |ws| ws.active_tab);
        let Some(tab) = st.find_tab_mut(tab_id) else {
            return;
        };

        let before = tab.pane_lifecycle(pane_id);
        let fallback_source = tab.agent_name.clone();
        let changed = match parsed_state {
            Some(crate::workspace::AgentActivityState::Idle) | None => {
                tab.clear_pane_agent_activity(pane_id)
            }
            Some(crate::workspace::AgentActivityState::Running) => {
                let summary = crate::agents::summarize_activity_text(
                    raw_text.as_deref().unwrap_or("working"),
                );
                let source = raw_source.or(fallback_source);
                let next = crate::workspace::AgentActivity::termprop(
                    crate::workspace::AgentActivityState::Running,
                    summary,
                    source,
                );
                let had_attention =
                    tab.needs_attention || tab.notified || tab.notification_msg.is_some();
                let changed = if let Some(next_activity) = next {
                    let changed = tab.pane_agent_activity(pane_id).is_none_or(|activity| {
                        activity.text != next_activity.text
                            || activity.source != next_activity.source
                            || !matches!(
                                activity.origin,
                                crate::workspace::AgentActivityOrigin::Termprop
                            )
                            || !matches!(
                                activity.state,
                                crate::workspace::AgentActivityState::Running
                            )
                    });
                    if changed {
                        tab.set_pane_agent_activity(pane_id, Some(next_activity));
                    }
                    changed || had_attention
                } else {
                    had_attention
                };
                tab.needs_attention = false;
                tab.notified = false;
                tab.notification_msg = None;
                tab.notification_pane_id = None;
                changed
            }
            Some(crate::workspace::AgentActivityState::WaitingInput) => {
                let summary = crate::agents::summarize_activity_text(
                    raw_text.as_deref().unwrap_or("waiting for input"),
                );
                let source = raw_source.or(fallback_source);
                let label_source = source.clone();
                let activity = crate::workspace::AgentActivity::termprop(
                    crate::workspace::AgentActivityState::WaitingInput,
                    &summary,
                    source,
                );
                tab.set_pane_agent_activity(pane_id, activity);
                tab.notified = false;
                tab.notification_pane_id = Some(pane_id);
                tab.notification_msg = Some(labeled_pane_notification(
                    tab,
                    label_source.as_deref(),
                    pane_id,
                    &summary,
                ));
                if tab_id != active_tab_id {
                    tab.needs_attention = true;
                }
                true
            }
            Some(crate::workspace::AgentActivityState::Errored) => {
                let summary = crate::agents::summarize_activity_text(
                    raw_text.as_deref().unwrap_or("errored"),
                );
                let source = raw_source.or(fallback_source);
                let label_source = source.clone();
                let activity = crate::workspace::AgentActivity::termprop(
                    crate::workspace::AgentActivityState::Errored,
                    &summary,
                    source,
                );
                tab.set_pane_agent_activity(pane_id, activity);
                tab.notified = false;
                tab.notification_pane_id = Some(pane_id);
                tab.notification_msg = Some(labeled_pane_notification(
                    tab,
                    label_source.as_deref(),
                    pane_id,
                    &summary,
                ));
                if tab_id != active_tab_id {
                    tab.needs_attention = true;
                }
                true
            }
            Some(crate::workspace::AgentActivityState::Done) => {
                let summary =
                    crate::agents::summarize_activity_text(raw_text.as_deref().unwrap_or("done"));
                let source = raw_source.or(fallback_source);
                let label_source = source.clone();
                let activity = crate::workspace::AgentActivity::termprop(
                    crate::workspace::AgentActivityState::Done,
                    &summary,
                    source,
                );
                clear_at = activity.as_ref().map(|activity| activity.updated_at);
                tab.set_pane_agent_activity(pane_id, activity);
                tab.notified = false;
                tab.notification_pane_id = Some(pane_id);
                tab.notification_msg = Some(labeled_pane_notification(
                    tab,
                    label_source.as_deref(),
                    pane_id,
                    &summary,
                ));
                if tab_id != active_tab_id {
                    tab.needs_attention = true;
                }
                true
            }
        };
        (changed, before != tab.pane_lifecycle(pane_id))
    };

    if should_refresh {
        crate::sidebar::refresh_tab_row(tab_list, state, tab_id);
        crate::dashboard::refresh_dashboard_if_open(state, term_stack);
    }
    if lifecycle_changed {
        emit_agent_activity_transition(state, tab_id, pane_id);
    }

    if let Some(done_at) = clear_at {
        schedule_termprop_done_clear(state.clone(), tab_list.clone(), tab_id, pane_id, done_at);
    }
}

/// Notification text for a pane-scoped agent event. The pane number is only
/// included when the tab has agent activity in more than one pane, so
/// single-agent tabs keep the short message.
fn labeled_pane_notification(
    tab: &crate::workspace::Tab,
    source: Option<&str>,
    pane_id: u32,
    summary: &str,
) -> String {
    let multi_pane = tab.pane_agent_activity.len() > 1;
    crate::agents::format_agent_notification(source, multi_pane.then_some(pane_id), summary)
}

/// VTE's built-in prompt signal, emitted with OSC 666;vte.shell.precmd! ST.
const SHELL_PRECMD_TERMPROP: &str = "vte.shell.precmd";

/// Record a prompt-marker cursor row on the explicit `vte.shell.precmd` signal.
/// The shell helper emits this alongside OSC 133; OSC 133 alone is insufficient.
/// These marks
/// power exact command-block copy and jump-to-prompt scrolling. The prop is a
/// VTE built-in (no `install_termprop` needed) and is already in
/// `NOISY_TERMPROPS`, so `connect_notifications` never raises a notification for
/// it — this handler only appends the row to the pane's mark buffer.
pub(super) fn connect_command_mark_tracking(
    terminal: &vte::Terminal,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
) {
    let state = state.clone();
    terminal.connect_termprop_changed(Some(SHELL_PRECMD_TERMPROP), move |term, _prop| {
        let (_col, row) = term.cursor_position();
        let mut st = state.borrow_mut();
        if let Some(tab) = st.find_tab_mut(tab_id) {
            if let Some(leaf) = tab.panes.leaf_mut(pane_id) {
                leaf.push_command_mark(row);
            }
        }
    });
}

pub(super) fn connect_activity_termprop_protocol(
    terminal: &vte::Terminal,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    tab_id: u32,
    pane_id: u32,
) {
    for prop_name in super::TAAROF_ACTIVITY_TERMPROPS {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        terminal.connect_termprop_changed(Some(prop_name), move |term, changed_prop| {
            sync_agent_activity_from_termprops(
                term,
                &state,
                &term_stack,
                &tab_list,
                tab_id,
                pane_id,
                changed_prop,
            );
        });
    }
}

fn sample_recent_terminal_output(terminal: &vte::Terminal) -> Option<String> {
    let (_cursor_col, cursor_row) = terminal.cursor_position();
    let cols = terminal.column_count();

    let start_row = (cursor_row - OUTPUT_SCAN_MAX_LINES as libc::c_long + 1).max(0);
    let (text, _len) =
        terminal.text_range_format(vte::Format::Text, start_row, 0, cursor_row, cols - 1);
    let text = text?;

    if text.is_empty() {
        return None;
    }
    Some(text.to_string())
}

fn apply_scanned_output_activity(
    tab: &mut crate::workspace::Tab,
    pane_id: u32,
    scanned_activity: Option<crate::agents::ScannedActivity>,
) -> bool {
    let pane_is_remote = tab.panes.pane_is_remote_shell(pane_id);
    apply_scanned_output_activity_for_pane(tab, pane_id, pane_is_remote, scanned_activity)
}

fn apply_scanned_output_activity_for_pane(
    tab: &mut crate::workspace::Tab,
    pane_id: u32,
    pane_is_remote: bool,
    scanned_activity: Option<crate::agents::ScannedActivity>,
) -> bool {
    if tab
        .socket_agent_activity
        .iter()
        .chain(tab.pane_agent_activity.values())
        .chain(tab.agent_activity.iter())
        .any(|activity| {
            matches!(activity.state, crate::workspace::AgentActivityState::Done)
                && matches!(
                    activity.origin,
                    crate::workspace::AgentActivityOrigin::Socket
                        | crate::workspace::AgentActivityOrigin::Termprop
                )
        })
    {
        return false;
    }

    if tab
        .socket_agent_activity
        .iter()
        .chain(tab.pane_agent_activity.values())
        .chain(tab.agent_activity.iter())
        .any(|activity| activity.has_fresh_explicit_update())
    {
        return false;
    }

    let scanned_running = scanned_activity.as_ref().is_some_and(|activity| {
        matches!(
            activity.state,
            crate::workspace::AgentActivityState::Running
        )
    });
    let local_agent_process_in_pane = !pane_is_remote && tab.agent_pane_id == Some(pane_id);
    let process_backed_running_scan = scanned_running && local_agent_process_in_pane;

    if scanned_running && !process_backed_running_scan {
        let changed = tab.clear_pane_agent_activity_if(pane_id, |activity| {
            matches!(
                activity.origin,
                crate::workspace::AgentActivityOrigin::OutputScan
            )
        });
        let was_agent_running = tab.agent_running;
        if !tab.has_fresh_running_activity() {
            tab.agent_running = false;
        }
        return changed || was_agent_running != tab.agent_running;
    }

    let Some(scanned) = scanned_activity else {
        let changed = tab.clear_pane_agent_activity_if(pane_id, |activity| {
            matches!(
                activity.origin,
                crate::workspace::AgentActivityOrigin::OutputScan
            )
        });
        if changed && !tab.has_fresh_running_activity() {
            tab.agent_running = false;
        }
        return changed;
    };

    let next = crate::workspace::AgentActivity::output_scan(
        scanned.state,
        scanned.summary,
        tab.agent_name.clone(),
    );
    let Some(next_activity) = next else {
        return false;
    };

    let changed = tab.pane_agent_activity(pane_id).is_none_or(|activity| {
        let identical_running_scan = matches!(
            (activity.state, next_activity.state),
            (
                crate::workspace::AgentActivityState::Running,
                crate::workspace::AgentActivityState::Running
            )
        ) && matches!(
            activity.origin,
            crate::workspace::AgentActivityOrigin::OutputScan
        );

        activity.text != next_activity.text
            || activity.source != next_activity.source
            || !matches!(
                activity.origin,
                crate::workspace::AgentActivityOrigin::OutputScan
            )
            || activity.state != next_activity.state
            || identical_running_scan
    });

    let was_agent_running = tab.agent_running;

    if changed {
        let state = next_activity.state;
        tab.set_pane_agent_activity(pane_id, Some(next_activity));
        tab.agent_running = matches!(state, crate::workspace::AgentActivityState::Running)
            && tab.has_fresh_running_activity();
        return changed || was_agent_running != tab.agent_running;
    }

    if scanned_running {
        tab.agent_running = tab.has_fresh_running_activity();
    }

    was_agent_running != tab.agent_running
}

fn update_output_scanned_activity(
    terminal: &vte::Terminal,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    tab_id: u32,
    pane_id: u32,
) {
    let scanned_activity = sample_recent_terminal_output(terminal)
        .and_then(|text| crate::agents::scan_output_signal(&text));

    let (should_refresh, lifecycle_changed) = {
        let mut st = state.borrow_mut();
        let Some(tab) = st.find_tab_mut(tab_id) else {
            return;
        };
        let before = tab.pane_lifecycle(pane_id);
        let changed = apply_scanned_output_activity(tab, pane_id, scanned_activity);
        (changed, before != tab.pane_lifecycle(pane_id))
    };

    if should_refresh {
        crate::sidebar::refresh_tab_row(tab_list, state, tab_id);
        crate::dashboard::refresh_dashboard_if_open(state, term_stack);
    }
    if lifecycle_changed {
        emit_agent_activity_transition(state, tab_id, pane_id);
    }
}

pub(super) fn connect_output_tracking(
    terminal: &vte::Terminal,
    output_tracker: &Rc<Cell<Option<std::time::Instant>>>,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    tab_id: u32,
    pane_id: u32,
) {
    let tracker = output_tracker.clone();
    let state_for_scan = state.clone();
    let term_stack_for_scan = term_stack.clone();
    let tab_list_for_scan = tab_list.clone();
    let terminal_for_scan = terminal.clone();
    let dirty_for_output = state.borrow().pane_dirty.clone();
    let scan_generation = Rc::new(Cell::new(0u64));
    let generation_for_signal = scan_generation.clone();
    terminal.connect_contents_changed(move |_term| {
        dirty_for_output.mark_dirty(tab_id, pane_id);
        tracker.set(Some(std::time::Instant::now()));

        let generation = generation_for_signal.get().wrapping_add(1);
        generation_for_signal.set(generation);

        let generation_for_timeout = scan_generation.clone();
        let state = state_for_scan.clone();
        let term_stack = term_stack_for_scan.clone();
        let tab_list = tab_list_for_scan.clone();
        let terminal = terminal_for_scan.clone();
        glib::timeout_add_local_once(OUTPUT_SCAN_DEBOUNCE, move || {
            if generation_for_timeout.get() != generation {
                return;
            }
            update_output_scanned_activity(
                &terminal,
                &state,
                &term_stack,
                &tab_list,
                tab_id,
                pane_id,
            );
        });
    });

    let dirty_for_resize_request = state.borrow().pane_dirty.clone();
    terminal.connect_resize_window(move |_term, _columns, _rows| {
        dirty_for_resize_request.mark_dirty(tab_id, pane_id);
    });

    let last_grid_size = Rc::new(Cell::new((terminal.column_count(), terminal.row_count())));
    let dirty_for_width = state.borrow().pane_dirty.clone();
    let last_grid_size_for_width = last_grid_size.clone();
    terminal.connect_notify_local(Some("width"), move |term, _| {
        let next_grid_size = (term.column_count(), term.row_count());
        if last_grid_size_for_width.get() != next_grid_size {
            last_grid_size_for_width.set(next_grid_size);
            dirty_for_width.mark_dirty(tab_id, pane_id);
        }
    });

    let dirty_for_height = state.borrow().pane_dirty.clone();
    let last_grid_size_for_height = last_grid_size.clone();
    terminal.connect_notify_local(Some("height"), move |term, _| {
        let next_grid_size = (term.column_count(), term.row_count());
        if last_grid_size_for_height.get() != next_grid_size {
            last_grid_size_for_height.set(next_grid_size);
            dirty_for_height.mark_dirty(tab_id, pane_id);
        }
    });

    let dirty_for_char_size = state.borrow().pane_dirty.clone();
    let last_grid_size_for_char_size = last_grid_size;
    terminal.connect_char_size_changed(move |term, _width, _height| {
        let next_grid_size = (term.column_count(), term.row_count());
        if last_grid_size_for_char_size.get() != next_grid_size {
            last_grid_size_for_char_size.set(next_grid_size);
            dirty_for_char_size.mark_dirty(tab_id, pane_id);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{apply_scanned_output_activity, apply_scanned_output_activity_for_pane};
    use crate::agents::ScannedActivity;
    use crate::pane::PaneNode;
    use crate::workspace::{
        AgentActivity, AgentActivityOrigin, AgentActivityState, Tab, TabKind, TabPrimaryState,
    };
    use std::collections::HashMap;
    use std::time::Duration;

    fn stub_tab(id: u32) -> Tab {
        Tab {
            id,
            name: format!("Tab {id}"),
            work_origin: crate::workspace::new_tab_work_origin(),
            kind: TabKind::Terminal,
            panes: Box::new(PaneNode::Stub { pane_id: id + 100 }),
            focused_pane_id: id + 100,
            next_pane_id: id + 101,
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
            workspace_action: None,
            discovery_cwd: None,
            discovered_actions: Vec::new(),
            task_buttons: Vec::new(),
            tracking_data: None,
        }
    }

    fn scanned_running() -> ScannedActivity {
        ScannedActivity {
            state: AgentActivityState::Running,
            summary: "running cargo test ...".to_string(),
        }
    }

    fn scanned_waiting() -> ScannedActivity {
        ScannedActivity {
            state: AgentActivityState::WaitingInput,
            summary: "waiting for input".to_string(),
        }
    }

    #[test]
    fn output_scan_running_is_accepted_when_local_agent_process_is_in_pane() {
        let mut tab = stub_tab(1);
        let pane_id = tab.focused_pane_id;
        tab.agent_name = Some("codex".to_string());
        tab.agent_pane_id = Some(pane_id);

        assert!(apply_scanned_output_activity(
            &mut tab,
            pane_id,
            Some(scanned_running())
        ));

        let activity = tab.pane_agent_activity(pane_id).expect("pane activity");
        assert_eq!(activity.state, AgentActivityState::Running);
        assert_eq!(activity.origin, AgentActivityOrigin::OutputScan);
        assert_eq!(activity.source.as_deref(), Some("codex"));
        assert!(tab.agent_running);
        assert_eq!(tab.primary_state(), TabPrimaryState::Running);
    }

    #[test]
    fn output_scan_running_is_ignored_for_local_pane_without_agent_process() {
        let mut tab = stub_tab(1);
        let pane_id = tab.focused_pane_id;

        assert!(!apply_scanned_output_activity(
            &mut tab,
            pane_id,
            Some(scanned_running())
        ));

        assert!(tab.pane_agent_activity(pane_id).is_none());
        assert!(tab.agent_activity.is_none());
        assert!(!tab.agent_running);
        assert_eq!(tab.primary_state(), TabPrimaryState::Idle);
    }

    #[test]
    fn output_scan_running_is_ignored_when_prior_running_lacks_matching_agent_process() {
        let mut tab = stub_tab(1);
        let pane_id = tab.focused_pane_id;
        tab.agent_running = true;
        tab.agent_pane_id = Some(pane_id + 1);

        assert!(apply_scanned_output_activity(
            &mut tab,
            pane_id,
            Some(scanned_running())
        ));

        assert!(tab.pane_agent_activity(pane_id).is_none());
        assert!(tab.agent_activity.is_none());
        assert!(!tab.agent_running);
        assert_eq!(tab.primary_state(), TabPrimaryState::Idle);
    }

    #[test]
    fn output_scan_running_is_ignored_for_remote_pane_even_with_matching_agent_process() {
        let mut tab = stub_tab(1);
        let pane_id = tab.focused_pane_id;
        tab.agent_running = true;
        tab.agent_pane_id = Some(pane_id);

        assert!(apply_scanned_output_activity_for_pane(
            &mut tab,
            pane_id,
            true,
            Some(scanned_running())
        ));

        assert!(tab.pane_agent_activity(pane_id).is_none());
        assert!(tab.agent_activity.is_none());
        assert!(!tab.agent_running);
        assert_eq!(tab.primary_state(), TabPrimaryState::Idle);
    }

    #[test]
    fn output_scan_repeated_identical_process_backed_running_refreshes_freshness() {
        let mut tab = stub_tab(1);
        let pane_id = tab.focused_pane_id;
        tab.agent_name = Some("codex".to_string());
        tab.agent_pane_id = Some(pane_id);

        assert!(apply_scanned_output_activity(
            &mut tab,
            pane_id,
            Some(scanned_running())
        ));
        let first_seen_at = tab
            .pane_agent_activity(pane_id)
            .expect("pane activity")
            .updated_at;
        let aged_but_fresh_at = first_seen_at - Duration::from_secs(1);
        tab.pane_agent_activity
            .get_mut(&pane_id)
            .expect("pane activity")
            .updated_at = aged_but_fresh_at;
        tab.agent_activity = tab.pane_agent_activity(pane_id).cloned();
        tab.agent_running = false;

        assert!(apply_scanned_output_activity(
            &mut tab,
            pane_id,
            Some(scanned_running())
        ));

        let refreshed = tab.pane_agent_activity(pane_id).expect("pane activity");
        assert_eq!(refreshed.state, AgentActivityState::Running);
        assert!(refreshed.updated_at > aged_but_fresh_at);
        assert!(tab.agent_running);
        assert_eq!(tab.primary_state(), TabPrimaryState::Running);
    }

    #[test]
    fn output_scan_waiting_input_stays_alert_and_not_running() {
        let mut tab = stub_tab(1);
        let pane_id = tab.focused_pane_id;

        assert!(apply_scanned_output_activity(
            &mut tab,
            pane_id,
            Some(scanned_waiting())
        ));

        let activity = tab.pane_agent_activity(pane_id).expect("pane activity");
        assert_eq!(activity.state, AgentActivityState::WaitingInput);
        assert_eq!(activity.origin, AgentActivityOrigin::OutputScan);
        assert!(!tab.agent_running);
        assert_eq!(tab.primary_state(), TabPrimaryState::Alert);
    }

    #[test]
    fn output_scan_running_does_not_override_fresh_socket_activity() {
        let mut tab = stub_tab(1);
        let pane_id = tab.focused_pane_id;
        tab.agent_name = Some("codex".to_string());
        tab.agent_pane_id = Some(pane_id);
        tab.socket_agent_activity = AgentActivity::socket(
            AgentActivityState::Running,
            "socket says running",
            Some("codex".to_string()),
        );

        assert!(!apply_scanned_output_activity(
            &mut tab,
            pane_id,
            Some(scanned_running())
        ));

        assert!(tab.pane_agent_activity(pane_id).is_none());
        assert_eq!(
            tab.socket_agent_activity
                .as_ref()
                .map(|activity| &activity.text),
            Some(&"socket says running".to_string())
        );
    }

    #[test]
    fn output_scan_running_does_not_override_fresh_termprop_activity() {
        let mut tab = stub_tab(1);
        let pane_id = tab.focused_pane_id;
        tab.agent_name = Some("codex".to_string());
        tab.agent_pane_id = Some(pane_id);
        tab.set_pane_agent_activity(
            pane_id,
            AgentActivity::termprop(
                AgentActivityState::Running,
                "termprop says running",
                Some("codex".to_string()),
            ),
        );

        assert!(!apply_scanned_output_activity(
            &mut tab,
            pane_id,
            Some(scanned_running())
        ));

        let activity = tab.pane_agent_activity(pane_id).expect("pane activity");
        assert_eq!(activity.state, AgentActivityState::Running);
        assert_eq!(activity.origin, AgentActivityOrigin::Termprop);
        assert_eq!(activity.text, "termprop says running");
    }

    #[test]
    fn output_scan_running_is_blocked_by_done_socket_activity() {
        let mut tab = stub_tab(1);
        let pane_id = tab.focused_pane_id;
        tab.agent_name = Some("codex".to_string());
        tab.agent_pane_id = Some(pane_id);
        tab.socket_agent_activity = AgentActivity::socket(
            AgentActivityState::Done,
            "socket says done",
            Some("codex".to_string()),
        );
        tab.agent_activity = tab.socket_agent_activity.clone();

        assert!(!apply_scanned_output_activity(
            &mut tab,
            pane_id,
            Some(scanned_running())
        ));

        assert!(tab.pane_agent_activity(pane_id).is_none());
        let activity = tab.socket_agent_activity.as_ref().expect("socket activity");
        assert_eq!(activity.state, AgentActivityState::Done);
        assert_eq!(activity.origin, AgentActivityOrigin::Socket);
        assert_eq!(activity.text, "socket says done");
    }

    #[test]
    fn output_scan_running_is_blocked_by_done_termprop_activity() {
        let mut tab = stub_tab(1);
        let pane_id = tab.focused_pane_id;
        tab.agent_name = Some("codex".to_string());
        tab.agent_pane_id = Some(pane_id);
        tab.set_pane_agent_activity(
            pane_id,
            AgentActivity::termprop(
                AgentActivityState::Done,
                "termprop says done",
                Some("codex".to_string()),
            ),
        );

        assert!(!apply_scanned_output_activity(
            &mut tab,
            pane_id,
            Some(scanned_running())
        ));

        let activity = tab.pane_agent_activity(pane_id).expect("pane activity");
        assert_eq!(activity.state, AgentActivityState::Done);
        assert_eq!(activity.origin, AgentActivityOrigin::Termprop);
        assert_eq!(activity.text, "termprop says done");
    }
}
