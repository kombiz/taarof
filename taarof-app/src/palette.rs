use gtk::prelude::*;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use crate::sidebar;
use crate::task_launch::{TaskLaunchError, TaskLaunchExecutor, TaskLaunchPlan, TaskLaunchRequest};
use crate::templates::{SavedWorkspaceTemplate, TemplateKind, TemplateRecord};
use crate::{AppState, RuntimeHandle};

// Poll interval for the in-memory task discovery cache used while the palette
// is open. The background worker runs mise/SSH on a separate thread; this timer
// only reads from the in-memory cache and repaints the list when results land.
// 500 ms is a reasonable tradeoff: the palette shows a "discovering…" entry
// immediately, and the list refreshes at most 2× per second without spinning
// the GTK main loop unnecessarily.
const TASK_DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// A single entry in the command palette.
struct PaletteEntry {
    label: String,
    shortcut: Option<String>,
    action: PaletteAction,
}

enum PaletteAction {
    /// Fire a bindable action's GAction. Typed rather than a `"win.…"` string so
    /// the compiler, not a silent runtime lookup miss, catches drift between the
    /// palette and [`crate::keybindings::Action`].
    Activate(crate::keybindings::Action),
    Noop,
    ShowError(String),
    OpenProject(crate::projects::RegisteredProject),
    ForgetProject(crate::projects::RegisteredProject),
    /// Open a tmux-backed tab on an SSH connection candidate resolved through
    /// [`crate::ssh_config`]. Typed like [`Self::OpenProject`] rather than
    /// carrying a GTK action name.
    ConnectHost(crate::host::HostConfig),
    SwitchTab(u32), // tab ID
    MoveCurrentTabToWorkspace(u32),
    MiseTask(TaskLaunchRequest),
    MiseTaskInFocusedPane(TaskLaunchRequest),
    WorkspaceAction(TaskLaunchRequest),
    WorkspaceActionInFocusedPane(TaskLaunchRequest),
    SaveCurrentTabTemplate,
    SaveCurrentWorkspaceTemplate,
    OpenTabTemplates,
    OpenWorkspaceTemplates,
    OpenTabTemplate(String),
    OpenWorkspaceTemplate(String),
    DeleteTemplates,
    DeleteTemplate(TemplateKind, String),
    CreateWorktree,
    ListWorktrees,
    RemoveWorktree,
    OpenWorktree(String),
    DeleteWorktree(String, String),
    ResetMode,
    DiscoverTab,
    OpenDashboard,
    SaveView(crate::views::ViewPreset),
    OpenSavedViews,
    OpenSavedView(String),
    DeleteSavedViews,
    DeleteSavedView(String),
    DetachPane,
    /// Open the "Attach session" picker (second-level palette mode).
    AttachSession,
    /// Open the read-only "Keyboard Shortcuts" help view (second-level palette
    /// mode). Lists every action's currently-effective binding.
    OpenKeybindingHelp,
    /// Attach a specific detached/live tmux session by identity.
    AttachSessionTarget {
        session_name: String,
        target: crate::tmux::TmuxTarget,
    },
    /// Focus an existing tab that already backs this session in this instance.
    FocusTab(u32),
    /// Open the "Send to pane" picker (second-level palette mode). Snapshots the
    /// payload source at open time: the current selection/clipboard, or — when
    /// `last_output` is set — the active pane's recent output.
    OpenSendToPane {
        last_output: bool,
    },
    /// Deliver the snapshotted payload to a specific pane. `run` chooses the
    /// "send and run" variant (appends a newline); otherwise the text lands in
    /// the composer without executing.
    SendToPane {
        tab_id: u32,
        pane_id: u32,
        run: bool,
    },
    /// Open the "Clipboard History" picker (second-level palette mode) over the
    /// in-memory clipboard "kill-ring".
    OpenClipboardHistory,
    /// Re-copy a chosen clipboard-history entry back onto the system clipboard.
    CopyClipboardEntry(String),
    /// Seed the "Send to pane" picker with a chosen history entry's text and
    /// switch into it (chains into the EXAMPLE-86 flow).
    OpenSendToPaneWithText(String),
    /// Open the "Send to pane" picker preloaded with the active pane's last agent
    /// message (falling back to recent output), for agent-to-agent relay
    /// (EXAMPLE-91). Keeps the palette open like `OpenSendToPane`.
    OpenRelayLastMessage,
    /// Open the "Recent Files" picker (second-level palette mode) listing files
    /// the focused tab's agents created/edited, newest-first (EXAMPLE-92).
    OpenRecentFiles,
    /// Open a chosen recent file in the configured editor.
    OpenRecentFile(PathBuf),
}

/// The payload a "send to pane" picker will deliver, snapshotted when the picker
/// opens so it is stable while the user chooses a target.
#[derive(Clone)]
enum SendPayloadSource {
    /// Use the captured selection if present, else fall back to the clipboard at
    /// delivery time.
    SelectionOrClipboard { selection: Option<String> },
    /// Use the captured recent-output text.
    LastOutput(String),
    /// Use the captured last agent message (raw markdown) for relay (EXAMPLE-91).
    LastMessage(String),
}

/// What attaching a candidate should do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachCandidateKind {
    /// Session is already attached in a tab of THIS taarof instance; selecting
    /// it should focus/switch to that tab rather than spawning a new pane.
    AttachedHere { tab_id: u32 },
    /// Session is not attached in this instance; selecting it runs the normal
    /// `terminal::attach_session` flow (new tab/pane).
    Attach,
}

/// A de-duplicated tmux session that the user can attach or jump to from the
/// palette. Identity is `(session_name, target)` — never name alone — so the
/// same session name on two different hosts yields two distinct candidates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachCandidate {
    pub session_name: String,
    pub target: crate::tmux::TmuxTarget,
    /// Display host label ("local" for `TmuxTarget::Local`, else the ssh target).
    pub host: String,
    /// Whether the underlying tmux session is currently attached somewhere.
    pub attached: bool,
    /// Current/last command for the session, if the source data carried it.
    pub command: Option<String>,
    pub kind: AttachCandidateKind,
}

/// One live tab's tmux backings: `(tab_id, [(session_name, target), …])`.
/// Extracted from the pane tree so `build_attach_candidates` stays GTK-free and
/// unit-testable.
pub type LiveTabBacking = (u32, Vec<(String, crate::tmux::TmuxTarget)>);

/// Human-friendly host label for a tmux target. Local collapses every host
/// spelling (localhost/127.0.0.1/::1/hostname) into a single "local" identity,
/// mirroring the CLI `tmux ls` normalization (commit d7960b8).
fn attach_host_label(target: &crate::tmux::TmuxTarget) -> String {
    match target {
        crate::tmux::TmuxTarget::Local => "local".to_string(),
        crate::tmux::TmuxTarget::Remote { ssh_target } => ssh_target.clone(),
    }
}

/// Find the tab in `live_tabs` whose pane tmux backing matches this identity.
fn live_tab_for_identity(
    live_tabs: &[LiveTabBacking],
    session_name: &str,
    target: &crate::tmux::TmuxTarget,
) -> Option<u32> {
    live_tabs.iter().find_map(|(tab_id, backings)| {
        backings
            .iter()
            .any(|(name, tgt)| name == session_name && tgt == target)
            .then_some(*tab_id)
    })
}

/// Merge detached sessions and probed dashboard sessions into a single,
/// de-duplicated list of attach candidates keyed by `(session_name, target)`.
///
/// - `detached`: sessions this instance explicitly detached (name + target + cmd).
/// - `dashboard_sessions`: probed local + configured-remote sessions.
/// - `live_tabs`: this instance's tabs with tmux-backed panes, used to resolve
///   "already attached here" so selection focuses the tab instead of respawning.
///
/// The returned order is stable: detached sessions first (in input order), then
/// any dashboard-only sessions not already represented. Empty input yields an
/// empty vec — the UI layer is responsible for the "no sessions" message.
pub fn build_attach_candidates(
    detached: &[crate::dashboard::DetachedSession],
    dashboard_sessions: &[crate::dashboard::DashboardSession],
    live_tabs: &[LiveTabBacking],
) -> Vec<AttachCandidate> {
    use std::collections::HashSet;

    let mut candidates: Vec<AttachCandidate> = Vec::new();
    let mut seen: HashSet<(String, crate::tmux::TmuxTarget)> = HashSet::new();

    let mut push_candidate = |session_name: String,
                              target: crate::tmux::TmuxTarget,
                              attached: bool,
                              command: Option<String>| {
        let key = (session_name.clone(), target.clone());
        if !seen.insert(key) {
            // Already represented; keep the first (detached) row but enrich a
            // missing command and promote attached-state if the later source
            // has more information.
            if let Some(existing) = candidates
                .iter_mut()
                .find(|c| c.session_name == session_name && c.target == target)
            {
                if existing.command.is_none() {
                    existing.command = command;
                }
                existing.attached = existing.attached || attached;
            }
            return;
        }

        let kind = match live_tab_for_identity(live_tabs, &session_name, &target) {
            Some(tab_id) => AttachCandidateKind::AttachedHere { tab_id },
            None => AttachCandidateKind::Attach,
        };
        let host = attach_host_label(&target);
        candidates.push(AttachCandidate {
            session_name,
            target,
            host,
            attached,
            command,
            kind,
        });
    };

    for d in detached {
        push_candidate(
            d.session_name.clone(),
            d.target.clone(),
            // A detached session is by definition not attached elsewhere; if
            // this instance re-attached it, live_tabs resolution flags it as
            // AttachedHere instead.
            false,
            d.last_command.clone(),
        );
    }

    for s in dashboard_sessions {
        let attached = matches!(
            s.status,
            crate::dashboard::SessionStatus::Running | crate::dashboard::SessionStatus::Idle
        ) && !s.is_detached;
        push_candidate(
            s.name.clone(),
            s.target.clone(),
            attached,
            s.command.clone(),
        );
    }

    candidates
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MiseDispatchMode {
    OpenNewTab,
}

#[derive(Clone, Copy, Default)]
enum PaletteMode {
    #[default]
    Default,
    WorktreeList,
    WorktreeRemove,
    TemplateTabList,
    TemplateWorkspaceList,
    TemplateDeleteList,
    SavedViewList,
    SavedViewDeleteList,
    AttachSessionList,
    KeybindingHelp,
    SendToPane,
    ClipboardHistory,
    RecentFiles,
}

/// Worktrees listed for one repo root, as produced by the async Git worker.
type WorktreeListing = (PathBuf, Vec<crate::git::WorktreeInfo>);

/// A palette store is populated by the bounded worker and read synchronously
/// only from memory while GTK redraws rows. An error is a visible error state,
/// never an empty healthy list.
#[derive(Default)]
struct PaletteStore<T> {
    value: Option<Result<T, String>>,
    in_flight: bool,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorktreeMutationKind {
    Create,
    Remove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WorktreeMutationSubmissionFeedback {
    message: &'static str,
    is_error: bool,
    /// Saturation is not a completed mutation. Keep the confirmation dialog
    /// open so the user can retry the same explicit target without reopening
    /// the palette and reconstructing it from a stale listing.
    keep_dialog_open: bool,
}

fn worktree_mutation_submission_feedback(
    kind: WorktreeMutationKind,
    submission: crate::git::GitAsyncSubmission,
) -> Option<WorktreeMutationSubmissionFeedback> {
    use crate::git::GitAsyncSubmission;

    match (kind, submission) {
        (_, GitAsyncSubmission::Started) => None,
        (WorktreeMutationKind::Create, GitAsyncSubmission::Coalesced) => {
            Some(WorktreeMutationSubmissionFeedback {
                message: "Worktree creation is already running",
                is_error: false,
                keep_dialog_open: false,
            })
        }
        (WorktreeMutationKind::Create, GitAsyncSubmission::Saturated) => {
            Some(WorktreeMutationSubmissionFeedback {
                message: "Git worker queue is busy; retry shortly",
                is_error: true,
                keep_dialog_open: true,
            })
        }
        (WorktreeMutationKind::Remove, GitAsyncSubmission::Coalesced) => {
            Some(WorktreeMutationSubmissionFeedback {
                message: "Worktree removal is already running; wait for its result",
                is_error: false,
                keep_dialog_open: false,
            })
        }
        (WorktreeMutationKind::Remove, GitAsyncSubmission::Saturated) => {
            Some(WorktreeMutationSubmissionFeedback {
                message: "Git worker queue is busy; worktree was not removed. Retry shortly",
                is_error: true,
                keep_dialog_open: true,
            })
        }
    }
}

fn worktree_rollback_submission_cause(
    submission: crate::git::GitAsyncSubmission,
) -> Option<&'static str> {
    match submission {
        crate::git::GitAsyncSubmission::Started => None,
        crate::git::GitAsyncSubmission::Coalesced => {
            Some("a rollback was already registered without this completion")
        }
        crate::git::GitAsyncSubmission::Saturated => Some("the Git worker queue is busy"),
    }
}

fn worktree_mutation_worker_failure(kind: WorktreeMutationKind, error: &str) -> String {
    match kind {
        WorktreeMutationKind::Create => format!("Could not create worktree: {error}"),
        WorktreeMutationKind::Remove => format!("Could not remove worktree: {error}"),
    }
}

fn worktree_rollback_worker_failure(worktree_path: &str, error: &str) -> String {
    format!(
        "Created worktree {worktree_path}, but could not roll it back after its source closed: {error}"
    )
}

fn worktree_listing_submission_error(
    submission: crate::git::GitAsyncSubmission,
) -> Option<&'static str> {
    match submission {
        crate::git::GitAsyncSubmission::Started => None,
        crate::git::GitAsyncSubmission::Coalesced => {
            Some("Git worktree listing coalesced unexpectedly; reopen Worktrees to retry")
        }
        crate::git::GitAsyncSubmission::Saturated => {
            Some("Git worker queue is busy; reopen Worktrees to retry")
        }
    }
}

/// Apply only the current worktree request. A late result must not clear a
/// replacement request or poison the cache for the palette's new context.
fn apply_worktree_listing_result(
    current_generation: &Cell<u64>,
    request_generation: u64,
    in_flight: &Cell<bool>,
    listing: &RefCell<Option<WorktreeListing>>,
    listing_error: &RefCell<Option<(PathBuf, String)>>,
    repo_root: PathBuf,
    result: Result<Vec<crate::git::WorktreeInfo>, String>,
) -> bool {
    if current_generation.get() != request_generation {
        return false;
    }

    in_flight.set(false);
    match result {
        Ok(worktrees) => {
            *listing.borrow_mut() = Some((repo_root, worktrees));
            *listing_error.borrow_mut() = None;
        }
        Err(error) => {
            // A failed worker is a visible error state, not a healthy empty
            // cache. Invalidation clears it and allows an explicit retry.
            *listing.borrow_mut() = None;
            *listing_error.borrow_mut() = Some((repo_root, error));
        }
    }
    true
}

fn invalidate_worktree_listing_state(
    generation: &Cell<u64>,
    listing: &RefCell<Option<WorktreeListing>>,
    listing_error: &RefCell<Option<(PathBuf, String)>>,
    in_flight: &Cell<bool>,
) {
    generation.set(generation.get().wrapping_add(1));
    *listing.borrow_mut() = None;
    *listing_error.borrow_mut() = None;
    in_flight.set(false);
}

thread_local! {
    static PALETTE_PROJECTS: RefCell<PaletteStore<Vec<crate::projects::RegisteredProject>>> = RefCell::new(PaletteStore::default());
    static PALETTE_VIEWS: RefCell<PaletteStore<Vec<crate::views::SavedView>>> = RefCell::new(PaletteStore::default());
    static PALETTE_TEMPLATES: RefCell<PaletteStore<Vec<TemplateRecord>>> = RefCell::new(PaletteStore::default());
}

/// Widgets returned from build_command_palette for wiring in main.
pub struct CommandPalette {
    pub container: gtk::Box,
    pub entry: gtk::SearchEntry,
    pub list: gtk::ListBox,
    mode: Rc<RefCell<PaletteMode>>,
    /// Payload snapshot for the "send to pane" picker, captured when the picker
    /// opens so the delivery is independent of later selection/clipboard changes.
    send_source: Rc<RefCell<Option<SendPayloadSource>>>,
    discovery_poll_source: Rc<RefCell<Option<glib::SourceId>>>,
    worktree_listing: Rc<RefCell<Option<WorktreeListing>>>,
    worktree_listing_error: Rc<RefCell<Option<(PathBuf, String)>>>,
    worktree_listing_in_flight: Rc<Cell<bool>>,
    /// Invalidating the worktree view must also invalidate an already-running
    /// worker.  Otherwise a remove/open transition can let its late result
    /// repopulate the next palette view with an obsolete listing.
    worktree_listing_generation: Rc<Cell<u64>>,
    search_changed_handler: RefCell<Option<glib::SignalHandlerId>>,
    activate_handler: RefCell<Option<glib::SignalHandlerId>>,
    row_activated_handler: RefCell<Option<glib::SignalHandlerId>>,
    key_controller: RefCell<Option<gtk::EventControllerKey>>,
}

impl Clone for CommandPalette {
    fn clone(&self) -> Self {
        Self {
            container: self.container.clone(),
            entry: self.entry.clone(),
            list: self.list.clone(),
            mode: self.mode.clone(),
            send_source: self.send_source.clone(),
            discovery_poll_source: self.discovery_poll_source.clone(),
            worktree_listing: self.worktree_listing.clone(),
            worktree_listing_error: self.worktree_listing_error.clone(),
            worktree_listing_in_flight: self.worktree_listing_in_flight.clone(),
            worktree_listing_generation: self.worktree_listing_generation.clone(),
            search_changed_handler: RefCell::new(None),
            activate_handler: RefCell::new(None),
            row_activated_handler: RefCell::new(None),
            key_controller: RefCell::new(None),
        }
    }
}

/// Build the command palette overlay widget (hidden by default).
pub fn build_command_palette() -> CommandPalette {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.add_css_class("command-palette");
    container.set_halign(gtk::Align::Center);
    container.set_valign(gtk::Align::Start);
    container.set_margin_top(80);
    container.set_visible(false);
    container.set_width_request(400);

    let entry = gtk::SearchEntry::new();
    entry.set_placeholder_text(Some("Type a command…"));
    entry.add_css_class("palette-entry");
    container.append(&entry);

    let list = gtk::ListBox::new();
    list.add_css_class("palette-list");
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_activate_on_single_click(true);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .max_content_height(320)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    container.append(&scroll);

    CommandPalette {
        container,
        entry,
        list,
        mode: Rc::new(RefCell::new(PaletteMode::Default)),
        send_source: Rc::new(RefCell::new(None)),
        discovery_poll_source: Rc::new(RefCell::new(None)),
        worktree_listing: Rc::new(RefCell::new(None)),
        worktree_listing_error: Rc::new(RefCell::new(None)),
        worktree_listing_in_flight: Rc::new(Cell::new(false)),
        worktree_listing_generation: Rc::new(Cell::new(0)),
        search_changed_handler: RefCell::new(None),
        activate_handler: RefCell::new(None),
        row_activated_handler: RefCell::new(None),
        key_controller: RefCell::new(None),
    }
}

fn palette_entries(palette: &CommandPalette, state: &Rc<RefCell<AppState>>) -> Vec<PaletteEntry> {
    let mode = *palette.mode.borrow();
    if matches!(mode, PaletteMode::SendToPane) {
        // This is the only entry point with access to the captured send source,
        // so build the picker here rather than in the palette-less build_entries.
        let source_label = send_source_label(palette.send_source.borrow().as_ref());
        return build_send_to_pane_entries(&source_label, state);
    }
    if matches!(
        mode,
        PaletteMode::WorktreeList | PaletteMode::WorktreeRemove
    ) {
        let listing = palette.worktree_listing.borrow().clone();
        let error = palette.worktree_listing_error.borrow().clone();
        return build_worktree_entries(state, mode, listing, error);
    }
    build_entries(state, mode)
}

fn filtered_entries<'a>(entries: &'a [PaletteEntry], query: &str) -> Vec<&'a PaletteEntry> {
    let query = query.to_lowercase();
    if query.is_empty() {
        entries
            .iter()
            .filter(|entry| !entry.action.is_search_only())
            .collect()
    } else {
        entries
            .iter()
            .filter(|entry| entry.label.to_lowercase().contains(&query))
            .collect()
    }
}

fn activated_entry_for_row<'a>(
    entries: &'a [PaletteEntry],
    query: &str,
    row_index: i32,
) -> Option<&'a PaletteEntry> {
    let row_index = usize::try_from(row_index).ok()?;
    filtered_entries(entries, query).get(row_index).copied()
}

impl PaletteAction {
    fn is_search_only(&self) -> bool {
        matches!(
            self,
            Self::MiseTaskInFocusedPane(_) | Self::WorkspaceActionInFocusedPane(_)
        )
    }

    #[cfg(test)]
    fn mise_dispatch_mode(&self) -> Option<MiseDispatchMode> {
        // All mise task variants now open a fresh terminal tab; FocusedPane
        // entries are legacy search-only aliases that no longer inject into the
        // active shell (see #23).
        match self {
            Self::MiseTask(_)
            | Self::WorkspaceAction(_)
            | Self::MiseTaskInFocusedPane(_)
            | Self::WorkspaceActionInFocusedPane(_) => Some(MiseDispatchMode::OpenNewTab),
            _ => None,
        }
    }
}

fn repopulate_palette_list(palette: &CommandPalette, state: &Rc<RefCell<AppState>>, query: &str) {
    sync_task_discovery(palette, state);
    ensure_worktree_listing(palette, state);
    ensure_palette_stores(palette, state);
    let entries = palette_entries(palette, state);
    let filtered = filtered_entries(&entries, query);
    while let Some(child) = palette.list.first_child() {
        palette.list.remove(&child);
    }
    for entry in filtered {
        palette.list.append(&make_row(entry));
    }
    if let Some(first) = palette.list.row_at_index(0) {
        palette.list.select_row(Some(&first));
    }
}

