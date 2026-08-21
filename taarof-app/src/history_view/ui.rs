//! Native GTK History overlay. SQLite work always crosses `gio::spawn_blocking`;
//! GTK only owns filter capture, generation checks, and rendering.

use adw::prelude::*;
use glib::variant::ToVariant;
use std::cell::RefCell;
use std::rc::Rc;

use crate::history::{HistoryFilters, HistoryOrder, HistoryQueryToken};
use crate::history_view::view_model::{
    export_page, HistoryRow, HistoryViewModel, LivePane, LivePullRequest, LiveSnapshot, RenderState,
};
use crate::AppState;

#[derive(Clone)]
pub struct HistoryView {
    pub container: gtk::CenterBox,
    search: gtk::SearchEntry,
    from_ts: gtk::Entry,
    to_ts: gtk::Entry,
    record_type: gtk::DropDown,
    session: gtk::Entry,
    workspace: gtk::Entry,
    pane: gtk::Entry,
    task: gtk::Entry,
    repository: gtk::Entry,
    authority: gtk::Entry,
    verification: gtk::Entry,
    severity: gtk::DropDown,
    order: gtk::DropDown,
    status: gtk::Label,
    list: gtk::ListBox,
    rendered_rows: Rc<RefCell<usize>>,
    load_more: gtk::Button,
    export: gtk::Button,
    model: Rc<RefCell<HistoryViewModel>>,
    active_token: Rc<RefCell<Option<HistoryQueryToken>>>,
    state: Rc<RefCell<AppState>>,
    tab_list: gtk::Box,
    window: adw::ApplicationWindow,
}

impl HistoryView {
    pub fn show(&self) {
        self.container.set_visible(true);
        self.search.grab_focus();
        self.run_query(false);
    }

    pub fn hide(&self) {
        if let Some(token) = self.active_token.borrow_mut().take() {
            token.cancel();
        }
        self.container.set_visible(false);
        if let Some(terminal) = crate::get_active_terminal(&self.state) {
            terminal.grab_focus();
        }
    }

    fn filters(&self) -> Result<HistoryFilters, String> {
        Ok(HistoryFilters {
            from_ts: optional_u64(&self.from_ts, "From time")?,
            to_ts: optional_u64(&self.to_ts, "To time")?,
            record_type: dropdown_filter(&self.record_type, &["", "event", "diagnostic", "work"]),
            session: entry_filter(&self.session),
            workspace: entry_filter(&self.workspace),
            pane: entry_filter(&self.pane),
            task: entry_filter(&self.task),
            repository: entry_filter(&self.repository),
            authority: entry_filter(&self.authority),
            verification: entry_filter(&self.verification),
            severity: dropdown_filter(&self.severity, &["", "info", "warn", "error"]),
            text: {
                let value = self.search.text();
                let value = value.trim();
                (!value.is_empty()).then(|| value.to_string())
            },
            order: if self.order.selected() == 0 {
                HistoryOrder::Desc
            } else {
                HistoryOrder::Asc
            },
            scan_budget: None,
        })
    }

    fn run_query(&self, append: bool) {
        if let Some(token) = self.active_token.borrow_mut().take() {
            token.cancel();
        }
        let generation = self.model.borrow_mut().begin_query(append);
        let status = self.state.borrow().history.status();
        if !status.available {
            let reason = status
                .reason
                .unwrap_or_else(|| format!("History storage is {}", status.state));
            self.model.borrow_mut().fail(generation, reason, true);
            self.render();
            return;
        }
        let filters = match self.filters() {
            Ok(filters) => filters,
            Err(error) => {
                self.model.borrow_mut().fail(generation, error, false);
                self.render();
                return;
            }
        };
        let since_id = append.then(|| self.model.borrow().next_id).flatten();
        let token = HistoryQueryToken::new();
        *self.active_token.borrow_mut() = Some(token.clone());
        self.render();

        let reader = self.state.borrow().history_reader.clone();
        let view = self.clone();
        glib::spawn_future_local(async move {
            let worker_token = token.clone();
            let result = gio::spawn_blocking(move || {
                reader.query_with_token(since_id, None, filters, &worker_token)
            })
            .await;
            if generation != view.model.borrow().generation {
                return;
            }
            view.active_token.borrow_mut().take();
            match result {
                Ok(Ok(page)) => {
                    let live = build_live_snapshot(&view.state.borrow());
                    let history_status = view.state.borrow().history.status();
                    let stale = history_status.state != "ok"
                        || history_status.maintenance.last_result == "failed";
                    view.model
                        .borrow_mut()
                        .apply_page(generation, page, &live, append, stale);
                }
                Ok(Err(error)) if error == "history query cancelled" => return,
                Ok(Err(error)) => {
                    view.model.borrow_mut().fail(generation, error, false);
                }
                Err(_) => {
                    view.model.borrow_mut().fail(
                        generation,
                        "History query worker failed".into(),
                        false,
                    );
                }
            }
            view.render();
        });
    }

