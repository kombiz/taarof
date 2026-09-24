//! Sidebar rendering and interaction wiring.
//!
//! Typed row/view-model state lives in `view_model`, while `.plan` discovery
//! and file-monitor lifecycle live in `discovery`. This module should not own
//! recursive GTK tree scans for routine updates.

mod discovery;
mod view_model;

use adw::prelude::*;
use vte::prelude::TerminalExt;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

#[cfg(test)]
pub(crate) use self::discovery::sidebar_task_launch_request;
pub(crate) use self::discovery::{discover_for_tab, invalidate_and_rediscover_if_target_changed};
pub(crate) use self::view_model::session_label;
use self::view_model::{
    agent_tab_expanded, background_section_handle, install_sidebar_registry, register_tab_row,
    register_workspace_row, remove_tab_row_handle, remove_workspace_row_handle,
    set_agent_tab_expanded, set_background_section_handle, tab_row_handle, workspace_row_handle,
    workspace_row_handle_for_header, workspace_rows_snapshot, BackgroundSectionHandle,
    TabRowHandle, WorkspaceRowHandle,
};
#[cfg(test)]
use self::view_model::{clear_plan_monitor_slot, PlanMonitorState, SidebarViewModel};
use crate::terminal;
use crate::{AppState, RuntimeHandle};

/// Returned from build_sidebar so callers can add tabs directly.
pub struct Sidebar {
    pub container: gtk::Box,
    pub tab_list: gtk::Box,
    /// Search box above the tab list; callers wire it to filter/jump (EXAMPLE-56).
    pub tab_search: gtk::SearchEntry,
    pub update_pending_button: gtk::Button,
    pub tmux_button: gtk::Button,
}

const EMPTY_WORKSPACE_STACK_NAME: &str = "taarof-empty-workspace";
const WORKSPACE_HEADER_NAME_MAX_WIDTH_CHARS: i32 = 32;
const WORK_STREAM_TEXT_MAX_WIDTH_CHARS: i32 = 26;

thread_local! {
    static COMPACT_MODE_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn compact_mode_active() -> bool {
    COMPACT_MODE_ACTIVE.with(Cell::get)
}

#[derive(Default)]
struct WorkStreamUiState {
    palette: crate::work_ledger::WorkStreamPalette,
    filter: crate::work_ledger::WorkStreamFilter,
    filter_options: Vec<(String, crate::work_ledger::WorkStreamFilter)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkStreamLegendActions {
    focus_origin: bool,
    promote_overflow: bool,
}

fn work_stream_legend_actions(color_slot: Option<u8>) -> WorkStreamLegendActions {
    WorkStreamLegendActions {
        focus_origin: true,
        promote_overflow: color_slot.is_none(),
    }
}

#[derive(Clone)]
struct WorkStreamWidgets {
    legend: gtk::Box,
    list: gtk::Box,
    scroll: gtk::ScrolledWindow,
    filter: gtk::ComboBoxText,
    filter_updating: Rc<Cell<bool>>,
    controls: Rc<RefCell<HashMap<String, gtk::Widget>>>,
    clear_pane: gtk::Button,
}

#[derive(Clone, Copy)]
enum WorkStreamControlRole {
    Summary,
    Context,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkStreamRestoreStep {
    Focus,
    VerticalScroll,
    HorizontalScroll,
}

const WORK_STREAM_RESTORE_ORDER: [WorkStreamRestoreStep; 3] = [
    WorkStreamRestoreStep::Focus,
    WorkStreamRestoreStep::VerticalScroll,
    WorkStreamRestoreStep::HorizontalScroll,
];

/// Draw the Offering mark: a prompt chevron presented on a tray.
/// The CSS `color` is used so `[sidebar].accent` overrides tint the mark along
/// with the rest of the chrome instead of baking the default amber into pixels.
fn build_brand_mark() -> gtk::DrawingArea {
    let mark = gtk::DrawingArea::new();
    mark.set_content_width(19);
    mark.set_content_height(19);
    mark.set_can_target(false);
    mark.add_css_class("sidebar-mark");
    mark.set_draw_func(|mark, cr, width, height| {
        let scale = f64::from(width.min(height)) / 34.0;
        let offset_x = (f64::from(width) - 34.0 * scale) / 2.0;
        let offset_y = (f64::from(height) - 34.0 * scale) / 2.0;
        cr.translate(offset_x, offset_y);
        cr.scale(scale, scale);

        // `WidgetExt::color()` requires the optional GTK 4.10 crate feature;
        // StyleContext exposes the same resolved CSS color on older GTK4 builds.
        let color = mark.style_context().color();
        cr.set_source_rgba(
            color.red().into(),
            color.green().into(),
            color.blue().into(),
            color.alpha().into(),
        );
        cr.set_line_width(2.6);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.set_line_join(gtk::cairo::LineJoin::Round);

        cr.move_to(14.5, 7.4);
        cr.line_to(20.1, 13.3);
        cr.line_to(14.5, 19.2);
        let _ = cr.stroke();

        cr.set_line_width(2.4);
        cr.move_to(7.1, 22.9);
        cr.line_to(26.9, 22.9);
        let _ = cr.stroke();

        cr.set_line_width(2.0);
        cr.move_to(10.2, 22.9);
        cr.line_to(8.6, 26.6);
        let _ = cr.stroke();
        cr.move_to(23.8, 22.9);
        cr.line_to(25.4, 26.6);
        let _ = cr.stroke();
    });
    mark
}

fn compact_visibility_override(compact: bool, runtime_owned: bool) -> Option<bool> {
    match (compact, runtime_owned) {
        (true, _) => Some(false),
        (false, false) => Some(true),
        (false, true) => None,
    }
}

fn set_compact_hidden_visibility(widget: &gtk::Widget, compact: bool) {
    if widget.has_css_class("compact-hidden") {
        if let Some(visible) =
            compact_visibility_override(compact, widget.has_css_class("runtime-visibility"))
        {
            widget.set_visible(visible);
        }
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        set_compact_hidden_visibility(&current, compact);
        child = current.next_sibling();
    }
}

fn sidebar_width_for_mode(compact: bool) -> i32 {
    if compact {
        64
    } else {
        220
    }
}

pub fn apply_compact_mode(sidebar: &gtk::Box, compact: bool) {
    COMPACT_MODE_ACTIVE.with(|active| active.set(compact));
    sidebar.set_width_request(sidebar_width_for_mode(compact));
    if compact {
        sidebar.add_css_class("compact");
    } else {
        sidebar.remove_css_class("compact");
    }
    set_compact_hidden_visibility(sidebar.upcast_ref(), compact);
}

/// Build the sidebar: logo, scrollable tab list, new-tab button, session info.
pub fn build_sidebar() -> Sidebar {
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar.set_width_request(220);
    sidebar.add_css_class("sidebar");

    // ── Majlis mark + wordmark ──
    let brand = gtk::Box::new(gtk::Orientation::Horizontal, 9);
    brand.set_halign(gtk::Align::Start);
    brand.add_css_class("sidebar-brand");
    brand.add_css_class("compact-hidden");
    brand.append(&build_brand_mark());

    let logo = gtk::Label::new(Some("taarof"));
    logo.set_halign(gtk::Align::Start);
    logo.add_css_class("sidebar-logo");
    brand.append(&logo);

    // The version chip carries provenance, not just a number: "v0.1.0" alone is
    // exactly the claim that misled the original debugging session. Both the
    // label and the tooltip are rendered from the in-memory identity cache, so
    // no hashing or `git` call happens on the GTK thread (EXAMPLE-164).
    let version = gtk::Label::new(Some(
        &version_label_text(&crate::runtime_identity::cached()),
    ));
    version.add_css_class("sidebar-version");
    // Provenance labels are intentionally more descriptive than a bare
    // version. Keep the full identity in the tooltip, but let the chip shrink
    // inside the fixed-width sidebar instead of increasing its minimum width.
    version.set_ellipsize(gtk::pango::EllipsizeMode::End);
    version.set_tooltip_text(Some(&version_tooltip_text(
        &crate::runtime_identity::cached(),
    )));
    brand.append(&version);
    {
        let version = version.clone();
        crate::update_watch::connect_status_changed(move |_| {
            let identity = crate::runtime_identity::cached();
            version.set_text(&version_label_text(&identity));
            version.set_tooltip_text(Some(&version_tooltip_text(&identity)));
        });
    }

    let update_pending_button = gtk::Button::with_label("Update");
    update_pending_button.add_css_class("update-pending");
    update_pending_button.set_tooltip_text(Some(
        "A newer taarof is installed — reload when you're ready.",
    ));
    update_pending_button.set_visible(false);
    update_pending_button.connect_clicked(show_update_pending_dialog);
    brand.append(&update_pending_button);
    {
        let button = update_pending_button.clone();
        crate::update_watch::connect_status_changed(move |status| {
            button.set_visible(status.state == crate::update_watch::UpdateState::UpdatePending);
        });
    }
    sidebar.append(&brand);

    // Divider
    let div = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    div.add_css_class("sidebar-divider");
    div.add_css_class("compact-hidden");
    sidebar.append(&div);

    // ── Section label ──
    let tabs_label = gtk::Label::new(Some("WORKSPACES"));
    tabs_label.set_halign(gtk::Align::Start);
    tabs_label.add_css_class("sidebar-section-label");
    tabs_label.add_css_class("compact-hidden");
    sidebar.append(&tabs_label);

    // ── Tab search (EXAMPLE-56) ── filters the tab list by name/path.
    let tab_search = gtk::SearchEntry::new();
    tab_search.set_placeholder_text(Some("Search tabs…"));
    tab_search.add_css_class("tab-search-entry");
    tab_search.add_css_class("compact-hidden");
    sidebar.append(&tab_search);

    // ── Scrollable tab list ──
    let tab_list = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&tab_list)
        .build();
    sidebar.append(&scroll);

    // ── New tab buttons ──
    let new_btn = gtk::Button::with_label("+ New tab");
    new_btn.add_css_class("new-tab-button");
    new_btn.add_css_class("compact-hidden");
    new_btn.connect_clicked(move |button| {
        activate_window_action(button, crate::keybindings::Action::NewTab);
    });
    sidebar.append(&new_btn);

    let tmux_ui = crate::terminal::explicit_tmux_tab_ui_state();
    let tmux_btn = gtk::Button::with_label("+ Tmux tab");
    tmux_btn.add_css_class("new-tab-button");
    tmux_btn.add_css_class("tmux-tab-button");
    tmux_btn.add_css_class("compact-hidden");
    tmux_btn.set_tooltip_text(Some(&tmux_ui.tooltip));
    tmux_btn.set_sensitive(tmux_ui.available);
    tmux_btn.connect_clicked(move |button| {
        activate_window_action(button, crate::keybindings::Action::NewTmuxTab);
    });
    sidebar.append(&tmux_btn);

    // ── Session info (bottom) — double-click to rename ──
    let div2 = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    div2.add_css_class("sidebar-divider");
    div2.add_css_class("compact-hidden");
    sidebar.append(&div2);

    let session_section = gtk::Box::new(gtk::Orientation::Vertical, 4);
    session_section.add_css_class("session-section");
    session_section.add_css_class("compact-hidden");

    let session_title = gtk::Label::new(Some("SESSION"));
    session_title.set_halign(gtk::Align::Start);
    session_title.add_css_class("sidebar-section-label");
    session_title.add_css_class("session-section-label");
    session_section.append(&session_title);

    let session_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    session_box.add_css_class("session-info");

    let session_dot = gtk::Label::new(Some("●"));
    session_dot.add_css_class("session-dot");
    let session_label = gtk::Label::new(Some("session"));
    session_label.set_halign(gtk::Align::Start);
    session_label.set_hexpand(true);
    session_label.add_css_class("session-name");
    session_box.append(&session_dot);
    session_box.append(&session_label);

    // Double-click to rename session
    {
        let label_for_gesture = session_label.clone();
        let box_for_gesture = session_box.clone();
        let gesture = gtk::GestureClick::new();
        gesture.set_button(1);
        gesture.connect_released(move |gesture, n_press, _x, _y| {
            if n_press == 2 {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                start_session_rename(&box_for_gesture, &label_for_gesture);
            }
        });
        session_box.add_controller(gesture);
    }

    session_section.append(&session_box);
    sidebar.append(&session_section);

    install_sidebar_registry(&tab_list, &session_label);

    Sidebar {
        container: sidebar,
        tab_list,
        tab_search,
        update_pending_button,
        tmux_button: tmux_btn,
    }
}

pub(crate) fn refresh_update_pending_chip(button: &gtk::Button) {
    button.set_visible(
        crate::update_watch::cached_status().state
            == crate::update_watch::UpdateState::UpdatePending,
    );
}

pub(crate) fn refresh_tmux_button(button: &gtk::Button) {
    let state = crate::terminal::explicit_tmux_tab_ui_state();
    button.set_tooltip_text(Some(&state.tooltip));
    button.set_sensitive(state.available);
}

/// Compact provenance for the sidebar chip.
///
/// Only a positively proved match is rendered bare. Anything else is marked, so
/// the chip can never be read as "this is the current source" by default.
pub(crate) fn version_label_text(identity: &crate::runtime_identity::RuntimeIdentity) -> String {
    use crate::runtime_identity::SourceState;
    let version = identity.app_version;
    match identity.source_state {
        SourceState::Matches => format!("v{version} · {}", identity.short_source_label()),
        SourceState::Differs => format!("v{version} · source differs"),
        SourceState::Unknown => format!("v{version} · source unknown"),
    }
}

pub(crate) fn version_tooltip_text(identity: &crate::runtime_identity::RuntimeIdentity) -> String {
    use crate::runtime_identity::SourceState;
    let unknown = || "unknown".to_string();
    let source_state = match identity.source_state {
        SourceState::Matches => "matches this checkout",
        SourceState::Differs => "differs from this checkout",
        SourceState::Unknown => "unknown — cannot be proved",
    };
    let running_vs_installed = match identity.running_matches_installed {
        Some(true) => "yes",
        Some(false) => "no — an update is installed",
        None => "unknown",
    };
    format!(
        "Version: {}\nBuilt from: {}\nCheckout now: {}\nSource match: {}\nRunning is installed binary: {}\nRunning: {}\nInstalled: {}",
        identity.app_version,
        identity
            .build
            .source_describe
            .clone()
            .or_else(|| identity.build.source_revision.clone())
            .unwrap_or_else(unknown),
        identity
            .source
            .describe
            .clone()
            .or_else(|| identity.source.revision.clone())
            .unwrap_or_else(unknown),
        source_state,
        running_vs_installed,
        identity.running.path.clone().unwrap_or_else(unknown),
        identity.installed.path.clone().unwrap_or_else(unknown),
    )
}

fn short_hash(value: Option<&str>) -> &str {
    value
        .map(|hash| &hash[..hash.len().min(12)])
        .unwrap_or("unavailable")
}

fn update_reason_label(reason: Option<crate::update_watch::UpdateReason>) -> String {
    reason
        .and_then(|reason| serde_json::to_value(reason).ok())
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_string())
}

fn show_update_pending_dialog(button: &gtk::Button) {
    let status = crate::update_watch::cached_status();
    if status.state != crate::update_watch::UpdateState::UpdatePending {
        refresh_update_pending_chip(button);
        return;
    }

    let running_deleted = if status.running.deleted {
        " (deleted)"
    } else {
        ""
    };
    let installed_path = status.installed.path.as_deref().unwrap_or("unavailable");
    let checked_at = status
        .checked_at_unix_ms
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unavailable".to_string());
    let body = format!(
        "Running: {}{}\nInstalled: {}\nInstalled path: {}\nChecked at (Unix ms): {}\nReason: {}\n\nReload saves the session and starts the installed version. Tmux panes reconnect. Non-tmux shells restart; detected agent panes automatically run their saved resume command.",
        short_hash(status.running.sha256.as_deref()),
        running_deleted,
        short_hash(status.installed.sha256.as_deref()),
        installed_path,
        checked_at,
        update_reason_label(status.reason),
    );
    let details = serde_json::to_string_pretty(&status).unwrap_or_else(|_| "{}".to_string());
    let Some(parent) = widget_window(button) else {
        return;
    };
    // MessageDialog keeps the repo's documented libadwaita 1.4 compatibility
    // floor while preserving an explicit operator-controlled restart boundary.
    let dialog = adw::MessageDialog::new(Some(&parent), Some("Update pending"), Some(&body));
    for (id, label) in [
        (
            crate::update_watch::dialog_actions()[0],
            "Reload into update",
        ),
        (crate::update_watch::dialog_actions()[1], "Copy details"),
        (crate::update_watch::dialog_actions()[2], "Close"),
    ] {
        dialog.add_response(id, label);
    }
    dialog.set_response_appearance("reload", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    dialog.choose(None::<&gio::Cancellable>, move |response| {
        match response.as_str() {
            "reload" => match crate::update_watch::spawn_installed_reload(&status) {
                Ok(()) => parent.close(),
                Err(error) => crate::show_error_toast(&error),
            },
            "copy-details" => {
                copy_text_to_clipboard(&details);
                crate::show_toast("Update details copied");
            }
            _ => {}
        }
    });
}

/// Build the single session-wide chronological work surface for the right dock.
pub(crate) fn build_session_work_panel(
    state: Rc<RefCell<AppState>>,
    tab_list: gtk::Box,
    term_stack: gtk::Stack,
) -> gtk::Box {
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 8);
    panel.set_hexpand(true);
    panel.set_vexpand(true);

    let work_filter = gtk::ComboBoxText::new();
    work_filter.add_css_class("work-stream-filter");
    work_filter.set_tooltip_text(Some("Filter the session work stream"));
    panel.append(&work_filter);

    let work_actions = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let clear_pane_work = gtk::Button::with_label("Clear pane…");
    clear_pane_work.set_sensitive(false);
    clear_pane_work.add_css_class("flat");
    clear_pane_work.set_tooltip_text(Some("Clear history for the pane selected by the filter"));
    let clear_session_work = gtk::Button::with_label("Clear session…");
    clear_session_work.add_css_class("flat");
    clear_session_work.set_tooltip_text(Some("Clear all work history in this Taarof session"));
    work_actions.append(&clear_pane_work);
    work_actions.append(&clear_session_work);
    panel.append(&work_actions);

    let work_content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    work_content.set_hexpand(true);
    work_content.set_halign(gtk::Align::Fill);
    let work_legend = gtk::Box::new(gtk::Orientation::Vertical, 4);
    work_legend.set_hexpand(true);
    work_legend.set_halign(gtk::Align::Fill);
    work_legend.add_css_class("work-stream-legend");
    work_content.append(&work_legend);

    let work_list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    work_list.set_hexpand(true);
    work_list.set_halign(gtk::Align::Fill);
    work_list.add_css_class("work-ledger-list");
    work_content.append(&work_list);
    let work_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_height(180)
        .child(&work_content)
        .build();
    work_scroll.set_propagate_natural_width(false);
    work_scroll.set_hexpand(true);
    work_scroll.set_vexpand(true);
    panel.append(&work_scroll);

    let work_widgets = WorkStreamWidgets {
        legend: work_legend,
        list: work_list,
        scroll: work_scroll,
        filter: work_filter,
        filter_updating: Rc::new(Cell::new(false)),
        controls: Rc::new(RefCell::new(HashMap::new())),
        clear_pane: clear_pane_work.clone(),
    };
    let restored_work_view = state.borrow().work_ledger.view_preferences();
    let work_ui = Rc::new(RefCell::new(WorkStreamUiState {
        palette: restored_work_view.palette,
        filter: restored_work_view.filter,
        filter_options: Vec::new(),
    }));
    {
        let work_ui = work_ui.clone();
        let state = state.clone();
        let widgets = work_widgets.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        clear_pane_work.connect_clicked(move |_| {
            let crate::work_ledger::WorkStreamFilter::Pane(origin) =
                work_ui.borrow().filter.clone()
            else {
                return;
            };
            prompt_clear_work_history(
                &tab_list,
                &state,
                &widgets,
                &work_ui,
                &term_stack,
                Some(origin),
            );
        });
    }
    {
        let work_ui = work_ui.clone();
        let state = state.clone();
        let widgets = work_widgets.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        clear_session_work.connect_clicked(move |_| {
            prompt_clear_work_history(&tab_list, &state, &widgets, &work_ui, &term_stack, None);
        });
    }
    {
        let work_ui = work_ui.clone();
        let work_widgets_for_change = work_widgets.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        work_widgets.filter.connect_changed(move |combo| {
            if work_widgets_for_change.filter_updating.get() {
                return;
            }
            let selected = combo.active().map(|value| value as usize);
            {
                let mut ui = work_ui.borrow_mut();
                let Some((_, filter)) = selected
                    .and_then(|index| ui.filter_options.get(index))
                    .cloned()
                else {
                    return;
                };
                ui.filter = filter;
                state.borrow_mut().set_work_stream_preferences(
                    crate::work_ledger::WorkStreamPreferences {
                        palette: ui.palette.clone(),
                        filter: ui.filter.clone(),
                    },
                );
            }
            refresh_work_ledger(
                &work_widgets_for_change,
                &work_ui,
                &state,
                &tab_list,
                &term_stack,
            );
        });
    }
    refresh_work_ledger(&work_widgets, &work_ui, &state, &tab_list, &term_stack);
    {
        let work_widgets = work_widgets.clone();
        let work_ui = work_ui.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        glib::timeout_add_seconds_local(1, move || {
            refresh_work_ledger(&work_widgets, &work_ui, &state, &tab_list, &term_stack);
            glib::ControlFlow::Continue
        });
    }

    panel
}

fn refresh_work_ledger(
    widgets: &WorkStreamWidgets,
    ui_state: &Rc<RefCell<WorkStreamUiState>>,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
) {
    let adjustment = widgets.scroll.vadjustment();
    let previous_scroll = adjustment.value();
    let was_at_bottom =
        work_stream_is_at_bottom(previous_scroll, adjustment.upper(), adjustment.page_size());
    let focused_control = widgets
        .controls
        .borrow()
        .iter()
        .find_map(|(key, widget)| widget.has_focus().then(|| key.clone()));
    widgets.controls.borrow_mut().clear();
    while let Some(child) = widgets.legend.first_child() {
        widgets.legend.remove(&child);
    }
    while let Some(child) = widgets.list.first_child() {
        widgets.list.remove(&child);
    }
    let (records, metadata, reconciliations, record_truths, restore_health, truth_summary) = {
        let st = state.borrow();
        let records = st.work_ledger.records();
        let metadata = work_stream_pane_metadata(&st, &records);
        let latest_status_records = crate::work_ledger::latest_task_status_records(&records);
        let truth_summary = crate::work_ledger::project_task_truth_summary(
            &crate::work_ledger::task_truth_inputs(&st, &metadata),
        );
        let reconciliations = records
            .iter()
            .map(|record| (record.seq, st.work_ledger.reconciliation_for(record.seq)))
            .collect::<HashMap<_, _>>();
        let record_truths = records
            .iter()
            .map(|record| {
                (
                    record.seq,
                    crate::work_ledger::task_truth_for_record_with(
                        &st,
                        &metadata,
                        &latest_status_records,
                        record,
                    ),
                )
            })
            .collect::<HashMap<_, _>>();
        (
            records,
            metadata,
            reconciliations,
            record_truths,
            st.work_ledger.restore_health().clone(),
            truth_summary,
        )
    };

    let mut ui = ui_state.borrow_mut();
    let selected_filter = ui.filter.clone();
    let mut projection = crate::work_ledger::project_work_stream(
        &records,
        &mut ui.palette,
        &selected_filter,
        &metadata,
    );
    let options = work_stream_filter_options(&projection, &records);
    let (resolved_filter, _) = resolve_work_stream_filter_selection(&ui.filter, &options);
    if resolved_filter != ui.filter {
        ui.filter = resolved_filter;
        let selected_filter = ui.filter.clone();
        projection = crate::work_ledger::project_work_stream(
            &records,
            &mut ui.palette,
            &selected_filter,
            &metadata,
        );
    }
    let filter_update = if ui.filter_options != options {
        let (_, selected) = resolve_work_stream_filter_selection(&ui.filter, &options);
        ui.filter_options = options.clone();
        Some((options, selected))
    } else {
        None
    };
    widgets.clear_pane.set_sensitive(matches!(
        ui.filter,
        crate::work_ledger::WorkStreamFilter::Pane(_)
    ));
    let effective_view = crate::work_ledger::WorkStreamPreferences {
        palette: ui.palette.clone(),
        filter: ui.filter.clone(),
    };
    drop(ui);
    state
        .borrow_mut()
        .set_work_stream_preferences(effective_view);
    if restore_health.status == "degraded" {
        let warning = gtk::Label::new(Some(
            restore_health
                .detail
                .as_deref()
                .unwrap_or("Restored work history is incomplete"),
        ));
        configure_work_stream_label(&warning, 3);
        warning.set_halign(gtk::Align::Start);
        warning.add_css_class("warning");
        warning.set_tooltip_text(Some("Taarof continued with partial session work history"));
        widgets.legend.append(&warning);
    }
    if let Some((options, selected)) = filter_update {
        widgets.filter_updating.set(true);
        widgets.filter.remove_all();
        for (label, _) in &options {
            widgets.filter.append_text(label);
        }
        widgets.filter.set_active(Some(selected as u32));
        widgets.filter_updating.set(false);
    }

    let counts = gtk::Label::new(Some(&format!(
        "{} live/open · {} historical{}",
        truth_summary.live_open_count,
        truth_summary.historical_count,
        if truth_summary.mismatch_count > 0 {
            format!(" · {} mismatch", truth_summary.mismatch_count)
        } else {
            String::new()
        }
    )));
    counts.set_halign(gtk::Align::Start);
    counts.add_css_class("dim-label");
    widgets.legend.append(&counts);

    for item in &projection.legend {
        let truth = truth_summary
            .items
            .iter()
            .find(|truth| truth.pane_origin == item.pane_origin);
        let label = truth.map_or_else(
            || work_stream_legend_text(item),
            |truth| {
                format!(
                    "{}\n{}",
                    work_stream_legend_text(item),
                    task_truth_labels(truth)
                )
            },
        );
        let tooltip = truth.map_or_else(
            || work_stream_legend_tooltip(item),
            |truth| {
                format!(
                    "{} · {}",
                    work_stream_legend_tooltip(item),
                    task_truth_tooltip(truth)
                )
            },
        );
        let actions = work_stream_legend_actions(item.color_slot);
        let identity = metadata
            .get(&item.pane_origin)
            .and_then(|meta| meta.identity.clone());
        if actions.promote_overflow {
            let promote = gtk::Button::with_label(&label);
            configure_work_stream_button_label(&promote, 8);
            promote.set_halign(gtk::Align::Fill);
            promote.set_hexpand(true);
            promote.add_css_class("flat");
            promote.add_css_class("work-stream-legend-item");
            promote.add_css_class("work-stream-overflow");
            promote.set_tooltip_text(Some(&format!(
                "{tooltip}\nFocus this pane and promote it into color slot P5"
            )));
            register_work_stream_control(widgets, format!("legend:{}", item.pane_origin), &promote);
            let origin = item.pane_origin.clone();
            let ui_state = ui_state.clone();
            let widgets_for_refresh = widgets.clone();
            let state = state.clone();
            let tab_list = tab_list.clone();
            let term_stack = term_stack.clone();
            promote.connect_clicked(move |_| {
                if let Some(identity) = identity.as_ref() {
                    let _ = focus_work_record_origin(&state, &tab_list, &term_stack, identity);
                }
                if ui_state.borrow_mut().palette.promote(&origin) {
                    let view = {
                        let ui = ui_state.borrow();
                        crate::work_ledger::WorkStreamPreferences {
                            palette: ui.palette.clone(),
                            filter: ui.filter.clone(),
                        }
                    };
                    state.borrow_mut().set_work_stream_preferences(view);
                    refresh_work_ledger(
                        &widgets_for_refresh,
                        &ui_state,
                        &state,
                        &tab_list,
                        &term_stack,
                    );
                }
            });
            widgets.legend.append(&promote);
        } else {
            let focus = gtk::Button::with_label(&label);
            configure_work_stream_button_label(&focus, 8);
            focus.set_halign(gtk::Align::Fill);
            focus.set_hexpand(true);
            focus.add_css_class("flat");
            focus.add_css_class("work-stream-legend-item");
            focus.set_tooltip_text(Some(&format!("{tooltip}\nFocus originating pane")));
            add_work_stream_color_class(&focus, item.color_slot);
            register_work_stream_control(widgets, format!("legend:{}", item.pane_origin), &focus);
            let state = state.clone();
            let tab_list = tab_list.clone();
            let term_stack = term_stack.clone();
            focus.connect_clicked(move |_| {
                if actions.focus_origin {
                    if let Some(identity) = identity.as_ref() {
                        let _ = focus_work_record_origin(&state, &tab_list, &term_stack, identity);
                    }
                }
            });
            widgets.legend.append(&focus);
        }
    }

    if projection.entries.is_empty() {
        let message = if records.is_empty() {
            "No recorded work in this session"
        } else {
            "No work matches this filter"
        };
        let empty = gtk::Label::new(Some(message));
        empty.set_halign(gtk::Align::Start);
        empty.set_wrap(true);
        empty.add_css_class("dim-label");
        widgets.list.append(&empty);
        restore_work_stream_interaction(widgets, focused_control, was_at_bottom, previous_scroll);
        return;
    }
    let now = crate::events::unix_time_ms();
    for entry in projection.entries {
        let record = entry.record;
        let reconciliation = reconciliations
            .get(&record.seq)
            .cloned()
            .unwrap_or_else(|| crate::work_ledger::WorkReconciliation {
                status: crate::work_ledger::WorkReconciliationStatus::Pending,
                source: "runtime".to_string(),
                reason: "Waiting for reconciliation".to_string(),
                origin_status: "unknown".to_string(),
                current_value: None,
                checked_at_unix_ms: None,
            });
        let row = gtk::Box::new(gtk::Orientation::Vertical, 1);
        row.set_hexpand(true);
        row.set_halign(gtk::Align::Fill);
        row.add_css_class("work-ledger-row");
        add_work_stream_color_class(&row, entry.color_slot);
        if entry.color_slot.is_none() {
            row.add_css_class("work-stream-overflow");
        }
        row.add_css_class(match reconciliation.status {
            crate::work_ledger::WorkReconciliationStatus::Pending => "work-reconcile-pending",
            crate::work_ledger::WorkReconciliationStatus::Verified => "work-reconcile-verified",
            crate::work_ledger::WorkReconciliationStatus::Stale => "work-reconcile-stale",
            crate::work_ledger::WorkReconciliationStatus::Unverified => "work-reconcile-unverified",
        });
        if let Some(pr) = record.pull_request.as_ref() {
            let summary = gtk::Button::with_label(&record.summary);
            register_work_stream_control(
                widgets,
                work_stream_control_key(record.seq, WorkStreamControlRole::Summary),
                &summary,
            );
            configure_work_stream_button_label(&summary, 3);
            summary.set_halign(gtk::Align::Fill);
            summary.set_hexpand(true);
            summary.add_css_class("flat");
            summary.set_tooltip_text(Some(&format!(
                "Open PR #{} actions for {}",
                pr.number, pr.repository
            )));
            let identity = record.identity.clone();
            let state = state.clone();
            let tab_list = tab_list.clone();
            let term_stack = term_stack.clone();
            summary.connect_clicked(move |_| {
                if focus_work_record_origin(&state, &tab_list, &term_stack, &identity) {
                    crate::task_panel::select_pull_requests_for_active_context(&state, &tab_list);
                }
            });
            row.append(&summary);
        } else {
            let summary = gtk::Button::with_label(&record.summary);
            register_work_stream_control(
                widgets,
                work_stream_control_key(record.seq, WorkStreamControlRole::Summary),
                &summary,
            );
            configure_work_stream_button_label(&summary, 3);
            summary.set_halign(gtk::Align::Fill);
            summary.set_hexpand(true);
            summary.add_css_class("flat");
            summary.add_css_class("work-stream-summary");
            summary.set_tooltip_text(Some("Focus originating pane"));
            let identity = record.identity.clone();
            let state = state.clone();
            let tab_list = tab_list.clone();
            let term_stack = term_stack.clone();
            summary.connect_clicked(move |_| {
                let _ = focus_work_record_origin(&state, &tab_list, &term_stack, &identity);
            });
            row.append(&summary);
        }
        let truth = record_truths.get(&record.seq);
        let task = record.identity.task_id.as_deref().unwrap_or("No task");
        let pr = record
            .pull_request
            .as_ref()
            .map(|pr| format!(" · PR #{}", pr.number))
            .unwrap_or_default();
        let reconciliation_label = match reconciliation.status {
            crate::work_ledger::WorkReconciliationStatus::Pending => "pending",
            crate::work_ledger::WorkReconciliationStatus::Verified => "verified",
            crate::work_ledger::WorkReconciliationStatus::Stale => "stale",
            crate::work_ledger::WorkReconciliationStatus::Unverified => "unverified",
        };
        let axes = truth.map_or_else(|| reconciliation_label.to_string(), task_truth_labels);
        let meta = gtk::Button::with_label(&format!(
            "{} · pane {} · {task}{pr}\n{axes} · #{} · {}",
            entry.marker,
            record.identity.pane_id,
            record.seq,
            crate::work_ledger::format_age(record.ts_unix_ms, now)
        ));
        register_work_stream_control(
            widgets,
            work_stream_control_key(record.seq, WorkStreamControlRole::Context),
            &meta,
        );
        // The second line now carries six independent truth axes. Allow it to
        // wrap instead of ellipsizing verification, last-checked, and mismatch
        // details out of the visible row.
        configure_work_stream_button_label(&meta, 6);
        meta.set_halign(gtk::Align::Fill);
        meta.set_hexpand(true);
        meta.add_css_class("flat");
        meta.add_css_class("work-truth-binding");
        if truth.is_some_and(|truth| truth.mismatch.is_some()) {
            meta.add_css_class("work-truth-mismatch");
        }
        let truth_tooltip = truth.map_or_else(String::new, |truth| {
            format!(" · {}", task_truth_tooltip(truth))
        });
        meta.set_tooltip_text(Some(&format!(
            "{}: {} · origin {}{truth_tooltip} · Focus originating pane",
            reconciliation.source, reconciliation.reason, reconciliation.origin_status
        )));
        {
            let identity = record.identity.clone();
            let state = state.clone();
            let tab_list = tab_list.clone();
            let term_stack = term_stack.clone();
            meta.connect_clicked(move |_| {
                let _ = focus_work_record_origin(&state, &tab_list, &term_stack, &identity);
            });
        }
        row.append(&meta);
        widgets.list.append(&row);
    }
    restore_work_stream_interaction(widgets, focused_control, was_at_bottom, previous_scroll);
}

fn work_stream_origin_state_label(
    state: crate::work_ledger::WorkStreamOriginState,
) -> &'static str {
    match state {
        crate::work_ledger::WorkStreamOriginState::Live => "Live",
        crate::work_ledger::WorkStreamOriginState::Lazy => "Lazy",
        crate::work_ledger::WorkStreamOriginState::Historical => "Historical",
    }
}