fn invalidate_palette_projects() {
    PALETTE_PROJECTS.with(|store| {
        let mut store = store.borrow_mut();
        store.generation = store.generation.wrapping_add(1);
        store.value = None;
        store.in_flight = false;
    });
}

fn invalidate_palette_views() {
    PALETTE_VIEWS.with(|store| {
        let mut store = store.borrow_mut();
        store.generation = store.generation.wrapping_add(1);
        store.value = None;
        store.in_flight = false;
    });
}

fn invalidate_palette_templates() {
    PALETTE_TEMPLATES.with(|store| {
        let mut store = store.borrow_mut();
        store.generation = store.generation.wrapping_add(1);
        store.value = None;
        store.in_flight = false;
    });
}

/// Start only the stores the currently-visible palette mode needs. The worker
/// owns all filesystem reads; redraws below only clone these cached values.
fn ensure_palette_stores(palette: &CommandPalette, state: &Rc<RefCell<AppState>>) {
    let load_projects = PALETTE_PROJECTS.with(|store| {
        let mut store = store.borrow_mut();
        if store.value.is_some() || store.in_flight {
            None
        } else {
            store.in_flight = true;
            Some(store.generation)
        }
    });
    if let Some(generation) = load_projects {
        let palette_for_apply = palette.clone();
        let state_for_apply = state.clone();
        let submission = crate::git::spawn_async_result(
            format!("palette-projects:{generation}"),
            || crate::projects::list().map_err(|error| error.to_string()),
            move |result| {
                PALETTE_PROJECTS.with(|store| {
                    let mut store = store.borrow_mut();
                    if store.generation != generation {
                        return;
                    }
                    store.in_flight = false;
                    store.value = Some(result);
                });
                if palette_for_apply.container.is_visible() {
                    let query = palette_for_apply.entry.text().to_string();
                    repopulate_palette_list(&palette_for_apply, &state_for_apply, &query);
                }
            },
        );
        if !matches!(submission, crate::git::GitAsyncSubmission::Started) {
            PALETTE_PROJECTS.with(|store| {
                let mut store = store.borrow_mut();
                if store.generation == generation {
                    store.in_flight = false;
                    let error = match submission {
                        crate::git::GitAsyncSubmission::Coalesced => {
                            "project worker was already registered without this palette; reopen to retry"
                        }
                        crate::git::GitAsyncSubmission::Saturated => {
                            "worker queue is busy; reopen the palette to retry"
                        }
                        crate::git::GitAsyncSubmission::Started => unreachable!(),
                    };
                    store.value = Some(Err(error.into()));
                }
            });
        }
    }

    if matches!(
        *palette.mode.borrow(),
        PaletteMode::TemplateTabList
            | PaletteMode::TemplateWorkspaceList
            | PaletteMode::TemplateDeleteList
    ) {
        let load_templates = PALETTE_TEMPLATES.with(|store| {
            let mut store = store.borrow_mut();
            if store.value.is_some() || store.in_flight {
                None
            } else {
                store.in_flight = true;
                Some(store.generation)
            }
        });
        if let Some(generation) = load_templates {
            let palette_for_apply = palette.clone();
            let state_for_apply = state.clone();
            let submission = crate::git::spawn_async_result(
                format!("palette-templates:{generation}"),
                || crate::templates::try_list().map_err(|error| error.to_string()),
                move |result| {
                    PALETTE_TEMPLATES.with(|store| {
                        let mut store = store.borrow_mut();
                        if store.generation != generation {
                            return;
                        }
                        store.in_flight = false;
                        store.value = Some(result);
                    });
                    if palette_for_apply.container.is_visible() {
                        let query = palette_for_apply.entry.text().to_string();
                        repopulate_palette_list(&palette_for_apply, &state_for_apply, &query);
                    }
                },
            );
            if !matches!(submission, crate::git::GitAsyncSubmission::Started) {
                PALETTE_TEMPLATES.with(|store| {
                    let mut store = store.borrow_mut();
                    if store.generation == generation {
                        store.in_flight = false;
                        let error = match submission {
                            crate::git::GitAsyncSubmission::Coalesced => {
                                "template worker was already registered without this palette; reopen to retry"
                            }
                            crate::git::GitAsyncSubmission::Saturated => {
                                "worker queue is busy; reopen Templates to retry"
                            }
                            crate::git::GitAsyncSubmission::Started => unreachable!(),
                        };
                        store.value = Some(Err(error.into()));
                    }
                });
            }
        }
    }

    if !matches!(
        *palette.mode.borrow(),
        PaletteMode::SavedViewList | PaletteMode::SavedViewDeleteList
    ) {
        return;
    }
    let load_views = PALETTE_VIEWS.with(|store| {
        let mut store = store.borrow_mut();
        if store.value.is_some() || store.in_flight {
            None
        } else {
            store.in_flight = true;
            Some(store.generation)
        }
    });
    if let Some(generation) = load_views {
        let palette_for_apply = palette.clone();
        let state_for_apply = state.clone();
        let submission = crate::git::spawn_async_result(
            format!("palette-views:{generation}"),
            || crate::views::list().map_err(|error| error.to_string()),
            move |result| {
                PALETTE_VIEWS.with(|store| {
                    let mut store = store.borrow_mut();
                    if store.generation != generation {
                        return;
                    }
                    store.in_flight = false;
                    store.value = Some(result);
                });
                if palette_for_apply.container.is_visible() {
                    let query = palette_for_apply.entry.text().to_string();
                    repopulate_palette_list(&palette_for_apply, &state_for_apply, &query);
                }
            },
        );
        if !matches!(submission, crate::git::GitAsyncSubmission::Started) {
            PALETTE_VIEWS.with(|store| {
                let mut store = store.borrow_mut();
                if store.generation == generation {
                    store.in_flight = false;
                    let error = match submission {
                        crate::git::GitAsyncSubmission::Coalesced => {
                            "saved-views worker was already registered without this palette; reopen to retry"
                        }
                        crate::git::GitAsyncSubmission::Saturated => {
                            "worker queue is busy; reopen Saved Views to retry"
                        }
                        crate::git::GitAsyncSubmission::Started => unreachable!(),
                    };
                    store.value = Some(Err(error.into()));
                }
            });
        }
    }
}

/// Populate worktree choices without letting a palette redraw run Git on GTK.
fn ensure_worktree_listing(palette: &CommandPalette, state: &Rc<RefCell<AppState>>) {
    let mode = *palette.mode.borrow();
    if !matches!(
        mode,
        PaletteMode::WorktreeList | PaletteMode::WorktreeRemove
    ) {
        return;
    }
    let repo_root = {
        let state = state.borrow();
        active_repo_root(&state)
    };
    let Some(repo_root) = repo_root else {
        return;
    };
    if palette
        .worktree_listing
        .borrow()
        .as_ref()
        .is_some_and(|(cached_root, _)| cached_root == &repo_root)
        || palette
            .worktree_listing_error
            .borrow()
            .as_ref()
            .is_some_and(|(cached_root, _)| cached_root == &repo_root)
        || palette.worktree_listing_in_flight.get()
    {
        return;
    }
    palette.worktree_listing_in_flight.set(true);
    *palette.worktree_listing_error.borrow_mut() = None;
    let generation = palette.worktree_listing_generation.get();
    let palette_for_apply = palette.clone();
    let state_for_apply = state.clone();
    let repo_for_worker = repo_root.clone();
    let repo_for_apply = repo_root.clone();
    let request_key = format!("worktree-list:{generation}:{}", repo_root.display());
    let submission = crate::git::spawn_async_result(
        request_key,
        move || crate::git::list_worktrees_checked(&repo_for_worker),
        move |result| {
            if !apply_worktree_listing_result(
                &palette_for_apply.worktree_listing_generation,
                generation,
                &palette_for_apply.worktree_listing_in_flight,
                &palette_for_apply.worktree_listing,
                &palette_for_apply.worktree_listing_error,
                repo_for_apply.clone(),
                result,
            ) {
                // The palette changed context while Git was running.  Do not
                // write an old result into the new view; a visible worktree
                // mode will submit a fresh request below.
                // `invalidate_worktree_listing` has released this old owner
                // already. Do not clear a replacement request for the same
                // repo/generation transition.
                if palette_for_apply.container.is_visible() {
                    let query = palette_for_apply.entry.text().to_string();
                    repopulate_palette_list(&palette_for_apply, &state_for_apply, &query);
                }
                return;
            }
            if palette_for_apply.container.is_visible()
                && matches!(
                    *palette_for_apply.mode.borrow(),
                    PaletteMode::WorktreeList | PaletteMode::WorktreeRemove
                )
            {
                let query = palette_for_apply.entry.text().to_string();
                repopulate_palette_list(&palette_for_apply, &state_for_apply, &query);
            }
        },
    );
    if let Some(message) = worktree_listing_submission_error(submission) {
        palette.worktree_listing_in_flight.set(false);
        *palette.worktree_listing_error.borrow_mut() = Some((repo_root, message.to_string()));
        if palette.container.is_visible() {
            let query = palette.entry.text().to_string();
            repopulate_palette_list(palette, state, &query);
        }
    }
}

fn invalidate_worktree_listing(palette: &CommandPalette) {
    invalidate_worktree_listing_state(
        &palette.worktree_listing_generation,
        &palette.worktree_listing,
        &palette.worktree_listing_error,
        &palette.worktree_listing_in_flight,
    );
}

fn clear_task_discovery_poll(palette: &CommandPalette) {
    if let Some(source_id) = palette.discovery_poll_source.borrow_mut().take() {
        source_id.remove();
    }
}

fn ensure_task_discovery_poll(palette: &CommandPalette, state: &Rc<RefCell<AppState>>) {
    if palette.discovery_poll_source.borrow().is_some() {
        return;
    }

    let palette_for_poll = palette.clone();
    let state_for_poll = state.clone();
    let source_id = glib::timeout_add_local(TASK_DISCOVERY_POLL_INTERVAL, move || {
        if !palette_for_poll.container.is_visible()
            || !matches!(*palette_for_poll.mode.borrow(), PaletteMode::Default)
        {
            palette_for_poll.discovery_poll_source.borrow_mut().take();
            return glib::ControlFlow::Break;
        }

        repopulate_palette_list(
            &palette_for_poll,
            &state_for_poll,
            &palette_for_poll.entry.text(),
        );

        let pending = {
            let st = state_for_poll.borrow();
            active_task_target(&st).is_some_and(|target| {
                matches!(
                    crate::mise::cached_task_discovery(&target),
                    crate::mise::CachedTaskDiscovery::Pending
                )
            })
        };

        if pending {
            glib::ControlFlow::Continue
        } else {
            palette_for_poll.discovery_poll_source.borrow_mut().take();
            glib::ControlFlow::Break
        }
    });

    palette
        .discovery_poll_source
        .borrow_mut()
        .replace(source_id);
}

fn sync_task_discovery(palette: &CommandPalette, state: &Rc<RefCell<AppState>>) {
    if !matches!(*palette.mode.borrow(), PaletteMode::Default) {
        clear_task_discovery_poll(palette);
        return;
    }

    let (target, active_tab_id) = {
        let st = state.borrow();
        (active_task_target(&st), st.active_tab().map(|tab| tab.id))
    };

    let Some(target) = target else {
        clear_task_discovery_poll(palette);
        return;
    };

    if let Some(tab_id) = active_tab_id {
        let mut st = state.borrow_mut();
        crate::mise::update_discovery_binary_cache(&mut st, tab_id, &target);
    }

    match crate::mise::spawn_task_discovery(target) {
        crate::mise::TaskDiscoveryRequest::UseCached => clear_task_discovery_poll(palette),
        crate::mise::TaskDiscoveryRequest::Pending | crate::mise::TaskDiscoveryRequest::Start => {
            ensure_task_discovery_poll(palette, state);
        }
    }
}

/// Resolve palette tasks only from the active tab. Unlike the legacy ambient
/// discovery helper, this never falls back to taarof's process CWD or HOME.
fn active_task_target(state: &AppState) -> Option<crate::mise::DiscoveryTarget> {
    let tab_id = state.active_tab()?.id;
    crate::mise::task_target_for_tab(state, tab_id)
}

/// Show the command palette: populate entries, filter, handle selection.
pub fn show_palette(
    palette: &CommandPalette,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    *palette.mode.borrow_mut() = PaletteMode::Default;
    // Clear any stale send-to-pane snapshot from a prior picker session.
    *palette.send_source.borrow_mut() = None;
    // Retrying an opened palette must not preserve a prior store error as if it
    // were a usable empty registry.
    invalidate_palette_projects();
    invalidate_palette_views();
    invalidate_palette_templates();
    clear_task_discovery_poll(palette);
    repopulate_palette_list(palette, state, "");
    install_palette_handlers_and_show(palette, state, tab_list, term_stack, window);
}

pub fn show_keybinding_help(
    palette: &CommandPalette,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    *palette.mode.borrow_mut() = PaletteMode::KeybindingHelp;
    *palette.send_source.borrow_mut() = None;
    clear_task_discovery_poll(palette);
    palette.entry.set_text("");
    repopulate_palette_list(palette, state, "");
    install_palette_handlers_and_show(palette, state, tab_list, term_stack, window);
}

/// Open the "Send to pane" picker directly (from a keybinding or sidebar menu),
/// snapshotting the payload source (selection/clipboard, or recent output when
/// `last_output`) before showing the picker.
pub fn open_send_to_pane_picker(
    palette: &CommandPalette,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
    last_output: bool,
) {
    *palette.send_source.borrow_mut() = capture_send_source(state, last_output);
    *palette.mode.borrow_mut() = PaletteMode::SendToPane;
    clear_task_discovery_poll(palette);
    repopulate_palette_list(palette, state, "");
    install_palette_handlers_and_show(palette, state, tab_list, term_stack, window);
}

/// Snapshot the payload for a "relay last message" picker: the active pane's last
/// agent message (raw markdown) when a transcript resolves, else fall back to the
/// recent-output capture with an explanatory toast (EXAMPLE-91).
fn relay_last_message_source(state: &Rc<RefCell<AppState>>) -> SendPayloadSource {
    match crate::terminal::last_agent_message_for_active_pane(state) {
        crate::terminal::LastAgentMessageResolution::Transcript(text) => {
            SendPayloadSource::LastMessage(text)
        }
        crate::terminal::LastAgentMessageResolution::Unavailable { agent, reason } => {
            crate::show_toast(&format!(
                "{agent}: {reason}; relaying recent output instead"
            ));
            capture_send_source(state, true)
                .unwrap_or(SendPayloadSource::SelectionOrClipboard { selection: None })
        }
    }
}

/// Open the "Send to pane" picker preloaded with the active pane's last agent
/// message (falling back to recent output), for agent-to-agent relay (EXAMPLE-91).
pub fn open_relay_last_message_picker(
    palette: &CommandPalette,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    *palette.send_source.borrow_mut() = Some(relay_last_message_source(state));
    *palette.mode.borrow_mut() = PaletteMode::SendToPane;
    clear_task_discovery_poll(palette);
    repopulate_palette_list(palette, state, "");
    install_palette_handlers_and_show(palette, state, tab_list, term_stack, window);
}

/// Open the "Clipboard History" picker directly (from a keybinding or palette
/// command). Reads the in-memory clipboard ring and shows newest-first previews;
/// Enter re-copies, and a secondary "Send to pane…" row chains into the
/// send-to-pane picker.
pub fn open_clipboard_history_picker(
    palette: &CommandPalette,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    *palette.send_source.borrow_mut() = None;
    *palette.mode.borrow_mut() = PaletteMode::ClipboardHistory;
    clear_task_discovery_poll(palette);
    repopulate_palette_list(palette, state, "");
    install_palette_handlers_and_show(palette, state, tab_list, term_stack, window);
}

/// Open the "Recent Files" picker directly (from a keybinding or sidebar menu).
/// Lists files the focused tab's agents created/edited, newest-first; Enter opens
/// the selected file in the configured editor (EXAMPLE-92).
pub fn open_recent_files_picker(
    palette: &CommandPalette,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    *palette.send_source.borrow_mut() = None;
    *palette.mode.borrow_mut() = PaletteMode::RecentFiles;
    clear_task_discovery_poll(palette);
    repopulate_palette_list(palette, state, "");
    install_palette_handlers_and_show(palette, state, tab_list, term_stack, window);
}