    fn render(&self) {
        let model = self.model.borrow();
        let mut rendered = self.rendered_rows.borrow_mut();
        // The model only ever appends rows or fully resets them (see
        // `apply_page`/`begin_query`), so a shrink signals a reset: rebuild from
        // scratch. Otherwise reuse existing rows and append only the delta, which
        // keeps repeated "Load more" clicks from destroying and recreating every
        // widget in the list.
        if model.rows.len() < *rendered {
            while let Some(child) = self.list.first_child() {
                self.list.remove(&child);
            }
            *rendered = 0;
        }
        match &model.render_state {
            RenderState::Loading => self.status.set_text(if model.rows.is_empty() {
                "Searching durable history…"
            } else {
                "Loading more durable history…"
            }),
            RenderState::Empty => self.status.set_text(if model.scan_exhausted {
                "No matches in this scan window. Load more to continue."
            } else {
                "No durable history matches these filters."
            }),
            RenderState::Ready => self.status.set_text(&format!(
                "{} historical observations · {} candidates scanned{}",
                model.rows.len(),
                model.scanned,
                if model.scan_exhausted {
                    " · scan budget reached"
                } else {
                    ""
                }
            )),
            RenderState::Stale => self.status.set_text(&format!(
                "{} historical observations · live verification is stale",
                model.rows.len()
            )),
            RenderState::Unavailable(error) => self
                .status
                .set_text(&format!("History unavailable: {error}")),
            RenderState::Failed(error) => self.status.set_text(&format!("History failed: {error}")),
        }
        for row in &model.rows[*rendered..] {
            self.list.append(&build_row(
                row,
                &self.window,
                &self.state,
                &self.tab_list,
                &self.container,
                &self.active_token,
            ));
        }
        *rendered = model.rows.len();
        self.load_more.set_visible(model.has_more);
        self.load_more
            .set_sensitive(!matches!(model.render_state, RenderState::Loading));
        self.export.set_sensitive(!model.rows.is_empty());
    }
}

pub fn build_history_view(
    state: Rc<RefCell<AppState>>,
    tab_list: gtk::Box,
    window: adw::ApplicationWindow,
) -> HistoryView {
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search sanitized summaries and event types")
        .hexpand(true)
        .build();
    let from_ts = filter_entry("From Unix ms");
    let to_ts = filter_entry("To Unix ms");
    let record_type = gtk::DropDown::from_strings(&["All types", "Events", "Diagnostics", "Work"]);
    let session = filter_entry("Session");
    let workspace = filter_entry("Workspace origin");
    let pane = filter_entry("Pane origin");
    let task = filter_entry("Task id");
    let repository = filter_entry("owner/repository");
    let authority = filter_entry("Authority");
    let verification = filter_entry("Verification");
    let severity = gtk::DropDown::from_strings(&["All severities", "Info", "Warn", "Error"]);
    let order = gtk::DropDown::from_strings(&["Newest first", "Oldest first"]);

    let filters = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .max_children_per_line(4)
        .min_children_per_line(2)
        .row_spacing(8)
        .column_spacing(8)
        .build();
    for widget in [
        from_ts.clone().upcast::<gtk::Widget>(),
        to_ts.clone().upcast(),
        record_type.clone().upcast(),
        session.clone().upcast(),
        workspace.clone().upcast(),
        pane.clone().upcast(),
        task.clone().upcast(),
        repository.clone().upcast(),
        authority.clone().upcast(),
        verification.clone().upcast(),
        severity.clone().upcast(),
        order.clone().upcast(),
    ] {
        filters.insert(&widget, -1);
    }

    let status = gtk::Label::new(Some("Search durable history"));
    status.set_halign(gtk::Align::Start);
    status.set_wrap(true);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&list)
        .build();
    let load_more = gtk::Button::with_label("Load more");
    let export = gtk::Button::with_label("Copy page export");
    let close = gtk::Button::with_label("Close");
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.append(&load_more);
    actions.append(&export);
    actions.append(&close);
    let title = gtk::Label::new(Some("History"));
    title.set_halign(gtk::Align::Start);
    title.add_css_class("title-2");
    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.set_size_request(900, 650);
    card.set_margin_top(24);
    card.set_margin_bottom(24);
    card.set_margin_start(24);
    card.set_margin_end(24);
    card.append(&title);
    card.append(&search);
    card.append(&filters);
    card.append(&status);
    card.append(&scroll);
    card.append(&actions);
    let container = gtk::CenterBox::new();
    container.set_orientation(gtk::Orientation::Vertical);
    container.set_hexpand(true);
    container.set_vexpand(true);
    container.set_focusable(true);
    container.set_visible(false);
    container.add_css_class("workspace-inspector-backdrop");
    container.set_center_widget(Some(&card));

    let view = HistoryView {
        container,
        search,
        from_ts,
        to_ts,
        record_type,
        session,
        workspace,
        pane,
        task,
        repository,
        authority,
        verification,
        severity,
        order,
        status,
        list,
        rendered_rows: Rc::new(RefCell::new(0)),
        load_more,
        export,
        model: Rc::new(RefCell::new(HistoryViewModel::default())),
        active_token: Rc::new(RefCell::new(None)),
        state,
        tab_list,
        window,
    };

    {
        let view = view.clone();
        close.connect_clicked(move |_| view.hide());
    }
    {
        let view = view.clone();
        view.load_more
            .clone()
            .connect_clicked(move |_| view.run_query(true));
    }
    {
        let view = view.clone();
        view.export.clone().connect_clicked(move |_| {
            let text = export_page(&view.model.borrow().rows);
            if let Some(display) = gdk::Display::default() {
                display.clipboard().set_text(&text);
                crate::show_toast("Sanitized history page copied");
            }
        });
    }
    {
        let view = view.clone();
        view.search
            .clone()
            .connect_search_changed(move |_| view.run_query(false));
    }
    for entry in [
        view.from_ts.clone(),
        view.to_ts.clone(),
        view.session.clone(),
        view.workspace.clone(),
        view.pane.clone(),
        view.task.clone(),
        view.repository.clone(),
        view.authority.clone(),
        view.verification.clone(),
    ] {
        let view = view.clone();
        entry.connect_changed(move |_| view.run_query(false));
    }
    for dropdown in [
        view.record_type.clone(),
        view.severity.clone(),
        view.order.clone(),
    ] {
        let view = view.clone();
        dropdown.connect_selected_notify(move |_| view.run_query(false));
    }
    view
}