fn task_truth_labels(truth: &crate::work_ledger::TaskTruth) -> String {
    let checked = truth.last_checked_unix_ms.map_or_else(
        || "Never checked".to_string(),
        |checked| {
            format!(
                "Checked {}",
                crate::work_ledger::format_age(checked, crate::events::unix_time_ms())
            )
        },
    );
    let mismatch = truth
        .mismatch
        .as_ref()
        .map_or_else(String::new, |mismatch| {
            format!(" · {} mismatch: {}", mismatch.source, mismatch.detail)
        });
    format!(
        "{} · {} · {} · {} · {} ({}) · {checked}{mismatch}",
        canonical_task_state_label(truth.canonical),
        binding_state_label(truth.binding),
        execution_state_label(truth.execution),
        work_stream_origin_state_label(truth.origin),
        reconciliation_state_label(truth.verification),
        truth.verification_source,
    )
}

fn task_truth_tooltip(truth: &crate::work_ledger::TaskTruth) -> String {
    task_truth_labels(truth)
}

fn canonical_task_state_label(state: crate::work_ledger::CanonicalTaskState) -> &'static str {
    match state {
        crate::work_ledger::CanonicalTaskState::Todo => "Todo",
        crate::work_ledger::CanonicalTaskState::InProgress => "In progress",
        crate::work_ledger::CanonicalTaskState::Blocked => "Blocked",
        crate::work_ledger::CanonicalTaskState::Done => "Done",
        crate::work_ledger::CanonicalTaskState::Cancelled => "Cancelled",
        crate::work_ledger::CanonicalTaskState::Absent => "Absent",
        crate::work_ledger::CanonicalTaskState::Unknown => "Unknown status",
    }
}

fn binding_state_label(state: crate::work_ledger::BindingState) -> &'static str {
    match state {
        crate::work_ledger::BindingState::Bound => "Bound",
        crate::work_ledger::BindingState::Unbound => "Unbound",
    }
}

fn execution_state_label(state: crate::work_ledger::ExecutionState) -> &'static str {
    match state {
        crate::work_ledger::ExecutionState::Running => "Running",
        crate::work_ledger::ExecutionState::Idle => "Idle",
        crate::work_ledger::ExecutionState::Unknown => "Execution unknown",
    }
}

fn reconciliation_state_label(state: crate::work_ledger::WorkReconciliationStatus) -> &'static str {
    match state {
        crate::work_ledger::WorkReconciliationStatus::Pending => "Pending",
        crate::work_ledger::WorkReconciliationStatus::Verified => "Verified",
        crate::work_ledger::WorkReconciliationStatus::Stale => "Stale",
        crate::work_ledger::WorkReconciliationStatus::Unverified => "Unverified",
    }
}

fn prompt_clear_work_history(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    widgets: &WorkStreamWidgets,
    ui_state: &Rc<RefCell<WorkStreamUiState>>,
    term_stack: &gtk::Stack,
    pane_origin: Option<String>,
) {
    let Some(window) = widget_window(tab_list) else {
        return;
    };
    let (title, message, accept) = if let Some(origin) = pane_origin.as_deref() {
        let marker = ui_state
            .borrow()
            .palette
            .marker(origin)
            .unwrap_or_else(|| "pane".to_string());
        (
            "Clear Pane Work History",
            format!(
                "Clear recorded work for {marker}? This does not change .plan tasks, pull requests, pane bindings, or agent transcripts."
            ),
            "Clear Pane History",
        )
    } else {
        (
            "Clear Session Work History",
            "Clear all recorded work for this Taarof session? This does not change .plan tasks, pull requests, pane bindings, or agent transcripts."
                .to_string(),
            "Clear Session History",
        )
    };
    let dialog = gtk::Dialog::builder()
        .transient_for(&window)
        .modal(true)
        .title(title)
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button(accept, gtk::ResponseType::Accept);
    let label = gtk::Label::new(Some(&message));
    label.set_wrap(true);
    label.set_halign(gtk::Align::Start);
    dialog.content_area().append(&label);
    {
        let state = state.clone();
        let widgets = widgets.clone();
        let ui_state = ui_state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                let (removed, barrier) = {
                    let mut state = state.borrow_mut();
                    if let Some(origin) = pane_origin.as_deref() {
                        state.clear_pane_work_ledger(origin)
                    } else {
                        state.clear_session_work_ledger()
                    }
                };
                let restored = state.borrow().work_ledger.view_preferences();
                {
                    let mut ui = ui_state.borrow_mut();
                    ui.palette = restored.palette;
                    ui.filter = restored.filter;
                }
                let state = state.clone();
                let widgets = widgets.clone();
                let ui_state = ui_state.clone();
                let tab_list = tab_list.clone();
                let term_stack = term_stack.clone();
                glib::spawn_future_local(async move {
                    let persistence_result = match barrier {
                        Some(barrier) => gio::spawn_blocking(move || match barrier.recv() {
                            Ok(result) => result.map_err(|error| error.to_string()),
                            Err(error) => Err(format!("writer acknowledgement failed: {error}")),
                        })
                        .await
                        .unwrap_or_else(|_| Err("writer task did not complete".to_string())),
                        None => Err("work history persistence is unavailable".to_string()),
                    };
                    refresh_work_ledger(&widgets, &ui_state, &state, &tab_list, &term_stack);
                    match persistence_result {
                        Ok(()) => crate::show_toast(&format!("Cleared {removed} work record(s)")),
                        Err(error) => crate::show_error_toast(&format!(
                            "Cleared in memory, but could not persist the change: {error}"
                        )),
                    }
                });
            }
            dialog.close();
        });
    }
    dialog.present();
}

fn work_stream_legend_text(item: &crate::work_ledger::WorkStreamLegendItem) -> String {
    let state = work_stream_origin_state_label(item.origin_state);
    let agent = item.agent_name.as_deref().unwrap_or("No active agent");
    let task = work_stream_task_label(item);
    format!(
        "{} · {state}\n{} / {}\npane {} · {} · {agent}\n{task}",
        item.marker, item.workspace_name, item.tab_name, item.pane_id, item.pane_origin
    )
}

fn work_stream_legend_tooltip(item: &crate::work_ledger::WorkStreamLegendItem) -> String {
    let state = work_stream_origin_state_label(item.origin_state);
    let agent = item.agent_name.as_deref().unwrap_or("No active agent");
    let task = work_stream_task_label(item);
    format!(
        "{} · {}/{} · pane {} · {} · {agent} · {task} · {state}",
        item.marker, item.workspace_name, item.tab_name, item.pane_id, item.pane_origin
    )
}

fn work_stream_task_label(item: &crate::work_ledger::WorkStreamLegendItem) -> String {
    match (item.task_id.as_deref(), item.task_title.as_deref()) {
        (Some(id), Some(title)) => format!("{id} — {title}"),
        (Some(id), None) => id.to_string(),
        (None, _) => "Unbound".to_string(),
    }
}

fn resolve_work_stream_filter_selection(
    current: &crate::work_ledger::WorkStreamFilter,
    options: &[(String, crate::work_ledger::WorkStreamFilter)],
) -> (crate::work_ledger::WorkStreamFilter, usize) {
    options
        .iter()
        .position(|(_, filter)| filter == current)
        .map_or_else(
            || (crate::work_ledger::WorkStreamFilter::All, 0),
            |selected| (current.clone(), selected),
        )
}

fn work_stream_control_key(seq: u64, role: WorkStreamControlRole) -> String {
    let role = match role {
        WorkStreamControlRole::Summary => "summary",
        WorkStreamControlRole::Context => "context",
    };
    format!("record:{seq}:{role}")
}

fn register_work_stream_control<W: IsA<gtk::Widget>>(
    widgets: &WorkStreamWidgets,
    key: String,
    widget: &W,
) {
    widget.set_widget_name(&key);
    widgets
        .controls
        .borrow_mut()
        .insert(key, widget.clone().upcast());
}

fn restore_work_stream_interaction(
    widgets: &WorkStreamWidgets,
    focused_control: Option<String>,
    was_at_bottom: bool,
    previous_scroll: f64,
) {
    let widgets = widgets.clone();
    let vertical = widgets.scroll.vadjustment();
    let horizontal = widgets.scroll.hadjustment();
    horizontal.set_value(0.0);
    glib::idle_add_local_once(move || {
        for step in WORK_STREAM_RESTORE_ORDER {
            match step {
                WorkStreamRestoreStep::Focus => {
                    let Some(key) = focused_control.as_ref() else {
                        continue;
                    };
                    let control = widgets.controls.borrow().get(key).cloned();
                    if let Some(control) = control {
                        control.grab_focus();
                    } else {
                        widgets.filter.grab_focus();
                    }
                }
                WorkStreamRestoreStep::VerticalScroll => {
                    vertical.set_value(work_stream_scroll_target(
                        was_at_bottom,
                        previous_scroll,
                        vertical.upper(),
                        vertical.page_size(),
                    ));
                }
                WorkStreamRestoreStep::HorizontalScroll => horizontal.set_value(0.0),
            }
        }
    });
}

fn configure_work_stream_button_label(button: &gtk::Button, lines: i32) {
    let Some(label) = button.child().and_downcast::<gtk::Label>() else {
        return;
    };
    configure_work_stream_label(&label, lines);
}

fn configure_work_stream_label(label: &gtk::Label, lines: i32) {
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_max_width_chars(WORK_STREAM_TEXT_MAX_WIDTH_CHARS);
    label.set_lines(lines);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
}

fn work_stream_filter_options(
    projection: &crate::work_ledger::WorkStreamProjection,
    records: &[crate::work_ledger::WorkRecord],
) -> Vec<(String, crate::work_ledger::WorkStreamFilter)> {
    let mut options = vec![
        (
            "Live / open work".to_string(),
            crate::work_ledger::WorkStreamFilter::All,
        ),
        (
            "Attention / blocked".to_string(),
            crate::work_ledger::WorkStreamFilter::Attention,
        ),
        (
            "History (done / removed panes)".to_string(),
            crate::work_ledger::WorkStreamFilter::History,
        ),
    ];
    for item in &projection.legend {
        options.push((
            format!("{} · {} pane {}", item.marker, item.tab_name, item.pane_id),
            crate::work_ledger::WorkStreamFilter::Pane(item.pane_origin.clone()),
        ));
    }
    let mut tasks = Vec::<String>::new();
    for record in records {
        let Some(task_id) = record.identity.task_id.as_ref() else {
            continue;
        };
        if !tasks.iter().any(|known| known == task_id) {
            tasks.push(task_id.clone());
        }
    }
    tasks.sort();
    options.extend(tasks.into_iter().map(|task_id| {
        (
            format!("Task {task_id}"),
            crate::work_ledger::WorkStreamFilter::Task(task_id),
        )
    }));
    options
}

fn work_stream_pane_metadata(
    state: &AppState,
    records: &[crate::work_ledger::WorkRecord],
) -> HashMap<String, crate::work_ledger::WorkStreamPaneMeta> {
    crate::work_ledger::work_stream_pane_metadata(state, records)
}

fn add_work_stream_color_class<W: IsA<gtk::Widget>>(widget: &W, slot: Option<u8>) {
    if let Some(slot) = slot {
        widget.add_css_class(&format!("work-stream-color-{}", slot + 1));
    }
}

fn work_stream_is_at_bottom(value: f64, upper: f64, page_size: f64) -> bool {
    upper <= page_size || value + page_size >= upper - 2.0
}

fn work_stream_scroll_target(
    was_at_bottom: bool,
    previous_value: f64,
    upper: f64,
    page_size: f64,
) -> f64 {
    let maximum = (upper - page_size).max(0.0);
    if was_at_bottom {
        maximum
    } else {
        previous_value.min(maximum)
    }
}

fn focus_work_record_origin(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    identity: &crate::work_ledger::WorkIdentity,
) -> bool {
    let (target, previous) = {
        let st = state.borrow();
        let previous = st.active_tab().map(|tab| (tab.id, tab.focused_pane_id));
        (
            crate::work_ledger::runtime_target_for_identity(&st, identity),
            previous,
        )
    };
    let Some((tab_id, _pane_id)) = target else {
        crate::show_error_toast("The originating pane is no longer available");
        return false;
    };
    if !activate_tab(tab_list, state, term_stack, tab_id) {
        return false;
    }
    // Activating a lazy tab materializes its saved pane tree. Resolve the
    // stable origin again rather than trusting the pre-activation runtime id.
    let revalidated = {
        let st = state.borrow();
        crate::work_ledger::runtime_target_for_identity(&st, identity)
            .filter(|(tab_id, _)| !st.pending_tab_restores.contains_key(tab_id))
    };
    let Some((revalidated_tab, revalidated_pane)) = revalidated.filter(|(tab, _)| *tab == tab_id)
    else {
        restore_previous_work_focus(state, tab_list, term_stack, previous);
        crate::show_error_toast("The originating pane changed while it was opening");
        return false;
    };
    let terminal = {
        let mut st = state.borrow_mut();
        st.find_tab_mut(revalidated_tab).and_then(|tab| {
            let terminal = tab
                .panes
                .leaf(revalidated_pane)
                .map(|leaf| leaf.terminal.clone());
            if terminal.is_some() {
                tab.focused_pane_id = revalidated_pane;
            }
            terminal
        })
    };
    let Some(terminal) = terminal else {
        restore_previous_work_focus(state, tab_list, term_stack, previous);
        crate::show_error_toast("The originating pane is not available to focus");
        return false;
    };
    terminal.grab_focus();
    true
}

fn restore_previous_work_focus(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    previous: Option<(u32, u32)>,
) {
    let Some((tab_id, pane_id)) = previous else {
        return;
    };
    if !activate_tab(tab_list, state, term_stack, tab_id) {
        return;
    }
    let terminal = {
        let mut st = state.borrow_mut();
        st.find_tab_mut(tab_id).and_then(|tab| {
            tab.panes.leaf(pane_id).map(|leaf| {
                tab.focused_pane_id = pane_id;
                leaf.terminal.clone()
            })
        })
    };
    if let Some(terminal) = terminal {
        terminal.grab_focus();
    }
}

/// Case-insensitive substring match used by the sidebar tab search (EXAMPLE-56).
/// An empty/whitespace query matches every tab; otherwise the query must be a
/// substring of the tab name or its working-directory path (the `cwd` form,
/// including a remote `file://host/path`).
pub(crate) fn tab_matches_query(name: &str, cwd: Option<&str>, query: &str) -> bool {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    if name.to_lowercase().contains(&needle) {
        return true;
    }
    cwd.is_some_and(|cwd| cwd.to_lowercase().contains(&needle))
}

/// Filter the sidebar tab list by `query`, toggling row visibility through the
/// existing row registry. While searching, matching tab rows are force-shown
/// (even inside collapsed workspaces) and a workspace header is hidden when none
/// of its tabs match. An empty query restores the collapse-correct visibility.
pub fn apply_tab_search_filter(tab_list: &gtk::Box, state: &Rc<RefCell<AppState>>, query: &str) {
    let searching = !query.trim().is_empty();
    let st = state.borrow();
    for ws in &st.workspaces {
        let mut any_match = false;
        for tab in &ws.tabs {
            let matched = tab_matches_query(&tab.name, tab.search_cwd().as_deref(), query);
            if matched {
                any_match = true;
            }
            if let Some(handle) = tab_row_handle(tab_list, tab.id) {
                let visible = if searching { matched } else { !ws.collapsed };
                handle.row.set_visible(visible);
            }
        }
        if let Some(handle) = workspace_row_handle(tab_list, ws.id) {
            handle
                .header
                .set_visible(if searching { any_match } else { true });
        }
    }
}

/// Activate (and return true for) the first tab matching `query` in workspace/tab
/// order, reusing the normal tab-activation path. Used for Enter in the search box.
pub fn activate_first_tab_match(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    query: &str,
) -> bool {
    let target = {
        let st = state.borrow();
        st.workspaces
            .iter()
            .flat_map(|ws| ws.tabs.iter())
            .find(|tab| tab_matches_query(&tab.name, tab.search_cwd().as_deref(), query))
            .map(|tab| tab.id)
    };
    match target {
        Some(tab_id) => activate_tab(tab_list, state, term_stack, tab_id),
        None => false,
    }
}

/// Run transient menu cleanup in the next main-loop turn. GTK pops a
/// `PopoverMenu` down before dispatching the clicked model button's action, so
/// synchronous `closed` cleanup can disconnect that action before activation.
fn defer_context_menu_cleanup(cleanup: impl FnOnce() + 'static) {
    glib::idle_add_local_once(cleanup);
}

/// Let the originating GTK signal finish before a user-requested tab close can
/// unparent the clicked row. Removing a button's ancestor from inside its own
/// `clicked`/action dispatch can leave GTK processing stale hover, drag, and
/// frame state for that row. Socket-driven closes do not need this boundary
/// because they are already dispatched from the main-context command queue.
fn defer_user_tab_close(close: impl FnOnce() + 'static) {
    glib::idle_add_local_once(close);
}

fn show_context_menu(
    parent: &impl IsA<gtk::Widget>,
    model: &gio::Menu,
    group_name: &str,
    group: &gio::SimpleActionGroup,
    x: f64,
    y: f64,
) {
    let popover = gtk::PopoverMenu::from_model(Some(model));
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Bottom);
    // GTK4 resolves popover-menu item actions through the popover's parent
    // widget hierarchy. A group inserted only on the transient popover is not
    // reliably visible from nested submenu pages (the "Move to Workspace"
    // submenu silently no-oped), so install it on the parent widget for the
    // lifetime of the popover as well.
    parent.insert_action_group(group_name, Some(group));
    popover.insert_action_group(group_name, Some(group));
    popover.set_parent(parent);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    let closed_group_name = group_name.to_string();
    popover.connect_closed(move |popover| {
        let popover = popover.clone();
        let closed_group_name = closed_group_name.clone();
        defer_context_menu_cleanup(move || {
            if let Some(parent) = popover.parent() {
                parent.insert_action_group(&closed_group_name, None::<&gio::ActionGroup>);
                popover.unparent();
            }
        });
    });
    popover.popup();
}

fn widget_window(widget: &impl IsA<gtk::Widget>) -> Option<adw::ApplicationWindow> {
    widget
        .root()
        .and_then(|root| root.downcast::<adw::ApplicationWindow>().ok())
}

fn activate_window_action(widget: &impl IsA<gtk::Widget>, action: crate::keybindings::Action) {
    crate::keybindings::activate(widget, action);
}

fn copy_text_to_clipboard(text: &str) {
    if text.trim().is_empty() {
        return;
    }
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(text);
    }
}

pub fn open_new_tab_in_workspace(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    ws_id: u32,
    window: &adw::ApplicationWindow,
) {
    let tab_list = tab_list.clone();
    let state = state.clone();
    let term_stack = term_stack.clone();
    let window = window.clone();
    let state_for_validation = state.clone();
    let window_for_validation = window.clone();
    crate::terminal::validate_workspace_tmux_then(
        &state_for_validation,
        &window_for_validation,
        ws_id,
        move || {
            let (tab_name, cwd) = {
                let st = state.borrow();
                let Some(workspace) = st.workspaces.iter().find(|ws| ws.id == ws_id) else {
                    return;
                };
                (
                    format!("Shell {}", workspace.tabs.len() + 1),
                    workspace
                        .working_tree_path
                        .clone()
                        .or_else(|| workspace.repo_root.clone()),
                )
            };

            activate_workspace(&tab_list, &state, &term_stack, ws_id);
            let runtime = RuntimeHandle::from_shared_state(state.clone());
            let tab_id =
                terminal::create_terminal(&runtime, &term_stack, &tab_name, cwd.as_deref(), None);
            add_tab_row(&tab_list, &state, &term_stack, tab_id, &tab_name, true);
            terminal::wire_tab_terminals(&state, &term_stack, &tab_list, &window, tab_id);
        },
    );
}

#[allow(deprecated)] // VTE 0.78 deprecated current_directory_uri; termprop migration is future scope
fn suggested_tmux_tab_defaults(
    state: &Rc<RefCell<AppState>>,
) -> (String, Option<String>, Option<String>) {
    let (default_name, workspace_dir, host_name) = {
        let st = state.borrow();
        let tab_name = st
            .active_ws()
            .map(|ws| format!("Tmux {}", ws.tabs.len() + 1))
            .unwrap_or_else(|| "Tmux 1".to_string());
        let workspace_dir = st.active_ws().and_then(|ws| {
            ws.working_tree_path
                .clone()
                .or_else(|| ws.repo_root.clone())
        });
        let host_name = st.active_ws().and_then(|ws| ws.host_config_name.clone());
        (tab_name, workspace_dir, host_name)
    };

    let cwd = state
        .borrow()
        .active_tab()
        .and_then(|tab| {
            tab.panes
                .leaf(tab.focused_pane_id)
                .or_else(|| tab.panes.leaves().into_iter().next())
                .and_then(|leaf| leaf.local_cwd())
        })
        .or(workspace_dir);

    (default_name, cwd, host_name)
}