/// Show the (already-populated) palette and (re)install the search/activate/key
/// handlers. Callers set the mode and repopulate the list first. The old
/// handlers are disconnected before reconnecting so reopening never double-fires.
fn install_palette_handlers_and_show(
    palette: &CommandPalette,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    let container = &palette.container;
    let entry = &palette.entry;
    let list = &palette.list;

    // Show and focus
    container.set_visible(true);
    entry.set_text("");
    entry.grab_focus();

    // Filter on text change
    {
        let state_for_filter = state.clone();
        let entry_ref = entry.clone();
        let palette_for_filter = palette.clone();
        if let Some(handler) = palette.search_changed_handler.borrow_mut().take() {
            entry.disconnect(handler);
        }
        let handler = entry.connect_search_changed(move |_| {
            repopulate_palette_list(&palette_for_filter, &state_for_filter, &entry_ref.text());
        });
        palette.search_changed_handler.borrow_mut().replace(handler);
    }

    // Enter key: SearchEntry's connect_activate fires on Enter (key controller misses it).
    {
        if let Some(handler) = palette.activate_handler.borrow_mut().take() {
            entry.disconnect(handler);
        }
        let list = list.clone();
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        let palette_for_activate = palette.clone();
        let handler = entry.connect_activate(move |_| {
            activate_palette_row(
                list.selected_row().as_ref(),
                &palette_for_activate,
                &state,
                &tab_list,
                &term_stack,
                &window,
            );
        });
        palette.activate_handler.borrow_mut().replace(handler);
    }

    // Mouse clicks dispatch through the same path as Enter. Every picker mode
    // reuses this ListBox, so the handler also covers secondary pickers.
    {
        if let Some(handler) = palette.row_activated_handler.borrow_mut().take() {
            list.disconnect(handler);
        }
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        let palette_for_activate = palette.clone();
        let handler = list.connect_row_activated(move |_, row| {
            activate_palette_row(
                Some(row),
                &palette_for_activate,
                &state,
                &tab_list,
                &term_stack,
                &window,
            );
        });
        palette.row_activated_handler.borrow_mut().replace(handler);
    }

    // Escape = close, Up/Down = navigate list
    {
        if let Some(controller) = palette.key_controller.borrow_mut().take() {
            entry.remove_controller(&controller);
        }
        let container = container.clone();
        let list = list.clone();
        let state_for_esc = state.clone();
        let key_ctrl = gtk::EventControllerKey::new();
        key_ctrl.connect_key_pressed(move |_ctrl, key, _code, _mods| {
            match key {
                gdk::Key::Escape => {
                    container.set_visible(false);
                    if let Some(terminal) = crate::get_active_terminal(&state_for_esc) {
                        terminal.grab_focus();
                    }
                    return glib::Propagation::Stop;
                }
                gdk::Key::Down => {
                    if let Some(row) = list.selected_row() {
                        if let Some(next) = list.row_at_index(row.index() + 1) {
                            list.select_row(Some(&next));
                        }
                    }
                    return glib::Propagation::Stop;
                }
                gdk::Key::Up => {
                    if let Some(row) = list.selected_row() {
                        let idx = row.index();
                        if idx > 0 {
                            if let Some(prev) = list.row_at_index(idx - 1) {
                                list.select_row(Some(&prev));
                            }
                        }
                    }
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
            glib::Propagation::Proceed
        });
        entry.add_controller(key_ctrl.clone());
        palette.key_controller.borrow_mut().replace(key_ctrl);
    }

    // Select first row
    if let Some(first) = list.row_at_index(0) {
        list.select_row(Some(&first));
    }
}

/// Dispatch the entry represented by `row` in the currently filtered palette.
/// Both keyboard activation and mouse row activation use this function so all
/// picker modes have identical close/refocus behavior.
fn activate_palette_row(
    row: Option<&gtk::ListBoxRow>,
    palette: &CommandPalette,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    if let Some(row) = row {
        let entries = palette_entries(palette, state);
        if let Some(selected) =
            activated_entry_for_row(&entries, &palette.entry.text(), row.index())
        {
            let keep_open = execute_action(
                &selected.action,
                palette,
                state,
                tab_list,
                term_stack,
                window,
            );
            if keep_open {
                palette.entry.grab_focus();
                return;
            }
        }
    }

    palette.container.set_visible(false);
    if let Some(terminal) = crate::get_active_terminal(state) {
        terminal.grab_focus();
    }
}

fn build_entries(state: &Rc<RefCell<AppState>>, mode: PaletteMode) -> Vec<PaletteEntry> {
    if !matches!(mode, PaletteMode::Default) {
        return build_mode_entries(state, mode);
    }

    let has_attention_targets = {
        let st = state.borrow();
        let has_attention = st.all_tabs().any(|tab| st.tab_needs_attention(tab));
        has_attention
    };

    let mut entries = build_core_tab_entries(has_attention_targets);

    let st = state.borrow();
    build_broadcast_entries(&mut entries, &st);
    build_tab_switch_entries(&mut entries, &st);

    let repo_root = active_repo_root(&st);
    let active_task_id = st.active_tab().map(|tab| tab.id);
    let discovery =
        active_task_id.and_then(|tab_id| st.task_discovery_snapshots.get(&tab_id).cloned());
    let discovered_actions = st
        .active_tab()
        .map(|tab| tab.discovered_actions.clone())
        .unwrap_or_default();
    let standard_task_names: Vec<&str> = discovered_actions.iter().map(|a| a.task_name()).collect();
    if let (Some(tab_id), Some(discovery)) = (active_task_id, discovery.as_ref()) {
        build_discovered_task_entries(&mut entries, &discovered_actions, tab_id, discovery.clone());
    }
    drop(st);

    if let (Some(tab_id), Some(discovery)) = (active_task_id, discovery.as_ref()) {
        build_mise_task_entries(
            &mut entries,
            tab_id,
            discovery.clone(),
            &standard_task_names,
        );
    }
    build_worktree_palette_entries(&mut entries, repo_root.as_deref());
    build_registered_project_entries(&mut entries);
    build_host_connect_entries(&mut entries);

    entries
}

/// Offer one "Connect: <host>" entry per SSH candidate — `~/.ssh/config`
/// aliases merged with `[hosts.*]` tuning by the shared resolution seam.
fn build_host_connect_entries(entries: &mut Vec<PaletteEntry>) {
    for candidate in crate::ssh_config::candidates() {
        let Some(ssh_target) = candidate.config.ssh_target.clone() else {
            continue;
        };
        let source = match candidate.source {
            crate::ssh_config::HostSource::ConfigToml => "config.toml",
            crate::ssh_config::HostSource::SshConfig => "ssh config",
        };
        entries.push(PaletteEntry {
            label: format!("Connect: {} — {ssh_target} — {source}", candidate.name),
            shortcut: None,
            action: PaletteAction::ConnectHost(candidate.config),
        });
    }
}

/// Build the static core palette entries (tab, pane, workspace, template, view,
/// and session ops). The `has_attention` flag controls placement of the
/// Jump to Attention entry.
fn build_registered_project_entries(entries: &mut Vec<PaletteEntry>) {
    PALETTE_PROJECTS.with(|store| match store.borrow().value.as_ref() {
        Some(Ok(projects)) => entries.extend(projects.iter().cloned().flat_map(|project| {
            let context = match &project.host {
                crate::projects::ProjectHost::Local => "local".to_string(),
                crate::projects::ProjectHost::Ssh { name, .. } => name.clone(),
            };
            [
                PaletteEntry {
                    label: format!(
                        "Project: {} — {} — {} — {}",
                        project.display_name,
                        context,
                        project.canonical_repo,
                        project.checkout_root
                    ),
                    shortcut: None,
                    action: PaletteAction::OpenProject(project.clone()),
                },
                PaletteEntry {
                    label: format!(
                        "Forget project: {} — {} — {}",
                        project.display_name, context, project.checkout_root
                    ),
                    shortcut: None,
                    action: PaletteAction::ForgetProject(project),
                },
            ]
        })),
        Some(Err(error)) => entries.push(PaletteEntry {
            label: format!("Projects unavailable: {error}"),
            shortcut: None,
            action: PaletteAction::ShowError(format!("Could not load projects: {error}")),
        }),
        None => entries.push(PaletteEntry {
            label: "Loading registered projects…".into(),
            shortcut: None,
            action: PaletteAction::Noop,
        }),
    });
}

fn build_core_tab_entries(has_attention: bool) -> Vec<PaletteEntry> {
    use crate::keybindings::{label_for, Action};
    let tmux_ui = crate::terminal::explicit_tmux_tab_ui_state();
    let recent_copy_lines = crate::config::terminal_config().copy_recent_lines;

    let jump_attention_entry = PaletteEntry {
        label: "Jump to Attention Tab".into(),
        shortcut: label_for(Action::JumpAttention),
        action: PaletteAction::Activate(Action::JumpAttention),
    };

    let mut entries = vec![
        PaletteEntry {
            label: "New Tab".into(),
            shortcut: label_for(Action::NewTab),
            action: PaletteAction::Activate(Action::NewTab),
        },
        PaletteEntry {
            label: "Previous Tab".into(),
            shortcut: label_for(Action::PreviousTab),
            action: PaletteAction::Activate(Action::PreviousTab),
        },
        PaletteEntry {
            label: if tmux_ui.available {
                "New tmux-backed Tab…".into()
            } else {
                "New tmux-backed Tab… (tmux unavailable)".into()
            },
            shortcut: label_for(Action::NewTmuxTab),
            action: tmux_ui
                .unavailable_message
                .clone()
                .map(PaletteAction::ShowError)
                .unwrap_or(PaletteAction::Activate(Action::NewTmuxTab)),
        },
        PaletteEntry {
            label: "Search in Terminal".into(),
            shortcut: label_for(Action::SearchToggle),
            action: PaletteAction::Activate(Action::SearchToggle),
        },
        PaletteEntry {
            label: "Toggle Right Dock".into(),
            shortcut: label_for(Action::ToggleDock),
            action: PaletteAction::Activate(Action::ToggleDock),
        },
        PaletteEntry {
            label: "Toggle Compact Sidebar".into(),
            shortcut: label_for(Action::ToggleSidebarCompact),
            action: PaletteAction::Activate(Action::ToggleSidebarCompact),
        },
        PaletteEntry {
            label: format!("Copy Recent Output ({recent_copy_lines} lines)"),
            shortcut: label_for(Action::CopyRecentOutput),
            action: PaletteAction::Activate(Action::CopyRecentOutput),
        },
        PaletteEntry {
            label: "Send to Pane…".into(),
            shortcut: label_for(Action::SendToPane),
            action: PaletteAction::OpenSendToPane { last_output: false },
        },
        PaletteEntry {
            label: "Send Last Output to Pane…".into(),
            shortcut: None,
            action: PaletteAction::OpenSendToPane { last_output: true },
        },
        PaletteEntry {
            label: "Copy Last Agent Message".into(),
            shortcut: label_for(Action::CopyLastMessage),
            action: PaletteAction::Activate(Action::CopyLastMessage),
        },
        PaletteEntry {
            label: "Relay Last Message to Pane…".into(),
            shortcut: None,
            action: PaletteAction::OpenRelayLastMessage,
        },
        PaletteEntry {
            label: "Clipboard History…".into(),
            shortcut: label_for(Action::ClipboardHistory),
            action: PaletteAction::OpenClipboardHistory,
        },
        PaletteEntry {
            label: "Recent Files…".into(),
            shortcut: label_for(Action::RecentFiles),
            action: PaletteAction::OpenRecentFiles,
        },
        PaletteEntry {
            label: "Workspace Inspector".into(),
            shortcut: label_for(Action::WorkspaceInspector),
            action: PaletteAction::Activate(Action::WorkspaceInspector),
        },
        PaletteEntry {
            label: "Split Vertical".into(),
            shortcut: label_for(Action::SplitVertical),
            action: PaletteAction::Activate(Action::SplitVertical),
        },
        PaletteEntry {
            label: "Split Horizontal".into(),
            shortcut: label_for(Action::SplitHorizontal),
            action: PaletteAction::Activate(Action::SplitHorizontal),
        },
        PaletteEntry {
            label: "Close Pane".into(),
            shortcut: label_for(Action::ClosePane),
            action: PaletteAction::Activate(Action::ClosePane),
        },
        PaletteEntry {
            label: "Toggle Pane Zoom".into(),
            shortcut: label_for(Action::TogglePaneZoom),
            action: PaletteAction::Activate(Action::TogglePaneZoom),
        },
        PaletteEntry {
            label: "New Workspace".into(),
            shortcut: label_for(Action::NewWorkspace),
            action: PaletteAction::Activate(Action::NewWorkspace),
        },
        PaletteEntry {
            label: "Save Current Tab as Template".into(),
            shortcut: None,
            action: PaletteAction::SaveCurrentTabTemplate,
        },
        PaletteEntry {
            label: "Save Current Workspace as Template".into(),
            shortcut: None,
            action: PaletteAction::SaveCurrentWorkspaceTemplate,
        },
        PaletteEntry {
            label: "New Tab from Template".into(),
            shortcut: None,
            action: PaletteAction::OpenTabTemplates,
        },
        PaletteEntry {
            label: "New Workspace from Template".into(),
            shortcut: None,
            action: PaletteAction::OpenWorkspaceTemplates,
        },
        PaletteEntry {
            label: "Delete Template".into(),
            shortcut: None,
            action: PaletteAction::DeleteTemplates,
        },
        PaletteEntry {
            label: "Previous Workspace".into(),
            shortcut: label_for(Action::PreviousWorkspace),
            action: PaletteAction::Activate(Action::PreviousWorkspace),
        },
        PaletteEntry {
            label: "Next Workspace".into(),
            shortcut: label_for(Action::NextWorkspace),
            action: PaletteAction::Activate(Action::NextWorkspace),
        },
        PaletteEntry {
            label: "Cycle Previous Workspace".into(),
            shortcut: label_for(Action::PrevWorkspace),
            action: PaletteAction::Activate(Action::PrevWorkspace),
        },
        PaletteEntry {
            label: "Open Dashboard".into(),
            shortcut: None,
            action: PaletteAction::OpenDashboard,
        },
        PaletteEntry {
            label: "Save View: Agent Activity".into(),
            shortcut: None,
            action: PaletteAction::SaveView(crate::views::ViewPreset::AgentActivity),
        },
        PaletteEntry {
            label: "Save View: Workspace Health".into(),
            shortcut: None,
            action: PaletteAction::SaveView(crate::views::ViewPreset::WorkspaceHealth),
        },
        PaletteEntry {
            label: "Save View: Listening Ports".into(),
            shortcut: None,
            action: PaletteAction::SaveView(crate::views::ViewPreset::ListeningPorts),
        },
        PaletteEntry {
            label: "Save View: Recent Alerts".into(),
            shortcut: None,
            action: PaletteAction::SaveView(crate::views::ViewPreset::RecentAlerts),
        },
        PaletteEntry {
            label: "Save View: Currently Flagged Panes".into(),
            shortcut: None,
            action: PaletteAction::SaveView(crate::views::ViewPreset::CurrentlyFlaggedPanes),
        },
        PaletteEntry {
            label: "Save View: Failing Test Runs".into(),
            shortcut: None,
            action: PaletteAction::SaveView(crate::views::ViewPreset::FailingTestRuns),
        },
        PaletteEntry {
            label: "Open Saved View".into(),
            shortcut: None,
            action: PaletteAction::OpenSavedViews,
        },
        PaletteEntry {
            label: "Delete Saved View".into(),
            shortcut: None,
            action: PaletteAction::DeleteSavedViews,
        },
        PaletteEntry {
            label: "Detach Pane".into(),
            shortcut: None,
            action: PaletteAction::DetachPane,
        },
        PaletteEntry {
            label: "Attach Session…".into(),
            shortcut: None,
            action: PaletteAction::AttachSession,
        },
        PaletteEntry {
            label: "Keyboard Shortcuts".into(),
            shortcut: None,
            action: PaletteAction::OpenKeybindingHelp,
        },
    ];

    // Promote the jump-attention entry to the top when something is flagged,
    // otherwise tuck it among the navigation commands.
    let jump_attention_index = if has_attention { 0 } else { 5 };
    entries.insert(jump_attention_index, jump_attention_entry);
    entries
}

/// Append broadcast input entries (tab scope, workspace scope, off) to `entries`.
fn build_broadcast_entries(entries: &mut Vec<PaletteEntry>, st: &AppState) {
    use crate::keybindings::{label_for, Action};
    let broadcast_scope = st.broadcast_scope();
    entries.push(PaletteEntry {
        label: match broadcast_scope {
            Some(crate::BroadcastScope::Tab) => "Broadcast Input: Active Tab (Current)".into(),
            _ => "Broadcast Input: Active Tab".into(),
        },
        // Show the chord for the action this row actually fires, not for
        // ToggleBroadcastInput. The two are different verbs — toggle flips
        // broadcast on/off, this row sets tab scope — and now that the scope
        // actions are bindable, borrowing the toggle's chord would display a
        // key that does something else.
        shortcut: label_for(Action::BroadcastTab),
        action: PaletteAction::Activate(Action::BroadcastTab),
    });
    entries.push(PaletteEntry {
        label: match broadcast_scope {
            Some(crate::BroadcastScope::Workspace) => {
                "Broadcast Input: Active Workspace (Current)".into()
            }
            _ => "Broadcast Input: Active Workspace".into(),
        },
        shortcut: label_for(Action::BroadcastWorkspace),
        action: PaletteAction::Activate(Action::BroadcastWorkspace),
    });
    if broadcast_scope.is_some() {
        entries.push(PaletteEntry {
            label: "Broadcast Input: Off".into(),
            shortcut: label_for(Action::BroadcastOff),
            action: PaletteAction::Activate(Action::BroadcastOff),
        });
    }
}

/// Append "Switch to: <tab>" and "Move current tab -> <ws>" entries.
fn build_tab_switch_entries(entries: &mut Vec<PaletteEntry>, st: &AppState) {
    for tab in st.all_tabs() {
        entries.push(PaletteEntry {
            label: format!("Switch to: {}", tab.name),
            shortcut: None,
            action: PaletteAction::SwitchTab(tab.id),
        });
    }
    if st.active_tab().is_some() {
        for ws in st
            .workspaces
            .iter()
            .filter(|ws| ws.id != st.active_workspace)
        {
            entries.push(PaletteEntry {
                label: format!("Move current tab -> {}", ws.name),
                shortcut: None,
                action: PaletteAction::MoveCurrentTabToWorkspace(ws.id),
            });
        }
    }
}

/// Append one "Quick Action" and one "Legacy" entry per discovered workspace
/// task. When there are no discovered actions, appends a Discover Tasks prompt.
fn build_discovered_task_entries(
    entries: &mut Vec<PaletteEntry>,
    discovered_actions: &[crate::workspace::WorkspaceAction],
    source_tab_id: u32,
    discovery: crate::task_launch::TaskDiscoverySnapshot,
) {
    for action in discovered_actions {
        let request = palette_task_request(
            source_tab_id,
            Some(discovery.clone()),
            action.task_name(),
            Some(*action),
        );
        entries.push(PaletteEntry {
            label: format!("Quick Action: {}", action.label()),
            shortcut: None,
            action: PaletteAction::WorkspaceAction(request.clone()),
        });
        entries.push(PaletteEntry {
            label: format!(
                "Legacy Alias: Run {} in Focused Pane (Dedicated Tab)",
                action.label()
            ),
            shortcut: None,
            action: PaletteAction::WorkspaceActionInFocusedPane(request),
        });
    }
    if discovered_actions.is_empty() {
        entries.push(PaletteEntry {
            label: "Discover Tasks (Ctrl+Shift+D)".to_string(),
            shortcut: None,
            action: PaletteAction::DiscoverTab,
        });
    }
}

/// Append mise task entries from the async discovery cache, skipping any
/// tasks already listed in `standard_task_names`.
fn build_mise_task_entries(
    entries: &mut Vec<PaletteEntry>,
    source_tab_id: u32,
    discovery: crate::task_launch::TaskDiscoverySnapshot,
    standard_task_names: &[&str],
) {
    match crate::mise::cached_task_discovery(discovery.target()) {
        crate::mise::CachedTaskDiscovery::Ready(tasks) => {
            for task in tasks {
                // The cache can advance after the sidebar applies its discovery
                // result. Keep the affordance bound to the task set discovered for
                // this target; the planner receives that same identity.
                if !discovery.contains_task(&task.name)
                    || standard_task_names.contains(&task.name.as_str())
                {
                    continue;
                }
                let label = if task.description.is_empty() {
                    format!("mise: {}", task.name)
                } else {
                    format!("mise: {} — {}", task.name, task.description)
                };
                let request = palette_task_request(
                    source_tab_id,
                    Some(discovery.clone()),
                    task.name.clone(),
                    None,
                );
                entries.push(PaletteEntry {
                    label,
                    shortcut: None,
                    action: PaletteAction::MiseTask(request),
                });
                entries.push(PaletteEntry {
                    label: format!(
                        "Legacy Alias: Run mise {} in Focused Pane (Dedicated Tab)",
                        task.name
                    ),
                    shortcut: None,
                    action: PaletteAction::MiseTaskInFocusedPane(palette_task_request(
                        source_tab_id,
                        Some(discovery.clone()),
                        task.name.clone(),
                        None,
                    )),
                });
            }
        }
        crate::mise::CachedTaskDiscovery::Pending => entries.push(PaletteEntry {
            label: "mise: discovering tasks...".into(),
            shortcut: None,
            action: PaletteAction::Noop,
        }),
        crate::mise::CachedTaskDiscovery::Missing => {}
    }
}

/// GTK-free half of a palette task entry callback.
pub(crate) fn palette_task_request(
    source_tab_id: u32,
    discovery: Option<crate::task_launch::TaskDiscoverySnapshot>,
    task_name: impl Into<String>,
    workspace_action: Option<crate::workspace::WorkspaceAction>,
) -> TaskLaunchRequest {
    TaskLaunchRequest::from_snapshot(
        crate::task_launch::TaskLaunchSurface::Palette,
        source_tab_id,
        discovery,
        task_name,
        workspace_action,
    )
}

/// Append Create/List/Remove Worktree entries when a repo root is present.
fn build_worktree_palette_entries(entries: &mut Vec<PaletteEntry>, repo_root: Option<&Path>) {
    if repo_root.is_some() {
        entries.push(PaletteEntry {
            label: "Create Worktree".into(),
            shortcut: None,
            action: PaletteAction::CreateWorktree,
        });
        entries.push(PaletteEntry {
            label: "List Worktrees".into(),
            shortcut: None,
            action: PaletteAction::ListWorktrees,
        });
        entries.push(PaletteEntry {
            label: "Remove Worktree".into(),
            shortcut: None,
            action: PaletteAction::RemoveWorktree,
        });
    }
}

fn make_row(entry: &PaletteEntry) -> gtk::ListBoxRow {
    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    hbox.set_margin_start(8);
    hbox.set_margin_end(8);
    hbox.set_margin_top(4);
    hbox.set_margin_bottom(4);

    let label = gtk::Label::new(Some(&entry.label));
    label.set_halign(gtk::Align::Start);
    label.set_hexpand(true);
    // Only clipboard-history previews contain newlines; render them across up to
    // three wrapped, ellipsized lines so other rows are unaffected.
    if entry.label.contains('\n') {
        label.set_wrap(true);
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_lines(3);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_xalign(0.0);
    }
    hbox.append(&label);

    if let Some(ref shortcut) = entry.shortcut {
        let sc_label = gtk::Label::new(Some(shortcut));
        sc_label.add_css_class("palette-shortcut");
        sc_label.set_halign(gtk::Align::End);
        hbox.append(&sc_label);
    }

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&hbox));
    row
}

fn wire_project_tab(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
    tab_id: u32,
    name: &str,
) {
    crate::sidebar::add_tab_row(tab_list, state, term_stack, tab_id, name, true);
    crate::terminal::wire_tab_terminals(state, term_stack, tab_list, window, tab_id);
}

fn confirm_forget_project(
    project: &crate::projects::RegisteredProject,
    window: &adw::ApplicationWindow,
) {
    let host = match &project.host {
        crate::projects::ProjectHost::Local => "local".to_string(),
        crate::projects::ProjectHost::Ssh { name, .. } => name.clone(),
    };
    let dialog = gtk::Dialog::builder()
        .transient_for(window)
        .modal(true)
        .title("Forget registered project?")
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Forget", gtk::ResponseType::Accept);
    let label = gtk::Label::new(Some(&format!(
        "Remove {} on {} at {} from the registered project list? The checkout will not be changed.",
        project.display_name, host, project.checkout_root
    )));
    label.set_wrap(true);
    label.set_margin_top(12);
    label.set_margin_bottom(12);
    label.set_margin_start(12);
    label.set_margin_end(12);
    dialog.content_area().append(&label);
    let id = project.id.clone();
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Accept {
            let worker_id = id.clone();
            let submission = crate::git::spawn_async_result(
                format!("palette-forget-project:{worker_id}"),
                move || crate::projects::forget(&worker_id).map_err(|error| error.to_string()),
                move |result| match result {
                    Ok(true) => {
                        invalidate_palette_projects();
                        crate::show_toast("Registered project forgotten")
                    }
                    Ok(false) => crate::show_error_toast("Registered project was not found"),
                    Err(error) => {
                        crate::show_error_toast(&format!("Could not forget project: {error}"));
                    }
                },
            );
            match submission {
                crate::git::GitAsyncSubmission::Started => {}
                crate::git::GitAsyncSubmission::Coalesced => {
                    crate::show_toast("Project removal is already running")
                }
                crate::git::GitAsyncSubmission::Saturated => {
                    crate::show_error_toast("Project update queue is busy; retry shortly")
                }
            }
        }
        dialog.close();
    });
    dialog.present();
}

fn mark_project_opened(project_id: &str) {
    let worker_project_id = project_id.to_string();
    let submission = crate::git::spawn_async_result(
        format!("palette-mark-project-opened:{worker_project_id}"),
        move || crate::projects::mark_opened(&worker_project_id).map_err(|error| error.to_string()),
        move |result| {
            if let Err(error) = result {
                crate::show_error_toast(&format!("Could not update project history: {error}"));
            }
        },
    );
    match submission {
        crate::git::GitAsyncSubmission::Started => {}
        crate::git::GitAsyncSubmission::Coalesced => {
            crate::show_toast("Project history update is already running")
        }
        crate::git::GitAsyncSubmission::Saturated => {
            crate::show_error_toast("Project history queue is busy; retry shortly")
        }
    }
}