fn filter_entry(placeholder: &str) -> gtk::Entry {
    gtk::Entry::builder()
        .placeholder_text(placeholder)
        .width_chars(18)
        .build()
}

fn entry_filter(entry: &gtk::Entry) -> Option<String> {
    let value = entry.text();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn optional_u64(entry: &gtk::Entry, label: &str) -> Result<Option<u64>, String> {
    let Some(value) = entry_filter(entry) else {
        return Ok(None);
    };
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| format!("{label} must be a positive Unix timestamp in milliseconds"))
}

fn dropdown_filter(dropdown: &gtk::DropDown, values: &[&str]) -> Option<String> {
    values
        .get(dropdown.selected() as usize)
        .filter(|value| !value.is_empty())
        .map(|value| (*value).to_string())
}

fn build_row(
    row: &HistoryRow,
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    container: &gtk::CenterBox,
    active_token: &Rc<RefCell<Option<HistoryQueryToken>>>,
) -> gtk::ListBoxRow {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.set_margin_top(10);
    body.set_margin_bottom(10);
    body.set_margin_start(12);
    body.set_margin_end(12);
    let title = gtk::Label::new(Some(&format!(
        "{} · {}",
        row.historical_label, row.type_label
    )));
    title.set_halign(gtk::Align::Start);
    title.add_css_class("heading");
    let snippet = gtk::Label::new(Some(&row.snippet));
    snippet.set_halign(gtk::Align::Start);
    snippet.set_wrap(true);
    snippet.set_selectable(true);
    let provenance = gtk::Label::new(Some(&format!(
        "Source {} · authority {} · verification {} · observed at {}",
        row.source_label, row.authority_label, row.verification_label, row.observed_at_unix_ms
    )));
    provenance.set_halign(gtk::Align::Start);
    provenance.set_wrap(true);
    provenance.add_css_class("dim-label");
    body.append(&title);
    body.append(&snippet);
    body.append(&provenance);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    if let Some(target) = row.navigation.pane.clone() {
        let button = gtk::Button::with_label(&format!(
            "Focus pane · checked at {}",
            target.checked_at_unix_ms
        ));
        let window = window.clone();
        let container = container.clone();
        let active_token = active_token.clone();
        button.connect_clicked(move |_| {
            dismiss_history(&container, &active_token);
            activate_focus_pane(&window, target.tab_id, target.pane_id);
        });
        actions.append(&button);
    } else if let Some(note) = &row.navigation.pane_note {
        actions.append(&plain_note(note));
    }
    if let Some(target) = row.navigation.task.clone() {
        let button = gtk::Button::with_label(&format!(
            "Open {} · checked at {}",
            target.task_id, target.checked_at_unix_ms
        ));
        let window = window.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let container = container.clone();
        let active_token = active_token.clone();
        button.connect_clicked(move |_| {
            dismiss_history(&container, &active_token);
            activate_focus_pane(&window, target.tab_id, target.pane_id);
            crate::task_panel::discover_tasks_explicitly(&state, &tab_list, target.tab_id);
        });
        actions.append(&button);
    } else if let Some(note) = &row.navigation.task_note {
        actions.append(&plain_note(note));
    }
    if let Some(target) = row.navigation.pull_request.clone() {
        let button = gtk::Button::with_label(&format!(
            "Open PR · checked at {}",
            target.checked_at_unix_ms
        ));
        button.connect_clicked(move |_| crate::task_panel::open_external_url(&target.url));
        actions.append(&button);
    } else if let Some(note) = &row.navigation.pull_request_note {
        actions.append(&plain_note(note));
    }
    body.append(&actions);
    let list_row = gtk::ListBoxRow::new();
    list_row.set_child(Some(&body));
    list_row
}