pub fn show_create_tmux_tab_dialog(
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
) {
    const LOCAL_TARGET_ID: &str = "__taarof_local_tmux__";

    // `~/.ssh/config` aliases merged with `[hosts.*]` tuning, through the shared
    // resolution seam.
    let candidates = crate::ssh_config::candidates();
    let tmux_ui = crate::terminal::explicit_tmux_tab_ui_state();
    if let Some(message) = tmux_ui.unavailable_message.as_deref() {
        crate::show_error_toast(message);
        return;
    }
    let (default_name, default_dir, default_host) = suggested_tmux_tab_defaults(state);

    let dialog = gtk::Dialog::builder()
        .transient_for(window)
        .modal(true)
        .title("New tmux-backed tab")
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Create", gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Accept);

    let content = dialog.content_area();
    content.set_spacing(8);

    let name_label = gtk::Label::new(Some("Tab name"));
    name_label.set_halign(gtk::Align::Start);
    let name_entry = gtk::Entry::new();
    name_entry.set_text(&default_name);
    name_entry.set_placeholder_text(Some("Tmux tab"));
    content.append(&name_label);
    content.append(&name_entry);

    let host_label = gtk::Label::new(Some("Target"));
    host_label.set_halign(gtk::Align::Start);
    let host_combo = gtk::ComboBoxText::new();
    if tmux_ui.local_target_available {
        host_combo.append(Some(LOCAL_TARGET_ID), "Local tmux");
    }
    for candidate in &candidates {
        let label = match (candidate.source, candidate.config.ssh_target.as_ref()) {
            (crate::ssh_config::HostSource::SshConfig, _) => {
                format!("{} (ssh config)", candidate.name)
            }
            (crate::ssh_config::HostSource::ConfigToml, Some(ssh)) => {
                format!("{} ({ssh})", candidate.name)
            }
            (crate::ssh_config::HostSource::ConfigToml, None) => candidate.name.clone(),
        };
        host_combo.append(Some(&candidate.name), &label);
    }
    if let Some(host_name) = default_host
        .as_deref()
        .filter(|name| candidates.iter().any(|candidate| candidate.name == *name))
    {
        host_combo.set_active_id(Some(host_name));
    } else if !tmux_ui.local_target_available {
        if let Some(first_remote_host) = candidates.first() {
            host_combo.set_active_id(Some(&first_remote_host.name));
        }
    } else {
        host_combo.set_active_id(Some(LOCAL_TARGET_ID));
    }
    content.append(&host_label);
    content.append(&host_combo);

    let ssh_label = gtk::Label::new(Some("Or connect to"));
    ssh_label.set_halign(gtk::Align::Start);
    let ssh_entry = gtk::Entry::new();
    ssh_entry.set_placeholder_text(Some("user@host — overrides the target above"));
    content.append(&ssh_label);
    content.append(&ssh_entry);

    let cwd_label = gtk::Label::new(Some("Working directory"));
    cwd_label.set_halign(gtk::Align::Start);
    let cwd_entry = gtk::Entry::new();
    cwd_entry.set_placeholder_text(Some("Leave blank for the default shell directory"));
    if let Some(dir) = default_dir.as_deref() {
        cwd_entry.set_text(dir);
    }
    content.append(&cwd_label);
    content.append(&cwd_entry);

    if !tmux_ui.local_target_available {
        let note = gtk::Label::new(Some(
            "Local tmux is unavailable on this machine. Choose a configured remote host to create a tmux-backed tab.",
        ));
        note.set_wrap(true);
        note.set_halign(gtk::Align::Start);
        note.add_css_class("tab-cwd");
        content.append(&note);
    }

    {
        let dialog = dialog.clone();
        name_entry.connect_activate(move |_| {
            dialog.response(gtk::ResponseType::Accept);
        });
    }

    {
        let dialog = dialog.clone();
        ssh_entry.connect_activate(move |_| {
            dialog.response(gtk::ResponseType::Accept);
        });
    }

    {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        let window = window.clone();
        let name_entry = name_entry.clone();
        let cwd_entry = cwd_entry.clone();
        let host_combo = host_combo.clone();
        let ssh_entry = ssh_entry.clone();
        let candidates = candidates.clone();
        let fallback_name = default_name.clone();
        let local_target_available = tmux_ui.local_target_available;
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                let name = match name_entry.text().trim() {
                    "" => fallback_name.clone(),
                    value => value.to_string(),
                };
                let working_dir = match cwd_entry.text().trim() {
                    "" => None,
                    value => Some(value.to_string()),
                };
                // A typed target wins over the dropdown. It resolves through the
                // shared seam first (so a known alias keeps its `[hosts.*]`
                // tuning) and falls back to a validated free-text `user@host`.
                let typed_target = ssh_entry.text().trim().to_string();
                let host_config = if typed_target.is_empty() {
                    let selected_host = host_combo.active_id().map(|id| id.to_string());
                    let resolved = match selected_host.as_deref() {
                        None | Some(LOCAL_TARGET_ID) => None,
                        Some(host_name) => candidates
                            .iter()
                            .find(|candidate| candidate.name == host_name)
                            .map(|candidate| candidate.config.clone()),
                    };

                    if selected_host.is_none() && !local_target_available {
                        crate::show_error_toast("No tmux target is available for this new tab");
                        return;
                    }

                    if selected_host.is_some()
                        && selected_host.as_deref() != Some(LOCAL_TARGET_ID)
                        && resolved.is_none()
                    {
                        crate::show_error_toast("Could not resolve the selected tmux host");
                        return;
                    }

                    resolved
                } else {
                    match crate::ssh_config::resolve(&typed_target)
                        .or_else(|_| crate::ssh_config::resolve_free_text(&typed_target))
                    {
                        Ok(config) => Some(config),
                        Err(error) => {
                            crate::show_error_toast(&format!(
                                "Could not use SSH target \"{typed_target}\": {error}"
                            ));
                            return;
                        }
                    }
                };

                dialog.set_sensitive(false);
                let dialog = dialog.clone();
                let state = state.clone();
                let term_stack = term_stack.clone();
                let tab_list = tab_list.clone();
                let window = window.clone();
                let host_config = host_config.clone();
                let state_for_validation = state.clone();
                let window_for_validation = window.clone();
                crate::terminal::validate_explicit_tmux_target_async(
                    &state_for_validation,
                    &window_for_validation,
                    host_config.clone(),
                    move |result| {
                        dialog.set_sensitive(true);
                        match result {
                            Ok(()) => {
                                let runtime = RuntimeHandle::from_shared_state(state.clone());
                                crate::terminal::open_explicit_tmux_tab_after_validation(
                                    &runtime,
                                    &term_stack,
                                    &tab_list,
                                    &window,
                                    &name,
                                    working_dir.as_deref(),
                                    host_config.as_ref(),
                                );
                                dialog.close();
                                // Warn if this new tmux-backed pane is likely to
                                // nest (taarof itself launched inside tmux, or
                                // the target is remote where the shell may
                                // already be in tmux). See docs/tmux-integration.
                                let over_ssh = host_config
                                    .as_ref()
                                    .and_then(|h| h.ssh_target.as_ref())
                                    .is_some();
                                if crate::config::tmux_nesting_warning(
                                    crate::config::app_launched_inside_tmux(),
                                    over_ssh,
                                ) {
                                    crate::show_toast(
                                        "Heads up: this tmux tab may nest inside an existing tmux (taarof or the remote shell is already in tmux) — prefix keys twice or detach the outer session",
                                    );
                                }
                            }
                            Err(err) => crate::show_error_toast(&err),
                        }
                    },
                );
            } else {
                dialog.close();
            }
        });
    }

    dialog.present();
    name_entry.grab_focus();
    name_entry.select_region(0, -1);
}

pub fn set_workspace_tmux_backed(
    state: &Rc<RefCell<AppState>>,
    ws_id: u32,
    enabled: bool,
) -> Option<bool> {
    let updated = {
        let mut st = state.borrow_mut();
        let ws = st.workspaces.iter_mut().find(|ws| ws.id == ws_id)?;
        ws.tmux_backed = enabled;
        Some(ws.tmux_backed)
    };

    if enabled && !crate::config::tmux_config().enabled {
        crate::show_error_toast(
            "Workspace tmux mode is on, but automatic tmux inheritance is disabled in config.toml ([tmux].enabled = false)",
        );
    }

    updated
}

pub fn toggle_workspace_tmux_backed(state: &Rc<RefCell<AppState>>, ws_id: u32) -> Option<bool> {
    let next = {
        let st = state.borrow();
        let ws = st.workspaces.iter().find(|ws| ws.id == ws_id)?;
        !ws.tmux_backed
    };
    let result = set_workspace_tmux_backed(state, ws_id, next);
    // Explain the inheritance consequence at the moment the user flips the mode
    // (interactive path only — the socket/restore callers use
    // `set_workspace_tmux_backed` directly and stay quiet).
    if let Some(true) = result {
        crate::show_toast(
            "tmux-backed workspace: new tabs and splits here run inside tmux sessions that survive detach",
        );
    } else if let Some(false) = result {
        crate::show_toast("tmux backing off: new tabs and splits here run as plain shells");
    }
    result
}

fn prompt_delete_workspace(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    ws_id: u32,
) {
    let Some(window) = widget_window(tab_list) else {
        return;
    };

    let workspace_meta = {
        let st = state.borrow();
        st.workspaces.iter().find(|ws| ws.id == ws_id).map(|ws| {
            (
                ws.name.clone(),
                ws.tabs.len(),
                ws.is_worktree,
                ws.working_tree_path.clone(),
                ws.repo_root.clone(),
            )
        })
    };
    let Some((workspace_name, tab_count, is_worktree, working_tree_path, repo_root)) =
        workspace_meta
    else {
        return;
    };

    if is_worktree {
        if let Some(path) = working_tree_path {
            prompt_close_worktree_workspace(
                tab_list, state, term_stack, ws_id, &path, repo_root, None,
            );
            return;
        }
    }

    if tab_count == 0 {
        let _ = close_workspace_by_id(tab_list, state, term_stack, ws_id);
        return;
    }

    let dialog = gtk::Dialog::builder()
        .transient_for(&window)
        .modal(true)
        .title("Delete Workspace")
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Delete Workspace", gtk::ResponseType::Accept);

    let content = dialog.content_area();
    content.set_spacing(8);

    let label = gtk::Label::new(Some(&format!(
        "Delete workspace \"{workspace_name}\" and close {tab_count} tab(s)?"
    )));
    label.set_wrap(true);
    label.set_halign(gtk::Align::Start);
    content.append(&label);

    {
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                let _ = close_workspace_by_id(&tab_list, &state, &term_stack, ws_id);
            }
            dialog.close();
        });
    }

    dialog.present();
}

#[allow(clippy::too_many_arguments)] // Menu builders keep their GTK dependencies explicit for local event wiring.
fn show_workspace_header_menu(
    header: &gtk::Box,
    name_label: &gtk::Label,
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    ws_id: u32,
    x: f64,
    y: f64,
) {
    let Some(window) = widget_window(header) else {
        return;
    };

    let (collapsed, can_delete, tmux_backed) = {
        let st = state.borrow();
        let Some(workspace) = st.workspaces.iter().find(|ws| ws.id == ws_id) else {
            return;
        };
        (
            workspace.collapsed,
            st.workspaces.len() > 1,
            workspace.tmux_backed,
        )
    };

    let menu = gio::Menu::new();

    let primary = gio::Menu::new();
    primary.append(Some("Rename Workspace"), Some("workspace.rename"));
    primary.append(Some("New Tab Here"), Some("workspace.new-tab"));
    primary.append(Some("New tmux Tab Here"), Some("workspace.new-tmux-tab"));
    primary.append(
        Some(if tmux_backed {
            "Disable tmux-backed Workspace"
        } else {
            "Enable tmux-backed Workspace"
        }),
        Some("workspace.toggle-tmux"),
    );
    primary.append(
        Some(if collapsed {
            "Expand Workspace"
        } else {
            "Collapse Workspace"
        }),
        Some("workspace.toggle-collapse"),
    );
    menu.append_section(None, &primary);

    if can_delete {
        let destructive = gio::Menu::new();
        destructive.append(Some("Delete Workspace"), Some("workspace.delete"));
        menu.append_section(None, &destructive);
    }

    let group = gio::SimpleActionGroup::new();

    let rename_action = gio::SimpleAction::new("rename", None);
    {
        let header = header.clone();
        let name_label = name_label.clone();
        let state = state.clone();
        rename_action.connect_activate(move |_, _| {
            start_workspace_rename(&header, &name_label, &state, ws_id);
        });
    }
    group.add_action(&rename_action);

    let new_tab_action = gio::SimpleAction::new("new-tab", None);
    {
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        new_tab_action.connect_activate(move |_, _| {
            open_new_tab_in_workspace(&tab_list, &state, &term_stack, ws_id, &window);
        });
    }
    group.add_action(&new_tab_action);

    let new_tmux_tab_action = gio::SimpleAction::new("new-tmux-tab", None);
    {
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        new_tmux_tab_action.connect_activate(move |_, _| {
            activate_workspace(&tab_list, &state, &term_stack, ws_id);
            show_create_tmux_tab_dialog(&state, &term_stack, &tab_list, &window);
        });
    }
    group.add_action(&new_tmux_tab_action);

    let toggle_tmux_action = gio::SimpleAction::new("toggle-tmux", None);
    {
        let state = state.clone();
        toggle_tmux_action.connect_activate(move |_, _| {
            let _ = toggle_workspace_tmux_backed(&state, ws_id);
        });
    }
    group.add_action(&toggle_tmux_action);

    let toggle_action = gio::SimpleAction::new("toggle-collapse", None);
    {
        let tab_list = tab_list.clone();
        let state = state.clone();
        toggle_action.connect_activate(move |_, _| {
            toggle_workspace_collapse(&tab_list, &state, ws_id);
        });
    }
    group.add_action(&toggle_action);

    if can_delete {
        let delete_action = gio::SimpleAction::new("delete", None);
        {
            let tab_list = tab_list.clone();
            let state = state.clone();
            let term_stack = term_stack.clone();
            delete_action.connect_activate(move |_, _| {
                prompt_delete_workspace(&tab_list, &state, &term_stack, ws_id);
            });
        }
        group.add_action(&delete_action);
    }

    show_context_menu(header, &menu, "workspace", &group, x, y);
}

/// Action-group prefix used by the tab-row context menu.
const TAB_MENU_ACTION_GROUP: &str = "tab";

/// One "Move to <workspace>" entry of the tab context menu. The menu item's
/// detailed action name and the registered `SimpleAction` name are derived
/// from the single `action_name` format site so they can never drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TabMoveMenuEntry {
    label: String,
    /// Name registered on the tab action group, e.g. `move-to-3`.
    action_name: String,
    /// Detailed name referenced by the menu item, e.g. `tab.move-to-3`.
    detailed_action_name: String,
    /// The right-clicked tab the action must move.
    tab_id: u32,
    target_ws_id: u32,
}

/// Build the enabled move targets for `tab_id`: every workspace except the
/// one currently containing the tab.
fn tab_move_menu_entries(
    tab_id: u32,
    current_ws_id: u32,
    workspaces: &[(u32, String)],
) -> Vec<TabMoveMenuEntry> {
    workspaces
        .iter()
        .filter(|(ws_id, _)| *ws_id != current_ws_id)
        .map(|(ws_id, name)| {
            let action_name = format!("move-to-{ws_id}");
            TabMoveMenuEntry {
                label: format!("Move to {name}"),
                detailed_action_name: format!("{TAB_MENU_ACTION_GROUP}.{action_name}"),
                action_name,
                tab_id,
                target_ws_id: *ws_id,
            }
        })
        .collect()
}

/// Resolve an activated move action back to the `(tab_id, target_ws_id)` pair
/// it was built for. Unknown action names resolve to `None` (inert).
fn resolve_tab_move_dispatch(
    entries: &[TabMoveMenuEntry],
    action_name: &str,
) -> Option<(u32, u32)> {
    entries
        .iter()
        .find(|entry| entry.action_name == action_name)
        .map(|entry| (entry.tab_id, entry.target_ws_id))
}

#[allow(clippy::too_many_arguments)] // Menu builders keep their GTK dependencies explicit for local event wiring.
fn show_tab_row_menu(
    row: &gtk::Box,
    hbox: &gtk::Box,
    name_label: &gtk::Label,
    cwd_label: &gtk::Label,
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_id: u32,
    x: f64,
    y: f64,
) {
    let Some(window) = widget_window(row) else {
        return;
    };

    let (workspace_id, workspaces, agent_resume) = {
        let st = state.borrow();
        let tab = st.find_tab(tab_id).map(|(ws, tab)| (ws.id, tab));
        let workspace_id = tab.as_ref().map(|(workspace_id, _)| *workspace_id);
        let agent_resume = tab
            .as_ref()
            .and_then(|(_, tab)| {
                tab.panes
                    .leaf(tab.focused_pane_id)
                    .and_then(|leaf| {
                        (!leaf.process_state.has_child_process)
                            .then(|| leaf.agent_resume.clone())
                            .flatten()
                    })
                    .or_else(|| {
                        st.pending_tab_restores.get(&tab.id).and_then(|pending| {
                            crate::terminal::plan_restored_spawns(&pending.saved, false)
                                .into_iter()
                                .find(|pane| pane.pane_id == tab.focused_pane_id)
                                .and_then(|pane| pane.agent_resume)
                        })
                    })
            })
            .filter(|_| !crate::config::should_auto_resume_agents_on_session_restore());
        let workspaces = st
            .workspaces
            .iter()
            .map(|ws| (ws.id, ws.name.clone()))
            .collect::<Vec<_>>();
        (workspace_id, workspaces, agent_resume)
    };

    let Some(workspace_id) = workspace_id else {
        return;
    };

    let move_entries = Rc::new(tab_move_menu_entries(tab_id, workspace_id, &workspaces));

    let menu = gio::Menu::new();

    let primary = gio::Menu::new();
    primary.append(Some("Rename Tab"), Some("tab.rename"));
    primary.append(Some("Close Tab"), Some("tab.close"));
    primary.append(Some("Duplicate Tab"), Some("tab.duplicate"));
    menu.append_section(None, &primary);

    if !move_entries.is_empty() {
        let move_menu = gio::Menu::new();
        for entry in move_entries.iter() {
            move_menu.append(Some(&entry.label), Some(&entry.detailed_action_name));
        }
        menu.append_submenu(Some("Move to Workspace"), &move_menu);
    }

    let layout = gio::Menu::new();
    layout.append(Some("Split Vertical"), Some("tab.split-vertical"));
    layout.append(Some("Split Horizontal"), Some("tab.split-horizontal"));
    layout.append(Some("Send to Pane…"), Some("tab.send-to-pane"));
    layout.append(
        Some("Send Last Output to Pane…"),
        Some("tab.send-last-output-to-pane"),
    );
    layout.append(
        Some("Copy Last Agent Message"),
        Some("tab.copy-last-message"),
    );
    layout.append(
        Some("Relay Last Message to Pane…"),
        Some("tab.relay-last-message"),
    );
    layout.append(Some("Recent Files…"), Some("tab.recent-files"));
    layout.append(Some("Copy CWD"), Some("tab.copy-cwd"));
    layout.append(Some("Discover Tasks"), Some("tab.discover"));
    if let Some(offer) = agent_resume.as_ref() {
        layout.append(
            Some(&format!("Resume {} session", offer.agent_name)),
            Some("tab.resume-agent"),
        );
    }
    menu.append_section(None, &layout);

    let group = gio::SimpleActionGroup::new();

    let rename_action = gio::SimpleAction::new("rename", None);
    {
        let hbox = hbox.clone();
        let name_label = name_label.clone();
        let state = state.clone();
        rename_action.connect_activate(move |_, _| {
            start_rename(&hbox, &name_label, &state, tab_id);
        });
    }
    group.add_action(&rename_action);

    let close_action = gio::SimpleAction::new("close", None);
    {
        let row = row.downgrade();
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        close_action.connect_activate(move |_, _| {
            let Some(row) = row.upgrade() else {
                return;
            };
            let tab_list = tab_list.clone();
            let state = state.clone();
            let term_stack = term_stack.clone();
            defer_user_tab_close(move || {
                confirm_then_close_tab(&tab_list, &state, &term_stack, tab_id, &row);
            });
        });
    }
    group.add_action(&close_action);

    let duplicate_action = gio::SimpleAction::new("duplicate", None);
    {
        let row = row.clone();
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        duplicate_action.connect_activate(move |_, _| {
            activate_tab_row(&tab_list, &state, &term_stack, tab_id, &row);
            let runtime = RuntimeHandle::from_shared_state(state.clone());
            if terminal::duplicate_tab(&runtime, &term_stack, &tab_list, &window, tab_id).is_some()
            {
                let state_for_focus = state.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                    if let Some(terminal) = crate::get_active_terminal(&state_for_focus) {
                        terminal.grab_focus();
                    }
                });
            }
        });
    }
    group.add_action(&duplicate_action);

    let split_vertical_action = gio::SimpleAction::new("split-vertical", None);
    {
        let row = row.clone();
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        split_vertical_action.connect_activate(move |_, _| {
            activate_tab_row(&tab_list, &state, &term_stack, tab_id, &row);
            activate_window_action(&row, crate::keybindings::Action::SplitVertical);
        });
    }
    group.add_action(&split_vertical_action);

    let split_horizontal_action = gio::SimpleAction::new("split-horizontal", None);
    {
        let row = row.clone();
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        split_horizontal_action.connect_activate(move |_, _| {
            activate_tab_row(&tab_list, &state, &term_stack, tab_id, &row);
            activate_window_action(&row, crate::keybindings::Action::SplitHorizontal);
        });
    }
    group.add_action(&split_horizontal_action);

    // Focus the right-clicked tab's pane first, then open the send-to-pane
    // picker seeded from that pane's selection/clipboard (or recent output).
    let send_to_pane_action = gio::SimpleAction::new("send-to-pane", None);
    {
        let row = row.clone();
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        send_to_pane_action.connect_activate(move |_, _| {
            activate_tab_row(&tab_list, &state, &term_stack, tab_id, &row);
            activate_window_action(&row, crate::keybindings::Action::SendToPane);
        });
    }
    group.add_action(&send_to_pane_action);

    let send_last_output_to_pane_action = gio::SimpleAction::new("send-last-output-to-pane", None);
    {
        let row = row.clone();
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        send_last_output_to_pane_action.connect_activate(move |_, _| {
            activate_tab_row(&tab_list, &state, &term_stack, tab_id, &row);
            activate_window_action(&row, crate::keybindings::Action::SendLastOutputToPane);
        });
    }
    group.add_action(&send_last_output_to_pane_action);

    let copy_last_message_action = gio::SimpleAction::new("copy-last-message", None);
    {
        let row = row.clone();
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        copy_last_message_action.connect_activate(move |_, _| {
            activate_tab_row(&tab_list, &state, &term_stack, tab_id, &row);
            activate_window_action(&row, crate::keybindings::Action::CopyLastMessage);
        });
    }
    group.add_action(&copy_last_message_action);

    let relay_last_message_action = gio::SimpleAction::new("relay-last-message", None);
    {
        let row = row.clone();
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        relay_last_message_action.connect_activate(move |_, _| {
            activate_tab_row(&tab_list, &state, &term_stack, tab_id, &row);
            activate_window_action(&row, crate::keybindings::Action::RelayLastMessage);
        });
    }
    group.add_action(&relay_last_message_action);

    // Focus the right-clicked tab first, then open the recent-files picker so it
    // targets that tab's agents (EXAMPLE-92).
    let recent_files_action = gio::SimpleAction::new("recent-files", None);
    {
        let row = row.clone();
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        recent_files_action.connect_activate(move |_, _| {
            activate_tab_row(&tab_list, &state, &term_stack, tab_id, &row);
            activate_window_action(&row, crate::keybindings::Action::RecentFiles);
        });
    }
    group.add_action(&recent_files_action);

    let copy_cwd_action = gio::SimpleAction::new("copy-cwd", None);
    {
        let cwd_label = cwd_label.clone();
        copy_cwd_action.connect_activate(move |_, _| {
            copy_text_to_clipboard(cwd_label.text().as_ref());
        });
    }
    group.add_action(&copy_cwd_action);

    let discover_action = gio::SimpleAction::new("discover", None);
    {
        let state = state.clone();
        let tab_list = tab_list.clone();
        discover_action.connect_activate(move |_, _| {
            crate::task_panel::discover_tasks_explicitly(&state, &tab_list, tab_id);
        });
    }
    group.add_action(&discover_action);

    if let Some(offer) = agent_resume {
        let resume_action = gio::SimpleAction::new("resume-agent", None);
        let row = row.clone();
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        resume_action.connect_activate(move |_, _| {
            activate_tab_row(&tab_list, &state, &term_stack, tab_id, &row);
            let target = {
                let mut st = state.borrow_mut();
                let Some(tab) = st.find_tab_mut(tab_id) else {
                    return;
                };
                let pane_id = tab.focused_pane_id;
                tab.panes.leaf_mut(pane_id).and_then(|leaf| {
                    if leaf.process_state.has_child_process {
                        return None;
                    }
                    let command = leaf
                        .agent_resume
                        .take()
                        .map(|resume| resume.shell_input())
                        .unwrap_or_else(|| offer.shell_input());
                    Some((leaf.terminal.clone(), command))
                })
            };
            if let Some((terminal, command)) = target {
                terminal.feed_child(format!("{command}\n").as_bytes());
            } else {
                crate::show_error_toast("The restored agent pane is no longer available");
            }
        });
        group.add_action(&resume_action);
    }

    for entry in move_entries.iter() {
        let action = gio::SimpleAction::new(&entry.action_name, None);
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        let move_entries = move_entries.clone();
        action.connect_activate(move |action, _| {
            // Resolve through the same entries that built the menu items, so
            // the dispatched pair always targets the right-clicked tab.
            let Some((move_tab_id, target_ws_id)) =
                resolve_tab_move_dispatch(&move_entries, action.name().as_str())
            else {
                return;
            };
            if let Err(err) =
                move_tab_to_workspace(&tab_list, &state, &term_stack, move_tab_id, target_ws_id)
            {
                crate::show_error_toast(&format!("Could not move tab to workspace: {err:?}"));
            }
        });
        group.add_action(&action);
    }

    show_context_menu(row, &menu, TAB_MENU_ACTION_GROUP, &group, x, y);
}

/// Add a workspace header row to the sidebar tab list.
/// Single-click switches workspace, chevron click toggles collapse/expand,
/// double-click starts inline rename.
pub fn add_workspace_header(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    ws_id: u32,
    name: &str,
) -> gtk::Box {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    header.add_css_class("workspace-header");
    header.set_widget_name(&workspace_header_widget_name(ws_id));

    let chevron = gtk::Label::new(Some("\u{25be}"));
    chevron.add_css_class("ws-chevron");
    chevron.add_css_class("compact-hidden");
    chevron.set_widget_name(&workspace_chevron_widget_name(ws_id));

    let icon_label = gtk::Label::new(Some(&workspace_icon_text(state, ws_id)));
    icon_label.add_css_class("ws-icon");
    icon_label.set_widget_name(&workspace_icon_widget_name(ws_id));

    let name_label = gtk::Label::new(Some(name));
    name_label.set_halign(gtk::Align::Start);
    name_label.set_hexpand(true);
    name_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name_label.set_single_line_mode(true);
    name_label.set_max_width_chars(WORKSPACE_HEADER_NAME_MAX_WIDTH_CHARS);
    name_label.set_tooltip_text(Some(name));
    name_label.add_css_class("ws-name");
    name_label.add_css_class("compact-hidden");
    name_label.set_widget_name(&workspace_name_widget_name(ws_id));

    let header_meta = workspace_header_meta_snapshot(state, ws_id);

    let tool_version_chip = gtk::Label::new(header_meta.tool_version_chip_text.as_deref());
    tool_version_chip.add_css_class("ws-tool-version-chip");
    tool_version_chip.add_css_class("compact-hidden");
    tool_version_chip.set_widget_name(&workspace_tool_version_widget_name(ws_id));
    tool_version_chip.set_ellipsize(gtk::pango::EllipsizeMode::End);
    tool_version_chip.set_single_line_mode(true);
    tool_version_chip.set_max_width_chars(26);
    tool_version_chip.set_visible(header_meta.tool_version_chip_text.is_some());

    let worktree_badge = gtk::Label::new(Some("WT"));
    worktree_badge.add_css_class("ws-worktree-badge");
    worktree_badge.add_css_class("compact-hidden");
    worktree_badge.set_widget_name(&workspace_worktree_widget_name(ws_id));
    worktree_badge.set_visible(header_meta.show_worktree_badge);

    let tab_count_badge = gtk::Label::new(Some(&header_meta.tab_count_text));
    tab_count_badge.add_css_class("ws-tab-count");
    tab_count_badge.add_css_class("compact-hidden");
    tab_count_badge.set_widget_name(&workspace_tab_count_widget_name(ws_id));

    // Status indicator dot (hidden by default, shown when Running/Errored)
    let status_dot = gtk::Label::new(None);
    status_dot.add_css_class("ws-status-dot");
    status_dot.set_widget_name(&workspace_status_widget_name(ws_id));
    status_dot.set_visible(false);

    header.append(&chevron);
    header.append(&icon_label);
    header.append(&name_label);
    header.append(&tool_version_chip);
    header.append(&tab_count_badge);
    header.append(&worktree_badge);
    header.append(&status_dot);

    // Chevron click → toggle collapse/expand
    {
        let tab_list = tab_list.clone();
        let state = state.clone();
        let chevron_gesture = gtk::GestureClick::new();
        chevron_gesture.set_button(1);
        chevron_gesture.connect_released(move |gesture, _n_press, _x, _y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            toggle_workspace_collapse(&tab_list, &state, ws_id);
        });
        chevron.add_controller(chevron_gesture);
    }

    // Header click: single-click → switch workspace, double-click → rename
    {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        let name_label = name_label.clone();
        let header_for_rename = header.clone();
        let rename_active = Rc::new(Cell::new(false));
        let gesture = gtk::GestureClick::new();
        gesture.set_button(1);
        let rename_active_c = rename_active.clone();
        gesture.connect_released(move |gesture, n_press, _x, _y| {
            if n_press == 2 {
                // Double-click: rename workspace
                gesture.set_state(gtk::EventSequenceState::Claimed);
                rename_active_c.set(true);
                start_workspace_rename(&header_for_rename, &name_label, &state, ws_id);
            } else if n_press == 1 {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                activate_workspace(&tab_list, &state, &term_stack, ws_id);
                // Delay focus to allow double-click detection
                let state_for_focus = state.clone();
                let rename_active = rename_active_c.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                    if !rename_active.get() {
                        if let Some(terminal) = crate::get_active_terminal(&state_for_focus) {
                            terminal.grab_focus();
                        }
                    }
                    rename_active.set(false);
                });
            }
        });
        header.add_controller(gesture);
    }

    // Right click → workspace context menu
    {
        let menu_header = header.clone();
        let name_label = name_label.clone();
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        let gesture = gtk::GestureClick::new();
        gesture.set_button(3);
        gesture.connect_released(move |gesture, _n_press, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            show_workspace_header_menu(
                &menu_header,
                &name_label,
                &tab_list,
                &state,
                &term_stack,
                ws_id,
                x,
                y,
            );
        });
        header.add_controller(gesture);
    }

    // Drop target: drag a tab onto this header to move it into this workspace
    setup_workspace_header_drop_target(&header, tab_list, state, term_stack, ws_id);

    set_compact_hidden_visibility(header.upcast_ref(), compact_mode_active());
    tab_list.append(&header);
    register_workspace_row(
        tab_list,
        ws_id,
        WorkspaceRowHandle {
            header: header.clone(),
            chevron,
            icon_label,
            tool_version_chip,
            worktree_badge,
            tab_count_badge,
            status_dot,
        },
    );
    header
}