/// Open a tmux-backed tab on an SSH candidate, reusing the same validated tmux
/// path the tmux-tab dialog and remote projects use.
fn dispatch_connect_host(
    host: &crate::host::HostConfig,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    let host = host.clone();
    let state_for_validation = state.clone();
    let window_for_validation = window.clone();
    let state = Rc::downgrade(state);
    let window = window.downgrade();
    let term_stack = term_stack.clone();
    let tab_list = tab_list.clone();
    crate::terminal::validate_explicit_tmux_target_async(
        &state_for_validation,
        &window_for_validation,
        Some(host.clone()),
        move |result| {
            let (Some(state), Some(window)) = (state.upgrade(), window.upgrade()) else {
                return;
            };
            match result {
                Ok(()) => {
                    let runtime = RuntimeHandle::from_shared_state(state);
                    crate::terminal::open_explicit_tmux_tab_after_validation(
                        &runtime,
                        &term_stack,
                        &tab_list,
                        &window,
                        &host.name,
                        None,
                        Some(&host),
                    );
                }
                Err(error) => {
                    crate::show_error_toast(&format!("Could not connect to {}: {error}", host.name))
                }
            }
        },
    );
}

fn open_registered_project(
    project: &crate::projects::RegisteredProject,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    let plan = match crate::projects::build_open_plan(project) {
        Ok(plan) => plan,
        Err(error) => {
            crate::show_error_toast(&format!("Could not open project: {error}"));
            return;
        }
    };
    let runtime = RuntimeHandle::from_shared_state(state.clone());
    match plan {
        crate::projects::ProjectOpenPlan::Local { working_dir, tmux } => {
            if tmux {
                let state_for_validation = state.clone();
                let window_for_validation = window.clone();
                let state = Rc::downgrade(state);
                let window = window.downgrade();
                let term_stack = term_stack.clone();
                let tab_list = tab_list.clone();
                let project_id = project.id.clone();
                let display_name = project.display_name.clone();
                crate::terminal::validate_explicit_tmux_target_async(
                    &state_for_validation,
                    &window_for_validation,
                    None,
                    move |result| {
                        let (Some(state), Some(window)) = (state.upgrade(), window.upgrade())
                        else {
                            return;
                        };
                        match result {
                            Ok(()) => {
                                let runtime = RuntimeHandle::from_shared_state(state);
                                crate::terminal::open_explicit_tmux_tab_after_validation(
                                    &runtime,
                                    &term_stack,
                                    &tab_list,
                                    &window,
                                    &display_name,
                                    Some(&working_dir),
                                    None,
                                );
                                mark_project_opened(&project_id);
                            }
                            Err(error) => {
                                crate::show_error_toast(&format!("Could not open project: {error}"))
                            }
                        }
                    },
                );
            } else {
                let tab_id = crate::terminal::create_terminal(
                    &runtime,
                    term_stack,
                    &project.display_name,
                    Some(&working_dir),
                    None,
                );
                wire_project_tab(
                    state,
                    tab_list,
                    term_stack,
                    window,
                    tab_id,
                    &project.display_name,
                );
                mark_project_opened(&project.id);
            }
        }
        crate::projects::ProjectOpenPlan::Remote {
            host_name,
            ssh_target,
            connection_argv,
            working_dir,
            tmux,
        } => {
            if tmux {
                let host = crate::host::HostConfig {
                    name: host_name.clone(),
                    ssh_target: Some(ssh_target),
                    ..crate::host::HostConfig::default()
                };
                let state_for_validation = state.clone();
                let window_for_validation = window.clone();
                let state = Rc::downgrade(state);
                let window = window.downgrade();
                let term_stack = term_stack.clone();
                let tab_list = tab_list.clone();
                let project_id = project.id.clone();
                let display_name = project.display_name.clone();
                crate::terminal::validate_explicit_tmux_target_async(
                    &state_for_validation,
                    &window_for_validation,
                    Some(host.clone()),
                    move |result| {
                        let (Some(state), Some(window)) = (state.upgrade(), window.upgrade())
                        else {
                            return;
                        };
                        match result {
                            Ok(()) => {
                                let runtime = RuntimeHandle::from_shared_state(state);
                                crate::terminal::open_explicit_tmux_tab_after_validation(
                                    &runtime,
                                    &term_stack,
                                    &tab_list,
                                    &window,
                                    &display_name,
                                    Some(&working_dir),
                                    Some(&host),
                                );
                                mark_project_opened(&project_id);
                            }
                            Err(error) => {
                                crate::show_error_toast(&format!("Could not open project: {error}"))
                            }
                        }
                    },
                );
            } else {
                match crate::projects::remote_shell_argv(
                    &ssh_target,
                    connection_argv.as_deref(),
                    &working_dir,
                ) {
                    Ok(argv) => {
                        let tab_id = crate::terminal::create_terminal_with_command(
                            &runtime,
                            term_stack,
                            &project.display_name,
                            None,
                            argv,
                        );
                        if let Some(pane) = state
                            .borrow_mut()
                            .find_tab_mut(tab_id)
                            .and_then(|tab| tab.panes.leaf_mut(tab.focused_pane_id))
                        {
                            pane.location_state.cwd = Some(working_dir.clone());
                            pane.location_state.cwd_host = Some(host_name.clone());
                        }
                        wire_project_tab(
                            state,
                            tab_list,
                            term_stack,
                            window,
                            tab_id,
                            &project.display_name,
                        );
                        mark_project_opened(&project.id);
                    }
                    Err(error) => {
                        crate::show_error_toast(&format!("Could not open project: {error}"));
                    }
                }
            }
        }
        crate::projects::ProjectOpenPlan::Coder {
            list_command,
            create_if_missing,
            create_command,
            connect_command,
        } => {
            let coder = project
                .coder
                .clone()
                .expect("validated Coder plan has binding");
            let state = state.clone();
            let tab_list = tab_list.clone();
            let term_stack = term_stack.clone();
            let window = window.clone();
            let name = project.display_name.clone();
            let project_id = project.id.clone();
            glib::spawn_future_local(async move {
                let workspace_name = coder.workspace_name.clone();
                let preflight = gio::spawn_blocking(move || {
                    let output = crate::terminal::run_tmux_command_sync_result_without_diagnostics(
                        &list_command,
                    )
                    .map_err(|_| "coder list failed".to_string())?;
                    if crate::projects::coder_workspace_exists(&output, &workspace_name)? {
                        return Ok(());
                    }
                    if !create_if_missing {
                        return Err(
                            "Coder workspace is missing and guarded creation is disabled"
                                .to_string(),
                        );
                    }
                    let create = create_command
                        .ok_or_else(|| "guarded Coder creation requires a template".to_string())?;
                    crate::projects::run_guarded_coder_create(&create)
                })
                .await
                .unwrap_or_else(|_| Err("Coder project preflight failed".to_string()));
                if let Err(error) = preflight {
                    crate::show_error_toast(&format!("Could not open project: {error}"));
                    return;
                }
                let runtime = RuntimeHandle::from_shared_state(state.clone());
                let tab_id = crate::terminal::create_terminal_with_command(
                    &runtime,
                    &term_stack,
                    &name,
                    None,
                    connect_command,
                );
                wire_project_tab(&state, &tab_list, &term_stack, &window, tab_id, &name);
                mark_project_opened(&project_id);
                if let Some(terminal) = state
                    .borrow()
                    .find_tab(tab_id)
                    .and_then(|(_, tab)| tab.panes.leaf(tab.focused_pane_id))
                    .map(|pane| pane.terminal.clone())
                {
                    terminal.grab_focus();
                }
            });
        }
    }
}

fn spawn_task_tab(
    plan: TaskLaunchPlan,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) -> Result<(), TaskLaunchError> {
    let TaskLaunchPlan {
        tab_name,
        working_dir,
        argv,
        respawn_on_exit,
        workspace_action,
        target_label,
    } = plan;
    let runtime = RuntimeHandle::from_shared_state(state.clone());
    let tab_id = crate::terminal::create_terminal_with_command_checked(
        &runtime,
        term_stack,
        &tab_name,
        working_dir.as_deref(),
        argv,
    )
    .map_err(|reason| TaskLaunchError::TaskTabStartFailed {
        target: target_label,
        reason,
    })?;
    {
        let mut st = state.borrow_mut();
        if let Some(tab) = st.find_tab_mut(tab_id) {
            tab.respawn_on_exit = respawn_on_exit;
            tab.workspace_action = workspace_action;
        }
        if workspace_action.is_some() {
            if let Some(ws) = st.active_ws_mut() {
                ws.run_status = crate::workspace::WorkspaceStatus::Running;
            }
        }
    }
    crate::sidebar::add_tab_row(tab_list, state, term_stack, tab_id, &tab_name, true);
    crate::terminal::wire_tab_terminals(state, term_stack, tab_list, window, tab_id);
    Ok(())
}

/// GTK adapter for the widget-free task-launch executor. Every UI entry
/// surface reaches this through [`launch_task`]; the planner itself has no API
/// for writing into an existing terminal.
struct GtkTaskTabExecutor<'a> {
    state: &'a Rc<RefCell<AppState>>,
    tab_list: &'a gtk::Box,
    term_stack: &'a gtk::Stack,
    window: &'a adw::ApplicationWindow,
}

impl TaskLaunchExecutor for GtkTaskTabExecutor<'_> {
    fn open_dedicated_task_tab(&mut self, plan: TaskLaunchPlan) -> Result<(), TaskLaunchError> {
        spawn_task_tab(
            plan,
            self.state,
            self.tab_list,
            self.term_stack,
            self.window,
        )
    }
}

/// Materialize a safe task plan in GTK. Callers receive the same actionable
/// planner error regardless of which task affordance they came from.
pub(crate) fn launch_task(
    request: &TaskLaunchRequest,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) -> Result<(), TaskLaunchError> {
    let plan = {
        let st = state.borrow();
        request.plan_for_current_tab(&st)?
    };
    GtkTaskTabExecutor {
        state,
        tab_list,
        term_stack,
        window,
    }
    .open_dedicated_task_tab(plan)
}

/// Dispatch a palette action. Returns `true` if the palette should stay open.
///
/// The match is exhaustive over `PaletteAction` — there is intentionally no
/// catch-all `_` arm so the compiler enforces that every new variant is handled.
fn execute_action(
    action: &PaletteAction,
    palette: &CommandPalette,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) -> bool {
    match action {
        // ── Navigation / UI ──────────────────────────────────────────────────
        PaletteAction::Activate(action_name) => {
            dispatch_activate(*action_name, window);
            false
        }
        PaletteAction::Noop => true,
        PaletteAction::ShowError(message) => {
            crate::show_error_toast(message);
            false
        }
        PaletteAction::OpenProject(project) => {
            open_registered_project(project, state, tab_list, term_stack, window);
            false
        }
        PaletteAction::ForgetProject(project) => {
            confirm_forget_project(project, window);
            false
        }
        PaletteAction::ConnectHost(host) => {
            dispatch_connect_host(host, state, tab_list, term_stack, window);
            false
        }
        PaletteAction::ResetMode => {
            *palette.mode.borrow_mut() = PaletteMode::Default;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }

        // ── Tab operations ───────────────────────────────────────────────────
        PaletteAction::SwitchTab(tab_id) => {
            dispatch_switch_tab(*tab_id, state, tab_list, term_stack);
            false
        }
        PaletteAction::MoveCurrentTabToWorkspace(target_ws_id) => {
            dispatch_move_tab_to_workspace(*target_ws_id, state, tab_list, term_stack);
            false
        }
        PaletteAction::DiscoverTab => {
            dispatch_discover_tab(state, tab_list);
            false
        }

        // ── Task operations ──────────────────────────────────────────────────
        PaletteAction::MiseTask(request) | PaletteAction::WorkspaceAction(request) => {
            dispatch_task_new_tab(request, state, tab_list, term_stack, window);
            false
        }
        PaletteAction::MiseTaskInFocusedPane(request)
        | PaletteAction::WorkspaceActionInFocusedPane(request) => {
            dispatch_task_focused_pane(request, state, tab_list, term_stack, window);
            false
        }

        // ── Template operations ──────────────────────────────────────────────
        PaletteAction::SaveCurrentTabTemplate => {
            show_save_tab_template_dialog(state, window);
            false
        }
        PaletteAction::SaveCurrentWorkspaceTemplate => {
            show_save_workspace_template_dialog(state, window);
            false
        }
        PaletteAction::OpenTabTemplates => {
            *palette.mode.borrow_mut() = PaletteMode::TemplateTabList;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }
        PaletteAction::OpenWorkspaceTemplates => {
            *palette.mode.borrow_mut() = PaletteMode::TemplateWorkspaceList;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }
        PaletteAction::OpenTabTemplate(name) => {
            open_tab_template(name, state, tab_list, term_stack, window);
            false
        }
        PaletteAction::OpenWorkspaceTemplate(name) => {
            open_workspace_template(name, state, tab_list, term_stack, window);
            false
        }
        PaletteAction::DeleteTemplates => {
            *palette.mode.borrow_mut() = PaletteMode::TemplateDeleteList;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }
        PaletteAction::DeleteTemplate(kind, name) => {
            show_delete_template_dialog(*kind, name, window);
            false
        }

        // ── Worktree operations ──────────────────────────────────────────────
        PaletteAction::CreateWorktree => {
            show_create_worktree_dialog(state, tab_list, term_stack, window);
            false
        }
        PaletteAction::ListWorktrees => {
            invalidate_worktree_listing(palette);
            *palette.mode.borrow_mut() = PaletteMode::WorktreeList;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }
        PaletteAction::RemoveWorktree => {
            invalidate_worktree_listing(palette);
            *palette.mode.borrow_mut() = PaletteMode::WorktreeRemove;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }
        PaletteAction::OpenWorktree(path) => {
            let _ = open_worktree_workspace(path, state, tab_list, term_stack, window);
            false
        }
        PaletteAction::DeleteWorktree(path, repo_root) => {
            show_remove_worktree_dialog(
                path,
                Some(repo_root.clone()),
                state,
                tab_list,
                term_stack,
                window,
            );
            false
        }

        // ── Dashboard / view operations ──────────────────────────────────────
        PaletteAction::OpenDashboard => {
            crate::dashboard::open_dashboard(state, term_stack, tab_list, window);
            false
        }
        PaletteAction::SaveView(preset) => {
            show_save_view_dialog(*preset, state, term_stack, window);
            false
        }
        PaletteAction::OpenSavedViews => {
            *palette.mode.borrow_mut() = PaletteMode::SavedViewList;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }
        PaletteAction::OpenSavedView(name) => {
            crate::dashboard::open_saved_view(name, state, term_stack, tab_list, window);
            false
        }
        PaletteAction::DeleteSavedViews => {
            *palette.mode.borrow_mut() = PaletteMode::SavedViewDeleteList;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }
        PaletteAction::DeleteSavedView(name) => {
            show_delete_saved_view_dialog(name, state, term_stack, window);
            false
        }

        // ── Session / pane operations ────────────────────────────────────────
        PaletteAction::DetachPane => {
            dispatch_detach_pane(state, tab_list, term_stack, window);
            false
        }
        PaletteAction::AttachSession => {
            *palette.mode.borrow_mut() = PaletteMode::AttachSessionList;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }
        PaletteAction::OpenKeybindingHelp => {
            *palette.mode.borrow_mut() = PaletteMode::KeybindingHelp;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }
        PaletteAction::AttachSessionTarget {
            session_name,
            target,
        } => {
            crate::terminal::attach_session_async(
                state,
                term_stack,
                tab_list,
                window,
                session_name,
                target,
                |result| {
                    if let Err(err) = result {
                        crate::show_error_toast(&format!("Attach failed: {err}"));
                    }
                },
            );
            false
        }
        PaletteAction::FocusTab(tab_id) => {
            dispatch_switch_tab(*tab_id, state, tab_list, term_stack);
            false
        }

        // ── Send to pane ─────────────────────────────────────────────────────
        PaletteAction::OpenSendToPane { last_output } => {
            *palette.send_source.borrow_mut() = capture_send_source(state, *last_output);
            *palette.mode.borrow_mut() = PaletteMode::SendToPane;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }
        PaletteAction::OpenRelayLastMessage => {
            *palette.send_source.borrow_mut() = Some(relay_last_message_source(state));
            *palette.mode.borrow_mut() = PaletteMode::SendToPane;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }
        PaletteAction::SendToPane {
            tab_id,
            pane_id,
            run,
        } => {
            dispatch_send_to_pane(palette, state, *tab_id, *pane_id, *run);
            false
        }

        // ── Clipboard history ────────────────────────────────────────────────
        PaletteAction::OpenClipboardHistory => {
            *palette.send_source.borrow_mut() = None;
            *palette.mode.borrow_mut() = PaletteMode::ClipboardHistory;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }
        PaletteAction::CopyClipboardEntry(text) => {
            if let Err(e) = crate::terminal::copy_ring_entry_to_clipboard(text) {
                crate::show_error_toast(&e);
            }
            false
        }
        PaletteAction::OpenSendToPaneWithText(text) => {
            *palette.send_source.borrow_mut() = Some(SendPayloadSource::SelectionOrClipboard {
                selection: Some(text.clone()),
            });
            *palette.mode.borrow_mut() = PaletteMode::SendToPane;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }

        // ── Recent files ─────────────────────────────────────────────────────
        PaletteAction::OpenRecentFiles => {
            *palette.mode.borrow_mut() = PaletteMode::RecentFiles;
            palette.entry.set_text("");
            repopulate_palette_list(palette, state, "");
            true
        }
        PaletteAction::OpenRecentFile(path) => {
            // Open the recent file in the in-app peek overlay. peek_path defers
            // its grab_focus so the palette's synchronous refocus-on-close does
            // not steal keyboard focus from the overlay.
            crate::peek::peek_path(path, None);
            false
        }
    }
}

/// Snapshot the payload a "send to pane" picker should deliver. For `last_output`
/// it captures the active pane's recent output now; otherwise it captures the
/// current selection (clipboard is read later, at delivery time).
fn capture_send_source(
    state: &Rc<RefCell<AppState>>,
    last_output: bool,
) -> Option<SendPayloadSource> {
    if last_output {
        let terminal = crate::get_active_terminal(state)?;
        let line_count = crate::config::terminal_config().copy_recent_lines;
        crate::terminal::capture_last_terminal_lines(&terminal, line_count)
            .ok()
            .map(|(text, _rows)| SendPayloadSource::LastOutput(text))
    } else {
        Some(SendPayloadSource::SelectionOrClipboard {
            selection: crate::terminal::active_selection_text(state),
        })
    }
}

/// A short label describing the snapshotted payload source, shown as a
/// non-selectable header in the picker.
fn send_source_label(source: Option<&SendPayloadSource>) -> String {
    match source {
        Some(SendPayloadSource::LastOutput(_)) => "Sending: recent output".to_string(),
        Some(SendPayloadSource::LastMessage(_)) => "Sending: last agent message".to_string(),
        Some(SendPayloadSource::SelectionOrClipboard { selection: Some(_) }) => {
            "Sending: current selection".to_string()
        }
        Some(SendPayloadSource::SelectionOrClipboard { selection: None }) => {
            "Sending: clipboard text".to_string()
        }
        None => "Sending: current selection or clipboard".to_string(),
    }
}

/// Deliver the snapshotted payload to `(tab_id, pane_id)`. Text that was already
/// captured (selection or recent output) is delivered synchronously; when only
/// the clipboard is available it is read asynchronously and delivered from the
/// callback (the pane is re-resolved then, and may have closed).
fn dispatch_send_to_pane(
    palette: &CommandPalette,
    state: &Rc<RefCell<AppState>>,
    tab_id: u32,
    pane_id: u32,
    run: bool,
) {
    let mode = if run {
        crate::terminal::SendToPaneMode::Run
    } else {
        crate::terminal::SendToPaneMode::Insert
    };
    let source = palette.send_source.borrow().clone();

    let inline_text = match source {
        Some(SendPayloadSource::LastOutput(text))
        | Some(SendPayloadSource::LastMessage(text))
        | Some(SendPayloadSource::SelectionOrClipboard {
            selection: Some(text),
        }) => Some(text),
        Some(SendPayloadSource::SelectionOrClipboard { selection: None }) | None => None,
    };

    if let Some(text) = inline_text {
        match crate::terminal::build_send_to_pane_payload(Some(&text), None, mode) {
            Some(bytes) => {
                if let Err(err) =
                    crate::terminal::send_bytes_to_pane(state, tab_id, pane_id, &bytes)
                {
                    crate::show_error_toast(&format!("Send to pane failed: {err}"));
                }
            }
            None => crate::show_error_toast("Nothing to send: the source text was empty."),
        }
        return;
    }

    // No captured text — fall back to the clipboard, read asynchronously.
    let Some(display) = gdk::Display::default() else {
        crate::show_error_toast("Nothing to send: no selection or clipboard text.");
        return;
    };
    let state = state.clone();
    display
        .clipboard()
        .read_text_async(gio::Cancellable::NONE, move |res| {
            let text = res.ok().flatten().map(|value| value.to_string());
            match crate::terminal::build_send_to_pane_payload(text.as_deref(), None, mode) {
                Some(bytes) => {
                    if let Err(err) =
                        crate::terminal::send_bytes_to_pane(&state, tab_id, pane_id, &bytes)
                    {
                        crate::show_error_toast(&format!("Send to pane failed: {err}"));
                    }
                }
                None => crate::show_error_toast("Nothing to send: no selection or clipboard text."),
            }
        });
}

// ── execute_action dispatch helpers ──────────────────────────────────────────

/// Activate a GAction by its "win.<name>" string.
fn dispatch_activate(action: crate::keybindings::Action, window: &adw::ApplicationWindow) {
    let gaction_name = action.gaction_name();
    if let Some(name) = gaction_name.strip_prefix("win.") {
        if let Some(gaction) = window.lookup_action(name) {
            if let Some(simple) = gaction.downcast_ref::<gio::SimpleAction>() {
                simple.activate(None);
                return;
            }
        }
    }
    // The typed enum guarantees the *name* exists; it cannot guarantee the
    // window registered a handler for it. Say so rather than no-oping silently.
    eprintln!("taarof: no window action registered for {gaction_name}; palette entry did nothing");
}