fn dismiss_history(
    container: &gtk::CenterBox,
    active_token: &Rc<RefCell<Option<HistoryQueryToken>>>,
) {
    if let Some(token) = active_token.borrow_mut().take() {
        token.cancel();
    }
    container.set_visible(false);
}

fn plain_note(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("dim-label");
    label
}

fn activate_focus_pane(window: &adw::ApplicationWindow, tab_id: u32, pane_id: u32) {
    let Some(application) = window.application() else {
        return;
    };
    let value = format!("{tab_id}:{pane_id}");
    gio::prelude::ActionGroupExt::activate_action(
        &application,
        "focus-pane",
        Some(&value.to_variant()),
    );
}

fn build_live_snapshot(state: &AppState) -> LiveSnapshot {
    let checked_at_unix_ms = crate::events::unix_time_ms();
    let work = crate::work_ledger::work_stream_snapshot_json(state);
    let mut panes = Vec::new();
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            for pane in tab.panes.leaves() {
                let task_id = pane
                    .current_task
                    .as_ref()
                    .and_then(|task| live_plan_task_id(&work, &pane.work_origin, &task.task_id));
                panes.push(LivePane {
                    workspace_origin: workspace.work_origin.clone(),
                    pane_origin: pane.work_origin.clone(),
                    tab_id: tab.id,
                    pane_id: pane.pane_id,
                    task_id,
                    checked_at_unix_ms,
                });
            }
        }
    }

    let mut pull_requests: Vec<LivePullRequest> = Vec::new();
    for entry in work["all_entries"].as_array().into_iter().flatten() {
        if entry["reconciliation"]["status"].as_str() != Some("verified")
            || entry["reconciliation"]["origin_status"].as_str() != Some("live")
        {
            continue;
        }
        let pull_request = &entry["record"]["pull_request"];
        let (Some(repository), Some(number), Some(url)) = (
            pull_request["repository"].as_str(),
            pull_request["number"].as_u64(),
            pull_request["url"].as_str(),
        ) else {
            continue;
        };
        let candidate = LivePullRequest {
            repository: repository.to_string(),
            number,
            url: url.to_string(),
            checked_at_unix_ms: entry["reconciliation"]["checked_at_unix_ms"]
                .as_u64()
                .unwrap_or(checked_at_unix_ms),
        };
        if let Some(existing) = pull_requests.iter_mut().find(|existing| {
            existing
                .repository
                .eq_ignore_ascii_case(&candidate.repository)
                && existing.number == candidate.number
                && existing.url == candidate.url
        }) {
            if candidate.checked_at_unix_ms > existing.checked_at_unix_ms {
                *existing = candidate;
            }
        } else {
            pull_requests.push(candidate);
        }
    }
    LiveSnapshot {
        panes,
        pull_requests,
    }
}

fn live_plan_task_id(work: &serde_json::Value, pane_origin: &str, task_id: &str) -> Option<String> {
    let matches = work["truth"]
        .as_array()?
        .iter()
        .filter(|truth| {
            truth["pane_origin"].as_str() == Some(pane_origin)
                && truth["task_id"].as_str() == Some(task_id)
                && truth["origin"].as_str() == Some("live")
                && matches!(
                    truth["canonical"].as_str(),
                    Some("todo" | "in_progress" | "blocked" | "done" | "cancelled")
                )
        })
        .count();
    (matches == 1).then(|| task_id.to_string())
}