pub fn activate_workspace(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    ws_id: u32,
) -> bool {
    let activated = state.borrow_mut().activate_workspace(ws_id).is_some();
    if activated {
        sync_active_selection_ui(tab_list, state, term_stack);
    }
    activated
}

/// Rename a workspace and refresh its sidebar header label to match.
///
/// Shares `AppState::rename_workspace` with the inline GUI rename so the
/// mutation, event emission, and validation stay identical; this variant also
/// updates the header label/tooltip/icon for callers that are not driven by the
/// inline entry (e.g. the `rename-workspace` socket action).
pub fn rename_workspace(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    ws_id: u32,
    new_name: &str,
) -> Result<String, String> {
    let applied = state.borrow_mut().rename_workspace(ws_id, new_name)?;
    if let Some(handle) = workspace_row_handle(tab_list, ws_id) {
        if let Some(label) = find_widget_by_name(
            &handle.header.clone().upcast(),
            &workspace_name_widget_name(ws_id),
        )
        .and_then(|w| w.downcast::<gtk::Label>().ok())
        {
            label.set_text(&applied);
            label.set_tooltip_text(Some(&applied));
        }
        refresh_workspace_header_icon(&handle.header, state, ws_id);
    }
    Ok(applied)
}

/// Refresh only the rendered workspace header. Async metadata callers use this
/// after validating workspace identity, without changing the workspace name.
pub(crate) fn set_workspace_header_label(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    ws_id: u32,
    label_text: &str,
) {
    if let Some(handle) = workspace_row_handle(tab_list, ws_id) {
        if let Some(label) = find_widget_by_name(
            &handle.header.clone().upcast(),
            &workspace_name_widget_name(ws_id),
        )
        .and_then(|widget| widget.downcast::<gtk::Label>().ok())
        {
            label.set_text(label_text);
            label.set_tooltip_text(Some(label_text));
        }
        refresh_workspace_header_icon(&handle.header, state, ws_id);
    }
}

pub fn move_tab_to_workspace(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_id: u32,
    target_ws_id: u32,
) -> Result<(), crate::MoveTabError> {
    let source_ws_id = {
        let st = state.borrow();
        st.find_tab(tab_id).map(|(ws, _)| ws.id)
    };

    state
        .borrow_mut()
        .move_tab_to_workspace(tab_id, target_ws_id)?;
    move_tab_row_to_workspace(tab_list, state, tab_id, target_ws_id);
    set_workspace_collapsed_ui(tab_list, state, target_ws_id, false);

    if let Some(source_ws_id) = source_ws_id {
        refresh_workspace_header_meta(tab_list, state, source_ws_id);
    }
    refresh_workspace_header_meta(tab_list, state, target_ws_id);
    sync_active_selection_ui(tab_list, state, term_stack);
    Ok(())
}

fn workspace_insert_anchor(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    target_ws_id: u32,
    moving_tab_id: u32,
) -> Option<gtk::Widget> {
    let tab_ids: Vec<u32> = {
        let st = state.borrow();
        st.workspaces
            .iter()
            .find(|ws| ws.id == target_ws_id)
            .map(|ws| ws.tabs.iter().map(|tab| tab.id).collect())
    }?;

    let last_other_tab = tab_ids
        .iter()
        .rev()
        .find(|tab_id| **tab_id != moving_tab_id)
        .copied();

    if let Some(tab_id) = last_other_tab {
        tab_row_handle(tab_list, tab_id).map(|handle| handle.row.upcast())
    } else {
        workspace_row_handle(tab_list, target_ws_id).map(|handle| handle.header.upcast())
    }
}

fn place_tab_row_in_workspace(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    row: &gtk::Box,
    tab_id: u32,
    target_ws_id: u32,
) {
    if row.parent().is_some() {
        tab_list.remove(row);
    }

    let collapsed = {
        let st = state.borrow();
        st.workspaces
            .iter()
            .find(|ws| ws.id == target_ws_id)
            .map(|ws| ws.collapsed)
            .unwrap_or(false)
    };

    row.set_visible(!collapsed);
    if let Some(anchor) = workspace_insert_anchor(tab_list, state, target_ws_id, tab_id) {
        tab_list.insert_child_after(row, Some(&anchor));
    } else {
        tab_list.append(row);
    }
}

pub fn move_tab_row_to_workspace(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    target_ws_id: u32,
) {
    if let Some(handle) = tab_row_handle(tab_list, tab_id) {
        place_tab_row_in_workspace(tab_list, state, &handle.row, tab_id, target_ws_id);
    }
}

fn set_workspace_collapsed_ui(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    ws_id: u32,
    collapsed: bool,
) {
    let Some(workspace_handle) = workspace_row_handle(tab_list, ws_id) else {
        return;
    };

    workspace_handle
        .chevron
        .set_text(if collapsed { "\u{25b8}" } else { "\u{25be}" });

    let tab_ids: Vec<u32> = {
        let st = state.borrow();
        st.workspaces
            .iter()
            .find(|ws| ws.id == ws_id)
            .map(|ws| ws.tabs.iter().map(|tab| tab.id).collect())
            .unwrap_or_default()
    };

    for tab_id in tab_ids {
        if let Some(handle) = tab_row_handle(tab_list, tab_id) {
            handle.row.set_visible(!collapsed);
        }
    }
}

/// Toggle collapse/expand for a workspace: hide/show its tab rows and update chevron.
pub fn toggle_workspace_collapse(tab_list: &gtk::Box, state: &Rc<RefCell<AppState>>, ws_id: u32) {
    let collapsed = {
        let mut st = state.borrow_mut();
        let ws = match st.workspaces.iter_mut().find(|w| w.id == ws_id) {
            Some(w) => w,
            None => return,
        };
        ws.collapsed = !ws.collapsed;
        ws.collapsed
    };

    set_workspace_collapsed_ui(tab_list, state, ws_id, collapsed);
}