/// Switch to the tab identified by `tab_id` and focus its terminal.
fn dispatch_switch_tab(
    tab_id: u32,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
) {
    if sidebar::activate_tab(tab_list, state, term_stack, tab_id) {
        if let Some(terminal) = crate::get_active_terminal(state) {
            terminal.grab_focus();
        }
    }
}

/// Move the active tab into `target_ws_id`.
fn dispatch_move_tab_to_workspace(
    target_ws_id: u32,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
) {
    let tab_id = {
        let st = state.borrow();
        let Some(tab) = st.active_tab() else { return };
        tab.id
    };
    if let Err(err) =
        sidebar::move_tab_to_workspace(tab_list, state, term_stack, tab_id, target_ws_id)
    {
        crate::show_error_toast(&format!("Could not move tab to workspace: {err:?}"));
    }
}

/// Trigger task discovery for the active tab.
fn dispatch_discover_tab(state: &Rc<RefCell<AppState>>, tab_list: &gtk::Box) {
    let tab_id = {
        let st = state.borrow();
        st.active_tab().map(|t| t.id)
    };
    if let Some(tab_id) = tab_id {
        crate::task_panel::discover_tasks_explicitly(state, tab_list, tab_id);
    }
}

/// Launch a task in a new tab. Shows an error toast if the plan cannot be built.
fn dispatch_task_new_tab(
    request: &TaskLaunchRequest,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    if let Err(error) = launch_task(request, state, tab_list, term_stack, window) {
        crate::show_error_toast(&error.to_string());
    }
}

/// "FocusedPane" entries are legacy search-only aliases kept for muscle-memory
/// compatibility. They now open a fresh terminal tab (same as
/// `dispatch_task_new_tab`) instead of injecting a command into the active
/// shell, which could disrupt in-progress work. See #23.
fn dispatch_task_focused_pane(
    request: &TaskLaunchRequest,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    dispatch_task_new_tab(request, state, tab_list, term_stack, window);
}

/// Detach the focused pane of the active tab.
fn dispatch_detach_pane(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    let ids = {
        let st = state.borrow();
        st.active_tab().map(|t| (t.id, t.focused_pane_id))
    };
    if let Some((tab_id, pane_id)) = ids {
        if let Err(err) = crate::terminal::detach_pane(state, term_stack, tab_list, tab_id, pane_id)
        {
            crate::show_error_toast(&format!("Detach failed: {err}"));
        } else {
            crate::sidebar::refresh_background_section(tab_list, state, term_stack, window);
        }
    }
}

/// Collect this instance's tabs and their tmux-backed pane sessions, so the
/// attach picker can resolve "already attached here" and offer a focus action.
fn live_tab_backings(st: &AppState) -> Vec<LiveTabBacking> {
    st.all_tabs()
        .map(|tab| {
            let backings = tab
                .panes
                .leaves()
                .into_iter()
                .filter_map(|leaf| {
                    leaf.tmux_backing
                        .as_ref()
                        .map(|b| (b.session_name.clone(), b.target.clone()))
                })
                .collect::<Vec<_>>();
            (tab.id, backings)
        })
        .filter(|(_, backings)| !backings.is_empty())
        .collect()
}

/// Format a single attach-candidate row label:
/// `<host> · <session>  [attached|detached]  — <command>`.
fn attach_candidate_label(candidate: &AttachCandidate) -> String {
    let state = if matches!(candidate.kind, AttachCandidateKind::AttachedHere { .. }) {
        "open here"
    } else if candidate.attached {
        "attached"
    } else {
        "detached"
    };
    let mut label = format!(
        "{} · {}  [{}]",
        candidate.host, candidate.session_name, state
    );
    if let Some(cmd) = candidate
        .command
        .as_ref()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
    {
        label.push_str(&format!("  — {cmd}"));
    }
    label
}

/// Build the second-level "Attach session" picker entries. Lists every
/// de-duplicated candidate from `detached_sessions` + `dashboard_state.sessions`,
/// with an explicit dead-end message when nothing is available.
/// One candidate destination for "send to pane", with a display label built
/// from the cached runtime probe (agent instance label + pane id + activity).
struct SendTargetPane {
    tab_id: u32,
    pane_id: u32,
    label: String,
}

/// Human-readable activity label for a pane, matching the socket/HTTP vocabulary
/// (see `api::agent_activity_state_label`).
fn send_activity_label(state: crate::workspace::AgentActivityState) -> &'static str {
    use crate::workspace::AgentActivityState;
    match state {
        AgentActivityState::Idle => "idle",
        AgentActivityState::Running => "running",
        AgentActivityState::WaitingInput => "waiting-input",
        AgentActivityState::Errored => "errored",
        AgentActivityState::Done => "done",
    }
}

/// Enumerate every pane in the active workspace as a send target, labelling each
/// from the CACHED `runtime_probe` snapshot (never a fresh probe) exactly as the
/// sidebar does, so labels match what the user already sees.
fn send_target_panes(st: &AppState) -> Vec<SendTargetPane> {
    let Some(ws) = st.active_ws() else {
        return Vec::new();
    };

    let mut panes = Vec::new();
    for tab in &ws.tabs {
        let instances = st
            .runtime_probe
            .as_ref()
            .map(|snapshot| {
                let ordered: Vec<(u32, crate::agents::AgentStatus)> = tab
                    .panes
                    .leaves()
                    .into_iter()
                    .filter_map(|leaf| {
                        snapshot
                            .pane_agents
                            .get(&(tab.id, leaf.pane_id))
                            .filter(|status| status.running)
                            .map(|status| (leaf.pane_id, status.clone()))
                    })
                    .collect();
                crate::runtime_probe::assign_agent_instance_labels(&ordered)
            })
            .unwrap_or_default();

        for leaf in tab.panes.leaves() {
            let agent_label = instances
                .iter()
                .find(|instance| instance.pane_id == leaf.pane_id)
                .map(|instance| instance.instance_label.clone())
                .unwrap_or_else(|| "shell".to_string());
            let activity = tab
                .pane_agent_activity(leaf.pane_id)
                .map(|activity| send_activity_label(activity.state))
                .unwrap_or(if leaf.shell_pid.is_some() { "idle" } else { "" });
            let label = if activity.is_empty() {
                format!("{agent_label} · pane {}", leaf.pane_id)
            } else {
                format!("{agent_label} · pane {} · {activity}", leaf.pane_id)
            };
            panes.push(SendTargetPane {
                tab_id: tab.id,
                pane_id: leaf.pane_id,
                label,
            });
        }
    }
    panes
}

/// Build the second-level "Send to pane" picker: a "Back" row, a non-selectable
/// source header, then two visually-distinct rows per pane — "Send →" (Insert,
/// no newline) and "Send + Run ⏎" (appends a newline).
fn build_send_to_pane_entries(
    source_label: &str,
    state: &Rc<RefCell<AppState>>,
) -> Vec<PaletteEntry> {
    let mut entries = vec![
        PaletteEntry {
            label: "Back to Commands".into(),
            shortcut: None,
            action: PaletteAction::ResetMode,
        },
        PaletteEntry {
            label: source_label.to_string(),
            shortcut: None,
            action: PaletteAction::Noop,
        },
    ];

    let targets = {
        let st = state.borrow();
        send_target_panes(&st)
    };

    if targets.is_empty() {
        entries.push(PaletteEntry {
            label: "No panes available to send to.".into(),
            shortcut: None,
            action: PaletteAction::Noop,
        });
        return entries;
    }

    for target in targets {
        entries.push(PaletteEntry {
            label: format!("Send → {}", target.label),
            shortcut: None,
            action: PaletteAction::SendToPane {
                tab_id: target.tab_id,
                pane_id: target.pane_id,
                run: false,
            },
        });
        entries.push(PaletteEntry {
            label: format!("Send + Run ⏎ {}", target.label),
            shortcut: None,
            action: PaletteAction::SendToPane {
                tab_id: target.tab_id,
                pane_id: target.pane_id,
                run: true,
            },
        });
    }

    entries
}

/// Build the second-level "Clipboard History" picker: a "Back" row, then two
/// rows per ring entry — a multi-line preview (Enter re-copies) and a "↳ Send to
/// pane…" row that chains into the send-to-pane picker seeded with that text.
fn build_clipboard_history_entries() -> Vec<PaletteEntry> {
    let mut entries = vec![PaletteEntry {
        label: "Back to Commands".into(),
        shortcut: None,
        action: PaletteAction::ResetMode,
    }];

    let snapshot = crate::terminal::clipboard_ring_snapshot();
    if snapshot.is_empty() {
        entries.push(PaletteEntry {
            label: "Clipboard history is empty.".into(),
            shortcut: None,
            action: PaletteAction::Noop,
        });
        return entries;
    }

    for entry in snapshot {
        entries.push(PaletteEntry {
            label: clipboard_preview(&entry.text),
            shortcut: Some(entry.source.clone()),
            action: PaletteAction::CopyClipboardEntry(entry.text.clone()),
        });
        entries.push(PaletteEntry {
            label: "  ↳ Send to pane…".into(),
            shortcut: None,
            action: PaletteAction::OpenSendToPaneWithText(entry.text.clone()),
        });
    }

    entries
}

/// Build the second-level "Recent Files" picker for the focused tab: a "Back"
/// row, then one row per file the tab's agents created/edited, newest-first,
/// deduped by resolved path. Paths are resolved against each pane's cwd. The
/// editor launch handles a real missing-file error rather than probing disk
/// synchronously while GTK redraws the palette.
fn build_recent_files_entries(state: &Rc<RefCell<AppState>>) -> Vec<PaletteEntry> {
    use crate::agents::FileOp;

    let mut entries = vec![PaletteEntry {
        label: "Back to Commands".into(),
        shortcut: None,
        action: PaletteAction::ResetMode,
    }];

    // Collect (timestamp, op, resolved-path) across every pane in the active
    // tab. Path resolution is pure; no filesystem probe belongs in this GTK
    // redraw path.
    let collected: Vec<(u64, FileOp, PathBuf)> = {
        let st = state.borrow();
        let Some(tab) = st.active_tab() else {
            drop(st);
            entries.push(PaletteEntry {
                label: "No recent files.".into(),
                shortcut: None,
                action: PaletteAction::Noop,
            });
            return entries;
        };
        let mut collected = Vec::new();
        for leaf in tab.panes.leaves() {
            let cwd = leaf.local_cwd();
            let Some(transcript) = st.pane_transcripts.get(&(tab.id, leaf.pane_id)) else {
                continue;
            };
            for touched in &transcript.recent_files {
                let resolved =
                    crate::agents::resolve_touched_file_path(&touched.path, cwd.as_deref());
                collected.push((touched.at_unix_ms, touched.op, resolved));
            }
        }
        collected
    };

    // Dedupe by resolved path keeping the newest operation, then sort
    // newest-first and cap the list.
    let mut latest: std::collections::HashMap<PathBuf, (u64, FileOp)> =
        std::collections::HashMap::new();
    for (at, op, resolved) in collected {
        latest
            .entry(resolved)
            .and_modify(|slot| {
                if at >= slot.0 {
                    *slot = (at, op);
                }
            })
            .or_insert((at, op));
    }
    let mut items: Vec<(PathBuf, u64, FileOp)> = latest
        .into_iter()
        .map(|(path, (at, op))| (path, at, op))
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.1));
    items.truncate(20);

    if items.is_empty() {
        entries.push(PaletteEntry {
            label: "No recent files.".into(),
            shortcut: None,
            action: PaletteAction::Noop,
        });
        return entries;
    }

    for (resolved, _at, op) in items {
        let name = resolved
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| resolved.to_string_lossy().into_owned());
        let location = resolved
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.to_string_lossy().into_owned())
            .unwrap_or_else(|| resolved.to_string_lossy().into_owned());
        entries.push(PaletteEntry {
            label: format!("{} {}", op.verb(), name),
            shortcut: Some(location),
            action: PaletteAction::OpenRecentFile(resolved),
        });
    }

    entries
}

/// Build a compact, multi-line-safe preview of a clipboard entry: up to the
/// first three lines, truncated to a char boundary, with an ellipsis when
/// clipped. Whitespace-only text is rendered as a visible placeholder.
fn clipboard_preview(text: &str) -> String {
    const MAX_CHARS: usize = 240;

    let mut lines = text.lines();
    let head: Vec<&str> = lines.by_ref().take(3).collect();
    // More than three lines means we clipped content.
    let mut truncated = lines.next().is_some();
    let mut preview = head.join("\n");

    // Char-boundary truncation (never a byte slice) so non-ASCII copies are safe.
    if preview.chars().count() > MAX_CHARS {
        let end = preview
            .char_indices()
            .nth(MAX_CHARS)
            .map(|(idx, _)| idx)
            .unwrap_or(preview.len());
        preview.truncate(end);
        truncated = true;
    }

    if preview.chars().all(char::is_whitespace) {
        return "(whitespace)".to_string();
    }

    if truncated {
        preview.push('…');
    }
    preview
}

fn build_attach_session_entries(state: &Rc<RefCell<AppState>>) -> Vec<PaletteEntry> {
    let mut entries = vec![PaletteEntry {
        label: "Back to Commands".into(),
        shortcut: None,
        action: PaletteAction::ResetMode,
    }];

    let candidates = {
        let st = state.borrow();
        let live = live_tab_backings(&st);
        build_attach_candidates(&st.detached_sessions, &st.dashboard_state.sessions, &live)
    };

    if candidates.is_empty() {
        let tmux_ui = crate::terminal::explicit_tmux_tab_ui_state();
        let message = if tmux_ui.available {
            "No tmux sessions found. Create one with a tmux tab, or configure \
             [hosts] in config.toml for remote discovery."
                .to_string()
        } else {
            tmux_ui.unavailable_message.unwrap_or_else(|| {
                "tmux is not available. Install tmux to create and attach sessions.".to_string()
            })
        };
        entries.push(PaletteEntry {
            label: message,
            shortcut: None,
            action: PaletteAction::Noop,
        });
        return entries;
    }

    for candidate in candidates {
        let label = attach_candidate_label(&candidate);
        let action = match candidate.kind {
            AttachCandidateKind::AttachedHere { tab_id } => PaletteAction::FocusTab(tab_id),
            AttachCandidateKind::Attach => PaletteAction::AttachSessionTarget {
                session_name: candidate.session_name,
                target: candidate.target,
            },
        };
        entries.push(PaletteEntry {
            label,
            shortcut: None,
            action,
        });
    }

    entries
}

fn snapshot_saved_tab(tab: &crate::Tab) -> crate::session::SavedTab {
    let cwd = tab
        .panes
        .leaf(tab.focused_pane_id)
        .or_else(|| tab.panes.leaves().into_iter().next())
        .and_then(|leaf| leaf.saved_cwd());

    crate::session::SavedTab {
        name: tab.name.clone(),
        work_origin: Some(tab.work_origin.clone()),
        cwd,
        panes: Some(crate::terminal::save_pane_tree_for_tab(
            tab,
            |_| (None, None),
            |_| (false, None),
        )),
        discovery_cwd: tab.discovery_cwd.clone(),
    }
}

fn build_mode_entries(state: &Rc<RefCell<AppState>>, mode: PaletteMode) -> Vec<PaletteEntry> {
    match mode {
        PaletteMode::Default => build_entries(state, PaletteMode::Default),
        PaletteMode::WorktreeList | PaletteMode::WorktreeRemove => {
            build_worktree_entries(state, mode, None, None)
        }
        PaletteMode::TemplateTabList => build_template_entries(TemplateMode::OpenTab),
        PaletteMode::TemplateWorkspaceList => build_template_entries(TemplateMode::OpenWorkspace),
        PaletteMode::TemplateDeleteList => build_template_entries(TemplateMode::Delete),
        PaletteMode::SavedViewList | PaletteMode::SavedViewDeleteList => {
            build_saved_view_entries(mode)
        }
        PaletteMode::AttachSessionList => build_attach_session_entries(state),
        PaletteMode::KeybindingHelp => build_keybinding_help_entries(),
        // Reached only if this is ever entered without the palette in scope;
        // `palette_entries` intercepts SendToPane with the captured source. Use a
        // generic header here so the match stays exhaustive.
        PaletteMode::SendToPane => {
            build_send_to_pane_entries("Sending: current selection or clipboard", state)
        }
        PaletteMode::ClipboardHistory => build_clipboard_history_entries(),
        PaletteMode::RecentFiles => build_recent_files_entries(state),
    }
}

/// Build the second-level "Keyboard Shortcuts" help view. Every row is a live
/// binding resolved from the installed keybinding config (defaults overlaid
/// with `keybindings.toml`), grouped by category with a section header per
/// group. Rows are display-only (`Noop`) so the view is read-only; the sole
/// actionable row is "Back to Commands". Generated at open time — never a
/// static table.
fn build_keybinding_help_entries() -> Vec<PaletteEntry> {
    use crate::keybindings::{installed_effective_bindings, ActionCategory};

    let mut entries = vec![PaletteEntry {
        label: "Back to Commands".into(),
        shortcut: None,
        action: PaletteAction::ResetMode,
    }];

    let bindings = installed_effective_bindings();
    for &category in ActionCategory::ALL {
        let mut group: Vec<_> = bindings.iter().filter(|b| b.category == category).collect();
        if group.is_empty() {
            continue;
        }
        group.sort_by(|a, b| a.display_name.cmp(b.display_name));

        // Section header (non-selectable label styled via make_row's plain
        // label; rendered as a Noop entry so navigation skips over meaning).
        entries.push(PaletteEntry {
            label: format!("— {} —", category.title()),
            shortcut: None,
            action: PaletteAction::Noop,
        });

        for binding in group {
            // Mark customized bindings so the user can see what they overrode.
            let name = if binding.customized {
                format!("{} (customized)", binding.display_name)
            } else {
                binding.display_name.to_string()
            };
            let shortcut = binding
                .trigger
                .clone()
                .unwrap_or_else(|| "unbound".to_string());
            entries.push(PaletteEntry {
                label: name,
                shortcut: Some(shortcut),
                action: PaletteAction::Noop,
            });
        }
    }

    entries
}

#[derive(Clone, Copy)]
enum TemplateMode {
    OpenTab,
    OpenWorkspace,
    Delete,
}

fn build_template_entries(mode: TemplateMode) -> Vec<PaletteEntry> {
    let mut entries = vec![PaletteEntry {
        label: "Back to Commands".into(),
        shortcut: None,
        action: PaletteAction::ResetMode,
    }];

    let templates = match PALETTE_TEMPLATES.with(|store| store.borrow().value.clone()) {
        Some(Ok(templates)) => templates,
        Some(Err(error)) => {
            entries.push(PaletteEntry {
                label: format!("⚠ Templates unavailable: {error}"),
                shortcut: None,
                action: PaletteAction::ShowError(format!("Could not load templates: {error}")),
            });
            return entries;
        }
        None => {
            entries.push(PaletteEntry {
                label: "Loading templates…".into(),
                shortcut: None,
                action: PaletteAction::Noop,
            });
            return entries;
        }
    };
    match mode {
        TemplateMode::OpenTab => {
            for template in templates {
                if template.kind() != TemplateKind::Tab {
                    continue;
                }
                entries.push(PaletteEntry {
                    label: format!("New Tab from Template: {}", template.name()),
                    shortcut: None,
                    action: PaletteAction::OpenTabTemplate(template.name().to_string()),
                });
            }
        }
        TemplateMode::OpenWorkspace => {
            for template in templates {
                if template.kind() != TemplateKind::Workspace {
                    continue;
                }
                entries.push(PaletteEntry {
                    label: format!("New Workspace from Template: {}", template.name()),
                    shortcut: None,
                    action: PaletteAction::OpenWorkspaceTemplate(template.name().to_string()),
                });
            }
        }
        TemplateMode::Delete => {
            for template in templates {
                entries.push(PaletteEntry {
                    label: format!(
                        "Delete {} Template: {}",
                        template_kind_label(template.kind()),
                        template.name()
                    ),
                    shortcut: None,
                    action: PaletteAction::DeleteTemplate(
                        template.kind(),
                        template.name().to_string(),
                    ),
                });
            }
        }
    }

    entries
}

fn build_saved_view_entries(mode: PaletteMode) -> Vec<PaletteEntry> {
    let mut entries = vec![PaletteEntry {
        label: "Back to Commands".into(),
        shortcut: None,
        action: PaletteAction::ResetMode,
    }];

    let views = match PALETTE_VIEWS.with(|store| store.borrow().value.clone()) {
        Some(Ok(views)) => views,
        Some(Err(error)) => {
            entries.push(PaletteEntry {
                label: format!("⚠ Saved views unavailable: {error}"),
                shortcut: None,
                action: PaletteAction::ShowError(format!("Could not load saved views: {error}")),
            });
            return entries;
        }
        None => {
            entries.push(PaletteEntry {
                label: "Loading saved views…".into(),
                shortcut: None,
                action: PaletteAction::Noop,
            });
            return entries;
        }
    };

    for view in views {
        let (prefix, action) = match mode {
            PaletteMode::SavedViewList => (
                "Open Saved View",
                PaletteAction::OpenSavedView(view.name.clone()),
            ),
            PaletteMode::SavedViewDeleteList => (
                "Delete Saved View",
                PaletteAction::DeleteSavedView(view.name.clone()),
            ),
            _ => continue,
        };
        entries.push(PaletteEntry {
            label: format!("{prefix}: {} ({})", view.name, view.preset.label()),
            shortcut: None,
            action,
        });
    }

    entries
}

fn template_kind_label(kind: TemplateKind) -> &'static str {
    match kind {
        TemplateKind::Tab => "Tab",
        TemplateKind::Workspace => "Workspace",
    }
}

fn template_kind_label_lower(kind: TemplateKind) -> &'static str {
    match kind {
        TemplateKind::Tab => "tab",
        TemplateKind::Workspace => "workspace",
    }
}

fn show_template_name_dialog<F>(
    window: &adw::ApplicationWindow,
    title: &str,
    accept_label: &str,
    field_label: &str,
    placeholder: &str,
    default_name: &str,
    on_accept: F,
) where
    F: Fn(String) + 'static,
{
    let dialog = gtk::Dialog::builder()
        .transient_for(window)
        .modal(true)
        .title(title)
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button(accept_label, gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Accept);

    let content = dialog.content_area();
    content.set_spacing(8);

    let label = gtk::Label::new(Some(field_label));
    label.set_halign(gtk::Align::Start);
    let entry = gtk::Entry::new();
    entry.set_text(default_name);
    entry.set_placeholder_text(Some(placeholder));
    content.append(&label);
    content.append(&entry);

    {
        let dialog = dialog.clone();
        entry.connect_activate(move |_| {
            dialog.response(gtk::ResponseType::Accept);
        });
    }

    {
        let on_accept = Rc::new(on_accept);
        let entry_for_response = entry.clone();
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                let name = entry_for_response.text().trim().to_string();
                if !name.is_empty() {
                    on_accept(name);
                }
            }
            dialog.close();
        });
    }

    dialog.present();
    entry.grab_focus();
    entry.select_region(0, -1);
}

fn show_save_tab_template_dialog(state: &Rc<RefCell<AppState>>, window: &adw::ApplicationWindow) {
    let default_name = {
        let st = state.borrow();
        let Some(tab) = st.active_tab() else {
            crate::show_error_toast("Cannot save tab template without an active tab");
            return;
        };
        tab.name.clone()
    };

    let state = state.clone();
    show_template_name_dialog(
        window,
        "Save Tab Template",
        "Save",
        "Template name",
        "Template name",
        &default_name,
        move |name| {
            let record = {
                let st = state.borrow();
                let Some(tab) = st.active_tab() else {
                    crate::show_error_toast("Active tab disappeared before saving template");
                    return;
                };
                TemplateRecord::Tab {
                    name,
                    tab: snapshot_saved_tab(tab),
                }
            };

            let name = record.name().to_string();
            let submission = crate::git::spawn_async_result(
                format!("palette-save-tab-template:{name}"),
                move || crate::templates::save(record).map_err(|error| error.to_string()),
                move |result| match result {
                    Ok(()) => invalidate_palette_templates(),
                    Err(error) => {
                        crate::show_error_toast(&format!("Could not save tab template: {error}"))
                    }
                },
            );
            match submission {
                crate::git::GitAsyncSubmission::Started => {}
                crate::git::GitAsyncSubmission::Coalesced => {
                    crate::show_toast("Tab template save is already running")
                }
                crate::git::GitAsyncSubmission::Saturated => {
                    crate::show_error_toast("Template save queue is busy; retry shortly")
                }
            }
        },
    );
}

fn show_save_view_dialog(
    preset: crate::views::ViewPreset,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    let default_name = preset.default_name().to_string();
    let state = state.clone();
    let term_stack = term_stack.clone();
    show_template_name_dialog(
        window,
        &format!("Save {} View", preset.label()),
        "Save",
        "View name",
        "View name",
        &default_name,
        move |name| {
            let saved_view = crate::views::SavedView {
                name,
                preset,
                limit: matches!(preset, crate::views::ViewPreset::RecentAlerts).then_some(20),
            };

            let state_for_apply = state.clone();
            let term_stack_for_apply = term_stack.clone();
            let submission = crate::git::spawn_async_result(
                format!("palette-save-view:{}", saved_view.name),
                move || crate::views::save(saved_view).map_err(|error| error.to_string()),
                move |result| match result {
                    Ok(()) => {
                        invalidate_palette_views();
                        crate::dashboard::invalidate_dashboard_views();
                        crate::dashboard::refresh_dashboard_if_open(
                            &state_for_apply,
                            &term_stack_for_apply,
                        );
                    }
                    Err(error) => crate::show_error_toast(&format!("Could not save view: {error}")),
                },
            );
            match submission {
                crate::git::GitAsyncSubmission::Started => {}
                crate::git::GitAsyncSubmission::Coalesced => {
                    crate::show_toast("View save is already running")
                }
                crate::git::GitAsyncSubmission::Saturated => {
                    crate::show_error_toast("View save queue is busy; retry shortly")
                }
            }
        },
    );
}

fn show_save_workspace_template_dialog(
    state: &Rc<RefCell<AppState>>,
    window: &adw::ApplicationWindow,
) {
    let default_name = {
        let st = state.borrow();
        let Some(workspace) = st.active_ws() else {
            crate::show_error_toast("Cannot save workspace template without an active workspace");
            return;
        };
        if workspace.tabs.is_empty() {
            crate::show_error_toast("Cannot save an empty workspace as a template");
            return;
        }
        workspace.name.clone()
    };

    let state = state.clone();
    show_template_name_dialog(
        window,
        "Save Workspace Template",
        "Save",
        "Template name",
        "Template name",
        &default_name,
        move |name| {
            let record = {
                let st = state.borrow();
                let Some(workspace) = st.active_ws() else {
                    crate::show_error_toast("Active workspace disappeared before saving template");
                    return;
                };
                if workspace.tabs.is_empty() {
                    crate::show_error_toast("Cannot save an empty workspace as a template");
                    return;
                }
                let active_tab_index = workspace
                    .tabs
                    .iter()
                    .position(|tab| tab.id == workspace.active_tab)
                    .unwrap_or(0);
                let tabs = workspace.tabs.iter().map(snapshot_saved_tab).collect();

                TemplateRecord::Workspace {
                    name,
                    workspace: SavedWorkspaceTemplate {
                        work_origin: Some(workspace.work_origin.clone()),
                        tabs,
                        active_tab_index,
                    },
                }
            };

            let name = record.name().to_string();
            let submission = crate::git::spawn_async_result(
                format!("palette-save-workspace-template:{name}"),
                move || crate::templates::save(record).map_err(|error| error.to_string()),
                move |result| match result {
                    Ok(()) => invalidate_palette_templates(),
                    Err(error) => crate::show_error_toast(&format!(
                        "Could not save workspace template: {error}"
                    )),
                },
            );
            match submission {
                crate::git::GitAsyncSubmission::Started => {}
                crate::git::GitAsyncSubmission::Coalesced => {
                    crate::show_toast("Workspace template save is already running")
                }
                crate::git::GitAsyncSubmission::Saturated => {
                    crate::show_error_toast("Template save queue is busy; retry shortly")
                }
            }
        },
    );
}

fn find_template(kind: TemplateKind, name: &str) -> Option<TemplateRecord> {
    PALETTE_TEMPLATES.with(|store| {
        store
            .borrow()
            .value
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .and_then(|templates| {
                templates
                    .iter()
                    .find(|template| {
                        template.kind() == kind && template.name().eq_ignore_ascii_case(name)
                    })
                    .cloned()
            })
    })
}

fn open_tab_template(
    name: &str,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    let Some(template) = find_template(TemplateKind::Tab, name) else {
        crate::show_error_toast(&format!("Tab template \"{name}\" was not found"));
        return;
    };
    let Some(saved_tab) = template.as_tab().cloned() else {
        crate::show_error_toast(&format!("Template \"{name}\" is not a tab template"));
        return;
    };

    let runtime = RuntimeHandle::from_shared_state(state.clone());
    let tab_id = crate::terminal::restore_tab(
        &runtime,
        term_stack,
        template.name(),
        saved_tab.panes.as_ref(),
        saved_tab.cwd.as_deref(),
        crate::terminal::SessionRestoreResumePolicy::lazy(
            crate::config::should_auto_resume_agents_on_session_restore(),
        ),
        false,
    );
    crate::sidebar::add_tab_row(tab_list, state, term_stack, tab_id, template.name(), true);
    crate::terminal::wire_tab_terminals(state, term_stack, tab_list, window, tab_id);

    if saved_tab.discovery_cwd.is_some() {
        schedule_template_discovery(state, tab_list, tab_id, 350);
    }
}

fn open_workspace_template(
    name: &str,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    let Some(template) = find_template(TemplateKind::Workspace, name) else {
        crate::show_error_toast(&format!("Workspace template \"{name}\" was not found"));
        return;
    };
    let Some(saved_workspace) = template.as_workspace().cloned() else {
        crate::show_error_toast(&format!("Template \"{name}\" is not a workspace template"));
        return;
    };
    if saved_workspace.tabs.is_empty() {
        crate::show_error_toast(&format!("Workspace template \"{name}\" has no tabs"));
        return;
    }

    let workspace_name = template.name().to_string();
    let runtime = RuntimeHandle::from_shared_state(state.clone());
    let git_discovery_path = saved_workspace
        .tabs
        .iter()
        .find_map(|tab| saved_template_cwd_to_local_path(tab.cwd.as_deref()))
        .map(|cwd| cwd.to_string());

    let ws_id = { runtime.create_workspace(&workspace_name, None) };

    crate::sidebar::add_workspace_header(tab_list, state, term_stack, ws_id, &workspace_name);

    let mut tab_ids = Vec::with_capacity(saved_workspace.tabs.len());
    let mut discovery_delay_idx = 0u64;
    for saved_tab in &saved_workspace.tabs {
        let tab_name = if saved_tab.name.trim().is_empty() {
            "Shell"
        } else {
            saved_tab.name.as_str()
        };
        let tab_id = crate::terminal::restore_tab(
            &runtime,
            term_stack,
            tab_name,
            saved_tab.panes.as_ref(),
            saved_tab.cwd.as_deref(),
            crate::terminal::SessionRestoreResumePolicy::lazy(
                crate::config::should_auto_resume_agents_on_session_restore(),
            ),
            false,
        );
        crate::sidebar::add_tab_row(tab_list, state, term_stack, tab_id, tab_name, false);
        crate::terminal::wire_tab_terminals(state, term_stack, tab_list, window, tab_id);
        if saved_tab.discovery_cwd.is_some() {
            let delay_ms = 500 + discovery_delay_idx * 200;
            schedule_template_discovery(state, tab_list, tab_id, delay_ms);
            discovery_delay_idx += 1;
        }
        tab_ids.push(tab_id);
    }

    if let Some(discovery_path) = git_discovery_path {
        let state_for_apply = state.clone();
        let tab_list_for_apply = tab_list.clone();
        let workspace_origin = state
            .borrow()
            .workspaces
            .iter()
            .find(|workspace| workspace.id == ws_id)
            .map(|workspace| workspace.work_origin.clone());
        let request_key = format!("discover:template-workspace:{ws_id}");
        let submission = crate::git::spawn_async(
            request_key,
            move || crate::git::discover(&discovery_path),
            move |git_info| {
                let mut st = state_for_apply.borrow_mut();
                let Some(workspace) = st.workspaces.iter_mut().find(|workspace| {
                    workspace.id == ws_id
                        && workspace_origin.as_deref() == Some(workspace.work_origin.as_str())
                }) else {
                    return;
                };
                crate::apply_git_info_to_workspace(workspace, &git_info);
                let workspace_name = workspace.name.clone();
                drop(st);
                crate::sidebar::set_workspace_header_label(
                    &tab_list_for_apply,
                    &state_for_apply,
                    ws_id,
                    &crate::workspace_header_label(&workspace_name, &git_info),
                );
                crate::sidebar::refresh_all_workspace_header_meta(
                    &tab_list_for_apply,
                    &state_for_apply,
                );
            },
        );
        if matches!(submission, crate::git::GitAsyncSubmission::Saturated) {
            crate::show_toast(
                "Git metadata refresh is busy; template workspace details will refresh later",
            );
        }
    }

    let active_tab_idx = saved_workspace
        .active_tab_index
        .min(tab_ids.len().saturating_sub(1));
    if let Some(&tab_id) = tab_ids.get(active_tab_idx) {
        crate::sidebar::activate_tab(tab_list, state, term_stack, tab_id);
        if let Some(terminal) = crate::get_active_terminal(state) {
            terminal.grab_focus();
        }
    }
}

fn saved_template_cwd_to_local_path(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?.trim();
    if cwd.is_empty() {
        return None;
    }
    if cwd.starts_with("file://") {
        let (host, path) = crate::terminal::parse_osc7_uri(cwd);
        return host.is_none().then_some(path);
    }
    if cwd.starts_with('/') {
        return Some(cwd.to_string());
    }
    cwd.find('/')
        .map(|idx| cwd[idx..].to_string())
        .or_else(|| Some(cwd.to_string()))
}

fn schedule_template_discovery(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    tab_id: u32,
    delay_ms: u64,
) {
    let state = state.clone();
    let tab_list = tab_list.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(delay_ms), move || {
        // Template-applied discovery refreshes mise data only; it does not reveal
        // the task UI (that stays governed by `[tasks] enabled` / explicit reveal).
        crate::sidebar::discover_for_tab(&state, &tab_list, tab_id, false);
    });
}

fn show_delete_template_dialog(kind: TemplateKind, name: &str, window: &adw::ApplicationWindow) {
    let dialog = gtk::Dialog::builder()
        .transient_for(window)
        .modal(true)
        .title("Delete Template")
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Delete", gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Cancel);

    let content = dialog.content_area();
    content.set_spacing(8);
    let label = gtk::Label::new(Some(&format!(
        "Delete {} template \"{}\"?",
        template_kind_label_lower(kind),
        name
    )));
    label.set_wrap(true);
    label.set_halign(gtk::Align::Start);
    content.append(&label);

    let template_name = name.to_string();
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Accept {
            let name_for_worker = template_name.clone();
            let name_for_apply = template_name.clone();
            let submission = crate::git::spawn_async_result(
                format!("palette-delete-template:{kind:?}:{template_name}"),
                move || {
                    crate::templates::delete(kind, &name_for_worker)
                        .map_err(|error| error.to_string())
                },
                move |result| match result {
                    Ok(true) => invalidate_palette_templates(),
                    Ok(false) => crate::show_error_toast(&format!(
                        "{} template \"{}\" no longer exists",
                        template_kind_label_lower(kind),
                        name_for_apply
                    )),
                    Err(error) => {
                        crate::show_error_toast(&format!("Could not delete template: {error}"))
                    }
                },
            );
            match submission {
                crate::git::GitAsyncSubmission::Started => {}
                crate::git::GitAsyncSubmission::Coalesced => {
                    crate::show_toast("Template deletion is already running")
                }
                crate::git::GitAsyncSubmission::Saturated => {
                    crate::show_error_toast("Template delete queue is busy; retry shortly")
                }
            }
        }
        dialog.close();
    });

    dialog.present();
}

fn show_delete_saved_view_dialog(
    name: &str,
    state: &Rc<RefCell<AppState>>,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    let dialog = gtk::Dialog::builder()
        .transient_for(window)
        .modal(true)
        .title("Delete Saved View")
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Delete", gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Cancel);

    let content = dialog.content_area();
    content.set_spacing(8);
    let label = gtk::Label::new(Some(&format!("Delete saved view \"{}\"?", name)));
    label.set_wrap(true);
    label.set_halign(gtk::Align::Start);
    content.append(&label);

    let view_name = name.to_string();
    let state = state.clone();
    let term_stack = term_stack.clone();
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Accept {
            let state_for_apply = state.clone();
            let term_stack_for_apply = term_stack.clone();
            let name_for_worker = view_name.clone();
            let name_for_apply = view_name.clone();
            let submission = crate::git::spawn_async_result(
                format!("palette-delete-view:{view_name}"),
                move || crate::views::delete(&name_for_worker).map_err(|error| error.to_string()),
                move |result| match result {
                    Ok(true) => {
                        invalidate_palette_views();
                        crate::dashboard::invalidate_dashboard_views();
                        if state_for_apply
                            .borrow()
                            .selected_dashboard_view
                            .as_deref()
                            .is_some_and(|selected| selected.eq_ignore_ascii_case(&name_for_apply))
                        {
                            state_for_apply.borrow_mut().selected_dashboard_view = None;
                        }
                        crate::dashboard::refresh_dashboard_if_open(
                            &state_for_apply,
                            &term_stack_for_apply,
                        );
                    }
                    Ok(false) => crate::show_error_toast(&format!(
                        "Saved view \"{}\" no longer exists",
                        name_for_apply
                    )),
                    Err(error) => {
                        crate::show_error_toast(&format!("Could not delete saved view: {error}"))
                    }
                },
            );
            match submission {
                crate::git::GitAsyncSubmission::Started => {}
                crate::git::GitAsyncSubmission::Coalesced => {
                    crate::show_toast("View deletion is already running")
                }
                crate::git::GitAsyncSubmission::Saturated => {
                    crate::show_error_toast("View delete queue is busy; retry shortly")
                }
            }
        }
        dialog.close();
    });

    dialog.present();
}

fn active_repo_root(state: &crate::AppState) -> Option<PathBuf> {
    state
        .active_ws()
        .and_then(|workspace| workspace.repo_root.clone())
        .map(PathBuf::from)
}

fn workspace_identity_is_current(
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

fn rollback_unopened_worktree(repo_root: PathBuf, worktree_path: String) {
    let path_for_worker = worktree_path.clone();
    let path_for_error = worktree_path.clone();
    let submission = crate::git::spawn_async_result(
        format!("worktree-rollback:{worktree_path}"),
        move || {
            crate::git::remove_worktree_from_repo(Some(&repo_root), Path::new(&path_for_worker))
        },
        move |result| match result {
            Ok(()) => crate::show_toast("Removed worktree created for a closed workspace"),
            Err(error) => {
                crate::show_error_toast(&worktree_rollback_worker_failure(&path_for_error, &error))
            }
        },
    );
    if let Some(cause) = worktree_rollback_submission_cause(submission) {
        crate::show_error_toast(&format!(
            "Created worktree {worktree_path}, but rollback could not be scheduled because {cause}"
        ));
    }
}

fn build_worktree_entries(
    state: &Rc<RefCell<AppState>>,
    mode: PaletteMode,
    listing: Option<WorktreeListing>,
    error: Option<(PathBuf, String)>,
) -> Vec<PaletteEntry> {
    let repo_root = {
        let st = state.borrow();
        active_repo_root(&st)
    };

    let mut entries = vec![PaletteEntry {
        label: "Back to Commands".into(),
        shortcut: None,
        action: PaletteAction::ResetMode,
    }];

    let Some(repo_root) = repo_root else {
        return entries;
    };

    let Some((cached_root, worktrees)) =
        listing.filter(|(cached_root, _)| cached_root == &repo_root)
    else {
        let error = error
            .filter(|(cached_root, _)| cached_root == &repo_root)
            .map(|(_, error)| error);
        entries.push(PaletteEntry {
            label: error
                .map(|error| format!("Could not refresh worktrees: {error}"))
                .unwrap_or_else(|| "Loading worktrees…".into()),
            shortcut: None,
            action: PaletteAction::Noop,
        });
        return entries;
    };
    debug_assert_eq!(cached_root, repo_root);
    match mode {
        PaletteMode::WorktreeList => {
            for info in worktrees {
                entries.push(PaletteEntry {
                    label: format!("Open Worktree: {}", worktree_entry_label(&info)),
                    shortcut: None,
                    action: PaletteAction::OpenWorktree(info.path),
                });
            }
        }
        PaletteMode::WorktreeRemove => {
            for info in worktrees.into_iter().skip(1) {
                entries.push(PaletteEntry {
                    label: format!("Remove Worktree: {}", worktree_entry_label(&info)),
                    shortcut: None,
                    action: PaletteAction::DeleteWorktree(
                        info.path,
                        repo_root.to_string_lossy().into_owned(),
                    ),
                });
            }
        }
        PaletteMode::Default => {}
        PaletteMode::TemplateTabList
        | PaletteMode::TemplateWorkspaceList
        | PaletteMode::TemplateDeleteList
        | PaletteMode::SavedViewList
        | PaletteMode::SavedViewDeleteList
        | PaletteMode::AttachSessionList
        | PaletteMode::KeybindingHelp
        | PaletteMode::SendToPane
        | PaletteMode::ClipboardHistory
        | PaletteMode::RecentFiles => {}
    }

    entries
}

fn worktree_entry_label(info: &crate::git::WorktreeInfo) -> String {
    let path = Path::new(&info.path);
    let display_path = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| info.path.clone());
    match &info.branch {
        Some(branch) => format!("{branch} ({display_path})"),
        None => display_path,
    }
}