/// Start inline rename for a workspace header.
fn start_workspace_rename(
    header: &gtk::Box,
    name_label: &gtk::Label,
    state: &Rc<RefCell<AppState>>,
    ws_id: u32,
) {
    // Guard: don't open a second entry if one is already active
    if !gtk::prelude::WidgetExt::is_visible(name_label) {
        return;
    }

    let current_name = name_label.text().to_string();

    let entry = gtk::Entry::new();
    entry.set_text(&current_name);
    entry.add_css_class("tab-rename-entry");
    entry.set_hexpand(true);

    // Hide label, show entry
    name_label.set_visible(false);
    header.append(&entry);
    entry.grab_focus();
    entry.select_region(0, -1);

    // Commit rename on Enter
    {
        let state = state.clone();
        let name_label = name_label.clone();
        let header = header.clone();
        let entry_ref = entry.clone();
        entry.connect_activate(move |entry| {
            let new_name = entry.text().to_string();
            finish_workspace_rename(&header, &name_label, &entry_ref, &state, ws_id, &new_name);
        });
    }

    // Cancel on Escape
    {
        let name_label = name_label.clone();
        let header = header.clone();
        let entry_ref = entry.clone();
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.connect_key_pressed(move |_ctrl, key, _code, _mods| {
            if key == gdk::Key::Escape {
                cancel_rename(&header, &name_label, &entry_ref);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        entry.add_controller(key_ctrl);
    }

    // Commit on focus loss
    {
        let state = state.clone();
        let name_label = name_label.clone();
        let header = header.clone();
        let entry_ref = entry.clone();
        let focus_ctrl = gtk::EventControllerFocus::new();
        focus_ctrl.connect_leave(move |_| {
            if gtk::prelude::WidgetExt::is_visible(&entry_ref) {
                let new_name = entry_ref.text().to_string();
                finish_workspace_rename(&header, &name_label, &entry_ref, &state, ws_id, &new_name);
            }
        });
        entry.add_controller(focus_ctrl);
    }
}

fn finish_workspace_rename(
    header: &gtk::Box,
    name_label: &gtk::Label,
    entry: &gtk::Entry,
    state: &Rc<RefCell<AppState>>,
    ws_id: u32,
    new_name: &str,
) {
    let new_name = new_name.trim();
    if !new_name.is_empty() {
        // Route the mutation through AppState so the sidebar rename and the
        // rename-workspace socket action share one source of truth.
        if state.borrow_mut().rename_workspace(ws_id, new_name).is_ok() {
            name_label.set_text(new_name);
            name_label.set_tooltip_text(Some(new_name));
        }
    }
    refresh_workspace_header_icon(header, state, ws_id);
    name_label.set_visible(true);
    header.remove(entry);
}

/// Update CSS classes on workspace headers: mark the active one.
fn update_workspace_header_styles(tab_list: &gtk::Box, active_ws_id: u32) {
    for (ws_id, handle) in workspace_rows_snapshot(tab_list) {
        handle.header.remove_css_class("active");
        if ws_id == active_ws_id {
            handle.header.add_css_class("active");
        }
    }
}

/// Update workspace status indicator dots in the sidebar.
/// Called from the 3-second poll loop.
pub fn update_workspace_status_dots(tab_list: &gtk::Box, state: &Rc<RefCell<AppState>>) {
    use crate::workspace::WorkspaceStatus;

    let statuses: Vec<(u32, WorkspaceStatus)> = {
        let st = state.borrow();
        st.workspaces
            .iter()
            .map(|ws| (ws.id, ws.run_status))
            .collect()
    };

    for (ws_id, status) in statuses {
        let Some(handle) = workspace_row_handle(tab_list, ws_id) else {
            continue;
        };
        let label = handle.status_dot;
        match status {
            WorkspaceStatus::Idle => {
                label.set_visible(false);
            }
            WorkspaceStatus::Running => {
                label.set_text("\u{25cf}"); // ●
                label.set_visible(true);
                label.remove_css_class("ws-status-error");
                label.add_css_class("ws-status-running");
            }
            WorkspaceStatus::Errored => {
                label.set_text("\u{25cf}"); // ●
                label.set_visible(true);
                label.remove_css_class("ws-status-running");
                label.add_css_class("ws-status-error");
            }
        }
    }
}

/// Labels returned from add_tab_row for CWD and git branch tracking.
#[derive(Clone)]
pub struct TabLabels {
    pub cwd_label: gtk::Label,
    pub branch_label: gtk::Label,
}

fn workspace_header_widget_name(ws_id: u32) -> String {
    format!("ws-header-{ws_id}")
}

fn workspace_chevron_widget_name(ws_id: u32) -> String {
    format!("ws-chevron-{ws_id}")
}

fn workspace_icon_widget_name(ws_id: u32) -> String {
    format!("ws-icon-{ws_id}")
}

fn workspace_name_widget_name(ws_id: u32) -> String {
    format!("ws-name-{ws_id}")
}

fn workspace_worktree_widget_name(ws_id: u32) -> String {
    format!("ws-worktree-{ws_id}")
}

fn workspace_tool_version_widget_name(ws_id: u32) -> String {
    format!("ws-tools-{ws_id}")
}

fn workspace_status_widget_name(ws_id: u32) -> String {
    format!("ws-status-{ws_id}")
}

fn workspace_tab_count_widget_name(ws_id: u32) -> String {
    format!("ws-count-{ws_id}")
}

fn workspace_icon_text(state: &Rc<RefCell<AppState>>, ws_id: u32) -> String {
    let st = state.borrow();
    let Some(workspace) = st.workspaces.iter().find(|ws| ws.id == ws_id) else {
        return "•".to_string();
    };

    crate::config::workspace_icon_for(
        &workspace.name,
        workspace.repo_root.as_deref(),
        workspace.is_worktree,
    )
}

fn refresh_workspace_header_icon(header: &gtk::Box, state: &Rc<RefCell<AppState>>, ws_id: u32) {
    let Some(handle) = workspace_row_handle_for_header(header) else {
        return;
    };
    handle
        .icon_label
        .set_text(&workspace_icon_text(state, ws_id));
}

pub(crate) fn refresh_all_workspace_icons(tab_list: &gtk::Box, state: &Rc<RefCell<AppState>>) {
    for (workspace_id, handle) in workspace_rows_snapshot(tab_list) {
        refresh_workspace_header_icon(&handle.header, state, workspace_id);
    }
}

fn refresh_workspace_header_meta(tab_list: &gtk::Box, state: &Rc<RefCell<AppState>>, ws_id: u32) {
    let Some(handle) = workspace_row_handle(tab_list, ws_id) else {
        return;
    };
    let meta = workspace_header_meta_snapshot(state, ws_id);
    handle.tab_count_badge.set_text(&meta.tab_count_text);
    handle.worktree_badge.set_visible(meta.show_worktree_badge);
    handle
        .tool_version_chip
        .set_text(meta.tool_version_chip_text.as_deref().unwrap_or(""));
    handle
        .tool_version_chip
        .set_visible(meta.tool_version_chip_text.is_some());
    if compact_mode_active() {
        set_compact_hidden_visibility(handle.header.upcast_ref(), true);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct WorkspaceHeaderMeta {
    tab_count_text: String,
    show_worktree_badge: bool,
    tool_version_chip_text: Option<String>,
}

fn workspace_header_meta_snapshot(
    state: &Rc<RefCell<AppState>>,
    ws_id: u32,
) -> WorkspaceHeaderMeta {
    let tool_target = {
        let st = state.borrow();
        crate::mise::discovery_target_for_workspace(&st, ws_id)
    };
    let (tab_count, is_worktree) = {
        let st = state.borrow();
        st.workspaces
            .iter()
            .find(|ws| ws.id == ws_id)
            .map(|ws| (ws.tabs.len(), ws.is_worktree))
            .unwrap_or((0, false))
    };

    WorkspaceHeaderMeta {
        tab_count_text: tab_count.to_string(),
        show_worktree_badge: is_worktree,
        tool_version_chip_text: tool_target
            .as_ref()
            .and_then(crate::mise::tool_version_chip_text_for_target),
    }
}

pub(crate) fn refresh_all_workspace_header_meta(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
) {
    let workspace_ids: Vec<u32> = {
        let st = state.borrow();
        st.workspaces.iter().map(|ws| ws.id).collect()
    };

    for ws_id in workspace_ids {
        refresh_workspace_header_meta(tab_list, state, ws_id);
    }
}

pub fn tab_row_widget_name(tab_id: u32) -> String {
    format!("tab-row-{tab_id}")
}

pub fn tab_name_widget_name(tab_id: u32) -> String {
    format!("tab-name-{tab_id}")
}

fn tab_agent_dot_widget_name(tab_id: u32) -> String {
    format!("agent-dot-{tab_id}")
}

fn tab_branch_widget_name(tab_id: u32) -> String {
    format!("tab-branch-{tab_id}")
}

fn tab_cwd_widget_name(tab_id: u32) -> String {
    format!("tab-cwd-{tab_id}")
}

fn tab_ports_widget_name(tab_id: u32) -> String {
    format!("tab-ports-{tab_id}")
}

fn tab_activity_widget_name(tab_id: u32) -> String {
    format!("tab-activity-{tab_id}")
}

pub fn tab_root_widget_name(tab_id: u32) -> String {
    format!("tab-root-{tab_id}")
}

fn tab_actions_widget_name(tab_id: u32) -> String {
    format!("tab-actions-{tab_id}")
}

fn tab_tracking_widget_name(tab_id: u32) -> String {
    format!("tab-tracking-{tab_id}")
}

fn tab_tmux_widget_name(tab_id: u32) -> String {
    format!("tab-tmux-{tab_id}")
}

fn find_tab_row(tab_list: &gtk::Box, tab_id: u32) -> Option<gtk::Box> {
    tab_row_handle(tab_list, tab_id).map(|handle| handle.row)
}

fn ensure_empty_workspace_placeholder(term_stack: &gtk::Stack) {
    if term_stack
        .child_by_name(EMPTY_WORKSPACE_STACK_NAME)
        .is_some()
    {
        return;
    }

    let empty = gtk::Box::new(gtk::Orientation::Vertical, 6);
    empty.set_hexpand(true);
    empty.set_vexpand(true);
    empty.set_halign(gtk::Align::Center);
    empty.set_valign(gtk::Align::Center);
    empty.add_css_class("empty-workspace");

    let label = gtk::Label::new(Some("after you"));
    label.add_css_class("empty-workspace-title");
    empty.append(&label);

    // Concise next-action hints, each paired with its current keybinding so the
    // guidance never drifts from the user's keybindings.toml. These render only
    // on this placeholder page, which the stack shows only when the active
    // workspace has no tabs — so the hints appear on empty state and vanish the
    // moment a tab exists (driven by sync_active_selection_ui at tab add/remove).
    let hints = gtk::Box::new(gtk::Orientation::Vertical, 2);
    hints.set_halign(gtk::Align::Center);
    hints.add_css_class("empty-workspace-hints");
    for line in crate::keybindings::installed_hint_lines() {
        let hint = gtk::Label::new(Some(&line));
        hint.add_css_class("empty-workspace-hint");
        hint.set_halign(gtk::Align::Center);
        hints.append(&hint);
    }
    empty.append(&hints);

    term_stack.add_named(&empty, Some(EMPTY_WORKSPACE_STACK_NAME));
}

fn sync_active_selection_ui(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
) {
    let (active_ws_id, active_tab_id) = {
        let st = state.borrow();
        let active_tab_id = st.presented_tab_id();
        (st.active_workspace, active_tab_id)
    };

    if let Some(active_tab_id) = active_tab_id {
        // Lazy restore (EXAMPLE-84): if this tab was restored as a hidden
        // placeholder, build its real pane tree and spawn its shells now so the
        // stack child exists before we try to show it. No-op for normal tabs.
        crate::materialize_active_tab_if_pending(state, term_stack, tab_list, active_tab_id);
        let stack_name = tab_root_widget_name(active_tab_id);
        if term_stack.child_by_name(&stack_name).is_some() {
            term_stack.set_visible_child_name(&stack_name);
        } else {
            ensure_empty_workspace_placeholder(term_stack);
            term_stack.set_visible_child_name(EMPTY_WORKSPACE_STACK_NAME);
        }
    } else {
        ensure_empty_workspace_placeholder(term_stack);
        term_stack.set_visible_child_name(EMPTY_WORKSPACE_STACK_NAME);
    }

    update_workspace_header_styles(tab_list, active_ws_id);
    refresh_all_workspace_header_meta(tab_list, state);
    refresh_all_tab_rows(tab_list, state);
}

pub fn activate_tab(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_id: u32,
) -> bool {
    if find_tab_row(tab_list, tab_id).is_none() {
        return false;
    }

    let activated = state.borrow_mut().present_tab(tab_id).is_some();
    if activated {
        sync_active_selection_ui(tab_list, state, term_stack);
    }
    activated
}

fn select_agent_pane_target(state: &mut AppState, tab_id: u32, pane_id: u32) -> bool {
    let Some(tab) = state.find_tab_mut(tab_id) else {
        return false;
    };
    let exists = match tab.panes.as_ref() {
        crate::pane::PaneNode::Stub { pane_id: stub_id } => *stub_id == pane_id,
        _ => tab.panes.leaf(pane_id).is_some(),
    };
    if exists {
        tab.focused_pane_id = pane_id;
    }
    exists
}

fn select_agent_pane(state: &mut AppState, tab_id: u32, pane_id: u32) -> Option<vte::Terminal> {
    if !select_agent_pane_target(state, tab_id, pane_id) {
        return None;
    }
    state
        .find_tab(tab_id)
        .and_then(|(_, tab)| tab.panes.leaf(pane_id))
        .map(|leaf| leaf.terminal.clone())
}

pub(crate) fn focus_agent_pane(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    tab_id: u32,
    pane_id: u32,
) -> bool {
    let terminal = {
        let mut state = state.borrow_mut();
        select_agent_pane(&mut state, tab_id, pane_id)
    };
    let Some(terminal) = terminal else {
        return false;
    };
    if !activate_tab(tab_list, state, term_stack, tab_id) {
        return false;
    }
    terminal.grab_focus();
    true
}

pub fn activate_previous_tab(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
) -> bool {
    let activated = state.borrow_mut().activate_previous_tab().is_some();
    if activated {
        sync_active_selection_ui(tab_list, state, term_stack);
    }
    activated
}

pub fn activate_previous_workspace(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
) -> bool {
    let activated = state.borrow_mut().activate_previous_workspace().is_some();
    if activated {
        sync_active_selection_ui(tab_list, state, term_stack);
    }
    activated
}

pub fn activate_workspace_relative(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    offset: isize,
) -> bool {
    let activated = state
        .borrow_mut()
        .activate_workspace_relative(offset)
        .is_some();
    if activated {
        sync_active_selection_ui(tab_list, state, term_stack);
    }
    activated
}

pub fn rename_tab(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    new_name: &str,
) -> bool {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return false;
    }

    {
        let mut st = state.borrow_mut();
        let Some(tab) = st.find_tab_mut(tab_id) else {
            return false;
        };
        tab.name = new_name.to_string();
    }

    if let Some(handle) = tab_row_handle(tab_list, tab_id) {
        handle.name_label.set_text(new_name);
    }

    true
}

/// Move `tab_id` to `new_index` within its own workspace, syncing both the
/// `AppState` model and the sidebar row widget so they cannot drift apart.
///
/// `new_index` is workspace-relative. Out-of-range indices are clamped to the
/// last position, and a tab that is already at the requested index is treated
/// as success (a no-op), so a caller alphabetising a workspace can re-run this
/// idempotently without spurious errors.
pub fn reorder_tab_to_index(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    new_index: usize,
) -> Result<(), String> {
    // Resolve the workspace, clamp the target, and detect a no-op under one borrow.
    let target = {
        let st = state.borrow();
        let ws = st
            .workspaces
            .iter()
            .find(|ws| ws.tabs.iter().any(|t| t.id == tab_id))
            .ok_or_else(|| "tab not found".to_string())?;
        let cur = ws
            .tabs
            .iter()
            .position(|t| t.id == tab_id)
            .ok_or_else(|| "tab not found".to_string())?;
        let last = ws.tabs.len().saturating_sub(1);
        let target = new_index.min(last);
        if cur == target {
            return Ok(());
        }
        target
    };

    if !state.borrow_mut().reorder_tab(tab_id, target) {
        return Err("reorder failed".to_string());
    }

    // Sync the sidebar row to the new model order. After the move, anchor the
    // dragged row relative to its same-workspace neighbour.
    enum Anchor {
        // Insert immediately after this tab's row (landed at index > 0).
        After(u32),
        // Insert before this tab's row (landed first); None means it is the
        // workspace's only tab and nothing needs to move.
        Before(Option<u32>),
    }
    let anchor = {
        let st = state.borrow();
        let ws = st
            .workspaces
            .iter()
            .find(|ws| ws.tabs.iter().any(|t| t.id == tab_id))
            .expect("workspace exists after successful reorder");
        let pos = ws
            .tabs
            .iter()
            .position(|t| t.id == tab_id)
            .expect("tab exists after successful reorder");
        if pos > 0 {
            Anchor::After(ws.tabs[pos - 1].id)
        } else {
            Anchor::Before(ws.tabs.get(1).map(|t| t.id))
        }
    };

    if let Some(dragged_row) = find_tab_row(tab_list, tab_id) {
        match anchor {
            Anchor::After(pred_id) => {
                if let Some(pred_row) = find_tab_row(tab_list, pred_id) {
                    tab_list.remove(&dragged_row);
                    tab_list.insert_child_after(&dragged_row, Some(&pred_row));
                }
            }
            Anchor::Before(Some(succ_id)) => {
                if let Some(succ_row) = find_tab_row(tab_list, succ_id) {
                    // Capture the slot before the successor *before* removing the
                    // dragged row, mirroring handle_tab_drop.
                    let before = succ_row.prev_sibling();
                    // Guard the adjacent case: if the dragged row already sits
                    // immediately before the successor, it is already in place.
                    if before.as_ref() != Some(dragged_row.upcast_ref::<gtk::Widget>()) {
                        tab_list.remove(&dragged_row);
                        tab_list.insert_child_after(&dragged_row, before.as_ref());
                    }
                }
            }
            Anchor::Before(None) => {}
        }
    }

    Ok(())
}

#[derive(Clone)]
struct TabRowState {
    is_active: bool,
    needs_attention: bool,
    primary_state: crate::workspace::TabPrimaryState,
    ports_label: String,
    runtime_probe_state: Option<crate::runtime_probe::RuntimeProbeState>,
    runtime_probe_tooltip: Option<String>,
    activity_text: Option<String>,
    notification_text: Option<String>,
    /// Names of running agents per pane, in pane order (may repeat).
    agent_names: Vec<String>,
    /// Shared per-pane projection also rendered by the right-hand Agents dock.
    agent_panes: Vec<crate::agent_projection::AgentPaneProjection>,
    tmux_command: Option<String>,
    tmux_probe_state: Option<crate::probe::ProbeState>,
    tmux_probe_error: Option<String>,
    /// True when the focused tmux-backed pane is likely nested (remote/SSH, or
    /// taarof itself launched inside tmux). Drives a warning on the tmux label.
    tmux_nested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RuntimeProbeSidebarUi {
    label: String,
    tooltip: String,
}

fn runtime_probe_sidebar_ui(
    truth: &crate::runtime_probe::RuntimeProbeTruth,
) -> Option<RuntimeProbeSidebarUi> {
    if matches!(truth.state, crate::runtime_probe::RuntimeProbeState::Ok) {
        return None;
    }

    fn age_label(age_ms: Option<u64>) -> Option<String> {
        age_ms.map(|age| {
            if age % 1_000 == 0 {
                format!("{}s", age / 1_000)
            } else {
                format!("{:.1}s", age as f64 / 1_000.0)
            }
        })
    }

    fn component_line(
        label: &str,
        state: crate::probe::ProbeState,
        age_ms: Option<u64>,
        error: Option<&str>,
    ) -> String {
        let mut line = format!("{label}: {}", state.label());
        if let Some(age) = age_label(age_ms) {
            if matches!(state, crate::probe::ProbeState::Ok) {
                line.push_str(&format!(" · observed {age} ago"));
            } else {
                line.push_str(&format!(" · last known {age} ago"));
            }
        }
        if let Some(error) = error {
            line.push_str(" · ");
            line.push_str(error);
        }
        line
    }

    Some(RuntimeProbeSidebarUi {
        label: format!("probe {}", truth.state.label()),
        tooltip: format!(
            "Runtime probe: {}\n{}\n{}",
            truth.state.label(),
            component_line(
                "Process",
                truth.process_state,
                truth.process_age_ms,
                truth.process_error.as_deref(),
            ),
            component_line(
                "Ports",
                truth.ports_state,
                truth.ports_age_ms,
                truth.ports_error.as_deref(),
            ),
        ),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TabAlertBinding {
    needs_attention: bool,
    primary_state: crate::workspace::TabPrimaryState,
    notification_text: Option<String>,
}

fn tab_alert_binding(state: &AppState, tab: &crate::Tab) -> TabAlertBinding {
    TabAlertBinding {
        needs_attention: state.tab_needs_attention(tab),
        primary_state: state.tab_primary_state(tab),
        notification_text: state.tab_notification_message(tab).map(str::to_string),
    }
}

/// Compact label for a multi-agent tab, e.g. `claude ×2 · codex`.
/// Returns None for zero or one agent — single-agent tabs keep today's look.
/// Resolve the distinct agent badges for a tab, in first-seen order.
///
/// Agents of the same kind collapse to a single chip (the sidebar row is a
/// rollup; per-pane / same-kind disambiguation lives in the tooltip). Returns
/// an empty vector when no agents are detected so single/no-agent rows stay
/// visually clean.
fn distinct_agent_badges(names: &[String]) -> Vec<crate::agents::AgentBadge> {
    let mut badges: Vec<crate::agents::AgentBadge> = Vec::new();
    for name in names {
        let badge = crate::agents::agent_badge(name);
        if !badges.iter().any(|existing| existing.name == badge.name) {
            badges.push(badge);
        }
    }
    badges
}

/// Rebuild the identity-chip widgets inside a tab row's badge container.
///
/// Each distinct agent kind gets a short-label chip carrying its stable colour
/// token as a CSS class (`agent-badge-<color_token>`), so the sidebar and web
/// Monitor render the same identity model. The container is hidden when empty.
fn refresh_agent_badges(container: &gtk::Box, badges: &[crate::agents::AgentBadge]) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    if badges.is_empty() {
        container.set_visible(false);
        container.set_tooltip_text(None);
        return;
    }
    for badge in badges {
        let chip = gtk::Label::new(Some(&badge.short_label));
        chip.add_css_class("tab-agent-badge");
        chip.add_css_class(&format!("agent-badge-{}", badge.color_token));
        chip.set_tooltip_text(Some(&badge.name));
        container.append(&chip);
    }
    container.set_visible(true);
}

fn format_agent_names_label(names: &[String]) -> Option<String> {
    if names.len() < 2 {
        return None;
    }
    let mut ordered: Vec<(String, usize)> = Vec::new();
    for name in names {
        if let Some(entry) = ordered.iter_mut().find(|(seen, _)| seen == name) {
            entry.1 += 1;
        } else {
            ordered.push((name.clone(), 1));
        }
    }
    Some(
        ordered
            .into_iter()
            .map(|(name, count)| {
                if count > 1 {
                    format!("{name} ×{count}")
                } else {
                    name
                }
            })
            .collect::<Vec<_>>()
            .join(" · "),
    )
}

/// Per-pane detail line for a multi-agent tab tooltip. Uses the same projection
/// as the right-hand Agents dock so labels, pane ids, states, and activity agree.
fn format_tab_agents_tooltip(
    panes: &[crate::agent_projection::AgentPaneProjection],
) -> Option<String> {
    if panes.len() < 2 {
        return None;
    }
    Some(
        panes
            .iter()
            .map(|pane| {
                let mut detail = format!(
                    "{} (pane {}): {}",
                    pane.agent_label,
                    pane.pane_id,
                    pane.state.label().to_lowercase()
                );
                if !pane.activity.trim().is_empty() {
                    detail.push_str(" · ");
                    detail.push_str(pane.activity.trim());
                }
                detail
            })
            .collect::<Vec<_>>()
            .join(" · "),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabIndicatorState {
    None,
    Running,
    Done,
    Alert,
}

fn tab_indicator_state(primary_state: crate::workspace::TabPrimaryState) -> TabIndicatorState {
    match primary_state {
        crate::workspace::TabPrimaryState::Idle => TabIndicatorState::None,
        crate::workspace::TabPrimaryState::Running => TabIndicatorState::Running,
        crate::workspace::TabPrimaryState::Done => TabIndicatorState::Done,
        crate::workspace::TabPrimaryState::Alert => TabIndicatorState::Alert,
    }
}

fn tab_indicator_glyph(indicator: TabIndicatorState) -> Option<&'static str> {
    match indicator {
        TabIndicatorState::None => None,
        TabIndicatorState::Running => Some("\u{25cf}"),
        TabIndicatorState::Done => Some("\u{2713}"),
        TabIndicatorState::Alert => Some("!"),
    }
}

fn tab_status_badge_text(indicator: TabIndicatorState) -> Option<&'static str> {
    match indicator {
        TabIndicatorState::None => None,
        TabIndicatorState::Running => Some("RUN"),
        TabIndicatorState::Done => Some("DONE"),
        TabIndicatorState::Alert => Some("ALERT"),
    }
}

fn format_tab_activity_summary(
    indicator: TabIndicatorState,
    activity_text: Option<&str>,
    notification_text: Option<&str>,
) -> Option<String> {
    let detail = activity_text
        .or(notification_text)
        .map(str::trim)
        .filter(|text| !text.is_empty());

    match indicator {
        TabIndicatorState::None => None,
        TabIndicatorState::Running => Some(match detail {
            Some(text)
                if text.eq_ignore_ascii_case("running") || text.eq_ignore_ascii_case("working") =>
            {
                "Running".to_string()
            }
            Some(text) => format!("Running: {text}"),
            None => "Running".to_string(),
        }),
        TabIndicatorState::Done => Some(match detail {
            Some(text) if text.eq_ignore_ascii_case("done") => "Done".to_string(),
            Some(text) => format!("Done: {text}"),
            None => "Done".to_string(),
        }),
        TabIndicatorState::Alert => Some(match detail {
            Some(text) if text.eq_ignore_ascii_case("alert") => "Alert".to_string(),
            Some(text) => format!("Alert: {text}"),
            None => "Alert".to_string(),
        }),
    }
}

pub fn refresh_all_tab_rows(tab_list: &gtk::Box, state: &Rc<RefCell<AppState>>) {
    let tab_ids: Vec<u32> = {
        let st = state.borrow();
        st.all_tabs().map(|tab| tab.id).collect()
    };

    for tab_id in tab_ids {
        refresh_tab_row(tab_list, state, tab_id);
    }
}

fn agent_children_visible(pane_count: usize, expanded: bool, compact: bool) -> bool {
    pane_count > 0 && (pane_count == 1 || expanded) && !compact
}

fn refresh_agent_child_rows(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    handle: &TabRowHandle,
    panes: &[crate::agent_projection::AgentPaneProjection],
) {
    let has_children = panes.len() > 1;
    let expanded = has_children && agent_tab_expanded(tab_list, panes[0].tab_id);
    let compact = compact_mode_active();
    let snapshot = (expanded, compact, panes.to_vec());
    let expected_expand_visible = has_children && !compact;
    let expected_children_visible = agent_children_visible(panes.len(), expanded, compact);
    if handle.last_agent_children.borrow().as_ref() == Some(&snapshot)
        && handle.agent_expand_button.is_visible() == expected_expand_visible
        && handle.agent_children.is_visible() == expected_children_visible
    {
        return;
    }
    *handle.last_agent_children.borrow_mut() = Some(snapshot);

    while let Some(child) = handle.agent_children.first_child() {
        handle.agent_children.remove(&child);
    }

    handle
        .agent_expand_button
        .set_visible(expected_expand_visible);
    handle
        .agent_expand_button
        .set_label(if expanded { "▾" } else { "▸" });
    handle
        .agent_expand_button
        .set_tooltip_text(has_children.then_some(if expanded {
            "Collapse agent panes"
        } else {
            "Expand agent panes"
        }));

    if !agent_children_visible(panes.len(), expanded, compact) {
        handle.agent_children.set_visible(false);
        return;
    }

    for pane in panes {
        let button = gtk::Button::new();
        button.add_css_class("flat");
        button.add_css_class("tab-agent-child");
        button.add_css_class(pane.state.css_class());
        button.set_tooltip_text(Some(&format!(
            "Focus {} in tab {} pane {}",
            pane.agent_label, pane.title, pane.pane_id
        )));

        let content = gtk::Box::new(gtk::Orientation::Vertical, 3);
        let parent_row = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        let badge = gtk::Label::new(Some(&pane.badge.short_label));
        badge.add_css_class("tab-agent-badge");
        badge.add_css_class(&format!("agent-badge-{}", pane.badge.color_token));
        parent_row.append(&badge);

        let labels = gtk::Box::new(gtk::Orientation::Vertical, 1);
        labels.set_hexpand(true);
        let identity = gtk::Label::new(Some(&format!(
            "{} · pane {}",
            pane.agent_label, pane.pane_id
        )));
        identity.add_css_class("tab-agent-child-identity");
        identity.set_halign(gtk::Align::Start);
        identity.set_ellipsize(gtk::pango::EllipsizeMode::End);
        labels.append(&identity);
        if !pane.activity.is_empty() {
            let activity = gtk::Label::new(Some(&pane.activity));
            activity.add_css_class("tab-agent-child-activity");
            activity.set_halign(gtk::Align::Start);
            activity.set_ellipsize(gtk::pango::EllipsizeMode::End);
            labels.append(&activity);
        }
        parent_row.append(&labels);

        let status = gtk::Label::new(Some(pane.state.ui_label()));
        status.add_css_class("tab-agent-child-state");
        status.add_css_class(pane.state.css_class());
        parent_row.append(&status);
        content.append(&parent_row);
        for child in &pane.children {
            let child_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            child_row.add_css_class("agent-headless-child");
            child_row.add_css_class(child.state.css_class());
            let marker = gtk::Label::new(Some("↳"));
            marker.add_css_class("agent-headless-marker");
            child_row.append(&marker);
            let child_label = gtk::Label::new(Some(&child.label));
            child_label.add_css_class("agent-headless-label");
            child_label.set_halign(gtk::Align::Start);
            child_label.set_hexpand(true);
            child_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            child_row.append(&child_label);
            let child_state = gtk::Label::new(Some(child.state.ui_label()));
            child_state.add_css_class("agent-headless-state");
            child_state.add_css_class(child.state.css_class());
            child_row.append(&child_state);
            content.append(&child_row);
        }
        button.set_child(Some(&content));

        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = handle.term_stack.clone();
        let tab_id = pane.tab_id;
        let pane_id = pane.pane_id;
        button.connect_clicked(move |_| {
            if !focus_agent_pane(&state, &tab_list, &term_stack, tab_id, pane_id) {
                crate::show_error_toast("That agent pane is no longer available");
            }
        });
        handle.agent_children.append(&button);
    }
    handle.agent_children.set_visible(true);
}

pub fn refresh_tab_row(tab_list: &gtk::Box, state: &Rc<RefCell<AppState>>, tab_id: u32) {
    let row_state = {
        let st = state.borrow();
        let Some((ws, tab)) = st.find_tab(tab_id) else {
            return;
        };
        let alert = tab_alert_binding(&st, tab);
        let agent_panes: Vec<_> = crate::agent_projection::project_tab_agent_panes(&st, tab_id);
        let runtime_probe_truth = crate::runtime_probe::runtime_probe_truth_for_state(
            &st,
            crate::events::unix_time_ms(),
            crate::runtime_probe::PROBE_TTL_MS,
        );
        let runtime_probe_ui = runtime_probe_sidebar_ui(&runtime_probe_truth);
        let ports_label = [
            Some(crate::agents::format_ports_label(&tab.listening_ports))
                .filter(|label| !label.is_empty()),
            runtime_probe_ui.as_ref().map(|ui| ui.label.clone()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        TabRowState {
            is_active: st.active_workspace == ws.id && st.presented_tab_id() == Some(tab_id),
            needs_attention: alert.needs_attention,
            primary_state: alert.primary_state,
            ports_label,
            runtime_probe_state: runtime_probe_ui.as_ref().map(|_| runtime_probe_truth.state),
            runtime_probe_tooltip: runtime_probe_ui.map(|ui| ui.tooltip),
            activity_text: tab
                .primary_agent_activity()
                .map(|(_, activity)| activity.text.clone()),
            notification_text: alert.notification_text,
            agent_names: agent_panes
                .iter()
                .map(|pane| pane.badge.name.clone())
                .collect(),
            agent_panes,
            tmux_command: tab.tmux_running_command().map(|s| s.to_string()),
            tmux_probe_state: tab.panes.leaf(tab.focused_pane_id).and_then(|leaf| {
                leaf.tmux_backing
                    .as_ref()
                    .map(|backing| backing.pane_info.state)
            }),
            tmux_probe_error: tab
                .panes
                .leaf(tab.focused_pane_id)
                .and_then(|leaf| leaf.tmux_backing.as_ref())
                .and_then(|backing| backing.pane_info.error.clone()),
            tmux_nested: tab
                .panes
                .leaf(tab.focused_pane_id)
                .and_then(|leaf| leaf.tmux_backing.as_ref())
                .is_some_and(|backing| {
                    let over_ssh = matches!(backing.target, crate::tmux::TmuxTarget::Remote { .. });
                    crate::config::tmux_nesting_warning(
                        crate::config::app_launched_inside_tmux(),
                        over_ssh,
                    )
                }),
        }
    };

    let Some(handle) = tab_row_handle(tab_list, tab_id) else {
        return;
    };
    refresh_agent_child_rows(tab_list, state, &handle, &row_state.agent_panes);
    let row = handle.row.clone();
    let indicator = tab_indicator_state(row_state.primary_state);

    if row_state.is_active {
        row.add_css_class("active");
    } else {
        row.remove_css_class("active");
    }

    if row_state.needs_attention {
        row.add_css_class("attention");
    } else {
        row.remove_css_class("attention");
    }

    if matches!(indicator, TabIndicatorState::Done) {
        row.add_css_class("activity-done");
    } else {
        row.remove_css_class("activity-done");
    }

    // Rollup vs detail: the compact combined label (badge + `claude ×2 · codex`)
    // is the aggregate; the tooltip preserves per-pane detail so multi-agent
    // tabs never lose which agent is in which pane / state.
    let mut tooltip = handle.name_label.text().to_string();
    let cwd = handle.cwd_label.text();
    if !cwd.is_empty() {
        tooltip.push('\n');
        tooltip.push_str(&cwd);
    }
    if let Some(detail) = format_tab_agents_tooltip(&row_state.agent_panes) {
        tooltip.push('\n');
        tooltip.push_str(&detail);
    }
    if let Some(detail) = row_state.runtime_probe_tooltip.as_deref() {
        tooltip.push('\n');
        tooltip.push_str(detail);
    }
    row.set_tooltip_text(Some(&tooltip));

    let dot = handle.agent_dot;
    dot.remove_css_class("running");
    dot.remove_css_class("waiting");
    dot.remove_css_class("done");
    if let Some(glyph) = tab_indicator_glyph(indicator) {
        dot.set_text(glyph);
        dot.set_visible(true);
        match indicator {
            TabIndicatorState::Running => dot.add_css_class("running"),
            TabIndicatorState::Done => dot.add_css_class("done"),
            TabIndicatorState::Alert => dot.add_css_class("waiting"),
            TabIndicatorState::None => {}
        }
    } else {
        dot.set_text("");
        dot.set_visible(false);
    }

    let status_badge = handle.status_badge;
    status_badge.remove_css_class("running");
    status_badge.remove_css_class("done");
    status_badge.remove_css_class("alert");
    if let Some(text) = tab_status_badge_text(indicator) {
        status_badge.set_text(text);
        status_badge.set_visible(true);
        match indicator {
            TabIndicatorState::Running => status_badge.add_css_class("running"),
            TabIndicatorState::Done => status_badge.add_css_class("done"),
            TabIndicatorState::Alert => status_badge.add_css_class("alert"),
            TabIndicatorState::None => {}
        }
    } else {
        status_badge.set_text("");
        status_badge.set_visible(false);
    }

    // Identity chips: one colored short-label chip per distinct agent kind.
    refresh_agent_badges(
        &handle.agent_badges,
        &distinct_agent_badges(&row_state.agent_names),
    );

    handle.ports_label.remove_css_class("probe-stale");
    handle.ports_label.remove_css_class("probe-error");
    if let Some(state) = row_state.runtime_probe_state {
        if matches!(state, crate::runtime_probe::RuntimeProbeState::Failed) {
            handle.ports_label.add_css_class("probe-error");
        } else if state.is_degraded() {
            handle.ports_label.add_css_class("probe-stale");
        }
    }
    handle.ports_label.set_text(&row_state.ports_label);
    handle
        .ports_label
        .set_tooltip_text(row_state.runtime_probe_tooltip.as_deref());
    handle
        .ports_label
        .set_visible(!row_state.ports_label.is_empty());

    let activity_label = handle.activity_label;
    activity_label.remove_css_class("alert");
    activity_label.remove_css_class("running");
    activity_label.remove_css_class("done");
    let activity_summary = format_tab_activity_summary(
        indicator,
        row_state.activity_text.as_deref(),
        row_state.notification_text.as_deref(),
    );
    let combined_summary = match (
        format_agent_names_label(&row_state.agent_names),
        activity_summary,
    ) {
        (Some(agents), Some(text)) => Some(format!("{agents} — {text}")),
        (Some(agents), None) => Some(agents),
        (None, summary) => summary,
    };
    match combined_summary {
        Some(text) => {
            activity_label.set_text(&text);
            activity_label.set_visible(true);
            match indicator {
                TabIndicatorState::Running => activity_label.add_css_class("running"),
                TabIndicatorState::Done => activity_label.add_css_class("done"),
                TabIndicatorState::Alert => activity_label.add_css_class("alert"),
                TabIndicatorState::None => {}
            }
        }
        None => {
            activity_label.set_text("");
            activity_label.set_visible(false);
        }
    }

    let tmux_label = handle.tmux_label;
    tmux_label.remove_css_class("probe-stale");
    tmux_label.remove_css_class("probe-error");
    tmux_label.remove_css_class("dim-label");
    match (
        row_state.tmux_probe_state,
        row_state.tmux_command.as_deref(),
    ) {
        (Some(crate::probe::ProbeState::Ok), Some(cmd)) => {
            if row_state.tmux_nested {
                tmux_label.set_text(&format!("{cmd} ⚠ nested"));
                tmux_label.add_css_class("probe-stale");
                tmux_label.set_tooltip_text(Some(
                    "This tmux session is nested (remote/SSH or taarof is already inside tmux). Prefix keys twice, or detach the outer session.",
                ));
            } else {
                tmux_label.set_text(cmd);
                tmux_label.set_tooltip_text(None);
            }
            tmux_label.set_visible(true);
        }
        (Some(crate::probe::ProbeState::Stale), Some(cmd)) => {
            tmux_label.set_text(&format!("stale: {cmd}"));
            tmux_label.set_visible(true);
            tmux_label.add_css_class("probe-stale");
            tmux_label.set_tooltip_text(row_state.tmux_probe_error.as_deref());
        }
        (Some(crate::probe::ProbeState::Stale), None) => {
            tmux_label.set_text("tmux stale");
            tmux_label.set_visible(true);
            tmux_label.add_css_class("probe-stale");
            tmux_label.set_tooltip_text(row_state.tmux_probe_error.as_deref());
        }
        (Some(crate::probe::ProbeState::Error), _) => {
            tmux_label.set_text("tmux error");
            tmux_label.set_visible(true);
            tmux_label.add_css_class("probe-error");
            tmux_label.set_tooltip_text(row_state.tmux_probe_error.as_deref());
        }
        _ => {
            tmux_label.set_text("");
            tmux_label.set_visible(false);
            tmux_label.set_tooltip_text(None);
        }
    }
    if compact_mode_active() {
        set_compact_hidden_visibility(row.upcast_ref(), true);
    }
}

/// Add a clickable tab row with close button and double-click rename.
/// Returns labels for CWD and git branch (updated by terminal on directory change).
pub fn add_tab_row(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_id: u32,
    name: &str,
    set_active: bool,
) -> TabLabels {
    // Outer container: Vertical so we can stack main row + actions + tracking
    let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
    row.add_css_class("tab-row");
    row.set_widget_name(&tab_row_widget_name(tab_id));

    // Main row content (the visible, clickable row)
    // Main clickable area (not a Button — avoids focus-stealing on click)
    let btn_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    btn_box.add_css_class("tab-button");
    btn_box.set_hexpand(true);

    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    hbox.set_margin_end(32);

    // Agent status indicator dot (hidden by default)
    let agent_dot = gtk::Label::new(Some("●"));
    agent_dot.add_css_class("agent-dot");
    agent_dot.set_visible(false);
    agent_dot.set_widget_name(&tab_agent_dot_widget_name(tab_id));

    let num_label = gtk::Label::new(Some(&format!("{tab_id}")));
    num_label.add_css_class("tab-number");
    let agent_expand_button = gtk::Button::with_label("▸");
    agent_expand_button.add_css_class("flat");
    agent_expand_button.add_css_class("tab-agent-expand");
    agent_expand_button.add_css_class("compact-hidden");
    agent_expand_button.add_css_class("runtime-visibility");
    agent_expand_button.set_focus_on_click(false);
    agent_expand_button.set_visible(false);
    let name_label = gtk::Label::new(Some(name));
    name_label.set_halign(gtk::Align::Start);
    name_label.set_hexpand(true);
    name_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name_label.add_css_class("tab-name");
    name_label.add_css_class("compact-hidden");
    name_label.set_widget_name(&tab_name_widget_name(tab_id));

    let status_badge = gtk::Label::new(None);
    status_badge.add_css_class("tab-status-badge");
    status_badge.add_css_class("compact-hidden");
    status_badge.set_visible(false);

    // Identity chips: one small colored short-label chip per detected agent
    // kind. Populated in `refresh_tab_row`; hidden when no agents are present.
    let agent_badges = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    agent_badges.add_css_class("tab-agent-badges");
    agent_badges.add_css_class("compact-hidden");
    agent_badges.set_visible(false);

    // Git branch label (right-aligned, muted)
    let branch_label = gtk::Label::new(None);
    branch_label.set_halign(gtk::Align::End);
    branch_label.add_css_class("tab-branch");
    branch_label.add_css_class("compact-hidden");
    branch_label.set_visible(false);
    branch_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    branch_label.set_max_width_chars(14);
    branch_label.set_widget_name(&tab_branch_widget_name(tab_id));

    hbox.append(&agent_dot);
    hbox.append(&num_label);
    hbox.append(&agent_expand_button);
    hbox.append(&name_label);
    hbox.append(&branch_label);

    // Keep fixed-width lifecycle/provider chips off the identity line. A
    // badge-rich row otherwise sums every chip into one minimum width, which
    // makes GtkPaned allocate the entire 220px sidebar offscreen to the left.
    let agent_summary = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    agent_summary.add_css_class("tab-agent-summary");
    agent_summary.add_css_class("compact-hidden");
    agent_summary.append(&status_badge);
    agent_summary.append(&agent_badges);

    // ── Line 2: CWD + ports ──
    let cwd_hbox = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    cwd_hbox.add_css_class("tab-cwd-box");
    cwd_hbox.add_css_class("compact-hidden");

    let cwd_label = gtk::Label::new(None);
    cwd_label.set_halign(gtk::Align::Start);
    // hexpand is new — needed so CWD absorbs slack and pushes ports_label to the right
    cwd_label.set_hexpand(true);
    cwd_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    cwd_label.add_css_class("tab-cwd");
    cwd_label.set_widget_name(&tab_cwd_widget_name(tab_id));

    let ports_label = gtk::Label::new(None);
    ports_label.set_halign(gtk::Align::End);
    ports_label.add_css_class("tab-ports");
    ports_label.set_visible(false);
    ports_label.set_widget_name(&tab_ports_widget_name(tab_id));
    ports_label.set_margin_start(6);

    let tmux_label = gtk::Label::new(None);
    tmux_label.set_halign(gtk::Align::End);
    tmux_label.add_css_class("tab-tmux");
    tmux_label.set_visible(false);
    tmux_label.set_widget_name(&tab_tmux_widget_name(tab_id));
    tmux_label.set_margin_start(4);

    let activity_label = gtk::Label::new(None);
    activity_label.set_halign(gtk::Align::Start);
    activity_label.set_hexpand(true);
    activity_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    activity_label.add_css_class("tab-activity");
    activity_label.add_css_class("compact-hidden");
    activity_label.set_widget_name(&tab_activity_widget_name(tab_id));
    activity_label.set_visible(false);

    cwd_hbox.append(&cwd_label);
    cwd_hbox.append(&ports_label);
    cwd_hbox.append(&tmux_label);

    // Close button (visible on hover via CSS)
    let close_btn = gtk::Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("tab-close");
    close_btn.add_css_class("compact-hidden");

    btn_box.append(&hbox);
    btn_box.append(&agent_summary);
    btn_box.append(&cwd_hbox);
    btn_box.append(&activity_label);
    let main_overlay = gtk::Overlay::new();
    main_overlay.set_child(Some(&btn_box));
    close_btn.set_halign(gtk::Align::End);
    close_btn.set_valign(gtk::Align::Start);
    main_overlay.add_overlay(&close_btn);
    row.append(&main_overlay);

    let agent_children = gtk::Box::new(gtk::Orientation::Vertical, 2);
    agent_children.add_css_class("tab-agent-children");
    agent_children.add_css_class("compact-hidden");
    agent_children.add_css_class("runtime-visibility");
    agent_children.set_visible(false);
    row.append(&agent_children);

    {
        let tab_list = tab_list.clone();
        let state = state.clone();
        agent_expand_button.connect_clicked(move |_| {
            let expanded = !agent_tab_expanded(&tab_list, tab_id);
            set_agent_tab_expanded(&tab_list, tab_id, expanded);
            refresh_tab_row(&tab_list, &state, tab_id);
        });
    }

    // Per-tab action buttons (hidden until discovery)
    let actions_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    actions_box.add_css_class("tab-actions");
    actions_box.add_css_class("compact-hidden");
    actions_box.set_widget_name(&tab_actions_widget_name(tab_id));
    actions_box.set_visible(false);
    row.append(&actions_box);

    // Per-tab tracking section (hidden until discovery)
    let tracking_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    tracking_box.add_css_class("tab-tracking");
    tracking_box.add_css_class("compact-hidden");
    tracking_box.set_widget_name(&tab_tracking_widget_name(tab_id));
    tracking_box.set_visible(false);
    row.append(&tracking_box);

    // GestureClick handles both single-click (switch) and double-click (rename)
    // Using a gesture on the box avoids the Button focus-steal problem.
    {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        let row = row.downgrade();
        let name_label = name_label.clone();
        let hbox = hbox.clone();

        // Track whether a rename was triggered to suppress the delayed focus
        let rename_active = Rc::new(Cell::new(false));

        let gesture = gtk::GestureClick::new();
        gesture.set_button(1);
        let rename_active_c = rename_active.clone();
        gesture.connect_released(move |gesture, n_press, _x, _y| {
            let Some(row) = row.upgrade() else {
                return;
            };
            if n_press == 2 {
                // Double-click: rename
                gesture.set_state(gtk::EventSequenceState::Claimed);
                rename_active_c.set(true);
                start_rename(&hbox, &name_label, &state, tab_id);
            } else if n_press == 1 {
                // Single click: switch tab, delay focus to allow double-click
                activate_tab_row(&tab_list, &state, &term_stack, tab_id, &row);
                let state_for_focus = state.clone();
                let rename_active = rename_active_c.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                    if !rename_active.get() {
                        if let Some(terminal) = crate::get_active_terminal(&state_for_focus) {
                            terminal.grab_focus();
                        }
                    }
                    rename_active.set(false);
                });
            }
        });
        btn_box.add_controller(gesture);
    }

    // Click close → remove tab
    {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        let row = row.downgrade();
        close_btn.connect_clicked(move |_| {
            let Some(row) = row.upgrade() else {
                return;
            };
            let state = state.clone();
            let term_stack = term_stack.clone();
            let tab_list = tab_list.clone();
            defer_user_tab_close(move || {
                confirm_then_close_tab(&tab_list, &state, &term_stack, tab_id, &row);
            });
        });
    }

    // Right click → tab context menu
    {
        let menu_row = row.downgrade();
        let hbox = hbox.clone();
        let name_label = name_label.clone();
        let cwd_label = cwd_label.clone();
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        let gesture = gtk::GestureClick::new();
        gesture.set_button(3);
        gesture.connect_released(move |gesture, _n_press, x, y| {
            let Some(menu_row) = menu_row.upgrade() else {
                return;
            };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            show_tab_row_menu(
                &menu_row,
                &hbox,
                &name_label,
                &cwd_label,
                &tab_list,
                &state,
                &term_stack,
                tab_id,
                x,
                y,
            );
        });
        row.add_controller(gesture);
    }

    // Drag-and-drop: make tab rows draggable and droppable
    setup_tab_drag_source(&row, tab_id);
    setup_tab_drop_target(&row, tab_list, state, term_stack, tab_id);

    let workspace_id = {
        let st = state.borrow();
        st.find_tab(tab_id).map(|(ws, _)| ws.id)
    };
    register_tab_row(
        tab_list,
        tab_id,
        TabRowHandle {
            row: row.clone(),
            term_stack: term_stack.clone(),
            name_label: name_label.clone(),
            status_badge: status_badge.clone(),
            agent_badges: agent_badges.clone(),
            agent_expand_button: agent_expand_button.clone(),
            agent_children: agent_children.clone(),
            last_agent_children: Rc::new(RefCell::new(None)),
            cwd_label: cwd_label.clone(),
            branch_label: branch_label.clone(),
            ports_label: ports_label.clone(),
            tmux_label: tmux_label.clone(),
            activity_label: activity_label.clone(),
            agent_dot: agent_dot.clone(),
            actions_box: actions_box.clone(),
            tracking_box: tracking_box.clone(),
            plan_tasks: Rc::new(RefCell::new(None)),
            discovery_generation: Rc::new(Cell::new(0)),
            discovery_poll_source: Rc::new(RefCell::new(None)),
            plan_monitor: Rc::new(RefCell::new(None)),
        },
    );
    if let Some(workspace_id) = workspace_id {
        place_tab_row_in_workspace(tab_list, state, &row, tab_id, workspace_id);
    } else {
        tab_list.append(&row);
    }

    if set_active {
        activate_tab_row(tab_list, state, term_stack, tab_id, &row);
    }

    refresh_tab_row(tab_list, state, tab_id);
    set_compact_hidden_visibility(row.upcast_ref(), compact_mode_active());

    TabLabels {
        cwd_label,
        branch_label,
    }
}

pub fn find_tab_labels(tab_list: &gtk::Box, tab_id: u32) -> Option<TabLabels> {
    let handle = tab_row_handle(tab_list, tab_id)?;
    Some(TabLabels {
        cwd_label: handle.cwd_label,
        branch_label: handle.branch_label,
    })
}

/// Replace the name label with an editable Entry for inline rename.
fn start_rename(
    hbox: &gtk::Box,
    name_label: &gtk::Label,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
) {
    // Guard: don't open a second entry if one is already active
    if !gtk::prelude::WidgetExt::is_visible(name_label) {
        return;
    }

    let current_name = name_label.text().to_string();

    let entry = gtk::Entry::new();
    entry.set_text(&current_name);
    entry.add_css_class("tab-rename-entry");
    entry.set_hexpand(true);

    // Hide label, show entry
    name_label.set_visible(false);
    hbox.append(&entry);
    entry.grab_focus();
    entry.select_region(0, -1);

    // Commit rename on Enter
    {
        let state = state.clone();
        let name_label = name_label.clone();
        let hbox = hbox.clone();
        let entry_ref = entry.clone();
        entry.connect_activate(move |entry| {
            let new_name = entry.text().to_string();
            finish_rename(&hbox, &name_label, &entry_ref, &state, tab_id, &new_name);
        });
    }

    // Cancel on Escape
    {
        let name_label = name_label.clone();
        let hbox = hbox.clone();
        let entry_ref = entry.clone();
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.connect_key_pressed(move |_ctrl, key, _code, _mods| {
            if key == gdk::Key::Escape {
                cancel_rename(&hbox, &name_label, &entry_ref);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        entry.add_controller(key_ctrl);
    }

    // Commit on focus loss
    {
        let state = state.clone();
        let name_label = name_label.clone();
        let hbox = hbox.clone();
        let entry_ref = entry.clone();
        let focus_ctrl = gtk::EventControllerFocus::new();
        focus_ctrl.connect_leave(move |_| {
            if gtk::prelude::WidgetExt::is_visible(&entry_ref) {
                let new_name = entry_ref.text().to_string();
                finish_rename(&hbox, &name_label, &entry_ref, &state, tab_id, &new_name);
            }
        });
        entry.add_controller(focus_ctrl);
    }
}

fn finish_rename(
    hbox: &gtk::Box,
    name_label: &gtk::Label,
    entry: &gtk::Entry,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    new_name: &str,
) {
    let new_name = new_name.trim();
    if !new_name.is_empty() {
        name_label.set_text(new_name);
        let mut st = state.borrow_mut();
        if let Some(tab) = st.find_tab_mut(tab_id) {
            tab.name = new_name.to_string();
        }
    }
    name_label.set_visible(true);
    hbox.remove(entry);
}

fn cancel_rename(hbox: &gtk::Box, name_label: &gtk::Label, entry: &gtk::Entry) {
    name_label.set_visible(true);
    hbox.remove(entry);
}

/// Inline rename for the session label at the bottom.
fn start_session_rename(session_box: &gtk::Box, session_label: &gtk::Label) {
    if !gtk::prelude::WidgetExt::is_visible(session_label) {
        return;
    }

    let current = session_label.text().to_string();
    let entry = gtk::Entry::new();
    entry.set_text(&current);
    entry.add_css_class("tab-rename-entry");
    entry.set_hexpand(true);

    session_label.set_visible(false);
    session_box.append(&entry);
    entry.grab_focus();
    entry.select_region(0, -1);

    // Enter → commit
    {
        let label = session_label.clone();
        let bx = session_box.clone();
        let e = entry.clone();
        entry.connect_activate(move |entry| {
            let name = entry.text().to_string().trim().to_string();
            if !name.is_empty() {
                label.set_text(&name);
            }
            label.set_visible(true);
            bx.remove(&e);
        });
    }

    // Escape → cancel
    {
        let label = session_label.clone();
        let bx = session_box.clone();
        let e = entry.clone();
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.connect_key_pressed(move |_ctrl, key, _code, _mods| {
            if key == gdk::Key::Escape {
                label.set_visible(true);
                bx.remove(&e);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        entry.add_controller(key_ctrl);
    }

    // Focus loss → commit
    {
        let label = session_label.clone();
        let bx = session_box.clone();
        let e = entry.clone();
        let focus_ctrl = gtk::EventControllerFocus::new();
        focus_ctrl.connect_leave(move |_| {
            if gtk::prelude::WidgetExt::is_visible(&e) {
                let name = e.text().to_string().trim().to_string();
                if !name.is_empty() {
                    label.set_text(&name);
                }
                label.set_visible(true);
                bx.remove(&e);
            }
        });
        entry.add_controller(focus_ctrl);
    }
}

/// Mark a tab row as active: update CSS classes and switch the terminal stack.
/// Note: does NOT grab_focus — that's handled with a delay in the gesture handler.
pub fn activate_tab_row(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_id: u32,
    _row: &gtk::Box,
) {
    let _ = activate_tab(tab_list, state, term_stack, tab_id);
}

/// Close a tab: remove from sidebar, destroy terminal, switch to neighbor.
/// Summary of the tmux sessions a tab is backing, used to decide whether
/// closing needs a kill/detach confirmation and to describe the sessions in it.
struct TabTmuxSummary {
    /// Number of live tmux-backed panes in the tab.
    session_count: usize,
    /// A representative session name (first backing) for messaging.
    first_session: Option<String>,
    /// A representative host label (localhost or ssh target) for messaging.
    first_host: Option<String>,
    /// Whether any backing targets a remote host over SSH.
    any_remote: bool,
}

/// Inspect a tab's panes and summarize its live tmux backings.
fn tab_tmux_summary(state: &Rc<RefCell<AppState>>, tab_id: u32) -> TabTmuxSummary {
    let st = state.borrow();
    let mut summary = TabTmuxSummary {
        session_count: 0,
        first_session: None,
        first_host: None,
        any_remote: false,
    };
    if let Some((_, tab)) = st.find_tab(tab_id) {
        for leaf in tab.panes.leaves() {
            if let Some(backing) = leaf.tmux_backing.as_ref() {
                summary.session_count += 1;
                let (host, remote) = match &backing.target {
                    crate::tmux::TmuxTarget::Local => ("localhost".to_string(), false),
                    crate::tmux::TmuxTarget::Remote { ssh_target } => (ssh_target.clone(), true),
                };
                if remote {
                    summary.any_remote = true;
                }
                if summary.first_session.is_none() {
                    summary.first_session = Some(backing.session_name.clone());
                    summary.first_host = Some(host);
                }
            }
        }
    }
    summary
}

/// User-initiated tab close. When the tab is tmux-backed and the configured
/// behavior would KILL the session (close_behavior = close), confirm first and
/// offer Detach as the non-destructive alternative. Otherwise (detach behavior,
/// or a non-tmux tab) close immediately; for detach behavior a toast reassures
/// the user the session survives. Programmatic callers use `close_tab` directly.
fn confirm_then_close_tab(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_id: u32,
    row: &gtk::Box,
) {
    let summary = tab_tmux_summary(state, tab_id);
    let behavior = crate::config::tmux_config().close_behavior;
    let consequence = crate::config::tmux_close_consequence(behavior, summary.session_count > 0);

    match consequence {
        crate::config::TmuxCloseConsequence::ConfirmKill => {
            prompt_kill_or_detach_tab(tab_list, state, term_stack, tab_id, row, &summary);
        }
        crate::config::TmuxCloseConsequence::DetachSurvives => {
            if summary.session_count > 0 {
                detach_tab_sessions(tab_list, state, term_stack, tab_id);
            } else {
                let _ = close_tab(tab_list, state, term_stack, tab_id, row);
            }
        }
    }
}

/// Modal Kill / Detach / Cancel dialog shown before killing a tmux-backed tab.
fn prompt_kill_or_detach_tab(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_id: u32,
    row: &gtk::Box,
    summary: &TabTmuxSummary,
) {
    let Some(window) = widget_window(tab_list) else {
        // No window to parent a dialog to — fall back to closing directly.
        close_tab(tab_list, state, term_stack, tab_id, row);
        return;
    };

    let session_name = summary
        .first_session
        .clone()
        .unwrap_or_else(|| "session".to_string());
    let host = summary
        .first_host
        .clone()
        .unwrap_or_else(|| "localhost".to_string());
    let headline = if summary.session_count > 1 {
        format!(
            "This kills {} tmux sessions (including {session_name}) on {host}.",
            summary.session_count
        )
    } else {
        format!("This kills tmux session {session_name} on {host}.")
    };

    let dialog = gtk::Dialog::builder()
        .transient_for(&window)
        .modal(true)
        .title("Close tmux-backed tab")
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    // Detach = keep session running; Kill = destroy it.
    dialog.add_button("Detach", gtk::ResponseType::No);
    dialog.add_button("Kill", gtk::ResponseType::Yes);
    dialog.set_default_response(gtk::ResponseType::No);

    let content = dialog.content_area();
    content.set_spacing(8);
    let label = gtk::Label::new(Some(&headline));
    label.set_halign(gtk::Align::Start);
    label.set_wrap(true);
    let detail = gtk::Label::new(Some(
        "Detach instead to leave it running in the background; reattach later from the command palette.",
    ));
    detail.set_halign(gtk::Align::Start);
    detail.set_wrap(true);
    detail.add_css_class("tab-cwd");
    content.append(&label);
    content.append(&detail);

    {
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        let row = row.clone();
        dialog.connect_response(move |dialog, response| {
            match response {
                gtk::ResponseType::Yes => {
                    // Kill: close_tab honors the configured close behavior, which
                    // is `close` (kill) in this branch.
                    close_tab(&tab_list, &state, &term_stack, tab_id, &row);
                }
                gtk::ResponseType::No => {
                    // Detach every tmux-backed pane in the tab, then close it.
                    detach_tab_sessions(&tab_list, &state, &term_stack, tab_id);
                }
                _ => {}
            }
            dialog.close();
        });
    }

    dialog.present();
}

/// Detach all tmux-backed panes in a tab (leaving their sessions running), then
/// remove the tab. Used by both configured auto-detach and the confirmation
/// dialog's Detach path.
fn detach_tab_sessions(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_id: u32,
) {
    // Match close_tab's guard before mutating tracker/backing state. In
    // particular, attempting to close the application's last tab remains a
    // no-op instead of leaving its still-visible terminal marked detached.
    let close_meta = {
        let st = state.borrow();
        let total_tabs: usize = st
            .workspaces
            .iter()
            .map(|workspace| workspace.tabs.len())
            .sum();
        let Some((workspace, _)) = st.find_tab(tab_id) else {
            return;
        };
        (
            total_tabs,
            workspace.id,
            workspace.tabs.len(),
            workspace.is_worktree,
            workspace.working_tree_path.clone(),
            workspace.repo_root.clone(),
        )
    };
    let Some(_row) = tab_row_handle(tab_list, tab_id).map(|handle| handle.row) else {
        return;
    };
    if close_meta.0 <= 1 {
        return;
    }

    // A single-tab worktree workspace has an asynchronous confirmation. Do
    // not mutate tracker/backing state until the user actually confirms it.
    if close_meta.2 == 1 && close_meta.3 {
        if let Some(path) = close_meta.4 {
            prompt_close_worktree_workspace(
                tab_list,
                state,
                term_stack,
                close_meta.1,
                &path,
                close_meta.5,
                Some(tab_id),
            );
            return;
        }
    }

    let prepared = match crate::socket::prepare_tab_close(
        state,
        tab_id,
        crate::config::TmuxCloseBehavior::Detach,
    ) {
        Ok(prepared) => prepared,
        Err(err) => {
            crate::show_error_toast(&format!("Detach failed: {err}"));
            return;
        }
    };
    apply_prepared_sidebar_close(tab_list, state, term_stack, tab_id, prepared);
}

fn apply_prepared_sidebar_close(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_id: u32,
    prepared: crate::socket::PreparedTabClose,
) {
    let _ = crate::socket::apply_tab_close_plan(
        prepared.plan,
        || close_tab_by_id(tab_list, state, term_stack, tab_id),
        |workspace_id| close_workspace_by_id(tab_list, state, term_stack, workspace_id),
    );
    surface_detached_tab(tab_list, state, term_stack, &prepared.detached_sessions);
}

fn surface_detached_tab(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    detached_sessions: &[String],
) {
    if detached_sessions.is_empty() {
        return;
    }
    if let Some(window) = widget_window(tab_list) {
        refresh_background_section(tab_list, state, term_stack, &window);
    }
    match detached_sessions.last() {
        Some(name) => crate::show_toast(&format!(
            "Detached tmux session {name}; it keeps running in the background"
        )),
        None => crate::show_toast("Detached tmux session; it keeps running in the background"),
    }
}

fn register_worktree_detach_after_confirmation(
    state: &Rc<RefCell<AppState>>,
    tab_id: Option<u32>,
    response: gtk::ResponseType,
) -> Result<Option<crate::socket::PreparedTabClose>, String> {
    if !matches!(response, gtk::ResponseType::No | gtk::ResponseType::Yes) {
        return Ok(None);
    }
    tab_id
        .map(|tab_id| {
            crate::socket::prepare_tab_close(
                state,
                tab_id,
                crate::config::TmuxCloseBehavior::Detach,
            )
        })
        .transpose()
}

fn close_tab(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_id: u32,
    row: &gtk::Box,
) -> Vec<String> {
    let (
        workspace_id,
        workspace_name,
        workspace_tab_count,
        is_worktree,
        working_tree_path,
        repo_root,
    ) = {
        let st = state.borrow();
        let total_tabs: usize = st.workspaces.iter().map(|w| w.tabs.len()).sum();
        if total_tabs <= 1 {
            return Vec::new();
        }
        let Some((workspace, _)) = st.find_tab(tab_id) else {
            return Vec::new();
        };
        (
            workspace.id,
            workspace.name.clone(),
            workspace.tabs.len(),
            workspace.is_worktree,
            workspace.working_tree_path.clone(),
            workspace.repo_root.clone(),
        )
    };

    if workspace_tab_count == 1 && is_worktree {
        if let Some(path) = working_tree_path {
            prompt_close_worktree_workspace(
                tab_list,
                state,
                term_stack,
                workspace_id,
                &path,
                repo_root,
                None,
            );
            return Vec::new();
        }
    }

    let was_active = {
        let st = state.borrow();
        st.active_ws().is_some_and(|w| w.active_tab == tab_id)
    };

    let neighbor_id = if was_active {
        let st = state.borrow();
        let tab_ids: Vec<u32> = st.all_tabs().map(|t| t.id).collect();
        let active_tab_id = st.active_ws().map_or(0, |w| w.active_tab);
        match next_tab_after_close(&tab_ids, active_tab_id, tab_id) {
            Some(id) => id,
            None => return Vec::new(),
        }
    } else {
        0
    };

    // Snapshot immutable teardown inputs while the tab is still live. Tmux/SSH
    // waits run before any pane tree or GTK row is removed.
    let tmux_backings: Vec<crate::pane::TmuxBacking> = {
        let st = state.borrow();
        let mut backings: Vec<crate::pane::TmuxBacking> = st
            .find_tab(tab_id)
            .map(|(_, tab)| {
                tab.panes
                    .leaves()
                    .into_iter()
                    .filter_map(|leaf| leaf.tmux_backing.clone())
                    .collect()
            })
            .unwrap_or_default();
        backings.extend(
            st.headless_panes
                .iter()
                .filter(|((current_tab_id, _), _)| *current_tab_id == tab_id)
                .filter_map(|(_, pane)| pane.tmux_backing.clone()),
        );
        let mut seen = std::collections::HashSet::new();
        backings
            .retain(|backing| seen.insert((backing.target.clone(), backing.session_name.clone())));
        backings
    };

    if !tmux_backings.is_empty() {
        let Some(window) = widget_window(tab_list) else {
            return vec!["could not close tmux-backed tab without a live window".to_string()];
        };
        let guard = crate::tmux::TmuxGtkApplyGuard::new(state, &window);
        let worker = crate::tmux::default_worker();
        let commands = tmux_backings
            .iter()
            .map(|backing| {
                crate::tmux::kill_session_command(&backing.target, &backing.session_name)
            })
            .collect();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let row = row.clone();
        glib::spawn_future_local(async move {
            let completion = worker
                .submit_coalesced(
                    crate::tmux::TmuxJobKey::CloseTab(tab_id),
                    commands,
                    std::time::Duration::from_secs(12),
                )
                .await;
            let Ok(completion) = completion else {
                crate::show_error_toast("Tab close failed: tmux cleanup worker unavailable");
                return;
            };
            let Some((state, _window)) = guard.upgrade(&completion) else {
                return;
            };
            if state.borrow().find_tab(tab_id).is_none() {
                return;
            }
            let kill_errors = crate::terminal::apply_tmux_kill_outcomes_before_removal(
                &state,
                &tmux_backings,
                completion.outcomes(),
                &workspace_name,
            );
            for error in &kill_errors {
                crate::show_error_toast(error);
            }
            finalize_tab_close(
                &tab_list,
                &state,
                &term_stack,
                tab_id,
                &row,
                workspace_id,
                workspace_tab_count,
                was_active,
                neighbor_id,
                !kill_errors.is_empty(),
            );
        });
        return Vec::new();
    }

    finalize_tab_close(
        tab_list,
        state,
        term_stack,
        tab_id,
        row,
        workspace_id,
        workspace_tab_count,
        was_active,
        neighbor_id,
        false,
    );
    Vec::new()
}

#[allow(clippy::too_many_arguments)]
fn finalize_tab_close(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_id: u32,
    row: &gtk::Box,
    workspace_id: u32,
    workspace_tab_count: usize,
    was_active: bool,
    neighbor_id: u32,
    refresh_background: bool,
) {
    let stack_name = tab_root_widget_name(tab_id);
    if let Some(child) = term_stack.child_by_name(&stack_name) {
        term_stack.remove(&child);
    }
    state.borrow_mut().remove_tab(tab_id);

    // Cancels per-row GLib sources and file monitors before the widget leaves
    // the tree, so no callback can race teardown against a stale row.
    let _ = remove_tab_row_handle(tab_list, tab_id);
    tab_list.remove(row);

    refresh_workspace_header_meta(tab_list, state, workspace_id);
    if refresh_background {
        if let Some(window) = widget_window(tab_list) {
            refresh_background_section(tab_list, state, term_stack, &window);
        }
    }

    if workspace_tab_count == 1 {
        remove_workspace_widgets(tab_list, workspace_id, &[tab_id]);
    }

    if was_active && activate_tab(tab_list, state, term_stack, neighbor_id) {
        if let Some(terminal) = crate::get_active_terminal(state) {
            terminal.grab_focus();
        }
    }
}

fn prompt_close_worktree_workspace(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    ws_id: u32,
    working_tree_path: &str,
    repo_root: Option<String>,
    detach_tab_id: Option<u32>,
) {
    let Some(root) = tab_list.root() else {
        return;
    };
    let Some(window) = root.downcast_ref::<adw::ApplicationWindow>() else {
        return;
    };

    let dialog = gtk::Dialog::builder()
        .transient_for(window)
        .modal(true)
        .title("Close Worktree Workspace")
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Keep Worktree", gtk::ResponseType::No);
    dialog.add_button("Remove Worktree", gtk::ResponseType::Yes);

    let content = dialog.content_area();
    content.set_spacing(8);
    let label = gtk::Label::new(Some("Also remove worktree?"));
    label.set_halign(gtk::Align::Start);
    let detail = gtk::Label::new(Some(working_tree_path));
    detail.set_halign(gtk::Align::Start);
    detail.add_css_class("tab-cwd");
    content.append(&label);
    content.append(&detail);

    let working_tree_path = working_tree_path.to_string();
    {
        let tab_list = tab_list.clone();
        let state = state.clone();
        let term_stack = term_stack.clone();
        let repo_root = repo_root.clone();
        dialog.connect_response(move |dialog, response| {
            match response {
                gtk::ResponseType::No => {
                    match register_worktree_detach_after_confirmation(
                        &state,
                        detach_tab_id,
                        response,
                    ) {
                        Ok(prepared) => {
                            if let (Some(prepared), Some(tab_id)) = (prepared, detach_tab_id) {
                                apply_prepared_sidebar_close(
                                    &tab_list,
                                    &state,
                                    &term_stack,
                                    tab_id,
                                    prepared,
                                );
                            } else {
                                let _ =
                                    close_workspace_by_id(&tab_list, &state, &term_stack, ws_id);
                            }
                        }
                        Err(err) => crate::show_error_toast(&format!("Detach failed: {err}")),
                    }
                }
                gtk::ResponseType::Yes => {
                    let request_key = format!("worktree-remove:{working_tree_path}");
                    let worktree_for_worker = working_tree_path.clone();
                    let repo_for_worker = repo_root.clone();
                    let state_for_apply = state.clone();
                    let tab_list_for_apply = tab_list.clone();
                    let term_stack_for_apply = term_stack.clone();
                    let submission = crate::git::spawn_async_result(
                        request_key,
                        move || {
                            crate::git::remove_worktree_from_repo(
                                repo_for_worker.as_deref().map(std::path::Path::new),
                                std::path::Path::new(&worktree_for_worker),
                            )
                        },
                        move |result| match result {
                            Ok(()) => match register_worktree_detach_after_confirmation(
                                &state_for_apply,
                                detach_tab_id,
                                response,
                            ) {
                                Ok(prepared) => {
                                    if let (Some(prepared), Some(tab_id)) =
                                        (prepared, detach_tab_id)
                                    {
                                        apply_prepared_sidebar_close(
                                            &tab_list_for_apply,
                                            &state_for_apply,
                                            &term_stack_for_apply,
                                            tab_id,
                                            prepared,
                                        );
                                    } else {
                                        let _ = close_workspace_by_id(
                                            &tab_list_for_apply,
                                            &state_for_apply,
                                            &term_stack_for_apply,
                                            ws_id,
                                        );
                                    }
                                }
                                Err(err) => {
                                    crate::show_error_toast(&format!("Detach failed: {err}"))
                                }
                            },
                            Err(error) => crate::show_error_toast(&format!(
                                "Could not remove worktree: {error}"
                            )),
                        },
                    );
                    match submission {
                        crate::git::GitAsyncSubmission::Started => {}
                        crate::git::GitAsyncSubmission::Coalesced => crate::show_toast(
                            "Worktree removal is already running; wait for its result",
                        ),
                        crate::git::GitAsyncSubmission::Saturated => crate::show_error_toast(
                            "Git worker queue is busy; could not remove worktree",
                        ),
                    }
                }
                _ => {
                    let _ = register_worktree_detach_after_confirmation(
                        &state,
                        detach_tab_id,
                        response,
                    );
                }
            }
            dialog.close();
        });
    }

    dialog.present();
}

pub fn close_tab_by_id(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_id: u32,
) -> Vec<String> {
    if let Some(row) = find_tab_row(tab_list, tab_id) {
        close_tab(tab_list, state, term_stack, tab_id, &row)
    } else {
        Vec::new()
    }
}

pub fn remove_tab_by_id_force(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_id: u32,
) {
    let (workspace_id, workspace_tab_count, workspace_count) = {
        let st = state.borrow();
        let Some((workspace, _)) = st.find_tab(tab_id) else {
            return;
        };
        (workspace.id, workspace.tabs.len(), st.workspaces.len())
    };

    let stack_name = tab_root_widget_name(tab_id);
    if let Some(child) = term_stack.child_by_name(&stack_name) {
        term_stack.remove(&child);
    }
    if let Some(handle) = remove_tab_row_handle(tab_list, tab_id) {
        if handle.row.parent().is_some() {
            tab_list.remove(&handle.row);
        }
    }

    state.borrow_mut().remove_tab(tab_id);

    if workspace_tab_count == 1 && workspace_count > 1 {
        remove_workspace_widgets(tab_list, workspace_id, &[tab_id]);
    } else {
        refresh_workspace_header_meta(tab_list, state, workspace_id);
    }
    sync_active_selection_ui(tab_list, state, term_stack);
}

pub fn close_workspace_by_id(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    ws_id: u32,
) -> Vec<String> {
    let (workspace_name, tab_ids, next_tab_id, tmux_backings) = {
        let st = state.borrow();
        let Some(workspace) = st.workspaces.iter().find(|ws| ws.id == ws_id) else {
            return Vec::new();
        };
        let tab_ids: Vec<u32> = workspace.tabs.iter().map(|tab| tab.id).collect();
        let next_tab_id = st
            .workspaces
            .iter()
            .filter(|ws| ws.id != ws_id)
            .flat_map(|ws| ws.tabs.iter())
            .map(|tab| tab.id)
            .next();
        let mut tmux_backings = workspace
            .tabs
            .iter()
            .flat_map(|tab| tab.panes.leaves())
            .filter_map(|leaf| leaf.tmux_backing.clone())
            .collect::<Vec<_>>();
        tmux_backings.extend(
            st.headless_panes
                .iter()
                .filter(|((tab_id, _), _)| tab_ids.contains(tab_id))
                .filter_map(|(_, pane)| pane.tmux_backing.clone()),
        );
        let mut seen = std::collections::HashSet::new();
        tmux_backings
            .retain(|backing| seen.insert((backing.target.clone(), backing.session_name.clone())));
        (workspace.name.clone(), tab_ids, next_tab_id, tmux_backings)
    };

    if !tmux_backings.is_empty() {
        let Some(window) = widget_window(tab_list) else {
            return vec!["could not close tmux-backed workspace without a live window".to_string()];
        };
        let guard = crate::tmux::TmuxGtkApplyGuard::new(state, &window);
        let worker = crate::tmux::default_worker();
        let commands = tmux_backings
            .iter()
            .map(|backing| {
                crate::tmux::kill_session_command(&backing.target, &backing.session_name)
            })
            .collect();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        glib::spawn_future_local(async move {
            let completion = worker
                .submit_coalesced(
                    crate::tmux::TmuxJobKey::CloseWorkspace(ws_id),
                    commands,
                    std::time::Duration::from_secs(12),
                )
                .await;
            let Ok(completion) = completion else {
                crate::show_error_toast("Workspace close failed: tmux cleanup worker unavailable");
                return;
            };
            let Some((state, _window)) = guard.upgrade(&completion) else {
                return;
            };
            if !state
                .borrow()
                .workspaces
                .iter()
                .any(|workspace| workspace.id == ws_id)
            {
                return;
            }
            let kill_errors = crate::terminal::apply_tmux_kill_outcomes_before_removal(
                &state,
                &tmux_backings,
                completion.outcomes(),
                &workspace_name,
            );
            for error in &kill_errors {
                crate::show_error_toast(error);
            }
            finalize_workspace_close(
                &tab_list,
                &state,
                &term_stack,
                ws_id,
                &tab_ids,
                next_tab_id,
                !kill_errors.is_empty(),
            );
        });
        return Vec::new();
    }

    finalize_workspace_close(
        tab_list,
        state,
        term_stack,
        ws_id,
        &tab_ids,
        next_tab_id,
        false,
    );
    Vec::new()
}

fn finalize_workspace_close(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    ws_id: u32,
    tab_ids: &[u32],
    next_tab_id: Option<u32>,
    refresh_background: bool,
) {
    for tab_id in tab_ids {
        let stack_name = tab_root_widget_name(*tab_id);
        if let Some(child) = term_stack.child_by_name(&stack_name) {
            term_stack.remove(&child);
        }
    }

    {
        let mut st = state.borrow_mut();
        st.remove_workspace(ws_id);
    }

    remove_workspace_widgets(tab_list, ws_id, tab_ids);
    if refresh_background {
        if let Some(window) = widget_window(tab_list) {
            refresh_background_section(tab_list, state, term_stack, &window);
        }
    }

    if let Some(next_tab_id) = next_tab_id {
        let _ = activate_tab(tab_list, state, term_stack, next_tab_id);
    } else {
        sync_active_selection_ui(tab_list, state, term_stack);
    }
}

fn remove_workspace_widgets(tab_list: &gtk::Box, ws_id: u32, tab_ids: &[u32]) {
    for tab_id in tab_ids {
        if let Some(handle) = remove_tab_row_handle(tab_list, *tab_id) {
            if handle.row.parent().is_some() {
                tab_list.remove(&handle.row);
            }
        }
    }

    if let Some(handle) = remove_workspace_row_handle(tab_list, ws_id) {
        if handle.header.parent().is_some() {
            tab_list.remove(&handle.header);
        }
    }
}

pub(crate) fn find_widget_by_name(root: &gtk::Widget, name: &str) -> Option<gtk::Widget> {
    if root.widget_name() == name {
        return Some(root.clone());
    }
    let mut child = root.first_child();
    while let Some(widget) = child {
        if let Some(found) = find_widget_by_name(&widget, name) {
            return Some(found);
        }
        child = widget.next_sibling();
    }
    None
}

fn next_tab_after_close(tab_ids: &[u32], active_tab: u32, closing_tab: u32) -> Option<u32> {
    if active_tab != closing_tab {
        return None;
    }
    let idx = tab_ids.iter().position(|id| *id == closing_tab)?;
    if idx + 1 < tab_ids.len() {
        Some(tab_ids[idx + 1])
    } else if idx > 0 {
        Some(tab_ids[idx - 1])
    } else {
        None
    }
}

// ── Drag-and-drop for tab reordering and cross-workspace moves ──

/// Attach a DragSource to a tab row so it can be dragged.
fn setup_tab_drag_source(row: &gtk::Box, tab_id: u32) {
    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gdk::DragAction::MOVE);
    // Use CAPTURE phase so the drag gesture tracks the pointer before
    // child GestureClick handlers (on btn_box) consume the sequence.
    drag_source.set_propagation_phase(gtk::PropagationPhase::Capture);

    drag_source.connect_prepare(move |_source, _x, _y| {
        let provider = gdk::ContentProvider::for_value(&tab_id.to_string().to_value());
        Some(provider)
    });

    // Make the row semi-transparent while being dragged
    let row_weak = row.downgrade();
    drag_source.connect_drag_begin(move |_source, _drag| {
        if let Some(row) = row_weak.upgrade() {
            row.add_css_class("tab-dragging");
        }
    });
    let row_weak = row.downgrade();
    drag_source.connect_drag_end(move |_source, _drag, _delete| {
        if let Some(row) = row_weak.upgrade() {
            row.remove_css_class("tab-dragging");
        }
    });

    row.add_controller(drag_source);
}

/// Attach a DropTarget to a tab row for intra/cross-workspace reordering.
/// Dropping onto the top half inserts before this tab; bottom half inserts after.
fn setup_tab_drop_target(
    row: &gtk::Box,
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    target_tab_id: u32,
) {
    let drop_target = gtk::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::MOVE);

    // Visual feedback: show a line above or below depending on cursor position
    let row_weak = row.downgrade();
    drop_target.connect_motion(move |_target, _x, y| {
        if let Some(row) = row_weak.upgrade() {
            let midpoint = row.height() as f64 / 2.0;
            if y < midpoint {
                row.add_css_class("drop-above");
                row.remove_css_class("drop-below");
            } else {
                row.add_css_class("drop-below");
                row.remove_css_class("drop-above");
            }
        }
        gdk::DragAction::MOVE
    });

    let row_weak = row.downgrade();
    drop_target.connect_leave(move |_target| {
        if let Some(row) = row_weak.upgrade() {
            row.remove_css_class("drop-above");
            row.remove_css_class("drop-below");
        }
    });

    let tab_list = tab_list.clone();
    let state = state.clone();
    let term_stack = term_stack.clone();
    let row_for_drop = row.downgrade();
    drop_target.connect_drop(move |_target, value, _x, y| {
        let Some(row_for_drop) = row_for_drop.upgrade() else {
            return false;
        };
        // Clean up visual indicators
        row_for_drop.remove_css_class("drop-above");
        row_for_drop.remove_css_class("drop-below");

        let text: String = match value.get() {
            Ok(t) => t,
            Err(_) => return false,
        };
        let dragged_tab_id: u32 = match text.parse() {
            Ok(id) => id,
            Err(_) => return false,
        };
        if dragged_tab_id == target_tab_id {
            return false;
        }

        let insert_before = y < row_for_drop.height() as f64 / 2.0;
        handle_tab_drop(
            &tab_list,
            &state,
            &term_stack,
            dragged_tab_id,
            target_tab_id,
            insert_before,
        )
    });

    row.add_controller(drop_target);
}

/// Attach a DropTarget to a workspace header for cross-workspace tab moves.
fn setup_workspace_header_drop_target(
    header: &gtk::Box,
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    ws_id: u32,
) {
    let drop_target = gtk::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::MOVE);

    let header_weak = header.downgrade();
    drop_target.connect_motion(move |_target, _x, _y| {
        if let Some(h) = header_weak.upgrade() {
            h.add_css_class("drop-target");
        }
        gdk::DragAction::MOVE
    });

    let header_weak = header.downgrade();
    drop_target.connect_leave(move |_target| {
        if let Some(h) = header_weak.upgrade() {
            h.remove_css_class("drop-target");
        }
    });

    let tab_list = tab_list.clone();
    let state = state.clone();
    let term_stack = term_stack.clone();
    let header_for_drop = header.clone();
    drop_target.connect_drop(move |_target, value, _x, _y| {
        header_for_drop.remove_css_class("drop-target");

        let text: String = match value.get() {
            Ok(t) => t,
            Err(_) => return false,
        };
        let tab_id: u32 = match text.parse() {
            Ok(id) => id,
            Err(_) => return false,
        };

        // Move tab to this workspace (appends to end)
        match move_tab_to_workspace(&tab_list, &state, &term_stack, tab_id, ws_id) {
            Ok(()) => true,
            Err(_) => false,
        }
    });

    header.add_controller(drop_target);
}

/// Execute a tab drop: reorder within workspace or move across workspaces.
fn handle_tab_drop(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    dragged_tab_id: u32,
    target_tab_id: u32,
    insert_before: bool,
) -> bool {
    let (dragged_ws_id, target_ws_id, target_index) = {
        let st = state.borrow();
        let dragged_ws = st.find_tab(dragged_tab_id).map(|(ws, _)| ws.id);
        let target_ws = st.find_tab(target_tab_id).map(|(ws, _)| ws.id);
        let target_idx = target_ws.and_then(|ws_id| {
            st.workspaces
                .iter()
                .find(|ws| ws.id == ws_id)
                .and_then(|ws| ws.tabs.iter().position(|t| t.id == target_tab_id))
        });
        (dragged_ws, target_ws, target_idx)
    };

    let (Some(dragged_ws_id), Some(target_ws_id), Some(target_index)) =
        (dragged_ws_id, target_ws_id, target_index)
    else {
        return false;
    };

    if dragged_ws_id == target_ws_id {
        // Same workspace → reorder
        let new_index = if insert_before {
            target_index
        } else {
            target_index + 1
        };
        // Adjust for removal: if dragged tab is before the target, removal shifts indices
        let dragged_index = {
            let st = state.borrow();
            st.workspaces
                .iter()
                .find(|ws| ws.id == dragged_ws_id)
                .and_then(|ws| ws.tabs.iter().position(|t| t.id == dragged_tab_id))
        };
        let Some(dragged_index) = dragged_index else {
            return false;
        };
        let adjusted_index = if dragged_index < new_index {
            new_index.saturating_sub(1)
        } else {
            new_index
        };

        if !state
            .borrow_mut()
            .reorder_tab(dragged_tab_id, adjusted_index)
        {
            return false;
        }

        // Move the widget in the sidebar to match
        if let Some(dragged_row) = find_tab_row(tab_list, dragged_tab_id) {
            if let Some(target_row) = find_tab_row(tab_list, target_tab_id) {
                tab_list.remove(&dragged_row);
                if insert_before {
                    tab_list.insert_child_after(&dragged_row, target_row.prev_sibling().as_ref());
                } else {
                    tab_list.insert_child_after(&dragged_row, Some(&target_row));
                }
            }
        }
    } else {
        // Different workspace → move tab then position it at the drop point
        if move_tab_to_workspace(tab_list, state, term_stack, dragged_tab_id, target_ws_id).is_err()
        {
            return false;
        }
        // Now reorder within the target workspace to the insertion point
        let new_index = {
            let st = state.borrow();
            let ws = st.workspaces.iter().find(|ws| ws.id == target_ws_id);
            ws.and_then(|ws| ws.tabs.iter().position(|t| t.id == target_tab_id))
                .map(|idx| if insert_before { idx } else { idx + 1 })
        };
        if let Some(new_index) = new_index {
            state.borrow_mut().reorder_tab(dragged_tab_id, new_index);
        }
        // Re-position the widget
        if let Some(dragged_row) = find_tab_row(tab_list, dragged_tab_id) {
            if let Some(target_row) = find_tab_row(tab_list, target_tab_id) {
                tab_list.remove(&dragged_row);
                if insert_before {
                    tab_list.insert_child_after(&dragged_row, target_row.prev_sibling().as_ref());
                } else {
                    tab_list.insert_child_after(&dragged_row, Some(&target_row));
                }
            }
        }
    }

    true
}

const BG_SECTION_NAME: &str = "background-section";
const BG_ROWS_NAME: &str = "background-rows";
const BG_CHEVRON_NAME: &str = "background-chevron";

fn background_chevron_widget_name() -> String {
    BG_CHEVRON_NAME.to_string()
}

/// Toggle collapse/expand for the BACKGROUND section.
fn toggle_background_section_collapse(tab_list: &gtk::Box, state: &Rc<RefCell<AppState>>) {
    let collapsed = {
        let mut st = state.borrow_mut();
        st.background_section_collapsed = !st.background_section_collapsed;
        st.background_section_collapsed
    };

    if let Some(handle) = background_section_handle(tab_list) {
        handle
            .chevron
            .set_text(if collapsed { "\u{25b8}" } else { "\u{25be}" });
        handle.rows_box.set_visible(!collapsed);
    }
}

/// Show context menu for a background session row.
#[allow(clippy::too_many_arguments)] // Menu builders keep their GTK dependencies explicit for local event wiring.
fn show_background_row_menu(
    row: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    tab_list: &gtk::Box,
    window: &adw::ApplicationWindow,
    session_name: &str,
    target: &crate::tmux::TmuxTarget,
    x: f64,
    y: f64,
) {
    if widget_window(row).is_none() {
        return;
    }

    let menu = gio::Menu::new();
    let actions = gio::Menu::new();
    actions.append(Some("Attach"), Some("bg.attach"));
    actions.append(Some("Kill"), Some("bg.kill"));
    actions.append(Some("Open Dashboard"), Some("bg.open-dashboard"));
    menu.append_section(None, &actions);

    let group = gio::SimpleActionGroup::new();

    // Attach action
    let attach_action = gio::SimpleAction::new("attach", None);
    {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        let window = window.clone();
        let session_name = session_name.to_string();
        let target = target.clone();
        attach_action.connect_activate(move |_, _| {
            crate::terminal::attach_session_async(
                &state,
                &term_stack,
                &tab_list,
                &window,
                &session_name,
                &target,
                |result| {
                    if let Err(err) = result {
                        crate::show_error_toast(&format!("Attach failed: {err}"));
                    }
                },
            );
        });
    }
    group.add_action(&attach_action);

    // Kill action
    let kill_action = gio::SimpleAction::new("kill", None);
    {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        let window = window.clone();
        let session_name = session_name.to_string();
        let target = target.clone();
        kill_action.connect_activate(move |_, _| {
            let state_for_apply = state.clone();
            let term_stack_for_apply = term_stack.clone();
            let tab_list_for_apply = tab_list.clone();
            let window_for_apply = window.clone();
            crate::terminal::kill_session_by_name_with_hint_async(
                &state,
                &window,
                &session_name,
                Some(&target),
                move |result| {
                    if let Err(error) = result {
                        crate::show_error_toast(&error);
                    }
                    refresh_background_section(
                        &tab_list_for_apply,
                        &state_for_apply,
                        &term_stack_for_apply,
                        &window_for_apply,
                    );
                    crate::dashboard::refresh_dashboard_if_open(
                        &state_for_apply,
                        &term_stack_for_apply,
                    );
                },
            );
        });
    }
    group.add_action(&kill_action);

    // Open Dashboard action
    let dashboard_action = gio::SimpleAction::new("open-dashboard", None);
    {
        let state = state.clone();
        let term_stack = term_stack.clone();
        let tab_list = tab_list.clone();
        let window = window.clone();
        dashboard_action.connect_activate(move |_, _| {
            // Use the shared open_dashboard function which handles both
            // focusing existing dashboard and creating new one
            crate::dashboard::open_dashboard(&state, &term_stack, &tab_list, &window);
        });
    }
    group.add_action(&dashboard_action);

    show_context_menu(row, &menu, "bg", &group, x, y);
}

#[derive(Clone, Debug)]
struct BackgroundSessionRow {
    session_name: String,
    display_name: String,
    host: String,
    finished: bool,
    target: crate::tmux::TmuxTarget,
}

fn background_session_rows(state: &AppState) -> Vec<BackgroundSessionRow> {
    state
        .detached_sessions
        .iter()
        .map(|session| BackgroundSessionRow {
            session_name: session.session_name.clone(),
            display_name: session
                .session_name
                .strip_prefix("taarof--")
                .unwrap_or(&session.session_name)
                .to_string(),
            host: session.host.clone(),
            finished: session.finished,
            target: session.target.clone(),
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn background_session_names(state: &AppState) -> Vec<String> {
    background_session_rows(state)
        .into_iter()
        .map(|row| row.session_name)
        .collect()
}

/// Build or refresh the BACKGROUND section in the sidebar.
/// Shows detached sessions with status icons. Only visible when non-empty.
/// Section is collapsible (default collapsed) and rows have context menu.
pub fn refresh_background_section(
    tab_list: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    if let Some(handle) = background_section_handle(tab_list) {
        if handle.section.parent().is_some() {
            tab_list.remove(&handle.section);
        }
        set_background_section_handle(tab_list, None);
    }

    let rows = background_session_rows(&state.borrow());
    if rows.is_empty() {
        return;
    }

    let collapsed = state.borrow().background_section_collapsed;

    let section = gtk::Box::new(gtk::Orientation::Vertical, 0);
    section.set_widget_name(BG_SECTION_NAME);
    section.add_css_class("background-section");
    section.add_css_class("compact-hidden");
    section.set_margin_top(8);

    // Header with chevron
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    header.set_margin_start(12);
    header.set_margin_end(8);
    header.set_margin_top(4);
    header.set_margin_bottom(4);
    header.add_css_class("sidebar-section-header");

    let chevron = gtk::Label::new(Some(if collapsed { "\u{25b8}" } else { "\u{25be}" }));
    chevron.set_widget_name(&background_chevron_widget_name());
    chevron.add_css_class("dim-label");

    let title = gtk::Label::new(Some(&format!("BACKGROUND ({})", rows.len())));
    title.add_css_class("dim-label");
    title.add_css_class("caption");
    title.set_halign(gtk::Align::Start);
    title.set_hexpand(true);

    header.append(&chevron);
    header.append(&title);
    section.append(&header);

    // Click header to toggle collapse
    {
        let tab_list = tab_list.clone();
        let state = state.clone();
        let gesture = gtk::GestureClick::new();
        gesture.set_button(1);
        gesture.connect_released(move |gesture, _n_press, _x, _y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            toggle_background_section_collapse(&tab_list, &state);
        });
        header.add_controller(gesture);
    }

    // Session rows container
    let rows_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    rows_box.set_widget_name(BG_ROWS_NAME);
    rows_box.set_visible(!collapsed);

    for model in &rows {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
        row.set_margin_start(16);
        row.set_margin_end(8);
        row.set_margin_top(2);
        row.set_margin_bottom(2);
        row.add_css_class("background-row");
        row.set_widget_name(&format!("bg-row-{}", model.session_name));

        let line1 = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let icon = if model.finished {
            "\u{2713}"
        } else {
            "\u{25CB}"
        };
        let icon_label = gtk::Label::new(Some(icon));
        icon_label.add_css_class(if model.finished {
            "dot-finished"
        } else {
            "dot-detached"
        });
        line1.append(&icon_label);

        let name_label = gtk::Label::new(Some(&model.display_name));
        name_label.add_css_class("dim-label");
        name_label.set_halign(gtk::Align::Start);
        name_label.set_hexpand(true);
        name_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line1.append(&name_label);
        row.append(&line1);

        let host_label = gtk::Label::new(Some(&model.host));
        host_label.add_css_class("dim-label");
        host_label.add_css_class("caption");
        host_label.set_halign(gtk::Align::Start);
        host_label.set_margin_start(22);
        row.append(&host_label);

        // Right-click context menu
        {
            let menu_row = row.clone();
            let state = state.clone();
            let term_stack = term_stack.clone();
            let tab_list = tab_list.clone();
            let window = window.clone();
            let sname = model.session_name.clone();
            let target = model.target.clone();
            let gesture = gtk::GestureClick::new();
            gesture.set_button(3);
            gesture.connect_released(move |gesture, _n_press, x, y| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                show_background_row_menu(
                    &menu_row,
                    &state,
                    &term_stack,
                    &tab_list,
                    &window,
                    &sname,
                    &target,
                    x,
                    y,
                );
            });
            row.add_controller(gesture);
        }

        rows_box.append(&row);
    }

    section.append(&rows_box);
    set_compact_hidden_visibility(section.upcast_ref(), compact_mode_active());
    tab_list.append(&section);
    set_background_section_handle(
        tab_list,
        Some(BackgroundSectionHandle {
            section,
            chevron,
            rows_box,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::{
        agent_children_visible, background_session_rows, clear_plan_monitor_slot,
        compact_visibility_override, defer_context_menu_cleanup, defer_user_tab_close,
        distinct_agent_badges, format_agent_names_label, format_tab_activity_summary,
        format_tab_agents_tooltip, next_tab_after_close,
        register_worktree_detach_after_confirmation, resolve_tab_move_dispatch,
        resolve_work_stream_filter_selection, runtime_probe_sidebar_ui, select_agent_pane_target,
        sidebar_width_for_mode, tab_alert_binding, tab_indicator_state, tab_matches_query,
        tab_move_menu_entries, tab_status_badge_text, work_stream_control_key,
        work_stream_is_at_bottom, work_stream_legend_actions, work_stream_legend_text,
        work_stream_legend_tooltip, work_stream_pane_metadata, work_stream_scroll_target,
        workspace_header_meta_snapshot, PlanMonitorState, SidebarViewModel, TabIndicatorState,
        WorkStreamControlRole, WorkStreamLegendActions, WorkStreamRestoreStep,
        TAB_MENU_ACTION_GROUP, WORK_STREAM_RESTORE_ORDER,
    };
    use crate::pane::PaneNode;
    use crate::workspace::{TabKind, TabPrimaryState};
    use crate::{AppState, Tab};
    use gio::prelude::*;
    use gtk::prelude::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn compact_sidebar_uses_minimal_width() {
        assert_eq!(sidebar_width_for_mode(false), 220);
        assert_eq!(sidebar_width_for_mode(true), 64);
    }

    fn dummy_terminal_tab(id: u32, name: &str) -> Tab {
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
        }
    }

    #[test]
    fn cancel_worktree_close_does_not_register_or_strip_detached_session() {
        let state = Rc::new(RefCell::new(AppState::new()));
        let workspace_id = state.borrow().active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state.borrow_mut(),
            workspace_id,
            "cancel-worktree-detach",
            crate::HeadlessPaneSeed {
                tmux_session: Some("cancel-must-preserve".to_string()),
                ..crate::HeadlessPaneSeed::default()
            },
        )
        .expect("headless tmux tab");
        {
            let mut st = state.borrow_mut();
            let workspace = st
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == workspace_id)
                .expect("workspace");
            workspace.is_worktree = true;
            workspace.working_tree_path = Some("/tmp/cancel-worktree-detach".to_string());
        }

        let result = register_worktree_detach_after_confirmation(
            &state,
            Some(tab_id),
            gtk::ResponseType::Cancel,
        )
        .expect("cancel response should be harmless");

        assert!(result.is_none());
        let st = state.borrow();
        assert!(st.detached_sessions.is_empty());
        assert!(st.find_tab(tab_id).is_some());
        assert_eq!(
            st.headless_pane(tab_id, pane_id)
                .and_then(|pane| pane.tmux_backing.as_ref())
                .map(|backing| backing.session_name.as_str()),
            Some("cancel-must-preserve")
        );
    }

    #[test]
    fn background_session_projection_preserves_count_and_target_identity() {
        let mut state = AppState::new();
        state.detached_sessions = vec![
            crate::dashboard::DetachedSession {
                session_name: "taarof--default--t3--0".to_string(),
                host: "localhost".to_string(),
                workspace: "default".to_string(),
                target: crate::tmux::TmuxTarget::Local,
                detached_at: Instant::now(),
                last_command: None,
                finished: false,
            },
            crate::dashboard::DetachedSession {
                session_name: "shared".to_string(),
                host: "builder@example".to_string(),
                workspace: "remote".to_string(),
                target: crate::tmux::TmuxTarget::Remote {
                    ssh_target: "builder@example".to_string(),
                },
                detached_at: Instant::now(),
                last_command: Some("cargo test".to_string()),
                finished: true,
            },
        ];

        let rows = background_session_rows(&state);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].session_name, "taarof--default--t3--0");
        assert_eq!(rows[0].display_name, "default--t3--0");
        assert_eq!(rows[0].target, crate::tmux::TmuxTarget::Local);
        assert_eq!(rows[1].session_name, "shared");
        assert_eq!(rows[1].host, "builder@example");
        assert!(rows[1].finished);
        assert_eq!(
            rows[1].target,
            crate::tmux::TmuxTarget::Remote {
                ssh_target: "builder@example".to_string()
            }
        );
    }

    #[test]
    fn work_stream_bottom_follow_uses_tolerance_and_follows_growth() {
        assert!(work_stream_is_at_bottom(699.0, 1_000.0, 300.0));
        assert_eq!(
            work_stream_scroll_target(true, 699.0, 1_120.0, 300.0),
            820.0
        );
    }

    #[test]
    fn work_stream_preserves_scrolled_up_position_across_growth_and_filter_shrink() {
        assert!(!work_stream_is_at_bottom(420.0, 1_000.0, 300.0));
        assert_eq!(
            work_stream_scroll_target(false, 420.0, 1_200.0, 300.0),
            420.0
        );
        assert_eq!(
            work_stream_scroll_target(false, 420.0, 650.0, 300.0),
            350.0,
            "a filter may clamp to its shorter range but must not jump to its bottom otherwise"
        );
    }

    #[test]
    fn work_stream_refresh_restores_focus_before_final_scroll_positions() {
        assert_eq!(
            WORK_STREAM_RESTORE_ORDER,
            [
                WorkStreamRestoreStep::Focus,
                WorkStreamRestoreStep::VerticalScroll,
                WorkStreamRestoreStep::HorizontalScroll,
            ]
        );
    }

    #[test]
    fn work_stream_keyboard_focus_keys_are_stable_and_keep_pr_actions_separate_from_context() {
        assert_eq!(
            work_stream_control_key(42, WorkStreamControlRole::Summary),
            "record:42:summary"
        );
        assert_eq!(
            work_stream_control_key(42, WorkStreamControlRole::Context),
            "record:42:context"
        );
        assert_ne!(
            work_stream_control_key(42, WorkStreamControlRole::Summary),
            work_stream_control_key(42, WorkStreamControlRole::Context)
        );
    }

    #[test]
    fn work_stream_filter_refresh_preserves_valid_selection_and_resets_removed_selection() {
        use crate::work_ledger::WorkStreamFilter;

        let options = vec![
            ("Live / open work".into(), WorkStreamFilter::All),
            ("Pane one".into(), WorkStreamFilter::Pane("pane-1".into())),
        ];
        assert_eq!(
            resolve_work_stream_filter_selection(
                &WorkStreamFilter::Pane("pane-1".into()),
                &options
            ),
            (WorkStreamFilter::Pane("pane-1".into()), 1)
        );
        assert_eq!(
            resolve_work_stream_filter_selection(
                &WorkStreamFilter::Pane("removed-pane".into()),
                &options
            ),
            (WorkStreamFilter::All, 0)
        );
    }

    #[test]
    fn session_work_identity_is_visible_and_keeps_full_accessible_tooltip() {
        let item = crate::work_ledger::WorkStreamLegendItem {
            pane_origin: "pane-origin-that-is-deliberately-long".into(),
            workspace_origin: "workspace-origin".into(),
            tab_origin: "tab-origin".into(),
            marker: "P1".into(),
            color_slot: Some(0),
            workspace_id: 1,
            tab_id: 2,
            pane_id: 7,
            workspace_name: "workspace-with-a-very-long-name".into(),
            tab_name: "tab-with-a-very-long-name".into(),
            agent_name: Some("agent-with-a-very-long-name".into()),
            task_id: Some("EXAMPLE-117".into()),
            task_title: Some("task-with-a-very-long-name".into()),
            origin_state: crate::work_ledger::WorkStreamOriginState::Live,
        };

        let visible = work_stream_legend_text(&item);
        assert!(visible.starts_with("P1 · Live"));
        assert!(visible.contains(&item.workspace_name));
        assert!(visible.contains(&item.tab_name));
        assert!(visible.contains("pane 7"));
        assert!(visible.contains(&item.pane_origin));
        assert!(visible.contains("agent-with-a-very-long-name"));
        assert!(visible.contains("EXAMPLE-117 — task-with-a-very-long-name"));

        let tooltip = work_stream_legend_tooltip(&item);
        assert!(tooltip.starts_with("P1 · "));
        assert!(tooltip.contains(&item.workspace_name));
        assert!(tooltip.contains(&item.tab_name));
        assert!(tooltip.contains(&item.pane_origin));
        assert!(tooltip.contains("agent-with-a-very-long-name"));
        assert!(tooltip.contains("EXAMPLE-117 — task-with-a-very-long-name"));
        assert!(tooltip.ends_with("Live"));
    }

    #[test]
    fn session_identity_click_focuses_every_pane_and_preserves_overflow_promotion() {
        assert_eq!(
            work_stream_legend_actions(Some(0)),
            WorkStreamLegendActions {
                focus_origin: true,
                promote_overflow: false,
            }
        );
        assert_eq!(
            work_stream_legend_actions(None),
            WorkStreamLegendActions {
                focus_origin: true,
                promote_overflow: true,
            }
        );
    }

    #[test]
    fn work_stream_palette_meets_text_contrast_and_keeps_non_color_markers() {
        fn channel(value: u8) -> f64 {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        fn luminance([red, green, blue]: [u8; 3]) -> f64 {
            0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)
        }
        fn contrast(foreground: [u8; 3], background: [u8; 3]) -> f64 {
            let foreground = luminance(foreground);
            let background = luminance(background);
            (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
        }

        let foreground = [0xf6, 0xf0, 0xe4];
        for background in [
            [0x14, 0x26, 0x38],
            [0x17, 0x2b, 0x1b],
            [0x30, 0x26, 0x12],
            [0x32, 0x19, 0x23],
            [0x27, 0x1b, 0x3d],
            [0x24, 0x1d, 0x13],
        ] {
            assert!(contrast(foreground, background) >= 4.5);
        }
        let css = include_str!("../resources/style.css");
        for marker in ["P1-P5/OVF", "work-stream-color-1", "work-stream-overflow"] {
            assert!(css.contains(marker));
        }
    }

    #[test]
    fn work_stream_metadata_classifies_live_lazy_and_historical_without_prose_inference() {
        let mut state = AppState::new();
        let workspace = state.active_workspace;
        let (live_tab, live_pane) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace,
            "live",
            crate::HeadlessPaneSeed::default(),
        )
        .unwrap();
        let lazy_tab = crate::seed_pending_restore_tab(
            &mut state,
            workspace,
            "lazy",
            crate::session::SavedPaneNode::Leaf {
                work_origin: Some("pane-lazy-work-stream".into()),
                cwd: Some("/tmp".into()),
                ssh_command: None,
                tmux_session: None,
                tmux_host: None,
                current_task: None,
                agent_session: None,
            },
            Some("/tmp".into()),
        )
        .unwrap();
        let live_tab_state = state.find_tab_mut(live_tab).unwrap();
        live_tab_state.agent_name = Some("codex".into());
        live_tab_state.agent_pane_id = Some(live_pane);
        live_tab_state.agent_running = false;
        let live_identity =
            crate::work_ledger::identity_for_pane(&state, live_tab, live_pane, None).unwrap();
        let lazy_identity =
            crate::work_ledger::identity_for_pane(&state, lazy_tab, 0, None).unwrap();
        let mut historical_identity = live_identity.clone();
        historical_identity.tab_origin = "tab-closed-work-stream".into();
        historical_identity.pane_origin = "pane-closed-work-stream".into();
        let record =
            |seq, identity: crate::work_ledger::WorkIdentity| crate::work_ledger::WorkRecord {
                seq,
                ts_unix_ms: seq,
                kind: crate::work_ledger::WorkKind::AssistantMessageCompleted,
                summary: "summary says codex but is not identity evidence".into(),
                identity,
                evidence_source: crate::work_ledger::EvidenceSource::AgentTranscript,
                authority: crate::work_ledger::WorkAuthority::AgentObservation,
                verification: crate::work_ledger::VerificationState::Observed,
                task_status: None,
                pull_request: None,
            };
        let records = vec![record(3, historical_identity.clone())];
        let metadata = work_stream_pane_metadata(&state, &records);

        assert_eq!(
            metadata[&live_identity.pane_origin].origin_state,
            crate::work_ledger::WorkStreamOriginState::Live
        );
        assert_eq!(
            metadata[&lazy_identity.pane_origin].origin_state,
            crate::work_ledger::WorkStreamOriginState::Lazy
        );
        assert_eq!(
            metadata[&historical_identity.pane_origin].origin_state,
            crate::work_ledger::WorkStreamOriginState::Historical
        );
        assert_eq!(
            metadata[&live_identity.pane_origin].agent_name.as_deref(),
            Some("codex")
        );
        assert!(metadata[&lazy_identity.pane_origin].agent_name.is_none());
        assert!(metadata[&historical_identity.pane_origin]
            .agent_name
            .is_none());
        let mut palette = crate::work_ledger::WorkStreamPalette::default();
        let projection = crate::work_ledger::project_work_stream(
            &records,
            &mut palette,
            &crate::work_ledger::WorkStreamFilter::All,
            &metadata,
        );
        assert_eq!(projection.legend.len(), 3);
        assert_eq!(projection.legend[0].pane_origin, live_identity.pane_origin);
        assert_eq!(projection.legend[1].pane_origin, lazy_identity.pane_origin);
        assert_eq!(
            projection.legend[2].pane_origin,
            historical_identity.pane_origin
        );
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("taarof-{name}-{}-{stamp}", std::process::id()))
    }

    #[test]
    fn test_next_tab_after_close_prefers_right_neighbor() {
        assert_eq!(next_tab_after_close(&[1, 2, 3], 2, 2), Some(3));
    }

    #[test]
    fn test_next_tab_after_close_falls_back_left_neighbor() {
        assert_eq!(next_tab_after_close(&[1, 2, 3], 3, 3), Some(2));
    }

    #[test]
    fn test_next_tab_after_close_ignores_inactive_close() {
        assert_eq!(next_tab_after_close(&[1, 2, 3], 1, 3), None);
    }

    #[test]
    fn tab_search_matches_query_by_name() {
        assert!(tab_matches_query("My Server", None, "serv"));
        assert!(tab_matches_query("My Server", None, "SERVER"));
        assert!(!tab_matches_query("My Server", None, "database"));
    }

    #[test]
    fn tab_search_matches_query_by_cwd_path_including_remote() {
        let remote = Some("file://devbox/tmp/user/proj");
        assert!(tab_matches_query("shell", remote, "devbox"));
        assert!(tab_matches_query("shell", remote, "/tmp/user"));
        assert!(tab_matches_query("shell", Some("/srv/api"), "srv"));
        assert!(!tab_matches_query("shell", Some("/srv/api"), "missing"));
    }

    #[test]
    fn tab_search_empty_query_matches_all_tabs() {
        assert!(tab_matches_query("anything", None, ""));
        assert!(tab_matches_query("anything", Some("/x"), "   "));
    }

    #[test]
    fn context_menu_cleanup_is_deferred_past_action_dispatch() {
        let _glib_guard = crate::glib_main_context_test_guard();
        let events = Rc::new(RefCell::new(Vec::new()));
        let events_for_cleanup = events.clone();

        defer_context_menu_cleanup(move || {
            events_for_cleanup.borrow_mut().push("cleanup");
        });
        events.borrow_mut().push("action");

        assert_eq!(&*events.borrow(), &["action"]);

        let context = glib::MainContext::default();
        while context.pending() {
            context.iteration(false);
        }

        assert_eq!(&*events.borrow(), &["action", "cleanup"]);
    }

    #[test]
    fn restored_background_tab_close_is_deferred_past_gtk_signal_dispatch() {
        let _glib_guard = crate::glib_main_context_test_guard();
        let events = Rc::new(RefCell::new(Vec::new()));
        let events_for_close = events.clone();

        defer_user_tab_close(move || {
            events_for_close.borrow_mut().push("close");
        });
        events.borrow_mut().push("clicked-returned");

        assert_eq!(&*events.borrow(), &["clicked-returned"]);

        let context = glib::MainContext::default();
        while context.pending() {
            context.iteration(false);
        }

        assert_eq!(&*events.borrow(), &["clicked-returned", "close"]);
    }

    #[test]
    #[ignore = "requires a GTK display; exercised by testing/kasm/test-restored-background-close.sh"]
    fn restored_background_tab_close_reclaims_row_without_materializing() {
        let _glib_guard = crate::glib_main_context_test_guard();
        gtk::init().expect("Kasm must provide a GTK display");
        let state = Rc::new(RefCell::new(AppState::new()));
        let workspace_id = state.borrow().active_workspace;
        let active_tab_id = 11;
        let restored_tab_id = 12;
        {
            let mut st = state.borrow_mut();
            let workspace = st.active_ws_mut().expect("default workspace should exist");
            workspace
                .tabs
                .push(dummy_terminal_tab(active_tab_id, "Active"));
            let mut restored = dummy_terminal_tab(restored_tab_id, "Restored");
            restored.panes = Box::new(PaneNode::Empty);
            workspace.tabs.push(restored);
            workspace.active_tab = active_tab_id;
            st.pending_tab_restores.insert(
                restored_tab_id,
                crate::session::PendingTabRestore {
                    saved: crate::session::SavedPaneNode::Leaf {
                        work_origin: None,
                        cwd: Some("/tmp".into()),
                        ssh_command: None,
                        tmux_session: None,
                        tmux_host: None,
                        current_task: None,
                        agent_session: None,
                    },
                    cwd: Some("/tmp".into()),
                    show_restore_legend: false,
                },
            );
        }

        let tab_list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let term_stack = gtk::Stack::new();
        super::add_tab_row(
            &tab_list,
            &state,
            &term_stack,
            active_tab_id,
            "Active",
            false,
        );
        super::add_tab_row(
            &tab_list,
            &state,
            &term_stack,
            restored_tab_id,
            "Restored",
            false,
        );
        let row = super::find_tab_row(&tab_list, restored_tab_id)
            .expect("restored row should be registered");
        let row_weak = row.downgrade();

        super::close_tab_by_id(&tab_list, &state, &term_stack, restored_tab_id);

        {
            let st = state.borrow();
            assert_eq!(
                st.active_ws().map(|workspace| workspace.active_tab),
                Some(active_tab_id),
                "closing a background restore must not activate its neighbour"
            );
            assert!(st.find_tab(restored_tab_id).is_none());
            assert!(!st.pending_tab_restores.contains_key(&restored_tab_id));
        }
        assert!(super::tab_row_handle(&tab_list, restored_tab_id).is_none());
        assert!(term_stack
            .child_by_name(&super::tab_root_widget_name(restored_tab_id))
            .is_none());

        drop(row);
        let context = glib::MainContext::default();
        while context.pending() {
            context.iteration(false);
        }
        assert!(
            row_weak.upgrade().is_none(),
            "row-owned GTK callbacks must not keep a closed restored row alive"
        );
        assert_eq!(state.borrow().active_workspace, workspace_id);
    }

    #[test]
    #[ignore = "requires a GTK display; exercised by testing/kasm/test-sidebar-width.sh"]
    fn badge_rich_sidebar_stays_inside_requested_width() {
        let _glib_guard = crate::glib_main_context_test_guard();
        gtk::init().expect("Kasm must provide a GTK display");
        crate::load_css();

        let state = Rc::new(RefCell::new(AppState::new()));
        let tab_id = 6;
        state
            .borrow_mut()
            .active_ws_mut()
            .expect("default workspace should exist")
            .tabs
            .push(dummy_terminal_tab(tab_id, "Multi-Agent"));

        let sidebar = super::build_sidebar();
        super::apply_compact_mode(&sidebar.container, false);
        let term_stack = gtk::Stack::new();
        super::add_tab_row(
            &sidebar.tab_list,
            &state,
            &term_stack,
            tab_id,
            "Multi-Agent",
            false,
        );
        let handle = super::tab_row_handle(&sidebar.tab_list, tab_id)
            .expect("badge-rich tab row should be registered");
        handle.agent_dot.set_text("●");
        handle.agent_dot.set_visible(true);
        handle.status_badge.set_text("ALERT");
        handle.status_badge.set_visible(true);
        super::refresh_agent_badges(
            &handle.agent_badges,
            &[
                crate::agents::agent_badge("codex"),
                crate::agents::agent_badge("claude"),
            ],
        );
        super::set_agent_tab_expanded(&sidebar.tab_list, tab_id, true);
        super::refresh_agent_child_rows(
            &sidebar.tab_list,
            &state,
            &handle,
            &[
                crate::agent_projection::AgentPaneProjection {
                    workspace_name: "default".to_string(),
                    tab_id,
                    pane_id: 0,
                    title: "Multi-Agent".to_string(),
                    agent_label: "codex #1".to_string(),
                    badge: crate::agents::agent_badge("codex"),
                    context: "default".to_string(),
                    activity: "review requested".to_string(),
                    state: crate::agents::AgentLifecycle::WaitingInput,
                    children: Vec::new(),
                },
                crate::agent_projection::AgentPaneProjection {
                    workspace_name: "default".to_string(),
                    tab_id,
                    pane_id: 1,
                    title: "Multi-Agent".to_string(),
                    agent_label: "claude".to_string(),
                    badge: crate::agents::agent_badge("claude"),
                    context: "default".to_string(),
                    activity: "tests failed".to_string(),
                    state: crate::agents::AgentLifecycle::Errored,
                    children: Vec::new(),
                },
            ],
        );

        let main_content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        main_content.set_hexpand(true);
        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        crate::configure_sidebar_layout(&paned, &sidebar.container, &main_content);
        let window = gtk::Window::builder()
            .default_width(1024)
            .default_height(700)
            .child(&paned)
            .build();
        window.present();

        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(2);
        while paned.width() == 0 && Instant::now() < deadline {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let bounds = sidebar
            .container
            .compute_bounds(&paned)
            .expect("realized sidebar should have paned-relative bounds");
        eprintln!(
            "EXAMPLE-157 allocation probe: divider={} sidebar_bounds={bounds:?}",
            paned.position()
        );

        assert_eq!(paned.position(), 220, "the divider must stay at 220px");
        assert_eq!(
            handle.name_label.ellipsize(),
            gtk::pango::EllipsizeMode::End
        );
        assert_eq!(
            handle.branch_label.ellipsize(),
            gtk::pango::EllipsizeMode::End
        );
        assert_eq!(
            handle.activity_label.ellipsize(),
            gtk::pango::EllipsizeMode::End
        );
        assert!(
            bounds.x() == 0.0 && bounds.width() == 220.0,
            "badge-rich rows must not force the whole sidebar outside its 220px allocation: {bounds:?}"
        );
        window.close();
    }

    #[test]
    fn move_tab_context_menu_builds_enabled_actions_for_other_workspaces() {
        let workspaces = vec![
            (1, "Alpha".to_string()),
            (2, "Beta".to_string()),
            (3, "Gamma".to_string()),
        ];

        let entries = tab_move_menu_entries(42, 2, &workspaces);

        assert_eq!(entries.len(), 2, "current workspace must be excluded");
        assert!(entries.iter().all(|entry| entry.target_ws_id != 2));
        for entry in &entries {
            assert_eq!(entry.tab_id, 42, "entries bind the right-clicked tab");
            assert_eq!(
                entry.detailed_action_name,
                format!("{TAB_MENU_ACTION_GROUP}.{}", entry.action_name),
                "menu item and registered action must never drift"
            );
        }
        assert_eq!(entries[0].label, "Move to Alpha");
        assert_eq!(entries[0].action_name, "move-to-1");
        assert_eq!(entries[0].detailed_action_name, "tab.move-to-1");
        assert_eq!(entries[1].detailed_action_name, "tab.move-to-3");
    }

    #[test]
    fn move_tab_context_menu_registered_actions_match_menu_items() {
        let workspaces = vec![
            (1, "A".to_string()),
            (2, "B".to_string()),
            (3, "C".to_string()),
        ];
        let entries = tab_move_menu_entries(5, 1, &workspaces);

        let group = gio::SimpleActionGroup::new();
        for entry in &entries {
            group.add_action(&gio::SimpleAction::new(&entry.action_name, None));
        }

        for entry in &entries {
            let (prefix, name) = entry
                .detailed_action_name
                .split_once('.')
                .expect("detailed action name must carry a group prefix");
            assert_eq!(prefix, TAB_MENU_ACTION_GROUP);
            assert!(
                group.has_action(name),
                "menu item {} must resolve to a registered action",
                entry.detailed_action_name
            );
        }
    }

    #[test]
    fn move_tab_context_menu_dispatches_right_clicked_tab() {
        // Tab 7 is the right-clicked row in workspace 1; whichever tab is
        // "active" plays no part in building or resolving the dispatch.
        let workspaces = vec![(1, "One".to_string()), (2, "Two".to_string())];
        let entries = tab_move_menu_entries(7, 1, &workspaces);

        assert_eq!(
            resolve_tab_move_dispatch(&entries, "move-to-2"),
            Some((7, 2))
        );
        // The current workspace is never offered as a target.
        assert_eq!(resolve_tab_move_dispatch(&entries, "move-to-1"), None);
        // Unknown action names stay inert instead of moving the wrong tab.
        assert_eq!(resolve_tab_move_dispatch(&entries, "move-to-99"), None);
    }

    #[test]
    fn single_agent_child_visibility_shows_row_without_disclosure() {
        assert_eq!(compact_visibility_override(true, true), Some(false));
        assert_eq!(compact_visibility_override(false, true), None);
        assert_eq!(compact_visibility_override(false, false), Some(true));
        assert!(!agent_children_visible(0, true, false));
        assert!(agent_children_visible(1, false, false));
        assert!(agent_children_visible(1, true, false));
        assert!(!agent_children_visible(3, false, false));
        assert!(!agent_children_visible(1, false, true));
        assert!(!agent_children_visible(3, true, true));
        assert!(agent_children_visible(3, true, false));
    }

    #[test]
    fn sidebar_view_model_tracks_session_and_row_ids() {
        let mut model = SidebarViewModel::default();
        model.install_session_label();
        model.register_workspace(7);
        model.register_tab(11);
        model.set_agent_tab_expanded(11, true);
        model.register_tab(11); // A refresh/re-registration preserves expansion.
        model.set_background_section(true);

        assert!(model.session_label_installed);
        assert_eq!(
            model.workspace_ids.iter().copied().collect::<Vec<_>>(),
            vec![7]
        );
        assert_eq!(model.tab_ids.iter().copied().collect::<Vec<_>>(), vec![11]);
        assert!(model.agent_tab_expanded(11));
        assert!(model.background_section_installed);
    }

    #[test]
    fn agent_child_target_selects_the_exact_real_pane() {
        let mut state = AppState::new();
        let workspace_id = state.active_workspace;
        let (tab_id, pane_id) = crate::seed_headless_terminal_tab(
            &mut state,
            workspace_id,
            "Agent tab",
            crate::HeadlessPaneSeed::default(),
        )
        .expect("pane should seed");
        state
            .find_tab_mut(tab_id)
            .expect("tab should exist")
            .focused_pane_id = pane_id.saturating_add(99);

        assert!(select_agent_pane_target(&mut state, tab_id, pane_id));
        assert_eq!(
            state
                .find_tab(tab_id)
                .expect("tab should exist")
                .1
                .focused_pane_id,
            pane_id
        );
        assert!(!select_agent_pane_target(&mut state, tab_id, pane_id + 1));
    }

    #[test]
    fn workspace_header_meta_snapshot_reflects_workspace_state() {
        let state = Rc::new(RefCell::new(AppState::new()));
        let ws_id = state.borrow().active_workspace;
        {
            let mut st = state.borrow_mut();
            let ws = st.active_ws_mut().expect("default workspace should exist");
            ws.is_worktree = true;
            ws.tabs.push(dummy_terminal_tab(11, "one"));
            ws.tabs.push(dummy_terminal_tab(12, "two"));
        }

        let meta = workspace_header_meta_snapshot(&state, ws_id);

        assert_eq!(meta.tab_count_text, "2");
        assert!(meta.show_worktree_badge);
    }

    #[test]
    fn workspace_header_meta_snapshot_includes_tool_version_chip() {
        let _guard = crate::mise::task_discovery_test_guard();
        crate::mise::clear_tool_version_cache_for_test();
        let root = unique_test_dir("workspace-tool-version-chip");
        fs::create_dir_all(&root).expect("test workspace dir should exist");
        fs::write(root.join(".mise.toml"), "[tools]\nnode = \"22.1.0\"\n")
            .expect("test mise config should be written");
        crate::mise::install_tool_version_test_probe(Some(vec![
            crate::mise::tool_version_test_entry("node", "22.1.0", &root),
            crate::mise::tool_version_test_entry("rust", "1.81.0", &root),
        ]));

        let state = Rc::new(RefCell::new(AppState::new()));
        let ws_id = state.borrow().active_workspace;
        {
            let mut st = state.borrow_mut();
            let ws = st.active_ws_mut().expect("default workspace should exist");
            ws.repo_root = Some(root.to_string_lossy().into_owned());
        }

        // Tool-version discovery is now non-blocking; warm the cache so the
        // snapshot sees the discovered chip.
        crate::mise::wait_for_tool_version_chip(&crate::mise::DiscoveryTarget::Local {
            cwd: root.to_string_lossy().into_owned(),
            binary_path: None,
        });

        let meta = workspace_header_meta_snapshot(&state, ws_id);

        assert_eq!(
            meta.tool_version_chip_text.as_deref(),
            Some("node 22.1 • rust 1.81")
        );
        crate::mise::clear_tool_version_test_probe();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sidebar_view_model_teardown_clears_registered_rows() {
        let mut model = SidebarViewModel::default();
        model.install_session_label();
        model.register_workspace(5);
        model.register_tab(11);
        model.register_tab(12);
        model.set_agent_tab_expanded(11, true);
        model.set_background_section(true);

        model.remove_tab(11);
        model.remove_workspace(5);

        assert_eq!(
            model.workspace_ids.iter().copied().collect::<Vec<_>>(),
            Vec::<u32>::new()
        );
        assert_eq!(model.tab_ids.iter().copied().collect::<Vec<_>>(), vec![12]);
        assert!(!model.agent_tab_expansion.contains_key(&11));

        model.teardown();

        assert_eq!(model, SidebarViewModel::default());
    }

    #[test]
    fn clear_plan_monitor_slot_cancels_pending_refresh() {
        let _glib_guard = crate::glib_main_context_test_guard();
        let dir = unique_test_dir("sidebar-monitor");
        fs::create_dir_all(&dir).expect("test monitor dir should be created");
        let monitor = gio::File::for_path(&dir)
            .monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
            .expect("monitor should be created");
        let refresh_source = Rc::new(RefCell::new(None));
        let refresh_source_for_timeout = refresh_source.clone();
        let source = glib::timeout_add_local_once(Duration::from_secs(60), move || {
            refresh_source_for_timeout.borrow_mut().take();
        });
        refresh_source.borrow_mut().replace(source);
        let plan_monitor = Rc::new(RefCell::new(Some(PlanMonitorState {
            _monitor: monitor,
            pending: Rc::new(Cell::new(true)),
            refresh_source: refresh_source.clone(),
        })));

        clear_plan_monitor_slot(&plan_monitor);

        assert!(plan_monitor.borrow().is_none());
        assert!(refresh_source.borrow().is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tab_indicator_state_maps_alert_primary_state() {
        assert_eq!(
            tab_indicator_state(TabPrimaryState::Alert),
            TabIndicatorState::Alert
        );
        assert_eq!(
            tab_status_badge_text(TabIndicatorState::Alert),
            Some("ALERT")
        );
    }

    #[test]
    fn tab_row_binding_projects_persistent_socket_notification() {
        let mut state = AppState::new();
        state.workspaces[0]
            .tabs
            .push(dummy_terminal_tab(41, "Build"));
        state.set_socket_notification(41, "Build done".to_string());
        let tab = state.find_tab(41).expect("tab").1;

        let binding = tab_alert_binding(&state, tab);

        assert!(binding.needs_attention);
        assert_eq!(binding.primary_state, TabPrimaryState::Alert);
        assert_eq!(binding.notification_text.as_deref(), Some("Build done"));
    }

    #[test]
    fn tab_indicator_state_maps_running_primary_state() {
        assert_eq!(
            tab_indicator_state(TabPrimaryState::Running),
            TabIndicatorState::Running
        );
    }

    #[test]
    fn format_tab_activity_summary_prefixes_running_and_done() {
        assert_eq!(
            format_tab_activity_summary(TabIndicatorState::Running, Some("indexing repo"), None),
            Some("Running: indexing repo".to_string())
        );
        assert_eq!(
            format_tab_activity_summary(TabIndicatorState::Done, Some("Build done"), None),
            Some("Done: Build done".to_string())
        );
    }

    #[test]
    fn format_tab_activity_summary_uses_notification_for_alerts() {
        assert_eq!(
            format_tab_activity_summary(
                TabIndicatorState::Alert,
                None,
                Some("Tests finished in another tab"),
            ),
            Some("Alert: Tests finished in another tab".to_string())
        );
    }

    #[test]
    fn format_agent_names_label_counts_duplicates_and_skips_single() {
        assert_eq!(format_agent_names_label(&[]), None);
        assert_eq!(format_agent_names_label(&["claude".to_string()]), None);
        assert_eq!(
            format_agent_names_label(&[
                "claude".to_string(),
                "claude".to_string(),
                "codex".to_string(),
            ]),
            Some("claude ×2 · codex".to_string())
        );
    }

    #[test]
    fn distinct_agent_badges_dedupes_by_kind_and_resolves_metadata() {
        // No agents -> no chips.
        assert!(distinct_agent_badges(&[]).is_empty());

        // Same-kind agents collapse to one chip; order is first-seen.
        let badges = distinct_agent_badges(&[
            "codex".to_string(),
            "codex".to_string(),
            "claude".to_string(),
        ]);
        assert_eq!(badges.len(), 2);
        assert_eq!(badges[0].name, "codex");
        assert_eq!(badges[0].short_label, "CDX");
        assert_eq!(badges[0].color_token, "agent-codex");
        assert_eq!(badges[1].name, "claude");
        assert_eq!(badges[1].short_label, "CLD");

        // Unknown kinds degrade to a generic chip carrying the source label.
        let unknown = distinct_agent_badges(&["mysterybot".to_string()]);
        assert_eq!(unknown.len(), 1);
        assert!(!unknown[0].known);
        assert_eq!(unknown[0].color_token, "agent-generic");
    }

    #[test]
    fn format_tab_agents_tooltip_preserves_per_pane_detail() {
        let pane = |label: &str, pane_id, state: crate::agents::AgentLifecycle, activity: &str| {
            crate::agent_projection::AgentPaneProjection {
                workspace_name: "default".into(),
                tab_id: 1,
                pane_id,
                title: "Implementation".into(),
                agent_label: label.into(),
                badge: crate::agents::agent_badge(label),
                context: "default".into(),
                activity: activity.into(),
                state,
                children: Vec::new(),
            }
        };
        // Zero/one agent: no tooltip (single-agent tabs stay clean).
        assert_eq!(format_tab_agents_tooltip(&[]), None);
        assert_eq!(
            format_tab_agents_tooltip(&[pane(
                "codex",
                4,
                crate::agents::AgentLifecycle::Working,
                "editing"
            )]),
            None
        );
        // Two same-kind codex + one claude: aggregate label lives elsewhere;
        // this detail keeps each pane, its disambiguated label, and its state.
        assert_eq!(
            format_tab_agents_tooltip(&[
                pane(
                    "codex #1",
                    4,
                    crate::agents::AgentLifecycle::Working,
                    "editing parser"
                ),
                pane(
                    "codex #2",
                    5,
                    crate::agents::AgentLifecycle::WaitingInput,
                    "waiting for input"
                ),
                pane(
                    "claude",
                    6,
                    crate::agents::AgentLifecycle::Idle,
                    ""
                ),
            ]),
            Some(
                "codex #1 (pane 4): working · editing parser · codex #2 (pane 5): waiting · waiting for input · claude (pane 6): idle"
                    .to_string()
            )
        );
    }

    #[test]
    fn runtime_probe_failure_sidebar_labels_partial_truth_and_age() {
        let truth = crate::runtime_probe::RuntimeProbeTruth {
            state: crate::runtime_probe::RuntimeProbeState::Partial,
            process_state: crate::probe::ProbeState::Stale,
            ports_state: crate::probe::ProbeState::Ok,
            process_observed_at_unix_ms: Some(1_000),
            ports_observed_at_unix_ms: Some(4_000),
            process_age_ms: Some(4_000),
            ports_age_ms: Some(1_000),
            process_error: Some("process source unreadable".to_string()),
            ports_error: None,
            checked_at_unix_ms: Some(5_000),
        };

        let ui = runtime_probe_sidebar_ui(&truth).expect("degraded probe indicator");

        assert_eq!(ui.label, "probe partial");
        assert!(ui.tooltip.contains("Process: stale · last known 4s ago"));
        assert!(ui.tooltip.contains("process source unreadable"));
        assert!(ui.tooltip.contains("Ports: ok · observed 1s ago"));

        let port_partial = runtime_probe_sidebar_ui(&crate::runtime_probe::RuntimeProbeTruth {
            state: crate::runtime_probe::RuntimeProbeState::Partial,
            process_state: crate::probe::ProbeState::Ok,
            ports_state: crate::probe::ProbeState::Stale,
            process_observed_at_unix_ms: Some(5_000),
            ports_observed_at_unix_ms: Some(1_000),
            process_age_ms: Some(0),
            ports_age_ms: Some(4_000),
            process_error: None,
            ports_error: Some("3 of 4 /proc/net tables unreadable".to_string()),
            checked_at_unix_ms: Some(5_000),
        })
        .expect("partial port truth should render independently");
        assert_eq!(port_partial.label, "probe partial");
        assert!(port_partial
            .tooltip
            .contains("Process: ok · observed 0s ago"));
        assert!(port_partial
            .tooltip
            .contains("Ports: stale · last known 4s ago"));
        assert!(port_partial
            .tooltip
            .contains("3 of 4 /proc/net tables unreadable"));

        let absent = runtime_probe_sidebar_ui(&crate::runtime_probe::RuntimeProbeTruth {
            state: crate::runtime_probe::RuntimeProbeState::Absent,
            process_state: crate::probe::ProbeState::Unknown,
            ports_state: crate::probe::ProbeState::Unknown,
            process_observed_at_unix_ms: None,
            ports_observed_at_unix_ms: None,
            process_age_ms: None,
            ports_age_ms: None,
            process_error: None,
            ports_error: None,
            checked_at_unix_ms: None,
        })
        .expect("startup absence should be distinguishable from healthy truth");
        assert_eq!(absent.label, "probe absent");
        assert!(absent.tooltip.contains("Process: unknown"));
    }

    /// EXAMPLE-61 regression proof: the full right-click "Move to Workspace"
    /// outcome, asserted end-to-end in one named test against a realistic
    /// starting layout (>= 2 workspaces, >= 2 tabs).
    ///
    /// EXAMPLE-59/60 already cover the pieces separately: context-menu action
    /// wiring (`move_tab_context_menu_*`), the state relocation semantics
    /// (`move_tab_*` in lib.rs), and session persistence of the post-move
    /// layout (`move_tab_workspace_state_survives_session_restore` in
    /// runtime_smoke.rs). This test is the single regression anchor that ties
    /// the acceptance criteria together so the workflow cannot silently break
    /// one assertion at a time:
    ///
    ///   * the moved row LEAVES the source workspace,
    ///   * the moved row APPEARS under the target workspace,
    ///   * the source keeps its other tab (it is not emptied/deleted),
    ///   * active workspace and active tab both FOLLOW the move to the target.
    ///
    /// The GTK-level `move_tab_to_workspace` wrapper is not exercised here
    /// because it drives real GTK widgets and cannot run headlessly; the
    /// containerized visual smoke and `testing/smoke-move-tab-to-workspace.sh`
    /// cover that surface. This asserts the relocation truth the wrapper
    /// delegates to.
    ///
    /// Discover with:
    ///   cargo test --manifest-path taarof-app/Cargo.toml right_click_move_tab_regression
    #[test]
    fn right_click_move_tab_regression_moves_visible_row_between_workspaces() {
        let mut state = AppState::new();

        // Source workspace: two tabs so the source is not emptied by the move.
        let source_ws = state.workspaces[0].id;
        let keep_tab = dummy_terminal_tab(101, "STAY");
        let keep_tab_id = keep_tab.id;
        let moving_tab = dummy_terminal_tab(102, "MOVE-ME");
        let moving_tab_id = moving_tab.id;
        state.workspaces[0].tabs.push(keep_tab);
        state.workspaces[0].tabs.push(moving_tab);
        state.workspaces[0].active_tab = moving_tab_id;

        // A second workspace with its own tab is the move target.
        let target_ws = state.create_workspace("target", None);
        let target_seed = dummy_terminal_tab(201, "target-shell");
        let target_seed_id = target_seed.id;
        let target = state
            .workspaces
            .iter_mut()
            .find(|ws| ws.id == target_ws)
            .expect("target workspace should exist");
        target.tabs.push(target_seed);
        target.active_tab = target_seed_id;

        // Start focused on the source so the move has somewhere to move *from*.
        state.active_workspace = source_ws;
        state.reset_navigation_history();

        assert!(
            state
                .move_tab_to_workspace(moving_tab_id, target_ws)
                .is_ok(),
            "move_tab_to_workspace should succeed for a valid cross-workspace move"
        );

        let source = state
            .workspaces
            .iter()
            .find(|ws| ws.id == source_ws)
            .expect("source workspace still exists");
        let target = state
            .workspaces
            .iter()
            .find(|ws| ws.id == target_ws)
            .expect("target workspace still exists");

        // The moved row left the source, and the source kept its other tab.
        assert!(
            !source.tabs.iter().any(|tab| tab.id == moving_tab_id),
            "moved tab must leave the source workspace"
        );
        assert!(
            source.tabs.iter().any(|tab| tab.id == keep_tab_id),
            "source workspace must keep its other tab (not be emptied)"
        );

        // The moved row appears under the target workspace.
        assert!(
            target.tabs.iter().any(|tab| tab.id == moving_tab_id),
            "moved tab must appear under the target workspace"
        );

        // Active workspace and active tab both follow the move to the target.
        assert_eq!(
            state.active_workspace, target_ws,
            "active workspace must follow the moved tab to the target"
        );
        assert_eq!(
            target.active_tab, moving_tab_id,
            "target workspace active tab must be the moved tab"
        );
    }
}