fn show_create_worktree_dialog(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    let repo_root = {
        let st = state.borrow();
        active_repo_root(&st)
    };
    let Some(repo_root) = repo_root else {
        crate::show_error_toast("Create worktree requires a git repository");
        return;
    };

    let dialog = gtk::Dialog::builder()
        .transient_for(window)
        .modal(true)
        .title("Create Worktree")
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Create", gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Accept);

    let content = dialog.content_area();
    content.set_spacing(8);
    let label = gtk::Label::new(Some("Branch name"));
    label.set_halign(gtk::Align::Start);
    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("feature/my-branch"));
    content.append(&label);
    content.append(&entry);

    {
        let dialog = dialog.clone();
        entry.connect_activate(move |_| {
            dialog.response(gtk::ResponseType::Accept);
        });
    }

    {
        let state = state.clone();
        let source_workspace = state
            .borrow()
            .active_ws()
            .map(|workspace| (workspace.id, workspace.work_origin.clone()));
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        let entry_for_response = entry.clone();
        dialog.connect_response(move |dialog, response| {
            let mut keep_dialog_open = false;
            if response == gtk::ResponseType::Accept {
                let branch_name = entry_for_response.text().trim().to_string();
                if !branch_name.is_empty() {
                    let request_key =
                        format!("worktree-create:{}:{branch_name}", repo_root.display());
                    let repo_for_worker = repo_root.clone();
                    let repo_for_apply = repo_root.clone();
                    let source_for_apply = source_workspace.clone();
                    let state_for_apply = state.clone();
                    let tab_list_for_apply = tab_list.clone();
                    let term_stack_for_apply = term_stack.clone();
                    let window_for_apply = window.clone();
                    let submission = crate::git::spawn_async_result(
                        request_key,
                        move || crate::git::create_worktree(&repo_for_worker, &branch_name),
                        move |result| match result {
                            Ok(path)
                                if workspace_identity_is_current(
                                    &state_for_apply,
                                    source_for_apply.as_ref(),
                                ) =>
                            {
                                let _ = open_worktree_workspace(
                                    &path,
                                    &state_for_apply,
                                    &tab_list_for_apply,
                                    &term_stack_for_apply,
                                    &window_for_apply,
                                );
                            }
                            Ok(path) => rollback_unopened_worktree(repo_for_apply, path),
                            Err(err) => crate::show_error_toast(&worktree_mutation_worker_failure(
                                WorktreeMutationKind::Create,
                                &err,
                            )),
                        },
                    );
                    if let Some(feedback) = worktree_mutation_submission_feedback(
                        WorktreeMutationKind::Create,
                        submission,
                    ) {
                        if feedback.is_error {
                            crate::show_error_toast(feedback.message);
                        } else {
                            crate::show_toast(feedback.message);
                        }
                        keep_dialog_open = feedback.keep_dialog_open;
                    }
                }
            }
            if !keep_dialog_open {
                dialog.close();
            }
        });
    }

    dialog.present();
    entry.grab_focus();
}

pub(crate) fn open_worktree_workspace(
    worktree_path: &str,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) -> (u32, u32) {
    open_worktree_workspace_scoped_to_repo(worktree_path, state, tab_list, term_stack, window, None)
}

/// Open a worktree workspace while preserving an explicit checkout context.
///
/// Most palette callers want Git discovery to record the owning checkout.
/// Socket agent-workspace requests are different: their published contract
/// identifies the checkout supplied by the caller, including linked
/// worktrees, so that path must survive the later Git metadata apply.
pub(crate) fn open_worktree_workspace_scoped_to_repo(
    worktree_path: &str,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
    repo_root_override: Option<&str>,
) -> (u32, u32) {
    let repo_root_override = repo_root_override.map(str::to_string);
    match ensure_worktree_workspace_state(state, worktree_path) {
        WorktreeWorkspaceState::Existing {
            workspace_id,
            tab_id,
        } => {
            if let Some(repo_root) = repo_root_override.as_ref() {
                if let Some(workspace) = state
                    .borrow_mut()
                    .workspaces
                    .iter_mut()
                    .find(|workspace| workspace.id == workspace_id)
                {
                    workspace.repo_root = Some(repo_root.clone());
                }
            }
            let _ = sidebar::activate_tab(tab_list, state, term_stack, tab_id);
            if let Some(terminal) = crate::get_active_terminal(state) {
                terminal.grab_focus();
            }
            (workspace_id, tab_id)
        }
        WorktreeWorkspaceState::Created {
            workspace_id,
            header_label,
        } => {
            if let Some(repo_root) = repo_root_override.as_ref() {
                if let Some(workspace) = state
                    .borrow_mut()
                    .workspaces
                    .iter_mut()
                    .find(|workspace| workspace.id == workspace_id)
                {
                    workspace.repo_root = Some(repo_root.clone());
                }
            }
            let runtime = RuntimeHandle::from_shared_state(state.clone());
            sidebar::add_workspace_header(tab_list, state, term_stack, workspace_id, &header_label);
            let tab_id = crate::terminal::create_terminal(
                &runtime,
                term_stack,
                "Shell",
                Some(worktree_path),
                None,
            );
            sidebar::add_tab_row(tab_list, state, term_stack, tab_id, "Shell", true);
            crate::terminal::wire_tab_terminals(state, term_stack, tab_list, window, tab_id);
            let state_for_apply = state.clone();
            let tab_list_for_apply = tab_list.clone();
            let workspace_origin = state
                .borrow()
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .map(|workspace| workspace.work_origin.clone());
            let target_path = worktree_path.to_string();
            let worker_path = target_path.clone();
            let repo_root_for_apply = repo_root_override.clone();
            let request_key = format!("discover:worktree-workspace:{workspace_id}:{target_path}");
            let submission = crate::git::spawn_async(
                request_key,
                move || crate::git::discover(&worker_path),
                move |git_info| {
                    let mut st = state_for_apply.borrow_mut();
                    let Some(workspace) = st.workspaces.iter_mut().find(|workspace| {
                        workspace.id == workspace_id
                            && workspace_origin.as_deref() == Some(workspace.work_origin.as_str())
                            && workspace.working_tree_path.as_deref() == Some(target_path.as_str())
                    }) else {
                        return;
                    };
                    crate::apply_git_info_to_workspace(workspace, &git_info);
                    if let Some(repo_root) = repo_root_for_apply.as_ref() {
                        workspace.repo_root = Some(repo_root.clone());
                    }
                    workspace.is_worktree = true;
                    workspace.working_tree_path = Some(target_path.clone());
                    let workspace_name = workspace.name.clone();
                    drop(st);
                    crate::sidebar::set_workspace_header_label(
                        &tab_list_for_apply,
                        &state_for_apply,
                        workspace_id,
                        &crate::workspace_header_label(&workspace_name, &git_info),
                    );
                    crate::sidebar::refresh_all_workspace_header_meta(
                        &tab_list_for_apply,
                        &state_for_apply,
                    );
                },
            );
            if matches!(submission, crate::git::GitAsyncSubmission::Saturated) {
                crate::show_toast(
                    "Git metadata refresh is busy; worktree details will refresh later",
                );
            }
            (workspace_id, tab_id)
        }
    }
}

pub(crate) enum WorktreeWorkspaceState {
    Existing {
        workspace_id: u32,
        tab_id: u32,
    },
    Created {
        workspace_id: u32,
        header_label: String,
    },
}

pub(crate) fn ensure_worktree_workspace_state(
    state: &Rc<RefCell<AppState>>,
    worktree_path: &str,
) -> WorktreeWorkspaceState {
    let existing = {
        let st = state.borrow();
        st.workspaces
            .iter()
            .find(|ws| {
                ws.working_tree_path.as_deref() == Some(worktree_path)
                    || ws.repo_root.as_deref() == Some(worktree_path)
            })
            .and_then(|ws| {
                ws.tabs
                    .iter()
                    .find(|tab| tab.id == ws.active_tab)
                    .or_else(|| ws.tabs.first())
                    .map(|tab| (ws.id, tab.id))
            })
    };

    if let Some((workspace_id, tab_id)) = existing {
        let _ = state.borrow_mut().activate_tab(tab_id);
        return WorktreeWorkspaceState::Existing {
            workspace_id,
            tab_id,
        };
    }

    let ws_name = crate::git::workspace_name_from_repo(worktree_path);
    let runtime = RuntimeHandle::from_shared_state(state.clone());
    let workspace_id = {
        let id = runtime.create_workspace(&ws_name, Some(worktree_path.to_string()));
        let mut st = state.borrow_mut();
        if let Some(ws) = st.active_ws_mut() {
            ws.is_worktree = true;
            ws.working_tree_path = Some(worktree_path.to_string());
        }
        id
    };

    WorktreeWorkspaceState::Created {
        workspace_id,
        header_label: ws_name,
    }
}

fn show_remove_worktree_dialog(
    worktree_path: &str,
    repo_root: Option<String>,
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    let dialog = gtk::Dialog::builder()
        .transient_for(window)
        .modal(true)
        .title("Remove Worktree")
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Remove", gtk::ResponseType::Accept);

    let content = dialog.content_area();
    content.set_spacing(8);
    let label = gtk::Label::new(Some(&format!("Remove worktree at {}?", worktree_path)));
    label.set_wrap(true);
    label.set_halign(gtk::Align::Start);
    content.append(&label);

    let worktree_path = worktree_path.to_string();
    {
        let state = state.clone();
        let tab_list = tab_list.clone();
        let term_stack = term_stack.clone();
        let window = window.clone();
        let repo_root = repo_root.clone();
        dialog.connect_response(move |dialog, response| {
            let mut keep_dialog_open = false;
            if response == gtk::ResponseType::Accept {
                let request_key = format!("worktree-remove:{worktree_path}");
                let worktree_for_worker = worktree_path.clone();
                let worktree_for_apply = worktree_path.clone();
                let repo_for_worker = repo_root.clone();
                let state_for_apply = state.clone();
                let tab_list_for_apply = tab_list.clone();
                let term_stack_for_apply = term_stack.clone();
                let window_for_apply = window.clone();
                let submission = crate::git::spawn_async_result(
                    request_key,
                    move || {
                        crate::git::remove_worktree_from_repo(
                            repo_for_worker.as_deref().map(Path::new),
                            Path::new(&worktree_for_worker),
                        )
                    },
                    move |result| match result {
                        Ok(()) => {
                            let workspace_id = {
                                let st = state_for_apply.borrow();
                                st.workspaces
                                    .iter()
                                    .find(|ws| {
                                        ws.working_tree_path.as_deref() == Some(&worktree_for_apply)
                                    })
                                    .map(|ws| ws.id)
                            };
                            if let Some(ws_id) = workspace_id {
                                let _ = sidebar::close_workspace_by_id(
                                    &tab_list_for_apply,
                                    &state_for_apply,
                                    &term_stack_for_apply,
                                    ws_id,
                                );
                            }
                            ensure_default_workspace(
                                &state_for_apply,
                                &tab_list_for_apply,
                                &term_stack_for_apply,
                                &window_for_apply,
                            );
                        }
                        Err(err) => crate::show_error_toast(&worktree_mutation_worker_failure(
                            WorktreeMutationKind::Remove,
                            &err,
                        )),
                    },
                );
                if let Some(feedback) =
                    worktree_mutation_submission_feedback(WorktreeMutationKind::Remove, submission)
                {
                    if feedback.is_error {
                        crate::show_error_toast(feedback.message);
                    } else {
                        crate::show_toast(feedback.message);
                    }
                    keep_dialog_open = feedback.keep_dialog_open;
                }
            }
            if !keep_dialog_open {
                dialog.close();
            }
        });
    }

    dialog.present();
}

fn ensure_default_workspace(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
) {
    if !state.borrow().workspaces.is_empty() {
        return;
    }

    let runtime = RuntimeHandle::from_shared_state(state.clone());
    let ws_id = runtime.create_workspace("default", None);
    sidebar::add_workspace_header(tab_list, state, term_stack, ws_id, "default");
    let tab_id = crate::terminal::create_terminal(&runtime, term_stack, "Shell", None, None);
    sidebar::add_tab_row(tab_list, state, term_stack, tab_id, "Shell", true);
    crate::terminal::wire_tab_terminals(state, term_stack, tab_list, window, tab_id);
}

/// Open a new tab running an arbitrary `argv` (not a mise task), reusing the same
/// tab-spawn path as task launches so the process is visible live. Used by the
/// loop-runner dispatch in the task panel.
/// Name of the dedicated workspace that loop-runner dispatches are grouped into.
const LOOPS_WORKSPACE_NAME: &str = "Loops";
/// Max panes in one Loops tab before a dispatch spills into a fresh tab.
const MAX_LOOP_PANES_PER_TAB: usize = 4;

/// Where the next loop dispatch should land within the Loops workspace.
#[derive(Debug, PartialEq, Eq)]
enum LoopDispatch {
    /// Open a fresh tab (in the Loops workspace) and run the command there.
    NewTab,
    /// Split this (active Loops) tab and run the command in the new pane.
    SplitActiveTab { tab_id: u32 },
}

/// Whether a loop dispatch should split the active Loops tab rather than open a
/// new one: only when that tab exists and still has room (`< max_panes`).
fn should_split(active_tab_pane_count: Option<usize>, max_panes: usize) -> bool {
    matches!(active_tab_pane_count, Some(n) if n < max_panes)
}

/// Decide where a loop dispatch lands: split the active Loops tab while it has
/// room, otherwise a new tab (also covers "no Loops workspace yet" and "Loops
/// workspace has no active tab"). Pure over `AppState` so it is unit-tested.
fn plan_loop_dispatch(state: &AppState, max_panes: usize) -> LoopDispatch {
    let active_tab = state
        .workspaces
        .iter()
        .find(|ws| ws.name == LOOPS_WORKSPACE_NAME)
        .and_then(|ws| ws.tabs.iter().find(|tab| tab.id == ws.active_tab));
    match active_tab {
        Some(tab) if should_split(Some(tab.panes.leaf_count()), max_panes) => {
            LoopDispatch::SplitActiveTab { tab_id: tab.id }
        }
        _ => LoopDispatch::NewTab,
    }
}

/// Find the existing "Loops" workspace or create it. Returns its id.
fn ensure_loops_workspace(state: &Rc<RefCell<AppState>>) -> u32 {
    if let Some(id) = state
        .borrow()
        .workspaces
        .iter()
        .find(|ws| ws.name == LOOPS_WORKSPACE_NAME)
        .map(|ws| ws.id)
    {
        return id;
    }
    RuntimeHandle::from_shared_state(state.clone()).create_workspace(LOOPS_WORKSPACE_NAME, None)
}

/// Dispatch a loop-runner command into the dedicated "Loops" workspace: group
/// concurrent runs as split panes in one tab, spilling into a new tab once the
/// active tab is full. Replaces new-tab-per-run so sequential "Build with loop
/// runner" clicks stay grouped and visible together.
pub(crate) fn dispatch_loop_command(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
    tab_name: &str,
    working_dir: Option<&str>,
    argv: Vec<String>,
) -> Option<(u32, u32)> {
    let ws_id = ensure_loops_workspace(state);
    crate::sidebar::activate_workspace(tab_list, state, term_stack, ws_id);

    let decision = plan_loop_dispatch(&state.borrow(), MAX_LOOP_PANES_PER_TAB);
    match decision {
        LoopDispatch::SplitActiveTab { tab_id } => {
            let pane_count = state
                .borrow()
                .workspaces
                .iter()
                .find(|ws| ws.id == ws_id)
                .and_then(|ws| ws.tabs.iter().find(|t| t.id == tab_id))
                .map(|t| t.panes.leaf_count())
                .unwrap_or(0);
            // Alternate direction so a few loops form a grid, not one strip.
            let direction = if pane_count % 2 == 1 {
                crate::pane::SplitDirection::Vertical
            } else {
                crate::pane::SplitDirection::Horizontal
            };
            let runtime = RuntimeHandle::from_shared_state(state.clone());
            let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
            let split = crate::terminal::split_pane_with_command(
                &runtime,
                term_stack,
                tab_list,
                window,
                tab_id,
                direction,
                &argv_refs,
                working_dir,
            );
            if let Some(pane_id) = split {
                Some((tab_id, pane_id))
            } else {
                // Split failed (e.g. zoomed/non-terminal pane) — fall back to a new tab.
                Some(spawn_command_in_new_tab(
                    state,
                    tab_list,
                    term_stack,
                    window,
                    tab_name,
                    working_dir,
                    argv,
                ))
            }
        }
        LoopDispatch::NewTab => Some(spawn_command_in_new_tab(
            state,
            tab_list,
            term_stack,
            window,
            tab_name,
            working_dir,
            argv,
        )),
    }
}

pub(crate) fn spawn_command_in_new_tab(
    state: &Rc<RefCell<AppState>>,
    tab_list: &gtk::Box,
    term_stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
    tab_name: &str,
    working_dir: Option<&str>,
    argv: Vec<String>,
) -> (u32, u32) {
    let runtime = RuntimeHandle::from_shared_state(state.clone());
    let tab_id = crate::terminal::create_terminal_with_command(
        &runtime,
        term_stack,
        tab_name,
        working_dir,
        argv,
    );
    crate::sidebar::add_tab_row(tab_list, state, term_stack, tab_id, tab_name, true);
    crate::terminal::wire_tab_terminals(state, term_stack, tab_list, window, tab_id);
    (tab_id, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::PaneNode;
    use crate::workspace::{Tab, TabKind};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn stub_tab(id: u32, name: &str) -> Tab {
        Tab {
            id,
            name: name.to_string(),
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
            pane_explicit_observation: HashMap::new(),
            agent_activity: None,
            needs_attention: false,
            notified: false,
            notification_msg: None,
            notification_pane_id: None,
            pane_last_notified: std::collections::HashMap::new(),
            pane_turn: std::collections::HashMap::new(),
            workspace_action: None,
            discovery_cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
            discovered_actions: Vec::new(),
            task_buttons: Vec::new(),
            tracking_data: None,
        }
    }

    fn create_discovery_test_project(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let project = std::env::temp_dir().join(format!(
            "taarof-palette-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&project).expect("test project dir should be created");
        std::fs::write(
            project.join("mise.toml"),
            "[tasks.app:test]\nrun = 'true'\n",
        )
        .expect("test mise config should be written");
        project
    }

    fn state_with_attention(attention: bool) -> (Rc<RefCell<AppState>>, PathBuf) {
        let project =
            create_discovery_test_project(if attention { "attention" } else { "default" });
        let mut state = AppState::new();
        let mut tab = stub_tab(11, "editor");
        if attention {
            tab.needs_attention = true;
            tab.notification_msg = Some("Build done".into());
        }
        tab.discovery_cwd = Some(project.to_string_lossy().into_owned());
        state.workspaces[0].repo_root = Some(project.to_string_lossy().into_owned());
        state.workspaces[0].tabs.push(tab);
        state.workspaces[0].active_tab = 11;
        state
            .discovery_binary_paths
            .insert(11, PathBuf::from("/tmp/taarof-test-mise"));
        let target = crate::mise::task_target_for_tab(&state, 11)
            .expect("test state has an active task target");
        state.task_discovery_snapshots.insert(
            11,
            crate::task_launch::TaskDiscoverySnapshot::with_task_names(
                target,
                ["app:test".to_string()],
            ),
        );
        (Rc::new(RefCell::new(state)), project)
    }

    fn local_target(cwd: &str) -> crate::mise::DiscoveryTarget {
        crate::mise::DiscoveryTarget::Local {
            cwd: cwd.to_string(),
            binary_path: Some(PathBuf::from("/usr/bin/true")),
        }
    }

    fn test_palette_task_request(
        target: crate::mise::DiscoveryTarget,
        task_name: &str,
        workspace_action: Option<crate::workspace::WorkspaceAction>,
    ) -> TaskLaunchRequest {
        palette_task_request(
            11,
            Some(crate::task_launch::TaskDiscoverySnapshot::with_task_names(
                target,
                [task_name.to_string()],
            )),
            task_name,
            workspace_action,
        )
    }

    #[test]
    fn worktree_mutation_feedback_is_honest_for_coalesced_and_saturated_requests() {
        use crate::git::GitAsyncSubmission::{Coalesced, Saturated, Started};

        assert_eq!(
            worktree_mutation_submission_feedback(WorktreeMutationKind::Create, Started),
            None
        );
        assert_eq!(
            worktree_mutation_submission_feedback(WorktreeMutationKind::Create, Coalesced),
            Some(WorktreeMutationSubmissionFeedback {
                message: "Worktree creation is already running",
                is_error: false,
                keep_dialog_open: false,
            })
        );
        assert_eq!(
            worktree_mutation_submission_feedback(WorktreeMutationKind::Create, Saturated),
            Some(WorktreeMutationSubmissionFeedback {
                message: "Git worker queue is busy; retry shortly",
                is_error: true,
                keep_dialog_open: true,
            })
        );
        assert_eq!(
            worktree_mutation_submission_feedback(WorktreeMutationKind::Remove, Coalesced),
            Some(WorktreeMutationSubmissionFeedback {
                message: "Worktree removal is already running; wait for its result",
                is_error: false,
                keep_dialog_open: false,
            })
        );
        assert_eq!(
            worktree_mutation_submission_feedback(WorktreeMutationKind::Remove, Saturated),
            Some(WorktreeMutationSubmissionFeedback {
                message: "Git worker queue is busy; worktree was not removed. Retry shortly",
                is_error: true,
                keep_dialog_open: true,
            })
        );

        assert_eq!(
            worktree_mutation_worker_failure(WorktreeMutationKind::Create, "worker failed"),
            "Could not create worktree: worker failed"
        );
        assert_eq!(
            worktree_mutation_worker_failure(WorktreeMutationKind::Remove, "worker failed"),
            "Could not remove worktree: worker failed"
        );
        assert_eq!(
            worktree_rollback_worker_failure("/tmp/new-worktree", "worker failed"),
            "Created worktree /tmp/new-worktree, but could not roll it back after its source closed: worker failed"
        );
        assert_eq!(
            worktree_rollback_submission_cause(Coalesced),
            Some("a rollback was already registered without this completion")
        );
        assert_eq!(
            worktree_rollback_submission_cause(Saturated),
            Some("the Git worker queue is busy")
        );
    }

    #[test]
    fn worktree_listing_join_failure_preserves_error_until_explicit_retry() {
        use crate::git::GitAsyncSubmission::{Coalesced, Saturated};

        assert_eq!(
            worktree_listing_submission_error(Coalesced),
            Some("Git worktree listing coalesced unexpectedly; reopen Worktrees to retry")
        );
        assert_eq!(
            worktree_listing_submission_error(Saturated),
            Some("Git worker queue is busy; reopen Worktrees to retry")
        );

        let root = PathBuf::from("/tmp/palette-worktree-retry");
        let generation = Cell::new(8);
        let in_flight = Cell::new(true);
        let listing = RefCell::new(Some((root.clone(), Vec::new())));
        let listing_error = RefCell::new(None);

        // A late worker completion must leave the current cache and request
        // owner untouched. This is the stale apply guard used by the palette.
        assert!(!apply_worktree_listing_result(
            &generation,
            7,
            &in_flight,
            &listing,
            &listing_error,
            root.clone(),
            Err("stale join failure".into()),
        ));
        assert!(in_flight.get());
        assert!(listing.borrow().is_some());
        assert!(listing_error.borrow().is_none());

        // The worker's join-failure result is not treated as an empty healthy
        // listing. It stays visible until an explicit palette re-entry retries.
        assert!(apply_worktree_listing_result(
            &generation,
            8,
            &in_flight,
            &listing,
            &listing_error,
            root.clone(),
            Err("Git worker did not complete".into()),
        ));
        assert!(!in_flight.get());
        assert!(listing.borrow().is_none());
        assert_eq!(
            listing_error.borrow().as_ref(),
            Some(&(root.clone(), "Git worker did not complete".into()))
        );

        invalidate_worktree_listing_state(&generation, &listing, &listing_error, &in_flight);
        assert_eq!(generation.get(), 9);
        assert!(!in_flight.get());
        assert!(listing.borrow().is_none());
        assert!(listing_error.borrow().is_none());
    }

    #[test]
    fn activated_row_resolves_the_clicked_secondary_picker_entry() {
        let entries = vec![
            PaletteEntry {
                label: "Back to Commands".into(),
                shortcut: None,
                action: PaletteAction::ResetMode,
            },
            PaletteEntry {
                label: "Open Workspace Template: backend".into(),
                shortcut: None,
                action: PaletteAction::OpenWorkspaceTemplate("backend".into()),
            },
            PaletteEntry {
                label: "Open Workspace Template: frontend".into(),
                shortcut: None,
                action: PaletteAction::OpenWorkspaceTemplate("frontend".into()),
            },
        ];

        let activated = activated_entry_for_row(&entries, "template", 1)
            .expect("the clicked filtered row should resolve");

        assert_eq!(activated.label, "Open Workspace Template: frontend");
    }

    #[test]
    fn activated_row_uses_the_same_visible_row_mapping_as_the_palette() {
        let entries = vec![
            PaletteEntry {
                label: "New Tab".into(),
                shortcut: None,
                action: PaletteAction::Activate(crate::keybindings::Action::NewTab),
            },
            PaletteEntry {
                label: "Run app:test in focused pane".into(),
                shortcut: None,
                action: PaletteAction::MiseTaskInFocusedPane(test_palette_task_request(
                    local_target("/tmp"),
                    "app:test",
                    None,
                )),
            },
            PaletteEntry {
                label: "New Workspace".into(),
                shortcut: None,
                action: PaletteAction::Activate(crate::keybindings::Action::NewWorkspace),
            },
        ];

        // Search-only aliases are absent from the unfiltered ListBox. A mouse
        // click must therefore resolve against visible rows, not the backing
        // entry index.
        let activated = activated_entry_for_row(&entries, "", 1)
            .expect("the second visible top-level row should resolve");

        assert_eq!(activated.label, "New Workspace");
        assert!(activated_entry_for_row(&entries, "", -1).is_none());
        assert!(activated_entry_for_row(&entries, "", 2).is_none());
    }

    #[test]
    fn jump_attention_is_ranked_first_when_attention_exists() {
        let (state, project) = state_with_attention(false);
        state
            .borrow_mut()
            .set_socket_notification(11, "Build done".to_string());
        let entries = build_entries(&state, PaletteMode::Default);
        let _ = std::fs::remove_dir_all(project);

        assert_eq!(entries[0].label, "Jump to Attention Tab");
        assert!(entries
            .iter()
            .any(|entry| entry.label == "Save View: Currently Flagged Panes"));
    }

    #[test]
    fn jump_attention_stays_available_without_attention() {
        let (state, project) = state_with_attention(false);
        let entries = build_entries(&state, PaletteMode::Default);
        let _ = std::fs::remove_dir_all(project);

        assert_ne!(entries[0].label, "Jump to Attention Tab");
        assert!(entries
            .iter()
            .any(|entry| entry.label == "Jump to Attention Tab"));
    }

    #[test]
    fn discovered_workspace_actions_are_labeled_as_quick_actions() {
        let mut state = AppState::new();
        let mut tab = stub_tab(11, "editor");
        tab.discovered_actions = vec![crate::WorkspaceAction::Dev];
        state.workspaces[0].tabs.push(tab);
        state.workspaces[0].active_tab = 11;
        let target = crate::mise::task_target_for_tab(&state, 11)
            .expect("test state has an active task target");
        state.task_discovery_snapshots.insert(
            11,
            crate::task_launch::TaskDiscoverySnapshot::with_task_names(target, ["dev".to_string()]),
        );
        let state = Rc::new(RefCell::new(state));

        let entries = build_entries(&state, PaletteMode::Default);

        assert!(entries
            .iter()
            .any(|entry| entry.label == "Quick Action: Dev Server"));
    }

    #[test]
    fn should_split_only_when_active_loops_tab_has_room() {
        assert!(!should_split(None, MAX_LOOP_PANES_PER_TAB)); // no active tab → new tab
        assert!(should_split(Some(1), MAX_LOOP_PANES_PER_TAB));
        assert!(should_split(Some(3), MAX_LOOP_PANES_PER_TAB));
        assert!(!should_split(Some(4), MAX_LOOP_PANES_PER_TAB)); // full → spill to new tab
        assert!(!should_split(Some(9), MAX_LOOP_PANES_PER_TAB));
    }

    #[test]
    fn plan_loop_dispatch_opens_new_tab_when_no_loops_workspace() {
        let state = AppState::new(); // only the default workspace exists
        assert_eq!(
            plan_loop_dispatch(&state, MAX_LOOP_PANES_PER_TAB),
            LoopDispatch::NewTab
        );
    }

    #[test]
    fn plan_loop_dispatch_opens_new_tab_when_loops_has_no_active_tab() {
        let mut state = AppState::new();
        state.create_workspace(LOOPS_WORKSPACE_NAME, None); // empty, active_tab == 0
        assert_eq!(
            plan_loop_dispatch(&state, MAX_LOOP_PANES_PER_TAB),
            LoopDispatch::NewTab
        );
    }

    #[test]
    fn plan_loop_dispatch_splits_active_loops_tab_with_room() {
        let mut state = AppState::new();
        let ws_id = state.create_workspace(LOOPS_WORKSPACE_NAME, None);
        let tab = stub_tab(11, "loop: LR-1"); // Stub pane → leaf_count 1 (< max)
        let ws = state
            .workspaces
            .iter_mut()
            .find(|w| w.id == ws_id)
            .expect("loops workspace should exist");
        ws.tabs.push(tab);
        ws.active_tab = 11;
        assert_eq!(
            plan_loop_dispatch(&state, MAX_LOOP_PANES_PER_TAB),
            LoopDispatch::SplitActiveTab { tab_id: 11 }
        );
    }

    #[test]
    fn test_palette_does_not_block_on_discovery() {
        let _guard = crate::mise::task_discovery_test_guard();
        crate::mise::clear_task_discovery_cache_for_test();

        let (state, project) = state_with_attention(false);
        let target = {
            let st = state.borrow();
            active_task_target(&st).expect("test state has an active task target")
        };
        crate::mise::set_pending_task_discovery_for_test(&target);

        let started_at = Instant::now();
        let entries = build_entries(&state, PaletteMode::Default);
        let _ = std::fs::remove_dir_all(project);

        assert!(started_at.elapsed() < Duration::from_millis(50));
        assert!(entries
            .iter()
            .any(|entry| entry.label == "mise: discovering tasks..."));
    }

    #[test]
    fn palette_uses_cached_mise_tasks_after_discovery() {
        let _guard = crate::mise::task_discovery_test_guard();
        crate::mise::clear_task_discovery_cache_for_test();

        let (state, project) = state_with_attention(false);
        let target = {
            let st = state.borrow();
            active_task_target(&st).expect("test state has an active task target")
        };
        crate::mise::set_ready_task_discovery_for_test(
            &target,
            vec![crate::mise::MiseTask {
                name: "app:test".into(),
                description: "Run tests".into(),
                hide: false,
                source: None,
                global: false,
            }],
            Duration::ZERO,
        );

        let entries = build_entries(&state, PaletteMode::Default);
        let _ = std::fs::remove_dir_all(project);

        assert!(entries
            .iter()
            .any(|entry| entry.label == "mise: app:test — Run tests"));
        assert!(!entries
            .iter()
            .any(|entry| entry.label == "mise: discovering tasks..."));
    }

    #[test]
    fn palette_hides_mise_section_when_cached_discovery_is_empty() {
        let _guard = crate::mise::task_discovery_test_guard();
        crate::mise::clear_task_discovery_cache_for_test();

        let (state, project) = state_with_attention(false);
        let target = {
            let st = state.borrow();
            active_task_target(&st).expect("test state has an active task target")
        };
        crate::mise::set_ready_task_discovery_for_test(&target, Vec::new(), Duration::ZERO);

        let entries = build_entries(&state, PaletteMode::Default);
        let _ = std::fs::remove_dir_all(project);

        assert!(!entries
            .iter()
            .any(|entry| entry.label.starts_with("mise: ")));
    }

    #[test]
    fn palette_skips_local_binary_resolution_on_ui_path() {
        let _guard = crate::mise::task_discovery_test_guard();
        crate::mise::clear_task_discovery_cache_for_test();
        crate::mise::clear_find_mise_test_probe();

        let root = std::env::temp_dir().join(format!(
            "taarof-palette-ui-discovery-{}",
            std::process::id()
        ));
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("test project dir should be created");
        std::fs::write(
            project.join("mise.toml"),
            "[tasks.app:test]\nrun = 'true'\n",
        )
        .expect("test mise config should be written");

        let state = {
            let mut state = AppState::new();
            state.workspaces[0].repo_root = Some(project.to_string_lossy().into_owned());
            let mut tab = stub_tab(11, "editor");
            tab.discovery_cwd = Some(project.to_string_lossy().into_owned());
            state.workspaces[0].tabs.push(tab);
            state.workspaces[0].active_tab = 11;
            let target = crate::mise::task_target_for_tab(&state, 11)
                .expect("test state has an active task target");
            state.task_discovery_snapshots.insert(
                11,
                crate::task_launch::TaskDiscoverySnapshot::with_task_names(
                    target,
                    ["app:test".to_string()],
                ),
            );
            Rc::new(RefCell::new(state))
        };
        let target = {
            let st = state.borrow();
            active_task_target(&st).expect("test state has an active task target")
        };
        crate::mise::set_pending_task_discovery_for_test(&target);
        let probe_delay = Duration::from_secs(1);
        crate::mise::install_find_mise_test_probe(None, probe_delay);

        let started_at = Instant::now();
        let entries = build_entries(&state, PaletteMode::Default);
        let call_count = crate::mise::find_mise_test_probe_call_count();

        crate::mise::clear_find_mise_test_probe();
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            started_at.elapsed() < probe_delay / 4,
            "palette UI path should not wait on slow mise resolution"
        );
        assert_eq!(call_count, 0);
        assert!(entries
            .iter()
            .any(|entry| entry.label == "mise: discovering tasks..."));
    }

    #[test]
    fn test_mise_task_opens_new_tab() {
        let request = test_palette_task_request(local_target("/tmp"), "dev", None);
        let plan = request
            .plan(Some(local_target("/tmp")))
            .expect("local mise task should produce a tab launch plan");

        assert_eq!(plan.tab_name, "mise: dev");
        assert_eq!(plan.working_dir.as_deref(), Some("/tmp"));
        assert_eq!(
            plan.argv,
            vec![
                "/usr/bin/true".to_string(),
                "run".to_string(),
                "--raw".to_string(),
                "dev".to_string()
            ]
        );
        assert!(plan.respawn_on_exit.is_none());
        assert_eq!(plan.workspace_action, None);
    }

    #[test]
    fn test_mise_task_always_opens_new_tab() {
        // Both the primary and legacy "focused pane" variants must open a fresh
        // terminal tab and never inject commands into the active shell (#23).
        let request = test_palette_task_request(local_target("/tmp"), "dev", None);
        assert_eq!(
            PaletteAction::MiseTask(request.clone()).mise_dispatch_mode(),
            Some(MiseDispatchMode::OpenNewTab)
        );
        assert_eq!(
            PaletteAction::MiseTaskInFocusedPane(request).mise_dispatch_mode(),
            Some(MiseDispatchMode::OpenNewTab)
        );
    }

    #[test]
    fn focused_pane_mise_entries_are_search_only() {
        let entries = vec![
            PaletteEntry {
                label: "mise: dev".into(),
                shortcut: None,
                action: PaletteAction::MiseTask(test_palette_task_request(
                    local_target("/tmp"),
                    "dev",
                    None,
                )),
            },
            PaletteEntry {
                label: "Legacy Alias: Run mise dev in Focused Pane (Dedicated Tab)".into(),
                shortcut: None,
                action: PaletteAction::MiseTaskInFocusedPane(test_palette_task_request(
                    local_target("/tmp"),
                    "dev",
                    None,
                )),
            },
        ];

        let default_labels: Vec<_> = filtered_entries(&entries, "")
            .into_iter()
            .map(|entry| entry.label.as_str())
            .collect();
        let focused_labels: Vec<_> = filtered_entries(&entries, "focused")
            .into_iter()
            .map(|entry| entry.label.as_str())
            .collect();

        assert_eq!(default_labels, vec!["mise: dev"]);
        assert_eq!(
            focused_labels,
            vec!["Legacy Alias: Run mise dev in Focused Pane (Dedicated Tab)"]
        );
    }

    // ── Attach-session candidate builder ─────────────────────────────────────

    use crate::dashboard::{DashboardSession, DetachedSession, SessionStatus};
    use crate::tmux::TmuxTarget;

    fn stub_detached(name: &str, target: TmuxTarget, command: Option<&str>) -> DetachedSession {
        DetachedSession {
            session_name: name.to_string(),
            host: match &target {
                TmuxTarget::Local => "localhost".to_string(),
                TmuxTarget::Remote { ssh_target } => ssh_target.clone(),
            },
            workspace: "default".to_string(),
            target,
            detached_at: Instant::now(),
            last_command: command.map(str::to_string),
            finished: false,
        }
    }

    fn stub_dashboard_session(
        name: &str,
        target: TmuxTarget,
        status: SessionStatus,
        is_detached: bool,
        command: Option<&str>,
    ) -> DashboardSession {
        DashboardSession {
            name: name.to_string(),
            host: match &target {
                TmuxTarget::Local => "localhost".to_string(),
                TmuxTarget::Remote { ssh_target } => ssh_target.clone(),
            },
            target,
            status,
            command: command.map(str::to_string),
            started: None,
            pid: None,
            output_preview: Vec::new(),
            is_detached,
            is_taarof_managed: true,
        }
    }

    fn remote(host: &str) -> TmuxTarget {
        TmuxTarget::Remote {
            ssh_target: host.to_string(),
        }
    }

    #[test]
    fn build_attach_candidates_empty_input_yields_empty() {
        let candidates = build_attach_candidates(&[], &[], &[]);
        assert!(candidates.is_empty());
    }

    #[test]
    fn build_attach_candidates_merges_and_dedups_by_identity() {
        // Same (name, target) appears in both detached and dashboard sources.
        let detached = vec![stub_detached("work", TmuxTarget::Local, None)];
        let dashboard = vec![stub_dashboard_session(
            "work",
            TmuxTarget::Local,
            SessionStatus::Detached,
            true,
            Some("vim"),
        )];

        let candidates = build_attach_candidates(&detached, &dashboard, &[]);
        assert_eq!(
            candidates.len(),
            1,
            "identical identity must dedup to one row"
        );
        let c = &candidates[0];
        assert_eq!(c.session_name, "work");
        assert_eq!(c.target, TmuxTarget::Local);
        assert_eq!(c.host, "local");
        // Command enriched from the dashboard source when detached lacked it.
        assert_eq!(c.command.as_deref(), Some("vim"));
        assert_eq!(c.kind, AttachCandidateKind::Attach);
    }

    #[test]
    fn build_attach_candidates_same_name_two_hosts_distinct_rows() {
        let dashboard = vec![
            stub_dashboard_session("api", remote("alpha"), SessionStatus::Running, false, None),
            stub_dashboard_session("api", remote("beta"), SessionStatus::Running, false, None),
            stub_dashboard_session(
                "api",
                TmuxTarget::Local,
                SessionStatus::Running,
                false,
                None,
            ),
        ];

        let candidates = build_attach_candidates(&[], &dashboard, &[]);
        assert_eq!(
            candidates.len(),
            3,
            "same name on 3 targets = 3 distinct rows"
        );

        let hosts: Vec<&str> = candidates.iter().map(|c| c.host.as_str()).collect();
        assert!(hosts.contains(&"alpha"));
        assert!(hosts.contains(&"beta"));
        assert!(hosts.contains(&"local"));

        // Every row shares the name but has a distinct target identity.
        assert!(candidates.iter().all(|c| c.session_name == "api"));
        let targets: std::collections::HashSet<_> =
            candidates.iter().map(|c| c.target.clone()).collect();
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn build_attach_candidates_attached_here_resolves_to_tab_id() {
        // A session already backing a live tab in THIS instance resolves to that
        // tab so selection focuses it instead of respawning.
        let dashboard = vec![stub_dashboard_session(
            "session-a",
            TmuxTarget::Local,
            SessionStatus::Running,
            false,
            None,
        )];
        let live_tabs: Vec<LiveTabBacking> =
            vec![(42, vec![("session-a".to_string(), TmuxTarget::Local)])];

        let candidates = build_attach_candidates(&[], &dashboard, &live_tabs);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].kind,
            AttachCandidateKind::AttachedHere { tab_id: 42 }
        );
    }

    #[test]
    fn build_attach_candidates_same_name_only_matching_target_is_attached_here() {
        // Live tab backs the LOCAL "api"; the remote "api" must stay a plain
        // Attach candidate (identity is (name, target), never name alone).
        let dashboard = vec![
            stub_dashboard_session(
                "api",
                TmuxTarget::Local,
                SessionStatus::Running,
                false,
                None,
            ),
            stub_dashboard_session("api", remote("beta"), SessionStatus::Running, false, None),
        ];
        let live_tabs: Vec<LiveTabBacking> =
            vec![(7, vec![("api".to_string(), TmuxTarget::Local)])];

        let candidates = build_attach_candidates(&[], &dashboard, &live_tabs);
        let local = candidates
            .iter()
            .find(|c| c.target == TmuxTarget::Local)
            .expect("local api candidate");
        let beta = candidates
            .iter()
            .find(|c| c.target == remote("beta"))
            .expect("beta api candidate");
        assert_eq!(local.kind, AttachCandidateKind::AttachedHere { tab_id: 7 });
        assert_eq!(beta.kind, AttachCandidateKind::Attach);
    }

    #[test]
    fn build_attach_candidates_detached_ordering_first() {
        // Detached rows appear before dashboard-only rows.
        let detached = vec![stub_detached("detached-one", TmuxTarget::Local, None)];
        let dashboard = vec![stub_dashboard_session(
            "dash-only",
            remote("beta"),
            SessionStatus::Running,
            false,
            None,
        )];

        let candidates = build_attach_candidates(&detached, &dashboard, &[]);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].session_name, "detached-one");
        assert_eq!(candidates[1].session_name, "dash-only");
        assert!(
            candidates[1].attached,
            "running dashboard session is attached"
        );
        assert!(!candidates[0].attached, "detached row is not attached");
    }

    #[test]
    fn attach_candidate_label_shows_host_state_and_command() {
        let candidate = AttachCandidate {
            session_name: "work".into(),
            target: remote("alpha"),
            host: "alpha".into(),
            attached: true,
            command: Some("cargo test".into()),
            kind: AttachCandidateKind::Attach,
        };
        let label = attach_candidate_label(&candidate);
        assert!(label.contains("alpha"));
        assert!(label.contains("work"));
        assert!(label.contains("attached"));
        assert!(label.contains("cargo test"));

        let here = AttachCandidate {
            kind: AttachCandidateKind::AttachedHere { tab_id: 1 },
            ..candidate
        };
        assert!(attach_candidate_label(&here).contains("open here"));
    }

    #[test]
    fn keybinding_help_lists_every_action_with_a_trigger_column() {
        let entries = build_keybinding_help_entries();

        // First row is the escape hatch back to the main palette.
        assert_eq!(entries[0].label, "Back to Commands");

        // Every action's display name appears somewhere in the view.
        for &action in crate::keybindings::Action::ALL {
            let name = action.label();
            assert!(
                entries
                    .iter()
                    .any(|e| e.label == name || e.label == format!("{name} (customized)")),
                "help view missing action row for {name}"
            );
        }

        // Bound actions carry a trigger column; unbound ones show "unbound".
        let new_tab = entries
            .iter()
            .find(|e| e.label == "New Tab")
            .expect("New Tab row present");
        let shortcut = new_tab.shortcut.as_deref().expect("New Tab has a shortcut");
        assert!(
            shortcut.to_lowercase().contains("ctrl"),
            "shortcut: {shortcut}"
        );

        let new_ws = entries
            .iter()
            .find(|e| e.label == "New Workspace")
            .expect("New Workspace row present");
        assert_eq!(new_ws.shortcut.as_deref(), Some("unbound"));

        // At least one category section header is rendered.
        assert!(
            entries.iter().any(|e| e.label.starts_with("— ")),
            "expected category section headers"
        );
    }
}
